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
//! So the *search target* is instead named **explicitly in the address list**
//! (`127.0.0.1:<port>`), which makes it unambiguous without binding anything.
//! Beacons cannot influence a reading either way: with
//! `EPICS_PVA_AUTO_ADDR_LIST=NO` and an explicit unicast target, a search can
//! only ever reach the one server we mean to measure.
//!
//! `EPICS_PVA_BROADCAST_PORT` is then set to a **quiet** port purely to keep the
//! client's beacon bind out of everyone's way — left at pvxs's default, the
//! client hears every PVA server on this host, and `pvxlist` reports
//! beacon-discovered servers as well as searched ones, which made
//! [`crate::ioc::verify_sole_server`] intermittently indict a foreign IOC as a
//! second server on our port. That port is allocated per invocation, never once
//! per run; [`PvaTools::run_raw`] explains why that distinction is what stops
//! whole record types from scoring ERROR.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// One batched tool's per-PV reading, or why that PV has none.
pub type Readings = Vec<Result<String, ToolError>>;

use crate::catool::{ToolError, wait_bounded};
use crate::ioc::{PvxTools, Side, alloc_free_port};

/// Did this tool die because it could not bind its beacon port?
///
/// Matched on the message rather than an exit code because pvxs reports it as a
/// plain `ERROR: Address already in use` and exits like any other failure. The
/// match is on the text pvxs prints; the `ERROR` label around it carries ANSI
/// colour codes, so only the stable part is compared.
fn is_beacon_collision(stderr: &str) -> bool {
    stderr.contains("Address already in use")
}

/// Wall-clock cap on any single tool invocation, mirroring
/// [`crate::catool`]'s. Bounds the process, not just the PVA operation.
const TOOL_TIMEOUT: Duration = Duration::from_secs(8);
/// PVA connect/operation timeout handed to the tools via `-w`.
const PVA_TIMEOUT_SECS: &str = "3";

