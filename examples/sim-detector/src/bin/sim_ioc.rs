//! SimDetector IOC binary.
//!
//! Usage:
//!   cargo run --bin sim_ioc --features ioc -- ioc/st.cmd

use ad_plugins::ioc::AdIoc;
use epics_base_rs::error::CaResult;

#[tokio::main]
async fn main() -> CaResult<()> {
    let mut ioc = AdIoc::new();
    sim_detector::ioc_support::register(&mut ioc);
    ioc.run_from_args().await
}
