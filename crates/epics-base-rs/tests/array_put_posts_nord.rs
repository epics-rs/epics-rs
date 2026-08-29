//! R18-109: NORD is posted by the put, not by the put *route*.
//!
//! C `put_array_info` (`waveformRecord.c:202-216`; `aaiRecord.c:227-240`,
//! `aaoRecord.c:163-189`, `subArrayRecord.c:186-198`):
//!
//! ```c
//! static long put_array_info(DBADDR *paddr, long nNew)
//! {
//!     waveformRecord *prec = (waveformRecord *) paddr->precord;
//!     epicsUInt32 nord = prec->nord;
//!
//!     prec->nord = nNew;
//!     if (prec->nord > prec->nelm)
//!         prec->nord = prec->nelm;
//!
//!     if (nord != prec->nord)
//!         db_post_events(prec, &prec->nord, DBE_VALUE | DBE_LOG);
//!     return 0;
//! }
//! ```
//!
//! `dbPut` calls it, so EVERY put route reaches it. The port posted NORD from
//! `put_pv_and_post` only; the CA route (`put_record_field_from_ca_inner`) and
//! the `dbPutLink` route (`put_pv`) posted nothing, so a client monitoring
//! `WF.NORD` — the standard way to learn how many elements a producer wrote —
//! learned nothing from a `caput -a`.
//!
//! Ground truth, softIoc (`bin/linux-x86_64`), waveform on `SCAN = 10 second`
//! so the put drives no process cycle. `camonitor WS.NORD WS`, then
//! `caput -a WS 3 1 2 3`:
//!
//! ```text
//! WS.NORD   <undefined> 0 UDF INVALID
//! WS        <undefined>   UDF INVALID
//! WS.NORD   <undefined> 3 UDF INVALID     <- the put posts NORD immediately
//! ```
//!
//! and no VAL event: waveform VAL is `pp(TRUE)`, so C suppresses the value
//! field's own `dbPut` post and the next scan is ten seconds away. NORD is the
//! only thing the subscriber hears — which is exactly why losing it matters.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(waveform, "WS")  { field(FTVL,"DOUBLE") field(NELM,"8") field(SCAN,"10 second") }
record(aai,      "AS")  { field(FTVL,"DOUBLE") field(NELM,"8") field(SCAN,"10 second") }
record(aao,      "AOS") { field(FTVL,"DOUBLE") field(NELM,"8") field(SCAN,"10 second") }
record(subArray, "SS")  { field(FTVL,"DOUBLE") field(MALM,"8") field(NELM,"4")
                          field(INP,"WS") field(SCAN,"10 second") }
record(waveform, "WL")  { field(FTVL,"DOUBLE") field(NELM,"8") field(SCAN,"10 second") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn nord_sub(db: &PvDatabase, rec: &str) -> EventReader {
    let r = db.get_record(rec).unwrap();
    let mut inst = r.write();
    inst.add_subscriber("NORD", 1, DbFieldType::Long, EventMask::VALUE.bits())
        .unwrap_or_else(|| panic!("{rec}.NORD subscription accepted"))
}

fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev.snapshot.value.clone());
    }
    out
}

/// The finding's own probe: a `caput -a` of three elements onto an
/// unprocessed waveform must post NORD = 3 to a NORD subscriber.
#[epics_macros_rs::epics_test]
async fn a_ca_array_put_posts_nord() {
    let db = build().await;

    for rec in ["WS", "AS", "AOS"] {
        let mut rx = nord_sub(&db, rec).await;

        db.put_record_field_from_ca(rec, "VAL", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
            .await
            .unwrap_or_else(|e| panic!("caput -a {rec} 3 1 2 3: {e:?}"));

        assert_eq!(
            drain(&mut rx),
            vec![EpicsValue::ULong(3)],
            "{rec}: C `put_array_info` posts NORD from inside `dbPut` — one event, value 3"
        );
    }
}

/// subArray's NORD is DBF_LONG, and its VAL put is the INP-driven slice, so it
/// gets its own case rather than riding the loop above.
#[epics_macros_rs::epics_test]
async fn a_ca_array_put_posts_nord_on_subarray() {
    let db = build().await;
    let mut rx = nord_sub(&db, "SS").await;

    db.put_record_field_from_ca("SS", "VAL", EpicsValue::DoubleArray(vec![1.0, 2.0]))
        .await
        .unwrap();

    assert_eq!(drain(&mut rx), vec![EpicsValue::Long(2)]);
}

/// The other silent route: `put_pv` is the `dbPutLink` `dbPut`. An OUT link
/// that lands an array on a waveform posts NORD in C even when the link is NPP
/// and the target never processes.
#[epics_macros_rs::epics_test]
async fn a_db_link_write_posts_nord() {
    let db = build().await;
    let mut rx = nord_sub(&db, "WL").await;

    db.put_pv("WL.VAL", EpicsValue::DoubleArray(vec![7.0, 8.0, 9.0, 10.0]))
        .await
        .unwrap();

    assert_eq!(
        drain(&mut rx),
        vec![EpicsValue::ULong(4)],
        "put_pv is a `dbPut` body and must reach `put_array_info` like the others"
    );
}

/// The `if (nord != prec->nord)` half of the C source: an array put that does
/// not move NORD posts nothing. Without this the fix would turn every put into
/// a NORD event and flood the subscriber it is meant to serve.
#[epics_macros_rs::epics_test]
async fn a_put_that_does_not_move_nord_posts_nothing() {
    let db = build().await;

    db.put_record_field_from_ca("WS", "VAL", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .await
        .unwrap();

    // Subscribe AFTER NORD is already 3, then put three different elements.
    let mut rx = nord_sub(&db, "WS").await;
    db.put_record_field_from_ca("WS", "VAL", EpicsValue::DoubleArray(vec![4.0, 5.0, 6.0]))
        .await
        .unwrap();

    assert_eq!(
        drain(&mut rx),
        Vec::<EpicsValue>::new(),
        "NORD stayed 3 — C posts it only when it moved"
    );
}
