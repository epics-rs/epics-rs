//! No C record posts PVAL: ao (`aoRecord.c:442,468`), longout
//! (`longoutRecord.c:126,492`) and calcout (`calcoutRecord.c:216,275`) only
//! store it. softIoc, `camonitor -m va` on VAL and PVAL of each across
//! `caput A 3`, `caput O 4`, `caput C.PROC 1`: VAL posts, PVAL never.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

#[epics_macros_rs::epics_test]
async fn pval_moves_and_is_not_posted() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(ao, "A") { field(VAL, "1") }
record(longout, "O") { field(VAL, "1") }
record(calcout, "C") { field(CALC, "VAL+1") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let mask = (EventMask::VALUE | EventMask::ALARM | EventMask::LOG).bits();
    let read = |rec: &str| {
        db.get_record(rec)
            .unwrap()
            .read()
            .record
            .get_field("PVAL")
            .and_then(|v| v.to_f64())
    };
    for (rec, field, value) in [
        ("A", "VAL", EpicsValue::Double(3.0)),
        ("O", "VAL", EpicsValue::Long(4)),
        ("C", "PROC", EpicsValue::Long(1)),
    ] {
        let mut subs = {
            let r = db.get_record(rec).unwrap();
            let mut g = r.write();
            ["VAL", "PVAL"].map(|f| g.add_subscriber(f, 1, DbFieldType::Double, mask).unwrap())
        };
        let before = read(rec);
        db.put_record_field_from_ca(rec, field, value)
            .await
            .unwrap();
        assert_ne!(read(rec), before, "{rec}.PVAL never moved");
        let [val, pval] = subs.each_mut().map(|rx| {
            let mut n = 0;
            while rx.try_recv().is_ok() {
                n += 1;
            }
            n
        });
        assert_eq!((val, pval), (1, 0), "{rec}: VAL, PVAL events");
    }
}
