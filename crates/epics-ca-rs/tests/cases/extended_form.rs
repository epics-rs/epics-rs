//! Extended-form header (`postsize == 0xFFFF`, `count == 0`)
//! triggers a 24-byte header layout that carries the real
//! `postsize` (u32) and `count` (u32) in the 8-byte annex after the
//! base 16 bytes.
//!
//! C reference:
//! - `libca/comQueSend.cpp:285` (`insertRequestHeader`): switches to
//!   the extended form when `nElem >= 0xFFFF` *or* `pBSize >= 0xFFFF`.
//! - `rsrv/camessage.c:2520` validates the 8-byte alignment of
//!   `m_postsize` *after* the extended form is unfolded.
//!
//! Several findings cited shape variants of this
//! annex. Goldens here pin the exact 24-byte layout for a CLIENT_NAME
//! whose body exceeds 0xFFFF bytes (forcing extended form) and for an
//! oversized WRITE whose `count` does the same.

use epics_ca_rs::protocol::{CA_PROTO_CLIENT_NAME, CA_PROTO_WRITE, CaHeader};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[test]
fn golden_ext_client_name_extended_postsize() {
    // 65,536-byte CLIENT_NAME body → extended form. The base header
    // carries `postsize=0xFFFF` + `count=0`; the annex carries the
    // real postsize (u32) followed by count (u32 = 0).
    let mut h = CaHeader::new(CA_PROTO_CLIENT_NAME);
    h.set_payload_size(0, 0, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header"); // count=0 for CLIENT_NAME
    // Force the extended form by hand-setting actual_count to 0 and
    // postsize to the real size via set_payload_size.
    h.set_payload_size(65_536, 0, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    let bytes = h.to_bytes_extended();

    // Expect 24 bytes:
    //   00 14            cmmd = CA_PROTO_CLIENT_NAME (20)
    //   ff ff            postsize sentinel
    //   00 00            dataType
    //   00 00            count sentinel
    //   00 00 00 00      cid
    //   00 00 00 00      available
    //   00 01 00 00      real postsize = 65536
    //   00 00 00 00      real count = 0
    assert_eq!(bytes.len(), 24);
    assert_eq!(
        hex(&bytes),
        "0014ffff00000000\
         0000000000000000\
         0001000000000000",
        "CLIENT_NAME extended postsize"
    );
}

#[test]
fn golden_ext_write_extended_count() {
    // count > 0xFFFE forces extended-form. Verifies the count slot
    // moves to the annex while the postsize stays in the base
    // header's u16 slot when it fits.
    let mut h = CaHeader::new(CA_PROTO_WRITE);
    h.data_type = 6; // DBR_DOUBLE
    h.cid = 0x0000_002A;
    h.available = 0x0000_0042;
    // 200_000 elements × 8 bytes = 1_600_000 bytes. postsize > u16,
    // so both annex slots used.
    h.set_payload_size(1_600_000, 200_000, epics_ca_rs::protocol::CA_MINOR_VERSION)
        .expect("modern peer accepts the extended header");
    let bytes = h.to_bytes_extended();
    assert_eq!(bytes.len(), 24);
    // Bytes 0..16: base header with sentinels in postsize+count.
    // Bytes 16..20: real postsize 0x0018_6A00.
    // Bytes 20..24: real count   0x0003_0D40.
    assert_eq!(
        hex(&bytes),
        "0004ffff00060000\
         0000002a00000042\
         00186a00\
         00030d40",
        "WRITE extended count + postsize"
    );
}
