//! pvxs PVA message-header wire shape — `pvaproto.h:665
//! to_wire(Header&)`.
//!
//! Layout: `0xCA` magic, version, flags, cmd, u32 length (in the
//! byte order indicated by the MSB flag bit). Every PVA message
//! starts with one — a misplaced flag bit is the kind of single-
//! byte bug that loses a session before the first response.
//!
//! Expected bytes come from `tools/pvxs-golden-capture/fixtures.txt`
//! (`to_wire(buf, Header{cmd,flags,len})` at run time).

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::proto::header::{HeaderFlags, PvaHeader};

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// pvxs `pva_app_msg_t` / `pva_ctrl_msg` command bytes (literal
// values — the Rust crate doesn't re-export the C++ constants).
const CMD_CONNECTION_VALIDATION: u8 = 0x01;
const CMD_GET: u8 = 0x0A;
const CMD_MONITOR: u8 = 0x0D;
const CTRL_SET_MARKER: u8 = 0x00;

// pvxs `pva_flags::SegFirst` (0x10) — segmented-message first piece.
const PVA_FLAG_SEG_FIRST: u8 = 0x10;

#[test]
fn golden_pvxs_header_app_get_be() {
    let h = PvaHeader::application(false, ByteOrder::Big, CMD_GET, 42);
    assert_eq!(hex(&h.encode()), golden("header_app_get_be"));
}

#[test]
fn golden_pvxs_header_app_get_le() {
    let h = PvaHeader::application(false, ByteOrder::Little, CMD_GET, 42);
    assert_eq!(hex(&h.encode()), golden("header_app_get_le"));
}

#[test]
fn golden_pvxs_header_control_marker_be() {
    let h = PvaHeader::control(false, ByteOrder::Big, CTRL_SET_MARKER, 0);
    assert_eq!(hex(&h.encode()), golden("header_control_marker_be"));
}

#[test]
fn golden_pvxs_header_seg_first_be() {
    // PvaHeader factories don't expose the segmented bit; OR it
    // onto the flags byte after the standard server/control/order
    // bits land.
    let mut flags = HeaderFlags::new(false, false, ByteOrder::Big);
    flags.0 |= PVA_FLAG_SEG_FIRST;
    let h = PvaHeader {
        version: 2,
        flags,
        command: CMD_MONITOR,
        payload_length: 8,
    };
    assert_eq!(hex(&h.encode()), golden("header_seg_first_be"));
}

#[test]
fn golden_pvxs_header_server_app_be() {
    let h = PvaHeader::application(true, ByteOrder::Big, CMD_CONNECTION_VALIDATION, 0);
    assert_eq!(hex(&h.encode()), golden("header_server_app_be"));
}
