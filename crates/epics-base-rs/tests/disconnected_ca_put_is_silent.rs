//! A put to a CA link that is down says nothing; the record's LINK alarm is the
//! whole report. Ground truth, softIoc R7.0.10 with
//! `record(ao, "R1") { field(OUT, "NOSUCH:R2.PROC CA") }` and `dbpf R1 1`:
//! no output at all, `STAT: LINK  SEVR: INVALID  AMSG: field OUT`. C refuses
//! at staging with a silent `-1` (`dbCa.c:558-560`) and skips a queued put
//! whose channel dropped since (`dbCa.c:1224`); only a write to a connected
//! channel that fails reaches `errlogPrintf` (`dbCa.c:1241`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::runtime::log::errlog_flush;
use epics_base_rs::server::database::{LinkPutOp, LinkSet, ProcStack, PvDatabase};
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
    fn is_connected(&self, _: &[u8]) -> bool {
        self.up.load(Ordering::SeqCst)
    }
    fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, _: &[u8], _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        if self.drops {
            self.up.store(false, Ordering::SeqCst);
            return Err("channel disconnected".into());
        }
        Err("write access denied".into())
    }
}

const CHILD: &str = "DISCONNECTED_CA_PUT_CHILD";

/// Run the `child_*` test `name` in a fresh process and return its stderr's
/// lines about `R2`: the console the operator sees, errlog and raw prints
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
        .filter(|l| l.contains("R2"))
        .map(str::to_string)
        .collect()
}

/// Process `R1` (OUT `ca://R2.PROC`) once, drain the put queue and the
/// errlog, and return the record's alarm.
async fn put_once(lset: Lset) -> (u16, AlarmSeverity) {
    let db = PvDatabase::new();
    db.register_link_set("ca", Arc::new(lset)).await;
    db.process_cp_links_on_connect().await;
    db.add_record("R1", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("R1").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("OUT", EpicsValue::String("ca://R2.PROC".into()))
            .unwrap();
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

#[test]
fn a_failed_write_on_a_live_link_reaches_the_errlog() {
    assert_eq!(
        console_of("child_live"),
        vec!["dbCa: external link put to 'R2.PROC' failed: write access denied".to_string()]
    );
}
