//! A PUT resolves after the monitor callback for the update the server sent
//! before its reply.
//!
//! pvxs runs a subscription's event callback and a put's result callback on
//! the client loop thread in wire order, so a caller that reads its monitor
//! after the put returns sees the value it wrote. The client woke the put on
//! the reader task while the update still waited for the subscription's
//! drain task, and the callback sometimes still held the previous value.
#![cfg(all(feature = "client", tokio_backend))]

use std::sync::Arc;

use epics_pva_rs::nt::NTScalar;
use epics_pva_rs::pvdata::{PvField, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};
use parking_lot::Mutex;

const PV: &str = "ORDER:X";
const PUTS: u32 = 200;

fn value_of(v: &PvField) -> Option<f64> {
    let PvField::Structure(s) = v else {
        return None;
    };
    match s.fields.iter().find(|(n, _)| n == "value") {
        Some((_, PvField::Scalar(ScalarValue::Double(x)))) => Some(*x),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_resolves_after_its_monitor_callback() {
    let desc = NTScalar::new(ScalarType::Double).build();
    let pv = SharedPV::build_mailbox();
    pv.open(
        desc.clone(),
        epics_pva_rs::pvdata::encode::default_value_for(&desc),
    )
    .expect("open");
    let source = SharedSource::new();
    source.add(PV, pv);
    let server = PvaServer::isolated(Arc::new(source)).expect("isolated server");
    let client = server.client_config();

    let latest = Arc::new(Mutex::new(None));
    let seen = latest.clone();
    let _monitor = client
        .pvmonitor_handle(PV, move |_, value| *seen.lock() = value_of(value), |_| {})
        .await
        .expect("subscribe");
    tokio::time::timeout(fact_budget::FACT_BUDGET, async {
        while latest.lock().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial value");

    for put in 1..=PUTS {
        let value = f64::from(put) + 0.5;
        client.pvput(PV, &value.to_string()).await.expect("put");
        assert_eq!(
            *latest.lock(),
            Some(value),
            "put {put} resolved before its monitor callback ran"
        );
    }
}
