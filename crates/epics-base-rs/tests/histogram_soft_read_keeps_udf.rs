//! `devHistogramSoft.c::read_histogram` never touches UDF; only the
//! constant-SVL init load and `clear_histogram` (a CMD put) clear it.
//! softIoc R7.0.10.1, SVL -> ao "S":
//!
//! ```text
//!        init  proc  put S, proc  CMD=Clear  proc
//! H      1     1     1            0          0     SVL=S
//! C      0                                         SVL="2.5" (constant)
//! ```
//!
//! The UDF=1 raises no alarm: SEVR NO_ALARM after the first process.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "S") { field(VAL, "3") }
record(histogram, "H") { field(SVL, "S") field(NELM, "4") field(ULIM, "10") }
record(histogram, "C") { field(SVL, "2.5") field(NELM, "4") field(ULIM, "10") }
"#;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn udf(db: &PvDatabase, rec: &str) -> u8 {
    db.get_pv(&format!("{rec}.UDF")).unwrap().to_f64().unwrap() as u8
}

#[epics_macros_rs::epics_test]
async fn a_histogram_soft_read_keeps_udf_until_a_clear() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    assert_eq!(udf(&db, "C"), 0, "constant SVL defines at init");
    let init = udf(&db, "H");
    process(&db, "H").await;
    let proc = udf(&db, "H");
    assert_eq!(
        db.get_pv("H.SEVR").unwrap().to_f64().unwrap(),
        0.0,
        "UDF raises no alarm"
    );
    db.put_record_field_from_ca("S", "VAL", EpicsValue::Double(4.0))
        .await
        .unwrap();
    process(&db, "H").await;
    let proc2 = udf(&db, "H");
    db.put_record_field_from_ca("H", "CMD", EpicsValue::Short(0))
        .await
        .unwrap();
    let clear = udf(&db, "H");
    process(&db, "H").await;
    assert_eq!(
        [init, proc, proc2, clear, udf(&db, "H")],
        [1, 1, 1, 0, 0],
        "init/proc/proc2/clear/proc3"
    );
}
