//! A stream record's `%(record.FIELD)` targets join its lock set by a
//! relink that takes their sets, so it runs with no set held: re-initialising
//! the record while another thread takes the target's set and then the
//! record's, in id order as `dbScanLockMany` does, does not deadlock. The
//! re-initialisations are a `streamReload` (against `lock_records` and
//! against a CA link put) and the first init of a record created after
//! `iocInit`.

#![allow(clippy::disallowed_macros, reason = "tests may keep eprintln!")]

mod redirect;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use epics_base_rs::runtime::task::test_block_on;
use epics_base_rs::server::database::{PvDatabase, RecordLoad};
use epics_base_rs::server::db_loader::{apply_fields, create_record, parse_db};
use epics_base_rs::types::EpicsValue;
use redirect::ioc;

/// A deadlocked relink keeps the record's set, and the test's next read of
/// it waits for it: abort the run instead of hanging it.
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

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("lock_partners_join_with_no_set_held")
        .join(test);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Protocol `p` reads `inputs`; `q` only writes.
fn protocol(file: &Path, inputs: &str) {
    let text =
        format!("Terminator = LF;\np {{ out \"Z1\"; in \"{inputs}\"; }}\nq {{ out \"Z0\"; }}\n");
    std::fs::write(file, text).unwrap();
}

fn id(db: &PvDatabase, record: &str) -> u64 {
    db.lock_set_of(record).unwrap().id
}

fn reload(record: &str) {
    let mut out = Vec::new();
    assert_eq!(stream_rs::epics::stream_reload(Some(record), &mut out), 0);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("{record}: Protocol reloaded\n")
    );
}

/// Takes `[first, second]` in id order, over and over, until stopped.
fn lock_many(
    db: Arc<PvDatabase>,
    first: &'static str,
    second: impl Fn() -> String + Send + 'static,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<u64>) {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        let mut n = 0;
        while !flag.load(Ordering::Acquire) {
            drop(db.lock_records([first.to_string(), second()]));
            n += 1;
        }
        n
    });
    (stop, thread)
}

#[test]
fn a_reload_against_lock_records_of_a_lower_target() {
    let file = dir("vs_lock_records").join("p.proto");
    protocol(&file, "%d");
    let _watchdog = Watchdog::arm("reload against lock_records");
    test_block_on(async {
        let db = ioc(&format!(
            r#"
record(ai, "t0") {{ field(DTYP, "stream") field(INP, "@{0} q R") }}
record(ai, "sx") {{ field(DTYP, "stream") field(INP, "@{0} p R") }}
"#,
            file.display()
        ))
        .await;
        assert!(id(&db, "t0") < id(&db, "sx"));
        let (stop, locker) = lock_many(db.clone(), "t0", || "sx".to_string());
        for round in 0..200 {
            protocol(&file, if round % 2 == 0 { "%(t0.VAL)d" } else { "%d" });
            reload("sx");
        }
        stop.store(true, Ordering::Release);
        assert!(locker.join().unwrap() > 0);
    });
}

/// `t0.FLNK = sx` takes both sets up front, as `dbPutFieldLink` does.
#[test]
fn a_reload_against_a_link_put_naming_it() {
    let file = dir("vs_link_put").join("p.proto");
    protocol(&file, "%d");
    let _watchdog = Watchdog::arm("reload against a link put");
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<Arc<PvDatabase>>();
    let putter = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let db = rx.recv().unwrap();
            let put = |text: &str| {
                let db = db.clone();
                let text = text.to_string();
                async move {
                    db.put_record_field_from_ca_no_notify(
                        "t0",
                        "FLNK",
                        EpicsValue::String(text.into()),
                    )
                    .await
                    .unwrap();
                }
            };
            let mut n = 0;
            test_block_on(async {
                while !stop.load(Ordering::Acquire) {
                    put("sx").await;
                    put("").await;
                    n += 1;
                }
            });
            n
        })
    };
    test_block_on(async {
        let db = ioc(&format!(
            r#"
record(ai, "t0") {{ field(DTYP, "stream") field(INP, "@{0} q R") }}
record(ai, "sx") {{ field(DTYP, "stream") field(INP, "@{0} p R") }}
"#,
            file.display()
        ))
        .await;
        assert!(id(&db, "t0") < id(&db, "sx"));
        tx.send(db.clone()).unwrap();
        for round in 0..400 {
            protocol(&file, if round % 2 == 0 { "%(t0.VAL)d" } else { "%d" });
            reload("sx");
        }
        stop.store(true, Ordering::Release);
    });
    assert!(putter.join().unwrap() > 0);
}

/// A record created after `iocInit` runs its first init then, its set
/// minted after every set `iocInit` built.
#[test]
fn a_record_created_after_ioc_init_against_lock_records() {
    const CREATES: usize = 200;
    let file = dir("created").join("p.proto");
    protocol(&file, "%(t0.VAL)d");
    let _watchdog = Watchdog::arm("create against lock_records");
    test_block_on(async {
        let db = ioc(&format!(
            "record(ai, \"t0\") {{ field(DTYP, \"stream\") field(INP, \"@{} q R\") }}\n",
            file.display()
        ))
        .await;
        let creating = Arc::new(AtomicUsize::new(0));
        let (stop, locker) = {
            let creating = creating.clone();
            lock_many(db.clone(), "t0", move || {
                format!("sx{}", creating.load(Ordering::Acquire))
            })
        };
        for n in 0..CREATES {
            creating.store(n, Ordering::Release);
            let text = format!(
                "record(ai, \"sx{n}\") {{ field(DTYP, \"stream\") field(INP, \"@{} p R\") }}",
                file.display()
            );
            for def in parse_db(&text, &HashMap::new()).unwrap() {
                let mut record = create_record(&def.record_type).unwrap();
                let mut common = Vec::new();
                apply_fields(&mut record, &def.fields, &mut common).unwrap();
                db.add_loaded_record(&def.name, record, RecordLoad::from_common_fields(common))
                    .await
                    .unwrap();
            }
        }
        stop.store(true, Ordering::Release);
        assert!(locker.join().unwrap() > 0);
        for n in 0..CREATES {
            assert_eq!(id(&db, &format!("sx{n}")), id(&db, "t0"), "sx{n}");
        }
    });
}
