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

/// One batched tool's per-PV reading, or why that PV has none.
pub type Readings = Vec<Result<String, ToolError>>;

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
    beacon_port: u16,
    side: Side,
}

impl PvaTools {
    pub fn new(tools: &PvxTools, port: u16, side: Side) -> Self {
        Self {
            bin: tools.bin.clone(),
            port,
            beacon_port: tools.beacon_port,
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
    ///
    fn env(&self) -> HashMap<&'static str, String> {
        HashMap::from([
            // Explicit `addr:port` — `split_addr_into` (config.cpp:580) takes
            // the port from the entry, so no default-port var is consulted.
            ("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", self.port)),
            // Never let a search escape to a real subnet, or be answered by
            // the other side of the pair.
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO".to_string()),
            // Beacon RX on a port nothing beacons to, so no foreign server can
            // reach this client. Not the side's port — the client binds this,
            // and the Rust side's socket exclusively owns its own. See
            // `PvxTools::beacon_port`.
            ("EPICS_PVA_BROADCAST_PORT", self.beacon_port.to_string()),
        ])
    }

    /// Spawn `tool`, wait for it bounded, and hand back its raw output.
    ///
    /// The one place a pvxs client is spawned, so the client environment and the
    /// wall-clock bound cannot drift between the single and batched paths.
    fn run_raw(&self, tool: &str, args: &[String]) -> Result<std::process::Output, ToolError> {
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
        wait_bounded(child, TOOL_TIMEOUT).map_err(|e| self.err(tool, e))
    }

