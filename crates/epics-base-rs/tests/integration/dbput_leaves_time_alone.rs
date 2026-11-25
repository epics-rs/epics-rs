//! C `dbPut` never writes `TIME`: only the process cycle's
//! `recGblGetTimeStamp` does. A put that drives no processing therefore keeps
//! the stamp of the last process, which is what `caget -a` shows and what the
//! next cycle's AFTC filter measures its gap from (`biRecord.c:134,259`).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_base_rs::types::WallTime;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const DB: &str = r#"
record(ai,        "AI") { field(VAL, "1") }
record(ao,        "AO") { field(VAL, "1") }
record(stringout, "SO") { field(VAL, "a") }
record(lso,       "LS") { field(SIZV, "16") }
"#;

const STAMP: Duration = Duration::new(42, 123_456_000);

async fn build() -> Arc<PvDatabase> {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for rec in ["AI", "AO", "SO", "LS"] {
        db.get_record(rec).unwrap().write().common.time = WallTime::UNIX_EPOCH + STAMP;
    }
    db
}

fn time(db: &PvDatabase, rec: &str) -> WallTime {
    db.get_record(rec).unwrap().read().common.time
}

fn value(rec: &str) -> EpicsValue {
    match rec {
        "SO" | "LS" => EpicsValue::String("b".into()),
        _ => EpicsValue::Double(5.0),
    }
}

#[epics_macros_rs::epics_test]
async fn no_put_body_restamps_time() {
    for body in [
        "put_pv",
        "put_pv_and_post",
        "put_pv_no_process",
        "ca_no_notify",
    ] {
        let db = build().await;
        for rec in ["AI", "AO", "SO", "LS"] {
            let name = format!("{rec}.VAL");
            match body {
                "put_pv" => db.put_pv(&name, value(rec)).await.unwrap(),
                "put_pv_and_post" => db.put_pv_and_post(&name, value(rec)).await.unwrap(),
                "put_pv_no_process" => db.put_pv_no_process(&name, value(rec)).await.unwrap(),
                // VAL is `pp(TRUE)` there, so a CA put of it processes and
                // takes that cycle's stamp, as C's does.
                _ => db
                    .put_record_field_from_ca_no_notify(rec, "DESC", EpicsValue::String("d".into()))
                    .await
                    .map(|_| ())
                    .unwrap(),
            }
            assert_eq!(
                time(&db, rec),
                WallTime::UNIX_EPOCH + STAMP,
                "{body} {name} restamped TIME"
            );
        }
    }
}
