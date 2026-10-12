//! A VAL put to lso/lsi posts LEN from `special()` (lsoRecord.c:239,
//! lsiRecord.c:197) and again from `monitor()` when `len != olen` (:254,
//! :212). softIoc R7.0.10, `camonitor LS64.LEN LSI.LEN` across `caput abc`,
//! `caput abc`, `caput abcd`:
//!
//! ```text
//! LS64.LEN  4 UDF INVALID
//! LS64.LEN  4
//! LS64.LEN  4
//! LS64.LEN  5
//! LS64.LEN  5
//! ```
//!
//! and the same for LSI.LEN. The `special()` post precedes the process, so
//! the first one still carries the born UDF alarm.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

fn drain(rx: &mut EventReader) -> Vec<(EpicsValue, u16)> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push((e.value().as_value().cloned().unwrap(), e.sevr()));
    }
    out
}

#[epics_macros_rs::epics_test]
async fn val_put_posts_len_from_special_and_monitor() {
    let db = IocBuilder::new()
        .db_string(
            r#"
record(lso, "LS64") { field(SIZV, "64") }
record(lsi, "LSI") { field(SIZV, "64") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    const INVALID: u16 = 3;
    for rec in ["LS64", "LSI"] {
        let mut rx = {
            let inst = db.get_record(rec).unwrap();
            let mut g = inst.write();
            g.add_subscriber(
                "LEN",
                1,
                DbFieldType::Long,
                (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
            )
            .unwrap()
        };
        drain(&mut rx);
        let len = EpicsValue::ULong;
        let steps: [(&str, Vec<(EpicsValue, u16)>); 3] = [
            ("abc", vec![(len(4), INVALID), (len(4), 0)]),
            ("abc", vec![(len(4), 0)]),
            ("abcd", vec![(len(5), 0), (len(5), 0)]),
        ];
        for (v, want) in steps {
            db.put_record_field_from_ca(rec, "VAL", EpicsValue::String(v.into()))
                .await
                .unwrap();
            epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
            assert_eq!(drain(&mut rx), want, "{rec}.LEN after VAL={v}");
        }
    }
}

/// softIoc, an lsi reading a stringin: the INP read posts LEN from
/// `monitor()` alone, only when it moved (`2`, nothing, `4`).
#[epics_macros_rs::epics_test]
async fn inp_read_posts_len_from_monitor_alone() {
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
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let mut rx = {
        let inst = db.get_record("L").unwrap();
        let mut g = inst.write();
        g.add_subscriber(
            "LEN",
            1,
            DbFieldType::Long,
            (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
        )
        .unwrap()
    };
    drain(&mut rx);
    let steps = [
        (Some("a"), vec![(EpicsValue::ULong(2), 0)]),
        (None, vec![]),
        (Some("bcd"), vec![(EpicsValue::ULong(4), 0)]),
    ];
    for (src, want) in steps {
        if let Some(s) = src {
            db.put_record_field_from_ca("S", "VAL", EpicsValue::String(s.into()))
                .await
                .unwrap();
        }
        db.put_record_field_from_ca("L", "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        assert_eq!(drain(&mut rx), want, "L.LEN after reading {src:?}");
    }
}
