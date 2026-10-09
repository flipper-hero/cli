//! The device API on top of [`Client`]: storage, device and power info, app
//! control, screen, buttons, GPIO, update staging. One-to-one with what the
//! iOS FlipperKit exposes over the same RPC surface.

use std::time::Duration;

use crate::client::Client;
use crate::error::{Error, Result};
use crate::path::FlipperPath;
use crate::pb::main::Content;
use crate::pb::{self, Main};
use crate::screen::FlipperScreenFrame;

/// Transfer and limit constants, mirroring `FlipperLimits`.
pub mod limits {
    use std::time::Duration;

    /// Bytes per storage write frame.
    pub const WRITE_CHUNK_SIZE: usize = 512;
    /// Refuses reads larger than this unless the caller asks for more.
    pub const DEFAULT_MAX_READ_BYTES: usize = 64 * 1024;
    /// The RPC client's default answer timeout.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

    pub const READ_TIMEOUT: Duration = Duration::from_secs(60);
    pub const APP_TIMEOUT: Duration = Duration::from_secs(30);

    /// The upload timeout grows with the size so large transfers over
    /// Bluetooth are not cut off (same formula as the iOS app).
    pub fn write_timeout(bytes: usize) -> Duration {
        Duration::from_secs(60 + (bytes / 2048) as u64)
    }
}

/// Apps on the Flipper that can be driven over RPC, with the loader name the
/// firmware expects and the file extension each one opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum App {
    SubGhz,
    Infrared,
    Rfid,
    Nfc,
    IButton,
    BadKeyboard,
}

impl App {
    pub fn loader_name(self) -> &'static str {
        match self {
            Self::SubGhz => "Sub-GHz",
            Self::Infrared => "Infrared",
            Self::Rfid => "125 kHz RFID",
            Self::Nfc => "NFC",
            Self::IButton => "iButton",
            Self::BadKeyboard => "Bad KB",
        }
    }

    pub fn file_extension(self) -> &'static str {
        match self {
            Self::SubGhz => "sub",
            Self::Infrared => "ir",
            Self::Rfid => "rfid",
            Self::Nfc => "nfc",
            Self::IButton => "ibtn",
            Self::BadKeyboard => "txt",
        }
    }

    /// App for a file extension, if one matches.
    pub fn for_extension(ext: &str) -> Option<Self> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        [
            Self::SubGhz,
            Self::Infrared,
            Self::Rfid,
            Self::Nfc,
            Self::IButton,
            Self::BadKeyboard,
        ]
        .into_iter()
        .find(|app| app.file_extension() == ext)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_directory: bool,
    pub size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageInfo {
    pub total_space: u64,
    pub free_space: u64,
}

/// GPIO pins exposed on the Flipper's header, as the firmware's RPC names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpioPin {
    Pc0,
    Pc1,
    Pc3,
    Pb2,
    Pb3,
    Pa4,
    Pa6,
    Pa7,
}

impl GpioPin {
    pub fn proto(self) -> pb::gpio::GpioPin {
        match self {
            Self::Pc0 => pb::gpio::GpioPin::Pc0,
            Self::Pc1 => pb::gpio::GpioPin::Pc1,
            Self::Pc3 => pb::gpio::GpioPin::Pc3,
            Self::Pb2 => pb::gpio::GpioPin::Pb2,
            Self::Pb3 => pb::gpio::GpioPin::Pb3,
            Self::Pa4 => pb::gpio::GpioPin::Pa4,
            Self::Pa6 => pb::gpio::GpioPin::Pa6,
            Self::Pa7 => pb::gpio::GpioPin::Pa7,
        }
    }

    pub fn parse_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().replace('-', "").as_str() {
            "pc0" => Self::Pc0,
            "pc1" => Self::Pc1,
            "pc3" => Self::Pc3,
            "pb2" => Self::Pb2,
            "pb3" => Self::Pb3,
            "pa4" => Self::Pa4,
            "pa6" => Self::Pa6,
            "pa7" => Self::Pa7,
            _ => return None,
        })
    }
}

/// Status of a staged firmware update request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateResult {
    Ok,
    Rejected(String),
}

