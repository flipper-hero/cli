//! Device API tests against the simulated Flipper: no device, no network,
//! mirroring the FlipperKit and DeviceTests conventions of the iOS app.

mod sim;

use std::time::Duration;

use sim::{setup, SimulatedFlipper};

use flipper_core::api::{App, GpioPin};
use flipper_core::error::Error;
use flipper_core::pb::gui::InputType;
use flipper_core::pb::main::Content;
use flipper_core::screen::{FlipperKey, Orientation};
use flipper_core::Client;

const FAST: Duration = Duration::from_millis(200);

async fn connected() -> (Client<sim::TestTransport>, SimulatedFlipper) {
    let (transport, sim) = setup();
    let client = Client::start(transport, FAST);
    (client, sim)
}

fn logged_contents(sim: &SimulatedFlipper) -> Vec<String> {
    sim.log
        .lock()
        .unwrap()
        .iter()
        .map(|message| match &message.content {
            Some(Content::StorageStatRequest(_)) => "stat".to_owned(),
            Some(Content::StorageWriteRequest(_)) => "write".to_owned(),
            Some(Content::AppStartRequest(start)) => format!("start:{}/{}", start.name, start.args),
            Some(Content::AppLoadFileRequest(load)) => format!("load:{}", load.path),
            Some(Content::AppButtonPressRequest(press)) => format!("press:{}", press.args),
            Some(Content::AppButtonReleaseRequest(_)) => "release".to_owned(),
            Some(Content::AppExitRequest(_)) => "exit".to_owned(),
            Some(Content::GuiSendInputEventRequest(event)) => {
                format!("input:{:?}/{:?}", event.key(), event.r#type())
            }
            Some(Content::SystemRebootRequest(_)) => "reboot".to_owned(),
            _ => "other".to_owned(),
        })
        .collect()
}

#[tokio::test]
async fn ping_round_trips() {
    let (client, _sim) = connected().await;
    client.ping().await.unwrap();
}

#[tokio::test]
async fn device_and_power_info() {
    let (client, _sim) = connected().await;
    let info = client.device_info().await.unwrap();
    assert!(info.contains(&("hardware_name".to_owned(), "Flipper Zero".to_owned())));
    let power = client.power_info().await.unwrap();
    assert!(power.contains(&("charge_level".to_owned(), "85".to_owned())));
}

#[tokio::test]
async fn list_sorts_directories_first() {
    let (client, sim) = connected().await;
    sim.put_file("/ext/b.sub", b"1");
    sim.put_file("/ext/a.sub", b"22");
    sim.put_dir("/ext/aaa");
    let entries = client.list("/ext").await.unwrap();
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["aaa", "a.sub", "b.sub"]);
    assert!(entries[0].is_directory);
}

#[tokio::test]
async fn stat_read_and_write_files() {
    let (client, sim) = connected().await;
    sim.put_file("/ext/existing.txt", b"hello flipper");
    let stat = client.stat("/ext/existing.txt").await.unwrap();
    assert_eq!(stat.size, 13);
    assert!(!stat.is_directory);
    assert_eq!(
        client.read("/ext/existing.txt").await.unwrap(),
        b"hello flipper"
    );

    // A multi-chunk write round-trips exactly.
    let payload: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
    let progress = std::sync::Mutex::new(Vec::new());
    client
        .write_with_progress(
            "/ext/big.bin",
            &payload,
            Some(&|done, total| {
                progress.lock().unwrap().push((done, total));
            }),
        )
        .await
        .unwrap();
    assert_eq!(sim.files.lock().unwrap()["/ext/big.bin"], payload);
    assert_eq!(
        *progress.lock().unwrap().last().unwrap(),
        (payload.len(), payload.len())
    );

    // Oversize reads are refused before any transfer.
    match client.read_with_limit("/ext/big.bin", 1024).await {
        Err(Error::Rpc(message)) => assert!(message.contains("limit")),
        other => panic!("expected Rpc error, got {other:?}"),
    }
    assert_eq!(
        client
            .read_with_limit("/ext/big.bin", payload.len())
            .await
            .unwrap(),
        payload
    );
}

#[tokio::test]
async fn mkdirs_tolerates_existing() {
    let (client, _sim) = connected().await;
    client
        .make_directories("/ext/deep/nested/dir")
        .await
        .unwrap();
    // Second run must succeed too: every level already exists.
    client
        .make_directories("/ext/deep/nested/dir")
        .await
        .unwrap();
}

