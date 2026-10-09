//! Transport resolution: pick USB or BLE, open it, and hand back a client.

use std::time::Duration;

use flipper_ble::BleTransport;
use flipper_core::client::Client;
use flipper_usb::UsbTransport;

/// Both concrete clients behind one enum, so commands can be generic over
/// `Client<T>` without dynamic dispatch.
pub enum Link {
    Usb(Client<UsbTransport>),
    Ble(Client<BleTransport>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TransportChoice {
    /// USB serial if a Flipper port exists, otherwise Bluetooth LE.
    Auto,
    /// Force USB CDC-ACM.
    Usb,
    /// Force Bluetooth LE.
    Ble,
}

pub struct ConnectOptions {
    pub transport: TransportChoice,
    /// BLE device id or exact name from `flipper scan`.
    pub device: Option<String>,
    /// USB device path (`/dev/cu.usbmodem*`, `/dev/ttyACM0`, `COM4`).
    pub port: Option<String>,
    pub timeout: Duration,
}

impl Link {
    pub async fn open(options: &ConnectOptions) -> anyhow::Result<Self> {
        match options.transport {
            TransportChoice::Usb => Ok(Self::Usb(Self::usb_client_retried(options).await?)),
            TransportChoice::Ble => Ok(Self::Ble(Self::ble_client_retried(options).await?)),
            TransportChoice::Auto => {
                if flipper_usb::find_port(options.port.as_deref()).is_ok() {
                    match Self::usb_client_retried(options).await {
                        Ok(client) => return Ok(Self::Usb(client)),
                        Err(error) => {
                            eprintln!("usb connect failed ({error:#}); trying bluetooth");
                        }
                    }
                }
                Ok(Self::Ble(Self::ble_client_retried(options).await?))
            }
        }
    }

    /// One immediate retry: the previous CLI process sometimes still holds
    /// the port for a moment (macOS allows shared opens that split bytes).
    async fn usb_client_retried(options: &ConnectOptions) -> anyhow::Result<Client<UsbTransport>> {
        match Self::usb_client(options).await {
            Ok(client) => Ok(client),
            Err(first) => {
                tokio::time::sleep(Duration::from_millis(400)).await;
                Self::usb_client(options)
                    .await
                    .map_err(|second| anyhow::anyhow!("{first}; retry also failed: {second}"))
            }
        }
    }

    async fn ble_client_retried(options: &ConnectOptions) -> anyhow::Result<Client<BleTransport>> {
        match Self::ble_client(options).await {
            Ok(client) => Ok(client),
            Err(first) => {
                tokio::time::sleep(Duration::from_millis(400)).await;
                Self::ble_client(options)
                    .await
                    .map_err(|second| anyhow::anyhow!("{first}; retry also failed: {second}"))
            }
        }
    }

    async fn usb_client(options: &ConnectOptions) -> anyhow::Result<Client<UsbTransport>> {
        let port = options.port.clone();
        let transport = tokio::task::spawn_blocking(move || {
            UsbTransport::open(port.as_deref()).map_err(|error| anyhow::anyhow!("{error}"))
        })
        .await??;
        Ok(Client::start(transport, options.timeout))
    }

    async fn ble_client(options: &ConnectOptions) -> anyhow::Result<Client<BleTransport>> {
        let adapter = flipper_ble::first_adapter()
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let transport = BleTransport::connect(&adapter, options.device.as_deref())
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(Client::start(transport, options.timeout))
    }
}
