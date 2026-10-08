//! `dbPut` takes `dbPutConvertRoutine` (`putString*`) for every
//! `special(SPC_DBADDR)` field, whatever the count (`dbAccess.c:1355`), so a
//! field the port stores as a scalar converts as an array element: no
//! empty-string arm, and `putStringEnum` parses the index with `&end`. Each
//! expectation is softIoc's `dbpf` / `dbgf` on the same database.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(mbbo, "N0") { field(VAL, "3") }
record(mbbo, "N1") { field(VAL, "3") }
record(mbbo, "S1") { field(ZRST, "zero") field(ONST, "one") field(TWST, "two") field(VAL, "1") }
record(aSub, "AS") { field(FTA, "DOUBLE") field(NOA, "1") field(FTVA, "LONG") field(NOVA, "1") }
record(ao, "AO") { field(VAL, "4") }
record(bo, "BO") { field(ZNAM, "off") field(ONAM, "on") field(VAL, "1") }
"#;

/// Each put in turn on one fresh IOC; the last one's status and value.
async fn puts(seq: &[(&str, &str)]) -> (bool, Option<EpicsValue>) {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut last = (false, None);
    for (pv, s) in seq {
        let ok = db
            .put_pv(pv, EpicsValue::String(PvString::from(*s)))
            .await
            .is_ok();
        last = (ok, db.get_pv(pv).ok());
    }
    last
}

async fn put(pv: &str, s: &str) -> (bool, Option<EpicsValue>) {
    puts(&[(pv, s)]).await
}

/// softIoc: `dbpf N0 ""` leaves 3 — stateless mbbo VAL is served
/// `DBF_USHORT` (`mbboRecord.c:308-310`) and `putStringUshort` refuses "".
#[epics_macros_rs::epics_test]
async fn an_empty_string_into_a_stateless_mbbo_is_refused() {
    assert_eq!(put("N0", "").await, (false, Some(EpicsValue::UShort(3))));
}

/// softIoc: `dbpf N1 "2xyz"` stores 2 — the same parse allows a tail.
#[epics_macros_rs::epics_test]
async fn a_stateless_mbbo_takes_the_number_before_a_tail() {
    assert_eq!(put("N1", "2xyz").await, (true, Some(EpicsValue::UShort(2))));
}

/// softIoc: `dbpf S1 "2xyz"` selects state 2 — `putStringEnum`, unlike the
/// `cvt_st_e` a non-`SPC_DBADDR` enum takes (`dbpf BO "1xyz"` is refused).
#[epics_macros_rs::epics_test]
async fn an_mbbo_index_with_a_tail_is_put_string_enum() {
    assert_eq!(put("S1", "2xyz").await, (true, Some(EpicsValue::Enum(2))));
    assert_eq!(put("BO", "1xyz").await, (false, Some(EpicsValue::Enum(1))));
}

/// softIoc: `dbpf AS.A ""` and `dbpf AS.VALA ""` leave 5 and 7 at NOA/NOVA 1.
#[epics_macros_rs::epics_test]
async fn an_empty_string_into_an_asub_channel_is_refused() {
    assert_eq!(
        puts(&[("AS.A", "5"), ("AS.A", "")]).await,
        (false, Some(EpicsValue::Double(5.0)))
    );
    assert_eq!(
        puts(&[("AS.VALA", "7"), ("AS.VALA", "")]).await,
        (false, Some(EpicsValue::Long(7)))
    );
}

/// softIoc: `dbpf AO ""` stores 0 — `ao.VAL` is not `SPC_DBADDR`, so its row
/// is `cvt_st_d` and its empty-string arm.
#[epics_macros_rs::epics_test]
async fn a_plain_scalar_keeps_the_empty_string_arm() {
    assert_eq!(put("AO", "").await, (true, Some(EpicsValue::Double(0.0))));
}
