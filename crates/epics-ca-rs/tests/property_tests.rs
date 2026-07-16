//! Property-based tests over CA wire-protocol invariants.
//!
//! These complement the example-based tests in `protocol_tests.rs` by
//! exploring tens of thousands of random inputs per property. Failures
//! shrink to a minimal counterexample, which is captured in
//! `proptest-regressions/` and replayed on every subsequent run.

use epics_ca_rs::protocol::*;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Header roundtrip
// ---------------------------------------------------------------------------

proptest! {
    /// Standard 16-byte headers must roundtrip byte-for-byte.
    ///
    /// We restrict to non-extended cases (postsize != 0xFFFF) since the
    /// extended form is exercised in a separate property below.
    #[test]
    fn header_roundtrip_standard(
        cmmd in 0u16..=64,
        postsize in 0u16..0xFFFE,
        data_type in any::<u16>(),
        count in any::<u16>(),
        cid in any::<u32>(),
        available in any::<u32>(),
    ) {
        let mut hdr = CaHeader::new(cmmd);
        hdr.postsize = postsize;
        hdr.data_type = data_type;
        hdr.count = count;
        hdr.cid = cid;
        hdr.available = available;
        let bytes = hdr.to_bytes();
        let parsed = CaHeader::from_bytes(&bytes).unwrap();
        prop_assert_eq!(hdr.cmmd, parsed.cmmd);
        prop_assert_eq!(hdr.postsize, parsed.postsize);
        prop_assert_eq!(hdr.data_type, parsed.data_type);
        prop_assert_eq!(hdr.count, parsed.count);
        prop_assert_eq!(hdr.cid, parsed.cid);
        prop_assert_eq!(hdr.available, parsed.available);
    }

    /// Extended headers (postsize=0xFFFF, count=0) must roundtrip plus
    /// preserve the u32 extended fields.
    #[test]
    fn header_roundtrip_extended(
        cmmd in 0u16..=64,
        ext_size in 0u32..=(MAX_FRAME_BODY_BYTES as u32),
        ext_count in any::<u32>(),
        cid in any::<u32>(),
        available in any::<u32>(),
    ) {
        let mut hdr = CaHeader::new(cmmd);
        hdr.cid = cid;
        hdr.available = available;
        hdr.set_payload_size(ext_size as usize, ext_count, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
        let bytes = hdr.to_bytes_extended();
        // Either it was forced extended, or set_payload_size kept it normal —
        // both branches must roundtrip.
        let (parsed, _) = CaHeader::from_bytes_extended(&bytes).unwrap();
        prop_assert_eq!(hdr.actual_postsize(), parsed.actual_postsize());
        prop_assert_eq!(hdr.actual_count(), parsed.actual_count());
        prop_assert_eq!(hdr.cmmd, parsed.cmmd);
        prop_assert_eq!(hdr.cid, parsed.cid);
        prop_assert_eq!(hdr.available, parsed.available);
    }

    /// `set_payload_size` chooses extended form iff size or count
    /// exceed the u16 range.
    #[test]
    fn extended_iff_overflow(size in 0usize..=(MAX_FRAME_BODY_BYTES), count in any::<u32>()) {
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.set_payload_size(size, count, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
        let needs_extended = size > 0xFFFE || count > 0xFFFF;
        prop_assert_eq!(hdr.is_extended(), needs_extended);
        prop_assert_eq!(hdr.actual_postsize(), size);
        prop_assert_eq!(hdr.actual_count(), count);
    }
}

// ---------------------------------------------------------------------------
// Parser robustness
// ---------------------------------------------------------------------------

proptest! {
    /// `from_bytes` must never panic on arbitrary input. It's allowed
    /// to return Err; we just don't want a crash.
    #[test]
    fn from_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = CaHeader::from_bytes(&bytes);
    }

    /// `from_bytes_extended` must never panic on arbitrary input, even
    /// when the buffer is too short for the extended form it claims.
    #[test]
    fn from_bytes_extended_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let _ = CaHeader::from_bytes_extended(&bytes);
    }

    /// Pathologically large `extended_postsize` values must PARSE — they are
    /// syntactically valid headers, and libca's receiver accepts them
    /// (R6-21). What must never happen is a panic or an allocation keyed on
    /// the claimed size: we pass a bare 24-byte buffer, so the parser sees
    /// the claimed size without any actual payload and must not touch it.
    #[test]
    fn extended_payload_past_max_frame_body_bytes_parses(
        ext_size in (MAX_FRAME_BODY_BYTES as u64 + 1)..=u32::MAX as u64,
        ext_count in any::<u32>(),
    ) {
        let mut buf = [0u8; 24];
        // postsize = 0xFFFF, count = 0 → extended form
        buf[2..4].copy_from_slice(&0xFFFFu16.to_be_bytes());
        buf[6..8].copy_from_slice(&0u16.to_be_bytes());
        buf[16..20].copy_from_slice(&(ext_size as u32).to_be_bytes());
        buf[20..24].copy_from_slice(&ext_count.to_be_bytes());
        // R6-21: the codec carries no receive policy. libca parses this
        // header and grows its body cache to fit (`tcpiiu.cpp:1214-1220`);
        // rejecting it here is what closed the circuit on a large waveform.
        let (hdr, consumed) = CaHeader::from_bytes_extended(&buf).unwrap();
        prop_assert_eq!(consumed, 24);
        prop_assert_eq!(hdr.actual_postsize(), ext_size as usize);
    }
}

// ---------------------------------------------------------------------------
// align8 invariants
// ---------------------------------------------------------------------------

proptest! {
    /// `align8` must:
    /// - return a multiple of 8
    /// - be >= the input
    /// - be < input + 8
    /// - be idempotent (align8(align8(x)) == align8(x))
    #[test]
    fn align8_invariants(n in 0usize..=(MAX_FRAME_BODY_BYTES)) {
        let aligned = align8(n);
        prop_assert_eq!(aligned % 8, 0);
        prop_assert!(aligned >= n);
        prop_assert!(aligned < n + 8);
        prop_assert_eq!(align8(aligned), aligned);
    }

    /// `pad_string` produces output whose length is 8-aligned, ends
    /// with at least one null byte, and starts with the original
    /// string bytes.
    #[test]
    fn pad_string_invariants(s in "[\\x20-\\x7e]{0,200}") {
        let padded = pad_string(&s);
        prop_assert_eq!(padded.len() % 8, 0);
        prop_assert!(padded.len() > s.len()); // at least one terminator
        prop_assert_eq!(&padded[..s.len()], s.as_bytes());
        prop_assert_eq!(padded[s.len()], 0u8);
    }
}

// ---------------------------------------------------------------------------
// ECA encoding invariants
// ---------------------------------------------------------------------------

proptest! {
    /// `defmsg(severity, msg_no)` then `eca_severity` / `eca_msg_no`
    /// must roundtrip for any valid (sev <= 7, msg_no <= 0x1FFF) pair.
    #[test]
    fn eca_encoding_roundtrip(
        sev in 0u32..=7,
        msg_no in 0u32..=0x1FFF,
    ) {
        let code = defmsg(sev, msg_no);
        prop_assert_eq!(eca_severity(code), sev);
        prop_assert_eq!(eca_msg_no(code), msg_no);
    }

    /// `eca_message` never panics — for unknown codes it returns the
    /// "Unknown ECA status" placeholder, which is fine.
    #[test]
    fn eca_message_total(code in any::<u32>()) {
        let _ = eca_message(code);
    }
}
