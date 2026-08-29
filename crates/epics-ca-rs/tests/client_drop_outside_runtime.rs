//! Regression test: dropping a `CaClient` from a thread with no ambient
//! tokio reactor must not panic.
//!
//! `CaClient::drop` routes its graceful drain through
//! `runtime::task::spawn`, which on the tokio backend calls
//! `tokio::spawn` and panics without a reactor context. That is exactly
//! the state of a sync `main` unwinding on an error: the drop panic
//! aborts the unwind and masks the error that caused it (observed as a
//! robot daemon dying with "there is no reactor running" instead of its
//! real bring-up failure). Post-fix the drop degrades to a sync
//! shutdown send plus task aborts; the panic-free property is the test.

#![cfg(feature = "client-core")]

// RTEMS-EXEC-MODEL-ALLOW(1): measured, not argued — all 1 case(s) here run and
// pass under `EPICS_RS_BUILD_EXEC_BACKEND=thread`. The file-level gate removed
// with this marker asserted a reactor panic the exec backend does not produce,
// and while it stood the exec-backend suite could not see this file at all.

use epics_ca_rs::client::CaClient;
use serial_test::serial;

/// SAFETY: `#[serial]` — no other test mutates the environment
/// concurrently, and the env is set before `CaClient::new()` snapshots
/// its resolver configuration. The address list points at loopback so
/// the client never probes the live network.
fn pin_env_to_loopback() {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
    }
}

#[test]
#[serial]
fn client_drop_outside_runtime_does_not_panic() {
    pin_env_to_loopback();
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let client = rt.block_on(async { CaClient::new().await.expect("client") });
    // The dropping thread has no reactor context — `Handle::try_current`
    // fails here. Pre-fix this line panicked inside `CaClient::drop`.
    drop(client);
    // The runtime is still alive, so the coordinator can observe the
    // sync-sent Shutdown before its abort lands; either way the drop
    // above must have returned normally.
    drop(rt);
}
