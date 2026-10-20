//! An `Async Soft Channel` output's put to a local CA link completes through
//! dbNotify: `dbNotifyCompletion` queues `notifyCallback` on priorityLow
//! (`dbNotify.c:129-131`, `:466`), whose `callDone` (`:182`) reaches
//! `putComplete` → `dbCaCallbackProcess` → `dbLinkAsyncComplete`, so the
//! record's completing pass runs on the low band whatever its PRIO. A target
//! that does not process gets no `dbNotifyCompletion`: `callDone` runs inline
//! on `dbCaTask` (`dbNotify.c:270`), at priorityMedium. A refused put follows
//! the same rule: one that still processes (`didPut`, `dbNotify.c:528-530`)
//! completes through `dbNotifyCompletion`, one that does not completes inline.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{
    BackgroundTaskHandle, CallbackPriority, spawn_blocking_background,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "OUT") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "TGT CA")
    field(PRIO, "HIGH")
}
record(ao, "TGT") { }
"#;

/// Boundary — the record's own band is busy for the whole put: the
/// completing pass still runs, on the low band.
#[epics_macros_rs::epics_test]
async fn the_completing_pass_does_not_wait_for_the_records_band() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    open_links(&db).await;
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (busy, is_busy) = std::sync::mpsc::channel();
    let hold = spawn_blocking_background(CallbackPriority::High, move |_| {
        busy.send(()).unwrap();
        let _ = held.recv_timeout(Duration::from_secs(30));
    });
    is_busy.recv_timeout(Duration::from_secs(10)).unwrap();

    db.put_record_field_from_ca("OUT", "VAL", EpicsValue::Double(4.0))
        .await
        .unwrap();
    let out = db.get_record("OUT").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while out.read().is_processing() && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    let completed = !out.read().is_processing();
    let _ = release.send(());
    let _ = hold.await;

    assert_eq!(db.get_pv("TGT").unwrap().to_f64(), Some(4.0));
    assert!(completed, "OUT's completing pass waited for its PRIO band");
}

/// Boundary — the low band is busy for the whole put: the target takes the
/// value, and the completing pass waits for the low band, not the record's.
#[epics_macros_rs::epics_test]
async fn the_completing_pass_waits_for_the_low_band() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    open_links(&db).await;
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (busy, is_busy) = std::sync::mpsc::channel();
    let hold = spawn_blocking_background(CallbackPriority::Low, move |_| {
        busy.send(()).unwrap();
        let _ = held.recv_timeout(Duration::from_secs(30));
    });
    is_busy.recv_timeout(Duration::from_secs(10)).unwrap();

    db.put_record_field_from_ca("OUT", "VAL", EpicsValue::Double(4.0))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while db.get_pv("TGT").unwrap().to_f64() != Some(4.0) && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    let put = db.get_pv("TGT").unwrap().to_f64();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    let out = db.get_record("OUT").unwrap();
    let waited = out.read().is_processing();
    let _ = release.send(());
    let _ = hold.await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while out.read().is_processing() && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }

    assert_eq!(put, Some(4.0), "the target took the value");
    assert!(waited, "OUT completed while the low band was held");
    assert!(!out.read().is_processing(), "OUT completed once released");
}

/// Boundary — the target does not process (SCAN is not Passive), with the low
/// band and the record's own band both busy for the whole put: the completing
/// pass runs on neither.
#[epics_macros_rs::epics_test]
async fn an_unprocessed_target_completes_on_the_medium_band() {
    let (db, _h) = IocBuilder::new()
        .db_string(
            r#"
record(ao, "OUT") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "TGT CA")
    field(PRIO, "HIGH")
}
record(ao, "TGT") { field(SCAN, "Event") field(EVNT, "117") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    open_links(&db).await;
    let (releases, holds) = hold(&[CallbackPriority::Low, CallbackPriority::High]);

    db.put_record_field_from_ca("OUT", "VAL", EpicsValue::Double(4.0))
        .await
        .unwrap();
    let out = db.get_record("OUT").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while out.read().is_processing() && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    let completed = !out.read().is_processing();
    drop(releases);
    for hold in holds {
        let _ = hold.await;
    }

    assert_eq!(db.get_pv("TGT").unwrap().to_f64(), Some(4.0));
    assert!(completed, "OUT's completing pass waited for a held band");
}

const REFUSED: &str = r#"
record(stringout, "OUT") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "TGT.CALC CA")
    field(PRIO, "HIGH")
}
record(calc, "TGT") { field(CALC, "A+1") field(DISP, "$(DISP)") }
"#;

/// Build [`REFUSED`] with TGT's `DISP`, links run.
async fn refused(disp: &str) -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    let (db, _h) = IocBuilder::new()
        .db_string(REFUSED, &HashMap::from([("DISP".into(), disp.into())]))
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    open_links(&db).await;
    db
}