fn unexpected<T>() -> Result<T> {
    Err(Error::UnexpectedResponse)
}

fn first(parts: &[Main]) -> Result<&Main> {
    parts.first().ok_or(Error::UnexpectedResponse)
}

impl<T: crate::client::Transport> Client<T> {
    // MARK: System

    /// Round-trips a ping and verifies the payload came back intact.
    pub async fn ping(&self) -> Result<()> {
        let request = pb::system::PingRequest {
            data: vec![1, 2, 3, 4],
        };
        let parts = self.call(vec![Content::SystemPingRequest(request)]).await?;
        match &first(&parts)?.content {
            Some(Content::SystemPingResponse(response)) if response.data == [1, 2, 3, 4] => Ok(()),
            _ => unexpected(),
        }
    }

    /// Hardware, firmware and protocol facts as key/value pairs.
    pub async fn device_info(&self) -> Result<Vec<(String, String)>> {
        let parts = self
            .call(vec![Content::SystemDeviceInfoRequest(
                pb::system::DeviceInfoRequest {},
            )])
            .await?;
        let mut info = Vec::new();
        for part in &parts {
            if let Some(Content::SystemDeviceInfoResponse(entry)) = &part.content {
                info.push((entry.key.clone(), entry.value.clone()));
            }
        }
        Ok(info)
    }

    /// Battery level and charging state. Over RPC the keys use underscores:
    /// `charge_level`, `charge_state`, not dotted names.
    pub async fn power_info(&self) -> Result<Vec<(String, String)>> {
        let parts = self
            .call(vec![Content::SystemPowerInfoRequest(
                pb::system::PowerInfoRequest {},
            )])
            .await?;
        let mut info = Vec::new();
        for part in &parts {
            if let Some(Content::SystemPowerInfoResponse(entry)) = &part.content {
                info.push((entry.key.clone(), entry.value.clone()));
            }
        }
        Ok(info)
    }

    /// Beeps, blinks and vibrates so the device can be located.
    pub async fn play_alert(&self) -> Result<()> {
        self.call(vec![Content::SystemPlayAudiovisualAlertRequest(
            pb::system::PlayAudiovisualAlertRequest {},
        )])
        .await
        .map(|_| ())
    }

    /// Restarts into normal firmware. The device drops the link while
    /// answering, so a lost connection or timeout here means the request was
    /// received, which is not an error.
    pub async fn reboot(&self) -> Result<()> {
        self.reboot_into(pb::system::reboot_request::RebootMode::Os)
            .await
    }

