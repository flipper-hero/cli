//! Output conventions: `--json` gives one machine-readable document on
//! stdout, plain mode gives human text; progress and diagnostics always go to
//! stderr. Exit codes: 0 ok, 1 operation failed, 3 device not found.

use serde_json::{json, Value};

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_NO_DEVICE: i32 = 3;

/// Prints a command result.
pub fn emit(json: bool, value: Value, human: impl Into<String>) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "data": value }))
                .expect("result is valid json")
        );
    } else {
        print!("{}", human.into());
    }
}

/// Prints an error and returns the process exit code.
pub fn fail(json: bool, error: &anyhow::Error) -> i32 {
    if error_chain_contains(error, "no flipper found") || error_chain_contains(error, "NotFound") {
        if json {
            println!(
                "{}",
                serde_json::json!({ "ok": false, "error": error.to_string(), "kind": "not-found" })
            );
        }
        return EXIT_NO_DEVICE;
    }
    if json {
        println!(
            "{}",
            serde_json::json!({ "ok": false, "error": error.to_string() })
        );
    } else {
        eprintln!("error: {error:#}");
    }
    EXIT_FAILED
}

fn error_chain_contains(error: &anyhow::Error, needle: &str) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().to_lowercase().contains(needle))
}
