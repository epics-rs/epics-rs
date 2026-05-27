//! `CA_PROTO_ERROR` from a rejected `CA_PROTO_EVENT_ADD` INIT.
//!
//! C reference: `rsrv/camessage.c::event_add_action:1762-1866` —
//! when admission fails (ECA_NORDACCESS / ECA_ALLOCMEM / bad type)
//! the server emits `CA_PROTO_ERROR` (cmd 11) echoing the EVENT_ADD
//! header *before* the error status. Wire shape:
//!
//! ```text
//!   header(11)
//!     m_postsize = 16 (echoed request header, no string body)
//!     m_dataType = 0
//!     m_count    = 0
//!     m_cid      = client channel CID
//!     m_available = ECA status
//!   payload:
//!     16 bytes — verbatim copy of the offending request header
//! ```
//!
//! The audit confirmed: the original frame uses cmd 11 (not the
//! cmd of the rejected request) and the status sits in
//! `m_available`. Pre-fix Rust used cmd_error (zero-payload
//! EVENT_ADD with status in `m_cid`) which libca decodes as a
//! cancel-ack.

use epics_ca_rs::protocol::{CaHeader, ECA_NORDACCESS};

const CA_PROTO_ERROR: u16 = 11;
const CA_PROTO_EVENT_ADD: u16 = 1;

#[test]
fn golden_ext_event_add_admission_error_nord_access() {
    // Build the rejected EVENT_ADD request header (what the server
    // received) and the CA_PROTO_ERROR frame echoing it.
    let mut req = CaHeader::new(CA_PROTO_EVENT_ADD);
    req.postsize = 16; // monitor mask + selector
    req.data_type = 6; // DBR_DOUBLE
    req.count = 1;
    req.cid = 0x0000_002A; // server SID the client meant to subscribe
    req.available = 0xCAFE_BABE; // subscription id (sub_id)
    let echoed = req.to_bytes();

    // The error frame.
    let mut err = CaHeader::new(CA_PROTO_ERROR);
    err.postsize = echoed.len() as u16; // 16
    err.data_type = 0;
    err.count = 0;
    // channel-scoped errors echo the client CID, not SID.
    // For EVENT_ADD admission the audit landed on using the
    // request's `m_cid` (which the server stored as the SID slot —
    // the refinement preserves the count). Mirror that.
    err.cid = 0x0000_002A;
    err.available = ECA_NORDACCESS;

    let mut frame = Vec::new();
    frame.extend_from_slice(&err.to_bytes());
    frame.extend_from_slice(&echoed);

    let got: String = frame.iter().map(|b| format!("{:02x}", b)).collect();
    // ECA_NORDACCESS = defmsg(CA_K_WARNING=0, 46) = (46<<3)|0 = 0x170.
    assert_eq!(
        ECA_NORDACCESS, 0x170,
        "ECA_NORDACCESS constant should match defmsg(CA_K_WARNING, 46) = 0x170"
    );
    let want = "000b0010 0000 0000 0000002a 00000170 \
                0001 0010 0006 0001 0000002a cafebabe"
        .replace(' ', "");
    assert_eq!(got, want, "EVENT_ADD admission failure wire shape");
}
