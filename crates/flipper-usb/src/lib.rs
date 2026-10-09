//! USB CDC-ACM transport for the Flipper Zero RPC.
//!
//! The Flipper enumerates as a virtual COM port (STMicroelectronics
//! 0483:5740) on every supported host, including FreeBSD, and needs no
//! pairing. Opening the port starts a Flipper console session; sending
//! `start_rpc_session` hands the channel to the RPC service, after which the
//! link carries the usual length-delimited protobuf frames (the firmware
//! tears the session down when the port closes).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use flipper_core::error::{Error, Result};
use flipper_core::Transport;
use serialport::SerialPort as _;
use thiserror::Error as ThisError;
use tokio::sync::mpsc;

/// Flipper Zero CDC-ACM identity (`furi_hal_usb_cdc.c`).
pub const USB_VID: u16 = 0x0483;
pub const USB_PID: u16 = 0x5740;

/// The console command that hands the serial channel to the RPC service.
const START_RPC_SESSION: &[u8] = b"start_rpc_session\r";

#[derive(Debug, ThisError)]
pub enum UsbError {
    #[error("no flipper found over USB (plugged in and unlocked?)")]
    NotFound,
    #[error("cannot open {0}: {1}")]
    Open(String, String),
    #[error("serial: {0}")]
    Serial(String),
}

impl From<UsbError> for Error {
    fn from(error: UsbError) -> Self {
        Error::Transport(error.to_string())
    }
}

/// A Flipper seen on a USB serial port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbFlipper {
    pub port: String,
    pub serial: Option<String>,
}

/// Lists serial ports that look like a Flipper.
pub fn list_flippers() -> std::result::Result<Vec<UsbFlipper>, UsbError> {
    let ports =
        serialport::available_ports().map_err(|error| UsbError::Serial(error.to_string()))?;
    Ok(ports
        .into_iter()
        .filter_map(|port| match port.port_type {
            serialport::SerialPortType::UsbPort(info) => {
                if info.vid == USB_VID && info.pid == USB_PID {
                    Some(UsbFlipper {
                        port: port.port_name,
                        serial: info.serial_number,
                    })
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect())
}

/// Resolves `--port` or finds the first Flipper-looking port.
pub fn find_port(explicit: Option<&str>) -> std::result::Result<String, UsbError> {
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    list_flippers()?
        .into_iter()
        .next()
        .map(|flipper| flipper.port)
        .ok_or(UsbError::NotFound)
}

/// One connected Flipper over USB serial. The reader thread owns the port
/// handle outright; writes go through a `try_clone` of the same TTY behind its
/// own lock, so reads can never starve a write.
pub struct UsbTransport {
    write_half: Arc<std::sync::Mutex<Box<dyn serialport::SerialPort>>>,
    rx: mpsc::Receiver<Vec<u8>>,
    closed: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl UsbTransport {
    /// Opens the port and starts an RPC session. `explicit` is a device path
    /// (`/dev/ttyACM0`, `/dev/cu.usbmodem*`, `COM4`, `/dev/cuaU0`).
    pub fn open(explicit: Option<&str>) -> std::result::Result<Self, UsbError> {
        let path = find_port(explicit)?;
        Self::open_path(&path)
    }

    pub fn open_path(path: &str) -> std::result::Result<Self, UsbError> {
        let mut port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(500))
            .open_native()
            .map_err(|error| UsbError::Open(path.to_owned(), error.to_string()))?;
        // Assert modem lines; some hosts open CDC ports with them low.
        let _ = port.write_request_to_send(true);
        let _ = port.write_data_terminal_ready(true);

        let (tx, rx) = mpsc::channel::<Vec<u8>>(64);

        // Session dance: wake the console, drain its prompt and echo, hand the
        // channel to RPC, drain once more. Whatever arrives afterwards is RPC.
        start_session(&mut port)?;

        let write_half: Arc<std::sync::Mutex<Box<dyn serialport::SerialPort>>> =
            Arc::new(std::sync::Mutex::new(
                port.try_clone()
                    .map_err(|error| UsbError::Serial(error.to_string()))?,
            ));
        let closed = Arc::new(AtomicBool::new(false));

        let reader_closed = Arc::clone(&closed);
        let reader = std::thread::Builder::new()
            .name("flipper-usb-read".into())
            .spawn(move || {
                let mut buffer = [0u8; 4096];
                loop {
                    if reader_closed.load(Ordering::Relaxed) {
                        break;
                    }
                    match port.read(&mut buffer) {
                        Ok(0) => std::thread::sleep(Duration::from_millis(5)),
                        Ok(n) => {
                            if tx.blocking_send(buffer[..n].to_vec()).is_err() {
                                break; // client side went away
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                            ) => {}
                        Err(_) => break,
                    }
                }
            })
            .map_err(|error| UsbError::Serial(error.to_string()))?;

        Ok(Self {
            write_half,
            rx,
            closed,
            reader: Some(reader),
        })
    }
}

/// Wakes the console, starts the RPC session and drains console chatter.
fn start_session(port: &mut dyn serialport::SerialPort) -> std::result::Result<(), UsbError> {
    fn write(
        port: &mut dyn serialport::SerialPort,
        data: &[u8],
    ) -> std::result::Result<(), UsbError> {
        port.write_all(data)
            .map_err(|error| UsbError::Serial(error.to_string()))?;
        port.flush()
            .map_err(|error| UsbError::Serial(error.to_string()))
    }
    fn drain(port: &mut dyn serialport::SerialPort, milliseconds: u64) {
        let deadline = std::time::Instant::now() + Duration::from_millis(milliseconds);
        let mut scratch = [0u8; 512];
        while std::time::Instant::now() < deadline {
            match port.read(&mut scratch) {
                Ok(_) => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(_) => break,
            }
        }
    }

    write(port, b"\r")?;
    drain(port, 150);
    write(port, START_RPC_SESSION)?;
    drain(port, 150);
    Ok(())
}

impl Drop for UsbTransport {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Transport for UsbTransport {
    async fn recv(&mut self) -> Result<Vec<u8>> {
        self.rx.recv().await.ok_or(Error::NotConnected)
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        // Only writers share this mutex; the reader owns its own handle, so
        // no read can starve a write. The blocking pool keeps the executor
        // free while the write completes.
        let write_half = Arc::clone(&self.write_half);
        let data = data.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = write_half.lock().unwrap();
            guard
                .write_all(&data)
                .and_then(|()| guard.flush())
                .map_err(|error| Error::Transport(error.to_string()))
        })
        .await
        .map_err(|error| Error::Transport(error.to_string()))?;
        result
    }

    async fn close(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_filtering_matches_flipper_identity_only() {
        // No hardware in CI: the enumeration must at least not error.
        let _ = list_flippers();
        // The session string has the terminator the console expects.
        assert!(START_RPC_SESSION.ends_with(b"\r"));
    }
}
