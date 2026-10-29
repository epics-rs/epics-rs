//! scaler's three `callbackRequestDelayed` timers run C's own callback bodies.
//!
//! `delayCallbackFunc` (`scalerRecord.c:216-231`) and `autoCallbackFunc`
//! (`:233-239`) end in `scanOnce`, a fresh `dbProcess` on the scan-once
//! thread; `updateCallbackFunc` (`:203-214`) calls `updateCounts` and never
//! processes. oracleScalerIoc (`st-scaler.cmd`), `TPRO 1`, `DLY 0.3`, `CNT 1`:
//!
//! ```text
//! _main_: dbProcess of 'SC'
//! scanOnce: dbProcess of 'SC'
//! ```
//!
//! and `TPRO 1`, `DLY1 0.3`, `CONT 1` ends in the same `scanOnce` line. With
//! `DISA 1` written mid-count, STAT and SEVR keep what they read while the
//! display refreshes, since no refresh reaches `dbProcess`'s disable test,
//! and the counts a refresh reads are still posted (`:579-588`).
//!
//! The three are separate `CALLBACK`s (`:269-294`), so arming or cancelling
//! one leaves the others pending. oracleScalerIoc, `SA` on drvScalerSoft with
//! `SCAN Event TP1 100 RAT1 10 DLY 5`, `CONT 1`, `a2 7`, then `CNT 1`: 0.4 s
//! later `S2 0 US 1 SS 2`, the autocount refresh zeroing the display while the
//! user count waits (`:571-575`); `CNT 0`, `a2 9`: `S2 9 US 0`. `SB` with
//! `TP 100 RATE 10 DLY1 0.5 TPRO 1`, `CONT 1`, `CNT 1`: the autocount restart
//! still fires inside the user count, `scanOnce: dbProcess of 'SB'`.

// RTEMS-EXEC-MODEL-ALLOW(5): checked, not waived — all ran and passed on
// the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run
// -p scaler-rs --all-features`); nothing here builds a CA server, and the
// reactor these sleep on comes from `#[tokio::test]` itself.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use scaler_rs::device_support::scaler_soft::SoftScalerDriver;
use scaler_rs::records::scaler::ScalerRecord;

const CHILD: &str = "SCALER_DELAYED_CALLBACK_CHILD";

