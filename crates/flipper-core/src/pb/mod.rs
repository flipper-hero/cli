//! Generated protobuf types for the Flipper RPC (do not edit by hand;
//! regenerate with `cargo run -p genproto`).
//!
//! prost-build emits the package root (`pb.rs`) and one file per proto
//! (`pb_system.rs`, ...). The root's message types reference the leaves as
//! `super::super::pb_system::...`, so the modules must stay siblings exactly
//! like this. Everything is re-exported flat (`pb::Main`, `pb::File`, ...) and
//! the leaf modules keep friendly names (`pb::gui`, `pb::system`, ...).

#[allow(clippy::all)]
mod root {
    include!("pb.rs");
}

#[allow(clippy::all)]
pub mod pb_app {
    include!("pb_app.rs");
}
#[allow(clippy::all)]
pub mod pb_desktop {
    include!("pb_desktop.rs");
}
#[allow(clippy::all)]
pub mod pb_gpio {
    include!("pb_gpio.rs");
}
#[allow(clippy::all)]
pub mod pb_gui {
    include!("pb_gui.rs");
}
#[allow(clippy::all)]
pub mod pb_property {
    include!("pb_property.rs");
}
#[allow(clippy::all)]
pub mod pb_storage {
    include!("pb_storage.rs");
}
#[allow(clippy::all)]
pub mod pb_system {
    include!("pb_system.rs");
}

pub use pb_app as app;
pub use pb_gpio as gpio;
pub use pb_gui as gui;
pub use pb_property as property;
pub use pb_storage as storage;
pub use pb_system as system;

pub use root::*;
