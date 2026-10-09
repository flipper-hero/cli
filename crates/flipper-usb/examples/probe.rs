//! Serial probe: opens the Flipper's port, performs the session dance and
//! dumps every byte that comes back. Debugging aid, not part of the CLI.

use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: probe <port>");
    let mut port: Box<dyn serialport::SerialPort> = serialport::new(&path, 115_200)
        .timeout(Duration::from_millis(200))
        .open()?;
    let _ = port.write_request_to_send(true);
    let _ = port.write_data_terminal_ready(true);

    fn dump(port: &mut Box<dyn serialport::SerialPort>, label: &str) {
        let start = std::time::Instant::now();
        let mut buffer = [0u8; 1024];
        while start.elapsed() < Duration::from_millis(600) {
            match port.read(&mut buffer) {
                Ok(0) => {}
                Ok(n) => println!(
                    "{label} (+{:?}): {}",
                    start.elapsed(),
                    buffer[..n]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                ),
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => println!("{label} read error: {e}"),
            }
        }
    }

    println!("-- newline");
    port.write_all(b"\r")?;
    dump(&mut port, "console");

    println!("-- start_rpc_session");
    port.write_all(b"start_rpc_session\r")?;
    dump(&mut port, "session");

    println!("-- ping frame");
    // PB.Main { command_id: 1, status: OK, has_next: false,
    //            content: systemPingRequest { data: [1,2,3,4] } }
    // body: 08 01 10 00 28 00 4a 06 0a 04 01 02 03 04   (15 bytes)
    let body: [u8; 14] = [
        0x08, 0x01, 0x10, 0x00, 0x28, 0x00, 0x4a, 0x06, 0x0a, 0x04, 0x01, 0x02, 0x03, 0x04,
    ];
    let mut frame = vec![body.len() as u8];
    frame.extend_from_slice(&body);
    println!(
        "frame: {}",
        frame.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    port.write_all(&frame)?;
    dump(&mut port, "rpc");

    Ok(())
}
