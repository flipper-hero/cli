//! `flipper`: an agent-friendly CLI for the Flipper Zero over USB and
//! Bluetooth LE. Every command prints human text by default and one JSON
//! document with `--json`, so agents and scripts can share the same surface.

mod connect;
mod output;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use flipper_core::api::{App, GpioPin, StorageInfo, UpdateResult};
use flipper_core::client::{Client, Transport};
use flipper_core::screen::FlipperKey;
use flipper_core::FlipperPath;

use crate::connect::{ConnectOptions, Link, TransportChoice};
use crate::output::{emit, fail, EXIT_OK};

#[derive(Parser)]
#[command(
    name = "flipper",
    about = "Drive a Flipper Zero over USB or Bluetooth LE, built for agents and humans alike.",
    version
)]
struct Cli {
    /// Machine-readable JSON output on stdout.
    #[arg(long, global = true)]
    json: bool,

    /// Transport to use (auto tries USB first, then Bluetooth).
    #[arg(long, value_enum, default_value = "auto", global = true)]
    transport: TransportChoice,

    /// Bluetooth device id or name from `flipper scan`.
    #[arg(long, global = true)]
    device: Option<String>,

    /// USB serial device path.
    #[arg(long, global = true)]
    port: Option<String>,

    /// Per-call RPC timeout in seconds.
    #[arg(long, default_value_t = 20, global = true)]
    timeout: u64,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan for Flippers over Bluetooth.
    Scan {
        /// How long to listen, in seconds.
        #[arg(short, long, default_value_t = 4)]
        duration: u64,
    },
    /// List USB serial ports that look like a Flipper.
    Ports,
    /// Device, power and storage facts in one shot.
    Info,
    /// Battery level and charging state.
    Battery,
    /// List a directory on the Flipper.
    Ls {
        /// Directory to list (default /ext).
        path: Option<String>,
    },
    /// Stat one file or directory.
    Stat { path: String },
    /// Print a file's bytes to stdout.
    Cat { path: String },
    /// Download a file from the Flipper.
    Get {
        path: String,
        /// Local destination (default: the file's name in this directory).
        out: Option<PathBuf>,
        /// Refuse files larger than this many bytes.
        #[arg(long, default_value_t = 64 * 1024 * 1024)]
        max_bytes: usize,
    },
    /// Upload a local file to the Flipper.
    Put {
        local: PathBuf,
        /// Destination path (default: /ext/<file name>).
        remote: Option<String>,
    },
    /// Create a directory (parents included).
    Mkdir { path: String },
    /// Delete a file or directory.
    Rm {
        path: String,
        /// Delete directories with their contents.
        #[arg(short, long)]
        recursive: bool,
    },
    /// Rename or move on the Flipper's storage.
    Mv { from: String, to: String },
    /// Transmit a Sub-GHz or infrared signal once.
    Tx {
        /// A .sub or .ir file on the Flipper.
        file: String,
        /// Named button inside an infrared remote file.
        #[arg(short, long, default_value = "")]
        button: String,
        /// How long to hold the send button, in milliseconds.
        #[arg(long, default_value_t = 400)]
        hold_ms: u64,
    },
    /// Emulate an NFC, RFID or iButton file until stopped.
    Emulate {
        file: String,
        /// Stop after this many seconds instead of waiting for Ctrl-C.
        #[arg(short, long)]
        duration: Option<u64>,
    },
    /// Load a Bad KB script (the operator presses Run on the device).
    BadUsb {
        #[command(subcommand)]
        action: BadUsbAction,
    },
    /// Start or stop an app by its loader name.
    App {
        #[command(subcommand)]
        action: AppAction,
    },
    /// Capture the Flipper's screen.
    Screen {
        /// Write a PNG here (default flipper-screen.png).
        out: Option<PathBuf>,
        /// Print an ASCII rendering instead of writing a PNG.
        #[arg(long)]
        ascii: bool,
    },
    /// Press a button on the device.
    Press {
        /// up, down, left, right, ok, back
        key: String,
        /// Hold it down (long press).
        #[arg(short, long)]
        long: bool,
    },
    /// Beep, blink and vibrate so the device can be found.
    Alert,
    /// Restart the Flipper.
    Reboot {
        /// Restart into the on-device updater instead.
        #[arg(long)]
        updater: bool,
    },
    /// Stage a firmware update package already on the SD card, then reboot
    /// into the updater.
    Update {
        /// Path to the update manifest, e.g. /ext/update/manifest.txt.
        manifest: String,
    },
    /// Send one raw protobuf request as JSON, get responses as JSON lines.
    Raw {
        /// The request JSON, e.g. {"content":{"systemPingRequest":{}}}.
        request: Option<String>,
        /// Or read the request JSON from a file.
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// GPIO header pins.
    Gpio {
        #[command(subcommand)]
        action: GpioAction,
    },
}

#[derive(Subcommand)]
enum BadUsbAction {
    /// Load a script; the firmware waits for the operator to press Run.
    Load { file: String },
}

#[derive(Subcommand)]
enum AppAction {
    Start {
        /// Loader name, e.g. "Sub-GHz", "Infrared", "NFC".
        name: String,
        /// Arguments passed to the app.
        args: Option<String>,
    },
    Exit,
    Error,
}

#[derive(Subcommand)]
enum GpioAction {
    /// Put a pin in input or output mode.
    Set {
        /// pc0, pc1, pc3, pb2, pb3, pa4, pa6, pa7
        pin: String,
        /// Input mode (default is output).
        #[arg(long, conflicts_with = "output")]
        input: bool,
        /// Explicitly select output mode.
        #[arg(long)]
        output: bool,
        /// Pull resistor for input mode: up, down or none.
        #[arg(long, default_value = "none")]
        pull: String,
    },
    /// Read an input pin (prints high or low).
    Read { pin: String },
    /// Drive an output pin (high or low).
    Write { pin: String, level: String },
}

fn parse_key(name: &str) -> anyhow::Result<FlipperKey> {
    FlipperKey::parse_key(name)
        .with_context(|| format!("unknown key '{name}' (up|down|left|right|ok|back)"))
}

fn parse_pin(name: &str) -> anyhow::Result<GpioPin> {
    GpioPin::parse_name(name).with_context(|| format!("unknown pin '{name}'"))
}

fn parse_pull(name: &str) -> anyhow::Result<Option<bool>> {
    match name.to_ascii_lowercase().as_str() {
        "up" => Ok(Some(true)),
        "down" => Ok(Some(false)),
        "none" | "no" | "float" => Ok(None),
        other => bail!("unknown pull '{other}' (up|down|none)"),
    }
}

/// True for errors that mean the link itself died, which is the expected
/// outcome when Bad KB takes over the USB port.
fn error_is_link_drop(error: &flipper_core::Error) -> bool {
    matches!(
        error,
        flipper_core::Error::NotConnected
            | flipper_core::Error::Timeout
            | flipper_core::Error::Transport(_)
    )
}

fn file_app(file: &str) -> Option<App> {
    App::for_extension(file_extension_of(file))
}

fn file_extension_of(file: &str) -> &str {
    FlipperPath::last_component(file)
        .rsplit('.')
        .next()
        .unwrap_or("")
}

fn app_for_transmit(file: &str) -> anyhow::Result<App> {
    match file_app(file) {
        Some(App::SubGhz) | Some(App::Infrared) => Ok(file_app(file).unwrap()),
        Some(_) => bail!("tx sends .sub and .ir files; use 'flipper emulate' for cards"),
        None => bail!("tx expects a .sub or .ir file"),
    }
}

fn app_for_emulate(file: &str) -> anyhow::Result<App> {
    match file_app(file) {
        Some(app @ (App::Nfc | App::Rfid | App::IButton)) => Ok(app),
        Some(_) => bail!("emulate runs .nfc, .rfid and .ibtn files; use 'flipper tx' for signals"),
        None => bail!("emulate expects a .nfc, .rfid or .ibtn file"),
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = run(cli).await;
    std::process::exit(code);
}

async fn run(cli: Cli) -> i32 {
    let json = cli.json;
    match dispatch(&cli).await {
        Ok(code) => code,
        Err(error) => fail(json, &error),
    }
}

async fn dispatch(cli: &Cli) -> anyhow::Result<i32> {
    let json = cli.json;
    let options = ConnectOptions {
        transport: cli.transport,
        device: cli.device.clone(),
        port: cli.port.clone(),
        timeout: Duration::from_secs(cli.timeout),
    };

    // Commands that do not need a connection.
    match &cli.command {
        Command::Scan { duration } => return scan(Duration::from_secs(*duration), json).await,
        Command::Ports => return ports(json),
        _ => {}
    }

    let link = Link::open(&options)
        .await
        .context("connecting to the flipper failed")?;

    let result: anyhow::Result<i32> = match link {
        Link::Usb(client) => {
            let result = run_command(&client, cli, json).await;
            client.stop().await;
            result
        }
        #[cfg(feature = "ble")]
        Link::Ble(client) => {
            let result = run_command(&client, cli, json).await;
            client.stop().await;
            result
        }
    };
    result
}

async fn run_command<T: Transport>(
    client: &Client<T>,
    cli: &Cli,
    json: bool,
) -> anyhow::Result<i32> {
    match &cli.command {
        Command::Info => info(client, json).await,
        Command::Battery => battery(client, json).await,
        Command::Ls { path } => ls(client, path.as_deref().unwrap_or("/ext"), json).await,
        Command::Stat { path } => stat(client, path, json).await,
        Command::Cat { path } => cat(client, path, json).await,
        Command::Get {
            path,
            out,
            max_bytes,
        } => get(client, path, out.as_ref(), *max_bytes, json).await,
        Command::Put { local, remote } => put(client, local, remote.as_deref(), json).await,
        Command::Mkdir { path } => {
            client
                .make_directories(path)
                .await
                .context("mkdir failed")?;
            emit(
                json,
                serde_json::json!({ "path": path }),
                format!("created {path}\n"),
            );
            Ok(EXIT_OK)
        }
        Command::Rm { path, recursive } => {
            client.delete(path, *recursive).await.context("rm failed")?;
            emit(
                json,
                serde_json::json!({ "path": path, "recursive": recursive }),
                format!("deleted {path}\n"),
            );
            Ok(EXIT_OK)
        }
        Command::Mv { from, to } => {
            client.rename(from, to).await.context("mv failed")?;
            emit(
                json,
                serde_json::json!({ "from": from, "to": to }),
                format!("moved {from} to {to}\n"),
            );
            Ok(EXIT_OK)
        }
        Command::Tx {
            file,
            button,
            hold_ms,
        } => {
            let app = app_for_transmit(file)?;
            client
                .transmit(app, file, button, Duration::from_millis(*hold_ms))
                .await
                .context("transmit failed")?;
            emit(
                json,
                serde_json::json!({ "file": file, "app": app.loader_name(), "button": button, "hold_ms": hold_ms }),
                format!("sent {file}\n"),
            );
            Ok(EXIT_OK)
        }
        Command::Emulate { file, duration } => emulate(client, file, *duration, json).await,
        Command::BadUsb {
            action: BadUsbAction::Load { file },
        } => {
            // The Bad KB app takes over the USB port as a HID keyboard, which
            // ends this session immediately. Warn before doing it.
            if cli.transport != TransportChoice::Ble {
                eprintln!(
                    "note: loading Bad KB switches the device's USB to keyboard mode; this CLI connection will drop (use --transport ble to keep the link)"
                );
            }
            let load_result = client.load_bad_keyboard_script(file).await;
            match load_result {
                Ok(()) => {}
                Err(error) if error_is_link_drop(&error) => {
                    // Expected over USB: the app opened and took the port.
                    emit(
                        json,
                        serde_json::json!({ "file": file, "started": false, "link": "usb taken over by Bad KB" }),
                        format!("loaded {file}; press Run on the Flipper to start it; the USB connection ended when Bad KB opened\n"),
                    );
                    return Ok(EXIT_OK);
                }
                Err(error) => return Err(error).context("badusb load failed"),
            }
            emit(
                json,
                serde_json::json!({ "file": file, "started": false }),
                format!("loaded {file}; press Run on the Flipper to start it\n"),
            );
            Ok(EXIT_OK)
        }
        Command::App { action } => app(client, action, json).await,
        Command::Screen { out, ascii } => screen(client, out.as_ref(), *ascii, json).await,
        Command::Press { key, long } => {
            let key = parse_key(key)?;
            client.press(key, *long).await.context("press failed")?;
            emit(
                json,
                serde_json::json!({ "key": format!("{key:?}"), "long": long }),
                format!("pressed {:?}{}\n", key, if *long { " (long)" } else { "" }),
            );
            Ok(EXIT_OK)
        }
        Command::Alert => {
            client.play_alert().await.context("alert failed")?;
            emit(json, serde_json::json!({}), "beeped and blinked\n");
            Ok(EXIT_OK)
        }
        Command::Reboot { updater } => {
            if *updater {
                client
                    .reboot_into(flipper_core::pb::system::reboot_request::RebootMode::Update)
                    .await?;
            } else {
                client.reboot().await?;
            }
            emit(
                json,
                serde_json::json!({ "mode": if *updater { "update" } else { "os" } }),
                "rebooting\n",
            );
            Ok(EXIT_OK)
        }
        Command::Update { manifest } => {
            let result = client
                .request_update(manifest)
                .await
                .context("update staging failed")?;
            match result {
                UpdateResult::Ok => {
                    client
                        .reboot_into(flipper_core::pb::system::reboot_request::RebootMode::Update)
                        .await?;
                    emit(
                        json,
                        serde_json::json!({ "staged": true, "rebooting": true }),
                        format!("staged {manifest}; rebooting into the updater\n"),
                    );
                }
                UpdateResult::Rejected(reason) => {
                    bail!("the flipper rejected the package: {reason}");
                }
            }
            Ok(EXIT_OK)
        }
        Command::Raw { request, file } => {
            let body = match (request, file) {
                (Some(text), _) => text.clone(),
                (None, Some(path)) => {
                    std::fs::read_to_string(path).context("reading request file failed")?
                }
                (None, None) => bail!("give the request as an argument or with --file"),
            };
            let answer = client.raw_json(&body).await.context("raw request failed")?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "ok": true, "data": { "responses": answer.lines().map(serde_json::Value::from).collect::<Vec<_>>() } })
                );
            } else {
                println!("{answer}");
            }
            Ok(EXIT_OK)
        }
        Command::Gpio { action } => gpio(client, action, json).await,
        Command::Scan { .. } | Command::Ports => unreachable!("handled without a connection"),
    }
}

