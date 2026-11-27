//! lsi `monitor()` (lsiRecord.c:206-210) posts VAL with DBE_VALUE|DBE_LOG
//! only when LEN or the bytes changed; MPST OnChange adds nothing else.

use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use std::collections::HashMap;

fn drain(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

#[epics_macros_rs::epics_test]
async fn lsi_posts_val_only_when_it_changes() {
    let db = IocBuilder::new()
        .db_string(
            r#"
record(stringin, "S") {}
record(lsi, "L") { field(INP, "S") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut rx = {
        let inst = db.get_record("L").unwrap();
        let mut g = inst.write();
        g.add_subscriber(
            "VAL",
            1,
            DbFieldType::Char,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .unwrap()
    };
    drain(&mut rx);
    for (source, want) in [(Some("a"), 1), (None, 0), (Some("a"), 0), (Some("bc"), 1)] {
        if let Some(s) = source {
            db.put_record_field_from_ca("S", "VAL", EpicsValue::String(s.into()))
                .await
                .unwrap();
        }
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links("L", &mut visited)
            .await
            .unwrap();
        assert_eq!(drain(&mut rx), want, "VAL events after reading {source:?}");
    }
}
