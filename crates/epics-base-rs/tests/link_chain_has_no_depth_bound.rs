//! A FLNK chain of any length runs to its end, exactly as under C.
//!
//! C has no depth counter: `dbProcess` (`dbAccess.c:485`) recurses through
//! `processTarget` (`dbDbLink.c:427-436`), which marks the source `pact` and
//! calls `dbProcess` on the target, and the only thing that stops a cascade is
//! a record that is already `pact`. The port refused the 17th hop with a
//! `MAX_LINK_DEPTH` of 16, which left synApps' `scaler32.db` — `scaler` ->
//! `_cts1..8` -> `_calc1..8`, seventeen records deep — with its last `calc`
//! never processed. The bound is gone; the `visited` cycle guard, the
//! synchronous half of C's PACT test, is the only refusal left.
//!
//! The chain runs on a 16 MB thread because each hop is one poll frame of the
//! `process_record_with_links_inner` future, and on linux-arm64 a debug-build
//! frame is large enough that a few dozen of them overflow the default 2 MB
//! test-thread stack.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::types::EpicsValue;

/// Past the former 16-hop bound and past the 32-deep chain the CA stack
/// measurements use, so neither number is what makes this pass.
const CHAIN: usize = 40;

/// `L0 -> L1 -> ... -> L39`, each `calc` computing `INPA + 1` from the constant
/// `i`, so a processed `L{i}` holds `i + 1` and an unprocessed one holds 0.
async fn chain_db() -> Arc<PvDatabase> {
    let mut db_text = String::new();
    for i in 0..CHAIN {
        let flnk = if i + 1 < CHAIN {
            format!("field(FLNK,\"L{}\")", i + 1)
        } else {
            String::new()
        };
        db_text.push_str(&format!(
            "record(calc, \"L{i}\") {{ field(CALC,\"A+1\") field(INPA,\"{i}\") {flnk} }}\n"
        ));
    }
    IocBuilder::new()
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

#[test]
fn a_forty_deep_flnk_chain_processes_every_record() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            epics_base_rs::runtime::task::test_block_on(async {
                let db = chain_db().await;
                let mut visited = HashSet::new();
                db.process_record_with_links("L0", &mut visited, 0)
                    .await
                    .expect("the head processes");
                assert!(visited.is_empty(), "the frame unwound: {visited:?}");

                for i in 0..CHAIN {
                    let rec = db.get_record(&format!("L{i}")).expect("record exists");
                    let inst = rec.read();
                    assert_eq!(
                        inst.record.get_field("VAL"),
                        Some(EpicsValue::Double((i + 1) as f64)),
                        "L{i} must have processed"
                    );
                    assert_ne!(
                        inst.common.stat,
                        alarm_status::SCAN_ALARM,
                        "L{i} carries a refusal: {:?}",
                        inst.common.amsg
                    );
                }
            });
        })
        .unwrap()
        .join()
        .unwrap();
}
