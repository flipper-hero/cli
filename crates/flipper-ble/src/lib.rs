//! Bluetooth LE transport for the Flipper Zero RPC, built on btleplug
//! (macOS CoreBluetooth, Linux BlueZ, Windows WinRT).
//!
//! The serial service requires authenticated encryption, so the host must be
//! paired with the Flipper before characteristics will talk:
//! - macOS: subscribing to the characteristics triggers the system pairing
//!   dialog on its own (the terminal needs Bluetooth permission on first run).
//! - Linux: this crate pairs over D-Bus on demand, with a passkey agent whose
//!   confirmations happen on the Flipper's screen.
//! - Windows: pair once via Settings > Bluetooth (or PowerShell), then the
//!   transport just connects; an explicit error explains this when unpaired.
//!
//! Framing and RPC live in flipper-core; this crate only implements the link:
//! notify on serialRead, writes to serialWrite, and flow control via the
//! flowControl characteristic (big-endian UInt32 = free buffer bytes), exactly
//! like `BLETransport` in the iOS app.

use std::time::Duration;

use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use flipper_core::client::FlowControl;
use flipper_core::error::{Error, Result};
use flipper_core::Transport;
use futures::StreamExt;
use thiserror::Error as ThisError;
use tokio::sync::mpsc;

/// GATT identity of the Flipper serial service (`furi_hal_bt_serial`).
pub mod uuids {
    use uuid::Uuid;

    pub const SERIAL: &str = "8fe5b3d5-2e7f-4a98-2a48-7acc60fe0000";
    pub const SERIAL_READ: &str = "19ed82ae-ed21-4c9d-4145-228e61fe0000";
    pub const SERIAL_WRITE: &str = "19ed82ae-ed21-4c9d-4145-228e62fe0000";
    pub const FLOW_CONTROL: &str = "19ed82ae-ed21-4c9d-4145-228e63fe0000";

    /// Services the Flipper advertises (16-bit 0x3080..0x3083 on the
    /// Bluetooth base UUID).
    pub fn advertised() -> Vec<Uuid> {
        [
            "00003080-0000-1000-8000-00805f9b34fb",
            "00003081-0000-1000-8000-00805f9b34fb",
            "00003082-0000-1000-8000-00805f9b34fb",
            "00003083-0000-1000-8000-00805f9b34fb",
        ]
        .iter()
        .filter_map(|raw| Uuid::parse_str(raw).ok())
        .collect()
    }

    pub fn parse(raw: &str) -> Uuid {
        Uuid::parse_str(raw).expect("constant UUID must parse")
    }
}

#[derive(Debug, ThisError)]
pub enum BleError {
    #[error("bluetooth is unavailable or was not approved in time; on macOS the terminal app needs Bluetooth permission (System Settings > Privacy & Security > Bluetooth)")]
    NoAdapter,
    #[error("no flipper found (is bluetooth on and the device advertising?)")]
    NotFound,
    #[error("timed out waiting for the flipper")]
    Timeout,
    #[error("this flipper is not paired with this computer; pair it once in the OS bluetooth settings and try again (macOS pairs automatically)")]
    NotPaired,
    #[error("bluetooth: {0}")]
    Btleplug(String),
}

impl From<BleError> for Error {
    fn from(error: BleError) -> Self {
        Error::Transport(error.to_string())
    }
}

/// A Flipper seen during a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFlipper {
    /// Stable-ish identifier as printed by `flipper scan`; pass it to
    /// `--device` to connect.
    pub id: String,
    pub name: String,
    pub rssi: i16,
    /// Only meaningful on Linux/macOS/Windows for the pairing shims.
    pub paired: bool,
}

pub async fn first_adapter() -> std::result::Result<Adapter, BleError> {
    let manager = Manager::new()
        .await
        .map_err(|error| BleError::Btleplug(error.to_string()))?;
    // On macOS without Bluetooth permission the central never powers on and
    // btleplug would wait forever; bound the wait and explain instead.
    let mut adapters = tokio::time::timeout(ADAPTER_WAIT, manager.adapters())
        .await
        .map_err(|_| BleError::NoAdapter)?
        .map_err(|error| BleError::Btleplug(error.to_string()))?;
    adapters.pop().ok_or(BleError::NoAdapter)
}

