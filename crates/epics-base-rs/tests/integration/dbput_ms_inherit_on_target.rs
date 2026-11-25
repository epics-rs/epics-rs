//! Spec §5 test 10: MS/MSS/MSI inheritance lands on the TARGET
//! (`dbDbLink.c:378`, `recGblInheritSevrMsg(…, pdest, psrce->nsta,
//! psrce->nsev, psrce->namsg)`) and never changes the source's alarm. MSI
//! passes only INVALID, NPP shows at the target's next process, and a
//! target's own worse alarm wins. Pinned to oracleStdIoc (C ground truth
//! §5.10, its DB verbatim).

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao,"TN"){}
record(ao,"TM"){}
record(ao,"TS"){}
record(ao,"TI"){}
record(ao,"TI2"){}
record(ao,"TMN"){}
record(ao,"TMJ"){field(HIHI,"10") field(HHSV,"MAJOR")}
record(ao,"SN"){field(HIHI,"10") field(HHSV,"MAJOR") field(HIGH,"5") field(HSV,"MINOR") field(OUT,"TN.VAL PP NMS")}
record(ao,"SM"){field(HIHI,"10") field(HHSV,"MAJOR") field(HIGH,"5") field(HSV,"MINOR") field(OUT,"TM.VAL PP MS")}
record(ao,"SS"){field(HIHI,"10") field(HHSV,"MAJOR") field(HIGH,"5") field(HSV,"MINOR") field(OUT,"TS.VAL PP MSS")}
record(ao,"SI"){field(HIHI,"10") field(HHSV,"MAJOR") field(HIGH,"5") field(HSV,"MINOR") field(OUT,"TI.VAL PP MSI")}
record(ao,"SI2"){field(HIHI,"10") field(HHSV,"INVALID") field(OUT,"TI2.VAL PP MSI")}
record(ao,"SMN"){field(HIHI,"10") field(HHSV,"MAJOR") field(OUT,"TMN.VAL NPP MS")}
record(ao,"SMJ"){field(OUT,"TMJ.VAL PP MS")}
"#;

const NO_ALARM: (u16, u16) = (0, 0);
const UDF_INVALID: (u16, u16) = (17, 3);
const HIHI_MAJOR: (u16, u16) = (3, 2);
const HIGH_MINOR: (u16, u16) = (4, 1);
const HIHI_INVALID: (u16, u16) = (3, 3);
const LINK_MINOR: (u16, u16) = (14, 1);
const LINK_MAJOR: (u16, u16) = (14, 2);
const LINK_INVALID: (u16, u16) = (14, 3);

fn alarm(db: &PvDatabase, rec: &str) -> (u16, u16) {
    let f = |f: &str| db.get_pv(format!("{rec}.{f}")).unwrap().to_f64().unwrap() as u16;
    (f("STAT"), f("SEVR"))
}

fn amsg(db: &PvDatabase, rec: &str) -> String {
    match db.get_pv(format!("{rec}.AMSG")).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{rec}.AMSG = {other:?}"),
    }
}

async fn put(db: &PvDatabase, rec: &str, v: f64) {
    db.put_record_field_from_ca(rec, "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn inheritance_lands_on_the_target_and_never_on_the_source() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    for s in ["SN", "SM", "SS", "SI"] {
        put(&db, s, 7.0).await;
        assert_eq!(alarm(&db, s), HIGH_MINOR, "{s}: its own alarm");
    }
    assert_eq!(alarm(&db, "TN"), NO_ALARM, "NMS");
    assert_eq!(alarm(&db, "TM"), LINK_MINOR, "MS");
    assert_eq!(alarm(&db, "TS"), HIGH_MINOR, "MSS copies the status");
    assert_eq!(amsg(&db, "TS"), "", "MSS copies the source's (empty) AMSG");
    assert_eq!(alarm(&db, "TI"), NO_ALARM, "MSI passes only INVALID");

    for s in ["SN", "SM", "SS", "SI"] {
        put(&db, s, 20.0).await;
        assert_eq!(alarm(&db, s), HIHI_MAJOR, "{s}: its own alarm");
    }
    assert_eq!(alarm(&db, "TN"), NO_ALARM);
    assert_eq!(alarm(&db, "TM"), LINK_MAJOR);
    assert_eq!(alarm(&db, "TS"), HIHI_MAJOR);
    assert_eq!(alarm(&db, "TI"), NO_ALARM);

    put(&db, "SI2", 20.0).await;
    assert_eq!(alarm(&db, "SI2"), HIHI_INVALID);
    assert_eq!(
        alarm(&db, "TI2"),
        LINK_INVALID,
        "MSI passes INVALID as LINK"
    );

    // NPP: the inherited alarm waits in the target's pending fields.
    put(&db, "SMN", 20.0).await;
    assert_eq!(alarm(&db, "SMN"), HIHI_MAJOR);
    assert_eq!(
        alarm(&db, "TMN"),
        UDF_INVALID,
        "nothing until TMN processes"
    );
    db.put_record_field_from_ca("TMN", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(alarm(&db, "TMN"), LINK_MAJOR);

    // A NO_ALARM source: the target's own HIHI stands.
    put(&db, "SMJ", 20.0).await;
    assert_eq!(alarm(&db, "SMJ"), NO_ALARM);
    assert_eq!(alarm(&db, "TMJ"), HIHI_MAJOR);

    for s in ["SN", "SM", "SS", "SI", "SI2", "SMJ"] {
        put(&db, s, 0.0).await;
        assert_eq!(alarm(&db, s), NO_ALARM, "{s}");
    }
    for t in ["TN", "TM", "TS", "TI", "TI2", "TMJ"] {
        assert_eq!(alarm(&db, t), NO_ALARM, "{t}");
    }
}
