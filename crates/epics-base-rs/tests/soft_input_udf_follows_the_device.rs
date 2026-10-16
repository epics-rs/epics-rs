//! A soft-channel INP read clears UDF only where the C dset does
//! (`devSiSoft.c:72`, `devEventSoft.c:84`); `devLsiSoft.c::readLocked` never
//! touches it. softIoc R7.0.10.1, INP -> stringin "src", `caput PROC 1`, then
//! `caput VAL x`, then `caput PROC 1`:
//!
//! ```text
//!        init  proc  put  proc2
//! LS      1     1     0    0     lsi, INP=SRC
//! LC      1     1     0    0     lsi, INP="hi" (constant)
//! LN      1     1     0    0     lsi, no INP
//! SI      1     0     0    0     stringin, INP=SRC
//! EV      1     0     0    0     event, INP=SRC
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringin, "SRC") { field(VAL, "src") }
record(lsi, "LS") { field(SIZV, "40") field(INP, "SRC") }
record(lsi, "LC") { field(SIZV, "40") field(INP, "hi") }
record(lsi, "LN") { field(SIZV, "40") }
record(stringin, "SI") { field(INP, "SRC") }
record(event, "EV") { field(INP, "SRC") }
"#;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn udf(db: &PvDatabase, rec: &str) -> u8 {
    db.get_pv(format!("{rec}.UDF")).unwrap().to_f64().unwrap() as u8
}

async fn sequence(db: &PvDatabase, rec: &str) -> [u8; 4] {
    let init = udf(db, rec);
    process(db, rec).await;
    let proc = udf(db, rec);
    db.put_record_field_from_ca(rec, "VAL", EpicsValue::String("x".into()))
        .await
        .unwrap();
    let put = udf(db, rec);
    process(db, rec).await;
    [init, proc, put, udf(db, rec)]
}

#[epics_macros_rs::epics_test]
async fn a_soft_read_clears_udf_only_where_the_dset_does() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for (rec, want) in [
        ("LS", [1, 1, 0, 0]),
        ("LC", [1, 1, 0, 0]),
        ("LN", [1, 1, 0, 0]),
        ("SI", [1, 0, 0, 0]),
        ("EV", [1, 0, 0, 0]),
    ] {
        assert_eq!(sequence(&db, rec).await, want, "{rec} init/proc/put/proc2");
    }
}
