//! pvxs `Status` wire shape — `pvaproto.h:441 to_wire(Status&)`.
//!
//! `0xFF` when `code == Ok && msg.empty() && trace.empty()` (the
//! "bare OK" sentinel that GET/PUT/MONITOR responses carry most of
//! the time). Otherwise: 1-byte `code` (0 Ok, 1 Warn, 2 Error,
//! 3 Fatal) followed by Size-prefixed `msg` then Size-prefixed
//! `trace`.
//!
//! Expected bytes come from `tools/pvxs-golden-capture/fixtures.txt`
//! (`to_wire(buf, Status&)` at run time).

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::proto::status::{Status, StatusKind};

use super::pvxs_fixtures::golden;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn encode(s: &Status, order: ByteOrder) -> String {
    let mut out = Vec::new();
    s.write_into(order, &mut out);
    hex(&out)
}

#[test]
fn golden_pvxs_status_ok_no_msg() {
    assert_eq!(
        encode(&Status::OkNoMsg, ByteOrder::Big),
        golden("status_ok_no_msg"),
    );
}

#[test]
fn golden_pvxs_status_ok_with_msg() {
    let s = Status::Detailed {
        kind: StatusKind::Ok,
        message: "ok".into(),
        stack: "trace".into(),
    };
    assert_eq!(encode(&s, ByteOrder::Big), golden("status_ok_with_msg"),);
}

#[test]
fn golden_pvxs_status_warning() {
    let s = Status::Detailed {
        kind: StatusKind::Warning,
        message: "be careful".into(),
        stack: String::new(),
    };
    assert_eq!(encode(&s, ByteOrder::Big), golden("status_warning"));
}

#[test]
fn golden_pvxs_status_error() {
    let s = Status::Detailed {
        kind: StatusKind::Error,
        message: "oh no".into(),
        stack: String::new(),
    };
    assert_eq!(encode(&s, ByteOrder::Big), golden("status_error"));
}

#[test]
fn golden_pvxs_status_fatal() {
    let s = Status::Detailed {
        kind: StatusKind::Fatal,
        message: "boom".into(),
        stack: "stack".into(),
    };
    assert_eq!(encode(&s, ByteOrder::Big), golden("status_fatal"));
}
