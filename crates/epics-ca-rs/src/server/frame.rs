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
//! the reserved prefix and returns the complete frame — still a single
//! contiguous buffer, as the [`Outbox`](super::outbox::Outbox) abort-safety
//! invariant requires.
//!
//! # Reuse across deliveries
//!
//! Reserving in one buffer removes the payload copy, but a `FrameBuf` per
//! delivery still *allocates* per delivery, where C's send buffer is allocated
//! once per client and reused for every reply and every monitor update it ever
//! sends. [`FramePool`] closes that: it is the connection's one send buffer,
//! [`FrameBuf::acquire`] borrows it, and [`PooledFrame`]'s `Drop` — running in
//! the single drain owner once the bytes are on the socket — returns it. A
//! producer therefore allocates only when the buffer is already out on loan.
//!
//! One buffer and one lock per connection is C's shape, not an addition to it:
//! `rsrv` guards its per-client send buffer with that client's `SEND_LOCK`.
//! The lock here is only ever `try_lock`ed, so a producer that loses the race
//! allocates a throwaway rather than waiting on another thread — the buffer is
//! an optimisation and must never be able to serialise two producers, let
//! alone invert their priorities.

use crate::protocol::{CaHeader, align8};
use std::sync::{Arc, Mutex};

/// Room reserved at the front of a [`FrameBuf`] — the largest CA header
/// (extended: 16 fixed bytes plus the 8 extended-postsize/count bytes).
const HDR_RESERVE: usize = 24;

/// One connection's reusable send buffer — the owner of the allocation that
/// [`FrameBuf`] borrows and [`PooledFrame`] returns.
///
/// Exactly one buffer, like C's per-client send buffer: the slot keeps the
/// larger of the returned and the resident capacity, so it settles at the
/// connection's high-water frame size and stays there. Bounding it at one is
/// what keeps a burst of concurrent producers from parking N high-water buffers
/// on a connection that only needs one at a time; producers past the first
/// allocate for that delivery and drop it, which is exactly today's behaviour.
pub(crate) struct FramePool {
    slot: Mutex<Option<Vec<u8>>>,
}

impl FramePool {
    pub(crate) fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }

    /// Take the buffer if it is resident and the slot is uncontended.
    fn take(&self) -> Option<Vec<u8>> {
        self.slot.try_lock().ok().and_then(|mut slot| slot.take())
    }

    /// Give a buffer back. Keeps the larger capacity when the slot is already
    /// occupied, and drops the buffer outright when the slot is contended —
    /// a missed return costs one allocation, never correctness.
    fn put(&self, mut buf: Vec<u8>) {
        buf.clear();
        if let Ok(mut slot) = self.slot.try_lock() {
            let keep = match slot.take() {
                Some(resident) if resident.capacity() >= buf.capacity() => resident,
                _ => buf,
            };
            *slot = Some(keep);
        }
    }
}

/// A finished CA frame on its way to the socket, which returns its allocation
/// to the [`FramePool`] it came from when the drain owner drops it.
///
/// The lend/return is the type's, not the caller's: there is no way to obtain
/// one of these without either borrowing from a pool
/// ([`FrameBuf::acquire`] → [`FrameBuf::seal`]) or supplying an owned `Vec`
/// (`From<Vec<u8>>`, for the fixed-size control frames that have nothing worth
/// reusing), and no way to keep the buffer past the drop.
pub(crate) struct PooledFrame {
    buf: Vec<u8>,
    home: Option<Arc<FramePool>>,
}

impl std::fmt::Debug for PooledFrame {
    /// Length and provenance, never the bytes: a frame can carry a megabyte of
    /// waveform and an assertion message that dumps it is unreadable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledFrame")
            .field("len", &self.buf.len())
            .field("pooled", &self.home.is_some())
            .finish()
    }
}

impl std::ops::Deref for PooledFrame {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf
    }
}

impl From<Vec<u8>> for PooledFrame {
    /// A frame built outside the pool: the CA control messages, whose bodies
    /// are a header or a fixed handful of bytes. Returning those to the slot
    /// would evict the high-water payload buffer the pool exists to keep, so
    /// they carry no home and free normally.
    fn from(buf: Vec<u8>) -> Self {
        Self { buf, home: None }
    }
}

impl Drop for PooledFrame {
    fn drop(&mut self) {
        if let Some(home) = self.home.take() {
            home.put(std::mem::take(&mut self.buf));
        }
    }
}

