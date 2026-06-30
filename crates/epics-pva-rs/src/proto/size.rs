//! PVA variable-length size encoding.
//!
//! See pvxs `pvaproto.h` `to_wire(Size)` / `from_wire(Size)`:
//!
//! - 0..=253       → single byte
//! - 254..=u32::MAX → 0xFE prefix + u32
//! - null marker   → 0xFF (used by nullable strings / unselected variant)
//!
//! For lengths 254..=2³¹-1 pvxs encodes as `0xFE` + 4-byte native u32; the
//! wire format is byte-exact when both peers use the same [`ByteOrder`].

use std::io::Cursor;

use super::buffer::{ByteOrder, DecodeError, ReadExt, WriteExt};

/// Wire byte that signals "null / undefined size" — used by nullable strings.
pub const NULL_MARKER: u8 = 0xFF;

/// Wire byte that signals "extended (32-bit) size follows".
pub const EXTENDED_MARKER: u8 = 0xFE;

/// Encode a non-null size. Returns a freshly allocated `Vec`.
pub fn encode_size(value: u32, order: ByteOrder) -> Vec<u8> {
    let mut out = Vec::new();
    encode_size_into(value, order, &mut out);
    out
}

/// Encode a non-null size into an existing buffer.
pub fn encode_size_into(value: u32, order: ByteOrder, out: &mut Vec<u8>) {
    if value < 254 {
        out.push(value as u8);
    } else {
        out.push(EXTENDED_MARKER);
        out.put_u32(value, order);
    }
}

/// Decode the next size from `cur`.
///
/// Returns `Ok(None)` for the explicit null marker (`0xFF`), `Ok(Some(n))`
/// otherwise.
pub fn decode_size(cur: &mut Cursor<&[u8]>, order: ByteOrder) -> Result<Option<u32>, DecodeError> {
    let b = cur.get_u8()?;
    match b {
        NULL_MARKER => Ok(None),
        EXTENDED_MARKER => {
            let v = cur.get_u32(order)?;
            Ok(Some(v))
        }
        other => Ok(Some(other as u32)),
    }
}

/// Decode a size that must NOT be the null marker.
///
/// This is pvxs's *default* size decode — `from_wire(buf, Size, /*allow_null*/
/// false)` (`pvaproto.h:299-304`), used for every count / length / required
/// size on the wire. It faults on `0xFF` instead of returning `Ok(None)`, so
/// the non-null invariant holds by construction: a caller that needs a
/// concrete count cannot silently accept null (e.g. `.unwrap_or(0)`). Only the
/// genuinely nullable fields — nullable strings, unselected union selectors —
/// stay on [`decode_size`] and opt into `None`. `what` names the field for the
/// fault message.
pub fn decode_size_nonnull(
    cur: &mut Cursor<&[u8]>,
    order: ByteOrder,
    what: &str,
) -> Result<u32, DecodeError> {
    decode_size(cur, order)?
        .ok_or_else(|| crate::decode_err!("{what} cannot be null (0xFF size marker)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(value: u32, order: ByteOrder) {
        let buf = encode_size(value, order);
        let mut cur = Cursor::new(buf.as_slice());
        assert_eq!(decode_size(&mut cur, order).unwrap(), Some(value));
        // All bytes consumed
        assert_eq!(cur.remaining(), 0);
    }

    #[test]
    fn small_sizes_single_byte() {
        for v in [0u32, 1, 127, 253] {
            for order in [ByteOrder::Little, ByteOrder::Big] {
                let buf = encode_size(v, order);
                assert_eq!(buf.len(), 1, "value={v} should be single-byte");
                assert_eq!(buf[0], v as u8);
                roundtrip(v, order);
            }
        }
    }

    #[test]
    fn extended_size_le() {
        let buf = encode_size(254, ByteOrder::Little);
        assert_eq!(buf, vec![0xFE, 0xFE, 0x00, 0x00, 0x00]);
        let buf = encode_size(0x1_0000, ByteOrder::Little);
        assert_eq!(buf, vec![0xFE, 0x00, 0x00, 0x01, 0x00]);
        roundtrip(0x1_0000, ByteOrder::Little);
    }

    #[test]
    fn extended_size_be() {
        let buf = encode_size(0x1_0000, ByteOrder::Big);
        assert_eq!(buf, vec![0xFE, 0x00, 0x01, 0x00, 0x00]);
        roundtrip(0x1_0000, ByteOrder::Big);
    }

    #[test]
    fn null_marker_decodes_to_none() {
        let buf = vec![NULL_MARKER];
        let mut cur = Cursor::new(buf.as_slice());
        assert_eq!(decode_size(&mut cur, ByteOrder::Little).unwrap(), None);
    }

    /// PVX-2: the non-null decode faults on `0xFF` (pvxs `allow_null=false`)
    /// instead of returning `None`, and passes a concrete value through.
    #[test]
    fn decode_size_nonnull_faults_on_null_marker() {
        let mut cur = Cursor::new([NULL_MARKER].as_slice());
        let err = decode_size_nonnull(&mut cur, ByteOrder::Little, "field count")
            .expect_err("0xFF null marker must fault, not pass through as a value");
        assert!(
            err.0.contains("field count"),
            "fault message must name the field: {err}"
        );

        let buf = encode_size(7, ByteOrder::Little);
        let mut cur = Cursor::new(buf.as_slice());
        assert_eq!(
            decode_size_nonnull(&mut cur, ByteOrder::Little, "field count").unwrap(),
            7
        );
    }

    #[test]
    fn matches_reference_encoding() {
        // Cross-check exact byte sequences against the pvAccess size encoding.
        // Per the pvxs `pvaproto.h` wire spec: 0 → [0x00], 253 → [0xFD],
        // 254 → [0xFE,0xFE,0x00,0x00,0x00] (LE) / [0xFE,0x00,0x00,0x00,0xFE] (BE).
        assert_eq!(encode_size(0, ByteOrder::Little), vec![0x00]);
        assert_eq!(encode_size(253, ByteOrder::Little), vec![0xFD]);
        assert_eq!(
            encode_size(254, ByteOrder::Little),
            vec![0xFE, 0xFE, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            encode_size(254, ByteOrder::Big),
            vec![0xFE, 0x00, 0x00, 0x00, 0xFE]
        );
    }
}
