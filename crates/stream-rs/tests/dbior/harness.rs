//! A stream IOC booted on `tests/oracle/dbior`, and `dbior stream` on it
//! against C's output for the same level, its version as rs's.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use stream_rs::epics::STREAM_VERSION;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/dbior");

/// streamApp's bus interfaces, as `dev::register_app` registers them, and
/// `db` built and run.
pub async fn boot(db: &str) -> Arc<PvDatabase> {
    // SAFETY: set before the build, which is the only reader.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", DIR) };
    stream_rs::debug_interface::register();
    stream_rs::dummy::register();
    stream_rs::asyn::register();
    let ioc = stream_rs::dev::register(IocBuilder::new())
        .db_string(db, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}

/// C's bytes for `dbior stream <level>` in `oracle`, its version as rs's.
pub fn c(oracle: &str, level: &str) -> Vec<u8> {
    let all = std::fs::read(format!("{DIR}/{oracle}")).unwrap();
    let marker = format!("==L{level}");
    let mut lines = all
        .split_inclusive(|&b| b == b'\n')
        .skip_while(|l| l.trim_ascii_end() != marker.as_bytes())
        .skip(1)
        .take_while(|l| !l.starts_with(b"=="))
        .peekable();
    let mut out = Vec::new();
    while let Some(line) = lines.next() {
        if line.starts_with(b"  StreamDevice ") {
            out.extend_from_slice(format!("  {STREAM_VERSION}\n").as_bytes());
            lines.next_if(|l| l.starts_with(b"  commit: "));
        } else {
            out.extend_from_slice(line);
        }
    }
    out
}

/// Run an iocsh line on a thread of its own, as the shell's thread runs it;
/// give its stdout.
pub async fn shell(db: &Arc<PvDatabase>, line: &str) -> Vec<u8> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "stream-rs-dbior-{}-{}.out",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let line = format!("{line} > {}", path.display());
    let db = db.clone();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let thread = std::thread::spawn(move || IocShell::new(db, bridge).execute_line(line).unwrap());
    while !thread.is_finished() {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    thread.join().unwrap();
    let out = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    out
}

/// `dbior stream <level>` on `db` is `oracle`'s, byte for byte.
pub async fn assert_levels(db: &Arc<PvDatabase>, oracle: &str, levels: &[&str]) {
    for level in levels {
        let rs = shell(db, &format!("dbior stream {level}")).await;
        let c = c(oracle, level);
        assert!(
            rs == c,
            "interest {level}\nrs:\n{}\nC:\n{}",
            String::from_utf8_lossy(&rs),
            String::from_utf8_lossy(&c)
        );
    }
}
