//! A `sub` record whose SNAM names nothing must say so.
//!
//! ```c
//! if (prec->snam[0] == 0) { ... }
//! prec->sadr = (SUBFUNCPTR)registryFunctionFind(prec->snam);
//! if (prec->sadr == NULL) {
//!     fprintf(stderr, "%s.SNAM " ERL_ERROR " function '%s' not found\n",
//!             prec->name, prec->snam);
//!     return S_db_BadSub;
//! }
//! ```
//! (`subRecord.c:123-130`; `aSubRecord.c:155-160` is the same, and `:145` /
//! `subRecord.c:110` are the INAM half.)
//!
//! The port resolved SNAM and threw the answer away when it was `None`, so an
//! IOC booted with a record that would process as a no-op and printed nothing
//! at all; measured across `scripts/compat-smoke.sh`, C wrote 9 such lines in
//! 6 cases and the port wrote 0. Two things hid it. The INAM half was worded
//! `iocInit: <name>.INAM function ... not found`, so it did not read as C's
//! and could not stand in; and `wire_subroutines` returned early when the
//! subroutine registry was EMPTY — precisely the IOC whose `.db` names
//! subroutines that its binary never registered, which is every case above.
//!
//! `fprintf(stderr, ...)` and not `errlogPrintf`, so this reads the console
//! rather than a listener.
//!
//! Unix only: reading that console means pointing fd 2 somewhere else and
//! putting it back. There is no fd 2 on Windows, and `libc` is a `cfg(unix)`
//! dependency of this crate.

#![cfg(unix)]

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

/// Point fd 2 at a file; the returned guard restores it and reads back what
/// was written. Not a closure: the body is `async`, and fd 2 is process-wide
/// so it does not need to be.
struct Captured {
    sink: tempfile::NamedTempFile,
    saved: i32,
}

impl Captured {
    fn start() -> Self {
        let sink = tempfile::NamedTempFile::new().expect("capture file");
        let saved = unsafe { libc::dup(2) };
        assert!(saved >= 0, "dup(2) failed");
        let fd = {
            use std::os::fd::AsRawFd;
            sink.as_file().as_raw_fd()
        };
        assert!(unsafe { libc::dup2(fd, 2) } >= 0, "dup2 onto fd 2 failed");
        Self { sink, saved }
    }

    /// Restore BEFORE anything can panic on an assertion, or the failure
    /// report has nowhere to go.
    fn finish(self) -> String {
        assert!(
            unsafe { libc::dup2(self.saved, 2) } >= 0,
            "restore fd 2 failed"
        );
        unsafe { libc::close(self.saved) };
        strip_ansi(&std::fs::read_to_string(self.sink.path()).expect("read the capture"))
    }
}

/// `ERL_ERROR` is `ANSI_RED("ERROR")`, so the escapes come off before the
/// text is compared — the same way `errlog` strips its own.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('\u{1b}') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match rest.find('m') {
            Some(end) => rest = &rest[end + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

const DB: &str = r#"
record(sub, "T:I") {
  field(INAM, "noSuchInit")
}
record(sub, "T:S") {
  field(SNAM, "noSuchSub")
}
record(sub, "T:B") {
  field(INAM, "noSuchInit")
  field(SNAM, "noSuchSub")
}
"#;

/// Both halves, with C's wording, from an IOC that registered no subroutines
/// at all — the state the old early return treated as "nothing to report".
///
/// One record each, because C's INAM failure is `return S_db_BadSub`
/// (`subRecord.c:113`) and everything below it — including the SNAM lookup —
/// is skipped. Measured on softIoc R7.0.10 with both fields wrong: the INAM
/// line alone.
#[epics_macros_rs::epics_test]
async fn an_unresolved_snam_and_inam_are_both_reported() {
    let capture = Captured::start();
    let built = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await;
    let out = capture.finish();
    built.expect("the IOC still builds; an unresolved SNAM is reported, not fatal");

    assert!(
        out.contains("T:S.SNAM ERROR function 'noSuchSub' not found"),
        "C `subRecord.c:126`, got {out:?}"
    );
    assert!(
        out.contains("T:I.INAM ERROR function 'noSuchInit' not found"),
        "C `subRecord.c:110` — and no `iocInit: ` frame of our own, got {out:?}"
    );
    assert!(
        out.contains("T:B.INAM ERROR function 'noSuchInit' not found"),
        "C `subRecord.c:110`, got {out:?}"
    );
    assert!(
        !out.contains("T:B.SNAM"),
        "C `subRecord.c:113` returns above the SNAM lookup, got {out:?}"
    );
}
