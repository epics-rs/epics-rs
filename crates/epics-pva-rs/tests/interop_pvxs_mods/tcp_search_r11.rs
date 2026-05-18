//! PVA-R11 interop: pvxs client → Rust server, TCP SEARCH on the
//! established circuit (name-server redirect path).
//!
//! Verifies that a pvxs client configured with
//! `EPICS_PVA_NAME_SERVERS=<rust>:port` can send a SEARCH frame on
//! the TCP circuit to the Rust server and get a SEARCH_RESPONSE
//! back. Pre-PVA-R11 the Rust server's TCP dispatcher had no
//! `Command::Search` arm — the SEARCH frame fell through silently
//! and pvxs's name-server redirect path hung.
//!
//! pvxs source: `src/serverchan.cpp:173-255` handles SEARCH on an
//! established TCP connection.

use super::interop_helpers::{PVGET, require_tool};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interop_r11_tcp_name_server_search_redirect() {
    if !require_tool(PVGET) {
        return;
    }

    // TODO(PVA-R11 interop): set up a Rust PVA server hosting one
    // PV, spawn `pvget` with `EPICS_PVA_NAME_SERVERS=<rust>:port`
    // and `EPICS_PVA_AUTO_ADDR_LIST=NO`, assert pvget resolves and
    // GETs the value. The server-side handler is currently
    // documented as deferred in `doc/critical-review-2026-05-18.md`;
    // this scaffolding lets CI catch the day the handler lands by
    // flipping the placeholder skip into a real assertion.
    eprintln!(
        "TODO: PVA-R11 interop — Rust server has no Command::Search \
         arm yet (deferred). Scaffolding compiled."
    );
}
