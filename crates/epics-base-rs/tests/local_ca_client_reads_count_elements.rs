//! An in-process CA channel reads a request of `count` elements as
//! `dbChannel_get` does (`db_access.c:121-138`): truncated, or zero padded
//! past what the field holds, on a get (`dbContextReadNotifyCache.cpp:86-90`)
//! and on each event of a subscription (`dbContext.cpp:178-183`). A count of
//! 0 reads what the field holds (`dbChannel_get_count`), as dbCa's native
//! monitor asks (`dbCa.c:1290-1293`).
//!
//! dbCa's attribute get is such a read of one element, so a link to an empty
//! waveform still caches its units and precision.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::database::{ClientChannel, ClientSubscription, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DBR_TIME_DOUBLE, EpicsValue};

const DB: &str = r#"
record(waveform, "WE") { field(FTVL, "DOUBLE") field(NELM, "4") field(EGU, "mm") field(PREC, "3") }
record(calc, "C") { field(CALC, "A") field(INPA, "WE CA") }
"#;

fn doubles(v: &[f64]) -> EpicsValue {
    EpicsValue::DoubleArray(v.to_vec().into())
}

async fn put(db: &PvDatabase, v: &[f64]) {
    db.put_record_field_from_ca_no_notify("WE", "VAL", doubles(v))
        .await
        .unwrap();
}

async fn get(ch: &dyn ClientChannel, count: u32) -> Result<EpicsValue, String> {
    ch.get(DBR_TIME_DOUBLE, count).await.map(|s| s.value)
}

/// A subscription of `count` elements, and the values it delivers.
async fn subscribe(
    ch: &dyn ClientChannel,
    count: u32,
) -> (ClientSubscription, mpsc::UnboundedReceiver<EpicsValue>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
    let callback = Box::new(move |s: epics_base_rs::server::snapshot::Snapshot| {
        let _ = tx.send(s.value);
    });
    let sub = ch
        .subscribe(DBR_TIME_DOUBLE, count, mask, callback)
        .await
        .unwrap();
    (sub, rx)
}

/// The next event of the count 1 and the count 4 subscription.
async fn next(
    one: &mut mpsc::UnboundedReceiver<EpicsValue>,
    four: &mut mpsc::UnboundedReceiver<EpicsValue>,
) -> (EpicsValue, EpicsValue) {
    (one.recv().await.unwrap(), four.recv().await.unwrap())
}

#[epics_macros_rs::epics_test]
async fn a_counted_request_is_truncated_or_zero_padded() {
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = std::sync::Arc::clone(ioc.database());
    assert!(ioc.run().await);
    let ch = db.ca_client().create_channel("WE").await.expect("WE");
    let (_one, mut one) = subscribe(&*ch, 1).await;
    let (_four, mut four) = subscribe(&*ch, 4).await;

    let born_empty = (next(&mut one, &mut four).await, get(&*ch, 1).await);
    put(&db, &[5.0, 6.0]).await;
    let full = (
        next(&mut one, &mut four).await,
        get(&*ch, 1).await,
        get(&*ch, 3).await,
        get(&*ch, 0).await,
    );
    put(&db, &[]).await;
    let emptied = (
        next(&mut one, &mut four).await,
        get(&*ch, 1).await,
        get(&*ch, 0).await,
    );
    assert_eq!(
        (born_empty, full, emptied),
        (
            (
                (EpicsValue::Double(0.0), doubles(&[0.0; 4])),
                Ok(EpicsValue::Double(0.0)),
            ),
            (
                (EpicsValue::Double(5.0), doubles(&[5.0, 6.0, 0.0, 0.0])),
                Ok(EpicsValue::Double(5.0)),
                Ok(doubles(&[5.0, 6.0, 0.0])),
                Ok(doubles(&[5.0, 6.0])),
            ),
            (
                (EpicsValue::Double(0.0), doubles(&[0.0; 4])),
                Ok(EpicsValue::Double(0.0)),
                Ok(doubles(&[])),
            ),
        ),
        "per phase (WE born empty, [5, 6], emptied): the count 1 and count 4 \
         events, then gets of count 1, 3 and 0"
    );

    let (_, _, link) = db
        .record_link_fields("C")
        .into_iter()
        .find(|(f, _, _)| f == "INPA")
        .unwrap();
    let link = link.external().unwrap();
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while db.external_link_metadata(link).is_none() && std::time::Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    let meta = db.external_link_metadata(link).expect("INPA connects");
    assert_eq!(
        (meta.units.as_deref(), meta.precision),
        (Some("mm"), Some(3)),
        "INPA's cached units and precision of an empty WE"
    );
}
