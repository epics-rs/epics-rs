//! aaiRecord.c calls its dset's `init_record` in pass 0 (`:119-127`), so
//! C's iocInit runs a stream aai's init before `doResolveLinks` and every
//! record's `add_record`. The C oracle (streamApp, this database) printed
//! b's protocol errors first and x's add_record error after them; iocsh's
//! thread flushes errlog per message (errlog.c:186), so that order is C's.

use std::collections::HashMap;
use std::io::Write;

use epics_base_rs::server::ioc_builder::IocBuilder;
use parking_lot::Mutex;

/// errlog's lines and the stream error output, in the order they came.
static OUT: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Out;

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        OUT.lock()
            .extend(String::from_utf8_lossy(b).lines().map(str::to_string));
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn aai_inits_before_add_record() {
    epics_base_rs::runtime::task::test_block_on(run());
}

async fn run() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", dir.path()) };
    stream_rs::debug_interface::register();
    stream_rs::error::set_stream_msg_time_stamped(0);
    stream_rs::error::set_stream_debug_colored(false);
    stream_rs::error::set_stream_debug_file(Some(Box::new(Out)));
    epics_base_rs::runtime::log::errlog_add_listener(|m| {
        OUT.lock().extend(m.lines().map(str::to_string));
    });
    let db = r#"
record(ai, "x") { field(DTYP, "Async Soft Channel") field(INP, "nosuch") }
record(aai, "b") { field(DTYP, "stream") field(INP, "@nofile.proto ra debug") field(FTVL, "DOUBLE") field(NELM, "2") }
"#;
    let ioc = stream_rs::dev::register(IocBuilder::new())
        .db_string(db, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    assert!(ioc.run().await);
    epics_base_rs::runtime::log::errlog_flush();

    let out = OUT.lock().clone();
    let at = |needle: &str| {
        out.iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no {needle:?} in {out:#?}"))
    };
    assert!(
        at("b: Record initialization failed") < at("(add_record) link target not found"),
        "{out:#?}"
    );
    assert_eq!(epics_base_rs::server::ioc_app::ioc_shutdown(), 0);
}