#[cfg(feature = "ble")]
async fn scan(duration: Duration, json: bool) -> anyhow::Result<i32> {
    let adapter = flipper_ble::first_adapter()
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("no bluetooth adapter")?;
    eprintln!("scanning for {}s...", duration.as_secs());
    let found = flipper_ble::scan(&adapter, duration)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let value = serde_json::json!(found
        .iter()
        .map(|f| serde_json::json!({ "id": f.id, "name": f.name, "rssi": f.rssi, "paired": f.paired }))
        .collect::<Vec<_>>());
    let human = {
        let mut text = String::new();
        for f in &found {
            text.push_str(&format!("{:<40} {:>4} dBm  {}\n", f.name, f.rssi, f.id));
        }
        if found.is_empty() {
            text.push_str("no flippers found; is bluetooth on and the device advertising?\n");
        }
        text
    };
    emit(json, value, human);
    Ok(EXIT_OK)
}

#[cfg(not(feature = "ble"))]
async fn scan(_duration: Duration, _json: bool) -> anyhow::Result<i32> {
    bail!(
        "this build has no Bluetooth support (btleplug has no backend for this platform); use USB"
    )
}

fn ports(json: bool) -> anyhow::Result<i32> {
    let found = flipper_usb::list_flippers().map_err(|error| anyhow::anyhow!("{error}"))?;
    let value = serde_json::json!(found
        .iter()
        .map(|f| serde_json::json!({ "port": f.port, "serial": f.serial }))
        .collect::<Vec<_>>());
    let human = found
        .iter()
        .map(|f| format!("{}  {}\n", f.port, f.serial.as_deref().unwrap_or("")))
        .collect::<String>()
        + if found.is_empty() {
            "no flippers over USB\n"
        } else {
            ""
        };
    emit(json, value, human);
    Ok(EXIT_OK)
}