/// A CA frame under construction: the header prefix is reserved, the payload
/// is appended after it.
pub(crate) struct FrameBuf {
    buf: Vec<u8>,
    home: Option<Arc<FramePool>>,
}

impl FrameBuf {
    /// Borrow the connection's send buffer, reserving header room plus
    /// `payload_hint` bytes of payload. Allocates only when the buffer is out
    /// on loan to another producer.
    pub(crate) fn acquire(pool: &Arc<FramePool>, payload_hint: usize) -> Self {
        let mut buf = pool
            .take()
            .unwrap_or_else(|| Vec::with_capacity(HDR_RESERVE + payload_hint));
        buf.clear();
        buf.resize(HDR_RESERVE, 0);
        Self {
            buf,
            home: Some(pool.clone()),
        }
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
    pub(crate) fn seal(mut self, hdr: &CaHeader) -> PooledFrame {
        let hdr_bytes = hdr.to_bytes_extended();
        debug_assert!(hdr_bytes.len() <= HDR_RESERVE);
        let start = HDR_RESERVE - hdr_bytes.len();
        self.buf[start..HDR_RESERVE].copy_from_slice(&hdr_bytes);
        if start > 0 {
            self.buf.drain(..start);
        }
        PooledFrame {
            buf: self.buf,
            home: self.home,
        }
    }
}

/// Size a DBR reply's payload to the requested element count and return the
/// element count its header must carry.
///
/// This is the single implementation of C `read_reply`'s reply sizing
/// (`rsrv/camessage.c:507-571`), which
///
/// * treats `mp->m_count == 0` as "all available elements" (autosize) and
///   frames the reply at the live count;
/// * otherwise writes the ORIGINAL request count into the header and sizes the
///   payload to `dbr_size_n(type, request_count)` — zero-filling when the
///   request asks for more elements than the value holds, and framing at the
///   request count when the value decoded wider.
///
/// `DBR_CLASS_NAME` is the one type outside that rule: its wire payload is a
/// single fixed 40-byte string whatever the record's element count is, and it
/// is never padded or truncated to a request count (CA-268 — a waveform's
/// `snapshot.value.count()` of N would make C clients parse `40 * N` body
/// bytes and fail).
///
/// Every CA reply that carries an encoded DBR payload sizes it here: the
/// steady-state monitor producer, the initial / access-restore monitor
/// snapshot, `EVENT_ADD` delivery in `monitor::send_event`, and the
/// `READ` / `READ_NOTIFY` reply. The one caller with a rule of its own is a
/// *deprecated* `CA_PROTO_READ` with `count == 0`, which C `read_action`
/// (unlike `read_reply`) frames at count 0 with a value-less body; that
/// caller applies its own case first and delegates every other count here.
pub(crate) fn size_dbr_reply(
    frame: &mut FrameBuf,
    data_type: u16,
    actual_count: u32,
    requested_count: u32,
) -> u32 {
    if data_type == epics_base_rs::types::DBR_CLASS_NAME {
        return 1;
    }
    if requested_count == 0 {
        return actual_count;
    }
    if let Ok(native) = epics_base_rs::types::native_type_for_dbr(data_type) {
        // Plain types (0-6) have no metadata; STS / TIME / GR / CTRL slot
        // metadata before the value array. `dbr_buffer_size(_, _, 0)` returns
        // just the metadata size, so this is `dbr_size_n(type, count)`.
        let meta_size = epics_base_rs::types::dbr_buffer_size(data_type, native, 0);
        let target_size = meta_size + (requested_count as usize) * native.element_size();
        if requested_count > actual_count {
            frame.zero_extend_payload(target_size);
        } else if requested_count < actual_count && frame.payload_len() > target_size {
            frame.truncate_payload(target_size);
        }
    }
    requested_count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CA_PROTO_EVENT_ADD;

    /// A pool of its own per test: the buffer is per connection, so a test
    /// standing in for one connection owns one.
    fn pool() -> Arc<FramePool> {
        Arc::new(FramePool::new())
    }

    /// Where the allocation lives, so a reuse claim is checked against the
    /// buffer's identity and not merely its capacity.
    fn resident(pool: &Arc<FramePool>) -> Option<(usize, usize)> {
        let slot = pool.slot.lock().unwrap();
        slot.as_ref().map(|b| (b.as_ptr() as usize, b.capacity()))
    }

    /// One delivery, then the next: the second borrows the allocation the first
    /// returned. This is the property the whole module exists for — C's send
    /// buffer is allocated once per client, not once per update.
    #[test]
    fn a_sealed_frame_returns_its_buffer_for_the_next_delivery() {
        let pool = pool();
        assert_eq!(resident(&pool), None, "a fresh pool holds nothing");

        let mut first = FrameBuf::acquire(&pool, 4096);
        first.dst().extend(std::iter::repeat_n(0xAAu8, 4096));
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.set_payload_size(4096, 1, 13).unwrap();
        let sealed = first.seal(&hdr);
        // Out on loan: the frame holds the buffer until the drain owner writes
        // it, so the slot is empty for as long as the bytes are in flight.
        assert_eq!(resident(&pool), None);
        assert_eq!(sealed.len(), 16 + 4096);

        drop(sealed); // the drain owner, once the bytes are on the socket
        let back = resident(&pool).expect("drop returns the buffer");
        assert!(
            back.1 >= 16 + 4096,
            "returned buffer kept its capacity: {back:?}"
        );

        let second = FrameBuf::acquire(&pool, 0);
        assert_eq!(resident(&pool), None, "the second delivery took it");
        // `seal` may shift the payload down by 8 when the header is not
        // extended, so compare against the buffer's own start, not the sealed
        // frame's: the allocation is what must be identical.
        assert_eq!(
            second.buf.as_ptr() as usize,
            back.0,
            "the second delivery must reuse the first's allocation, not allocate"
        );
    }

    /// A control frame handed to the outbox as a plain `Vec` has no home, so it
    /// cannot evict the payload buffer the pool is holding. A 16-byte header
    /// displacing a 1 MiB waveform buffer would make the pool worse than no
    /// pool at all.
    #[test]
    fn an_unpooled_control_frame_never_displaces_the_payload_buffer() {
        let pool = pool();
        let mut f = FrameBuf::acquire(&pool, 65536);
        f.dst().extend(std::iter::repeat_n(0x5Au8, 65536));
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.set_payload_size(65536, 1, 13).unwrap();
        drop(f.seal(&hdr));
        let big = resident(&pool).expect("payload buffer resident");
        assert!(big.1 >= 65536);

        drop(PooledFrame::from(vec![0u8; 16]));
        assert_eq!(
            resident(&pool),
            Some(big),
            "an unpooled frame must leave the slot untouched"
        );
    }

    /// Two producers at once — the async driver runs one monitor task per
    /// subscription — and only one buffer. The second must allocate rather than
    /// wait, and the slot must not end up holding the smaller of the two.
    #[test]
    fn a_second_concurrent_producer_allocates_and_the_slot_keeps_the_larger() {
        let pool = pool();
        let mut small = FrameBuf::acquire(&pool, 64);
        small.dst().extend(std::iter::repeat_n(1u8, 64));
        // While `small` holds the (only) buffer, a second producer gets its own.
        let mut large = FrameBuf::acquire(&pool, 65536);
        large.dst().extend(std::iter::repeat_n(2u8, 65536));
        assert_ne!(
            small.buf.as_ptr() as usize,
            large.buf.as_ptr() as usize,
            "concurrent producers must not share one buffer"
        );

        let mut h_small = CaHeader::new(CA_PROTO_EVENT_ADD);
        h_small.set_payload_size(64, 1, 13).unwrap();
        let mut h_large = CaHeader::new(CA_PROTO_EVENT_ADD);
        h_large.set_payload_size(65536, 1, 13).unwrap();
        drop(large.seal(&h_large));
        drop(small.seal(&h_small));

        let kept = resident(&pool).expect("one of them is resident");
        assert!(
            kept.1 >= 65536,
            "the slot kept the smaller buffer: {kept:?}"
        );
    }

    /// A payload small enough for the 16-byte header seals to
    /// `header || payload` with the unused 8 reserved bytes gone.
    #[test]
    fn short_payload_seals_without_the_extended_prefix() {
        let mut f = FrameBuf::acquire(&pool(), 8);
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
        let mut f = FrameBuf::acquire(&pool(), n);
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

    /// Encode `value` as `dbr_type` straight into a fresh `FrameBuf` — the
    /// native-type path where `encode_dbr_into` appends into the reserved
    /// buffer with no intermediate `Vec`.
    fn encoded(dbr_type: u16, value: epics_base_rs::types::EpicsValue) -> (FrameBuf, u32) {
        let count = value.count() as u32;
        let snapshot = epics_base_rs::server::snapshot::Snapshot::new(
            value,
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        );
        let mut frame = FrameBuf::acquire(&pool(), 0);
        epics_base_rs::types::encode_dbr_into(frame.dst(), dbr_type, &snapshot)
            .expect("native-type encode must succeed");
        (frame, count)
    }

    /// The four `requested_count` boundaries against a native 4-element
    /// `DBR_LONG` array, on the zero-copy encode path: below, equal, above,
    /// and 0 (autosize). A plain DBR type has no metadata, so the payload is
    /// exactly `count * 4` and the reshaping is directly checkable.
    #[test]
    fn size_dbr_reply_covers_every_requested_count_boundary() {
        use epics_base_rs::types::{DBR_LONG, EpicsValue};

        let elems = || EpicsValue::LongArray(vec![1, 2, 3, 4]);

        // requested < native: framed at the request count, and the surviving
        // bytes are the leading elements (nothing shifted).
        let (mut frame, actual) = encoded(DBR_LONG, elems());
        assert_eq!((actual, frame.payload_len()), (4, 16));
        assert_eq!(size_dbr_reply(&mut frame, DBR_LONG, actual, 2), 2);
        assert_eq!(frame.payload_len(), 8);
        assert_eq!(frame.payload(), &[0, 0, 0, 1, 0, 0, 0, 2]);

        // requested == native: neither branch fires, payload untouched.
        let (mut frame, actual) = encoded(DBR_LONG, elems());
        assert_eq!(size_dbr_reply(&mut frame, DBR_LONG, actual, 4), 4);
        assert_eq!(frame.payload_len(), 16);
        assert_eq!(
            frame.payload(),
            &[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4]
        );

        // requested > native: zero-filled up to `dbr_size_n(type, request)`.
        let (mut frame, actual) = encoded(DBR_LONG, elems());
        assert_eq!(size_dbr_reply(&mut frame, DBR_LONG, actual, 6), 6);
        assert_eq!(frame.payload_len(), 24);
        assert_eq!(&frame.payload()[16..], &[0; 8]);

        // requested == 0: autosize — header carries the live count and the
        // payload is left at its encoded shape.
        let (mut frame, actual) = encoded(DBR_LONG, elems());
        assert_eq!(size_dbr_reply(&mut frame, DBR_LONG, actual, 0), 4);
        assert_eq!(frame.payload_len(), 16);
    }

    /// A compound DBR type's metadata sits before the value array, so the
    /// target size must be `meta + count * element_size`. Ignoring the
    /// metadata would truncate a request count below the native count *into*
    /// the STS/TIME header.
    #[test]
    fn size_dbr_reply_counts_the_metadata_before_the_value_array() {
        use epics_base_rs::types::{DBR_TIME_LONG, EpicsValue};

        let native = epics_base_rs::types::native_type_for_dbr(DBR_TIME_LONG).unwrap();
        let meta = epics_base_rs::types::dbr_buffer_size(DBR_TIME_LONG, native, 0);
        assert!(meta > 0, "DBR_TIME_LONG must carry metadata");

        let (mut frame, actual) = encoded(DBR_TIME_LONG, EpicsValue::LongArray(vec![1, 2, 3, 4]));
        assert_eq!(frame.payload_len(), meta + 16);
        assert_eq!(size_dbr_reply(&mut frame, DBR_TIME_LONG, actual, 1), 1);
        assert_eq!(frame.payload_len(), meta + 4);

        let (mut frame, actual) = encoded(DBR_TIME_LONG, EpicsValue::LongArray(vec![1, 2, 3, 4]));
        assert_eq!(size_dbr_reply(&mut frame, DBR_TIME_LONG, actual, 5), 5);
        assert_eq!(frame.payload_len(), meta + 20);
    }

    /// CA-268: `DBR_CLASS_NAME` reports count 1 and its payload is never
    /// reshaped — for any request count, including autosize.
    #[test]
    fn size_dbr_reply_leaves_class_name_at_one_element() {
        use epics_base_rs::types::DBR_CLASS_NAME;

        for requested in [0u32, 1, 4, 9] {
            let mut frame = FrameBuf::acquire(&pool(), 0);
            frame.dst().extend_from_slice(&[0xCD; 40]);
            assert_eq!(size_dbr_reply(&mut frame, DBR_CLASS_NAME, 7, requested), 1);
            assert_eq!(
                frame.payload_len(),
                40,
                "CLASS_NAME payload reshaped at requested={requested}",
            );
        }
    }

    /// `align_payload` pads to the 8-byte boundary the C client's TCP parser
    /// requires, and `truncate_payload` / `zero_extend_payload` measure from
    /// the payload start, not the buffer start.
    #[test]
    fn payload_ops_are_relative_to_the_payload_start() {
        let mut f = FrameBuf::acquire(&pool(), 0);
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
