//! An output `{calc:…}` link (`lnkCalc_putValue`, `lnkCalc.c:631-703`): the
//! value put, or `expr` over the args when there is one, is checked by
//! `major` then `minor` and put through the `out` child. Every expectation is
//! what softIoc (8f5015b66) printed for `dbpf` / `dbgf` on these records.

use std::sync::Arc;

use epics_base_rs::server::database::{LinkRef, LinkSet, ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(bo, "LC:MAJ") { field(OUT, "{state:'lcmaj'}") }
record(bo, "LC:MIN") { field(OUT, "{state:'lcmin'}") }
record(bo, "LC:RED") { field(OUT, "{state:'lcred'}") }
record(ao, "LC:NOEXPR") { field(OUT, "{calc:{out:{state:'lc1'}}}") }
record(ao, "LC:EXPR") { field(OUT, "{calc:{expr:'!A', out:{state:'lc2'}, args:[{state:'lcred'}]}}") }
record(ao, "LC:ALM") { field(OUT, "{calc:{out:{state:'lc3'}, args:[{state:'lcmaj'},{state:'lcmin'}], major:'A', minor:'B'}}") }
record(stringout, "LC:SO") { field(OUT, "{calc:{out:{state:'lc4'}}}") }
record(longin, "LC:RB1") { field(INP, "{state:'lc1'}") }
record(longin, "LC:RB2") { field(INP, "{state:'lc2'}") }
record(longin, "LC:RB3") { field(INP, "{state:'lc3'}") }
record(longin, "LC:RB4") { field(INP, "{state:'lc4'}") }
"#;

async fn dbpf(db: &PvDatabase, rec: &str, value: EpicsValue) {
    db.put_pv(rec, value).await.unwrap();
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
}

async fn readback(db: &PvDatabase, rec: &str) -> f64 {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
    db.get_pv(rec).unwrap().to_f64().unwrap()
}

/// STAT, SEVR and AMSG, as `dbgf` prints them.
fn alarm(db: &PvDatabase, rec: &str) -> (&'static str, AlarmSeverity, String) {
    let cell = db.get_record(rec).unwrap();
    let inst = cell.read();
    let stat = match inst.common.stat {
        0 => "NO_ALARM",
        14 => "LINK",
        other => panic!("{rec}.STAT {other}"),
    };
    (stat, inst.common.sevr, String::from(&inst.common.amsg))
}

#[epics_macros_rs::epics_test]
async fn an_output_calc_puts_through_its_out_child() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();

    dbpf(&db, "LC:NOEXPR", EpicsValue::Double(1.0)).await;
    assert_eq!(readback(&db, "LC:RB1").await, 1.0);
    dbpf(&db, "LC:NOEXPR", EpicsValue::Double(0.0)).await;
    assert_eq!(readback(&db, "LC:RB1").await, 0.0);

    // `expr` replaces the value put.
    dbpf(&db, "LC:RED", EpicsValue::Enum(1)).await;
    dbpf(&db, "LC:EXPR", EpicsValue::Double(5.0)).await;
    assert_eq!(readback(&db, "LC:RB2").await, 0.0);
    dbpf(&db, "LC:RED", EpicsValue::Enum(0)).await;
    dbpf(&db, "LC:EXPR", EpicsValue::Double(5.0)).await;
    assert_eq!(readback(&db, "LC:RB2").await, 1.0);

    // `major` wins over `minor`, and neither stops the put.
    dbpf(&db, "LC:MAJ", EpicsValue::Enum(1)).await;
    dbpf(&db, "LC:MIN", EpicsValue::Enum(1)).await;
    dbpf(&db, "LC:ALM", EpicsValue::Double(1.0)).await;
    assert_eq!(
        alarm(&db, "LC:ALM"),
        ("LINK", AlarmSeverity::Major, "post_major error".into())
    );
    assert_eq!(readback(&db, "LC:RB3").await, 1.0);
    dbpf(&db, "LC:MAJ", EpicsValue::Enum(0)).await;
    dbpf(&db, "LC:ALM", EpicsValue::Double(0.0)).await;
    assert_eq!(
        alarm(&db, "LC:ALM"),
        ("LINK", AlarmSeverity::Minor, "post_minor error".into())
    );
    assert_eq!(readback(&db, "LC:RB3").await, 0.0);
    dbpf(&db, "LC:MIN", EpicsValue::Enum(0)).await;
    dbpf(&db, "LC:ALM", EpicsValue::Double(1.0)).await;
    assert_eq!(
        alarm(&db, "LC:ALM"),
        ("NO_ALARM", AlarmSeverity::NoAlarm, String::new())
    );
    assert_eq!(readback(&db, "LC:RB3").await, 1.0);
}

#[epics_macros_rs::epics_test]
async fn a_value_that_is_not_a_number_fails_the_put() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();

    dbpf(&db, "LC:SO", EpicsValue::String("3".into())).await;
    assert_eq!(
        alarm(&db, "LC:SO"),
        ("NO_ALARM", AlarmSeverity::NoAlarm, String::new())
    );
    assert_eq!(readback(&db, "LC:RB4").await, 1.0);
    dbpf(&db, "LC:SO", EpicsValue::String("abc".into())).await;
    assert_eq!(
        alarm(&db, "LC:SO"),
        ("LINK", AlarmSeverity::Invalid, "field OUT".into())
    );
    dbpf(&db, "LC:SO", EpicsValue::String("0".into())).await;
    assert_eq!(readback(&db, "LC:RB4").await, 0.0);
    dbpf(&db, "LC:SO", EpicsValue::String(String::new().into())).await;
    assert_eq!(
        alarm(&db, "LC:SO"),
        ("NO_ALARM", AlarmSeverity::NoAlarm, String::new())
    );
}

/// A link set whose channels never connect.
struct Down;

#[async_trait::async_trait]
impl LinkSet for Down {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        false
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
}

/// The `out` child is no field of the record, so the alarm of its failed
/// put names `"????"` (`dbLinkFieldName`, `dbLink.c:52-66`); the record's own
/// `setLinkAlarm` after it changes nothing. A failed put of the record's
/// own link names the field.
#[epics_macros_rs::epics_test]
async fn a_failed_put_through_the_out_child_names_no_field() {
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(
            r#"
record(ao, "LC:CHILD") { field(OUT, "{calc:{out:{pva:'LC:NOT:HERE'}}}") }
record(ao, "LC:OWN") { field(OUT, "{pva:'LC:NOT:HERE'}") }
"#,
            &Default::default(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.register_link_set("pva", Arc::new(Down)).await;
    db.ioc_init().await;

    dbpf(&db, "LC:CHILD", EpicsValue::Double(1.0)).await;
    assert_eq!(
        alarm(&db, "LC:CHILD"),
        ("LINK", AlarmSeverity::Invalid, "field ????".into())
    );
    dbpf(&db, "LC:OWN", EpicsValue::Double(1.0)).await;
    assert_eq!(
        alarm(&db, "LC:OWN"),
        ("LINK", AlarmSeverity::Invalid, "field OUT".into())
    );
}
