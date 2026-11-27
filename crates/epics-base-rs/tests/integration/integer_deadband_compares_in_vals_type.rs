//! longin/longout and int64in/int64out deadband with C's integer `DELTA`
//! (`longinRecord.c:370`, `int64inRecord.c:363`), not `recGblCheckDeadband`:
//! `mdel < 0 || DELTA(mlst, val) > (epicsUInt64) mdel`, with MLST/ALST
//! declared `DBF_LONG` / `DBF_INT64`.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const TWO_53: i64 = 1 << 53;

async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Puts `fields`, then VAL, and returns what the deadband says.
fn crossed(
    db: &PvDatabase,
    name: &str,
    fields: &[(&str, EpicsValue)],
    val: EpicsValue,
) -> (bool, bool) {
    let rec = db.get_record(name).unwrap();
    let mut inst = rec.write();
    for (field, v) in fields {
        inst.record.put_field(field, v.clone()).unwrap();
    }
    inst.record.put_field("VAL", val).unwrap();
    inst.check_deadband_ext()
}

#[epics_macros_rs::epics_test]
async fn int64_one_count_past_2_53_crosses_mdel_0() {
    for rtype in ["int64in", "int64out"] {
        let db = ioc(&format!(
            r#"record({rtype}, "R") {{ field(VAL, "{TWO_53}") }}"#
        ))
        .await;
        let rec = db.get_record("R").unwrap();
        assert_eq!(
            rec.read().record.get_field("MLST"),
            Some(EpicsValue::Int64(TWO_53)),
            "{rtype}: MLST is DBF_INT64 and seeded from VAL"
        );
        assert_eq!(
            crossed(&db, "R", &[], EpicsValue::Int64(TWO_53 + 1)),
            (true, true),
            "{rtype}: 2^53 -> 2^53+1"
        );
        assert_eq!(
            rec.read().record.get_field("ALST"),
            Some(EpicsValue::Int64(TWO_53 + 1)),
            "{rtype}"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn integer_delta_equal_to_mdel_does_not_cross_and_one_more_does() {
    for rtype in ["longin", "longout"] {
        let v = EpicsValue::Long;
        let db = ioc(&format!(r#"record({rtype}, "R") {{ field(VAL, "10") }}"#)).await;
        let del = [("MDEL", v(2)), ("ADEL", v(3))];
        assert_eq!(
            crossed(&db, "R", &del, v(12)),
            (false, false),
            "{rtype} 10->12"
        );
        assert_eq!(
            crossed(&db, "R", &[], v(13)),
            (true, false),
            "{rtype} 10->13"
        );
        assert_eq!(
            crossed(&db, "R", &[], v(14)),
            (false, true),
            "{rtype} 10->14"
        );
        assert_eq!(
            db.get_record("R").unwrap().read().record.get_field("MLST"),
            Some(v(13)),
            "{rtype}: MLST is DBF_LONG"
        );
    }
    for rtype in ["int64in", "int64out"] {
        let db = ioc(&format!(r#"record({rtype}, "R") {{ field(VAL, "10") }}"#)).await;
        let v = EpicsValue::Int64;
        let del = [("MDEL", v(2)), ("ADEL", v(3))];
        assert_eq!(
            crossed(&db, "R", &del, v(12)),
            (false, false),
            "{rtype} 10->12"
        );
        assert_eq!(
            crossed(&db, "R", &[], v(13)),
            (true, false),
            "{rtype} 10->13"
        );
        assert_eq!(
            crossed(&db, "R", &[], v(14)),
            (false, true),
            "{rtype} 10->14"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn negative_mdel_posts_an_unchanged_value() {
    let db = ioc(r#"record(longin, "L") { field(VAL, "5") }
record(int64in, "I") { field(VAL, "5") }"#)
    .await;
    let l = [
        ("MDEL", EpicsValue::Long(-1)),
        ("ADEL", EpicsValue::Long(-1)),
    ];
    assert_eq!(crossed(&db, "L", &l, EpicsValue::Long(5)), (true, true));
    let i = [
        ("MDEL", EpicsValue::Int64(-1)),
        ("ADEL", EpicsValue::Int64(-1)),
    ];
    assert_eq!(crossed(&db, "I", &i, EpicsValue::Int64(5)), (true, true));
}

/// `DELTA(INT_MIN, INT_MAX)` is `0xFFFFFFFF` in `epicsUInt32`, above any
/// non-negative MDEL.
#[epics_macros_rs::epics_test]
async fn full_range_delta_does_not_wrap() {
    let db = ioc(&format!(
        r#"record(longin, "L") {{ field(VAL, "{}") }}
record(int64in, "I") {{ field(VAL, "{}") }}"#,
        i32::MIN,
        i64::MIN
    ))
    .await;
    let l = [
        ("MDEL", EpicsValue::Long(i32::MAX)),
        ("ADEL", EpicsValue::Long(i32::MAX)),
    ];
    assert_eq!(
        crossed(&db, "L", &l, EpicsValue::Long(i32::MAX)),
        (true, true)
    );
    let i = [
        ("MDEL", EpicsValue::Int64(i64::MAX)),
        ("ADEL", EpicsValue::Int64(i64::MAX)),
    ];
    assert_eq!(
        crossed(&db, "I", &i, EpicsValue::Int64(i64::MAX)),
        (true, true)
    );
}