/// How many times a tool is retried when it cannot bind its beacon port.
///
/// The port is allocated by binding, then released so the child can bind it
/// (`alloc_free_port`) — a child process cannot inherit the fd. That window is
/// small but real, and the same window `CIoc::boot`/`PvxIoc::boot` close by
/// retrying on a fresh port; this is that discipline applied to the clients.
const BEACON_ATTEMPTS: usize = 8;

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
    ///
    /// `beacon` is passed in rather than read off `self`: it belongs to one
    /// invocation, and every invocation that shares it deserves its own. See
    /// [`Self::run_raw`].
    fn env(&self, beacon: u16) -> HashMap<&'static str, String> {
        HashMap::from([
            // Explicit `addr:port` — `split_addr_into` (config.cpp:580) takes
            // the port from the entry, so no default-port var is consulted.
            ("EPICS_PVA_ADDR_LIST", format!("127.0.0.1:{}", self.port)),
            // Never let a search escape to a real subnet, or be answered by
            // the other side of the pair.
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO".to_string()),
            // Beacon RX on a port nothing beacons to, so no foreign server can
            // reach this client. Not the side's port — the client binds this,
            // and the Rust side's socket exclusively owns its own.
            ("EPICS_PVA_BROADCAST_PORT", beacon.to_string()),
        ])
    }

    /// Spawn `tool` on a beacon port of its own and wait for it, bounded.
    ///
    /// **The single owner of the beacon port**, and the only place a pvxs client
    /// is spawned. A pvxs client binds `EPICS_PVA_BROADCAST_PORT` for beacon RX
    /// (`client.cpp:641`); it must be a quiet port, or `pvxlist` hears every PVA
    /// server on this host and [`crate::ioc::verify_sole_server`] reports a
    /// foreign one as a second server on our port. It also cannot be 0 —
    /// `config.cpp:561` rejects that and silently substitutes pvxs's real
    /// default, 5076, which is the very port being avoided.
    ///
    /// # Why it is allocated here and not once per run
    ///
    /// Because the client *binds* it, the number must be one **no live server
    /// holds** — and a number allocated up front is owned by nobody.
    /// `alloc_free_port` binds a port, reads its number, and releases it, so a
    /// beacon port chosen at start-up is merely a number that *was* free once.
    /// Every later `alloc_free_port` may hand the same number to a `softIocPVX`
    /// or `oracle-ioc` search port, entirely legitimately — it is free. From
    /// that moment every client aimed at that beacon port dies on `Address
    /// already in use` (measured directly: a plain UDP socket squatting the port
    /// reproduces exactly that error), and it stays dead for the whole life of
    /// that pair — one record type's worth of channels, all scored ERROR. That
    /// is the harness manufacturing unmeasured cases out of its own instrument.
    /// Measured: 45 channels of `subArray`, and 0 in the previous run of the
    /// same binaries.
    ///
    /// Allocating immediately before the spawn is what makes the number exclude
    /// the live pair's ports: the servers are booted and holding their sockets
    /// by then, so a port that binds now is not one of theirs. The retry then
    /// covers the residual window between the release and the child's bind —
    /// the same window, closed the same way, as `CIoc::boot`/`PvxIoc::boot`.
    ///
    /// Concurrency is *not* the issue: pvxs clients share a beacon port happily
    /// (measured — eight concurrent `pvxlist` on one port all succeed), which is
    /// why the fault looked random rather than tracking the batch width.
    fn run_raw(&self, tool: &str, args: &[String]) -> Result<std::process::Output, ToolError> {
        let mut last = String::new();
        for _ in 0..BEACON_ATTEMPTS {
            let beacon = alloc_free_port().map_err(|e| self.err(tool, e.to_string()))?;
            let mut cmd = Command::new(self.bin.join(tool));
            cmd.args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (k, v) in self.env(beacon) {
                cmd.env(k, v);
            }
            let child = cmd
                .spawn()
                .map_err(|e| self.err(tool, format!("spawn: {e}")))?;
            let out = wait_bounded(child, TOOL_TIMEOUT).map_err(|e| self.err(tool, e))?;

            // The client builds its context — and binds the beacon port —
            // before it reads anything, so a bind failure means nothing was
            // measured and the whole invocation is safe to retry on a fresh
            // port. The only other socket a client binds is its search TX, and
            // that one is kernel-assigned (`client.cpp:580`), so EADDRINUSE
            // here is always the beacon port.
            if is_beacon_collision(&String::from_utf8_lossy(&out.stderr)) {
                last = format!("beacon port {beacon}: address already in use");
                continue;
            }
            return Ok(out);
        }
        Err(self.err(
            tool,
            format!("could not get a free beacon port in {BEACON_ATTEMPTS} attempts ({last})"),
        ))
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

    /// A beacon-port collision, exactly as pvxs reports it.
    ///
    /// Captured by holding a plain UDP socket on a port and pointing a real
    /// `pvxlist` at it via `EPICS_PVA_BROADCAST_PORT` — the same shape the
    /// harness produced when a booted IOC had taken the client's beacon port.
    /// Note the ANSI colour codes wrapping `ERROR`: they are why the match is on
    /// the message text and not on the label.
    const BIND_FAILURE: &str = "\u{1b}[31;1mERROR\u{1b}[0m: Address already in use";

    /// The retry in `run_raw` is only as good as its trigger. If this stops
    /// matching, a collision silently becomes an ERROR verdict again.
    #[test]
    fn a_beacon_collision_is_recognized_from_what_pvxs_actually_prints() {
        assert!(is_beacon_collision(BIND_FAILURE));
    }

    /// ...and it must not fire on the ordinary failures, or a real ERROR would
    /// be retried eight times and then reported as a port problem.
    #[test]
    fn an_ordinary_tool_failure_is_not_read_as_a_beacon_collision() {
        assert!(!is_beacon_collision("Timeout with 1 outstanding"));
        assert!(!is_beacon_collision(""));
        assert!(!is_beacon_collision(
            "\u{1b}[31;1mERROR\u{1b}[0m: Channel not found"
        ));
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
