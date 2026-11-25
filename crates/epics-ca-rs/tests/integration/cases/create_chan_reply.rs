//! `CA_PROTO_CREATE_CHAN` reply (cmd 18) — server → client.
//!
//! C reference: `rsrv/caservertask.c::claim_ciu_reply` builds a
//! 16-byte header with `m_cmmd=18`, `m_postsize=0`,
//! `m_dataType = native DBR type`, `m_count = element count` (capped
//! at 0xFFFF in non-extended form), `m_cid = client cid`,
//! `m_available = server SID`. Mirrors libca's
//! `cac::createChanRespAction` parse order (cid first → SID → type
//! → count).
//!
//! Failure path: same opcode but `m_dataType = m_count = 0` and the
//! SID slot carries an ECA status. The audit cited this
//! shape; the wire byte order is the part golden tests pin.

use epics_ca_rs::protocol::{CaHeader, ECA_ALLOCMEM};

const CA_PROTO_CREATE_CHAN: u16 = 18;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn assert_hex(actual: &[u8], expected_hex: &str, label: &str) {
    let got = hex(actual);
    let want = expected_hex.replace(' ', "");
    assert_eq!(got, want, "{label}:\n  got:  {got}\n  want: {want}");
}

#[test]
fn golden_ext_create_chan_reply_scalar_double() {
    // DBR_DOUBLE = 6, count = 1.
    let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
    h.data_type = 6;
    h.count = 1;
    h.cid = 0x0000_002A; // client cid
    h.available = 0x0000_0042; // server SID
    assert_hex(
        &h.to_bytes(),
        "0012 0000 0006 0001 0000002a 00000042",
        "CREATE_CHAN reply scalar double",
    );
}

#[test]
fn golden_ext_create_chan_reply_array_short() {
    // DBR_SHORT = 1, count = 256.
    let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
    h.data_type = 1;
    h.count = 256;
    h.cid = 0x0000_002B;
    h.available = 0x0000_0043;
    assert_hex(
        &h.to_bytes(),
        "0012 0000 0001 0100 0000002b 00000043",
        "CREATE_CHAN reply array short",
    );
}

#[test]
fn golden_ext_create_chan_reply_failure_alloc() {
    // pvxs/rsrv failure path: dataType=count=0, available carries
    // the ECA code. The audit ensured channel-scoped errors use
    // the client cid in m_cid; this golden pins that arrangement.
    let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
    h.data_type = 0;
    h.count = 0;
    h.cid = 0x0000_002C; // echo of client cid
    h.available = ECA_ALLOCMEM;
    // ECA_ALLOCMEM = defmsg(CA_K_WARNING=0, 6) = (6<<3)|0 = 0x30.
    // pvxs/libca encode severity in the low 3 bits; here that's 0.
    assert_eq!(
        ECA_ALLOCMEM, 0x30,
        "ECA_ALLOCMEM constant should match defmsg(CA_K_WARNING, 6) = 0x30"
    );
    assert_hex(
        &h.to_bytes(),
        "0012 0000 0000 0000 0000002c 00000030",
        "CREATE_CHAN reply failure",
    );
}
