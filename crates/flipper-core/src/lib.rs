//! Transport-agnostic Flipper Zero RPC.
//!
//! `Client` speaks the Flipper's protobuf RPC over any [`Transport`] byte pipe:
//! framing, command-id matching, chunked responses, timeouts and unsolicited
//! messages (screen frames) are handled here. On top of it, [`api`] methods
//! mirror what the FlipperHero iOS app can do over the same link.
//!
//! Transports live in their own crates: [`flipper-usb`] (CDC-ACM serial) and
//! [`flipper-ble`] (Bluetooth LE via btleplug).

pub mod api;
pub mod client;
pub mod error;
pub mod frame;
pub mod path;
pub mod raw;
pub mod screen;
pub mod varint;

pub mod pb;

pub use api::limits;
pub use client::{Client, Transport};
pub use error::{Error, Result};
pub use path::FlipperPath;
pub use screen::{FlipperKey, FlipperScreenFrame, Orientation};
