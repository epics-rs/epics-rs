//! `spawn_blocking` from a callback band, which has no runtime entered, uses
//! the runtime the program started: `#[epics_main]` and `run_phased` each
//! adopt theirs. Without that it falls back to the seam's own `blockingRt`.
//! Process-global state, so each test relies on nextest's process per test.

#![cfg(tokio_backend)]

use epics_base_rs::runtime::task::{CallbackPriority, spawn_background, spawn_blocking};
use epics_base_rs::server::ioc_app::IocApplication;

/// The name of the thread a band's `spawn_blocking` ran on.
async fn blocking_thread_from_a_band() -> String {
    spawn_background(CallbackPriority::Medium, async {
        spawn_blocking(|| std::thread::current().name().unwrap_or("").to_owned())
            .await
            .unwrap()
    })
    .await
    .unwrap()
}

mod program {
    #[epics_base_rs::epics_main]
    pub async fn main() {
        let name = super::blocking_thread_from_a_band().await;
        // tokio's default thread name, which `#[epics_main]` leaves alone.
        assert_eq!(name, "tokio-rt-worker", "not #[epics_main]'s runtime");
    }
}

#[test]
fn epics_main_adopts_its_runtime() {
    program::main();
}

#[test]
fn run_phased_adopts_the_iocs_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name("iocRt")
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(IocApplication::new().run_phased(|_config| async move {
            assert_eq!(blocking_thread_from_a_band().await, "iocRt");
            Ok(())
        }))
        .unwrap();
}
