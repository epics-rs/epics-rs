//! The measuring instrument for the PVA pair: the **pvxs** client tools,
//! pointed at one server.
//!
//! The same principle as [`crate::catool`], one protocol over: both sides of
//! the PVA pair are driven through the same `pvxget`/`pvxinfo`/`pvxlist`
//! binaries out of the pvxs tree. That removes `epics-pva-rs`'s *client* from
//! the experiment — otherwise a client-side bug would surface as a
//! server-side "diff" — and it measures the contract actually owed: a pvxs
//! client must not be able to tell the two servers apart.
//!
//! Every invocation is timeout-bounded and every non-zero exit becomes a
//! [`ToolError`], never an empty-but-successful reading.
//!
//! # Why the client env is not the obvious one
//!
//! The natural shape — `EPICS_PVA_ADDR_LIST=127.0.0.1` plus
//! `EPICS_PVA_BROADCAST_PORT=<that side's port>` — is **not** used, because it
//! works against only one of the two sides and the instrument must be
//! identical for both.
//!
//! `EPICS_PVA_BROADCAST_PORT` does two jobs in a pvxs client (`config.cpp:556`,
//! `client.cpp:641`): it supplies the default port for address-list entries,
//! *and* it is the port the client **binds** to receive beacons. That bind is
//! fatal to the tool if it fails. It succeeds against `softIocPVX`, whose
//! search socket always carries `SO_REUSEPORT`; it fails with `Address already
//! in use` against `oracle-ioc --pva`, because `epics-base-rs` deliberately
//! omits the reuse flags for a kernel-assigned port so the socket exclusively
//! owns it (`async_udp_v4.rs`, `if port != 0`). Measured: `pvxget` prints
//! nothing and dies.
//!
//! So the port is instead named **explicitly in the address list**
//! (`127.0.0.1:<port>`), which makes the search target unambiguous without
//! binding anything, and `EPICS_PVA_BROADCAST_PORT` is left alone. The client
//! then binds pvxs's default beacon port, which is the well-known
//! fanout-shareable co-bind case those flags exist for. Beacons are irrelevant
//! here anyway: with `EPICS_PVA_AUTO_ADDR_LIST=NO` and an explicit unicast
//! target, a search can only ever reach the one server we mean to measure.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::catool::{ToolError, wait_bounded};
use crate::ioc::{PvxTools, Side};

/// Wall-clock cap on any single tool invocation, mirroring
/// [`crate::catool`]'s. Bounds the process, not just the PVA operation.
const TOOL_TIMEOUT: Duration = Duration::from_secs(8);
/// PVA connect/operation timeout handed to the tools via `-w`.
const PVA_TIMEOUT_SECS: &str = "3";

/// The pvxs client tools aimed at exactly one PVA server.
#[derive(Debug, Clone)]
pub struct PvaTools {
    bin: PathBuf,
    port: u16,
    side: Side,
}

impl PvaTools {
    pub fn new(tools: &PvxTools, port: u16, side: Side) -> Self {
        Self {
            bin: tools.bin.clone(),
            port,
            side,
        }
    }

    /// Which server these tools are aimed at.
    pub fn side(&self) -> Side {
        self.side
    }

    /// The UDP search port these tools search.
    pub fn port(&self) -> u16 {
        self.port
    }

    fn err(&self, tool: &str, message: impl Into<String>) -> ToolError {
        ToolError {
            side: self.side,
            tool: tool.to_string(),
            message: message.into(),
        }
    }

    /// The client environment. See the module docs for why the port rides in
    /// the address list rather than in `EPICS_PVA_BROADCAST_PORT`.
    fn env(&self) -> HashMap<&'static str, String> {
        HashMap::from([
            // Explicit `addr:port` — `split_addr_into` (config.cpp:580) takes
            // the port from the entry, so no default-port var is consulted.
            ("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", self.port)),
            // Never let a search escape to a real subnet, or be answered by
            // the other side of the pair.
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO".to_string()),
        ])
    }

    /// Run a tool to completion, returning stdout on success.
    fn run(&self, tool: &str, args: &[String]) -> Result<String, ToolError> {
        let mut cmd = Command::new(self.bin.join(tool));
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        let child = cmd
            .spawn()
            .map_err(|e| self.err(tool, format!("spawn: {e}")))?;
        let out = wait_bounded(child, TOOL_TIMEOUT).map_err(|e| self.err(tool, e))?;

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() {
            let msg = if stderr.is_empty() {
                stdout.trim().to_string()
            } else {
                stderr
            };
            return Err(self.err(
                tool,
                if msg.is_empty() {
                    format!("exit {:?}", out.status.code())
                } else {
                    msg
                },
            ));
        }
        // pvxs tools can exit 0 having printed only a diagnostic — a timed-out
        // GET reports `Timeout` on stderr and still succeeds. Treating that as
        // a reading would score a case AGREED on two empty strings, which is
        // the "exit 0 when you could not look" failure this harness exists to
        // prevent.
        if stdout.trim().is_empty() {
            return Err(self.err(
                tool,
                if stderr.is_empty() {
                    "tool exited 0 but printed nothing".to_string()
                } else {
                    stderr
                },
            ));
        }
        Ok(stdout)
    }

    /// `pvxget <pv>` — the value as a pvxs client renders it.
    ///
    /// The default output format is deliberate: it is what a client actually
    /// sees, whitespace and all. The read phase compares it as text rather
    /// than parsing it into a model first, so a difference the port introduces
    /// cannot be normalized away by the harness before anyone sees it.
    pub fn pvxget(&self, pv: &str) -> Result<String, ToolError> {
        let out = self.run(
            "pvxget",
            &["-w".into(), PVA_TIMEOUT_SECS.into(), pv.to_string()],
        )?;
        Ok(out.trim_end().to_string())
    }

    /// `pvxinfo <pv>` — the channel's introspected type, without its value.
    pub fn pvxinfo(&self, pv: &str) -> Result<String, ToolError> {
        let out = self.run(
            "pvxinfo",
            &["-w".into(), PVA_TIMEOUT_SECS.into(), pv.to_string()],
        )?;
        Ok(out.trim_end().to_string())
    }

    /// How many distinct PVA servers answer a search on this side's port.
    ///
    /// **This is the PVA port-exclusivity check, and it has no CA analogue.**
    /// A CA port collision announces itself (`cas WARNING: ... two or more
    /// servers share the same UDP port`), so [`crate::ioc::CIoc`] can scan
    /// startup output for it. A PVA search socket sets `SO_REUSEPORT`, so a
    /// collision **binds silently** — there is no line to scan for, and two
    /// servers then answer searches at random. Measured directly: two
    /// `softIocPVX` on one port, six `pvxget` of the same PV, values
    /// `1,2,2,1,1,2`.
    ///
    /// Since both sides of this pair serve the *same PV names* from the same
    /// `.db`, that failure would silently misattribute every reading. So
    /// exclusivity is established **positively, by measurement**: `pvxlist`
    /// prints one line per server that answers, and anything but exactly one
    /// means the port is not ours alone.
    pub fn server_count(&self) -> Result<usize, ToolError> {
        let out = self.run("pvxlist", &["-w".into(), PVA_TIMEOUT_SECS.into()])?;
        Ok(out.lines().filter(|l| !l.trim().is_empty()).count())
    }
}
