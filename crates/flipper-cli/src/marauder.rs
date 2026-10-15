//! A narrow RPC bridge to the Marauder UART. The Flipper FAP enforces the same
//! command allowlist, so `flipper raw` cannot turn this into arbitrary serial IO.

use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Subcommand, ValueEnum};
use flipper_core::client::{Client, Transport};
use flipper_core::pb::main::Content;
use flipper_core::pb::{self, Main};
use tokio::sync::broadcast;
use tokio::time::{timeout, Instant};

use crate::output::{emit, EXIT_OK};

const APP_PATH: &str = "/ext/apps/GPIO/marauder_bridge.fap";
const MAX_OUTPUT: usize = 24 * 1024;

#[derive(serde::Serialize, Debug, PartialEq, Eq)]
struct AccessPoint {
    ssid: String,
    channel: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    bssid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rssi: Option<i16>,
}

#[derive(Subcommand)]
pub enum MarauderAction {
    /// Show the commands compiled into the connected Marauder firmware.
    Commands,
    /// Read Marauder's current list of discovered networks.
    List,
    /// Read Marauder's current list of discovered stations.
    Stations,
    /// Read Marauder's current list of Wi-Fi names for probe tests.
    Ssids,
    /// Scan for nearby Wi-Fi access points, then stop automatically.
    Scan {
        /// Scan duration in seconds (1..30).
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=30))]
        duration: u64,
    },
    /// Capture bounded Wi-Fi observations, then stop automatically.
    Sniff {
        #[arg(value_enum)]
        mode: SniffMode,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=30))]
        duration: u64,
    },
    /// Run a bounded radio test. Deauth needs an AP index; probe needs an SSID index.
    Attack {
        #[arg(value_enum)]
        mode: AttackMode,
        /// Index of the target AP, required for deauth (0..99).
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=99))]
        ap: Option<u8>,
        /// Index of the Wi-Fi name, required for probe (0..99).
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=99))]
        ssid: Option<u8>,
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(1..=10))]
        duration: u64,
    },
    /// Stop a scan that was left running on the Marauder.
    Stop,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum SniffMode {
    Beacon,
    Probe,
    Deauth,
    Pmkid,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum AttackMode {
    Deauth,
    Probe,
    Beacon,
    Rickroll,
}

impl MarauderAction {
    fn command(&self) -> &'static str {
        match self {
            Self::Commands => "help",
            Self::List => "list -a",
            Self::Stations => "list -c",
            Self::Ssids => "list -s",
            Self::Scan { .. } => "scanap",
            Self::Sniff { mode, .. } => match mode {
                SniffMode::Beacon => "sniffbeacon",
                SniffMode::Probe => "sniffprobe",
                SniffMode::Deauth => "sniffdeauth",
                SniffMode::Pmkid => "sniffpmkid",
            },
            Self::Attack { mode, .. } => match mode {
                AttackMode::Deauth => "attack -t deauth",
                AttackMode::Probe => "attack -t probe",
                AttackMode::Beacon => "attack -t beacon -r",
                AttackMode::Rickroll => "attack -t rickroll",
            },
            Self::Stop => "stopscan",
        }
    }
}

