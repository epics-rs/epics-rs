//! Helpers shared across `interop_pvxs.rs` sub-modules. Mirrors the
//! `tests/common/mod.rs` pattern used by `epics-ca-rs`.

#![allow(dead_code)]

use std::process::{Child, Command, Stdio};

/// Returns true when the named binary resolves on PATH.
pub fn have_tool(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Skip the test with a SKIP-prefixed stderr line when `name` is
/// missing. Mirrors `epics-ca-rs::common::require_tool`.
pub fn require_tool(name: &str) -> bool {
    if have_tool(name) {
        true
    } else {
        eprintln!(
            "SKIP: `{name}` not on PATH; install pvxs (`~/codes/pvxs` per the audit doc) \
             to run this interop test."
        );
        false
    }
}

/// Spawn a child process the test will tear down when dropped.
pub struct DropChild {
    pub child: Child,
}

impl Drop for DropChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// pvxs ships these CLI binaries (built from `~/codes/pvxs/tools/`):
/// - `pvget`     — single GET
/// - `pvput`     — single PUT
/// - `pvmonitor` — subscribe stream
/// - `pvlist`    — discovery / list channels on a server
/// - `softIocPVA` — PVA-only soft IOC
///
/// The interop matrix assumes they're on PATH (the EPICS install
/// script puts them in `${EPICS_BASE}/bin/${EPICS_HOST_ARCH}/`).
pub const PVGET: &str = "pvget";
pub const PVPUT: &str = "pvput";
pub const PVMONITOR: &str = "pvmonitor";
pub const PVLIST: &str = "pvlist";
pub const SOFT_IOC_PVA: &str = "softIocPVA";
