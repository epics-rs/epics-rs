//! One-allocation CA frame construction for DBR-payload replies.
//!
//! # The copy this removes
//!
//! Every server site that answers with a DBR value used to build the payload
//! and the frame separately:
//!
//! ```text
//! let payload = encode_dbr(type, snapshot)?;      // allocation 1
//! let hdr_bytes = hdr.to_bytes_extended();
//! let mut frame = Vec::with_capacity(hdr + payload.len());  // allocation 2
//! frame.extend_from_slice(&hdr_bytes);
//! frame.extend_from_slice(&payload);              // copy of the whole value
//! ```
//!
//! Both buffers are live across that last copy, so a 1 MiB waveform costs
//! 2 MiB at the moment of framing — per delivery, per subscriber. On
//! `x86_64-wrs-vxworks` four clients monitoring one 1 MiB waveform aborted the
//! RTP with `memory allocation of 1048576 bytes failed` and signal 6.
//!
//! # What C does
//!
//! C never has two buffers. `read_reply` calls `cas_copy_in_header`
//! (`rsrv/camessage.c:516`), which reserves the header *and* the payload
//! inside the client's already-allocated send buffer and returns `pPayload`
//! pointing just past the header; `dbChannel_get_count` → `dbGet` then converts
//! the record's live field straight into that space (`dbAccess.c:1020`). One
//! buffer, reused per client, no payload copy at all.
//!
//! # This module
//!
//! [`FrameBuf`] is the port of that reservation: one `Vec` that starts with
//! room for the largest CA header, into which `encode_dbr_into` writes the
//! payload directly. [`FrameBuf::seal`] then writes the finished header into
//! the reserved prefix and returns the complete frame — still the single
//! contiguous `Vec<u8>` the [`Outbox`](super::outbox::Outbox) abort-safety
//! invariant requires.

use crate::protocol::{CaHeader, align8};

/// Room reserved at the front of a [`FrameBuf`] — the largest CA header
/// (extended: 16 fixed bytes plus the 8 extended-postsize/count bytes).
const HDR_RESERVE: usize = 24;

/// A CA frame under construction: the header prefix is reserved, the payload
/// is appended after it.
pub(crate) struct FrameBuf {
    buf: Vec<u8>,
}

impl FrameBuf {
    /// Reserve header room plus `payload_hint` bytes for the payload.
    pub(crate) fn new(payload_hint: usize) -> Self {
        let mut buf = Vec::with_capacity(HDR_RESERVE + payload_hint);
        buf.resize(HDR_RESERVE, 0);
        Self { buf }
    }

    /// Append-only destination for the payload encoder (`encode_dbr_into`).
    ///
    /// The reserved header already occupies the front of this buffer, so
    /// appending lands in the payload region. Reshaping the payload goes
    /// through [`truncate_payload`](Self::truncate_payload) /
    /// [`zero_extend_payload`](Self::zero_extend_payload) /
    /// [`align_payload`](Self::align_payload), which measure from the payload
    /// start — never `truncate`/`resize` this buffer directly, since its
    /// indices include the reservation.
    pub(crate) fn dst(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    /// Bytes of payload written so far.
    pub(crate) fn payload_len(&self) -> usize {
        self.buf.len() - HDR_RESERVE
    }

    /// Read-only view of the payload written so far — for callers that must
    /// inspect the encoded bytes (C `read_action`'s `epicsStrnLen` scan of a
    /// scalar DBR_STRING slot, `camessage.c:666-680`).
    pub(crate) fn payload(&self) -> &[u8] {
        &self.buf[HDR_RESERVE..]
    }

    /// Truncate the payload to `len` bytes (C `read_reply` framing a reply at
    /// the request count when the value decoded wider).
    pub(crate) fn truncate_payload(&mut self, len: usize) {
        self.buf.truncate(HDR_RESERVE + len);
    }

    /// Zero-extend the payload to `len` bytes (C `read_reply` zero-filling up
    /// to `dbr_size_n(type, request_count)`).
    pub(crate) fn zero_extend_payload(&mut self, len: usize) {
        if self.payload_len() < len {
            self.buf.resize(HDR_RESERVE + len, 0);
        }
    }

    /// Zero-pad the payload to an 8-byte boundary — the C client's TCP parser
    /// requires an 8-aligned postsize.
    pub(crate) fn align_payload(&mut self) {
        let padded = align8(self.payload_len());
        self.zero_extend_payload(padded);
    }

    /// Write `hdr` into the reserved prefix and return the finished frame.
    ///
    /// A non-extended (16-byte) header leaves 8 reserved bytes unused, so the
    /// payload shifts down by 8. That shift is bounded by the protocol itself:
    /// `CaHeader::set_payload_size` selects the extended header for any
    /// payload `>= 0xFFFF`, so a frame that shifts always carries under 64 KiB
    /// — and the large-array frames this module exists for fill the
    /// reservation exactly and never move a byte.
    pub(crate) fn seal(mut self, hdr: &CaHeader) -> Vec<u8> {
        let hdr_bytes = hdr.to_bytes_extended();
        debug_assert!(hdr_bytes.len() <= HDR_RESERVE);
        let start = HDR_RESERVE - hdr_bytes.len();
        self.buf[start..HDR_RESERVE].copy_from_slice(&hdr_bytes);
        if start > 0 {
            self.buf.drain(..start);
        }
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CA_PROTO_EVENT_ADD;

    /// A payload small enough for the 16-byte header seals to
    /// `header || payload` with the unused 8 reserved bytes gone.
    #[test]
    fn short_payload_seals_without_the_extended_prefix() {
        let mut f = FrameBuf::new(8);
        f.dst().extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(f.payload_len(), 8);
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.set_payload_size(8, 1, 13).unwrap();
        let frame = f.seal(&hdr);
        assert_eq!(frame.len(), 16 + 8);
        assert_eq!(&frame[..16], &hdr.to_bytes()[..]);
        assert_eq!(&frame[16..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    /// A payload needing the extended header fills the reservation exactly, so
    /// `seal` writes the 24 bytes in place and moves no payload byte.
    #[test]
    fn extended_payload_seals_in_place() {
        let n = 0x1_0000; // >= 0xFFFF forces the extended header
        let mut f = FrameBuf::new(n);
        f.dst().extend(std::iter::repeat_n(0xABu8, n));
        assert_eq!(f.payload_len(), n);
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.set_payload_size(n, 1, 13).unwrap();
        let hdr_bytes = hdr.to_bytes_extended();
        assert_eq!(
            hdr_bytes.len(),
            24,
            "payload >= 0xFFFF needs 24 header bytes"
        );
        let frame = f.seal(&hdr);
        assert_eq!(frame.len(), 24 + n);
        assert_eq!(&frame[..24], &hdr_bytes[..]);
        assert!(frame[24..].iter().all(|b| *b == 0xAB));
    }

    /// `align_payload` pads to the 8-byte boundary the C client's TCP parser
    /// requires, and `truncate_payload` / `zero_extend_payload` measure from
    /// the payload start, not the buffer start.
    #[test]
    fn payload_ops_are_relative_to_the_payload_start() {
        let mut f = FrameBuf::new(0);
        f.dst().extend_from_slice(&[7; 5]);
        f.align_payload();
        assert_eq!(f.payload_len(), 8);
        f.zero_extend_payload(16);
        assert_eq!(f.payload_len(), 16);
        f.truncate_payload(3);
        assert_eq!(f.payload_len(), 3);
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.set_payload_size(3, 1, 13).unwrap();
        assert_eq!(f.seal(&hdr).len(), 16 + 3);
    }
}
