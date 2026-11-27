//! Each `{calc:…}` input is its own link whose `precord` is the reading
//! record (`lnkCalc.c:458`), read with `dbGetLink` into `clink->arg[i]`
//! (`:585`, `:665`): what the read raises lands on that record, and a failed
//! read raises `setLinkAlarm` with the name of a link that is no field
//! (`dbLink.c:66`) and leaves `arg[i]` as it was, while the evaluation goes
//! on. The nested-calc expectations are what softIoc (8f5015b66) printed for
//! `dbpf` / `dbgf` on these records; pvxs 1.5.3 cannot open a `{pva:…}`
//! input ("DBLINK* corrupt"), so the PVA inputs follow `dbGetLink`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::server::database::{LinkRef, LinkSet, ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "N1") { field(INP, "{calc:{expr:'A', args:[{calc:{expr:'5', major:'1'}}]}}") }
record(ai, "N2") { field(INP, "{calc:{expr:'A', args:[{calc:{expr:'5', minor:'1'}}], major:'A>4'}}") }
record(ai, "N3") { field(INP, "{calc:{expr:'A', args:[{calc:{expr:'5', major:'1'}}], minor:'A>4'}}") }
record(ai, "N4") { field(INP, "{calc:{expr:'A+B', args:[{calc:{expr:'5', minor:'1'}}, {calc:{expr:'1', major:'1'}}]}}") }
record(ai, "F1") { field(INP, "{calc:{expr:'A+B+VAL', args:[{pva:'REMOTE:A'}, 2]}}") }
record(ao, "O1") { field(OUT, "{calc:{expr:'A+VAL', out:{state:'s1'}, args:[{calc:{expr:'5', major:'1'}}]}}") }
record(ao, "O2") { field(OUT, "{calc:{expr:'A+VAL', out:{state:'s2'}, major:'VAL>100', args:[{calc:{expr:'5', minor:'1'}}]}}") }
record(ao, "O3") { field(OUT, "{calc:{expr:'A+VAL', out:{state:'s3'}, args:[{pva:'REMOTE:A'}]}}") }
record(ai, "R1") { field(INP, "{state:'s1'}") }
record(ai, "R3") { field(INP, "{state:'s3'}") }
"#;

/// A PVA server that answers `REMOTE:A` with 7 until `down` is set.
struct Remote {
    down: AtomicBool,
}

#[epics_base_rs::async_trait]
impl LinkSet for Remote {
    fn is_connected(&self, link: LinkRef<'_>) -> bool {
        let name = link.name;
        name == b"REMOTE:A" && !self.down.load(Ordering::Relaxed)
    }
    fn get_cached_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        self.is_connected(link).then_some(EpicsValue::Double(7.0))
    }
    async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        self.get_cached_value(link)
    }
}

/// (VAL, SEVR, STAT, AMSG) as the record now holds them.
fn state(db: &PvDatabase, rec: &str) -> (f64, AlarmSeverity, u16, String) {
    let cell = db.get_record(rec).unwrap();
    let inst = cell.read();
    (
        db.get_pv(rec).unwrap().to_f64().unwrap(),
        inst.common.sevr,
        inst.common.stat,
        String::from(&inst.common.amsg),
    )
}

async fn process(db: &PvDatabase, rec: &str) -> (f64, AlarmSeverity, u16, String) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
    state(db, rec)
}

async fn dbpf(db: &PvDatabase, rec: &str, value: f64) -> (f64, AlarmSeverity, u16, String) {
    db.put_pv(rec, EpicsValue::Double(value)).await.unwrap();
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
    state(db, rec)
}

async fn ioc() -> (Arc<PvDatabase>, Arc<Remote>) {
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    let remote = Arc::new(Remote {
        down: AtomicBool::new(false),
    });
    db.register_link_set("pva", remote.clone()).await;
    (db, remote)
}

const LINK: u16 = 14;

#[epics_macros_rs::epics_test]
async fn a_nested_calc_input_raises_its_alarm_on_the_reader() {
    let (db, _) = ioc().await;
    let major = |val| {
        (
            val,
            AlarmSeverity::Major,
            LINK,
            "post_major error".to_string(),
        )
    };
    assert_eq!(process(&db, "N1").await, major(5.0));
    assert_eq!(process(&db, "N2").await, major(5.0));
    assert_eq!(process(&db, "N3").await, major(5.0));
    assert_eq!(process(&db, "N4").await, major(6.0));
}

#[epics_macros_rs::epics_test]
async fn a_failed_input_keeps_its_last_value_and_raises_link_invalid() {
    let (db, remote) = ioc().await;
    assert_eq!(
        process(&db, "F1").await,
        (9.0, AlarmSeverity::NoAlarm, 0, String::new())
    );
    // A keeps 7 and the link's own VAL is 9: a zeroed A would give 11, and
    // an evaluation that stopped would leave 9.
    remote.down.store(true, Ordering::Relaxed);
    assert_eq!(
        process(&db, "F1").await,
        (18.0, AlarmSeverity::Invalid, LINK, "field ????".into())
    );
}

#[epics_macros_rs::epics_test]
async fn an_output_calc_link_raises_its_inputs_alarms_and_still_puts() {
    let (db, remote) = ioc().await;
    assert_eq!(
        dbpf(&db, "O1", 1.0).await,
        (1.0, AlarmSeverity::Major, LINK, "post_major error".into())
    );
    assert_eq!(process(&db, "R1").await.0, 1.0);
    assert_eq!(
        dbpf(&db, "O2", 1.0).await,
        (1.0, AlarmSeverity::Minor, LINK, "post_minor error".into())
    );
    remote.down.store(true, Ordering::Relaxed);
    assert_eq!(
        dbpf(&db, "O3", 1.0).await,
        (1.0, AlarmSeverity::Invalid, LINK, "field ????".into())
    );
    assert_eq!(process(&db, "R3").await.0, 1.0);
}
