//! SimDetector IOC binary — CA + PVA dual-protocol.
//!
//! Usage:
//!   cargo run --bin sim_ioc --features ioc -- ioc/st.cmd
// On `exec_backend` this program's `main` refuses instead of running, so
// everything below is unreachable in that configuration by construction.
// The default build still lints the file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

#[cfg(tokio_backend)]
use ad_plugins_rs::ioc::AdIoc;
use epics_base_rs::error::CaResult;

#[cfg(tokio_backend)]
#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    let mut ioc = AdIoc::new();
    sim_detector::ioc_support::register(&mut ioc);
    ioc.run_from_args_with_pva().await
}

/// The `exec_backend` arm: `ad_plugins_rs::ioc` is compiled only on the tokio
/// backend, so there is no IOC to build here.
#[cfg(exec_backend)]
fn main() -> CaResult<()> {
    eprintln!(
        "sim_ioc needs the tokio backend; this build selects the \
         reactor-free execution model (EPICS_RS_BUILD_EXEC_BACKEND=thread)."
    );
    Ok(())
}