/// How long to wait for the platform's Bluetooth stack to come up.
const ADAPTER_WAIT: Duration = Duration::from_secs(6);

async fn name_of(peripheral: &Peripheral) -> String {
    peripheral
        .properties()
        .await
        .ok()
        .flatten()
        .and_then(|props| props.local_name)
        .unwrap_or_else(|| "Flipper".to_owned())
}

/// Whether this advertisement looks like a Flipper: by advertised service or
/// by name, mirroring the iOS app's filter.
async fn is_flipper(peripheral: &Peripheral) -> bool {
    let Ok(Some(properties)) = peripheral.properties().await else {
        return false;
    };
    let advertised = uuids::advertised();
    properties
        .services
        .iter()
        .any(|service| advertised.contains(service))
        || properties
            .local_name
            .as_deref()
            .is_some_and(|name| name.starts_with("Flipper"))
}

/// Scans for Flippers for `duration` and returns the ones found, strongest
/// signal first. No filter is passed to the stack: BlueZ merges scan filters
/// across all D-Bus clients and can drop advertisement types, so post-filter
/// instead (the btleplug README's own advice).
pub async fn scan(
    adapter: &Adapter,
    duration: Duration,
) -> std::result::Result<Vec<DiscoveredFlipper>, BleError> {
    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|error| BleError::Btleplug(error.to_string()))?;
    tokio::time::sleep(duration).await;

    let mut found: Vec<DiscoveredFlipper> = Vec::new();
    let peripherals = adapter
        .peripherals()
        .await
        .map_err(|error| BleError::Btleplug(error.to_string()))?;
    for peripheral in peripherals {
        if !is_flipper(&peripheral).await {
            continue;
        }
        let properties = peripheral
            .properties()
            .await
            .ok()
            .flatten()
            .ok_or(BleError::NotFound)?;
        found.push(DiscoveredFlipper {
            id: format!("{:?}", peripheral.id()),
            name: properties
                .local_name
                .unwrap_or_else(|| "Flipper".to_owned()),
            rssi: properties.rssi.unwrap_or(i16::MIN),
            paired: cfg!(target_os = "macos") || pairing_state(&peripheral).await.unwrap_or(true),
        });
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.rssi));
    Ok(found)
}

/// One connected Flipper over Bluetooth LE.
pub struct BleTransport {
    peripheral: Peripheral,
    write_char: btleplug::api::Characteristic,
    rx: mpsc::Receiver<Vec<u8>>,
    flow: std::sync::Arc<FlowControl>,
    write_limit: usize,
}

