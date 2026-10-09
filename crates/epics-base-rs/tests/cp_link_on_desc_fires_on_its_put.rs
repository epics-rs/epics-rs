//! A CP link reading a field only a put posts (`X.DESC`) processes its holder
//! on each put of that field and not when the source processes: C's `caLink`
//! monitor fires on the field's own post. softIoc (R7.0.10-146), `HDESC.INPA`
//! = `X.DESC CP`: 1 at connect, 2 after `dbpf X.DESC 5`, 3 after `7`;
//! `dbpf X 9` leaves it 3.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

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
    db.setup_cp_links().await;
    db.process_cp_links_on_connect().await;
    let num = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    assert_eq!(num("HDESC"), 1.0, "connect");

    for (desc, val, a) in [("5", 2.0, 5.0), ("7", 3.0, 7.0)] {
        db.put_record_field_from_ca("X", "DESC", EpicsValue::String(PvString::from(desc)))
            .await
            .unwrap();
        assert_eq!((num("HDESC"), num("HDESC.A")), (val, a), "X.DESC {desc}");
        assert_eq!(num("HDESC.SEVR"), 0.0, "X.DESC {desc}");
    }

    db.put_record_field_from_ca("X", "VAL", EpicsValue::Double(9.0))
        .await
        .unwrap();
    assert_eq!(num("HDESC"), 3.0, "processing X fires no DESC edge");
}
