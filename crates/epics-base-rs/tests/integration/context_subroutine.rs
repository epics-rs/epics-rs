//! A `Subroutine::Context` routine reaches what C's aSub routines reach
//! through `prec`: the record name, a `dpvt` the INAM routine sets and the
//! SNAM routine reads (`configMenuSub.c` keeps its file list there), and
//! the database it `scanOnce`s.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_context_subroutine(
            "ctxInit",
            Box::new(|_rec, ctx| {
                let name = ctx.name().to_string();
                ctx.set_dpvt(name);
                Ok(0)
            }),
        )
        .register_context_subroutine(
            "ctxDo",
            Box::new(|rec, ctx| {
                let sees_db = ctx.db().is_some_and(|db| db.get_record("CTX").is_some());
                let pvt = ctx.dpvt::<String>().expect("INAM set dpvt");
                pvt.push('!');
                let n = pvt.len() as f64;
                rec.put_field("VALA", EpicsValue::Double(n))?;
                rec.put_field("VALB", EpicsValue::Double(f64::from(u8::from(sees_db))))?;
                Ok(0)
            }),
        )
        .register_subroutine("plain", |rec| {
            rec.put_field("VALA", EpicsValue::Double(7.0))?;
            Ok(0)
        })
        .db_string(
            r#"record(aSub, "CTX") { field(INAM, "ctxInit") field(SNAM, "ctxDo") }
record(aSub, "PLAIN") { field(SNAM, "plain") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, name: &str) {
    db.process_record_with_links(name, &mut ProcStack::new())
        .await
        .unwrap();
}

fn field(db: &PvDatabase, rec: &str, f: &str) -> Option<EpicsValue> {
    db.get_record(rec).unwrap().read().record.get_field(f)
}

#[epics_macros_rs::epics_test]
async fn inam_dpvt_persists_into_every_snam_call() {
    let db = ioc().await;
    process(&db, "CTX").await;
    // "CTX" stored by INAM, one '!' per process.
    assert_eq!(field(&db, "CTX", "VALA"), Some(EpicsValue::Double(4.0)));
    process(&db, "CTX").await;
    assert_eq!(field(&db, "CTX", "VALA"), Some(EpicsValue::Double(5.0)));
    assert_eq!(
        field(&db, "CTX", "VALB"),
        Some(EpicsValue::Double(1.0)),
        "the context reaches the record's own database"
    );
}

#[epics_macros_rs::epics_test]
async fn a_plain_subroutine_still_runs() {
    let db = ioc().await;
    process(&db, "PLAIN").await;
    assert_eq!(field(&db, "PLAIN", "VALA"), Some(EpicsValue::Double(7.0)));
}
