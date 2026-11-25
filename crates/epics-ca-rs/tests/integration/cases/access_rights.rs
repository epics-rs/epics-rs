//! `CA_PROTO_ACCESS_RIGHTS` (cmd 22) — server → client byte shape.
//!
//! C reference: `rsrv/caservertask.c::access_rights_reply` builds a
//! 16-byte header with `m_cmmd=22`, `m_postsize=0`, `m_dataType=0`,
//! `m_count=0`, `m_cid = <client cid>`, `m_available = <access mask>`.
//! libca dispatches on `m_cid` (channel CID, not SID) — the access
//! mask is the wire payload of `m_available`.
//!
//! Mask values (`rsrv/caservertask.c::CA_PROTO_ACCESS_RIGHTS` send
//! sites):
//!     0 — no access
//!     1 — read only
//!     2 — write only (unused in practice)
//!     3 — read + write

use epics_ca_rs::protocol::CaHeader;

const CA_PROTO_ACCESS_RIGHTS: u16 = 22;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn assert_hex(actual: &[u8], expected_hex: &str, label: &str) {
    let got = hex(actual);
    let want = expected_hex.replace(' ', "");
    assert_eq!(got, want, "{label}:\n  got:  {got}\n  want: {want}");
}

#[test]
fn golden_ext_access_rights_read_only() {
    // cmmd=22, no payload, mask = 1 (read).
    let mut h = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
    h.cid = 0xCAFE_BABE;
    h.available = 1;
    assert_hex(
        &h.to_bytes(),
        "0016 0000 0000 0000 cafebabe 00000001",
        "ACCESS_RIGHTS read-only",
    );
}

#[test]
fn golden_ext_access_rights_read_write() {
    let mut h = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
    h.cid = 0xDEAD_BEEF;
    h.available = 3;
    assert_hex(
        &h.to_bytes(),
        "0016 0000 0000 0000 deadbeef 00000003",
        "ACCESS_RIGHTS read+write",
    );
}

#[test]
fn golden_ext_access_rights_denied() {
    // mask=0 ⇒ no access. Wire shape is the same; `m_available=0`.
    let mut h = CaHeader::new(CA_PROTO_ACCESS_RIGHTS);
    h.cid = 0x1234_5678;
    h.available = 0;
    assert_hex(
        &h.to_bytes(),
        "0016 0000 0000 0000 12345678 00000000",
        "ACCESS_RIGHTS denied",
    );
}
