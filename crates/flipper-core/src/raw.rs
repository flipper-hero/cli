//! `flipper raw`: one protobuf request assembled from JSON, responses as JSON.
//! Uses the committed descriptor set, so the JSON follows proto3 JSON mapping
//! (`bytes` are base64, enums by name), like the iOS app's `rpc_raw`.

use std::sync::OnceLock;
use std::time::Duration;

use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};

use crate::client::Client;
use crate::error::{Error, Result};
use crate::pb::Main;

const DESCRIPTOR: &[u8] = include_bytes!("pb/descriptor.bin");
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_PARTS: usize = 50;

fn main_descriptor() -> &'static MessageDescriptor {
    static POOL: OnceLock<MessageDescriptor> = OnceLock::new();
    POOL.get_or_init(|| {
        DescriptorPool::decode(DESCRIPTOR)
            .expect("committed descriptor set must decode")
            .get_message_by_name("PB.Main")
            .expect("PB.Main must exist in the descriptor set")
    })
}

impl<T: crate::client::Transport> Client<T> {
    /// Sends one request assembled from JSON and returns the responses as
    /// JSON lines. Only the `content` field is taken from the request; command
    /// id and framing stay ours.
    pub async fn raw_json(&self, json_request: &str) -> Result<String> {
        self.raw_json_with(json_request, Duration::from_secs(20))
            .await
    }

    pub async fn raw_json_with(&self, json_request: &str, timeout: Duration) -> Result<String> {
        let trimmed = json_request.trim();
        if trimmed.len() > MAX_REQUEST_BYTES {
            return Err(Error::Rpc("request larger than 16 KB".into()));
        }

        // The request is proto3 JSON against PB.Main. In proto3 JSON the
        // oneof's members sit at the top level ("systemPingRequest": {...});
        // an iOS-style {"content": {...}} wrapper is unwrapped for convenience.
        let json: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|error| Error::Rpc(format!("invalid request JSON: {error}")))?;
        let json = match json {
            serde_json::Value::Object(map) => match map.get("content") {
                Some(inner @ serde_json::Value::Object(_)) => inner.clone(),
                _ => serde_json::Value::Object(map),
            },
            other => other,
        };
        let envelope = DynamicMessage::deserialize(main_descriptor().clone(), json)
            .map_err(|error| Error::Rpc(format!("invalid request JSON: {error}")))?;
        let request: Main = envelope
            .transcode_to()
            .map_err(|error| Error::Rpc(format!("request does not fit PB.Main: {error}")))?;
        let Some(content) = request.content else {
            return Err(Error::Rpc(
                "request must set a 'content' field, e.g. {\"content\":{\"systemPingRequest\":{}}}"
                    .into(),
            ));
        };

        let parts = self.call_with(vec![content], Some(timeout), None).await?;

        let mut out = String::new();
        for part in parts.iter().take(MAX_RESPONSE_PARTS) {
            if !out.is_empty() {
                out.push('\n');
            }
            let mut dynamic = DynamicMessage::new(main_descriptor().clone());
            dynamic
                .transcode_from(part)
                .map_err(|error| Error::Rpc(format!("response transcode failed: {error}")))?;
            // The serde impl on DynamicMessage follows the proto3 JSON mapping.
            let line = serde_json::to_string(&dynamic)
                .map_err(|error| Error::Rpc(format!("response JSON failed: {error}")))?;
            if out.len() + line.len() > MAX_RESPONSE_BYTES {
                out.push_str("\n[truncated]");
                break;
            }
            out.push_str(&line);
        }
        Ok(out)
    }
}