impl BleTransport {
    /// Connects to `id` as printed by [`scan`], or to the strongest Flipper
    /// found in a quick scan when `id` is None.
    pub async fn connect(
        adapter: &Adapter,
        id: Option<&str>,
    ) -> std::result::Result<Self, BleError> {
        let peripheral = match id {
            Some(id) => find_by_id(adapter, id).await?,
            None => {
                let found = scan(adapter, Duration::from_secs(4)).await?;
                let Some(first) = found.first() else {
                    return Err(BleError::NotFound);
                };
                find_by_id(adapter, &first.id).await?
            }
        };

        #[cfg(target_os = "linux")]
        pairing::ensure_paired(&peripheral).await?;

        peripheral
            .connect()
            .await
            .map_err(|error| BleError::Btleplug(error.to_string()))?;
        peripheral
            .discover_services()
            .await
            .map_err(|error| BleError::Btleplug(error.to_string()))?;

        let serial = uuids::parse(uuids::SERIAL);
        let read_uuid = uuids::parse(uuids::SERIAL_READ);
        let write_uuid = uuids::parse(uuids::SERIAL_WRITE);
        let flow_uuid = uuids::parse(uuids::FLOW_CONTROL);

        let Some(service) = peripheral
            .services()
            .iter()
            .find(|service| service.uuid == serial)
            .cloned()
        else {
            let _ = peripheral.disconnect().await;
            return Err(BleError::Btleplug(
                "device does not expose the flipper serial service".to_owned(),
            ));
        };

        let read_char = service
            .characteristics
            .iter()
            .find(|c| c.uuid == read_uuid)
            .cloned();
        let write_char = service
            .characteristics
            .iter()
            .find(|c| c.uuid == write_uuid)
            .cloned();
        let flow_char = service
            .characteristics
            .iter()
            .find(|c| c.uuid == flow_uuid)
            .cloned();
        let (Some(read_char), Some(write_char), Some(flow_char)) =
            (read_char, write_char, flow_char)
        else {
            let _ = peripheral.disconnect().await;
            return Err(BleError::Btleplug(
                "serial service is missing required characteristics".to_owned(),
            ));
        };

        // Subscribing to the authenticated characteristics is what triggers
        // pairing on macOS; on an unpaired Windows host it fails here.
        for characteristic in [&read_char, &flow_char] {
            if let Err(error) = peripheral.subscribe(characteristic).await {
                let _ = peripheral.disconnect().await;
                return Err(classify_link_error(error));
            }
        }

        let flow = std::sync::Arc::new(FlowControl::default());
        let (tx, rx) = mpsc::channel::<Vec<u8>>(64);

        let notifications = peripheral
            .notifications()
            .await
            .map_err(|error| BleError::Btleplug(error.to_string()))?;
        let notify_flow = std::sync::Arc::clone(&flow);
        tokio::spawn(notifications.for_each(move |notification| {
            let tx = tx.clone();
            let flow = std::sync::Arc::clone(&notify_flow);
            async move {
                if notification.uuid == read_uuid {
                    let _ = tx.send(notification.value).await;
                } else if notification.uuid == flow_uuid && notification.value.len() >= 4 {
                    let free = u32::from_be_bytes([
                        notification.value[0],
                        notification.value[1],
                        notification.value[2],
                        notification.value[3],
                    ]);
                    flow.update(free).await;
                }
            }
        }));

        // The first flow-control report arrives as a notification; until then
        // assume the documented ~1000-byte session buffer so small requests go
        // out without waiting.
        flow.seed(512).await;

        // One GATT write carries at most MTU-3 ATT payload bytes; floor at 20
        // like the iOS client when the platform does not report a real MTU.
        let mtu = peripheral.mtu() as usize;
        let write_limit = mtu.saturating_sub(3).max(20);

        Ok(Self {
            peripheral,
            write_char,
            rx,
            flow,
            write_limit,
        })
    }

    pub async fn disconnect(&self) {
        let _ = self.peripheral.disconnect().await;
    }

    /// ATT payload bytes we may put in one write.
    fn chunk_limit(&self) -> usize {
        self.write_limit
    }
}

fn classify_link_error(error: btleplug::Error) -> BleError {
    let text = error.to_string();
    if text.to_lowercase().contains("pair")
        || text.to_lowercase().contains("encrypt")
        || text.to_lowercase().contains("insufficient")
    {
        BleError::NotPaired
    } else {
        BleError::Btleplug(text)
    }
}

async fn find_by_id(adapter: &Adapter, id: &str) -> std::result::Result<Peripheral, BleError> {
    // A fresh scan is how we resolve names and (on Linux) addresses; an
    // already-known peripheral matches without it.
    let _ = adapter.start_scan(ScanFilter::default()).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let peripherals = adapter
        .peripherals()
        .await
        .map_err(|error| BleError::Btleplug(error.to_string()))?;
    for peripheral in peripherals {
        if format!("{:?}", peripheral.id()) == id || name_of(&peripheral).await == id {
            return Ok(peripheral);
        }
    }
    Err(BleError::NotFound)
}

