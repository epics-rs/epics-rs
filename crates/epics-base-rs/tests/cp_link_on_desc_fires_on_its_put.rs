//! A CP link reading a field only a put posts (`X.DESC`) processes its holder
//! on each put of that field and not when the source processes: C's `caLink`
//! monitor fires on the field's own post. softIoc (R7.0.10-146), `HDESC.INPA`
//! = `X.DESC CP`: 1 at connect, 2 after `dbpf X.DESC 5`, 3 after `7`;
//! `dbpf X 9` leaves it 3.

mod calink_ioc;

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

const DB: &str = r#"
record(ao, "X") { field(VAL, "0") }
record(calc, "HDESC") { field(INPA, "X.DESC CP") field(CALC, "VAL+1") }
"#;

#[epics_macros_rs::epics_test]
async fn hdesc_follows_puts_of_x_desc_only() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.initialize_link_locality().await;
    calink_ioc::connect(&db).await;
    let num = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    calink_ioc::until("HDESC processes on connect", || num("HDESC") == 1.0).await;

    for (desc, val, a) in [("5", 2.0, 5.0), ("7", 3.0, 7.0)] {
        db.put_record_field_from_ca("X", "DESC", EpicsValue::String(PvString::from(desc)))
            .await
            .unwrap();
        calink_ioc::until(&format!("X.DESC {desc} processes HDESC"), || {
            (num("HDESC"), num("HDESC.A")) == (val, a)
        })
        .await;
        assert_eq!(num("HDESC.SEVR"), 0.0, "X.DESC {desc}");
    }

    // The link's only trigger is a DESC event, so a VAL put that posts no
    // DESC event cannot process HDESC.
    let mut desc_rx = db
        .get_record("X")
        .unwrap()
        .write()
        .add_subscriber(
            "DESC",
            1,
            DbFieldType::String,
            (EventMask::VALUE | EventMask::ALARM).bits(),
        )
        .unwrap();
    db.put_record_field_from_ca("X", "VAL", EpicsValue::Double(9.0))
        .await
        .unwrap();
    assert!(
        desc_rx.try_recv().is_err(),
        "processing X posts no DESC event, so fires no DESC edge"
    );
    assert_eq!(num("HDESC"), 3.0);
}
