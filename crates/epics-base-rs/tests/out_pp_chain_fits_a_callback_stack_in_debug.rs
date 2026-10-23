//! A 20000-deep OUT PP chain, C's depth, fits the stack a `cbLow` thread
//! gets (`StackSizeClass::Big`, `callback_executor.rs`), debug build
//! included.
//!
//! Puts recurse down to the thread's hop margin; below it each hop parks
//! after its put and the chain's driver takes the put's `processTarget`
//! (`drive_hops`). Recursing every hop, the chain overflowed `cbLow` 700
//! records in (release).

use std::collections::HashMap;

use epics_base_rs::runtime::task::StackSizeClass;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const CHAIN: usize = 20000;

#[test]
fn a_twenty_thousand_deep_out_pp_chain_runs_on_a_callback_sized_stack() {
    std::thread::Builder::new()
        .stack_size(StackSizeClass::Big.bytes())
        .spawn(|| {
            epics_base_rs::runtime::task::test_block_on(async {
                let mut db_text =
                    String::from("record(ao, \"L0\") { field(VAL,\"1\") field(OUT,\"L1 PP\") }\n");
                for i in 1..CHAIN {
                    let out = if i + 1 < CHAIN {
                        format!("field(OUT,\"L{} PP\")", i + 1)
                    } else {
                        String::new()
                    };
                    db_text.push_str(&format!("record(ao, \"L{i}\") {{ {out} }}\n"));
                }
                let db = IocBuilder::new()
                    .db_string(&db_text, &HashMap::new())
                    .unwrap()
                    .build()
                    .await
                    .unwrap()
                    .0;
                let mut visited = epics_base_rs::server::database::ProcStack::new();
                db.process_record_with_links("L0", &mut visited)
                    .await
                    .expect("the head processes");
                let last = db.get_record(format!("L{}", CHAIN - 1)).unwrap();
                assert_eq!(
                    last.read().record.get_field("VAL"),
                    Some(EpicsValue::Double(1.0))
                );
            });
        })
        .unwrap()
        .join()
        .unwrap();
}