/// TGT's STAT: a process with the refused expression raises CALC_ALARM.
fn tgt_stat(db: &epics_base_rs::server::database::PvDatabase) -> Option<f64> {
    db.get_pv("TGT.STAT").unwrap().to_f64()
}

const CALC_ALARM: Option<f64> = Some(12.0);

/// Hold `bands` until the returned senders drop.
fn hold(
    bands: &[CallbackPriority],
) -> (
    Vec<std::sync::mpsc::Sender<()>>,
    Vec<BackgroundTaskHandle<()>>,
) {
    let (busy, is_busy) = std::sync::mpsc::channel();
    let held: (Vec<_>, Vec<_>) = bands
        .iter()
        .map(|&band| {
            let (release, held) = std::sync::mpsc::channel::<()>();
            let busy = busy.clone();
            let hold = spawn_blocking_background(band, move |_| {
                busy.send(()).unwrap();
                let _ = held.recv_timeout(Duration::from_secs(30));
            });
            (release, hold)
        })
        .unzip();
    for _ in bands {
        is_busy.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    held
}

/// Boundary — a refused put that still processes the target (calc's
/// `special` refuses the expression, `CALC` is pp): the completing pass waits
/// for the low band.
#[epics_macros_rs::epics_test]
async fn a_refused_put_that_processes_waits_for_the_low_band() {
    let db = refused("0").await;
    let (releases, holds) = hold(&[CallbackPriority::Low]);

    db.put_record_field_from_ca("OUT", "VAL", EpicsValue::String("A+".into()))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while tgt_stat(&db) != CALC_ALARM && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    let processed = tgt_stat(&db);
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    let out = db.get_record("OUT").unwrap();
    let waited = out.read().is_processing();
    drop(releases);
    for hold in holds {
        let _ = hold.await;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while out.read().is_processing() && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }

    assert_eq!(processed, CALC_ALARM, "the refused put processed TGT");
    assert!(waited, "OUT completed while the low band was held");
    assert!(!out.read().is_processing(), "OUT completed once released");
}

/// Boundary — a refused put that processes nothing (`DISP=1`), with the low
/// band and the record's own band both busy: the completing pass runs on
/// neither.
#[epics_macros_rs::epics_test]
async fn a_refused_put_that_does_not_process_completes_on_the_medium_band() {
    let db = refused("1").await;
    let (releases, holds) = hold(&[CallbackPriority::Low, CallbackPriority::High]);

    db.put_record_field_from_ca("OUT", "VAL", EpicsValue::String("A+".into()))
        .await
        .unwrap();
    let out = db.get_record("OUT").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while out.read().is_processing() && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    let completed = !out.read().is_processing();
    drop(releases);
    for hold in holds {
        let _ = hold.await;
    }

    assert_ne!(tgt_stat(&db), CALC_ALARM, "TGT not processed");
    assert!(completed, "OUT's completing pass waited for a held band");
}

/// Boundary — a refused put whose process goes async (calc's `special`
/// refuses the expression, `CALC` is pp, and TGT's forward link reaches a
/// calcout whose `ODLY` defers it): the completing pass waits for that
/// chain, as `dbNotifyCompletion` ends it.
#[epics_macros_rs::epics_test]
async fn a_refused_put_waits_for_the_chain_it_started() {
    let (db, _h) = IocBuilder::new()
        .db_string(
            r#"
record(stringout, "OUT") {
    field(DTYP, "Async Soft Channel")
    field(OUT,  "TGT.CALC CA")
}
record(calc, "TGT") { field(CALC, "1") field(FLNK, "DLY") }
record(calcout, "DLY") { field(CALC, "1") field(ODLY, "0.5") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    open_links(&db).await;

    db.put_record_field_from_ca("OUT", "VAL", EpicsValue::String("A+".into()))
        .await
        .unwrap();
    let (out, dly) = (db.get_record("OUT").unwrap(), db.get_record("DLY").unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !dly.read().is_processing() && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    let delayed = tgt_stat(&db) == CALC_ALARM && dly.read().is_processing();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    let waited = out.read().is_processing() && dly.read().is_processing();
    let deadline = Instant::now() + Duration::from_secs(5);
    while out.read().is_processing() && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }

    assert!(
        delayed,
        "the refused put processed TGT, and DLY into its delay"
    );
    assert!(waited, "OUT completed before the chain did");
    assert!(!out.read().is_processing(), "OUT completed with the chain");
}

/// iocInit's open pass, then each external link connected: C opens every CA
/// link at `dbCaAddLink` and refuses its puts until it connects.
async fn open_links(db: &epics_base_rs::server::database::PvDatabase) {
    db.setup_external_link_opens().await;
    for record in db.all_record_names() {
        for (field, _, link) in db.record_link_fields(&record) {
            let Some(link) = link.external() else {
                continue;
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while db.external_link_metadata(link).is_none() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{record}.{field} connects"
                );
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
    }
}
