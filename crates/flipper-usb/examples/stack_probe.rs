//! Step-logged end-to-end probe of the real UsbTransport + Client stack.

use std::time::Duration;

use flipper_core::client::Transport;
use flipper_core::pb::main::Content;
use flipper_core::pb::system::PingRequest;
use flipper_core::pb::{CommandStatus, Main};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port_path = std::env::args().nth(1);

    // Stage 1: raw transport round trip.
    eprintln!("1. opening transport");
    let mut transport = {
        let port = port_path.clone();
        tokio::task::spawn_blocking(move || flipper_usb::UsbTransport::open(port.as_deref()))
            .await
            .expect("join")?
    };
    eprintln!("2. transport open; sending raw ping frame");

    let frame = flipper_core::frame::encode(&Main {
        command_id: 1,
        command_status: CommandStatus::Ok as i32,
        has_next: false,
        content: Some(Content::SystemPingRequest(PingRequest {
            data: vec![1, 2, 3, 4],
        })),
    });
    transport.send(&frame).await?;
    eprintln!("3. raw frame sent; waiting for bytes");

    let raw = tokio::time::timeout(Duration::from_secs(3), transport.recv()).await;
    match raw {
        Ok(Ok(bytes)) => eprintln!(
            "4. raw recv: {}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
        Ok(Err(error)) => eprintln!("4. raw recv error: {error}"),
        Err(_) => eprintln!("4. raw recv timed out"),
    }

    // Stage 2: the full client.
    eprintln!("5. starting client");
    let client = flipper_core::Client::start(transport, Duration::from_secs(3));
    let mut unsolicited = client.unsolicited();
    tokio::spawn(async move {
        loop {
            if let Ok(message) = unsolicited.recv().await {
                eprintln!(
                    "   [unsolicited] id={} status={:?}",
                    message.command_id,
                    message.command_status()
                );
            }
        }
    });
    let started = std::time::Instant::now();
    eprintln!("6. pinging");
    match client.ping().await {
        Ok(()) => eprintln!("7. ping ok after {:?}", started.elapsed()),
        Err(error) => eprintln!("7. ping failed after {:?}: {error}", started.elapsed()),
    }
    Ok(())
}