    /// Run a tool to completion, returning stdout on success.
    fn run(&self, tool: &str, args: &[String]) -> Result<String, ToolError> {
        let out = self.run_raw(tool, args)?;

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

    /// Run a tool, keeping stdout **regardless of exit status**.
    ///
    /// Only a failure to run at all (spawn, timeout) is an error here. A
    /// non-zero exit is not, because for a batch it does not mean what it means
    /// for a single PV: see [`Self::batch`].
    fn run_partial(&self, tool: &str, args: &[String]) -> Result<(String, String), ToolError> {
        let out = self.run_raw(tool, args)?;
        Ok((
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }

    /// Read many PVs in one invocation, attributing each reading to its own PV.
    ///
    /// # Why this is not [`crate::runner::probe_bisect`]
    ///
    /// The C CA tools are all-or-nothing: one unconnectable PV and `caget`
    /// prints *nothing at all*, so the only way to isolate the bad one is to
    /// split the batch and retry. The pvxs tools are not. Measured — a batch of
    /// three where the middle PV does not exist exits 1 and prints
    /// `Timeout with 1 outstanding` on stderr, and **still prints a full block
    /// for each of the other two**. So the good PVs need no retry and the bad
    /// one is identified by the absence of its block, at one spawn per batch
    /// rather than O(k log n). Attribution is by name, not by position, so it
    /// survives the tools answering out of order.
    ///
    /// This is why a non-zero exit must not fail the whole batch: doing so
    /// would throw away readings that were successfully obtained and turn one
    /// missing PV into `n` ERRORs — manufacturing unmeasured cases out of a
    /// measured run.
    fn batch(&self, tool: &str, pvs: &[String], header: fn(&str) -> Option<&str>) -> Readings {
        if pvs.is_empty() {
            return Vec::new();
        }
        let mut args = vec!["-w".to_string(), PVA_TIMEOUT_SECS.to_string()];
        args.extend(pvs.iter().cloned());

        let (stdout, stderr) = match self.run_partial(tool, &args) {
            Ok(x) => x,
            // The tool never ran: nothing was measured, so every PV in the
            // batch is an ERROR carrying the same cause.
            Err(e) => return pvs.iter().map(|_| Err(e.clone())).collect(),
        };

        split_blocks(pvs, &stdout, header)
            .into_iter()
            .map(|b| match b {
                // A header with an empty body is not a reading. Same rule as
                // `run`: a tool that printed nothing did not look.
                Some(t) if !t.trim().is_empty() => Ok(t.trim_end().to_string()),
                Some(_) => Err(self.err(tool, "reported this PV with no body")),
                None => Err(self.err(
                    tool,
                    if stderr.is_empty() {
                        "no output for this PV in the batch".to_string()
                    } else {
                        stderr.clone()
                    },
                )),
            })
            .collect()
    }

    /// `pvxget` for many PVs at once — the value, and which fields the reply
    /// marked. One entry per requested PV, in the order requested.
    pub fn pvxget_batch(&self, pvs: &[String]) -> Readings {
        self.batch("pvxget", pvs, pvxget_header)
    }

    /// `pvxinfo` for many PVs at once — the declared type, without any value.
    /// One entry per requested PV, in the order requested.
    pub fn pvxinfo_batch(&self, pvs: &[String]) -> Readings {
        self.batch("pvxinfo", pvs, pvxinfo_header)
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
    ///
    /// Lines are deduplicated, because the question is how many *distinct*
    /// servers answered — a server that replies twice is still one server, and
    /// it prints its own address both times. The count is scoped to this port
    /// by [`PvxTools::beacon_port`]: without that, `pvxlist` also reports
    /// beacon-discovered servers, and a foreign IOC elsewhere on the host
    /// would be miscounted as a second server here.
    pub fn server_count(&self) -> Result<usize, ToolError> {
        let out = self.run("pvxlist", &["-w".into(), PVA_TIMEOUT_SECS.into()])?;
        let distinct: std::collections::BTreeSet<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        Ok(distinct.len())
    }
}

/// The PV a `pvxget` block header announces: the bare PV name, at column 0.
/// Every line of a value body is indented, so an unindented line is a header
/// candidate — and only counts as one if it names a PV that was asked for.
fn pvxget_header(line: &str) -> Option<&str> {
    if line.starts_with([' ', '\t']) || line.trim().is_empty() {
        return None;
    }
    Some(line)
}

/// The PV a `pvxinfo` block header announces, out of `<pv> from <addr>:<port>`.
///
/// The ` from ` guard is what keeps `ORACLE:AI` from swallowing
/// `ORACLE:AI.SCAN`'s header, and what keeps the `struct "..." {` line — also at
/// column 0 — from being mistaken for a header.
fn pvxinfo_header(line: &str) -> Option<&str> {
    if line.starts_with([' ', '\t']) {
        return None;
    }
    line.split_once(" from ").map(|(pv, _)| pv)
}

/// Split a batched tool's stdout into one block per requested PV, keyed by name.
///
/// The header line is **not** part of the block, and for `pvxinfo` that is
/// load-bearing rather than cosmetic. Its header is `<pv> from 127.0.0.1:<port>`
/// — and the two sides can never agree on that port, because this harness
/// assigns them and [`crate::ioc::PvaPair::boot`] *refuses to run* if they ever
/// match. Comparing the header would therefore mark every channel a DEFECT for
/// a difference the harness itself created, which is the one normalization the
/// module docs' "declare it with evidence" rule plainly earns. Nothing else is
/// normalized: the type declaration and the value text are compared verbatim.
///
/// A PV with no block gets `None` — the caller turns that into an ERROR for
/// that PV alone, never for its batch-mates.
fn split_blocks(
    pvs: &[String],
    out: &str,
    header: fn(&str) -> Option<&str>,
) -> Vec<Option<String>> {
    let index: HashMap<&str, usize> = pvs
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i))
        .collect();
    let mut blocks: Vec<Option<String>> = vec![None; pvs.len()];
    let mut cur: Option<usize> = None;

    for line in out.lines() {
        if let Some(name) = header(line)
            && let Some(&i) = index.get(name)
        {
            cur = Some(i);
            blocks[i] = Some(String::new());
            continue;
        }
        if let Some(i) = cur
            && let Some(b) = blocks[i].as_mut()
        {
            b.push_str(line);
            b.push('\n');
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pvs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// Verbatim `pvxget` output for three PVs, as `softIocPVX` printed it.
    const GET_OUT: &str = "ORACLE:AI\n    value double = 0\n    alarm.severity int32_t = 3\nORACLE:AI.SCAN\n    value.index int32_t = 0\nORACLE:BI\n    value.index int32_t = 0\n";

    /// Verbatim `pvxinfo` output for two PVs. Note the ports in the headers.
    const INFO_OUT: &str = "ORACLE:AI from 127.0.0.1:45627\nstruct \"epics:nt/NTScalar:1.0\" {\n    double value\n}\nORACLE:AI.SCAN from 127.0.0.1:45627\nstruct \"epics:nt/NTEnum:1.0\" {\n    struct \"enum_t\" {\n        int32_t index\n    } value\n}\n";

    #[test]
    fn pvxget_blocks_are_split_by_pv_and_exclude_the_header() {
        let list = pvs(&["ORACLE:AI", "ORACLE:AI.SCAN", "ORACLE:BI"]);
        let b = split_blocks(&list, GET_OUT, pvxget_header);
        assert_eq!(
            b[0].as_deref().unwrap().trim_end(),
            "    value double = 0\n    alarm.severity int32_t = 3"
        );
        assert_eq!(
            b[1].as_deref().unwrap().trim_end(),
            "    value.index int32_t = 0"
        );
        assert!(b[2].is_some());
        assert!(
            !b[0].as_deref().unwrap().contains("ORACLE:AI"),
            "the header names the PV; it is the separator, not part of the reading"
        );
    }

    /// The port in `pvxinfo`'s header differs between the two sides **by
    /// construction**, so it must never reach the compared text.
    #[test]
    fn pvxinfo_blocks_drop_the_header_and_with_it_the_server_port() {
        let list = pvs(&["ORACLE:AI", "ORACLE:AI.SCAN"]);
        let b = split_blocks(&list, INFO_OUT, pvxinfo_header);
        let ai = b[0].as_deref().unwrap();
        assert!(ai.starts_with("struct \"epics:nt/NTScalar:1.0\" {"));
        assert!(
            !ai.contains("45627") && !ai.contains("127.0.0.1"),
            "the server port must not be comparable text: got {ai:?}"
        );
        assert!(b[1].as_deref().unwrap().contains("NTEnum"));
    }

    /// A PV name that is a prefix of another must not steal its block.
    #[test]
    fn a_prefix_pv_name_does_not_swallow_a_longer_one() {
        let list = pvs(&["ORACLE:AI", "ORACLE:AI.SCAN"]);
        let b = split_blocks(&list, INFO_OUT, pvxinfo_header);
        assert!(b[0].as_deref().unwrap().contains("NTScalar"));
        assert!(b[1].as_deref().unwrap().contains("NTEnum"));
    }

    /// The behaviour the batch design rests on: pvxs prints the good PVs even
    /// when another PV in the same batch never answers. The missing one must be
    /// the ONLY one reported absent.
    #[test]
    fn one_absent_pv_does_not_cost_its_batch_mates_their_readings() {
        let list = pvs(&["ORACLE:AI", "ORACLE:NOPE", "ORACLE:AI.SCAN"]);
        let b = split_blocks(&list, GET_OUT, pvxget_header);
        assert!(b[0].is_some(), "a good PV keeps its reading");
        assert!(b[1].is_none(), "the absent PV is the one reported absent");
        assert!(b[2].is_some(), "and its batch-mates are untouched");
    }

    /// Attribution is by name, so it survives the tools answering out of order.
    #[test]
    fn blocks_are_attributed_by_name_not_by_position() {
        let list = pvs(&["ORACLE:BI", "ORACLE:AI"]);
        let b = split_blocks(&list, GET_OUT, pvxget_header);
        assert!(b[0].as_deref().unwrap().contains("value.index"), "BI");
        assert!(b[1].as_deref().unwrap().contains("value double"), "AI");
    }

    #[test]
    fn an_empty_request_reads_nothing() {
        assert!(split_blocks(&[], GET_OUT, pvxget_header).is_empty());
    }
}
