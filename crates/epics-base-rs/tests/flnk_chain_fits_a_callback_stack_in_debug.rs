//! A FLNK chain as deep as C runs on `cbLow`, 20000 records, fits the stack
//! a `cbLow` thread gets (`StackSizeClass::Big`, `callback_executor.rs`),
//! debug build included.
//!
//! `#[inline(always)]` in `processing.rs` folded every helper's locals into
//! `run_process_frame`, about 100 KB per hop unoptimised, so a debug
//! `ad-simdetector-ioc` overflowed `cbLow` on `cam1:Acquire=1`. Release
//! frames are unchanged by keeping the attribute release-only.
//!
//! Targets recurse down to the thread's hop margin; below it each hop's
//! cycle parks at its FLNK and the chain's driver runs the target
//! (`drive_hops`), so the rest of the chain costs no stack. Recursing every
//! hop, a chain overflowed `cbLow` 400 records in (debug) and 2500 in
//! (release).

use std::collections::HashMap;

use epics_base_rs::runtime::task::StackSizeClass;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const CHAIN: usize = 20000;

#[test]
fn a_twenty_thousand_deep_flnk_chain_runs_on_a_callback_sized_stack() {
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
