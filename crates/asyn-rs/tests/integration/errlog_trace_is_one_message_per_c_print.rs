//! A trace on the `<errlog>` sink is C's errlog traffic: each `printPort` /
//! `printSource` piece, the message and every I/O data block is its own
//! `errlogPrintf`, which errlog cuts at `maxMsgSize` and hands to its
//! listeners (asynManager.c:3043-3058, :3108, :3171-3221; errlog.c:155-160).

use asyn_rs::trace::{TraceFile, TraceInfoMask, TraceIoMask, TraceManager, TraceMask};
use epics_libcom_rs::runtime::log::{DEFAULT_MAX_MSG_SIZE, ErrlogCapture, eltc};

#[test]
fn an_errlog_trace_is_one_errlog_message_per_c_print() {
    eltc(false);
    let errlog = ErrlogCapture::start();

    let mgr = TraceManager::new();
    mgr.set_trace_file(Some("EL"), TraceFile::Errlog);
    mgr.set_trace_mask(Some("EL"), TraceMask::ERROR);
    mgr.set_trace_info_mask(Some("EL"), TraceInfoMask::PORT | TraceInfoMask::SOURCE);
    let long = "x".repeat(DEFAULT_MAX_MSG_SIZE + 10);
    mgr.print_source(
        Some("EL"),
        None,
        0,
        TraceMask::ERROR,
        ("dir/f.c", 7),
        format_args!("{long}\n"),
    );
    mgr.set_trace_io_mask(Some("EL"), TraceIoMask::ASCII | TraceIoMask::HEX);
    mgr.output_device_io("EL", None, 0, TraceMask::ERROR, b"AB", "io ", "f.c", 9);
    let got = errlog.finish();

    let trunc = "<<TRUNCATED>>\n";
    let cut = format!("{}{trunc}", &long[..DEFAULT_MAX_MSG_SIZE - 1 - trunc.len()]);
    assert_eq!(
        got,
        [
            "[EL,-1,0] ",
            "[f.c:7] ",
            cut.as_str(),
            "[EL,-1,0] ",
            "[f.c:9] ",
            "io ",
            "AB\n",
            "\n",
            "41 ",
            "42 ",
            "\n",
        ]
    );
}
