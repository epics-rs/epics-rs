//! A PV-name put made while the link's previous name is still connecting
//! waits for that connect, at most `sscanRecordConnectWaitSeconds` (1 s).

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Closed, it holds every read of `G`'s fields, so a link's connect to
/// `G` (which reads its metadata) lands only once it opens.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn set_closed(&self, closed: bool) {
        let (state, wake) = &*self.0;
        *state.lock().unwrap() = closed;
        wake.notify_all();
    }

    fn pass(&self) {
        let (state, wake) = &*self.0;
        drop(
            wake.wait_while(state.lock().unwrap(), |closed| *closed)
                .unwrap(),
        );
    }
}

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

struct Gated(Gate);

impl Record for Gated {
    fn record_type(&self) -> &'static str {
        "gated"
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        self.0.pass();
        (name == "VAL").then_some(EpicsValue::Double(0.0))
    }
    fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
        Err(CaError::FieldNotFound(name.into()))
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

async fn ioc(gate: &Gate) -> Arc<PvDatabase> {
    let (name, factory) = sscan_rs::sscan_record_factory();
    let gate = gate.clone();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_record_type("gated", move || Box::new(Gated(gate.clone())))
        .db_string(
            r#"
record(gated, "G") {}
record(calc, "E") { field(CALC, "2") }
record(sscan, "S") { field(MPTS, "10") field(NPTS, "5") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn put_t1pv(db: &PvDatabase, pv: &str) {
    db.put_record_field_from_ca("S", "T1PV", EpicsValue::String(pv.into()))
        .await
        .unwrap();
}

async fn until_t1_connects(db: &PvDatabase) {
    let deadline = epics_libcom_rs::runtime::time::deadline_from_now(fact_budget::FACT_BUDGET);
    while db.get_pv("S.T1NV").unwrap().to_f64().unwrap() != 0.0 {
        assert!(Instant::now() < deadline, "S.T1NV never reached PV OK");
        sleep(Duration::from_millis(5)).await;
    }
}

#[epics_base_rs::epics_test]
async fn a_trigger_name_put_during_a_connect_returns_at_the_connect() {
    let gate = Gate::default();
    let db = ioc(&gate).await;
    gate.set_closed(true);
    put_t1pv(&db, "G.VAL").await;
    let opener = gate.clone();
    let open = std::thread::spawn(move || {
        // not-a-wait: pacing, so G's connect lands after the second put
        // has found it in progress and started waiting.
        std::thread::sleep(Duration::from_millis(100));
        opener.set_closed(false);
    });
    let t0 = Instant::now();
    put_t1pv(&db, "E.PROC").await;
    let took = t0.elapsed();
    open.join().unwrap();
    // C sleeps out a whole second once it finds the connect in progress.
    assert!(
        took < Duration::from_secs(1),
        "the second T1PV put took {took:?}"
    );
    until_t1_connects(&db).await;
}

#[epics_base_rs::epics_test]
async fn a_trigger_name_put_after_a_name_nobody_serves_waits_out_the_bound() {
    let db = ioc(&Gate::default()).await;
    put_t1pv(&db, "nosuch:pv").await;
    let t0 = Instant::now();
    put_t1pv(&db, "E.PROC").await;
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_secs(1),
        "the put skipped sscanRecordConnectWaitSeconds: {took:?}"
    );
    until_t1_connects(&db).await;
}