pub async fn run<T: Transport>(
    client: &Client<T>,
    action: &MarauderAction,
    json: bool,
) -> anyhow::Result<i32> {
    let mut incoming = client.unsolicited();
    client.start_app(APP_PATH, "RPC").await.with_context(|| {
        format!("cannot start {APP_PATH}; install the Marauder Bridge FAP first")
    })?;

    let result = async {
        wait_for_start(&mut incoming).await?;
        let command = action.command();
        if let MarauderAction::Attack { mode, ap, ssid, .. } = action {
            let selection = match (mode, ap, ssid) {
                (AttackMode::Deauth, Some(index), None) => Some(("-a", index)),
                (AttackMode::Probe, None, Some(index)) => Some(("-s", index)),
                (AttackMode::Deauth, None, _) => {
                    bail!("deauth requires --ap INDEX from `marauder list`")
                }
                (AttackMode::Probe, _, None) => {
                    bail!("probe requires --ssid INDEX from `marauder ssids`")
                }
                (_, None, None) => None,
                _ => bail!("--ap is only for deauth and --ssid is only for probe"),
            };
            if let Some((kind, index)) = selection {
                send_command(client, &format!("select {kind} {index}")).await?;
                let response =
                    clean_output(&collect_until_idle(&mut incoming, Duration::from_secs(2)).await?);
                if response.is_empty() || response.to_ascii_lowercase().contains("invalid") {
                    bail!("Marauder did not accept index {index}; check the current Marauder list");
                }
            }
        }
        send_command(client, command).await?;
        let output = match action {
            MarauderAction::Scan { duration } => {
                let scanned = collect_for(&mut incoming, Duration::from_secs(*duration)).await;
                // A failed read must still stop the radio scan.
                let stopped = send_command(client, "stopscan").await;
                let mut bytes = scanned?;
                stopped?;
                append_output(
                    &mut bytes,
                    collect_until_idle(&mut incoming, Duration::from_secs(2)).await?,
                )?;
                send_command(client, "list -a").await?;
                append_output(
                    &mut bytes,
                    collect_until_idle(&mut incoming, Duration::from_secs(2)).await?,
                )?;
                bytes
            }
            MarauderAction::Sniff { duration, .. } | MarauderAction::Attack { duration, .. } => {
                let observed = collect_for(&mut incoming, Duration::from_secs(*duration)).await;
                let stopped = send_command(client, "stopscan").await;
                let mut bytes = observed?;
                stopped?;
                append_output(
                    &mut bytes,
                    collect_until_idle(&mut incoming, Duration::from_secs(2)).await?,
                )?;
                bytes
            }
            _ => collect_until_idle(&mut incoming, Duration::from_secs(4)).await?,
        };
        if output.is_empty()
            && !matches!(
                action,
                MarauderAction::Sniff { .. } | MarauderAction::Attack { .. }
            )
        {
            bail!(
                "Marauder did not answer '{command}' (check board power, UART pins and firmware)"
            );
        }
        let text = clean_output(&output);
        let access_points = match action {
            MarauderAction::Scan { .. } => parse_scan_access_points(&text),
            MarauderAction::List => parse_list_access_points(&text),
            _ => Vec::new(),
        };
        emit(
            json,
            serde_json::json!({ "command": command, "text": text, "access_points": access_points }),
            format!("{text}\n"),
        );
        Ok(EXIT_OK)
    }
    .await;

    let exit = client.exit_app().await;
    if let Err(error) = exit {
        eprintln!("note: Marauder Bridge did not close cleanly: {error}");
    }
    result
}

async fn wait_for_start(incoming: &mut broadcast::Receiver<Main>) -> anyhow::Result<()> {
    timeout(Duration::from_secs(4), async {
        loop {
            let message = incoming.recv().await.context("RPC stream closed")?;
            if let Some(Content::AppStateResponse(state)) = message.content {
                if state.state == pb::app::AppState::AppStarted as i32 {
                    return Ok(());
                }
            }
        }
    })
    .await
    .context("Marauder Bridge did not signal readiness")?
}

async fn send_command<T: Transport>(client: &Client<T>, command: &str) -> anyhow::Result<()> {
    client
        .call(vec![Content::AppDataExchangeRequest(
            pb::app::DataExchangeRequest {
                data: command.as_bytes().to_vec(),
            },
        )])
        .await
        .with_context(|| format!("Marauder Bridge rejected '{command}'"))?;
    Ok(())
}

fn append_data(message: Main, output: &mut Vec<u8>) -> anyhow::Result<()> {
    if let Some(Content::AppDataExchangeRequest(data)) = message.content {
        append_output(output, data.data)?;
    }
    Ok(())
}

fn append_output(output: &mut Vec<u8>, data: Vec<u8>) -> anyhow::Result<()> {
    if output.len() + data.len() > MAX_OUTPUT {
        bail!("Marauder output exceeded {MAX_OUTPUT} bytes");
    }
    output.extend(data);
    Ok(())
}

