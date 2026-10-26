//! A put to a CA link that is down says nothing; the record's LINK alarm is the
//! whole report. Ground truth, softIoc R7.0.10 with
//! `record(ao, "R1") { field(OUT, "NOSUCH:R2.PROC CA") }` and `dbpf R1 1`:
//! no output at all, `STAT: LINK  SEVR: INVALID  AMSG: field OUT`. C refuses
//! at staging with a silent `-1` (`dbCa.c:558-560`) and skips a queued put
//! whose channel dropped since (`dbCa.c:1224`); only a write to a connected
//! channel that `ca_array_put` refuses reaches `errlogPrintf`, from the CA
//! link set (`dbCa.c:1240-1243`). With `record(stringout, "R1") { field(OUT,
//! "R2.CALC CA") }` and `dbpf R1 "1+"` on a local calc R2, softIoc prints
//! `dbCaTask ca_array_put Channel write request failed` and
//! `R1 has DB CA link to R2.CALC`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::runtime::log::errlog_flush;
use epics_base_rs::server::database::{LinkPutOp, LinkRef, LinkSet, ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

/// A CA link set whose put fails, and which is connected or not as `up` says.
/// `drops` takes the link down inside the put: a disconnect that lands
/// between staging and the owner's write.
struct Lset {
    up: AtomicBool,
    drops: bool,
}

#[async_trait::async_trait]
impl LinkSet for Lset {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        self.up.load(Ordering::SeqCst)
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, _: LinkRef<'_>, _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        if self.drops {
            self.up.store(false, Ordering::SeqCst);
            return Err("channel disconnected".into());
        }
        Err("write access denied".into())
    }
}

const CHILD: &str = "DISCONNECTED_CA_PUT_CHILD";

/// Run the `child_*` test `name` in a fresh process and return its stderr's
/// link put reports: the console the operator sees, errlog and raw prints
/// alike, which an in-process listener cannot hear.
fn console_of(name: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{name}: {out:?}");
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.starts_with("dbCa") || l.contains("DB CA link"))
        .map(str::to_string)
        .collect()
}

/// Process `R1` (OUT `R2.PROC CA`) once, drain the put queue and the
/// errlog, and return the record's alarm.
async fn put_once(lset: Lset) -> (u16, AlarmSeverity) {
    let db = PvDatabase::new();
    // C `dbCaLinkInit`: no build starts the `dbCaLink` worker here.
    db.ca_link_init();
    db.register_link_set("ca", Arc::new(lset)).await;
    db.process_cp_links_on_connect().await;
    db.add_record("R1", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    {
        db.put_pv_no_process("R1.OUT", EpicsValue::String("R2.PROC CA".into()))
            .await
            .unwrap();
        let rec = db.get_record("R1").unwrap();
        let mut inst = rec.write();
        inst.common.udf = 0;
    }
    db.process_record_with_links("R1", &mut ProcStack::new())
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
    db.sync_external_link_puts().await;
    errlog_flush();
    let rec = db.get_record("R1").unwrap();
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

#[epics_macros_rs::epics_test]
async fn child_down() {
    if std::env::var_os(CHILD).is_some() {
        let alarm = put_once(Lset {
            up: AtomicBool::new(false),
            drops: false,
        })
        .await;
        assert_eq!(alarm, (alarm_status::LINK_ALARM, AlarmSeverity::Invalid));
    }
}

#[epics_macros_rs::epics_test]
async fn child_drops() {
    if std::env::var_os(CHILD).is_some() {
        put_once(Lset {
            up: AtomicBool::new(true),
            drops: true,
        })
        .await;
    }
}

#[epics_macros_rs::epics_test]
async fn child_live() {
    if std::env::var_os(CHILD).is_some() {
        put_once(Lset {
            up: AtomicBool::new(true),
            drops: false,
        })
        .await;
    }
}

#[test]
fn a_put_to_a_down_link_alarms_and_says_nothing() {
    assert_eq!(console_of("child_down"), Vec::<String>::new());
}

#[test]
fn a_link_that_drops_after_staging_says_nothing() {
    assert_eq!(console_of("child_drops"), Vec::<String>::new());
}

/// Process `R1` (OUT `R2.CALC CA`, a local calc) with VAL `1+`, which R2
/// refuses: as a plain put, or as `Async Soft Channel`'s put callback
/// (`dbPutLinkAsync`) when `callback`. A put-notify chain alone puts plain.
async fn put_local_refused(callback: bool) {
    let dtyp = if callback {
        r#"field(DTYP, "Async Soft Channel") "#
    } else {
        ""
    };
    let (db, _h) = IocBuilder::new()
        .db_string(
            &format!(
                r#"
record(calc, "R2") {{ }}
record(stringout, "R1") {{ {dtyp}field(OUT, "R2.CALC CA") }}
"#
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_external_link_opens().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    let (_, _, out) = db
        .record_link_fields("R1")
        .into_iter()
        .find(|(field, _, _)| *field == "OUT")
        .unwrap();
    let out = out.external().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while db.external_link_metadata(out).is_none() {
        assert!(std::time::Instant::now() < deadline, "R1.OUT connects");
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    db.put_pv_no_process("R1", EpicsValue::String("1+".into()))
        .await
        .unwrap();
    db.process_record_with_links("R1", &mut ProcStack::new())
        .await
        .unwrap();
    db.sync_external_link_puts().await;
    errlog_flush();
}

#[epics_macros_rs::epics_test]
async fn child_local_refused() {
    if std::env::var_os(CHILD).is_some() {
        put_local_refused(false).await;
    }
}

#[epics_macros_rs::epics_test]
async fn child_local_refused_callback() {
    if std::env::var_os(CHILD).is_some() {
        put_local_refused(true).await;
    }
}

/// The database reports no lset's failure for it: dbCa's report is the CA
/// link set's own.
#[test]
fn a_failed_write_the_lset_does_not_report_says_nothing() {
    assert_eq!(console_of("child_live"), Vec::<String>::new());
}

/// `putComplete` ignores the status (`dbCa.c:1056-1072`).
#[test]
fn a_put_callback_the_target_refuses_says_nothing() {
    assert_eq!(
        console_of("child_local_refused_callback"),
        Vec::<String>::new()
    );
}

#[test]
fn a_local_put_the_target_refuses_prints_dbcatasks_lines() {
    assert_eq!(
        console_of("child_local_refused"),
        [
            "dbCaTask ca_array_put Channel write request failed",
            "R1 has DB CA link to R2.CALC"
        ]
    );
}
