//! Hardware test suite: runs the full API surface against a real Flipper
//! attached over USB. Opt-in, so plain `cargo test` stays hardware-free:
//!
//! ```sh
//! cargo test -p flipper-cli --test hardware -- --ignored --nocapture
//! ```
//!
//! Set `FLIPPER_TEST_PORT` to force a specific serial device; otherwise the
//! first Flipper-looking port is used. Without hardware every test prints a
//! skip notice and passes, so this is safe to run anywhere.
//!
//! Tests confine their writes to `/ext/.flipper-test` and clean up after
//! themselves. Two things are deliberately NOT tested, both for the same
//! reason — they take the device away from under the suite:
//!
//! - `tx` on a `.sub` file sends actual RF into the room.
//! - `bad-usb load` switches the device's USB to keyboard mode, which cuts
//!   the very link the suite runs on and needs a manual Back press on the
//!   device to recover. (The load path itself is covered in `device.rs`
//!   against the simulated Flipper.)

use std::sync::LazyLock;
use std::time::Duration;

use flipper_core::api::GpioPin;
use flipper_core::api::{App, StorageInfo, UpdateResult};
use flipper_core::client::Client;
use flipper_core::screen::FlipperKey;
use flipper_core::{Error, FlipperPath};
use flipper_usb::UsbTransport;
use tokio::sync::{Mutex, OwnedMutexGuard};

const TIMEOUT: Duration = Duration::from_secs(15);

/// One USB port, one conversation: tests in this binary must not interleave
/// their RPC sessions, even when cargo runs them in parallel.
static PORT_LOCK: LazyLock<std::sync::Arc<Mutex<()>>> =
    LazyLock::new(|| std::sync::Arc::new(Mutex::new(())));

/// A live connection plus the port lock that protects it.
struct MaybeDevice {
    _guard: Option<OwnedMutexGuard<()>>,
    client: Option<Client<UsbTransport>>,
}

