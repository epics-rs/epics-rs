//! A panic three hops into a FLNK chain, caught on the processing thread,
//! leaves every record processable: `HopChain`'s unwind ends each parked
//! cycle as unwinding its frame would have, PACT included, at every switch
//! between recursing and parked targets (`hop_rule::sweep`).

mod hop_rule;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;

static BOOM: AtomicBool = AtomicBool::new(true);

fn run(db: &PvDatabase, name: &str) {
    let mut visited = ProcStack::new();
    let mut f = db.process_record_with_links(name, &mut visited);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(r) => r.expect("processes"),
        Poll::Pending => panic!("pending"),
    }
}

fn val(db: &PvDatabase, name: &str) -> f64 {
    db.get_pv(name).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_panic_mid_chain_leaves_the_chain_processable() {
    hop_rule::sweep(|rule| async move {
        BOOM.store(true, Ordering::SeqCst);
        let text = r#"
record(calc, "H") { field(CALC, "VAL+1") field(FLNK, "A") }
record(calc, "A") { field(CALC, "VAL+1") field(FLNK, "B") }
record(calc, "B") { field(CALC, "VAL+1") field(FLNK, "P") }
record(sub, "P") { field(SNAM, "boom") field(FLNK, "Z") }
record(calc, "Z") { field(CALC, "VAL+1") }
"#;
        let db = IocBuilder::new()
            .register_subroutine("boom", |_| {
                if BOOM.load(Ordering::SeqCst) {
                    panic!("boom");
                }
                Ok(0)
            })
            .db_string(text, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap()
            .0;
        db.ioc_init().await;
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&db, "H")));
        let msg = unwound.unwrap_err();
        let msg = msg
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| msg.downcast_ref::<String>().cloned());
        let pact: Vec<f64> = ["H", "A", "B", "P", "Z"]
            .iter()
            .map(|n| val(&db, &format!("{n}.PACT")))
            .collect();
        assert_eq!(pact, [0.0; 5], "{rule}: no PACT held past the unwind");
        assert_eq!(msg.as_deref(), Some("boom"), "{rule}");
        assert_eq!(
            (val(&db, "H"), val(&db, "A"), val(&db, "B"), val(&db, "Z")),
            (1.0, 1.0, 1.0, 0.0),
            "{rule}"
        );
        BOOM.store(false, Ordering::SeqCst);
        run(&db, "H");
        assert_eq!(
            (val(&db, "H"), val(&db, "A"), val(&db, "B"), val(&db, "Z")),
            (2.0, 2.0, 2.0, 1.0),
            "{rule}"
        );
        run(&db, "A");
        assert_eq!(
            (val(&db, "A"), val(&db, "B"), val(&db, "Z")),
            (3.0, 3.0, 2.0),
            "{rule}"
        );
    })
    .await;
}