async fn info<T: Transport>(client: &Client<T>, json: bool) -> anyhow::Result<i32> {
    let device = client.device_info().await.context("device info failed")?;
    let power = client.power_info().await.unwrap_or_default();
    let storage = client.storage_info("/ext").await.unwrap_or(StorageInfo {
        total_space: 0,
        free_space: 0,
    });
    let mut map = serde_json::Map::new();
    for (key, value) in device.iter().chain(power.iter()) {
        map.insert(key.clone(), serde_json::Value::String(value.clone()));
    }
    map.insert(
        "storage".into(),
        serde_json::json!({ "total": storage.total_space, "free": storage.free_space }),
    );
    let human = device
        .iter()
        .chain(power.iter())
        .map(|(key, value)| format!("{key:<28} {value}\n"))
        .collect::<String>()
        + &format!(
            "storage free/total           {}/{}\n",
            storage.free_space, storage.total_space
        );
    emit(json, serde_json::Value::Object(map), human);
    Ok(EXIT_OK)
}

async fn battery<T: Transport>(client: &Client<T>, json: bool) -> anyhow::Result<i32> {
    let power = client.power_info().await.context("power info failed")?;
    let get = |key: &str| {
        power
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value.clone())
    };
    let level = get("charge_level").unwrap_or_else(|| "?".into());
    let state = get("charge_state").unwrap_or_else(|| "unknown".into());
    let health = get("battery_health");
    emit(
        json,
        serde_json::json!({ "level": level, "state": state, "health": health }),
        format!("battery {level}% ({state})\n"),
    );
    Ok(EXIT_OK)
}

