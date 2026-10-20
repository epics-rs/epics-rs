//! A 200-deep FLNK chain fits the stack a `cbLow` thread gets
//! (`StackSizeClass::Big`, `callback_executor.rs`), debug build included.
//!
//! `#[inline(always)]` in `processing.rs` folded every helper's locals into
//! `run_process_frame`, about 100 KB per hop unoptimised, so a debug
//! `ad-simdetector-ioc` overflowed `cbLow` on `cam1:Acquire=1`. Release
//! frames are unchanged by keeping the attribute release-only.
//!
//! The hop itself is taken after `process_cycle`'s frame is popped
//! (`run_cycle_tail`); holding it across the hop overflowed this chain
//! 130 records in (debug) and 600 in (release), where C runs 12000.

use std::collections::HashMap;

use epics_base_rs::runtime::task::StackSizeClass;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const CHAIN: usize = 200;

#[test]
fn a_two_hundred_deep_flnk_chain_runs_on_a_callback_sized_stack() {
    std::thread::Builder::new()
        .stack_size(StackSizeClass::Big.bytes())
        .spawn(|| {
            epics_base_rs::runtime::task::test_block_on(async {
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
                    Some(EpicsValue::Double(CHAIN as f64))
                );
            });
        })
        .unwrap()
        .join()
        .unwrap();
}
