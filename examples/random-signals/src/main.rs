//! Random Signals IOC — four PVs updated every 10ms with random values.
//!
//! PV names:
//!   BLM:001:SA:A, BLM:001:SA:B, BLM:001:SA:C, BLM:001:SA:D
//!
//! Usage:
//!   cargo run --release -p random-signals

use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServer;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

const PV_NAMES: [&str; 4] = [
    "BLM:001:SA:A",
    "BLM:001:SA:B",
    "BLM:001:SA:C",
    "BLM:001:SA:D",
];

#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    let port: u16 = std::env::var("EPICS_CA_SERVER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5064);

    let server = CaServer::builder()
        .port(port)
        .pv(PV_NAMES[0], EpicsValue::Double(0.0))
        .pv(PV_NAMES[1], EpicsValue::Double(0.0))
        .pv(PV_NAMES[2], EpicsValue::Double(0.0))
        .pv(PV_NAMES[3], EpicsValue::Double(0.0))
        .build()
        .await?;

    let db = server.database().clone();

    eprintln!(
        "Random signals IOC started — {} @ 100 Hz",
        PV_NAMES.join(", ")
    );

    // Spawn 10ms update task
    epics_base_rs::runtime::task::spawn(async move {
        let mut interval = epics_base_rs::runtime::task::interval(Duration::from_millis(10));
        let mut rng = SmallRng::from_os_rng();
        loop {
            interval.tick().await;
            for name in &PV_NAMES {
                let val = rng.random_range(-10.0..10.0);
                let _ = db.put_pv(name, EpicsValue::Double(val)).await;
            }
        }
    });

    server.run().await
}
