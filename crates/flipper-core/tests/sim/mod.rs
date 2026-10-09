//! A simulated Flipper that answers real protobuf messages over an in-memory
//! pipe: framing, RPC matching and the device API can be tested with no
//! hardware and no network, like `SimulatedFlipper` in the iOS app.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// Simulated SD card writes in flight, by command id.
type PendingWrites = Arc<Mutex<HashMap<u32, (String, Vec<u8>)>>>;

use tokio::sync::mpsc;

use flipper_core::client::Transport;
use flipper_core::error::{Error, Result};
use flipper_core::frame::{self, Decoder};
use flipper_core::pb::main::Content;
use flipper_core::pb::{self, CommandStatus, Main};

/// One end of the in-memory link, as the client sees it.
pub struct TestTransport {
    rx: mpsc::Receiver<Vec<u8>>,
    tx: mpsc::Sender<Vec<u8>>,
    /// Holds the device-to-client direction open when there is no device.
    _keepalive: Option<mpsc::Sender<Vec<u8>>>,
    /// Holds the client-to-device direction open when there is no device.
    _sink: Option<mpsc::Receiver<Vec<u8>>>,
}

impl Transport for TestTransport {
    async fn recv(&mut self) -> Result<Vec<u8>> {
        self.rx.recv().await.ok_or(Error::NotConnected)
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        self.tx
            .send(data.to_vec())
            .await
            .map_err(|_| Error::NotConnected)
    }

    async fn close(&mut self) {}
}

#[derive(Default)]
pub struct SimulatedFlipper {
    /// Every request the device received, in order.
    pub log: Arc<Mutex<Vec<Main>>>,
    /// Files by absolute path.
    pub files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// Directories by absolute path.
    pub dirs: Arc<Mutex<HashSet<String>>>,
    /// Set when the simulated device drops the link (reboot).
    pub link_dropped: Arc<AtomicBool>,
    /// Whether the screen stream is on (for the already-started error).
    pub screen_stream: Arc<AtomicBool>,
    /// The app currently running, if any.
    pub app_running: Arc<Mutex<Option<String>>>,
    /// The path passed to app_load_file, if any.
    pub loaded_path: Arc<Mutex<Option<String>>>,
}

/// What `setup` hands back: the client transport plus the device handle.
pub fn setup() -> (TestTransport, SimulatedFlipper) {
    let (to_device, device_rx) = mpsc::channel::<Vec<u8>>(64);
    let (to_client, client_rx) = mpsc::channel::<Vec<u8>>(64);
    let transport = TestTransport {
        rx: client_rx,
        tx: to_device,
        _keepalive: None,
        _sink: None,
    };

    let sim = SimulatedFlipper {
        dirs: Arc::new(Mutex::new(HashSet::from([
            "/ext".to_owned(),
            "/int".to_owned(),
        ]))),
        ..Default::default()
    };

    let state = DeviceState {
        log: Arc::clone(&sim.log),
        files: Arc::clone(&sim.files),
        dirs: Arc::clone(&sim.dirs),
        screen_stream: Arc::clone(&sim.screen_stream),
        app_running: Arc::clone(&sim.app_running),
        loaded_path: Arc::clone(&sim.loaded_path),
        pending_writes: Arc::new(Mutex::new(HashMap::new())),
        link_dropped: Arc::clone(&sim.link_dropped),
    };
    let link_dropped = Arc::clone(&sim.link_dropped);
    tokio::spawn(async move {
        let mut decoder = Decoder::new();
        let mut device_rx = device_rx;
        let tx = to_client;
        loop {
            tokio::select! {
                biased;
                _ = dropped(link_dropped.clone()) => break,
                chunk = device_rx.recv() => {
                    let Some(bytes) = chunk else { break };
                    let Ok(messages) = decoder.push(&bytes) else { break };
                    for message in messages {
                        state.log.lock().unwrap().push(message.clone());
                        for reply in state.handle(message) {
                            if tx.send(frame::encode(&reply)).await.is_err() {
                                return;
                            }
                        }
                        if state.link_dropped.load(Ordering::Relaxed) {
                            return; // firmware reboots: answer went out, link dies
                        }
                    }
                }
            }
        }
        drop(tx); // closing the link = client recv fails
    });

    (transport, sim)
}

/// A link with no device on the other end, for timeout tests: writes are
/// accepted and swallowed, nothing ever answers.
pub fn setup_silent() -> TestTransport {
    let (to_device, device_rx) = mpsc::channel::<Vec<u8>>(64);
    let (to_client, client_rx) = mpsc::channel::<Vec<u8>>(64);
    TestTransport {
        rx: client_rx,
        tx: to_device,
        _keepalive: Some(to_client),
        _sink: Some(device_rx),
    }
}

