//! A failed TSEL read raises LINK/INVALID inside `recGblGetTimeStamp`. calcout
//! runs `checkAlarms` before that (`calcoutRecord.c:244/249`), so its HIHI
//! INVALID wins the severity tie; ai stamps first, so LINK wins.
//! softIoc, TSEL "NOPE:X CA", HHSV INVALID, value 20 over HIHI 10:
//! CO STAT=HIHI SEVR=INVALID, AI STAT=LINK SEVR=INVALID.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calcout, "CO") {
    field(TSEL, "NOPE:X CA") field(INPA, "20") field(CALC, "A")
    field(HIHI, "10") field(HHSV, "INVALID")
}
record(ai, "AI") {
    field(TSEL, "NOPE:X CA") field(VAL, "20")
    field(HIHI, "10") field(HHSV, "INVALID")
}
"#;

#[epics_macros_rs::epics_test]
async fn the_record_type_declares_which_alarm_wins_the_tie() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    // alarm_status: HIHI = 3, LINK = 14.
    for (rec, stat) in [("CO", 3u16), ("AI", 14)] {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links(rec, &mut visited)
            .await
            .unwrap();
        assert_eq!(
            db.get_pv(&format!("{rec}.SEVR")).unwrap(),
            EpicsValue::Short(3),
            "{rec}"
        );
        assert_eq!(
            db.get_pv(&format!("{rec}.STAT")).unwrap(),
            EpicsValue::Short(stat as i16),
            "{rec}"
        );
    }
}
