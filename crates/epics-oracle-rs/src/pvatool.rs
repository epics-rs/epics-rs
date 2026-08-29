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
//! `verify_sole_server` intermittently indict a foreign IOC as a
//! second server on our port. That port is allocated per invocation, never once
//! per run; `run_raw` explains why that distinction is what stops
//! whole record types from scoring ERROR.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// One batched tool's per-PV reading, or why that PV has none.
pub type Readings = Vec<Result<String, ToolError>>;

use crate::catool::{ToolError, wait_bounded};
use crate::ioc::{PvxTools, Side, alloc_free_port};

/// One monitor event: the PV it names, and the body `pvxmonitor` printed under
/// it.
///
/// The body is kept as text for the same reason [`PvaTools::pvxget`]'s is: in
/// pvxs's default **Delta** format it is exactly the set of leaves the update
/// *marked*, so parsing it into a model first would discard the very thing the
/// monitor phase measures.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PvaEvent {
    pub pv: String,
    pub body: String,
}

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

/// How long to wait for a killed tool's stderr reader to reach EOF.
///
/// Only ever waited on *after* the child is killed, so it bounds a drain that is
/// already finishing rather than the tool itself.
const STDERR_DRAIN: Duration = Duration::from_secs(2);

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
            // Explicit `addr:port` — `split_addr_into` (config.cpp:581) takes
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
    /// server on this host and `verify_sole_server` reports a
    /// foreign one as a second server on our port. It also cannot be 0 —
    /// `config.cpp:563-566` rejects that and silently substitutes pvxs's real
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
    /// One tool process, wired to a beacon port of its own.
    ///
    /// The single place a pvxs client's command line and environment are built,
    /// so [`Self::run_raw`] and [`Self::subscribe`] cannot drift into aiming at
    /// different servers or binding beacons by different rules.
    fn command(&self, tool: &str, args: &[String], beacon: u16) -> Command {
        let mut cmd = Command::new(self.bin.join(tool));
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in self.env(beacon) {
            cmd.env(k, v);
        }
        cmd
    }

    fn run_raw(&self, tool: &str, args: &[String]) -> Result<std::process::Output, ToolError> {
        let mut last = String::new();
        for _ in 0..BEACON_ATTEMPTS {
            let beacon = alloc_free_port().map_err(|e| self.err(tool, e.to_string()))?;
            let child = self
                .command(tool, args, beacon)
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

    /// `pvxput <pv> <assign>` — drive one value, and **prove it landed**.
    ///
    /// `assign` is the tool's own put syntax, not a bare value, because the
    /// right spelling depends on the channel's NT shape: an `NTScalar` takes
    /// `1`, an `NTEnum`'s `value` is a *struct* and takes `value.index=1`. The
    /// caller picks it from the shape ground truth declared; see
    /// [`crate::pvamonitor`].
    ///
    /// # Why the exit code is not the answer
    ///
    /// **`pvxput` exits 0 when the put was refused.** Measured, verbatim:
    ///
    /// ```text
    /// $ pvxput M:bo 1 ; echo $?
    /// ERROR St13runtime_error : Unable to assign value from "1" : Unable to assign struct with String
    /// 0
    /// $ pvxput M:sel 1 ; echo $?
    /// ERROR N4pvxs6client11RemoteErrorE : Attempt to modify noMod field
    /// 0
    /// ```
    ///
    /// A drive believed on its exit code would therefore report "no monitor
    /// event" for a put that never happened — and since a port that posts
    /// nothing would then *agree* with a ground truth that also posted nothing,
    /// the case would score AGREED on an experiment that was never run. That is
    /// precisely the false-clean this harness exists to eliminate, so the
    /// success condition is what pvxs actually distinguishes: **a silent
    /// stderr**. A successful put prints nothing at all on either stream.
    pub fn pvxput(&self, pv: &str, assign: &str) -> Result<(), ToolError> {
        let out = self.run_raw(
            "pvxput",
            &[
                "-w".into(),
                PVA_TIMEOUT_SECS.into(),
                pv.to_string(),
                assign.to_string(),
            ],
        )?;
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !stderr.is_empty() {
            return Err(self.err("pvxput", format!("{pv} <- {assign}: {stderr}")));
        }
        if !out.status.success() {
            return Err(self.err(
                "pvxput",
                format!("{pv} <- {assign}: exit {:?}", out.status.code()),
            ));
        }
        Ok(())
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
    /// # Why this is not `probe_bisect`
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

    /// Subscribe with `pvxmonitor`, run `drive`, and return every event posted.
    ///
    /// The ordering is the whole experiment, so it is made deterministic rather
    /// than left to luck — the same five steps, for the same reasons, as
    /// [`crate::catool::CaTools::monitor`]:
    ///
    /// 1. spawn `pvxmonitor` on `pvs`;
    /// 2. **block until every PV has printed its seed event**. The seed is the
    ///    server's reply to the subscription, so its arrival is proof the
    ///    subscription is live. Driving before that would race it and lose
    ///    events, and the harness would report an event-count difference that is
    ///    an artifact of its own timing rather than a difference in the IOC;
    /// 3. run `drive` (the puts);
    /// 4. hold the window open for `settle`, so late or *extra* events are
    ///    counted — a port that posts events C suppresses must be caught, so we
    ///    cannot stop listening the instant the puts return;
    /// 5. kill the subscriber and parse.
    ///
    /// A PV that never seeds inside `connect_timeout` is a [`ToolError`], and so
    /// is a subscription that drops mid-experiment: the case then scores ERROR,
    /// never agreement.
    pub fn pvxmonitor<F>(
        &self,
        pvs: &[String],
        settle: Duration,
        connect_timeout: Duration,
        drive: F,
    ) -> Result<Vec<PvaEvent>, ToolError>
    where
        F: FnOnce(&PvaTools),
    {
        // Every retry lives inside `subscribe`, which returns only once the
        // subscription is proven live — so `drive` runs exactly once, on an
        // experiment that is already known to have started.
        let sub = self.subscribe(pvs, connect_timeout)?;
        drive(self);
        std::thread::sleep(settle);
        sub.collect(self, pvs)
    }

    /// Spawn `pvxmonitor` and return only once every PV has seeded.
    ///
    /// Retries on a beacon collision exactly as [`Self::run_raw`] does, and for
    /// the same reason: the client *binds* `EPICS_PVA_BROADCAST_PORT`, and a
    /// number that was free when it was allocated can be taken by an IOC before
    /// the child binds it. A collision here kills the tool before it ever
    /// subscribes, so nothing was measured and the whole invocation is safe to
    /// retry on a fresh port.
    fn subscribe(
        &self,
        pvs: &[String],
        connect_timeout: Duration,
    ) -> Result<Subscription, ToolError> {
        use std::io::{BufRead, BufReader, Read};

        let mut last = String::new();
        for _ in 0..BEACON_ATTEMPTS {
            let beacon = alloc_free_port().map_err(|e| self.err("pvxmonitor", e.to_string()))?;
            let mut child = self
                .command("pvxmonitor", pvs, beacon)
                .spawn()
                .map_err(|e| self.err("pvxmonitor", format!("spawn: {e}")))?;

            // Both pipes are drained on threads: `pvxmonitor` never exits on its
            // own, so it cannot go through `wait_bounded`, and an undrained pipe
            // would wedge it mid-experiment.
            let stdout = child.stdout.take().expect("piped");
            let (tx, rx) = mpsc::channel::<String>();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        return;
                    }
                }
            });
            let stderr = child.stderr.take().expect("piped");
            let (etx, erx) = mpsc::channel::<String>();
            std::thread::spawn(move || {
                let mut buf = String::new();
                let _ = BufReader::new(stderr).read_to_string(&mut buf);
                let _ = etx.send(buf);
            });

            let mut lines = Vec::new();
            let mut seeded = vec![false; pvs.len()];
            let deadline = std::time::Instant::now() + connect_timeout;
            let mut why = String::new();
            while seeded.iter().any(|s| !s) {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    let missing: Vec<&str> = pvs
                        .iter()
                        .zip(&seeded)
                        .filter(|(_, s)| !**s)
                        .map(|(p, _)| p.as_str())
                        .collect();
                    why = format!("no seed event for {missing:?} within {connect_timeout:?}");
                    break;
                }
                match rx.recv_timeout(left) {
                    Ok(line) => {
                        if let Some(i) = header_index(pvs, &line) {
                            seeded[i] = true;
                        }
                        lines.push(line);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        why = "pvxmonitor exited before subscribing".to_string();
                        break;
                    }
                }
            }
            if why.is_empty() {
                return Ok(Subscription {
                    child,
                    rx,
                    erx,
                    lines,
                });
            }

            // Nothing was measured. Kill first, so the stderr reader reaches EOF
            // and the diagnostic can be read back rather than waited on forever.
            let _ = child.kill();
            let _ = child.wait();
            let stderr = erx.recv_timeout(STDERR_DRAIN).unwrap_or_default();
            if is_beacon_collision(&stderr) {
                last = format!("beacon port {beacon}: address already in use");
                continue;
            }
            let stderr = stderr.trim();
            return Err(self.err(
                "pvxmonitor",
                if stderr.is_empty() {
                    why
                } else {
                    format!("{why} ({stderr})")
                },
            ));
        }
        Err(self.err(
            "pvxmonitor",
            format!("could not get a free beacon port in {BEACON_ATTEMPTS} attempts ({last})"),
        ))
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
    /// by `run_raw`'s per-invocation beacon port: without that, `pvxlist` also reports
    /// beacon-discovered servers, and a foreign IOC elsewhere on the host
    /// would be miscounted as a second server here.
    /// Returns the lines themselves rather than their count, because the count
    /// alone cannot be read: "2 servers answer here" is unactionable without
    /// the two addresses that answered, and the caller that reports the
    /// failure has no other way to get them.
    pub fn servers(&self) -> Result<Vec<String>, ToolError> {
        let out = self.run("pvxlist", &["-w".into(), PVA_TIMEOUT_SECS.into()])?;
        let distinct: std::collections::BTreeSet<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        Ok(distinct.into_iter().map(str::to_string).collect())
    }
}