async fn ls<T: Transport>(client: &Client<T>, path: &str, json: bool) -> anyhow::Result<i32> {
    let entries = client.list(path).await.context("ls failed")?;
    let value = serde_json::json!(entries
        .iter()
        .map(|entry| serde_json::json!({
            "name": entry.name,
            "dir": entry.is_directory,
            "size": entry.size,
        }))
        .collect::<Vec<_>>());
    let human = entries
        .iter()
        .map(|entry| {
            if entry.is_directory {
                format!("{:<40} <dir>\n", format!("{}/", entry.name))
            } else {
                format!("{:<40} {}\n", entry.name, entry.size)
            }
        })
        .collect::<String>();
    emit(json, value, human);
    Ok(EXIT_OK)
}

async fn stat<T: Transport>(client: &Client<T>, path: &str, json: bool) -> anyhow::Result<i32> {
    let entry = client.stat(path).await.context("stat failed")?;
    emit(
        json,
        serde_json::json!({ "path": path, "dir": entry.is_directory, "size": entry.size }),
        format!(
            "{path}: {} ({} bytes)\n",
            if entry.is_directory { "dir" } else { "file" },
            entry.size
        ),
    );
    Ok(EXIT_OK)
}

async fn cat<T: Transport>(client: &Client<T>, path: &str, json: bool) -> anyhow::Result<i32> {
    let data = client.read(path).await.context("cat failed")?;
    if json {
        use base64::Engine as _;
        emit(
            json,
            serde_json::json!({ "path": path, "size": data.len(), "data_base64": base64::engine::general_purpose::STANDARD.encode(&data) }),
            String::new(),
        );
    } else {
        use std::io::Write as _;
        std::io::stdout()
            .write_all(&data)
            .context("writing to stdout failed")?;
    }
    Ok(EXIT_OK)
}