async fn dropped(flag: Arc<AtomicBool>) {
    loop {
        if flag.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

#[derive(Clone)]
struct DeviceState {
    log: Arc<Mutex<Vec<Main>>>,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    dirs: Arc<Mutex<HashSet<String>>>,
    screen_stream: Arc<AtomicBool>,
    app_running: Arc<Mutex<Option<String>>>,
    loaded_path: Arc<Mutex<Option<String>>>,
    /// Chunked writes in flight, by command id: (path, accumulated bytes).
    pending_writes: PendingWrites,
    /// Set when the simulated firmware reboots and drops the link.
    link_dropped: Arc<AtomicBool>,
}

impl DeviceState {
    fn accumulate(&self, id: u32, path: &str, data: Vec<u8>) {
        let mut pending = self.pending_writes.lock().unwrap();
        let entry = pending
            .entry(id)
            .or_insert_with(|| (path.to_owned(), Vec::new()));
        entry.1.extend_from_slice(&data);
    }

    fn finish_write(&self, id: u32, tail: Vec<u8>) -> Vec<u8> {
        let (_path, mut accumulated) = self
            .pending_writes
            .lock()
            .unwrap()
            .remove(&id)
            .unwrap_or_default();
        accumulated.extend_from_slice(&tail);
        accumulated
    }
}

impl SimulatedFlipper {
    pub fn put_file(&self, path: &str, data: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), data.to_vec());
    }

    pub fn put_dir(&self, path: &str) {
        self.dirs.lock().unwrap().insert(path.to_owned());
    }
}

impl DeviceState {
    fn ok(id: u32, content: Content) -> Main {
        Main {
            command_id: id,
            command_status: CommandStatus::Ok as i32,
            has_next: false,
            content: Some(content),
        }
    }

    fn err(id: u32, status: CommandStatus) -> Main {
        Main {
            command_id: id,
            command_status: status as i32,
            has_next: false,
            content: Some(Content::Empty(Default::default())),
        }
    }

    fn info_chain(
        id: u32,
        facts: &[(&str, &str)],
        wrap: fn((String, String)) -> Content,
    ) -> Vec<Main> {
        facts
            .iter()
            .enumerate()
            .map(|(index, (key, value))| Main {
                command_id: id,
                command_status: CommandStatus::Ok as i32,
                has_next: index + 1 < facts.len(),
                content: Some(wrap(((*key).to_owned(), (*value).to_owned()))),
            })
            .collect()
    }