/// A Passive scaler with no device support: a started count never finishes,
/// so the only passes after the put are the ones its timers make.
async fn add_scaler(db: &PvDatabase, name: &str, set: impl FnOnce(&mut ScalerRecord)) {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.nch = 4;
    set(&mut rec);
    rec.init_record(1).unwrap();
    db.add_record(name, Box::new(rec)).await.unwrap();
    db.put_record_field_from_ca(name, "TPRO", EpicsValue::Char(1))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_delayed_start_and_the_autocount_restart_are_scan_once_passes() {
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "the_delayed_start_and_the_autocount_restart_are_scan_once_passes",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = String::from_utf8(out.stdout).unwrap();
        let mut trace: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("scanOnce: "))
            .collect();
        trace.sort_unstable();
        assert_eq!(
            trace,
            ["scanOnce: dbProcess of 'SA'", "scanOnce: dbProcess of 'SC'",],
            "{out}"
        );
        return;
    }
    let db = PvDatabase::new();
    add_scaler(&db, "SC", |r| r.dly = 0.2).await;
    add_scaler(&db, "SA", |r| r.dly1 = 0.2).await;
    db.put_record_field_from_ca("SC", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("SA", "CONT", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
}

#[tokio::test]
async fn a_display_refresh_never_raises_the_disable_alarm() {
    let db = PvDatabase::new();
    add_scaler(&db, "SC", |r| r.rate = 10.0).await;
    db.put_record_field_from_ca("SC", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    let alarm = |db: &PvDatabase| {
        let r = db.get_record("SC").unwrap();
        let r = r.read();
        (r.common.stat, r.common.sevr)
    };
    let before = alarm(&db);
    db.put_record_field_from_ca("SC", "DISA", EpicsValue::Short(1))
        .await
        .unwrap();
    // Three refreshes at RATE 10, all inside the count.
    tokio::time::sleep(Duration::from_millis(350)).await;
    let r = db.get_record("SC").unwrap();
    assert_eq!(
        r.read().record.get_field("SS"),
        Some(EpicsValue::Short(2)),
        "still counting"
    );
    assert_eq!(
        alarm(&db),
        before,
        "updateCallbackFunc calls updateCounts, not dbProcess"
    );
}

#[tokio::test]
async fn a_display_refresh_posts_the_counts_it_reads() {
    let driver = SoftScalerDriver::new(4);
    let inputs = driver.shared_counts();
    let db = PvDatabase::new();
    add_scaler(&db, "SC", |r| {
        r.rate = 10.0;
        r.bind_dset(std::sync::Arc::new(std::sync::Mutex::new(Box::new(driver))));
    })
    .await;
    db.put_record_field_from_ca("SC", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    let mut s2 = db
        .get_record("SC")
        .unwrap()
        .write()
        .add_subscriber("S2", 1, DbFieldType::Long, EventMask::VALUE.bits())
        .unwrap();
    while s2.try_recv().is_ok() {}

    inputs.lock().unwrap()[1] = 7;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let posted: Vec<f64> = std::iter::from_fn(|| s2.try_recv().ok())
        .filter_map(|e| e.value().as_value().cloned().and_then(|v| v.to_f64()))
        .collect();
    assert_eq!(posted, [7.0], "updateCounts posts a changed S2 once");
}

#[tokio::test]
async fn a_delayed_start_leaves_the_autocount_refresh_running() {
    let driver = SoftScalerDriver::new(4);
    let inputs = driver.shared_counts();
    let db = PvDatabase::new();
    add_scaler(&db, "SA", |r| {
        r.tp1 = 100.0;
        r.rat1 = 10.0;
        r.dly = 5.0;
        r.bind_dset(std::sync::Arc::new(std::sync::Mutex::new(Box::new(driver))));
    })
    .await;
    db.put_record_field_from_ca("SA", "SCAN", EpicsValue::String("Event".into()))
        .await
        .unwrap();
    db.put_record_field_from_ca("SA", "CONT", EpicsValue::Short(1))
        .await
        .unwrap();
    let rec = db.get_record("SA").unwrap();
    let field = |f: &str| rec.read().record.get_field(f);
    for _ in 0..100 {
        if field("SS") == Some(EpicsValue::Short(2)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut s2 = rec
        .write()
        .add_subscriber("S2", 1, DbFieldType::Long, EventMask::VALUE.bits())
        .unwrap();
    while s2.try_recv().is_ok() {}
    let mut posted = || -> Vec<f64> {
        std::iter::from_fn(|| s2.try_recv().ok())
            .filter_map(|e| e.value().as_value().cloned().and_then(|v| v.to_f64()))
            .collect()
    };

    // The autocount start's arm cleared the inputs.
    inputs.lock().unwrap()[1] = 7;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(posted(), [7.0], "the autocount refresh runs");

    db.put_record_field_from_ca("SA", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(
        posted(),
        [0.0],
        "arming the delay callback leaves the update callback pending"
    );
    assert_eq!(
        (field("US"), field("SS")),
        (Some(EpicsValue::Short(1)), Some(EpicsValue::Short(2)))
    );

    db.put_record_field_from_ca("SA", "CNT", EpicsValue::Short(0))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        posted(),
        [7.0],
        "the abort's scanOnce reads the inputs again"
    );
    assert_eq!(field("US"), Some(EpicsValue::Short(0)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_count_leaves_the_autocount_restart_armed() {
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "a_user_count_leaves_the_autocount_restart_armed",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = String::from_utf8(out.stdout).unwrap();
        let trace: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("scanOnce: "))
            .collect();
        assert_eq!(trace, ["scanOnce: dbProcess of 'SB'"], "{out}");
        return;
    }
    let db = PvDatabase::new();
    add_scaler(&db, "SB", |r| {
        r.tp = 100.0;
        r.rate = 10.0;
        r.dly1 = 0.5;
    })
    .await;
    db.put_record_field_from_ca("SB", "CONT", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("SB", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let r = db.get_record("SB").unwrap();
    assert_eq!(r.read().record.get_field("US"), Some(EpicsValue::Short(3)));
}
