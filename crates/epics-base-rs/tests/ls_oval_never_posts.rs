//! lso/lsi `monitor()` copies VAL into OVAL and posts only VAL and LEN
//! (lsoRecord.c:246-265, lsiRecord.c:204-223); nothing posts OVAL or OLEN.
//! softIoc R7.0.10, `camonitor LS.OVAL LS.OVAL$ LI.OVAL LI.OVAL$` across
//! `caput LS abc`, `caput LS abd`, `caput LS.PROC 1`, `caput LI abc`,
//! `caput LI.PROC 1`, and the same with `LS.OLEN LI.OLEN`: the connect events
//! and nothing after them, while SEVR still posts the first process's UDF
//! clear.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

#[epics_macros_rs::epics_test]
async fn a_process_never_posts_oval() {
    let db = IocBuilder::new()
        .db_string(
            r#"
record(lso, "LS") { field(SIZV, "64") }
record(lsi, "LI") { field(SIZV, "64") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    for rec in ["LS", "LI"] {
        let mut sevr = {
            let inst = db.get_record(rec).unwrap();
            let mut g = inst.write();
            g.add_subscriber("SEVR", 9, DbFieldType::Short, EventMask::VALUE.bits())
                .unwrap()
        };
        while sevr.try_recv().is_ok() {}
        let mut rxs: Vec<_> = [
            ("OVAL", DbFieldType::String),
            ("OVAL", DbFieldType::Char),
            ("OLEN", DbFieldType::Long),
        ]
        .into_iter()
        .enumerate()
        .map(|(sid, (f, t))| {
            let inst = db.get_record(rec).unwrap();
            let mut g = inst.write();
            g.add_subscriber(
                f,
                sid as u32 + 1,
                t,
                (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
            )
            .unwrap()
        })
        .collect();
        for rx in &mut rxs {
            while rx.try_recv().is_ok() {}
        }
        for (step, (field, v)) in [
            ("VAL", EpicsValue::String("abc".into())),
            ("VAL", EpicsValue::String("abd".into())),
            ("PROC", EpicsValue::Short(1)),
        ]
        .into_iter()
        .enumerate()
        {
            db.put_record_field_from_ca(rec, field, v).await.unwrap();
            epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
            for rx in &mut rxs {
                let mut got = Vec::new();
                while let Ok(e) = rx.try_recv() {
                    got.push(e.snapshot.value.clone());
                }
                assert_eq!(got, [], "{rec} OVAL/OLEN after the {field} put");
            }
            let mut sevr_posts = 0;
            while sevr.try_recv().is_ok() {
                sevr_posts += 1;
            }
            assert_eq!(
                sevr_posts,
                usize::from(step == 0),
                "{rec}.SEVR after the {field} put"
            );
        }
    }
}