    /// Handles one request, returning zero or more response messages.
    fn handle(&self, request: Main) -> Vec<Main> {
        let id = request.command_id;
        let Some(content) = request.content else {
            return vec![Self::err(id, CommandStatus::ErrorDecode)];
        };
        match content {
            Content::SystemPingRequest(ping) => vec![Self::ok(
                id,
                Content::SystemPingResponse(pb::system::PingResponse { data: ping.data }),
            )],
            Content::SystemDeviceInfoRequest(_) => Self::info_chain(
                id,
                &[
                    ("hardware_name", "Flipper Zero"),
                    ("firmware_name", "Momentum"),
                    ("protocol_version", "1"),
                ],
                |entry| {
                    Content::SystemDeviceInfoResponse(pb::system::DeviceInfoResponse {
                        key: entry.0,
                        value: entry.1,
                    })
                },
            ),
            Content::SystemPowerInfoRequest(_) => Self::info_chain(
                id,
                &[("charge_level", "85"), ("charge_state", "Discharging")],
                |entry| {
                    Content::SystemPowerInfoResponse(pb::system::PowerInfoResponse {
                        key: entry.0,
                        value: entry.1,
                    })
                },
            ),
            Content::StorageListRequest(list) => {
                let dirs = self.dirs.lock().unwrap();
                let files = self.files.lock().unwrap();
                let mut names: Vec<(String, bool, u32)> = Vec::new();
                for dir in dirs.iter() {
                    if parent_of(dir) == list.path {
                        names.push((name_of(dir), true, 0));
                    }
                }
                for (file_path, data) in files.iter() {
                    if parent_of(file_path) == list.path {
                        names.push((name_of(file_path), false, data.len() as u32));
                    }
                }
                names.sort();
                let total = names.len();
                names
                    .into_iter()
                    .enumerate()
                    .map(|(index, (name, is_dir, size))| Main {
                        command_id: id,
                        command_status: CommandStatus::Ok as i32,
                        has_next: index + 1 < total,
                        content: Some(Content::StorageListResponse(pb::storage::ListResponse {
                            file: vec![file_pb(
                                name,
                                if is_dir {
                                    pb::storage::file::FileType::Dir
                                } else {
                                    pb::storage::file::FileType::File
                                },
                                size,
                            )],
                        })),
                    })
                    .collect()
            }
            Content::StorageStatRequest(stat) => {
                if let Some(data) = self.files.lock().unwrap().get(&stat.path) {
                    vec![Self::ok(
                        id,
                        Content::StorageStatResponse(pb::storage::StatResponse {
                            file: Some(file_pb(
                                name_of(&stat.path),
                                pb::storage::file::FileType::File,
                                data.len() as u32,
                            )),
                        }),
                    )]
                } else if self.dirs.lock().unwrap().contains(&stat.path) {
                    vec![Self::ok(
                        id,
                        Content::StorageStatResponse(pb::storage::StatResponse {
                            file: Some(file_pb(
                                name_of(&stat.path),
                                pb::storage::file::FileType::Dir,
                                0,
                            )),
                        }),
                    )]
                } else {
                    vec![Self::err(id, CommandStatus::ErrorStorageNotExist)]
                }
            }
            Content::StorageReadRequest(read) => {
                let files = self.files.lock().unwrap();
                let Some(data) = files.get(&read.path) else {
                    return vec![Self::err(id, CommandStatus::ErrorStorageNotExist)];
                };
                // The firmware streams the file in 512-byte frames.
                let total = data.len().div_ceil(512).max(1);
                data.chunks(512)
                    .chain(std::iter::once(&[][..]).filter(|_| data.is_empty()))
                    .enumerate()
                    .map(|(index, chunk)| Main {
                        command_id: id,
                        command_status: CommandStatus::Ok as i32,
                        has_next: index + 1 < total,
                        content: Some(Content::StorageReadResponse(pb::storage::ReadResponse {
                            file: Some(pb::storage::File {
                                r#type: pb::storage::file::FileType::File as i32,
                                name: name_of(&read.path),
                                size: chunk.len() as u32,
                                data: chunk.to_vec(),
                                ..Default::default()
                            }),
                        })),
                    })
                    .collect()
            }
            Content::StorageWriteRequest(write) => {
                let data = write.file.map(|f| f.data).unwrap_or_default();
                if request.has_next {
                    self.accumulate(request.command_id, &write.path, data);
                    return vec![];
                }
                let path = write.path;
                let accumulated = self.finish_write(request.command_id, data);
                // FAT semantics: the parent directory must exist.
                if !self.dirs.lock().unwrap().contains(&parent_of(&path)) {
                    return vec![Self::err(id, CommandStatus::ErrorStorageNotExist)];
                }
                self.files.lock().unwrap().insert(path, accumulated);
                vec![Self::ok(id, Content::Empty(Default::default()))]
            }
            Content::StorageMkdirRequest(mkdir) => {
                let mut dirs = self.dirs.lock().unwrap();
                if dirs.contains(&mkdir.path) {
                    vec![Self::err(id, CommandStatus::ErrorStorageExist)]
                    // FAT semantics: a missing parent is an error, not a create.
                } else if !dirs.contains(&parent_of(&mkdir.path)) {
                    vec![Self::err(id, CommandStatus::ErrorStorageNotExist)]
                } else {
                    dirs.insert(mkdir.path);
                    vec![Self::ok(id, Content::Empty(Default::default()))]
                }
            }
            Content::StorageDeleteRequest(delete) => {
                let mut dirs = self.dirs.lock().unwrap();
                let mut files = self.files.lock().unwrap();
                let prefix = format!("{}/", delete.path);
                if files.remove(&delete.path).is_some() {
                    return vec![Self::ok(id, Content::Empty(Default::default()))];
                }
                if dirs.contains(&delete.path) {
                    let has_children = dirs.iter().any(|d| d.starts_with(&prefix))
                        || files.keys().any(|f| f.starts_with(&prefix));
                    if has_children && !delete.recursive {
                        return vec![Self::err(id, CommandStatus::ErrorStorageDirNotEmpty)];
                    }
                    dirs.retain(|d| *d != delete.path && !d.starts_with(&prefix));
                    files.retain(|f, _| !f.starts_with(&prefix));
                    return vec![Self::ok(id, Content::Empty(Default::default()))];
                }
                vec![Self::err(id, CommandStatus::ErrorStorageNotExist)]
            }
            Content::StorageRenameRequest(rename) => {
                let mut files = self.files.lock().unwrap();
                if let Some(data) = files.remove(&rename.old_path) {
                    files.insert(rename.new_path, data);
                    return vec![Self::ok(id, Content::Empty(Default::default()))];
                }
                let mut dirs = self.dirs.lock().unwrap();
                if dirs.remove(&rename.old_path) {
                    dirs.insert(rename.new_path);
                    return vec![Self::ok(id, Content::Empty(Default::default()))];
                }
                vec![Self::err(id, CommandStatus::ErrorStorageNotExist)]
            }
            Content::StorageInfoRequest(_) => vec![Self::ok(
                id,
                Content::StorageInfoResponse(pb::storage::InfoResponse {
                    total_space: 1 << 20,
                    free_space: 1 << 18,
                }),
            )],
            Content::AppStartRequest(start) => {
                *self.app_running.lock().unwrap() = Some(start.name.clone());
                vec![Self::ok(id, Content::Empty(Default::default()))]
            }
            Content::AppExitRequest(_) => {
                *self.app_running.lock().unwrap() = None;
                *self.loaded_path.lock().unwrap() = None;
                vec![Self::ok(id, Content::Empty(Default::default()))]
            }
            Content::AppLoadFileRequest(load) => {
                let running = self.app_running.lock().unwrap().is_some();
                if !running {
                    vec![Self::err(id, CommandStatus::ErrorAppNotRunning)]
                } else if !self.files.lock().unwrap().contains_key(&load.path) {
                    vec![Self::err(id, CommandStatus::ErrorStorageNotExist)]
                } else {
                    *self.loaded_path.lock().unwrap() = Some(load.path);
                    vec![Self::ok(id, Content::Empty(Default::default()))]
                }
            }
            Content::AppButtonPressRequest(_) | Content::AppButtonReleaseRequest(_) => {
                vec![Self::ok(id, Content::Empty(Default::default()))]
            }
            Content::GuiStartScreenStreamRequest(_) => {
                let was_on = self.screen_stream.swap(true, Ordering::Relaxed);
                if was_on {
                    return vec![Self::err(
                        id,
                        CommandStatus::ErrorVirtualDisplayAlreadyStarted,
                    )];
                }
                // The firmware redraws on stream start: push a frame unsolicited.
                let frame = Self::ok(id, Content::Empty(Default::default()));
                let unsolicited = screen_frame_message();
                vec![frame, unsolicited]
            }
            Content::GuiStopScreenStreamRequest(_) => {
                self.screen_stream.store(false, Ordering::Relaxed);
                vec![Self::ok(id, Content::Empty(Default::default()))]
            }
            Content::GuiSendInputEventRequest(_) => {
                vec![Self::ok(id, Content::Empty(Default::default()))]
            }
            Content::SystemRebootRequest(_) => {
                self.link_dropped.store(true, Ordering::Relaxed);
                vec![Self::ok(id, Content::Empty(Default::default()))]
            }
            Content::SystemUpdateRequest(update) => {
                let exists = self
                    .files
                    .lock()
                    .unwrap()
                    .contains_key(&update.update_manifest);
                if exists {
                    vec![Self::ok(
                        id,
                        Content::SystemUpdateResponse(pb::system::UpdateResponse {
                            code: pb::system::update_response::UpdateResultCode::Ok as i32,
                        }),
                    )]
                } else {
                    vec![Self::err(id, CommandStatus::ErrorInvalidParameters)]
                }
            }
            Content::SystemPlayAudiovisualAlertRequest(_) => {
                vec![Self::ok(id, Content::Empty(Default::default()))]
            }
            Content::GpioSetPinMode(_)
            | Content::GpioSetInputPull(_)
            | Content::GpioWritePin(_) => {
                vec![Self::ok(id, Content::Empty(Default::default()))]
            }
            Content::GpioReadPin(_) => vec![Self::ok(
                id,
                Content::GpioReadPinResponse(pb::gpio::ReadPinResponse { value: 1 }),
            )],
            _ => vec![Self::err(id, CommandStatus::ErrorNotImplemented)],
        }
    }
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(index) => path[..index].to_owned(),
        None => "/".to_owned(),
    }
}

fn name_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

fn file_pb(name: String, r#type: pb::storage::file::FileType, size: u32) -> pb::storage::File {
    pb::storage::File {
        name,
        r#type: r#type as i32,
        size,
        data: Vec::new(),
        md5sum: String::new(),
    }
}

/// An unsolicited screen frame, as the firmware pushes on stream start.
pub fn screen_frame_message() -> Main {
    Main {
        command_id: 0,
        command_status: CommandStatus::Ok as i32,
        has_next: false,
        content: Some(Content::GuiScreenFrame(pb::gui::ScreenFrame {
            data: vec![0u8; 1024],
            orientation: pb::gui::ScreenOrientation::Horizontal as i32,
        })),
    }
}