async fn collect_until_idle(
    incoming: &mut broadcast::Receiver<Main>,
    first_timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let wait = if output.is_empty() {
            first_timeout
        } else {
            Duration::from_millis(600)
        };
        match timeout(wait, incoming.recv()).await {
            Ok(Ok(message)) => append_data(message, &mut output)?,
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                bail!("lost {n} Marauder response frames");
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                bail!("Flipper RPC stream closed while reading Marauder");
            }
            Err(_) => return Ok(output),
        }
    }
}

async fn collect_for(
    incoming: &mut broadcast::Receiver<Main>,
    duration: Duration,
) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::new();
    let deadline = Instant::now() + duration;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match timeout(remaining, incoming.recv()).await {
            Ok(Ok(message)) => append_data(message, &mut output)?,
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                bail!("lost {n} Marauder response frames");
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                bail!("Flipper RPC stream closed during Marauder scan");
            }
            Err(_) => break,
        }
    }
    Ok(output)
}

/// Serial output is untrusted: keep printable text and whitespace, remove
/// terminal escape sequences before writing anything to a user's terminal.
fn clean_output(bytes: &[u8]) -> String {
    let mut result = String::new();
    let decoded = String::from_utf8_lossy(bytes);
    let mut iter = decoded.chars().peekable();
    while let Some(ch) = iter.next() {
        if ch == '\u{1b}' {
            if iter.next() == Some('[') {
                for next in iter.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
        } else if ch == '\r' {
            continue;
        } else if ch == '\n' || ch == '\t' || !ch.is_control() {
            result.push(ch);
        }
    }
    result.trim().to_owned()
}

fn parse_scan_access_points(text: &str) -> Vec<AccessPoint> {
    text.lines()
        .filter_map(|line| {
            let line = line.strip_prefix("RSSI: ")?;
            let (rssi, line) = line.split_once(" Ch: ")?;
            let (channel, line) = line.split_once(" BSSID: ")?;
            let (bssid, ssid) = line.split_once(" ESSID: ")?;
            Some(AccessPoint {
                ssid: ssid.trim().to_owned(),
                channel: channel.trim().parse().ok()?,
                bssid: Some(bssid.trim().to_owned()),
                rssi: Some(rssi.trim().parse().ok()?),
            })
        })
        .collect()
}

fn parse_list_access_points(text: &str) -> Vec<AccessPoint> {
    text.lines()
        .filter_map(|line| {
            let line = line.strip_prefix('[')?;
            let (_, line) = line.split_once("][CH:")?;
            let (channel, ssid) = line.split_once("] ")?;
            Some(AccessPoint {
                ssid: ssid.trim().trim_end_matches('\u{fffd}').trim().to_owned(),
                channel: channel.parse().ok()?,
                bssid: None,
                rssi: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{clean_output, parse_list_access_points, parse_scan_access_points, AccessPoint};

    #[test]
    fn serial_output_cannot_change_terminal_state() {
        assert_eq!(clean_output(b"ok\x1b[31m red\x1b[0m\r\n"), "ok red");
        assert_eq!(clean_output(b"SSID\0\x07\n"), "SSID");
    }

    #[test]
    fn parses_actual_marauder_scan_and_list_shapes() {
        let scan = "RSSI: -65 Ch: 6 BSSID: aa:bb:cc:dd:ee:ff ESSID: Guest Wi-Fi \n";
        assert_eq!(
            parse_scan_access_points(scan),
            vec![AccessPoint {
                ssid: "Guest Wi-Fi".into(),
                channel: 6,
                bssid: Some("aa:bb:cc:dd:ee:ff".into()),
                rssi: Some(-65),
            }]
        );
        let list = "[0][CH:6] Guest Wi-Fi �\n0 selected\n";
        assert_eq!(
            parse_list_access_points(list),
            vec![AccessPoint {
                ssid: "Guest Wi-Fi".into(),
                channel: 6,
                bssid: None,
                rssi: None,
            }]
        );
    }
}