async fn get<T: Transport>(
    client: &Client<T>,
    path: &str,
    out: Option<&PathBuf>,
    max_bytes: usize,
    json: bool,
) -> anyhow::Result<i32> {
    let data = client
        .read_with_limit(path, max_bytes)
        .await
        .context("download failed")?;
    let destination = out
        .cloned()
        .unwrap_or_else(|| PathBuf::from(FlipperPath::last_component(path)));
    std::fs::write(&destination, &data)
        .with_context(|| format!("writing {} failed", destination.display()))?;
    emit(
        json,
        serde_json::json!({ "path": path, "out": destination.display().to_string(), "size": data.len() }),
        format!("wrote {} bytes to {}\n", data.len(), destination.display()),
    );
    Ok(EXIT_OK)
}

async fn put<T: Transport>(
    client: &Client<T>,
    local: &PathBuf,
    remote: Option<&str>,
    json: bool,
) -> anyhow::Result<i32> {
    let data =
        std::fs::read(local).with_context(|| format!("reading {} failed", local.display()))?;
    let destination = match remote {
        Some(path) => path.to_owned(),
        None => FlipperPath::join(
            "/ext",
            FlipperPath::last_component(&local.display().to_string()),
        ),
    };
    eprintln!("uploading {} bytes to {destination}...", data.len());
    client
        .write_with_progress(
            &destination,
            &data,
            Some(&|done, total| {
                eprint!("\r{done}/{total} bytes");
            }),
        )
        .await
        .context("upload failed")?;
    eprintln!();
    emit(
        json,
        serde_json::json!({ "local": local.display().to_string(), "path": destination, "size": data.len() }),
        format!("uploaded {} bytes to {destination}\n", data.len()),
    );
    Ok(EXIT_OK)
}

