//! The compiled `SIS38XX_SNL.st` on `SIS38XX_soft.db`, started as
//! `seq SIS38XX_SNL, "P=SIS:,R=mca,NUM_SIGNALS=<n>,FIELD=READ"`. The reads,
//! puts and lines each case checks are the ones a C IOC (seq 2.2.9,
//! `mcaRecord.c`, `devMCA_soft.c`, busy, asyn) gave for the same db, macros
//! and puts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::{self, BlockingBridge};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;

/// `mcaSupport.dbd`'s `device(mca,CONSTANT,devMCA_soft,"Soft Channel")`.
static SOFT_MCA: DevSup = DevSup {
    name: "devMCA_soft",
    choice: "Soft Channel",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], false),
};

const DEBUG_CHILD: &str = "SIS38XX_SNL_DEBUG_CHILD";

struct Ioc {
    db: Arc<PvDatabase>,
    _scan: ScanOwner,
}

/// `SIS38XX_soft.db` with `P=SIS:,R=mca` past `iocInit`, then the program
/// with `NUM_SIGNALS=num_signals`, started by the shell's `seq`.
async fn boot(num_signals: &str) -> Ioc {
    mca_rs::register_sis38xx_dbd();
    let (mca, mca_factory) = mca_rs::mca_record_factory();
    let (asyn, asyn_factory) = asyn_rs::asyn_record::asyn_record_factory();
    let macros = HashMap::from([("P".into(), "SIS:".into()), ("R".into(), "mca".into())]);
    let (db, _) = IocBuilder::new()
        .register_record_type(mca, mca_factory)
        .register_record_type(asyn, asyn_factory)
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_device_support("mca", &SOFT_MCA, || Box::new(mca_rs::SoftMca))
        .db_string(include_str!("SIS38XX_soft.db"), &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let scan = ScanOwner::start(db.clone());
    let seq = format!(r#"seq SIS38XX_SNL, "P=SIS:,R=mca,NUM_SIGNALS={num_signals},FIELD=READ""#);
    let shell_db = db.clone();
    let bridge = BlockingBridge::capture();
    let result = task::spawn_blocking(move || IocShell::new(shell_db, bridge).execute_line(&seq))
        .await
        .unwrap();
    match result {
        Ok(CommandOutcome::Continue) => {}
        Ok(_) => panic!("seq: failed"),
        Err(e) => panic!("seq: {e}"),
    }
    Ioc { db, _scan: scan }
}

/// Poll `probe` for up to ten seconds.
async fn eventually(mut probe: impl FnMut() -> bool) -> bool {
    for _ in 0..1000 {
        if probe() {
            return true;
        }
        task::sleep(Duration::from_millis(10)).await;
    }
    false
}

fn value(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// How many times each mca record has processed.
fn reads(db: &PvDatabase) -> [f64; 2] {
    [value(db, "SIS:mca1:reads"), value(db, "SIS:mca2:reads")]
}

/// A `caput` of `v` to `record.field`, then a quarter second for the
/// program to answer.
async fn put(db: &PvDatabase, record: &str, field: &str, v: u16) {
    db.put_record_field_from_ca(record, field, EpicsValue::Enum(v))
        .await
        .unwrap();
    task::sleep(Duration::from_millis(250)).await;
}

/// Once every channel connects the program sets SNL_Connected and, for the
/// first HardwareAcquiring monitor (Done), reads each array and clears
/// Acquiring. HardwareAcquiring going to Acquiring reads nothing; going back
/// to Done reads each array again and clears Acquiring.
#[epics_base_rs::epics_test]
async fn a_hardware_stop_reads_every_array_and_clears_acquiring() {
    let ioc = boot("2").await;
    let db = &ioc.db;
    assert!(eventually(|| value(db, "SIS:SNL_Connected") == 1.0).await);
    assert!(eventually(|| reads(db) == [1.0, 1.0]).await);

    put(db, "SIS:Acquiring", "VAL", 1).await;
    put(db, "SIS:HardwareAcquiring", "VAL", 1).await;
    assert_eq!(reads(db), [1.0, 1.0]);
    assert_eq!(value(db, "SIS:Acquiring"), 1.0);

    put(db, "SIS:HardwareAcquiring", "VAL", 0).await;
    assert!(eventually(|| reads(db) == [2.0, 2.0]).await);
    assert!(eventually(|| value(db, "SIS:Acquiring") == 0.0).await);
}

/// DoReadAll set to Read reads each array and is put back to Done; set to
/// Done it reads nothing.
#[epics_base_rs::epics_test]
async fn do_read_all_reads_every_array_and_resets() {
    let ioc = boot("2").await;
    let db = &ioc.db;
    assert!(eventually(|| reads(db) == [1.0, 1.0]).await);

    put(db, "SIS:DoReadAll", "VAL", 1).await;
    assert!(eventually(|| reads(db) == [2.0, 2.0]).await);
    assert!(eventually(|| value(db, "SIS:DoReadAll") == 0.0).await);

    put(db, "SIS:DoReadAll", "VAL", 0).await;
    assert_eq!(reads(db), [2.0, 2.0]);
}

/// With an array record missing the program waits for it, so it never
/// sets SNL_Connected and answers neither trigger.
#[epics_base_rs::epics_test]
async fn a_missing_array_record_holds_the_program() {
    let ioc = boot("3").await;
    let db = &ioc.db;
    task::sleep(Duration::from_millis(500)).await;
    put(db, "SIS:DoReadAll", "VAL", 1).await;
    put(db, "SIS:HardwareAcquiring", "VAL", 1).await;
    put(db, "SIS:HardwareAcquiring", "VAL", 0).await;
    assert_eq!(value(db, "SIS:SNL_Connected"), 0.0);
    assert_eq!(reads(db), [0.0, 0.0]);
    assert_eq!(value(db, "SIS:DoReadAll"), 1.0);
}

/// The lines the program prints: its connection, and with Asyn.TB1 set,
/// each trigger it takes; for an illegal NUM_SIGNALS, why it stops.
#[epics_base_rs::epics_test]
async fn the_program_prints_what_c_prints() {
    let Some(case) = std::env::var_os(DEBUG_CHILD) else {
        for (case, lines) in [
            (
                "2",
                &[
                    "SIS: All channels connected.",
                    "SIS.st: Read array data",
                    "SIS.st: HardwareAcquiringMon, HardwareAcquiring=1",
                    "SIS.st: HardwareAcquiringMon, HardwareAcquiring=0",
                ][..],
            ),
            (
                "0",
                &[
                    "NUM_SIGNALS is illegal.",
                    ">>>>>>>>>>>>>> SIS is dead. <<<<<<<<<<<<<<",
                ],
            ),
            (
                "33",
                &[
                    "NUM_SIGNALS is illegal.",
                    ">>>>>>>>>>>>>> SIS is dead. <<<<<<<<<<<<<<",
                ],
            ),
        ] {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "the_program_prints_what_c_prints",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(DEBUG_CHILD, case)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(
                out.status.success(),
                "{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let printed: Vec<_> = stdout
                .lines()
                .map(|l| l.trim_start_matches("test the_program_prints_what_c_prints ... "))
                .filter(|l| l.starts_with("SIS") || l.contains("NUM_SIGNALS") || l.contains(">>>"))
                .collect();
            assert_eq!(printed, lines, "NUM_SIGNALS={case}\n{stdout}");
        }
        return;
    };
    let case = case.into_string().unwrap();
    let ioc = boot(&case).await;
    let db = &ioc.db;
    if case != "2" {
        task::sleep(Duration::from_millis(500)).await;
        assert_eq!(reads(db), [0.0, 0.0]);
        return;
    }
    assert!(eventually(|| reads(db) == [1.0, 1.0]).await);
    put(db, "SIS:Asyn", "TB1", 1).await;
    put(db, "SIS:DoReadAll", "VAL", 1).await;
    assert!(eventually(|| reads(db) == [2.0, 2.0]).await);
    put(db, "SIS:HardwareAcquiring", "VAL", 1).await;
    put(db, "SIS:HardwareAcquiring", "VAL", 0).await;
    assert!(eventually(|| reads(db) == [3.0, 3.0]).await);
}
