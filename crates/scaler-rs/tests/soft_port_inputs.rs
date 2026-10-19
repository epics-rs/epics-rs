//! A soft port's input counts whatever field type it names. A string or array
//! field fails C's `useValque` test, so its post is a log that holds no value
//! (`dbfl_type_ref`) and the port reads the field at delivery, as C's
//! `dbGetField` reads it on every scaler read.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use scaler_rs::MAX_SCALER_CHANNELS;
use scaler_rs::device_support::soft_port::SoftScalerPorts;

const DB: &str = r#"
record(waveform, "IN1") { field(FTVL, "LONG") field(NELM, "1") }
record(stringin, "IN2") {}
record(ai, "IN3") {}
record(longout, "IN4") { field(MDEL, "10") }
"#;

async fn port() -> (Arc<SoftScalerPorts>, Arc<PvDatabase>) {
    let ports = Arc::new(SoftScalerPorts::default());
    ports.configure("P", 4, "IN%d").unwrap();
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    ports.connect(&db);
    (ports, db)
}

/// Waits, on the port's delivery signal, until channel `ch` counts `want`.
async fn counts(ports: &SoftScalerPorts, ch: usize, want: u32) {
    let driver = ports.device_support("P").unwrap().driver();
    let mut delivered = ports.deliveries("P").unwrap();
    let wait = async {
        loop {
            delivered.borrow_and_update();
            let mut counts = [0u32; MAX_SCALER_CHANNELS];
            driver.lock().unwrap().read(&mut counts).unwrap();
            if counts[ch] == want {
                return;
            }
            delivered.changed().await.unwrap();
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .unwrap_or_else(|_| panic!("channel {ch} never counted {want}"));
}

async fn put(db: &PvDatabase, record: &str, value: EpicsValue) {
    db.put_record_field_from_ca(record, "VAL", value)
        .await
        .unwrap();
}

// RTEMS-EXEC-MODEL-ALLOW(4): checked, not waived — ran and passed on the exec
// backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p scaler-rs
// --test soft_port_inputs`). No CA server is built; the reactor comes from
// `#[tokio::test]`, which the backend does not remove.
#[tokio::test(flavor = "multi_thread")]
async fn a_one_element_waveform_input_updates_its_count() {
    let (ports, db) = port().await;
    put(&db, "IN1", EpicsValue::LongArray(vec![7].into())).await;
    counts(&ports, 0, 7).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stringin_input_updates_its_count() {
    let (ports, db) = port().await;
    put(&db, "IN2", EpicsValue::String("12".into())).await;
    counts(&ports, 1, 12).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ai_input_updates_its_count() {
    let (ports, db) = port().await;
    put(&db, "IN3", EpicsValue::Double(5.0)).await;
    counts(&ports, 2, 5).await;
}

/// C's arm writes 0 to each input and the next read takes the input live. A
/// longout at 5 with MDEL 10 posts nothing for the zero, so the 5 it posted
/// before the arm, still queued, is not the input's count after it.
#[tokio::test(flavor = "multi_thread")]
async fn an_arm_whose_zero_posts_nothing_counts_zero() {
    let (ports, db) = port().await;
    let driver = ports.device_support("P").unwrap().driver();
    let mut delivered = ports.deliveries("P").unwrap();
    let read = || {
        let mut counts = [0u32; MAX_SCALER_CHANNELS];
        driver.lock().unwrap().read(&mut counts).unwrap();
        counts[3]
    };

    // Held, this stops the delivery task at its next delivery signal.
    let stop = delivered.borrow_and_update();
    let before = *stop;
    put(&db, "IN4", EpicsValue::Long(30)).await;
    // The task has stored the 30 and waits on `stop`.
    tokio::time::timeout(Duration::from_secs(5), async {
        while read() != 30 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the 30 was never delivered");
    put(&db, "IN4", EpicsValue::Long(5)).await;
    driver.lock().unwrap().arm(true).unwrap();
    drop(stop);

    // The 30's signal, then the arm's.
    tokio::time::timeout(
        Duration::from_secs(5),
        delivered.wait_for(|n| *n >= before + 2),
    )
    .await
    .expect("the arm's zeros were never written")
    .unwrap();
    assert_eq!(read(), 0, "IN4's count after the arm");
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the exec
// backend, as the four above. No CA server; `#[tokio::test]` gives the reactor.
/// C's arm zeroes each input with `dbPutField` (drvScalerSoft.c:321), which
/// writes the zero into a busy input and raises RPRO (dbAccess.c:1263-1273);
/// only a put-notify waits out the busy cycle unwritten. A calcout held in its
/// ODLY cycle, its VAL 7 unposted until the cycle ends, therefore counts 0 at
/// the arm's read.
#[tokio::test(flavor = "multi_thread")]
async fn an_arm_writes_the_zero_into_a_busy_input() {
    let ports = Arc::new(SoftScalerPorts::default());
    ports.configure("B", 1, "BUSY%d").unwrap();
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(calcout, "BUSY1") { field(CALC, "A") field(ODLY, "5") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    ports.connect(&db);
    let driver = ports.device_support("B").unwrap().driver();
    let mut delivered = ports.deliveries("B").unwrap();
    let read = || {
        let mut counts = [0u32; MAX_SCALER_CHANNELS];
        driver.lock().unwrap().read(&mut counts).unwrap();
        counts[0]
    };

    db.put_record_field_from_ca_no_notify("BUSY1", "A", EpicsValue::Long(7))
        .await
        .unwrap();
    let rec = db.get_record(b"BUSY1").unwrap();
    assert!(rec.read().is_processing(), "ODLY holds the calcout busy");
    let before = *delivered.borrow_and_update();
    driver.lock().unwrap().arm(true).unwrap();
    tokio::time::timeout(Duration::from_secs(4), delivered.wait_for(|n| *n > before))
        .await
        .expect("the arm's zeros were never written")
        .unwrap();
    assert!(rec.read().is_processing(), "still inside the ODLY cycle");
    assert_eq!(read(), 0, "BUSY1's count after the arm");
}
