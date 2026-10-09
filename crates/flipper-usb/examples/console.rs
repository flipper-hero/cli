//! Console probe: opens the Flipper's port and runs a CLI command on the
//! firmware console, printing everything that comes back.
//! Usage: cargo run -p flipper-usb --example console -- <port> <command>

use std::io::Read as _;
use std::io::Write as _;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: console <port> <command>");
    let command = args.next().unwrap_or_else(|| "help".to_owned());

    let mut port: Box<dyn serialport::SerialPort> = serialport::new(&path, 115_200)
        .timeout(Duration::from_millis(200))
        .open()?;
    let _ = port.write_request_to_send(true);
    let _ = port.write_data_terminal_ready(true);

    // Wake the console and drain the banner.
    port.write_all(b"\r")?;
    drain(&mut port, 400);

    let line = format!("{command}\r");
    port.write_all(line.as_bytes())?;

    // Print everything for up to 3 seconds of silence-bounded reads.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buffer = [0u8; 1024];
    let mut last_byte = Instant::now();
    while Instant::now() < deadline {
        match port.read(&mut buffer) {
            Ok(0) => {}
            Ok(n) => {
                print!("{}", String::from_utf8_lossy(&buffer[..n]));
                last_byte = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if last_byte.elapsed() > Duration::from_millis(700) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    println!();
    Ok(())
}

fn drain(port: &mut Box<dyn serialport::SerialPort>, milliseconds: u64) {
    let deadline = Instant::now() + Duration::from_millis(milliseconds);
    let mut scratch = [0u8; 1024];
    while Instant::now() < deadline {
        match port.read(&mut scratch) {
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
}
