//! ERRS on a record with no port, against the asyn R4-46 oracle IOC
//! (oracleIoc): `camonitor -m v|a|l AS.ERRS` on `record(asyn, "AS") {}` driven
//! with `caput AS.PROC 1` three times posts "Not connect to a port" once, on
//! DBE_VALUE and DBE_LOG, and DBE_ALARM sees nothing after connect.
//!
//! C `process()` takes the `stateNoDevice` arm (asynRecord.c:355-357), which
//! calls `reportError` and never `resetError` — that is the `stateIdle` arm's
//! (:339). `reportError` posts only when the text moved off `old.errs`
//! (:2044-2048), so the second and third cycles post nothing.
//!
//! ERRS posts by reference (`dbfl_type_ref`): an event carries no copy, so
//! the pin counts events per class and reads the text from the field.

use std::collections::HashMap;
use std::time::Duration;

use asyn_rs::asyn_record::AsynRecord;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn settle() {
    for _ in 0..40 {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

fn text(value: &EpicsValue) -> String {
    let bytes: Vec<u8> = match value {
        EpicsValue::CharArray(b) => b.to_vec(),
        EpicsValue::String(s) => s.as_bytes().to_vec(),
        other => panic!("ERRS: {other:?}"),
    };
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[epics_base_rs::epics_test]
async fn a_portless_record_posts_errs_once_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string("record(asyn, \"AS\") {\n}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("AS").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let mut readers: Vec<_> = {
        let mut inst = cell.write();
        classes
            .iter()
            .enumerate()
            .map(|(sid, class)| {
                inst.add_subscriber("ERRS", sid as u32, DbFieldType::Char, class.bits())
                    .unwrap()
            })
            .collect()
    };
    for _ in 0..3 {
        db.put_record_field_from_ca("AS", "PROC", EpicsValue::String("1".into()))
            .await
            .unwrap();
        settle().await;
    }
    let got: Vec<usize> = readers
        .iter_mut()
        .map(|reader| std::iter::from_fn(|| reader.try_recv().ok()).count())
        .collect();
    assert_eq!(
        got,
        [1, 0, 1],
        "ERRS events on DBE_VALUE, DBE_ALARM, DBE_LOG"
    );
    let errs = cell.read().record.get_field("ERRS").unwrap();
    assert_eq!(text(&errs), "Not connect to a port");
}
