//! A device byte that is not UTF-8 reaches C's errlog console as itself:
//! `errlogPrintf("%.*s\n", nBytes, buffer)` copies it into errlog's byte
//! buffer and the worker `fprintf`s it out (asynManager.c:3175, errlog.c:795).

use asyn_rs::trace::{TraceFile, TraceInfoMask, TraceIoMask, TraceManager, TraceMask};
use epics_libcom_rs::runtime::log::{
    eltc, errlog_flush, errlog_set_console, errlog_set_console_to_stderr,
};

#[test]
fn a_non_utf8_device_byte_reaches_the_errlog_console_unchanged() {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("errlog-console-{}", std::process::id()));
    errlog_set_console(std::fs::File::create(&path).unwrap());
    eltc(true);

    let mgr = TraceManager::new();
    mgr.set_trace_file(Some("EB"), TraceFile::Errlog);
    mgr.set_trace_mask(Some("EB"), TraceMask::ERROR);
    mgr.set_trace_info_mask(Some("EB"), TraceInfoMask::empty());
    mgr.set_trace_io_mask(Some("EB"), TraceIoMask::ASCII);
    mgr.output_device_io("EB", None, 0, TraceMask::ERROR, b"a\xffb", "rd ", "f.c", 1);
    mgr.print_with(Some("EB"), None, 0, TraceMask::ERROR, ("", 0), |out| {
        out.write_bytes(b"reply \xfe\n")
    });
    errlog_flush();
    errlog_set_console_to_stderr();

    let console = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(console, b"rd a\xffb\nreply \xfe\n");
}