#[tokio::test]
async fn mkdirs_creates_every_level_below_the_root() {
    let (client, sim) = connected().await;
    // The simulated FAT requires parents to exist, so a skipped level would
    // fail here — regression guard for the real filesystem.
    client.make_directories("/ext/one/two/three").await.unwrap();
    let dirs = sim.dirs.lock().unwrap();
    for expected in ["/ext/one", "/ext/one/two", "/ext/one/two/three"] {
        assert!(dirs.contains(expected), "missing {expected}");
    }
}

#[tokio::test]
async fn delete_requires_recursive_for_nonempty_dirs() {
    let (client, sim) = connected().await;
    sim.put_dir("/ext/dir");
    sim.put_file("/ext/dir/x.txt", b"x");
    assert!(matches!(
        client.delete("/ext/dir", false).await,
        Err(Error::Rpc(_))
    ));
    client.delete("/ext/dir", true).await.unwrap();
    assert!(client.stat("/ext/dir").await.is_err());
}

#[tokio::test]
async fn rename_moves_files() {
    let (client, sim) = connected().await;
    sim.put_file("/ext/old.txt", b"data");
    client.rename("/ext/old.txt", "/ext/new.txt").await.unwrap();
    let files = sim.files.lock().unwrap();
    assert!(!files.contains_key("/ext/old.txt"));
    assert_eq!(files["/ext/new.txt"], b"data");
}

#[tokio::test]
async fn transmit_once_presses_releases_and_exits() {
    let (client, sim) = connected().await;
    sim.put_file("/ext/garage.sub", b"RAW:315 1 -1");
    client
        .transmit_once(App::SubGhz, "/ext/garage.sub", "")
        .await
        .unwrap();
    let log = logged_contents(&sim);
    assert_eq!(
        log,
        vec![
            "stat".to_owned(),
            "start:Sub-GHz/RPC".to_owned(),
            "load:/ext/garage.sub".to_owned(),
            "press:".to_owned(),
            "release".to_owned(),
            "exit".to_owned(),
        ]
    );
}

#[tokio::test]
async fn transmit_with_named_infrared_button() {
    let (client, sim) = connected().await;
    sim.put_file("/ext/tv.ir", b"Filetype: IR signals file");
    client
        .transmit_once(App::Infrared, "/ext/tv.ir", "Power")
        .await
        .unwrap();
    let log = logged_contents(&sim);
    assert!(log.contains(&"press:Power".to_owned()));
}

