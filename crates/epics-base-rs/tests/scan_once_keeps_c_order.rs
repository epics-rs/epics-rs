//! C `onceTask` takes the ring FIFO and runs each request's `dbProcess` to
//! its asynchronous return before it takes the next (`dbScan.c:705-719`).

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;

const N: usize = 20;

/// Back-to-back `scanOnce` of records that go asynchronous (calcout ODLY):
/// each one's synchronous part reads the shared counter in request order.
#[epics_macros_rs::epics_test]
async fn back_to_back_scan_once_of_async_records_keeps_request_order() {
    let mut db_text = String::from("record(calc, \"SEQ\") { field(CALC, \"VAL+1\") }\n");
    for i in 0..N {
        db_text.push_str(&format!(
            "record(calcout, \"R{i}\") {{ field(INPA, \"SEQ PP\") field(CALC, \"A\") field(ODLY, \"0.05\") }}\n"
        ));
    }
    let (db, _) = IocBuilder::new()
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for i in 0..N {
        db.scan_once(format!("R{i}").as_bytes());
    }
    let got = || -> Vec<f64> {
        (0..N)
            .map(|i| db.get_pv(format!("R{i}")).unwrap().to_f64().unwrap())
            .collect()
    };
    for _ in 0..500 {
        if db.get_pv("SEQ").unwrap().to_f64().unwrap() == N as f64 {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    let want: Vec<f64> = (1..=N).map(|v| v as f64).collect();
    assert_eq!(got(), want);
}

/// A request whose record is locked elsewhere holds up the ones behind it,
/// as `onceTask` blocks in `dbScanLock` (`dbScan.c:715`): Y, queued after X
/// in another lock set, is not processed until X's set is let go.
#[epics_macros_rs::epics_test]
async fn a_request_waiting_for_its_lock_holds_the_requests_behind_it() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(calc, "X") { field(CALC, "VAL+1") }
record(calc, "Y") { field(CALC, "VAL+1") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let val = |n: &str| db.get_pv(n).unwrap().to_f64().unwrap();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let db = db.clone();
        std::thread::spawn(move || {
            let _held = db.lock_record("X");
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
    };
    locked_rx.recv().unwrap();
    db.scan_once(b"X");
    db.scan_once(b"Y");
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    // X itself cannot be read while its set is held.
    assert_eq!(val("Y"), 0.0, "Y ran past a blocked X");
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    for _ in 0..500 {
        if val("Y") == 1.0 {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!((val("X"), val("Y")), (1.0, 1.0));
}
