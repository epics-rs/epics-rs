//! `aSubRecord.c:163` seeds `oval = val` below the INAM/SNAM early returns,
//! and `monitor()` posts VAL only on `val != oval` (`:414`). softIoc reads
//! OVAL 3 for `field(VAL,"3")` with no SNAM or with `LFLG=READ`, and 0 when
//! SNAM does not resolve.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_subroutine("three", |_| Ok(3))
        .db_string(
            r#"record(aSub, "OK") { field(SNAM, "three") field(VAL, "3") }
record(aSub, "NONE") { field(VAL, "3") }
record(aSub, "READ") { field(LFLG, "READ") field(SNAM, "noSuch") field(VAL, "3") }
record(aSub, "BAD") { field(SNAM, "noSuch") field(VAL, "3") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn oval(db: &PvDatabase, name: &str) -> Option<EpicsValue> {
    db.get_record(name).unwrap().read().record.get_field("OVAL")
}

#[epics_macros_rs::epics_test]
async fn oval_is_val_only_past_the_snam_lookup() {
    let db = ioc().await;
    for (name, want, posts) in [
        ("OK", 3, false),
        ("NONE", 3, false),
        ("READ", 3, false),
        ("BAD", 0, true),
    ] {
        assert_eq!(oval(&db, name), Some(EpicsValue::Long(want)), "{name}.OVAL");
        let rec = db.get_record(name).unwrap();
        assert_eq!(
            rec.write().record.monitor_value_changed(),
            Some(posts),
            "{name}: VAL 3 against OVAL {want}"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn a_posted_val_advances_oval() {
    let db = ioc().await;
    let rec = db.get_record("OK").unwrap();
    rec.write()
        .record
        .put_field("VAL", EpicsValue::Long(4))
        .unwrap();
    assert_eq!(rec.write().record.monitor_value_changed(), Some(true));
    assert_eq!(oval(&db, "OK"), Some(EpicsValue::Long(4)));

    // The subroutine puts 3 back; that is a change against OVAL 4.
    db.process_record_with_links("OK", &mut ProcStack::new())
        .await
        .unwrap();
    assert_eq!(oval(&db, "OK"), Some(EpicsValue::Long(3)));
}
