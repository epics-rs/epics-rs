//! `aiRecord.c:495` `prec->rval = (long)floor(prec->sval)` lands in the
//! `epicsInt32` RVAL: on x86-64 the `(long)` is a 64-bit cvttsd2si, so an
//! out-of-int range SIOL keeps its low 32 bits instead of saturating.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "SRC") {}
record(ai, "AR") { field(SIMM, "RAW") field(SIOL, "SRC") }
"#;

#[epics_macros_rs::epics_test]
async fn ai_raw_keeps_the_low_word_of_a_long() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for (v, rval) in [
        (3e9, 3_000_000_000_i64 as i32),
        (-1.5, -2),
        (4294967296.0 + 7.0, 7),
    ] {
        db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(v))
            .await
            .unwrap();
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links("AR", &mut visited)
            .await
            .unwrap();
        let inst = db.get_record("AR").unwrap();
        assert_eq!(
            inst.read().record.get_field("RVAL"),
            Some(EpicsValue::Long(rval)),
            "SIOL {v}"
        );
    }
}