    /// Restarts into the given mode; `Update` starts the on-device updater.
    pub async fn reboot_into(&self, mode: pb::system::reboot_request::RebootMode) -> Result<()> {
        let request = pb::system::RebootRequest { mode: mode as i32 };
        match self
            .call_with(
                vec![Content::SystemRebootRequest(request)],
                Some(Duration::from_secs(5)),
                None,
            )
            .await
        {
            Ok(_) | Err(Error::Timeout) | Err(Error::NotConnected) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Asks the firmware to validate and stage an update package already on
    /// the SD card. Follow with [`reboot_into`](Self::reboot_into) into
    /// `Update` to install.
    pub async fn request_update(&self, manifest_path: &str) -> Result<UpdateResult> {
        let request = pb::system::UpdateRequest {
            update_manifest: FlipperPath::normalize(manifest_path)?,
        };
        let parts = self
            .call_with(
                vec![Content::SystemUpdateRequest(request)],
                Some(Duration::from_secs(60)),
                None,
            )
            .await?;
        match &first(&parts)?.content {
            Some(Content::SystemUpdateResponse(response)) => {
                if response.code() == pb::system::update_response::UpdateResultCode::Ok {
                    Ok(UpdateResult::Ok)
                } else {
                    Ok(UpdateResult::Rejected(
                        response.code().as_str_name().to_owned(),
                    ))
                }
            }
            _ => unexpected(),
        }
    }

    // MARK: Storage

    /// Total and free bytes of the storage at `path` (default `/ext`).
    pub async fn storage_info(&self, path: &str) -> Result<StorageInfo> {
        let request = pb::storage::InfoRequest {
            path: FlipperPath::normalize(path)?,
        };
        let parts = self
            .call(vec![Content::StorageInfoRequest(request)])
            .await?;
        match &first(&parts)?.content {
            Some(Content::StorageInfoResponse(response)) => Ok(StorageInfo {
                total_space: response.total_space,
                free_space: response.free_space,
            }),
            _ => unexpected(),
        }
    }

    /// Lists a directory, directories first then names, case-insensitively.
    pub async fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        let request = pb::storage::ListRequest {
            path: FlipperPath::normalize(path)?,
            ..Default::default()
        };
        let parts = self
            .call(vec![Content::StorageListRequest(request)])
            .await?;
        let mut entries: Vec<DirEntry> = Vec::new();
        for part in &parts {
            if let Some(Content::StorageListResponse(response)) = &part.content {
                for file in &response.file {
                    entries.push(DirEntry {
                        name: file.name.clone(),
                        is_directory: file.r#type() == pb::storage::file::FileType::Dir,
                        size: file.size,
                    });
                }
            }
        }
        entries.sort_by(|a, b| {
            b.is_directory
                .cmp(&a.is_directory)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    /// Stats one path.
    pub async fn stat(&self, path: &str) -> Result<DirEntry> {
        let normalized = FlipperPath::normalize(path)?;
        let request = pb::storage::StatRequest {
            path: normalized.clone(),
        };
        let parts = self
            .call(vec![Content::StorageStatRequest(request)])
            .await?;
        let response = match &first(&parts)?.content {
            Some(Content::StorageStatResponse(response)) => response,
            _ => return unexpected(),
        };
        let Some(file) = response.file.as_ref() else {
            return unexpected();
        };
        Ok(DirEntry {
            name: FlipperPath::last_component(&normalized).to_owned(),
            is_directory: file.r#type() == pb::storage::file::FileType::Dir,
            size: file.size,
        })
    }

    /// Reads a file, refusing anything larger than `max_bytes` (checked via
    /// stat before any data moves).
    pub async fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.read_with_limit(path, limits::DEFAULT_MAX_READ_BYTES)
            .await
    }

    pub async fn read_with_limit(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let normalized = FlipperPath::normalize(path)?;
        let info = self.stat(&normalized).await?;
        if info.is_directory {
            return Err(Error::InvalidPath("is a directory".into()));
        }
        if info.size as usize > max_bytes {
            return Err(Error::Rpc(format!(
                "file is {} bytes, limit is {max_bytes}",
                info.size
            )));
        }
        let request = pb::storage::ReadRequest { path: normalized };
        let parts = self
            .call_with(
                vec![Content::StorageReadRequest(request)],
                Some(limits::READ_TIMEOUT),
                None,
            )
            .await?;
        let mut data = Vec::with_capacity(info.size as usize);
        for part in &parts {
            if let Some(Content::StorageReadResponse(response)) = &part.content {
                if let Some(file) = response.file.as_ref() {
                    data.extend_from_slice(&file.data);
                }
            }
        }
        Ok(data)
    }

    /// Writes a file in chunks. `progress` reports (bytes sent, total).
    pub async fn write_with_progress(
        &self,
        path: &str,
        data: &[u8],
        progress: Option<crate::client::Progress<'_>>,
    ) -> Result<()> {
        let normalized = FlipperPath::normalize(path)?;
        let mut contents = Vec::new();
        if data.is_empty() {
            contents.push(Content::StorageWriteRequest(pb::storage::WriteRequest {
                path: normalized.clone(),
                file: Some(pb::storage::File::default()),
            }));
        } else {
            for chunk in data.chunks(limits::WRITE_CHUNK_SIZE) {
                contents.push(Content::StorageWriteRequest(pb::storage::WriteRequest {
                    path: normalized.clone(),
                    file: Some(pb::storage::File {
                        data: chunk.to_vec(),
                        ..Default::default()
                    }),
                }));
            }
        }
        let total = data.len();
        let progress_fn = progress.map(|report| {
            move |sent: usize, _frames: usize| {
                report((sent * limits::WRITE_CHUNK_SIZE).min(total), total)
            }
        });
        match progress_fn {
            Some(report) => {
                self.call_with(contents, Some(limits::write_timeout(total)), Some(&report))
                    .await
            }
            None => {
                self.call_with(contents, Some(limits::write_timeout(total)), None)
                    .await
            }
        }
        .map(|_| ())
    }

    /// Writes a file without progress reporting.
    pub async fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        self.write_with_progress(path, data, None).await
    }

    /// Creates one directory. An existing directory is fine.
    pub async fn make_directory(&self, path: &str) -> Result<()> {
        let request = pb::storage::MkdirRequest {
            path: FlipperPath::normalize(path)?,
        };
        match self.call(vec![Content::StorageMkdirRequest(request)]).await {
            Ok(_) => Ok(()),
            Err(Error::Rpc(status)) if status.to_lowercase().contains("exist") => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Creates `path` and any missing parents. Existing folders are fine.
    /// `/ext` itself always exists and is skipped.
    pub async fn make_directories(&self, path: &str) -> Result<()> {
        let normalized = FlipperPath::normalize(path)?;
        let mut current = String::new();
        for component in normalized.split('/').filter(|c| !c.is_empty()) {
            current = format!("{current}/{component}");
            // Skip exactly the first component: /ext itself always exists.
            if current.split('/').filter(|c| !c.is_empty()).count() > 1 {
                self.make_directory(&current).await?;
            }
        }
        Ok(())
    }

    /// Deletes a file or (with `recursive`) a folder.
    pub async fn delete(&self, path: &str, recursive: bool) -> Result<()> {
        let request = pb::storage::DeleteRequest {
            path: FlipperPath::normalize(path)?,
            recursive,
        };
        self.call(vec![Content::StorageDeleteRequest(request)])
            .await
            .map(|_| ())
    }

    /// Renames or moves within the Flipper's storage.
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let request = pb::storage::RenameRequest {
            old_path: FlipperPath::normalize(from)?,
            new_path: FlipperPath::normalize(to)?,
        };
        self.call(vec![Content::StorageRenameRequest(request)])
            .await
            .map(|_| ())
    }

    // MARK: App control

    /// Opens `app` in RPC mode and loads `path`. The caller must call
    /// [`exit_app`](Self::exit_app) afterwards.
    async fn open_in_rpc_mode(&self, app: App, path: &str) -> Result<()> {
        let normalized = FlipperPath::normalize(path)?;
        if !normalized
            .rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(app.file_extension()))
        {
            return Err(Error::InvalidPath(format!(
                "{} expects a .{} file",
                app.loader_name(),
                app.file_extension()
            )));
        }
        self.stat(&normalized).await?; // fails early if the file is missing

        let start = pb::app::StartRequest {
            name: app.loader_name().to_owned(),
            args: "RPC".to_owned(),
        };
        self.call_with(
            vec![Content::AppStartRequest(start)],
            Some(limits::APP_TIMEOUT),
            None,
        )
        .await
        .map_err(|error| match &error {
            // LoaderStatusErrorUnknownApp: the loader does not know this
            // app's name. Compiled-in apps are always there; external
            // (FAP) builds of the main apps vary by firmware.
            Error::Rpc(status) if status.contains("INVALID_PARAMETERS") => Error::Rpc(format!(
                "this firmware does not expose the app '{}' to RPC \
                 (the loader rejected the name; {})",
                app.loader_name(),
                "its main apps may be external on this build"
            )),
            _ => error,
        })?;

        let load = pb::app::AppLoadFileRequest { path: normalized };
        let load_call = || {
            self.call_with(
                vec![Content::AppLoadFileRequest(load.clone())],
                Some(limits::APP_TIMEOUT),
                None,
            )
        };
        // The app must register its RPC callback before it accepts loads;
        // right after start it may not be there yet, so give it a moment.
        let mut load_error = None;
        for attempt in 0..5 {
            match load_call().await {
                Ok(_) => {
                    load_error = None;
                    break;
                }
                Err(Error::Rpc(status)) if status.contains("APP_NOT_RUNNING") => {
                    load_error = Some(Error::Rpc(status));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(error) => {
                    load_error = Some(error);
                    break;
                }
            }
            let _ = attempt;
        }
        if let Some(error) = load_error {
            let _ = self.exit_app().await;
            return Err(error);
        }
        Ok(())
    }

    /// Closes the app opened over RPC. An app started with `RPC` binds to
    /// the session that started it, so this only reaches apps this same
    /// connection launched (which [`transmit`](Self::transmit) and
    /// [`emulate`](Self::emulate) always do).
    pub async fn exit_app(&self) -> Result<()> {
        self.call_with(
            vec![Content::AppExitRequest(pb::app::AppExitRequest {})],
            Some(Duration::from_secs(15)),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Starts an arbitrary app by loader name, with optional arguments.
    pub async fn start_app(&self, name: &str, args: &str) -> Result<()> {
        let request = pb::app::StartRequest {
            name: name.to_owned(),
            args: args.to_owned(),
        };
        self.call(vec![Content::AppStartRequest(request)])
            .await
            .map(|_| ())
    }

    /// Loads a file and emulates it until the app is exited: NFC, RFID and
    /// iButton start emulating on load.
    pub async fn emulate(&self, app: App, path: &str) -> Result<()> {
        self.open_in_rpc_mode(app, path).await
    }

    /// Loads a signal and transmits it once (press, hold, release), then
    /// closes the app. `button` selects a named button for Infrared remotes;
    /// Sub-GHz ignores it.
    pub async fn transmit_once(&self, app: App, path: &str, button: &str) -> Result<()> {
        self.transmit(app, path, button, Duration::from_millis(400))
            .await
    }

    /// [`transmit_once`](Self::transmit_once) with an explicit hold time.
    pub async fn transmit(&self, app: App, path: &str, button: &str, hold: Duration) -> Result<()> {
        self.open_in_rpc_mode(app, path).await?;
        let result = async {
            let press = pb::app::AppButtonPressRequest {
                args: button.to_owned(),
                index: 0,
            };
            self.call_with(
                vec![Content::AppButtonPressRequest(press)],
                Some(Duration::from_secs(20)),
                None,
            )
            .await?;
            tokio::time::sleep(hold).await;
            self.call(vec![Content::AppButtonReleaseRequest(
                pb::app::AppButtonReleaseRequest {},
            )])
            .await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = self.exit_app().await;
            return result;
        }
        self.exit_app().await
    }

    /// Opens Bad KB with a script loaded. The script is NOT started: the
    /// firmware waits for a press on the Flipper itself, which we deliberately
    /// do not automate.
    pub async fn load_bad_keyboard_script(&self, path: &str) -> Result<()> {
        let normalized = FlipperPath::normalize(path)?;
        self.stat(&normalized).await?;
        let start = pb::app::StartRequest {
            name: App::BadKeyboard.loader_name().to_owned(),
            args: normalized,
        };
        self.call_with(
            vec![Content::AppStartRequest(start)],
            Some(limits::APP_TIMEOUT),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Last error reported by the running app, if any.
    pub async fn app_error(&self) -> Result<Option<String>> {
        let parts = self
            .call(vec![Content::AppGetErrorRequest(
                pb::app::GetErrorRequest {},
            )])
            .await?;
        match &first(&parts)?.content {
            Some(Content::AppGetErrorResponse(response)) if response.code != 0 => {
                Ok(Some(if response.text.is_empty() {
                    format!("error code {}", response.code)
                } else {
                    response.text.clone()
                }))
            }
            _ => Ok(None),
        }
    }

    // MARK: Screen and buttons

    /// Live screen frames. The caller must hold the screen stream via
    /// [`acquire_screen_stream`](Self::acquire_screen_stream) and listen on
    /// [`unsolicited`](Client::unsolicited).
    pub async fn acquire_screen_stream(&self) -> Result<()> {
        let mut users = self.screen_users.lock().await;
        *users += 1;
        if *users > 1 {
            return Ok(());
        }
        match self
            .call(vec![Content::GuiStartScreenStreamRequest(
                pb::gui::StartScreenStreamRequest {},
            )])
            .await
        {
            Ok(_) => Ok(()),
            // Left running by an earlier session; that is fine.
            Err(Error::Rpc(status))
                if status
                    .to_lowercase()
                    .contains("virtual_display_already_started") =>
            {
                Ok(())
            }
            Err(error) => {
                *users -= 1;
                Err(error)
            }
        }
    }

    /// Stops the screen stream when the last user is done.
    pub async fn release_screen_stream(&self) {
        let mut users = self.screen_users.lock().await;
        if *users == 0 {
            return;
        }
        *users -= 1;
        if *users == 0 {
            let _ = self
                .call(vec![Content::GuiStopScreenStreamRequest(
                    pb::gui::StopScreenStreamRequest {},
                )])
                .await;
        }
    }

    /// One current frame. Starting the stream makes the firmware redraw, so a
    /// frame arrives even on a static screen.
    pub async fn capture_screen(&self, timeout: Duration) -> Result<FlipperScreenFrame> {
        let mut unsolicited = self.unsolicited();
        self.acquire_screen_stream().await?;
        let result = tokio::time::timeout(timeout, async {
            loop {
                match unsolicited.recv().await {
                    Ok(message) => {
                        if let Some(frame) = crate::screen::frame_from_message(&message) {
                            return Ok(frame);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(Error::NotConnected)
                    }
                }
            }
        })
        .await;
        self.release_screen_stream().await;
        match result {
            Ok(inner) => inner,
            Err(_elapsed) => Err(Error::Timeout),
        }
    }

    /// Presses a button the way a finger does: press, short or long, release.
    /// The three events go out together in one write.
    pub async fn press(&self, key: crate::screen::FlipperKey, long: bool) -> Result<()> {
        let event = |input_type: pb::gui::InputType| {
            Content::GuiSendInputEventRequest(pb::gui::SendInputEventRequest {
                key: key.proto() as i32,
                r#type: input_type as i32,
            })
        };
        self.call_pipelined(vec![
            event(pb::gui::InputType::Press),
            event(if long {
                pb::gui::InputType::Long
            } else {
                pb::gui::InputType::Short
            }),
            event(pb::gui::InputType::Release),
        ])
        .await
    }

    // MARK: GPIO

    /// Puts a pin in input or output mode; input pins can select a pull
    /// resistor (`Some(true)` up, `Some(false)` down, `None` floating).
    pub async fn gpio_set_mode(
        &self,
        pin: GpioPin,
        output: bool,
        pull_up: Option<bool>,
    ) -> Result<()> {
        self.call(vec![Content::GpioSetPinMode(pb::gpio::SetPinMode {
            pin: pin.proto() as i32,
            mode: if output {
                pb::gpio::GpioPinMode::Output as i32
            } else {
                pb::gpio::GpioPinMode::Input as i32
            },
        })])
        .await?;
        if !output {
            let pull_mode = match pull_up {
                Some(true) => pb::gpio::GpioInputPull::Up,
                Some(false) => pb::gpio::GpioInputPull::Down,
                None => pb::gpio::GpioInputPull::No,
            };
            self.call(vec![Content::GpioSetInputPull(pb::gpio::SetInputPull {
                pin: pin.proto() as i32,
                pull_mode: pull_mode as i32,
            })])
            .await?;
        }
        Ok(())
    }

    /// Reads a pin set to input mode: true means the level is high.
    pub async fn gpio_read(&self, pin: GpioPin) -> Result<bool> {
        let parts = self
            .call(vec![Content::GpioReadPin(pb::gpio::ReadPin {
                pin: pin.proto() as i32,
            })])
            .await?;
        match &first(&parts)?.content {
            Some(Content::GpioReadPinResponse(response)) => Ok(response.value != 0),
            _ => unexpected(),
        }
    }

    /// Drives a pin set to output mode: true means the level goes high.
    pub async fn gpio_write(&self, pin: GpioPin, level: bool) -> Result<()> {
        self.call(vec![Content::GpioWritePin(pb::gpio::WritePin {
            pin: pin.proto() as i32,
            value: if level { 1 } else { 0 },
        })])
        .await
        .map(|_| ())
    }
}
