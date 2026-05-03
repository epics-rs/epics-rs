//! PVA string encoding: variable-length [`Size`](super::size) prefix followed
//! by raw UTF-8 bytes.
//!
//! Empty strings are wire-encoded as the single byte `0x00`.
//! The null marker (`0xFF`) is reserved for nullable strings; we surface it
//! as `Ok(None)` from [`decode_string`].

use std::io::Cursor;

use super::buffer::{ByteOrder, DecodeError, ReadExt};
use super::size::{decode_size, encode_size_into};
use crate::decode_err;

/// Encode a string and return a freshly allocated buffer.
pub fn encode_string(value: &str, order: ByteOrder) -> Vec<u8> {
    let mut out = Vec::new();
    encode_string_into(value, order, &mut out);
    out
}

/// Encode a string into an existing buffer.
pub fn encode_string_into(value: &str, order: ByteOrder, out: &mut Vec<u8>) {
    let bytes = value.as_bytes();
    encode_size_into(bytes.len() as u32, order, out);
    out.extend_from_slice(bytes);
}

/// Decode a string. `Ok(None)` indicates the null marker (`0xFF` size byte).
pub fn decode_string(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<Option<String>, DecodeError> {
    let len = match decode_size(cur, order)? {
        Some(n) => n as usize,
        None => return Ok(None),
    };
    let bytes = cur.get_bytes(len)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|e| decode_err!("invalid UTF-8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_single_zero_byte() {
        let buf = encode_string("", ByteOrder::Little);
        assert_eq!(buf, vec![0x00]);
    }

    #[test]
    fn ascii_round_trip() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let original = "MY:PV:NAME";
            let buf = encode_string(original, order);
            assert_eq!(buf[0] as usize, original.len());
            let mut cur = Cursor::new(buf.as_slice());
            assert_eq!(
                decode_string(&mut cur, order).unwrap().as_deref(),
                Some(original)
            );
        }
    }

    #[test]
    fn utf8_round_trip() {
        let original = "한글: pvAccess 🎉";
        let buf = encode_string(original, ByteOrder::Little);
        assert_eq!(buf[0] as usize, original.len());
        let mut cur = Cursor::new(buf.as_slice());
        assert_eq!(
            decode_string(&mut cur, ByteOrder::Little)
                .unwrap()
                .as_deref(),
            Some(original)
        );
    }

    #[test]
    fn long_string_uses_extended_size() {
        let original = "x".repeat(300);
        let buf = encode_string(&original, ByteOrder::Little);
        assert_eq!(buf[0], 0xFE);
        assert_eq!(buf.len(), 5 + 300);
        let mut cur = Cursor::new(buf.as_slice());
        assert_eq!(
            decode_string(&mut cur, ByteOrder::Little)
                .unwrap()
                .as_deref(),
            Some(original.as_str())
        );
    }

    #[test]
    fn null_marker_yields_none() {
        let buf = vec![0xFF];
        let mut cur = Cursor::new(buf.as_slice());
        assert_eq!(decode_string(&mut cur, ByteOrder::Little).unwrap(), None);
    }

    #[test]
    fn matches_spvirit_byte_layout() {
        // spvirit::encode_string("MY:PV") → [0x05, b'M', b'Y', b':', b'P', b'V']
        assert_eq!(
            encode_string("MY:PV", ByteOrder::Little),
            vec![0x05, b'M', b'Y', b':', b'P', b'V']
        );
    }
}