/// A live `pvxmonitor` whose every PV has already produced its seed event.
///
/// Exists so the beacon retry can live entirely *before* the drive: a value of
/// this type is a subscription that is proven established, which is the only
/// state in which driving the experiment is meaningful.
struct Subscription {
    child: Child,
    rx: mpsc::Receiver<String>,
    erx: mpsc::Receiver<String>,
    /// Lines already drained during the seed sync — the seed events themselves.
    lines: Vec<String>,
}

impl Subscription {
    /// Stop the subscriber and turn everything it printed into events.
    fn collect(mut self, tools: &PvaTools, pvs: &[String]) -> Result<Vec<PvaEvent>, ToolError> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        while let Ok(line) = self.rx.try_recv() {
            self.lines.push(line);
        }

        // A subscription that dropped mid-experiment did not measure the event
        // sequence it was asked to measure — the count is then a count of what
        // survived, not of what the server posted. That is an ERROR, not a
        // reading. Checked against the whole stderr because `pvxmonitor` reports
        // both a drop and a subscription error there, never on stdout
        // (`pvxs/tools/monitor.cpp:162-167`).
        let stderr = self.erx.recv_timeout(STDERR_DRAIN).unwrap_or_default();
        if let Some(bad) = stderr
            .lines()
            .find(|l| l.ends_with(" Disconnected") || l.contains(" Error "))
        {
            return Err(tools.err("pvxmonitor", format!("subscription broke: {bad}")));
        }
        Ok(parse_events(pvs, &self.lines))
    }
}