#[tokio::test]
async fn transmit_rejects_wrong_file_kind_before_touching_hardware() {
    let (client, sim) = connected().await;
    match client.transmit_once(App::SubGhz, "/ext/card.nfc", "").await {
        Err(Error::InvalidPath(message)) => assert!(message.contains(".sub")),
        other => panic!("expected InvalidPath, got {other:?}"),
    }
    assert!(sim.log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn emulate_loads_until_stopped() {
    let (client, sim) = connected().await;
    sim.put_file("/ext/badge.nfc", b"Filetype: Flipper NFC device");
    client.emulate(App::Nfc, "/ext/badge.nfc").await.unwrap();
    assert_eq!(
        sim.loaded_path.lock().unwrap().as_deref(),
        Some("/ext/badge.nfc")
    );
    client.exit_app().await.unwrap();
    assert!(sim.app_running.lock().unwrap().is_none());
}

#[tokio::test]
async fn bad_keyboard_is_loaded_but_never_started() {
    let (client, sim) = connected().await;
    sim.put_file("/ext/payload.txt", b"DELAY 1000 STRING hello");
    client
        .load_bad_keyboard_script("/ext/payload.txt")
        .await
        .unwrap();
    let log = logged_contents(&sim);
    assert_eq!(
        log,
        vec![
            "stat".to_owned(),
            "start:Bad KB//ext/payload.txt".to_owned()
        ]
    );
    // No button press anywhere in the log: the operator presses Run.
    assert!(!log
        .iter()
        .any(|entry| entry.starts_with("press") || entry.starts_with("input")));
}

#[tokio::test]
async fn press_sends_the_three_events_pipelined() {
    let (client, sim) = connected().await;
    client.press(FlipperKey::Ok, false).await.unwrap();
    client.press(FlipperKey::Back, true).await.unwrap();
    let log = logged_contents(&sim);
    assert_eq!(
        log,
        vec![
            "input:Ok/Press".to_owned(),
            "input:Ok/Short".to_owned(),
            "input:Ok/Release".to_owned(),
            "input:Back/Press".to_owned(),
            "input:Back/Long".to_owned(),
            "input:Back/Release".to_owned(),
        ]
    );
    assert_eq!(InputType::Short as i32, 2);
}

#[tokio::test]
async fn capture_screen_gets_the_frame_pushed_on_stream_start() {
    let (client, _sim) = connected().await;
    let frame = client.capture_screen(Duration::from_secs(2)).await.unwrap();
    assert_eq!(frame.buffer.len(), 1024);
    assert_eq!(frame.orientation, Orientation::Normal);
    assert!(!frame.pixels().iter().any(|pixel| *pixel));
}

#[tokio::test]
async fn screen_stream_already_started_is_tolerated() {
    let (client, sim) = connected().await;
    // Mark the stream as already on so the firmware answers with the
    // VirtualDisplay error; acquiring must still succeed.
    sim.screen_stream
        .store(true, std::sync::atomic::Ordering::Relaxed);
    client.acquire_screen_stream().await.unwrap();
    client.release_screen_stream().await;
}

#[tokio::test]
async fn reboot_swallows_the_dropped_link() {
    let (client, sim) = connected().await;
    client.reboot().await.unwrap();
    assert!(sim.link_dropped.load(std::sync::atomic::Ordering::Relaxed));
    // After the device dropped the link, calls fail with NotConnected.
    loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        match client.ping().await {
            Err(Error::NotConnected) => break,
            Err(Error::Timeout) => continue,
            Ok(()) => continue,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}

#[tokio::test]
async fn gpio_round_trip() {
    let (client, _sim) = connected().await;
    client
        .gpio_set_mode(GpioPin::Pa7, false, Some(true))
        .await
        .unwrap();
    assert!(client.gpio_read(GpioPin::Pa7).await.unwrap());
    client.gpio_write(GpioPin::Pa7, true).await.unwrap();
}

#[tokio::test]
async fn request_update_checks_the_manifest() {
    let (client, sim) = connected().await;
    match client.request_update("/ext/missing/manifest.txt").await {
        Err(Error::Rpc(status)) => assert!(status.contains("INVALID_PARAMETERS")),
        other => panic!("expected Rpc error, got {other:?}"),
    }
    sim.put_file("/ext/update/manifest.txt", b"Firmware update manifest");
    assert_eq!(
        client
            .request_update("/ext/update/manifest.txt")
            .await
            .unwrap(),
        flipper_core::api::UpdateResult::Ok
    );
}

#[tokio::test]
async fn unsolicited_messages_reach_subscribers() {
    let (client, _sim) = connected().await;
    let mut unsolicited = client.unsolicited();
    client.acquire_screen_stream().await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(2), unsolicited.recv())
        .await
        .expect("frame within timeout")
        .unwrap();
    assert!(matches!(frame.content, Some(Content::GuiScreenFrame(_))));
    client.release_screen_stream().await;
}

#[tokio::test(start_paused = true)]
async fn timeouts_fail_calls_and_release_pending() {
    // A link with no device on the other end: requests are never answered.
    let client: Client<sim::TestTransport> =
        Client::start(sim::setup_silent(), Duration::from_millis(100));
    match client.ping().await {
        Err(Error::Timeout) => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[tokio::test]
async fn error_status_becomes_rpc_error() {
    let (client, _sim) = connected().await;
    match client.read("/ext/missing.txt").await {
        Err(Error::Rpc(status)) => assert_eq!(status, "ERROR_STORAGE_NOT_EXIST"),
        other => panic!("expected Rpc error, got {other:?}"),
    }
}

#[tokio::test]
async fn calls_are_serialized() {
    let (client, _sim) = connected().await;
    // Two concurrent calls must both succeed, in either completion order.
    let (a, b) = tokio::join!(client.device_info(), client.power_info());
    assert_eq!(a.unwrap().len(), 3);
    assert_eq!(b.unwrap().len(), 2);
}

#[tokio::test]
async fn raw_json_ping_round_trip() {
    let (client, _sim) = connected().await;
    let answer = client
        .raw_json(r#"{"content":{"systemPingRequest":{"data":"AQIDBA=="}}}"#)
        .await
        .unwrap();
    assert!(answer.contains("systemPingResponse"), "got: {answer}");
    assert!(answer.contains("AQIDBA=="));
}
