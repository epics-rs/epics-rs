//! 1500 `A.BAUD` puts in a burst while the port sits in a read. C
//! `queueRequest` appends to an unbounded list (asynManager.c:1610-1617), so the
//! C `test` IOC refuses none of them; ERRS only ever carries the read's timeout
//! or the BAUD option's error. A capacity on the port's queue refused every put
//! past it, and `special()` reports a refusal in the put itself (asynRecord.c:
//! 571-578).
//!
//! How often ERRS posts is not C's to fix: every put's `special()` clears it
//! (`resetError`, :390), a special sets it from the port thread as it ends
//! (:834-900) and the read's completion from a callback thread later
//! (:819-827), so the posts follow how the threads interleave.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use asyn_rs::asyn_record::AsynRecord;
use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::registry::register_port;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const OPTION_ERROR: &str = "Error setting option, Unsupported key \"baud\"";

#[epics_base_rs::epics_test]
async fn special_puts_behind_a_blocked_read_are_never_refused() {
    // A device that takes the connection and never answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let _held = listener.accept();
        std::thread::sleep(Duration::from_secs(30));
    });
    let driver = DrvAsynIPPort::new("SPQBR", &addr.to_string()).unwrap();
    let (rt, _jh) = create_port_runtime_unclaimed(driver, RuntimeConfig::default()).unwrap();
    register_port(
        "SPQBR",
        rt.port_handle().clone(),
        &asyn_rs::registry::open_owner(),
    )
    .unwrap();

    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string(
            "record(asyn, \"A\") {\n  field(PORT, \"SPQBR\")\n  field(TMOD, \"Read\")\n  \
             field(TMOT, \"5\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("A").unwrap();
    let errs_text = || {
        let Some(EpicsValue::CharArray(buf)) = cell.read().record.get_field("ERRS") else {
            panic!("ERRS is a char array");
        };
        String::from_utf8_lossy(buf.split(|b| *b == 0).next().unwrap()).into_owned()
    };

    db.put_record_field_from_ca_no_notify("A", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    assert!(cell.read().is_processing(), "precondition: the read is out");
    let put_baud = || async {
        db.put_record_field_from_ca_no_notify("A", "BAUD", EpicsValue::String("9600".into()))
            .await
            .unwrap();
        let text = errs_text();
        assert!(
            text.is_empty() || text == OPTION_ERROR || text.starts_with("timeout "),
            "no put was refused: {text}"
        );
    };
    for _ in 0..1500 {
        put_baud().await;
    }
    assert!(
        cell.read().is_processing(),
        "precondition: the burst beat the read"
    );

    // A put after the read has completed ends every interleaving on the
    // option's error, so the queued specials have all run.
    let deadline = Instant::now() + Duration::from_secs(15);
    while cell.read().is_processing() && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    put_baud().await;
    while errs_text() != OPTION_ERROR && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(errs_text(), OPTION_ERROR);
}