#[cfg(target_os = "linux")]
async fn pairing_state(peripheral: &Peripheral) -> Option<bool> {
    pairing::is_paired(peripheral).await
}

#[cfg(not(target_os = "linux"))]
async fn pairing_state(_peripheral: &Peripheral) -> Option<bool> {
    None
}

impl Transport for BleTransport {
    async fn recv(&mut self) -> Result<Vec<u8>> {
        self.rx.recv().await.ok_or(Error::NotConnected)
    }

    async fn send(&mut self, data: &[u8]) -> Result<()> {
        // Spend flow-control credit in ATT-write-sized chunks, mirroring the
        // iOS transport's reserve/acknowledge loop.
        let limit = self.chunk_limit();
        let mut offset = 0;
        while offset < data.len() {
            let granted = self.flow.reserve((data.len() - offset).min(limit)).await;
            let chunk = &data[offset..offset + granted];
            self.peripheral
                .write(&self.write_char, chunk, WriteType::WithResponse)
                .await
                .map_err(|error| Error::Transport(error.to_string()))?;
            offset += granted;
        }
        Ok(())
    }

    async fn close(&mut self) {
        let _ = self.peripheral.disconnect().await;
    }
}

#[cfg(target_os = "linux")]
mod pairing {
    //! BlueZ pairing over D-Bus. btleplug does not expose pairing, so before
    //! connecting we make sure a bond exists: we register a minimal agent,
    //! call `Device1.Pair`, and let the user confirm the passkey on the
    //! Flipper's screen (the firmware uses PinCodeShow / PinCodeVerifyYesNo).

    use std::collections::HashMap;
    use std::time::Duration;

    use btleplug::api::Peripheral as _;
    use btleplug::platform::Peripheral;
    use tokio::time::timeout;
    use zbus::fdo::Result as FdoResult;
    use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

    use super::BleError;

    const AGENT_PATH: &str = "/flipper_hero/cli/agent";

