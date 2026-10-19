//! The work a record's `special(field, true)` leaves for after the gate runs
//! once per put, after the gate drops, on every put entry; and an OUT put
//! runs `special("OUT", true)` once.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AfterGate, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{EpicsValue, PvString};

struct OutProbe {
    specials: Arc<AtomicUsize>,
    runs: Arc<AtomicUsize>,
    owed: Option<AfterGate>,
}

impl Record for OutProbe {
    fn record_type(&self) -> &'static str {
        "after_gate_probe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(0.0)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
        // OUT is dbCommon's here, so its put reaches the common writer.
        match name {
            "VAL" => Ok(()),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
    fn special(
        &mut self,
        field: &str,
        after: bool,
        _common: &epics_base_rs::server::record::CommonFields,
    ) -> CaResult<()> {
        if field == "OUT" && after {
            self.specials.fetch_add(1, Ordering::SeqCst);
            let runs = Arc::clone(&self.runs);
            self.owed = Some(AfterGate::new(move |db: &PvDatabase| {
                // Taking P's lock here deadlocks if the put's gate is still up.
                drop(db.lock_records(["P"]));
                runs.fetch_add(1, Ordering::SeqCst);
            }));
        }
        Ok(())
    }
    fn take_after_gate(&mut self) -> Option<AfterGate> {
        self.owed.take()
    }
}

fn out(text: &str) -> EpicsValue {
    EpicsValue::String(PvString::from(text))
}

#[epics_macros_rs::epics_test]
async fn an_out_put_runs_special_once_and_its_work_once_after_the_gate() {
    let specials = Arc::new(AtomicUsize::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "P",
        Box::new(OutProbe {
            specials: Arc::clone(&specials),
            runs: Arc::clone(&runs),
            owed: None,
        }),
    )
    .await
    .unwrap();
    let counts = || (specials.load(Ordering::SeqCst), runs.load(Ordering::SeqCst));

    db.put_record_field_from_ca("P", "OUT", out("A"))
        .await
        .unwrap();
    assert_eq!(counts(), (1, 1), "put_record_field_from_ca");

    db.put_record_field_from_ca_no_notify("P", "OUT", out("B"))
        .await
        .unwrap();
    assert_eq!(counts(), (2, 2), "put_record_field_from_ca_no_notify");

    db.put_pv_no_process("P.OUT", out("C")).await.unwrap();
    assert_eq!(counts(), (3, 3), "put_pv_no_process");

    db.put_pv_and_post("P.OUT", out("D")).await.unwrap();
    assert_eq!(counts(), (4, 4), "put_pv_and_post");

    let held = db.lock_for_put("P", "OUT", &out("E"));
    db.put_record_field_from_ca_no_notify_already_locked(held, out("E"))
        .unwrap();
    assert_eq!(
        counts(),
        (5, 5),
        "put_record_field_from_ca_no_notify_already_locked"
    );

    let held = db.lock_for_put("P", "OUT", &out("F"));
    db.put_pv_already_locked(held, out("F")).unwrap();
    assert_eq!(counts(), (6, 6), "put_pv_already_locked");

    db.put_many_locked(&["P"], 2, |puts| {
        let p = puts.record(0).unwrap();
        puts.put_record_field_from_ca_no_notify(p, "OUT", out("G"))
            .unwrap();
        puts.put_pv(p, "OUT", out("H")).unwrap();
        assert_eq!(counts(), (8, 6), "the work waits for the gate");
    });
    assert_eq!(counts(), (8, 8), "put_many_locked runs it after");
}