async fn connect() -> MaybeDevice {
    let guard = PORT_LOCK.clone().lock_owned().await;
    let port: Option<flipper_usb::UsbFlipper> = match std::env::var("FLIPPER_TEST_PORT") {
        Ok(path) => Some(flipper_usb::UsbFlipper {
            port: path,
            serial: None,
        }),
        Err(_) => flipper_usb::list_flippers()
            .ok()
            .and_then(|ports| ports.into_iter().next()),
    };
    let Some(port) = port else {
        eprintln!("skip: no flipper found over USB");
        return MaybeDevice {
            _guard: Some(guard),
            client: None,
        };
    };
    eprintln!("device: {} ({:?})", port.port, port.serial);
    let port_path = port.port.clone();
    // macOS claims the CDC port exclusively and takes a moment to release it
    // after the previous test's process/runtime tears down; retry briefly.
    // Even past open, the first exchange can hit a device still settling
    // after the previous test closed an app, so settle-check with a ping and
    // reconnect once on a dead link.
    for attempt in 0..3 {
        let port_path = port_path.clone();
        let transport = tokio::task::spawn_blocking(move || {
            let mut last = None;
            for _ in 0..8 {
                match UsbTransport::open(Some(&port_path)) {
                    Ok(transport) => return Ok(transport),
                    Err(error) => {
                        last = Some(error);
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
            Err(last.expect("at least one attempt"))
        })
        .await
        .expect("join");
        let Ok(transport) = transport else {
            continue;
        };
        let client = Client::start(transport, TIMEOUT);
        if client.ping().await.is_ok() {
            return MaybeDevice {
                _guard: Some(guard),
                client: Some(client),
            };
        }
        eprintln!("link settled dead (attempt {}), reconnecting", attempt + 1);
        client.stop().await;
    }
    panic!("could not establish a working connection to the flipper");
}

/// Runs the body only when hardware is present.
macro_rules! on_device {
    ($name:ident, $client:ident => $body:expr) => {
        #[tokio::test]
        #[ignore = "needs a real flipper over USB"]
        async fn $name() {
            let device = connect().await;
            let Some($client) = device.client.as_ref() else {
                return;
            };
            $body;
        }
    };
}

on_device!(ping, client => {
    client.ping().await.expect("ping");
});

on_device!(device_and_power_info, client => {
    let info = client.device_info().await.expect("device info");
    // Firmware builds differ in which facts they report; every build answers
    // with SOMETHING and the power info is stable.
    assert!(!info.is_empty(), "device info came back empty");
    let power = client.power_info().await.expect("power info");
    assert!(power.iter().any(|(key, _)| key == "charge_level"), "charge_level missing");
});

on_device!(storage_info_sane, client => {
    let info: StorageInfo = client.storage_info("/ext").await.expect("storage info");
    assert!(info.total_space > 0);
    assert!(info.free_space <= info.total_space);
});

on_device!(file_round_trip_multi_chunk_and_integrity, client => {
    let dir = "/ext/.flipper-test";
    client.make_directories(dir).await.expect("mkdirs");

    // Deterministic pseudo-random payload spanning many 512-byte chunks.
    let payload: Vec<u8> = (0..200_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    client
        .write(&format!("{dir}/big.bin"), &payload)
        .await
        .expect("write");

    let downloaded = client
        .read_with_limit(&format!("{dir}/big.bin"), payload.len())
        .await
        .expect("read");
    assert_eq!(downloaded.len(), payload.len(), "size mismatch");
    assert_eq!(fnv1a64(&downloaded), fnv1a64(&payload), "content mismatch");

    // Empty files round-trip too.
    client
        .write(&format!("{dir}/empty.txt"), b"")
        .await
        .expect("empty write");
    assert!(
        client
            .read(&format!("{dir}/empty.txt"))
            .await
            .expect("empty read")
            .is_empty()
    );

    client.delete(dir, true).await.expect("cleanup");
    assert!(client.stat(dir).await.is_err(), "test dir should be gone");
});

on_device!(rename_and_move, client => {
    let dir = "/ext/.flipper-test";
    client.make_directories(dir).await.expect("mkdirs");
    client
        .write(&format!("{dir}/a.txt"), b"move me")
        .await
        .expect("write");
    client
        .rename(&format!("{dir}/a.txt"), &format!("{dir}/b.txt"))
        .await
        .expect("rename");
    let data = client
        .read(&format!("{dir}/b.txt"))
        .await
        .expect("read after move");
    assert_eq!(data, b"move me");
    client.delete(dir, true).await.expect("cleanup");
});

on_device!(app_start_exit_cycle, client => {
    // The full app-control handshake, in one session: an RPC-mode app binds
    // to the session that started it. NFC is present on every build; the
    // Sub-GHz loader name varies on external-FAP firmware builds.
    client.start_app("NFC", "RPC").await.expect("app start");
    tokio::time::sleep(Duration::from_millis(500)).await;
    client.exit_app().await.expect("app exit");
});

on_device!(emulate_nfc_loads_and_stops, client => {
    // Find any .nfc file on the card, emulate it briefly, exit the app.
    // NFC emulation is passive: the device generates a field, sends nothing.
    let Some(nfc_file) = find_file(client, "/ext/nfc", "nfc").await else {
        eprintln!("skip: no .nfc file on the device");
        return;
    };
    client.emulate(App::Nfc, &nfc_file).await.expect("emulate");
    tokio::time::sleep(Duration::from_secs(1)).await;
    client.exit_app().await.expect("exit after emulate");
});

on_device!(screen_capture_has_content, client => {
    let frame = client
        .capture_screen(Duration::from_secs(5))
        .await
        .expect("capture");
    assert_eq!(frame.buffer.len(), 1024);
    assert_eq!(frame.pixels().len(), 128 * 64);
});

on_device!(press_changes_the_screen, client => {
    let before = client
        .capture_screen(Duration::from_secs(5))
        .await
        .expect("capture 1")
        .buffer;
    client.press(FlipperKey::Ok, false).await.expect("press ok");
    tokio::time::sleep(Duration::from_millis(700)).await;
    let after = client
        .capture_screen(Duration::from_secs(5))
        .await
        .expect("capture 2")
        .buffer;
    client.press(FlipperKey::Back, false).await.expect("press back");
    assert_ne!(before, after, "pressing OK must change the screen");
});

on_device!(gpio_pin_cycle, client => {
    client
        .gpio_set_mode(GpioPin::Pa7, true, None)
        .await
        .expect("output mode");
    client.gpio_write(GpioPin::Pa7, false).await.expect("write low");
    client
        .gpio_set_mode(GpioPin::Pa7, false, Some(false))
        .await
        .expect("input mode");
    let high = client.gpio_read(GpioPin::Pa7).await.expect("read");
    assert!(!high, "pulled-down pin must read low");
    // Restore floating input like we found it.
    client.gpio_set_mode(GpioPin::Pa7, false, None).await.ok();
});

on_device!(raw_json_round_trip, client => {
    let answer = client
        .raw_json(r#"{"content":{"systemPingRequest":{"data":"AQIDBA=="}}}"#)
        .await
        .expect("raw ping");
    assert!(answer.contains("systemPingResponse"));
    assert!(answer.contains("AQIDBA=="));
});

on_device!(update_rejects_missing_manifest, client => {
    match client
        .request_update("/ext/.flipper-test/nope/manifest.txt")
        .await
    {
        Err(Error::Rpc(status)) => assert!(status.contains("INVALID_PARAMETERS")),
        Err(Error::InvalidPath(_)) => {}
        Ok(UpdateResult::Rejected(_)) => {}
        other => panic!("expected rejection, got {other:?}"),
    }
});

/// Recursively finds the first file with `extension` under `dir`.
async fn find_file(client: &Client<UsbTransport>, dir: &str, extension: &str) -> Option<String> {
    for entry in client.list(dir).await.ok()? {
        let path = FlipperPath::join(dir, &entry.name);
        if entry.is_directory {
            if let Some(found) = Box::pin(find_file(client, &path, extension)).await {
                return Some(found);
            }
        } else if path.ends_with(&format!(".{extension}")) {
            return Some(path);
        }
    }
    None
}

/// FNV-1a 64: catches transfer corruption without pulling a crypto crate.
fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