    #[zbus::proxy(
        interface = "org.bluez.AgentManager1",
        default_service = "org.bluez",
        default_path = "/org/bluez"
    )]
    trait AgentManager {
        fn register_agent(&self, agent: ObjectPath<'_>, capability: &str) -> zbus::Result<()>;
        fn unregister_agent(&self, agent: ObjectPath<'_>) -> zbus::Result<()>;
    }

    #[zbus::proxy(interface = "org.bluez.Device1", assume_defaults = true)]
    trait Device {
        fn pair(&self) -> zbus::Result<()>;
        #[zbus(property)]
        fn paired(&self) -> zbus::Result<bool>;
        #[zbus(property)]
        fn address(&self) -> zbus::Result<String>;
    }

    #[zbus::proxy(
        interface = "org.bluez.ObjectManager",
        default_service = "org.bluez",
        default_path = "/"
    )]
    trait ObjectManager {
        fn get_managed_objects(
            &self,
        ) -> zbus::Result<HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>>;
    }

    /// A no-dialog agent: the Flipper shows the code and its user confirms on
    /// the device itself, so the host side just accepts and surfaces the code.
    struct Agent;

    #[zbus::interface(name = "org.bluez.Agent1")]
    impl Agent {
        fn release(&self) {}

        fn request_pin_code(&self, _device: ObjectPath<'_>) -> FdoResult<String> {
            Ok("0000".to_owned())
        }

        fn display_pin_code(&self, _device: ObjectPath<'_>, pin_code: String) {
            eprintln!("Pairing: confirm code {pin_code} on the Flipper, then press OK there.");
        }

        fn request_passkey(&self, _device: ObjectPath<'_>) -> FdoResult<u32> {
            Ok(0)
        }

        fn display_passkey(&self, _device: ObjectPath<'_>, passkey: u32, _entered: u16) {
            eprintln!("Pairing: confirm code {passkey:06} on the Flipper, then press OK there.");
        }

        fn request_confirmation(&self, _device: ObjectPath<'_>, passkey: u32) -> FdoResult<()> {
            eprintln!("Pairing: confirm code {passkey:06} on the Flipper, then press OK there.");
            Ok(())
        }

        fn request_authorization(&self, _device: ObjectPath<'_>) -> FdoResult<()> {
            Ok(())
        }

        fn authorize_service(&self, _device: ObjectPath<'_>, _uuid: String) -> FdoResult<()> {
            Ok(())
        }
    }

    pub async fn is_paired(peripheral: &Peripheral) -> Option<bool> {
        let properties = peripheral.properties().await.ok()??;
        let connection = zbus::Connection::system().await.ok()?;
        let address = properties.address.to_string();
        let proxy = ObjectManagerProxy::new(&connection).await.ok()?;
        let objects = proxy.get_managed_objects().await.ok()?;
        objects
            .into_iter()
            .find_map(|(path, interfaces)| {
                interfaces.get("org.bluez.Device1").and_then(|properties| {
                    properties
                        .get("Address")
                        .and_then(|value| value.try_into().ok())
                        .filter(|found: &String| found.eq_ignore_ascii_case(&address))
                        .and_then(|_| {
                            interfaces
                                .get("org.bluez.Device1")?
                                .get("Paired")
                                .and_then(|value| <&Value as TryInto<bool>>::try_into(value).ok())
                        })
                })
            })
            .or(Some(true)) // unknown device state: let the connect attempt decide
    }

    pub async fn ensure_paired(peripheral: &Peripheral) -> std::result::Result<(), BleError> {
        let Ok(Some(properties)) = peripheral.properties().await else {
            return Ok(()); // let the connect attempt surface anything real
        };
        let connection = zbus::Connection::system()
            .await
            .map_err(|error| BleError::Btleplug(format!("system bus: {error}")))?;
        let address = properties.address.to_string();

        // Find (or wait for) the Device1 object for this address.
        let object_proxy = {
            let object_manager = ObjectManagerProxy::new(&connection)
                .await
                .map_err(|error| BleError::Btleplug(error.to_string()))?;
            let objects = object_manager
                .get_managed_objects()
                .await
                .map_err(|error| BleError::Btleplug(error.to_string()))?;
            let path = objects
                .into_iter()
                .find_map(|(path, interfaces)| {
                    interfaces.get("org.bluez.Device1").and_then(|device| {
                        device
                            .get("Address")
                            .and_then(|value| value.try_into().ok())
                            .filter(|found: &String| found.eq_ignore_ascii_case(&address))
                            .map(|_| path)
                    })
                })
                .ok_or(BleError::NotFound)?;
            DeviceProxy::builder(&connection)
                .path(path)
                .map_err(|error| BleError::Btleplug(error.to_string()))?
                .build()
                .await
                .map_err(|error| BleError::Btleplug(error.to_string()))?
        };

        if object_proxy.paired().await.unwrap_or(false) {
            return Ok(());
        }

        // Register the agent, pair, clean the agent up afterwards.
        let connection_ref = &connection;
        let agent = Agent;
        connection_ref
            .object_server()
            .at(AGENT_PATH, agent)
            .await
            .map_err(|error| BleError::Btleplug(error.to_string()))?;
        let agent_manager = AgentManagerProxy::new(connection_ref)
            .await
            .map_err(|error| BleError::Btleplug(error.to_string()))?;
        let agent_path = ObjectPath::try_from(AGENT_PATH)
            .map_err(|error| BleError::Btleplug(error.to_string()))?;
        let _ = agent_manager
            .register_agent(agent_path, "KeyboardDisplay")
            .await;

        let pairing = timeout(Duration::from_secs(90), object_proxy.pair()).await;
        let _ = agent_manager.unregister_agent(agent_path).await;
        let _ = connection_ref
            .object_server()
            .remove::<Agent, _>(AGENT_PATH)
            .await;

        match pairing {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(BleError::Btleplug(format!("pairing: {error}"))),
            Err(_) => Err(BleError::Timeout),
        }
    }
}
