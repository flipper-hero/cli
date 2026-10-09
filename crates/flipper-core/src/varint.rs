//! Varint encoding for RPC frame length prefixes.

use crate::error::{Error, Result};

/// Encodes a varint, least significant group first.
pub fn encode(mut value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return out;
        }
    }
}

/// Decodes a varint from the front of `bytes`.
/// Returns `Ok(None)` when the buffer ends mid-varint, and an error when the
/// varint is absurdly long (the Flipper only ever length-frames 32-bit sizes).
pub fn decode(bytes: &[u8]) -> Result<Option<(u64, usize)>> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(Some((result, index + 1)));
        }
        shift += 7;
        if shift > 28 {
            return Err(Error::MalformedFrame);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for value in [
            0u64,
            1,
            127,
            128,
            300,
            16383,
            16384,
            u32::MAX as u64,
            1 << 30,
        ] {
            let encoded = encode(value);
            let (decoded, used) = decode(&encoded).unwrap().unwrap();
            assert_eq!(decoded, value, "round trip for {value}");
            assert_eq!(used, encoded.len());
        }
    }

    #[test]
    fn incomplete_returns_none() {
        assert_eq!(decode(&[]).unwrap(), None);
        assert_eq!(decode(&[0x80]).unwrap(), None);
        assert_eq!(decode(&[0x80, 0x80]).unwrap(), None);
    }

    #[test]
    fn overlong_is_malformed() {
        assert!(matches!(
            decode(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]),
            Err(Error::MalformedFrame)
        ));
    }

    #[test]
    fn trailing_bytes_are_ignored() {
        let (value, used) = decode(&[0x7f, 0xff, 0xff]).unwrap().unwrap();
        assert_eq!(value, 0x7f);
        assert_eq!(used, 1);
    }
}
