//! A boot restore stores the way C's `reboot_restore` does: a scalar through
//! the static database (`dbrestore.c` `scalar_restore`: `dbPutString`,
//! `dbPutMenuIndex`), an array straight through `dbPutConvertRoutine`
//! (`SR_put_array_values`), neither running `special()` nor processing. Only
//! a `DBF_NOACCESS` scalar takes `dbPut`. Links restore in pass 0 only and
//! arrays in pass 1 only; a `DBF_DEVICE` field is not restored at all.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::types::EpicsValue;

/// Boot an IOC from `db`, restoring `sav` in `pass`, and return `read` of
/// its database once iocInit is done.
async fn booted<T: Send + 'static>(
    db: &str,
    pass: u8,
    sav: &str,
    read: impl FnOnce(&PvDatabase) -> T + Send + 'static,
) -> T {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.db"), db).unwrap();
    std::fs::write(dir.path().join("s.sav"), format!("# h\n{sav}<END>\n")).unwrap();
    let found = Arc::new(Mutex::new(None));
    let out = found.clone();
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_line(&format!("set_savefile_path(\"{}\")", dir.path().display()))
        .startup_line(&format!("set_pass{pass}_restoreFile(\"s.sav\")"))
        .startup_line(&format!(
            "dbLoadRecords(\"{}\")",
            dir.path().join("t.db").display()
        ))
        .run(move |config| async move {
            *out.lock().unwrap() = Some(read(&config.db));
            Ok(())
        })
        .await
        .unwrap();
    found.lock().unwrap().take().unwrap()
}

fn num(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name).unwrap().to_f64().unwrap()
}

fn text(db: &PvDatabase, name: &str) -> String {
    match db.get_pv(name).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("{name}: {other:?}"),
    }
}

/// `dbPutString` of VAL also writes UDF = 0 (dbStaticLib.c:2653-2660) and
/// nothing else: the record has not processed, so it keeps dbCommon's
/// initial UDF / INVALID.
#[epics_macros_rs::epics_test]
async fn a_restored_value_keeps_the_unprocessed_alarm() {
    let read = booted("record(ao, \"A\") { }\n", 1, "A 5\n", |db| {
        (
            num(db, "A.VAL"),
            num(db, "A.UDF"),
            num(db, "A.STAT"),
            num(db, "A.SEVR"),
        )
    })
    .await;
    assert_eq!(read, (5.0, 0.0, 17.0, 3.0));
}

/// The rontec `SetMode` shape: an mbbo with states.
#[epics_macros_rs::epics_test]
async fn a_restored_mbbo_keeps_the_unprocessed_alarm() {
    let read = booted(
        "record(mbbo, \"M\") { field(ZRVL, \"0\") field(ZRST, \"Off\") field(ONVL, \"1\") field(ONST, \"On\") }\n",
        1,
        "M 1\n",
        |db| {
            (
                num(db, "M.VAL"),
                num(db, "M.UDF"),
                num(db, "M.STAT"),
                num(db, "M.SEVR"),
            )
        },
    )
    .await;
    assert_eq!(read, (1.0, 0.0, 17.0, 3.0));
}

/// histogramRecord's `special()` recomputes WDTH from ULIM; the static put
/// stores ULIM alone, so WDTH keeps what init_record derived.
#[epics_macros_rs::epics_test]
async fn a_restored_field_runs_no_special() {
    let read = booted(
        "record(histogram, \"H\") { field(NELM, \"10\") field(LLIM, \"0\") field(ULIM, \"10\") }\n",
        1,
        "H.ULIM 20\n",
        |db| (num(db, "H.ULIM"), num(db, "H.WDTH")),
    )
    .await;
    assert_eq!(read, (20.0, 1.0));
}

/// "Can't restore link field (%s) in pass 1." (dbrestore.c:280).
#[epics_macros_rs::epics_test]
async fn a_link_is_not_restored_in_pass_1() {
    let read = booted("record(ao, \"A\") { }\n", 1, "A.DOL B\n", |db| {
        text(db, "A.DOL")
    })
    .await;
    assert_eq!(read, "");
}

#[epics_macros_rs::epics_test]
async fn a_link_is_restored_in_pass_0() {
    let read = booted("record(ao, \"A\") { }\n", 0, "A.DOL B\n", |db| {
        text(db, "A.DOL")
    })
    .await;
    assert_eq!(read, "B NPP NMS");
}

/// "No array write in pass 0." (dbrestore.c:723).
#[epics_macros_rs::epics_test]
async fn an_array_is_not_restored_in_pass_0() {
    let read = booted(
        "record(waveform, \"W\") { field(NELM, \"4\") field(FTVL, \"DOUBLE\") }\n",
        0,
        "W @array@ { \"1\" \"2\" }\n",
        |db| num(db, "W.NORD"),
    )
    .await;
    assert_eq!(read, 0.0);
}

#[epics_macros_rs::epics_test]
async fn an_array_is_restored_in_pass_1() {
    let read = booted(
        "record(waveform, \"W\") { field(NELM, \"4\") field(FTVL, \"DOUBLE\") }\n",
        1,
        "W @array@ { \"1\" \"2\" }\n",
        |db| (num(db, "W.NORD"), db.get_pv("W").unwrap()),
    )
    .await;
    assert_eq!(read.0, 2.0);
    assert_eq!(read.1, EpicsValue::DoubleArray(vec![1.0, 2.0].into()));
}

/// `scalar_restore`'s switch has no `DBF_DEVICE` arm (dbrestore.c:306-311).
#[epics_macros_rs::epics_test]
async fn a_dtyp_is_not_restored() {
    let read = booted(
        "record(ao, \"A\") { }\n",
        0,
        "A.DTYP Raw Soft Channel\n",
        |db| num(db, "A.DTYP"),
    )
    .await;
    assert_eq!(read, 0.0, "Soft Channel");
}

/// `dbPutMenuIndex` stores `0 <= index < nChoice` and refuses the rest
/// (dbStaticRun.c:556-557), where the `.db` loader lets `nChoice` and 65535
/// through. menuScan has 10 choices.
#[epics_macros_rs::epics_test]
async fn a_menu_index_outside_the_menu_is_not_restored() {
    let read = booted(
        "record(ao, \"A\") { field(SSCN, \"Event\") }\nrecord(ao, \"B\") { }\nrecord(ao, \"C\") { }\nrecord(ao, \"D\") { }\n",
        1,
        "A.SSCN 65535\nB.SCAN 10\nC.SCAN -1\nD.SCAN 9\n",
        |db| {
            (
                num(db, "A.SSCN"),
                num(db, "B.SCAN"),
                num(db, "C.SCAN"),
                num(db, "D.SCAN"),
            )
        },
    )
    .await;
    assert_eq!(read, (1.0, 0.0, 0.0, 9.0));
}

/// The same bound on every `DBF_MENU` field, menuAlarmSevr's 4 and menuPini's
/// 6 choices as menuScan's 10; an index inside each menu is stored.
#[epics_macros_rs::epics_test]
async fn every_menu_field_keeps_the_bound() {
    let read = booted(
        "record(ao, \"A\") { field(DISS, \"MINOR\") field(PINI, \"YES\") }\nrecord(ao, \"B\") { }\n",
        1,
        "A.DISS 4\nA.PINI 7\nB.DISS 3\nB.PINI 2\n",
        |db| {
            (
                num(db, "A.DISS"),
                num(db, "A.PINI"),
                num(db, "B.DISS"),
                num(db, "B.PINI"),
            )
        },
    )
    .await;
    assert_eq!(read, (1.0, 1.0, 3.0, 2.0));
}
