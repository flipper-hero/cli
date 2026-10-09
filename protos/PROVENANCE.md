# Provenance

These `.proto` (and nanopb `.options`) files are vendored verbatim from
https://github.com/flipperdevices/flipperzero-protobuf,
commit `1c84fa48919cbb71d1cc65236fc0ee36740e24c6`.

They describe the Flipper Zero RPC protocol. The firmware implementation lives in
`applications/services/rpc/` of flipperzero-firmware (and Momentum-Firmware).

Regenerate the Rust bindings with:

```sh
cargo run -p genproto
```

which writes `crates/flipper-core/src/pb/flipper.pb.rs` and
`crates/flipper-core/src/pb/descriptor.bin` (used by `flipper raw` for JSON
encoding/decoding via prost-reflect). Generated files are committed, matching the
convention of the ios-app repository; do not edit them by hand.
