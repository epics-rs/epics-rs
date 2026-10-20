//! The records a protocol's `%(record.FIELD)` formats name share the
//! record's lock set (`InitHold::set_lock_partners`), so an entry that
//! holds the record puts to them without taking a second set: two records
//! redirecting into each other from their bus threads do not deadlock (a),
//! nor does an I/O Intr input against a PROC put of the same record (b),
//! and a `streamReload` merges a target the protocol gains and splits off
//! one it drops (c).

#![allow(clippy::disallowed_macros, reason = "tests may keep eprintln!")]

mod redirect;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{sleep, test_block_on};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use redirect::{ASYNC_FED, ASYNC_FEED, REPLIES, WRITES, get, ioc, process};

/// A deadlocked entry keeps its record's set, and the test's next read of
/// that record waits for it: abort the run instead of hanging it.
struct Watchdog(Arc<AtomicBool>);

impl Watchdog {
    fn arm(what: &'static str) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            if !flag.load(Ordering::Acquire) {
                eprintln!("{what}: still running after 60 s, deadlocked");
                std::process::abort();
            }
        });
        Self(done)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn f(db: &PvDatabase, pv: &str) -> f64 {
    get(db, pv).to_f64().unwrap()
}

fn same_set(db: &PvDatabase, a: &str, b: &str) -> bool {
    db.lock_set_of(a).unwrap().id == db.lock_set_of(b).unwrap().id
}

fn processing(db: &PvDatabase, record: &str) -> bool {
    db.get_record(record).unwrap().read().is_processing()
}

async fn put_proc(db: &PvDatabase, record: &str) {
    db.put_record_field_from_ca_no_notify(record, "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
}

#[test]
fn two_records_redirecting_into_each_other_from_bus_threads() {
    let words: &'static str = vec!["a"; 40].join(" ").leak();
    REPLIES.lock().extend([("CX", words), ("CY", words)]);
    let _watchdog = Watchdog::arm("cross redirect");
    test_block_on(async {
        let db = ioc(r#"
record(ai, "cx") { field(DTYP, "stream") field(INP, "@redirect.proto cross_x R") }
record(ai, "cy") { field(DTYP, "stream") field(INP, "@redirect.proto cross_y R") }
"#)
        .await;
        assert!(same_set(&db, "cx", "cy"));
        for round in 0..200 {
            let before = WRITES.lock().len();
            put_proc(&db, "cx").await;
            put_proc(&db, "cy").await;
            let deadline = Instant::now() + Duration::from_secs(5);
            while WRITES.lock().len() < before + 2 || processing(&db, "cx") || processing(&db, "cy")
            {
                assert!(Instant::now() < deadline, "round {round}: still processing");
                sleep(Duration::from_millis(1)).await;
            }
        }
        for r in ["cx", "cy"] {
            assert_eq!(get(&db, &format!("{r}.DESC")).to_string(), "a", "{r}");
            assert_eq!(f(&db, &format!("{r}.SEVR")), 0.0, "{r}");
        }
    });
}

/// The bus thread holds `ia` for each input while PROC puts process it.
#[test]
fn an_io_intr_input_during_proc_puts_of_the_same_record() {
    const INPUTS: usize = 300;
    ASYNC_FEED.store(INPUTS, Ordering::Release);
    let _watchdog = Watchdog::arm("I/O Intr against PROC");
    test_block_on(async {
        let db = ioc(r#"
record(ao, "it") { }
record(ai, "ia") { field(DTYP, "stream") field(INP, "@redirect.proto intr_feed RA") field(SCAN, "I/O Intr") }
"#)
        .await;
        assert!(same_set(&db, "ia", "it"));
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut puts = 0;
        while ASYNC_FED.load(Ordering::Acquire) < INPUTS {
            assert!(Instant::now() < deadline, "{puts} puts, input stalled");
            put_proc(&db, "ia").await;
            puts += 1;
        }
        while f(&db, "ia") != INPUTS as f64 || f(&db, "it") != INPUTS as f64 {
            assert!(Instant::now() < deadline, "the last input did not land");
            sleep(Duration::from_millis(5)).await;
        }
        assert!(puts > 0);
    });
}

#[test]
fn a_reload_merges_a_target_the_protocol_gains_and_splits_one_it_drops() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("field_redirect_lockset");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("reload.proto");
    let protocol = |inputs: &str| {
        let text = format!("Terminator = LF;\np {{ out \"L1\"; in \"{inputs}\"; }}\n");
        std::fs::write(&file, text).unwrap();
    };
    protocol("%(lt1.VAL)d");
    REPLIES.lock().push(("L1", "4 5"));
    let reload = || {
        let mut out = Vec::new();
        assert_eq!(stream_rs::epics::stream_reload(Some("la"), &mut out), 0);
        assert_eq!(String::from_utf8(out).unwrap(), "la: Protocol reloaded\n");
    };
    test_block_on(async {
        let db = ioc(&format!(
            r#"
record(ao, "lt1") {{ }}
record(ao, "lt2") {{ }}
record(ai, "la") {{ field(DTYP, "stream") field(INP, "@{} p R") }}
"#,
            file.display()
        ))
        .await;
        assert!(same_set(&db, "la", "lt1"));
        assert!(!same_set(&db, "la", "lt2"));

        protocol("%(lt1.VAL)d %(lt2.VAL)d");
        reload();
        assert!(same_set(&db, "la", "lt1"));
        assert!(same_set(&db, "la", "lt2"));
        assert_eq!(process(&db, "la", 1).await, ["L1"]);
        assert_eq!(f(&db, "lt1"), 4.0);
        assert_eq!(f(&db, "lt2"), 5.0);
        assert_eq!(f(&db, "la.SEVR"), 0.0);

        protocol("%(lt2.VAL)d");
        reload();
        assert!(same_set(&db, "la", "lt2"));
        assert!(!same_set(&db, "la", "lt1"));
    });
}