/// Which requested PV a line is the header of, if any.
///
/// `pvxmonitor` prints the subscription's name verbatim on its own line and
/// indents the body under it (`monitor.cpp:142-146`), so a header is an exact
/// match against a name we asked for. Matching exactly, rather than "an
/// unindented line", is what keeps a body line that happens to start at column 0
/// from being read as the start of a new event.
fn header_index(pvs: &[String], line: &str) -> Option<usize> {
    pvs.iter().position(|p| p == line)
}

/// Split a `pvxmonitor` stream into events, in the order they were printed.
///
/// Unlike [`split_blocks`], repeats matter: a PV posts many events and the
/// *sequence* is the measurement, so each header opens a new event rather than
/// replacing the previous one. Lines before the first header are dropped — there
/// are none in practice, since every diagnostic goes to stderr.
fn parse_events(pvs: &[String], lines: &[String]) -> Vec<PvaEvent> {
    let mut events: Vec<PvaEvent> = Vec::new();
    for line in lines {
        if let Some(i) = header_index(pvs, line) {
            events.push(PvaEvent {
                pv: pvs[i].clone(),
                body: String::new(),
            });
            continue;
        }
        if let Some(e) = events.last_mut() {
            e.body.push_str(line);
            e.body.push('\n');
        }
    }
    for e in &mut events {
        e.body = e.body.trim_end().to_string();
    }
    events
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

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    /// Verbatim `pvxmonitor` stdout for one PV: the seed, then the update a
    /// `pvxput` drove. Captured from `softIocPVX` — note that the seed frames
    /// the whole structure while the update frames only what changed, which is
    /// the signature the monitor phase exists to compare.
    const MON_OUT: &str = "M:ai\n    value double = 0\n    alarm.severity int32_t = 0\n    timeStamp.secondsPastEpoch int64_t = 631152000\nM:ai\n    value double = 1.5\n    timeStamp.secondsPastEpoch int64_t = 1784213743\n";

    /// The sequence is the measurement, so a PV's second event must not
    /// overwrite its first — the trap [`split_blocks`] would fall into here.
    #[test]
    fn every_event_is_kept_in_order_including_repeats_of_one_pv() {
        let ev = parse_events(&pvs(&["M:ai"]), &lines(MON_OUT));
        assert_eq!(ev.len(), 2, "seed and update are two events, not one");
        assert!(ev[0].body.contains("value double = 0"));
        assert!(ev[1].body.contains("value double = 1.5"));
        assert!(
            !ev[1].body.contains("alarm.severity"),
            "the update frames only what changed: {:?}",
            ev[1].body
        );
    }

    /// Events from several PVs interleave in one stream, so each must be
    /// attributed to the PV whose header opened it.
    #[test]
    fn interleaved_events_are_attributed_to_their_own_pv() {
        let out = "M:ai\n    value double = 1\nM:bo\n    value.index int32_t = 1\nM:ai\n    value double = 2\n";
        let ev = parse_events(&pvs(&["M:ai", "M:bo"]), &lines(out));
        let by_pv: Vec<&str> = ev.iter().map(|e| e.pv.as_str()).collect();
        assert_eq!(by_pv, ["M:ai", "M:bo", "M:ai"]);
        assert!(ev[1].body.contains("value.index"));
    }

    /// A header is an exact name match, so a PV name that is a prefix of another
    /// cannot open its neighbour's event.
    #[test]
    fn a_prefix_pv_name_does_not_open_a_longer_pvs_event() {
        assert_eq!(
            header_index(&pvs(&["M:ai", "M:ai.SCAN"]), "M:ai.SCAN"),
            Some(1)
        );
        assert_eq!(header_index(&pvs(&["M:ai"]), "M:ai.SCAN"), None);
        assert_eq!(header_index(&pvs(&["M:ai"]), "    value double = 0"), None);
    }
}
