//! PVA string encoding: variable-length [`Size`](super::size) prefix followed
//! by raw bytes (NOT guaranteed valid UTF-8 — pvxs stores
//! `std::string((char*)buf,len)` with no validation, `pvaproto.h:403`).
//!
//! Empty strings are wire-encoded as the single byte `0x00`.
//! The null marker (`0xFF`) is reserved for nullable strings; we surface it
//! as `Ok(None)` from the decoders.
//!
//! Two decode flavours over one byte primitive ([`decode_string_bytes`]):
//! - [`decode_string`] → [`String`], **lossy** (`from_utf8_lossy`). For
//!   internal ASCII-grammar labels (protocol/auth tags, field & struct
//!   names) that are stored as Rust `String`. Lossy — never rejecting — so a
//!   non-UTF-8 frame can no longer fault the whole decode (the PVA-89 root
//!   bug was `from_utf8` returning `Err` here, diverging from pvxs).
//! - [`decode_string_value`] → [`PvString`], **byte-preserving**. For wire
//!   *value* strings (`ScalarValue::String` / `TypedScalarArray::String`),
//!   so decode → store → re-encode is lossless even for non-UTF-8 (gateway
//!   pass-through parity with pvxs).

use std::io::Cursor;

use epics_base_rs::types::PvString;

use super::buffer::{ByteOrder, DecodeError, ReadExt};
use super::size::{decode_size, encode_size_into};

/// Encode a `str` and return a freshly allocated buffer.
pub fn encode_string(value: &str, order: ByteOrder) -> Vec<u8> {
    let mut out = Vec::new();
    encode_string_into(value, order, &mut out);
    out
}

/// Encode raw bytes (size prefix + verbatim payload) into an existing buffer.
/// The byte primitive both string encoders build on; the value path feeds it
/// [`PvString::as_bytes`] so non-UTF-8 content is preserved on the wire.
pub fn encode_string_bytes_into(bytes: &[u8], order: ByteOrder, out: &mut Vec<u8>) {
    encode_size_into(bytes.len() as u32, order, out);
    out.extend_from_slice(bytes);
}

/// Encode a `str` (label path) into an existing buffer.
pub fn encode_string_into(value: &str, order: ByteOrder, out: &mut Vec<u8>) {
    encode_string_bytes_into(value.as_bytes(), order, out);
}

/// Decode raw bytes. `Ok(None)` indicates the null marker (`0xFF` size byte).
/// No UTF-8 validation — bytes are returned verbatim.
pub fn decode_string_bytes(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<Option<Vec<u8>>, DecodeError> {
    let len = match decode_size(cur, order)? {
        Some(n) => n as usize,
        None => return Ok(None),
    };
    Ok(Some(cur.get_bytes(len)?))
}

/// Decode a label string as lossy UTF-8. `Ok(None)` is the null marker.
/// Non-UTF-8 bytes become `U+FFFD` rather than faulting the decode.
pub fn decode_string(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<Option<String>, DecodeError> {
    Ok(decode_string_bytes(cur, order)?.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// Decode a wire *value* string, preserving the raw bytes in a [`PvString`].
/// `Ok(None)` is the null marker.
pub fn decode_string_value(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<Option<PvString>, DecodeError> {
    Ok(decode_string_bytes(cur, order)?.map(PvString::from_bytes))
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

    #[test]
    fn value_path_round_trips_non_utf8_losslessly() {
        // pvxs stores raw bytes off the wire (pvaproto.h:403); the value path
        // must preserve them rather than reject (the PVA-89 root bug).
        let raw = vec![0xff, 0x00, 0x80, b'a', 0xfe];
        let mut out = Vec::new();
        encode_string_bytes_into(&raw, ByteOrder::Little, &mut out);
        let mut cur = Cursor::new(out.as_slice());
        let decoded = decode_string_value(&mut cur, ByteOrder::Little)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.as_bytes(), raw.as_slice());
    }

    #[test]
    fn label_path_is_lossy_not_rejecting() {
        // A non-UTF-8 label decodes lossily (U+FFFD) and must NOT error —
        // before the fix `from_utf8` returned Err and faulted the frame.
        let mut out = Vec::new();
        encode_string_bytes_into(&[0xff, 0xfe], ByteOrder::Little, &mut out);
        let mut cur = Cursor::new(out.as_slice());
        let label = decode_string(&mut cur, ByteOrder::Little).unwrap().unwrap();
        assert!(label.contains('\u{FFFD}'));
    }
}