async fn emulate<T: Transport>(
    client: &Client<T>,
    file: &str,
    duration: Option<u64>,
    json: bool,
) -> anyhow::Result<i32> {
    let app = app_for_emulate(file)?;
    client.emulate(app, file).await.context("emulate failed")?;
    emit(
        json,
        serde_json::json!({ "file": file, "app": app.loader_name(), "emulating": true }),
        format!("emulating {file}; stop with Ctrl-C\n"),
    );

    let stopped = async {
        match duration {
            Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    };
    tokio::select! {
        _ = stopped => {}
        _ = client.closed() => {
            eprintln!("link lost");
            return Ok(EXIT_OK);
        }
    }

    let _ = client.exit_app().await;
    emit(
        json,
        serde_json::json!({ "file": file, "emulating": false }),
        format!("stopped emulating {file}\n"),
    );
    Ok(EXIT_OK)
}

async fn app<T: Transport>(
    client: &Client<T>,
    action: &AppAction,
    json: bool,
) -> anyhow::Result<i32> {
    match action {
        AppAction::Start { name, args } => {
            client
                .start_app(name, args.as_deref().unwrap_or(""))
                .await
                .context("app start failed")?;
            emit(
                json,
                serde_json::json!({ "name": name, "args": args }),
                format!("started {name}\n"),
            );
        }
        AppAction::Exit => {
            client.exit_app().await.map_err(|error| {
                if matches!(&error, flipper_core::Error::Rpc(status) if status.contains("APP_NOT_RUNNING"))
                {
                    anyhow::anyhow!(
                        "app exit failed: {error}; an RPC-started app belongs to the connection \
                         that started it, so a new 'flipper' invocation cannot close another one's \
                         app — exit it on the device, or use tx/emulate which manage their own app"
                    )
                } else {
                    anyhow::Error::from(error).context("app exit failed")
                }
            })?;
            emit(json, serde_json::json!({}), "closed the app\n");
        }
        AppAction::Error => {
            let error = client.app_error().await.context("app error failed")?;
            emit(
                json,
                serde_json::json!({ "error": error }),
                error
                    .map(|text| format!("app error: {text}\n"))
                    .unwrap_or_else(|| "no app error\n".into()),
            );
        }
    }
    Ok(EXIT_OK)
}

async fn screen<T: Transport>(
    client: &Client<T>,
    out: Option<&PathBuf>,
    ascii: bool,
    json: bool,
) -> anyhow::Result<i32> {
    let frame = client
        .capture_screen(Duration::from_secs(5))
        .await
        .context("screen capture failed")?;

    if ascii {
        let art = frame.ascii();
        if json {
            emit(
                json,
                serde_json::json!({ "ascii": art, "orientation": format!("{:?}", frame.orientation) }),
                String::new(),
            );
        } else {
            print!("{art}");
        }
        return Ok(EXIT_OK);
    }

    let destination = out
        .cloned()
        .unwrap_or_else(|| PathBuf::from("flipper-screen.png"));
    let (width, height, gray) = frame.grayscale();
    let file = std::fs::File::create(&destination)
        .with_context(|| format!("creating {} failed", destination.display()))?;
    let mut encoder = png::Encoder::new(file, width as u32, height as u32);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("png header failed")?;
    writer.write_image_data(&gray).context("png write failed")?;
    emit(
        json,
        serde_json::json!({ "file": destination.display().to_string(), "width": width, "height": height }),
        format!("wrote {}\n", destination.display()),
    );
    Ok(EXIT_OK)
}

async fn gpio<T: Transport>(
    client: &Client<T>,
    action: &GpioAction,
    json: bool,
) -> anyhow::Result<i32> {
    match action {
        GpioAction::Set {
            pin,
            input,
            output: _,
            pull,
        } => {
            let pin = parse_pin(pin)?;
            let pull = if *input { parse_pull(pull)? } else { None };
            client
                .gpio_set_mode(pin, !*input, pull)
                .await
                .context("gpio set failed")?;
            let mode = if *input { "input" } else { "output" };
            emit(
                json,
                serde_json::json!({ "pin": format!("{pin:?}"), "mode": mode }),
                format!("{pin:?} is now {mode}\n"),
            );
        }
        GpioAction::Read { pin } => {
            let pin = parse_pin(pin)?;
            let high = client.gpio_read(pin).await.map_err(|error| {
                if matches!(&error, flipper_core::Error::Rpc(status) if status.contains("GPIO_MODE"))
                {
                    anyhow::anyhow!(
                        "gpio read failed: {error}; put the pin in input mode first: flipper gpio set {} --input",
                        format!("{pin:?}").to_lowercase()
                    )
                } else {
                    anyhow::Error::from(error).context("gpio read failed")
                }
            })?;
            let level = if high { "high" } else { "low" };
            emit(
                json,
                serde_json::json!({ "pin": format!("{pin:?}"), "level": level }),
                format!("{level}\n"),
            );
        }
        GpioAction::Write { pin, level } => {
            let pin = parse_pin(pin)?;
            let high = match level.to_ascii_lowercase().as_str() {
                "high" | "1" | "on" | "true" => true,
                "low" | "0" | "off" | "false" => false,
                other => bail!("unknown level '{other}' (high|low)"),
            };
            client
                .gpio_write(pin, high)
                .await
                .context("gpio write failed")?;
            emit(
                json,
                serde_json::json!({ "pin": format!("{pin:?}"), "level": if high { "high" } else { "low" } }),
                format!("{:?} driven {}\n", pin, if high { "high" } else { "low" }),
            );
        }
    }
    Ok(EXIT_OK)
}
