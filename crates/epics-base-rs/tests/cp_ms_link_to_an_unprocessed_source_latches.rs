//! The connect-time wait behind `scanRun`'s work holds back no alarm C
//! raises: a CP MS link to a source nothing processed reads it UDF and
//! latches INVALID/LINK, while one whose source processed before `iocRun`
//! raises none. softIoc R7.0.10-146 on this db: `LNK` SEVR/ACKS INVALID,
//! STAT LINK; `SRC` INVALID/UDF; `LNK2` NO_ALARM.

mod calink_ioc;

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(longin, "SRC") { field(INP, "7") }
record(longout, "LNK") { field(DOL, "SRC CP MS") field(OMSL, "closed_loop") }
record(longin, "SRC2") { field(INP, "7") }
record(longout, "LNK2") { field(DOL, "SRC2 CP MS") field(OMSL, "closed_loop") }
"#;

/// C `menuAlarmSevr` INVALID, `menuAlarmStat` LINK and UDF.
const INVALID: f64 = 3.0;
const LINK: f64 = 14.0;
const UDF: f64 = 17.0;

#[epics_macros_rs::epics_test]
async fn only_the_link_to_the_unprocessed_source_latches() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.put_record_field_from_ca("SRC2", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    db.scan_control().run();
    calink_ioc::connect(&db).await;
    db.ca_sync().await;

    let num = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    assert_eq!((num("SRC.SEVR"), num("SRC.STAT")), (INVALID, UDF));
    assert_eq!(
        (num("LNK.SEVR"), num("LNK.STAT"), num("LNK.ACKS")),
        (INVALID, LINK, INVALID)
    );
    assert_eq!((num("LNK2.SEVR"), num("LNK2.ACKS")), (0.0, 0.0));
}
