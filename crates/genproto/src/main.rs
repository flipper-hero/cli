//! Regenerates `crates/flipper-core/src/pb/` from the vendored protos.
//! Run: `cargo run -p genproto` (protoc must be on PATH).

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let protos = workspace.join("protos");
    let out = workspace.join("crates/flipper-core/src/pb");
    std::fs::create_dir_all(&out)?;

    let tmp = tempfile_out();
    prost_build::Config::new()
        .file_descriptor_set_path(&tmp)
        .out_dir(&out)
        .compile_protos(&[protos.join("flipper.proto")], &[&protos])?;

    std::fs::rename(&tmp, out.join("descriptor.bin"))?;
    println!("wrote {}", out.join("flipper.pb.rs").display());
    println!("wrote {}", out.join("descriptor.bin").display());
    Ok(())
}

fn tempfile_out() -> PathBuf {
    std::env::temp_dir().join("flipper-cli-descriptor.bin")
}
