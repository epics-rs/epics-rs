//! A PUT resolves while the awaiting task holds, but does not poll, its own
//! `pvmonitor` future.
//!
//! The put's reply settles only after the monitor callback for the update the
//! server sent first has run. That callback ran inside the caller's
//! `pvmonitor` future, so a select loop that awaited the put in one arm
//! stopped polling the future that had to release it, and every put sat out
//! the op timeout before failing.
#![cfg(all(feature = "client", tokio_backend))]

use std::sync::Arc;

use epics_pva_rs::nt::NTScalar;
use epics_pva_rs::pvdata::{PvField, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};
use parking_lot::Mutex;

const PV: &str = "BESIDE:X";

fn value_of(v: &PvField) -> Option<f64> {
    let PvField::Structure(s) = v else {
        return None;
    };
    match s.fields.iter().find(|(n, _)| n == "value") {
        Some((_, PvField::Scalar(ScalarValue::Double(x)))) => Some(*x),
        _ => None,
    }
}

#[tokio::test]
async fn a_put_resolves_beside_its_own_monitor_future() {
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
    let monitor = client.pvmonitor(PV, move |value| *seen.lock() = value_of(value));
    tokio::pin!(monitor);
    tokio::time::timeout(fact_budget::FACT_BUDGET, async {
        tokio::select! {
            end = &mut monitor => panic!("monitor ended: {end:?}"),
            _ = async {
                while latest.lock().is_none() {
                    tokio::task::yield_now().await;
                }
            } => {}
        }
    })
    .await
    .expect("initial value");

    client
        .pvput(PV, "2.5")
        .await
        .expect("put stalled behind the caller's own monitor future");
    assert_eq!(
        *latest.lock(),
        Some(2.5),
        "put resolved before its monitor callback ran"
    );
}
