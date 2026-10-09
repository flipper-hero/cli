//! Flipper RPC frames: a varint length prefix followed by one `PB.Main`.

use prost::Message;

use crate::error::{Error, Result};
use crate::pb::Main;
use crate::varint;

/// Matches the firmware's receive buffer limit.
pub const MAX_FRAME_SIZE: u64 = 1 << 20;

/// Encodes one message as `varint(len) || body`.
pub fn encode(message: &Main) -> Vec<u8> {
    let body = message.encode_to_vec();
    let mut out = varint::encode(body.len() as u64);
    out.extend_from_slice(&body);
    out
}

/// Reassembles frames out of arbitrarily chunked transport reads,
/// mirroring `FrameDecoder` in the iOS FlipperKit.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk and returns every complete frame it completed.
    /// On a bad frame the buffer is reset so the stream can resynchronize.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Main>> {
        self.buf.extend_from_slice(chunk);
        let mut messages = Vec::new();
        while let Some((length, prefix)) = varint::decode(&self.buf)? {
            if length > MAX_FRAME_SIZE {
                self.buf.clear();
                return Err(Error::FrameTooLarge(
                    usize::try_from(length).unwrap_or(usize::MAX),
                ));
            }
            let total = prefix + length as usize;
            if self.buf.len() < total {
                break;
            }
            match Main::decode(&self.buf[prefix..total]) {
                Ok(message) => messages.push(message),
                Err(_) => {
                    self.buf.clear();
                    return Err(Error::MalformedFrame);
                }
            }
            self.buf.drain(..total);
        }
        Ok(messages)
    }

    /// Bytes buffered but not yet part of a complete frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::main::Content;
    use crate::pb::system::PingRequest;
    use crate::pb::Empty;

    fn ping(id: u32) -> Main {
        Main {
            command_id: id,
            command_status: crate::pb::CommandStatus::Ok as i32,
            has_next: false,
            content: Some(Content::SystemPingRequest(PingRequest {
                data: vec![1, 2, 3, 4],
            })),
        }
    }

    fn empty(id: u32, has_next: bool) -> Main {
        Main {
            command_id: id,
            command_status: crate::pb::CommandStatus::Ok as i32,
            has_next,
            content: Some(Content::Empty(Empty {})),
        }
    }

    #[test]
    fn round_trips_one_frame() {
        let message = ping(7);
        let bytes = encode(&message);
        let mut decoder = Decoder::new();
        let decoded = decoder.push(&bytes).unwrap();
        assert_eq!(decoded, vec![message]);
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn reassembles_across_arbitrary_chunks() {
        let messages = vec![empty(1, true), empty(1, true), empty(1, false)];
        let mut bytes = Vec::new();
        for message in &messages {
            bytes.extend_from_slice(&encode(message));
        }
        // Feed one byte at a time; frames only complete at the end.
        let mut decoder = Decoder::new();
        let mut decoded = Vec::new();
        for byte in &bytes {
            decoded.extend(decoder.push(&[*byte]).unwrap());
        }
        assert_eq!(decoded, messages);
    }

    #[test]
    fn multiple_frames_in_one_chunk() {
        let messages = vec![ping(1), ping(2), empty(3, false)];
        let mut bytes = Vec::new();
        for message in &messages {
            bytes.extend_from_slice(&encode(message));
        }
        let decoded = Decoder::new().push(&bytes).unwrap();
        assert_eq!(decoded, messages);
    }

    #[test]
    fn oversized_frame_is_rejected_and_resets() {
        let mut bytes = varint::encode(MAX_FRAME_SIZE + 1);
        bytes.extend_from_slice(&[0u8; 16]);
        let mut decoder = Decoder::new();
        // The length prefix alone already proves the frame is too large,
        // exactly like the iOS FrameDecoder.
        match decoder.push(&bytes[..bytes.len() / 2]) {
            Err(Error::FrameTooLarge(n)) => assert!(n > MAX_FRAME_SIZE as usize),
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
        // Buffer was cleared, the decoder still works afterwards.
        let recovered = decoder.push(&encode(&ping(9))).unwrap();
        assert_eq!(recovered.len(), 1);
    }

    #[test]
    fn malformed_body_is_rejected_and_resets() {
        // A valid-looking length prefix followed by garbage.
        let mut bytes = varint::encode(8);
        bytes.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        let mut decoder = Decoder::new();
        assert!(matches!(decoder.push(&bytes), Err(Error::MalformedFrame)));
        assert_eq!(decoder.buffered(), 0);
        assert_eq!(decoder.push(&encode(&ping(5))).unwrap().len(), 1);
    }
}
