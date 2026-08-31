//! Booting the two sides of the differential pair, and the port discipline
//! that keeps their answers attributable.
//!
//! # Why the port discipline is load-bearing
//!
//! A CA client finds a server by UDP search on `EPICS_CA_SERVER_PORT` and then
//! connects to whatever TCP port that server *advertises*. So if two servers
//! end up sharing a UDP port, a `caget` aimed at one can be answered by the
//! other — and the harness would score a diff (or, far worse, an agreement)
//! against the wrong IOC. This is not hypothetical: booting the C `softIoc` on
//! a hard-coded 5064-style port on this host reproduces it immediately —
//!
//! ```text
//! cas WARNING: Configured TCP port was unavailable.
//! cas WARNING: Using dynamically assigned TCP port 33309,
//! cas WARNING: but now two or more servers share the same UDP port.
//! ```
//!
//! ...and the IOC *keeps running* and *serves values*. That is precisely the
//! silent-wrong-answer this harness exists to not produce. Hence:
//!
//! - **Rust side:** true bind-`:0`-and-read-back.
//!   `CaServer::from_parts(db, 0, ..)` — the four trailing arguments are the
//!   ACF cell and the optional TLS and cap-token seams — binds the sockets and
//!   reports the port it actually got, so no number is ever guessed and
//!   nothing can take it in between. Named with its real arity because the
//!   `bin/oracle_ioc.rs` call is what a reader copies; and named in a code
//!   span rather than linked because the constructor is `tokio_backend`-only,
//!   so on the reactor-free backend this paragraph still renders while the
//!   item it names is not there to link to.
//! - **C side:** `softIoc` takes its port from the environment and cannot
//!   inherit a pre-bound fd, so bind-read-back is *not available*. The honest
//!   substitute is allocate-then-**verify**: [`alloc_free_port`] binds `:0` on
//!   UDP *and* TCP to find a number free on both, then [`CIoc::boot`] scans the
//!   IOC's own startup output and treats any bind complaint as a boot failure
//!   to be retried on a fresh port. The residual TOCTOU window is closed by
//!   verification after the fact, not by hoping.
//!
//! Neither side ever hard-codes 5064, and neither ever probe-then-rebinds a
//! port it has already decided to use.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::catool::{CaTools, ToolError};

/// How long to wait for an IOC to report itself up before calling the boot an
/// ERROR. Generous: a slow boot must surface as an error, never as a case that
/// quietly "agreed".
const BOOT_TIMEOUT: Duration = Duration::from_secs(20);

/// How many times a booted IOC is re-probed for **observed** reachability
/// before the boot is called a genuine failure.
///
/// A process reporting itself "up" is not the same as its CA layer answering a
/// search. The Rust `oracle-ioc` prints its port right after `from_parts` binds
/// the sockets — but the UDP search responder is not spawned until `run()`,
/// which happens *after* that line. Under full-run load the harness reads the
/// port and drives cases in that window, the client's search retries exhaust
/// before `run()` starts answering, and every case of the type comes back
/// "not found" — a whole record type turned `errored` by a boot race rather
/// than by any real defect. So readiness is **observed**, not assumed: poll a
/// known channel through the real client until it connects.
///
/// The budget is an ATTEMPT COUNT, not a wall clock. A wall-clock gate spends
/// itself on the probes rather than on the wait, so it buys *fewer* attempts
/// exactly when the box is loaded and the extra attempt is what was needed: one
/// killed `cainfo` costs `catool::TOOL_TIMEOUT` (8 s), so the 15 s gate this
/// replaces gave ~20 probes idle and as few as two under load. An attempt count
/// is load-invariant.
const REACHABLE_ATTEMPTS: usize = 12;
/// First inter-probe pause; doubles up to [`REACHABLE_BACKOFF_MAX`]. Small so a
/// server that is already up is measured as up almost immediately.
const REACHABLE_BACKOFF_START: Duration = Duration::from_millis(50);
const REACHABLE_BACKOFF_MAX: Duration = Duration::from_millis(500);

/// Boot failures are retried on a *fresh* port this many times. A retry only
/// happens when the failure is a port collision, which is inherently racy; any
/// other failure is returned immediately.
const BOOT_ATTEMPTS: usize = 5;

/// Anything that went wrong booting an IOC. Never silently swallowed: a case
/// that cannot run is reported ERROR.
///
/// The attribution travels WITH the failure. Every construction site knows
/// which IOC it was talking to, and the case builders have no way to guess it
/// back afterwards, so a failure that carried only a message got recorded
/// against both sides — one ERROR became two, and one of the two named an IOC
/// that had booted fine.
#[derive(Debug)]
pub struct BootError {
    /// The side this failure belongs to, or `None` when it belongs to
    /// **neither** — a workdir write, a port the host would not hand out, a
    /// refusal to run because the two sides collided. `None` is the only
    /// attribution that is legitimately recorded against both sides.
    pub side: Option<Side>,
    pub message: String,
    /// Whether booting again **on a different port** could succeed.
    ///
    /// The one thing a retry varies is the port, so this says "the port was
    /// the problem" and nothing else. It used to be inferred by the boot
    /// loops from `message.contains("PORT_COLLISION")`, which made the
    /// message text load-bearing and covered exactly one of the ways a port
    /// can be lost: base prints that warning only when it *recovered* from
    /// the collision by moving its TCP port. The fatal case — `rsrv`'s UDP
    /// bind losing the port, which is silent (`caservertask.c:131-146`
    /// prints nothing on `EADDRINUSE`) and ends in `cantProceed` — carried no
    /// such text and was read as an ordinary boot timeout.
    pub retryable: bool,
}

impl BootError {
    pub fn new(side: Side, message: impl Into<String>) -> Self {
        Self {
            side: Some(side),
            message: message.into(),
            retryable: false,
        }
    }

    /// A failure a fresh port could fix. See [`BootError::retryable`].
    pub fn retryable(side: Side, message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            ..Self::new(side, message)
        }
    }

    /// A failure that belongs to neither IOC.
    pub fn neither(message: impl Into<String>) -> Self {
        Self {
            side: None,
            message: message.into(),
            retryable: false,
        }
    }

    /// A failure a fresh port could fix that belongs to neither IOC — the two
    /// sides drawing the same number is the only one. [`Self::retryable`] and
    /// [`Self::neither`] in one; there is no third way to set the flag.
    pub fn retryable_neither(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            ..Self::neither(message)
        }
    }

    /// The tool errors this failure is recorded as: one entry for the side that
    /// produced it, and two only for a failure that belongs to neither.
    pub fn tool_errors(&self, tool: &str) -> Vec<ToolError> {
        let one = |side| ToolError {
            side,
            tool: tool.to_string(),
            message: self.message.clone(),
        };
        match self.side {
            Some(side) => vec![one(side)],
            None => vec![one(Side::C), one(Side::Rust)],
        }
    }
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for BootError {}

fn err<T>(side: Side, msg: impl Into<String>) -> Result<T, BootError> {
    Err(BootError::new(side, msg))
}

fn err_neither<T>(msg: impl Into<String>) -> Result<T, BootError> {
    Err(BootError::neither(msg))
}

fn err_retryable<T>(side: Side, msg: impl Into<String>) -> Result<T, BootError> {
    Err(BootError::retryable(side, msg))
}

fn err_retryable_neither<T>(msg: impl Into<String>) -> Result<T, BootError> {
    Err(BootError::retryable_neither(msg))
}

/// Find a port number that is free on **both** UDP and TCP, by binding it.
///
/// A CA server needs the same number on both protocols, so checking only one
/// would let the other collide. We bind UDP `:0` to have the kernel hand us a
/// number it believes is free, then bind the same number on TCP to prove it.
/// If TCP is taken we discard the number and ask the kernel for another —
/// which is *not* a probe-then-rebind of a port we intend to use, it is
/// rejecting a candidate before anyone commits to it.
///
/// Both sockets are dropped on return, because `softIoc` cannot inherit them.
/// The window this opens is what [`CIoc::boot`]'s output verification exists
/// to close.
pub fn alloc_free_port() -> Result<u16, BootError> {
    for _ in 0..64 {
        let Ok(udp) = UdpSocket::bind(("127.0.0.1", 0)) else {
            continue;
        };
        let Ok(addr) = udp.local_addr() else {
            continue;
        };
        let port = addr.port();
        // Prove the same number is free on TCP too; if not, this candidate is
        // unusable and we never tell anyone about it.
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(tcp) => {
                drop(tcp);
                drop(udp);
                return Ok(port);
            }
            Err(_) => continue,
        }
    }
    err_neither("could not find a port free on both UDP and TCP after 64 tries")
}

/// Tell a `softIoc`-family binary to serve `db`, by whichever route that `db`
/// requires.
///
/// **The single owner of the load route**, shared by [`CIoc`] (base's fat
/// `softIoc`) and [`PvxIoc`] (the fat `softIocPVX`) because both are built from
/// base's `softMain` and both must stage an asyn reproducer *identically*. If
/// the two sides staged it differently the oracle would be diffing two
/// different configurations while reporting on one.
///
/// An asyn reproducer names a port ([`crate::ORACLE_ASYN_PORT`]) that must
/// already exist when `init_record` connects the record. `softMain` gates its
/// auto-`iocInit` on a `-d` load and runs a positional st.cmd *before* it, so
/// `-d` can only create the port too late. Measured on the fat `softIocPVX`,
/// which is exactly what makes this route mandatory rather than stylistic:
///
/// ```text
/// -S -d asyn.db  -> T:ASYN: Connect error, status=3,
///                   asynManager:connectDevice port ORACLEASYN not found
/// -S asyn.st.cmd -> T:ASYN: queueRequest failed
/// ```
///
/// Both then print `iocRun: All initialization complete`, so `-d` *boots* — it
/// just boots a record that never found its port. That is the shape of failure
/// this harness exists to not accept. The st.cmd route reaches the intended
/// state: the port exists, the record is attached, and only the device is
/// unreachable — `queueRequest failed` is the disconnected `localhost:1`
/// talking, which is the point (it mirrors the Rust `NullOctetPort`:
/// noAutoConnect → CNCT/AUCT=0, {octet,option} → the `*IV` set).
///
/// Every other record type keeps the plain `-d` path.
fn load_db_into(cmd: &mut Command, db: &Path) -> Result<(), BootError> {
    let db_text = std::fs::read_to_string(db)
        .map_err(|e| BootError::neither(format!("read db {}: {e}", db.display())))?;
    if !db_text.contains(crate::ORACLE_ASYN_PORT) {
        cmd.arg("-d").arg(db);
        return Ok(());
    }

    let db_abs = db.canonicalize().unwrap_or_else(|_| db.to_path_buf());
    let st_cmd = db.with_extension("st.cmd");
    let script = format!(
        "drvAsynIPPortConfigure(\"{port_name}\",\"localhost:1\",0,1,0)\n\
         dbLoadRecords(\"{db}\")\n\
         iocInit\n",
        port_name = crate::ORACLE_ASYN_PORT,
        db = db_abs.display(),
    );
    std::fs::write(&st_cmd, script)
        .map_err(|e| BootError::neither(format!("write st.cmd {}: {e}", st_cmd.display())))?;
    cmd.arg(&st_cmd);
    Ok(())
}

/// The last few lines an IOC printed, kept so a failure can quote the process
/// it is about.
///
/// A reachability timeout on its own says only that a client gave up, and that
/// is the shape of every unexplained boot failure this harness has produced: a
/// budget expiring, with the one witness that could name the cause — the IOC's
/// own output — read and discarded by the watcher thread a second earlier. A
/// `cas WARNING`, a panic, a db that loaded no record, or nothing whatsoever
/// are four different faults behind one message, and keeping twelve lines
/// tells them apart for free.
#[derive(Clone, Default)]
pub struct OutputTail(Arc<Mutex<VecDeque<String>>>);

impl OutputTail {
    /// Enough to hold an IOC's startup banner and the line that went wrong,
    /// bounded so a chatty IOC cannot grow it without limit.
    const KEEP: usize = 12;

    fn new() -> Self {
        Self::default()
    }

    fn push(&self, line: &str) {
        let mut q = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() == Self::KEEP {
            q.pop_front();
        }
        q.push_back(line.trim_end().to_string());
    }

    /// The kept lines, oldest first. Silence is a finding in its own right —
    /// an IOC that printed nothing at all did not merely boot slowly — so it
    /// is reported as such rather than as an empty string.
    pub fn text(&self) -> String {
        let q = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            "(the IOC printed nothing)".to_string()
        } else {
            q.iter().cloned().collect::<Vec<_>>().join(" | ")
        }
    }
}

/// A booted IOC serving a `.db` on a known-exclusive CA port.
pub trait Ioc {
    /// The CA server port: both the UDP search port and (for a healthy boot)
    /// the advertised TCP port.
    fn port(&self) -> u16;
    /// Which side of the differential pair this is — used to label diffs.
    fn side(&self) -> Side;
    /// What this IOC last said about itself. Quoted by whoever declares it
    /// unreachable, so the failure names a cause instead of a budget.
    fn recent_output(&self) -> String;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// The C `softIoc` — ground truth.
    C,
    /// The Rust IOC — the thing under test.
    Rust,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Side::C => "C",
            Side::Rust => "rust",
        })
    }
}

/// Paths to the built C EPICS tree. The harness refuses to invent these: if
/// the tree is absent it is an ERROR, not a skipped-and-passed run.
#[derive(Debug, Clone)]
pub struct CTools {
    /// Base's CA client tools (caget/caput/cainfo/camonitor) — the drivers.
    pub bin: PathBuf,
    /// The C ground-truth IOC binary. This is the *fat* softIoc built under
    /// `oracle-ioc/`, which links busy/calc/asyn record+device support on top
    /// of base, so the sweep covers those record types with the same
    /// `softIoc -S -d db` interface base's stock softIoc provides.
    pub ioc_bin: PathBuf,
}

impl CTools {
    pub const DEFAULT_BIN: &'static str = "/home/stevek/work/epics-base/bin/linux-x86_64";
    /// The fat dbd — base's 34 record types plus busy/transform/sseq/acalcout/
    /// scalcout/asyn. A strict superset of base's `softIoc.dbd`.
    ///
    /// Private on purpose: [`Self::dbd_path`] is the only way to ask where the
    /// dbd is, so no caller can hardcode this path and silently bypass
    /// `EPICS_ORACLE_DBD` the way every consumer of the old public constant did.
    const DEFAULT_DBD: &'static str = "/home/stevek/work/oracle-ioc/dbd/softIoc.dbd";
    /// The fat softIoc binary that serves the [`Self::dbd_path`] record types.
    pub const DEFAULT_IOC_BIN: &'static str =
        "/home/stevek/work/oracle-ioc/bin/linux-x86_64/softIoc";

    /// The expanded dbd that supplies the denominator, honoring
    /// `EPICS_ORACLE_DBD` for hosts where the C tree lives elsewhere.
    ///
    /// The same override discipline as `EPICS_BASE_BIN`/`EPICS_ORACLE_IOC_BIN`
    /// in [`Self::discover`]: the harness never invents the path, but it must be
    /// nameable, or the oracle is runnable on exactly one machine.
    pub fn dbd_path() -> PathBuf {
        PathBuf::from(
            std::env::var("EPICS_ORACLE_DBD").unwrap_or_else(|_| Self::DEFAULT_DBD.to_string()),
        )
    }

    /// Locate the C tree, honoring `EPICS_BASE_BIN` for hosts where it lives
    /// elsewhere. Verifies every binary the harness actually invokes, so a
    /// missing tool is a loud error at startup rather than a mysterious
    /// "errored" on every case.
    pub fn discover() -> Result<Self, BootError> {
        let bin = std::env::var("EPICS_BASE_BIN").unwrap_or_else(|_| Self::DEFAULT_BIN.to_string());
        let bin = PathBuf::from(bin);
        if !bin.is_dir() {
            return err(
                Side::C,
                format!(
                    "C EPICS bin dir not found at {}. Set EPICS_BASE_BIN to the built \
                     linux-x86_64 bin dir. The oracle cannot run without ground truth.",
                    bin.display()
                ),
            );
        }
        for tool in ["caget", "caput", "cainfo", "camonitor"] {
            if !bin.join(tool).is_file() {
                return err(
                    Side::C,
                    format!("missing C tool `{tool}` in {}", bin.display()),
                );
            }
        }
        let ioc_bin = std::env::var("EPICS_ORACLE_IOC_BIN")
            .unwrap_or_else(|_| Self::DEFAULT_IOC_BIN.to_string());
        let ioc_bin = PathBuf::from(ioc_bin);
        if !ioc_bin.is_file() {
            return err(
                Side::C,
                format!(
                    "fat C softIoc not found at {}. Build it under oracle-ioc/ (or set \
                     EPICS_ORACLE_IOC_BIN). The oracle cannot run without ground truth.",
                    ioc_bin.display()
                ),
            );
        }
        Ok(Self { bin, ioc_bin })
    }

    pub fn tool(&self, name: &str) -> PathBuf {
        self.bin.join(name)
    }
}

/// The C `softIoc`, booted on the given `.db`. Ground truth.
pub struct CIoc {
    port: u16,
    child: Child,
    tail: OutputTail,
}

impl CIoc {
    pub fn boot(tools: &CTools, db: &Path) -> Result<Self, BootError> {
        let mut why = Vec::new();
        for n in 1..=BOOT_ATTEMPTS {
            let port = alloc_free_port()?;
            match Self::try_boot(tools, db, port) {
                Ok(ioc) => return Ok(ioc),
                Err(e) if e.retryable => {
                    // Racy by nature: someone took the port between our probe
                    // and softIoc's bind. Discard this port entirely and get a
                    // new one — never re-use or re-probe the losing number.
                    why.push(format!("attempt {n} on port {port}: {}", e.message));
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        err(
            Side::C,
            format!(
                "C softIoc could not get an exclusive port in {BOOT_ATTEMPTS} attempts: {}",
                why.join("; ")
            ),
        )
    }

    fn try_boot(tools: &CTools, db: &Path, port: u16) -> Result<Self, BootError> {
        let mut cmd = Command::new(&tools.ioc_bin);
        cmd.arg("-S"); // no interactive shell
        load_db_into(&mut cmd, db)?;
        let mut child = cmd
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .env("EPICS_CAS_SERVER_PORT", port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BootError::new(Side::C, format!("spawn softIoc: {e}")))?;

        // softIoc announces readiness with "iocRun: All initialization
        // complete". It announces a *port collision* with a `cas WARNING`
        // about the TCP port being unavailable -- and then keeps running and
        // serving, which is the silent-wrong-answer we must never accept. So
        // we watch both streams and let the collision win over the ready line.
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let (tx, rx) = mpsc::channel::<BootSignal>();
        let tail = OutputTail::new();
        spawn_watcher(stdout, tx.clone(), tail.clone());
        spawn_watcher(stderr, tx, tail.clone());

        let deadline = Instant::now() + BOOT_TIMEOUT;
        let mut ready = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let _ = child.kill();
                return err(
                    Side::C,
                    format!(
                        "C softIoc did not report ready within {BOOT_TIMEOUT:?} on port {port}; \
                         it said: {}",
                        tail.text()
                    ),
                );
            }
            match rx.recv_timeout(left) {
                Ok(BootSignal::PortCollision(line)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return err_retryable(Side::C, format!("PORT_COLLISION on {port}: {line}"));
                }
                // `rsrv` lost the CA port on the UDP bind, which base does not
                // report (`caservertask.c:131-146` returns silently on
                // EADDRINUSE) and then turns fatal at `cantProceed("CAS: No
                // TCP server started")`. A different port is exactly the
                // remedy, so it retries rather than waiting out the timeout.
                Ok(BootSignal::Died(said)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return err_retryable(
                        Side::C,
                        format!(
                            "C softIoc declared itself unable to proceed on port {port}; \
                                 it said: {said}"
                        ),
                    );
                }
                Ok(BootSignal::Ready) => {
                    ready = true;
                    break;
                }
                Ok(BootSignal::Other) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                // Both streams closed without a ready line: the IOC died.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if !ready {
            let status = child.try_wait().ok().flatten();
            let _ = child.kill();
            return err(
                Side::C,
                format!(
                    "C softIoc exited during boot (status {status:?}) — db rejected? \
                     it said: {}",
                    tail.text()
                ),
            );
        }
        Ok(Self { port, child, tail })
    }
}

enum BootSignal {
    Ready,
    PortCollision(String),
    /// The IOC has declared itself unable to continue. `cantProceed`
    /// (`libcom/src/misc/cantProceed.c:54-72`) prints this banner, dumps a
    /// stack, then loops on `epicsThreadSuspendSelf()` — so the process never
    /// exits, the pipes never close, and without this signal the boot can
    /// only be discovered by waiting out the full timeout.
    Died(String),
    /// Any other output line. Carries no payload we act on, but it must still
    /// be a distinct signal so the boot loop keeps waiting rather than treating
    /// a chatty startup as silence.
    Other,
}

fn spawn_watcher(
    stream: impl std::io::Read + Send + 'static,
    tx: mpsc::Sender<BootSignal>,
    tail: OutputTail,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            tail.push(&line);
            // Order matters: a collision line must not be mistaken for noise,
            // and it can appear *before* the ready line.
            let sig = if line.contains("can't proceed, suspending") {
                BootSignal::Died(tail.text())
            } else if line.contains("Configured TCP port was unavailable")
                || line.contains("two or more servers share the same UDP port")
                || line.contains("Unable to bind")
            {
                BootSignal::PortCollision(line.trim().to_string())
            } else if line.contains("iocRun: All initialization complete")
                || line.contains("ORACLE_IOC_READY")
            {
                BootSignal::Ready
            } else {
                BootSignal::Other
            };
            // A closed channel ends the SIGNALLING, not the watching. The boot
            // loop drops its receiver the moment the IOC reports ready, but an
            // IOC that dies later says why on the way out -- glibc's
            // `corrupted size vs. prev_size` when a client read overruns a
            // buffer, say -- and that line is the only evidence naming what
            // killed it. Returning here discarded exactly the words
            // `OutputTail` exists to keep, leaving every post-boot death as a
            // budget expiring with no cause.
            let _ = tx.send(sig);
        }
    });
}

impl Ioc for CIoc {
    fn port(&self) -> u16 {
        self.port
    }
    fn side(&self) -> Side {
        Side::C
    }
    fn recent_output(&self) -> String {
        self.tail.text()
    }
}

impl Drop for CIoc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Which protocol a [`RustIoc`] serves.
///
/// The port line is per-protocol on purpose. A CA port and a PVA UDP search
/// port are not interchangeable, and a harness that read one and aimed the
/// other's client at it would score every case ERROR for a reason that looks
/// like a port bug rather than a wiring mistake. Distinct names make that
/// mistake a parse failure at boot instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RustMode {
    Ca,
    Pva,
}

impl RustMode {
    fn flag(self) -> Option<&'static str> {
        match self {
            RustMode::Ca => None,
            RustMode::Pva => Some("--pva"),
        }
    }
    fn port_line(self) -> &'static str {
        match self {
            RustMode::Ca => "ORACLE_IOC_PORT",
            RustMode::Pva => "ORACLE_IOC_PVA_PORT",
        }
    }
}

/// The Rust IOC (`oracle-ioc` binary), booted on the same `.db`.
///
/// Runs as a subprocess rather than in-process so that both sides of the pair
/// are driven through the *identical* external instrument (the C CA tools) and
/// neither gets a privileged in-process path the other lacks. It binds CA on
/// `:0` and prints the port it was actually given, so no number is guessed.
pub struct RustIoc {
    port: u16,
    child: Child,
    tail: OutputTail,
}

impl RustIoc {
    /// Locate the built `oracle-ioc` binary. Cargo hands tests the path to the
    /// binaries of their own package via `CARGO_BIN_EXE_*`; the standalone
    /// binary falls back to looking next to itself.
    pub fn binary() -> Result<PathBuf, BootError> {
        if let Ok(p) = std::env::var("ORACLE_IOC_BIN") {
            return Ok(PathBuf::from(p));
        }
        let exe = std::env::current_exe()
            .map_err(|e| BootError::new(Side::Rust, format!("current_exe: {e}")))?
            .parent()
            .and_then(|d| {
                // tests live in target/<profile>/deps/, binaries one level up
                let here = d.join("oracle-ioc");
                if here.is_file() {
                    return Some(here);
                }
                let up = d.parent()?.join("oracle-ioc");
                up.is_file().then_some(up)
            });
        exe.ok_or_else(|| {
            BootError::new(
                Side::Rust,
                "cannot find the `oracle-ioc` binary. Build it \
                 (`cargo build -p epics-oracle-rs`) or set ORACLE_IOC_BIN.",
            )
        })
    }

    /// Boot the CA side, reporting the bound CA port.
    pub fn boot(db: &Path) -> Result<Self, BootError> {
        Self::boot_mode(db, RustMode::Ca)
    }

    /// Boot the **PVA** side, reporting the bound UDP search port.
    ///
    /// Same binary, same `.db`, same bind-and-read-back discipline — only the
    /// protocol and the port line differ (see `RustMode`).
    pub fn boot_pva(db: &Path) -> Result<Self, BootError> {
        Self::boot_mode(db, RustMode::Pva)
    }

    fn boot_mode(db: &Path, mode: RustMode) -> Result<Self, BootError> {
        let bin = Self::binary()?;
        let mut cmd = Command::new(&bin);
        cmd.arg("--db").arg(db);
        if let Some(flag) = mode.flag() {
            cmd.arg(flag);
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BootError::new(Side::Rust, format!("spawn {}: {e}", bin.display())))?;
        let port_line = mode.port_line();

        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        // The Rust IOC prints `<port_line> <p>` after the socket is bound, so
        // the number is read back from the bind, never predicted.
        let (tx, rx) = mpsc::channel::<Result<u16, String>>();
        let ttx = tx.clone();
        let prefix = format!("{port_line} ");
        let tail = OutputTail::new();
        let otail = tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                otail.push(&line);
                if let Some(p) = line.strip_prefix(&prefix) {
                    let parsed = p
                        .trim()
                        .parse::<u16>()
                        .map_err(|e| format!("bad port line `{line}`: {e}"));
                    let _ = ttx.send(parsed);
                    // Keep reading. The port line is the *start* of this IOC's
                    // life, and everything worth quoting when it later fails to
                    // answer is printed after it.
                    continue;
                }
            }
            let _ = ttx.send(Err(format!(
                "oracle-ioc closed stdout without reporting a `{port_line}` port"
            )));
        });
        // Capture stderr so a panic/failure has a diagnosable message rather
        // than a bare timeout.
        let (etx, erx) = mpsc::channel::<String>();
        let etail = tail.clone();
        std::thread::spawn(move || {
            let mut buf = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                etail.push(&line);
                buf.push_str(&line);
                buf.push('\n');
            }
            let _ = etx.send(buf);
        });

        // The reader thread reports the bound port, or explains why there is
        // none. Either way the IOC's own stderr is the useful diagnostic, so a
        // failure carries it rather than a bare "boot failed".
        let outcome = rx.recv_timeout(BOOT_TIMEOUT);
        let reason = match outcome {
            Ok(Ok(port)) => return Ok(Self { port, child, tail }),
            Ok(Err(e)) => e,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                "oracle-ioc exited without reporting a port".to_string()
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                format!("no port reported within {BOOT_TIMEOUT:?}")
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        let detail = erx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
        // `detail` needs the child's stderr to reach EOF; the tail does not, so
        // a boot that fails while the process is still alive still says why.
        err(
            Side::Rust,
            format!(
                "Rust IOC failed to boot: {reason}: {} (it said: {})",
                first_useful_line(&detail),
                tail.text()
            ),
        )
    }
}

/// The most informative line of a failed IOC's stderr — the panic message if
/// there is one, else the first non-empty line.
fn first_useful_line(s: &str) -> String {
    if s.trim().is_empty() {
        return "(no stderr)".into();
    }
    s.lines()
        .find(|l| l.contains("panicked") || l.contains("Error") || l.contains("error"))
        .or_else(|| s.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("(no stderr)")
        .trim()
        .to_string()
}

impl Ioc for RustIoc {
    fn port(&self) -> u16 {
        self.port
    }
    fn side(&self) -> Side {
        Side::Rust
    }
    fn recent_output(&self) -> String {
        self.tail.text()
    }
}

impl Drop for RustIoc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The booted differential pair: the same `.db` on both sides, each on its own
/// exclusive CA port.
pub struct Pair {
    pub c: CIoc,
    pub rust: RustIoc,
}

impl Pair {
    /// Boot both sides and return only once **each** is observed reachable.
    ///
    /// `probe_pv` must name a channel present in `db` (every caller's `.db` has
    /// at least one record, so its name is the natural probe). The returned
    /// `Pair` is reachable by construction: a caller cannot obtain one whose CA
    /// layer is not yet answering searches, which is the race that used to turn
    /// a whole record type `errored`.
    pub fn boot(tools: &CTools, db: &Path, probe_pv: &str) -> Result<Self, BootError> {
        retry_pair("CA", || Self::try_boot(tools, db, probe_pv))
    }

    fn try_boot(tools: &CTools, db: &Path, probe_pv: &str) -> Result<Self, BootError> {
        let c = CIoc::boot(tools, db)?;
        let rust = RustIoc::boot(db)?;
        if c.port() == rust.port() {
            return err_retryable_neither(
                "C and Rust IOC landed on the same CA port — refusing to run, \
                        answers would not be attributable",
            );
        }
        // Gate on OBSERVED reachability, both sides. The C softIoc's "All
        // initialization complete" is a genuine ready signal, but the Rust
        // side prints its port before `run()` spawns the search responder;
        // probing both is symmetric and future-proofs the C side too. A side
        // that never answers within the budget is a real boot failure, named
        // as such — not a silent per-case `errored`.
        wait_reachable(tools, &c, probe_pv)?;
        wait_reachable(tools, &rust, probe_pv)?;
        Ok(Self { c, rust })
    }
}

/// Poll a known channel through the real C client until it connects.
///
/// Uses `cainfo`, the same instrument the harness measures with, so "reachable"
/// means exactly "a C client can create this channel" — the property the cases
/// depend on. The probe carries C's own `-w` default rather than the harness's
/// measurement budget: a probe is retried, so it wants C's short wait and many
/// attempts (`tool_lib.h:51` `#define DEFAULT_TIMEOUT 1.0`).
fn wait_reachable(tools: &CTools, ioc: &dyn Ioc, probe_pv: &str) -> Result<(), BootError> {
    let t = CaTools::new(tools, ioc.port(), ioc.side());
    wait_probe("IOC", ioc, probe_pv, || {
        t.cainfo_probe(probe_pv).map(|_| ())
    })
}

/// The single owner of the reachability retry policy, shared by the CA and PVA
/// probes so one cannot drift off the other.
///
/// Returns `Ok` on the first successful probe; exhausting
/// [`REACHABLE_ATTEMPTS`] is a genuine boot failure carrying the attempt count,
/// the last probe error and what the IOC itself printed, never masked as a
/// per-case timeout. It takes the [`Ioc`] rather than a port and a [`Side`]
/// because those three facts have one owner — the process — and a probe that
/// reports the attempt count without the child's own words leaves the reader
/// with a budget and no cause.
fn wait_probe<E: std::fmt::Display>(
    what: &str,
    ioc: &dyn Ioc,
    probe_pv: &str,
    mut probe: impl FnMut() -> Result<(), E>,
) -> Result<(), BootError> {
    let side = ioc.side();
    let mut backoff = REACHABLE_BACKOFF_START;
    let mut last = String::new();
    for attempt in 1..=REACHABLE_ATTEMPTS {
        match probe() {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        if attempt == REACHABLE_ATTEMPTS {
            break;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(REACHABLE_BACKOFF_MAX);
    }
    // Retryable for the same reason a lost port is: the one thing a fresh
    // attempt varies is the port, and a server that never answered a SEARCH on
    // the number it reported is a port that did not carry traffic. Charging it
    // to the cases instead cost 1004 `acalcout` ERRORs, 43 % of a four-type
    // run, from one boot a second draw survived.
    err_retryable(
        side,
        format!(
            "{side} {what} did not become reachable after {REACHABLE_ATTEMPTS} attempts \
             on port {} (probe {probe_pv}: {last}); it said: {}",
            ioc.port(),
            ioc.recent_output()
        ),
    )
}

/// Boot a differential pair, retrying the WHOLE pair on a port-scoped failure.
///
/// One uniform rule for every way a pair can fail to come up on the numbers it
/// drew: a port taken between the probe and the bind, `rsrv` losing its UDP
/// bind and ending in `cantProceed`, the two sides landing on one number, a
/// side that never answered a SEARCH on the port it reported, a second server
/// silently sharing a PVA port. Each is a property of *these* numbers, so the
/// answer to all five is the same — throw the pair away and draw fresh ones.
///
/// [`BootError::retryable`] already decided which failures qualify; before
/// this, only the two single-IOC loops read it, so a pair that booted but
/// never answered was terminal and the harness charged it to the *cases*: one
/// unreachable Rust IOC turned an entire record type into ERRORs — measured at
/// 1004 cases for `acalcout`, 43 % of a four-type run, from a single boot that
/// a second attempt survived. An infrastructure flake published as 1004
/// unmeasured cases is the reporting hole this closes.
///
/// Exhaustion still fails, and it names the attempt count and the last reason
/// so the failure cannot be read as a case-level timeout.
fn retry_pair<T>(
    protocol: &str,
    mut attempt: impl FnMut() -> Result<T, BootError>,
) -> Result<T, BootError> {
    let mut last = None;
    let mut why = Vec::new();
    for n in 1..=BOOT_ATTEMPTS {
        match attempt() {
            Ok(pair) => return Ok(pair),
            Err(e) if e.retryable => {
                // Say so on stderr: a silent retry would let a host that needs
                // three tries every time look as healthy as one that needs none.
                eprintln!(
                    "  {protocol} pair boot attempt {n}/{BOOT_ATTEMPTS} failed, \
                     retrying on fresh ports: {e}"
                );
                why.push(format!("attempt {n}: {}", e.message));
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    let last = last.expect("a retryable failure is recorded before the loop can exhaust");
    Err(BootError {
        side: last.side,
        message: format!(
            "{protocol} pair did not come up in {BOOT_ATTEMPTS} attempts, \
             each on fresh ports: {}",
            why.join("; ")
        ),
        retryable: false,
    })
}

// ---------------------------------------------------------------------------
// The PVA pair: pvxs QSRV2 (`softIocPVX`) vs `oracle-ioc --pva`.
// ---------------------------------------------------------------------------

/// Paths to the built pvxs tree — the PVA reference side and its client tools.
///
/// The same refusal-to-invent as [`CTools`]: an absent tree is an ERROR at
/// startup, not a skipped-and-passed run.
#[derive(Debug, Clone)]
pub struct PvxTools {
    /// pvxs client tools (`pvxget`/`pvxinfo`/`pvxlist`) — the instrument both
    /// sides are driven through.
    pub bin: PathBuf,
    /// pvxs QSRV2's IOC binary — the PVA ground truth.
    pub ioc_bin: PathBuf,
    // NO beacon port here, on purpose. The client tools need one, but a client
    // *binds* it, so the number must be one no live server holds — and
    // `alloc_free_port` releases the port it names, so a number allocated once
    // at start-up is owned by nobody. Any later allocation may legitimately
    // hand it to a `softIocPVX` or `oracle-ioc` search port, after which every
    // client aimed at it dies on `Address already in use` for the life of that
    // pair (measured: 45 channels of one record type, ERROR, nondeterministic).
    // The invariant this field's docs used to *state* — "must not be either
    // side's search port" — was enforced by nothing. It is now enforced by
    // construction in `crate::pvatool::PvaTools::run_raw`, which allocates the
    // port immediately before spawning, when the pair's sockets are live and
    // therefore excluded. The field is deleted rather than left unused so the
    // shared number cannot come back.
}

impl PvxTools {
    pub const DEFAULT_BIN: &'static str = "/home/stevek/work/epics-modules/pvxs/bin/linux-x86_64";

    /// The **fat** `softIocPVX` — QSRV2 plus the same busy/calc/asyn support the
    /// fat CA [`CTools::DEFAULT_IOC_BIN`] links, so the PVA ground truth serves
    /// the same record types the `.dbd` denominator enumerates.
    ///
    /// Stock pvxs `softIocPVX` cannot load six of them (`acalcout`, `asyn`,
    /// `busy`, `scalcout`, `sseq`, `transform`): it exits during boot and every
    /// channel of those types scored ERROR — 835 of 3386, which is why coverage
    /// was capped at 75.3% by the *instrument* rather than by the port. It lives
    /// under `oracle-ioc/` rather than in the pvxs tree, exactly like its CA
    /// sibling; the client tools still come from `bin`. It derives its dbd from
    /// its own exe dir, so no `-D` is passed.
    pub const DEFAULT_IOC_BIN: &'static str =
        "/home/stevek/work/oracle-ioc/bin/linux-x86_64/softIocPVX";

    /// The client tools this harness actually invokes. Verified up front so a
    /// missing binary is one loud error rather than an "errored" verdict on
    /// every case.
    const REQUIRED: [&'static str; 5] = ["pvxget", "pvxinfo", "pvxlist", "pvxmonitor", "pvxput"];

    /// Locate the pvxs tree, honoring `PVXS_BIN`.
    pub fn discover() -> Result<Self, BootError> {
        let bin = std::env::var("PVXS_BIN").unwrap_or_else(|_| Self::DEFAULT_BIN.to_string());
        let bin = PathBuf::from(bin);
        if !bin.is_dir() {
            return err(
                Side::C,
                format!(
                    "pvxs bin dir not found at {}. Set PVXS_BIN to the built \
                     linux-x86_64 bin dir. The PVA oracle cannot run without ground truth.",
                    bin.display()
                ),
            );
        }
        for tool in Self::REQUIRED {
            if !bin.join(tool).is_file() {
                return err(
                    Side::C,
                    format!("missing pvxs tool `{tool}` in {}", bin.display()),
                );
            }
        }
        // Same discipline as `CTools::discover`: the fat IOC is named, verified,
        // and its absence is one loud error. Never fall back to the stock
        // `softIocPVX` in `bin` — it cannot load six of the denominator's record
        // types, so a silent fallback would turn a missing build into 835
        // ERRORs and a 75.3% coverage number that looks like a port problem.
        let ioc_bin = std::env::var("EPICS_ORACLE_PVX_IOC_BIN")
            .unwrap_or_else(|_| Self::DEFAULT_IOC_BIN.to_string());
        let ioc_bin = PathBuf::from(ioc_bin);
        if !ioc_bin.is_file() {
            return err(
                Side::C,
                format!(
                    "fat `softIocPVX` not found at {}. Build it under oracle-ioc/ (or set \
                     EPICS_ORACLE_PVX_IOC_BIN). The PVA oracle cannot run without ground truth.",
                    ioc_bin.display()
                ),
            );
        }
        Ok(Self { bin, ioc_bin })
    }
}

/// pvxs QSRV2's `softIocPVX`, booted on the given `.db`. PVA ground truth.
pub struct PvxIoc {
    port: u16,
    child: Child,
    tail: OutputTail,
}

impl PvxIoc {
    /// Boot `softIocPVX` serving `db`, loopback-isolated, on a UDP search port
    /// nothing else holds.
    pub fn boot(tools: &PvxTools, db: &Path) -> Result<Self, BootError> {
        let mut why = Vec::new();
        for n in 1..=BOOT_ATTEMPTS {
            let udp = alloc_free_port()?;
            match Self::try_boot(tools, db, udp) {
                Ok(ioc) => return Ok(ioc),
                Err(e) if e.retryable => {
                    why.push(format!("attempt {n} on UDP {udp}: {}", e.message));
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        err(
            Side::C,
            format!(
                "softIocPVX could not get an exclusive port in {BOOT_ATTEMPTS} attempts: {}",
                why.join("; ")
            ),
        )
    }

    fn try_boot(tools: &PvxTools, db: &Path, udp: u16) -> Result<Self, BootError> {
        let mut cmd = Command::new(&tools.ioc_bin);
        cmd.arg("-S"); // no interactive shell
        // The same load route as the CA side, from the same owner: the fat
        // `softIocPVX` is a `softMain` too, so an asyn db needs its port created
        // before `iocInit` here exactly as it does there.
        load_db_into(&mut cmd, db)?;
        let mut child = cmd
            // --- PVA server, loopback only. Names verified against pvxs
            // `src/config.cpp:402-432`, not guessed. ---
            .env("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_PVAS_BEACON_ADDR_LIST", "127.0.0.1")
            // Refuse auto-beaconing outright, so no frame reaches a real
            // subnet (`config.cpp:430`).
            .env("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO")
            .env("EPICS_PVAS_BROADCAST_PORT", udp.to_string())
            // TCP is the one port that needs no allocation: pvxs binds `:0`
            // and stamps the real port back (`server.cpp:484`), then
            // advertises it in the search reply. A number nobody chose cannot
            // be a number somebody else took.
            // --- CA server: not started at all. softIocPVX links base's
            // rsrv, which binds CA 5064 by default and would fight the host's
            // real IOCs and the pair's other side. No CA is measured in this
            // phase, so the CA server is removed rather than moved: base
            // skips any `dbServer` named in `EPICS_IOC_IGNORE_SERVERS`
            // (`db/dbServer.c:32-56`), and rsrv registers itself as `rsrv`
            // (`rsrv/caservertask.c:1561-1569`).
            //
            // Moving it was the earlier fix and it could not be made sound.
            // A port has to be *named* in the environment, so it has to be
            // allocated and released before the child binds it, and whoever
            // takes it in that window kills the IOC outright: rsrv's UDP bind
            // fails silently (`caservertask.c:131-146` prints nothing on
            // EADDRINUSE), the interface is dropped, and `rsrv_init` ends at
            // `cantProceed("CAS: No TCP server started")`. Nor can the number
            // be delegated to the kernel — `envGetInetPortConfigParam`
            // (`libcom/src/env/envSubr.c:409-416`) clips a port of 0 back to
            // the 5064 default. Not registering the server has no window at
            // all. ---
            .env("EPICS_IOC_IGNORE_SERVERS", "rsrv")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BootError::new(Side::C, format!("spawn softIocPVX: {e}")))?;

        // Same ready watch as `CIoc`: softIocPVX prints base's `iocRun: All
        // initialization complete`. Note what this does NOT buy us — a *PVA*
        // search-port collision prints nothing at all (see `PvaPair::boot`),
        // and with rsrv unregistered there is no CA side left to warn either.
        // What it still catches is a death: `cantProceed` suspends the thread
        // instead of exiting, so nothing else would ever end the wait.
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let (tx, rx) = mpsc::channel::<BootSignal>();
        let tail = OutputTail::new();
        spawn_watcher(stdout, tx.clone(), tail.clone());
        spawn_watcher(stderr, tx, tail.clone());

        let deadline = Instant::now() + BOOT_TIMEOUT;
        let mut ready = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let _ = child.kill();
                return err(
                    Side::C,
                    format!(
                        "softIocPVX did not report ready within {BOOT_TIMEOUT:?} on UDP {udp}; \
                         it said: {}",
                        tail.text()
                    ),
                );
            }
            match rx.recv_timeout(left) {
                Ok(BootSignal::PortCollision(line)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return err_retryable(Side::C, format!("PORT_COLLISION on UDP {udp}: {line}"));
                }
                Ok(BootSignal::Died(said)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return err_retryable(
                        Side::C,
                        format!(
                            "softIocPVX declared itself unable to proceed on UDP {udp}; \
                                 it said: {said}"
                        ),
                    );
                }
                Ok(BootSignal::Ready) => {
                    ready = true;
                    break;
                }
                Ok(BootSignal::Other) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if !ready {
            let status = child.try_wait().ok().flatten();
            let _ = child.kill();
            return err(
                Side::C,
                format!(
                    "softIocPVX exited during boot (status {status:?}) — db rejected? \
                     it said: {}",
                    tail.text()
                ),
            );
        }
        Ok(Self {
            port: udp,
            child,
            tail,
        })
    }
}

impl Ioc for PvxIoc {
    fn port(&self) -> u16 {
        self.port
    }
    fn side(&self) -> Side {
        Side::C
    }
    fn recent_output(&self) -> String {
        self.tail.text()
    }
}

impl Drop for PvxIoc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The booted PVA differential pair: the same `.db` on both sides, each on its
/// own **proven-exclusive** UDP search port.
pub struct PvaPair {
    pub c: PvxIoc,
    pub rust: RustIoc,
}

impl PvaPair {
    /// Boot both PVA sides and return only once each is observed reachable
    /// **and** proven to be the only server on its port.
    ///
    /// The exclusivity proof is the part with no CA analogue, and it is not
    /// belt-and-braces. CA announces a shared UDP port (`cas WARNING: ... two
    /// or more servers share the same UDP port`), which is what [`CIoc::boot`]
    /// scans for. A PVA search socket sets `SO_REUSEPORT`, so a second server
    /// binds the same port **silently** — measured: two `softIocPVX` on one
    /// port, six `pvxget` of one PV, values `1,2,2,1,1,2`. Both sides here
    /// serve identical PV names from the same `.db`, so that outcome would
    /// misattribute readings with nothing in any log to show for it.
    ///
    /// There is no output to scan, so exclusivity is established the only way
    /// left: by measurement. `pvxlist` on each side's port must find exactly
    /// one server ([`crate::pvatool::PvaTools::servers`]).
    pub fn boot(tools: &PvxTools, db: &Path, probe_pv: &str) -> Result<Self, BootError> {
        retry_pair("PVA", || Self::try_boot(tools, db, probe_pv))
    }

    fn try_boot(tools: &PvxTools, db: &Path, probe_pv: &str) -> Result<Self, BootError> {
        let c = PvxIoc::boot(tools, db)?;
        let rust = RustIoc::boot_pva(db)?;
        if c.port() == rust.port() {
            return err_retryable_neither(
                "pvxs and Rust PVA servers landed on the same UDP search port — \
                 refusing to run, answers would not be attributable",
            );
        }
        wait_pva_reachable(tools, &c, probe_pv)?;
        wait_pva_reachable(tools, &rust, probe_pv)?;
        verify_sole_server(tools, &c)?;
        verify_sole_server(tools, &rust)?;
        Ok(Self { c, rust })
    }
}

/// One side of the PVA pair, replaceable in place after it dies mid-sweep.
///
/// A pvxs client read can **kill the server it reads**: a channel whose ground
/// truth overruns a buffer aborts `softIocPVX` outright, and every reading that
/// side still owed then belongs to a process that no longer exists. Recovering
/// from that is per-side — the surviving side keeps its port and its readings —
/// so the reader needs one name for "is this side still there" and "put a
/// proven-exclusive server back on it", across two sides that boot by different
/// routes.
///
/// [`CIoc`] deliberately does NOT implement this: it boots from [`CTools`] and
/// answers CA, so it is not a member of a PVA pair and has no port to keep
/// disjoint from one.
pub trait PvaServer: Ioc {
    /// How the server process ended, or `None` while it still runs.
    fn exit_status(&mut self) -> Option<std::process::ExitStatus>;

    /// Is the server still running? A process that has exited cannot have
    /// answered anything since, which is what makes a reading attributable.
    fn alive(&mut self) -> bool {
        self.exit_status().is_none()
    }

    /// Boot a replacement onto this side, on the same proof a first boot needs.
    ///
    /// `avoid` is the port the other side of the pair currently holds: both
    /// sides serve the same PV names, so a replacement that landed there would
    /// silently misattribute every reading rather than fail.
    fn reboot(
        &mut self,
        tools: &PvxTools,
        db: &Path,
        probe_pv: &str,
        avoid: u16,
    ) -> Result<(), BootError>;
}

/// Admit a freshly booted server onto a side of the pair.
///
/// **The single owner of "a replacement is admitted only on the proof an
/// original needs"**: reachable, sole server on its port, and on a port the
/// other side does not hold. Assigning through `slot` is what retires the dead
/// process — [`PvxIoc`]/[`RustIoc`]'s `Drop` reaps it — so no path can leave a
/// side holding a corpse and a live replacement at once.
fn adopt<S: Ioc>(
    slot: &mut S,
    tools: &PvxTools,
    db: &Path,
    probe_pv: &str,
    avoid: u16,
    mut boot: impl FnMut(&Path) -> Result<S, BootError>,
) -> Result<(), BootError> {
    let mut why = Vec::new();
    for n in 1..=BOOT_ATTEMPTS {
        let fresh = boot(db)?;
        let proof = if fresh.port() == avoid {
            Err(BootError::retryable(
                fresh.side(),
                format!("replacement landed on the other side's UDP port {avoid}"),
            ))
        } else {
            wait_pva_reachable(tools, &fresh, probe_pv)
                .and_then(|()| verify_sole_server(tools, &fresh))
        };
        match proof {
            Ok(()) => {
                *slot = fresh;
                return Ok(());
            }
            Err(e) if e.retryable => {
                why.push(format!("attempt {n}: {}", e.message));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    err(
        slot.side(),
        format!(
            "could not put a proven-exclusive replacement server back in \
             {BOOT_ATTEMPTS} attempts: {}",
            why.join("; ")
        ),
    )
}

impl PvaServer for PvxIoc {
    fn exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
    fn reboot(
        &mut self,
        tools: &PvxTools,
        db: &Path,
        probe_pv: &str,
        avoid: u16,
    ) -> Result<(), BootError> {
        adopt(self, tools, db, probe_pv, avoid, |db| {
            PvxIoc::boot(tools, db)
        })
    }
}

impl PvaServer for RustIoc {
    fn exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
    fn reboot(
        &mut self,
        tools: &PvxTools,
        db: &Path,
        probe_pv: &str,
        avoid: u16,
    ) -> Result<(), BootError> {
        adopt(self, tools, db, probe_pv, avoid, RustIoc::boot_pva)
    }
}

/// Poll a known channel through the real pvxs client until it reads. The PVA
/// sibling of [`wait_reachable`], sharing its [`wait_probe`] retry policy.
///
/// The probe keeps the harness's `-w`: unlike the CA tools, pvxs names no short
/// default to fall back on — `pvxget`'s own is 5.0 s (`tools/get.cpp:49`),
/// longer than what the harness already passes.
fn wait_pva_reachable(tools: &PvxTools, ioc: &dyn Ioc, probe_pv: &str) -> Result<(), BootError> {
    let t = crate::pvatool::PvaTools::new(tools, ioc.port(), ioc.side());
    wait_probe("PVA server", ioc, probe_pv, || {
        t.pvxget(probe_pv).map(|_| ())
    })
}

/// Prove exactly one PVA server answers on this IOC's port. See
/// [`PvaPair::boot`] for why this is mandatory rather than defensive.
///
/// Takes the [`Ioc`] rather than a port and a [`Side`] for the reason
/// [`wait_probe`] does: this is the last step of a pair boot and the one whose
/// failure ends the whole sweep, so its message is the only thing a reader
/// gets — and a count without the IOC's own words leaves them a number and no
/// cause ([`OutputTail`]). Both arms quote it, and the disagreeing arm also
/// quotes the addresses that answered, which is the only place they exist.
fn verify_sole_server(tools: &PvxTools, ioc: &dyn Ioc) -> Result<(), BootError> {
    let (port, side) = (ioc.port(), ioc.side());
    let t = crate::pvatool::PvaTools::new(tools, port, side);
    match t.servers() {
        Ok(s) if s.len() == 1 => Ok(()),
        Ok(s) => err_retryable(
            side,
            format!(
                "{} PVA servers answer on the {side} side's UDP port {port} — refusing to \
                 run. A PVA port collision binds silently (SO_REUSEPORT), and both sides \
                 serve the same PV names, so readings here would be misattributed rather \
                 than failed. pvxlist named: {}; it said: {}",
                s.len(),
                s.join(" | "),
                ioc.recent_output()
            ),
        ),
        // Not "assume one": the side was reachable a moment ago, so a
        // countless port is an unexplained change, and this harness does not
        // score what it could not measure.
        Err(e) => err(
            side,
            format!(
                "could not count the PVA servers on the {side} side's UDP port {port}, so \
                 exclusivity is unproven and no reading from it is attributable: {e}; \
                 it said: {}",
                ioc.recent_output()
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive `spawn_watcher` over a canned stream and collect what it decided.
    fn classify(output: &str) -> Vec<BootSignal> {
        let (tx, rx) = mpsc::channel::<BootSignal>();
        spawn_watcher(
            std::io::Cursor::new(output.to_string().into_bytes()),
            tx,
            OutputTail::new(),
        );
        rx.iter().collect()
    }

    /// The boot loop drops its receiver once the IOC is up; the tail must go on
    /// filling anyway. Without this, an IOC that a later client read kills is
    /// reported with the banner it printed at boot and nothing about the death
    /// -- and the abort contract has no evidence to name.
    #[test]
    fn the_tail_keeps_filling_after_the_boot_loop_stops_listening() {
        let tail = OutputTail::new();
        let (tx, rx) = mpsc::channel::<BootSignal>();
        drop(rx);
        spawn_watcher(
            std::io::Cursor::new(
                b"iocRun: All initialization complete\nmalloc(): unaligned fastbin chunk detected\n"
                    .to_vec(),
            ),
            tx,
            tail.clone(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !tail.text().contains("unaligned fastbin") {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            tail.text().contains("unaligned fastbin chunk detected"),
            "the dying words must survive a dropped receiver, got {:?}",
            tail.text()
        );
    }

    /// The formerly-invisible outcome. `rsrv` losing its UDP port is silent
    /// (`caservertask.c:131-146` prints nothing on EADDRINUSE), so the only
    /// thing on the wire is base's fatal banner — and `cantProceed` then
    /// suspends the thread forever rather than exiting, so no stream ever
    /// closes. Read as `Other`, this cost the full boot timeout and then
    /// reported a *reachability* failure for a process that had already said
    /// it was dead.
    #[test]
    fn a_cant_proceed_banner_is_a_death_not_noise() {
        let signals = classify(concat!(
            "CAS: No TCP server started\n",
            "CRITICAL ERROR Thread _main_ (0x7f0e4c0010a0) can't proceed, suspending.\n",
        ));
        let died = signals
            .iter()
            .filter_map(|s| match s {
                BootSignal::Died(said) => Some(said.clone()),
                _ => None,
            })
            .next()
            .expect("the fatal banner must be reported as a death");
        assert!(
            died.contains("CAS: No TCP server started"),
            "the death must quote what led to it, got {died:?}"
        );
    }

    /// Boundary — base's *recovered* collision. rsrv prints this when it
    /// moved its TCP port and kept running, which is not a death but still
    /// means the pair is no longer isolated.
    #[test]
    fn a_moved_tcp_port_is_a_collision_not_a_death() {
        let signals = classify("cas WARNING: Configured TCP port was unavailable.\n");
        assert!(
            signals
                .iter()
                .any(|s| matches!(s, BootSignal::PortCollision(_))),
            "the moved-port warning must stay a collision"
        );
        assert!(
            !signals.iter().any(|s| matches!(s, BootSignal::Died(_))),
            "a server that kept running has not died"
        );
    }

    /// Boundary — a healthy boot, so neither of the above may fire.
    #[test]
    fn an_ordinary_boot_is_ready_and_nothing_else() {
        let signals = classify(concat!(
            "Starting iocInit\n",
            "iocRun: All initialization complete\n",
        ));
        assert!(
            signals.iter().any(|s| matches!(s, BootSignal::Ready)),
            "the ready line must be recognised"
        );
        assert!(
            !signals
                .iter()
                .any(|s| matches!(s, BootSignal::Died(_) | BootSignal::PortCollision(_))),
            "a healthy boot is neither a death nor a collision"
        );
    }

    /// A retryable failure is the only kind the boot loops may take another
    /// port for; everything else must surface on the first attempt.
    #[test]
    fn only_a_port_failure_is_retryable() {
        assert!(BootError::retryable(Side::C, "PORT_COLLISION on 5064").retryable);
        assert!(BootError::retryable_neither("both sides drew one number").retryable);
        assert!(!BootError::new(Side::C, "db rejected").retryable);
        assert!(!BootError::neither("no workdir").retryable);
    }

    /// The reachability flake is inside the retry family — that membership is
    /// the whole reason a lost SEARCH no longer costs a record type.
    #[test]
    fn exhausting_the_reachability_probe_is_retryable() {
        let ioc = FakeIoc {
            port: 5075,
            side: Side::Rust,
            said: "ORACLE_IOC_READY",
        };
        let e = wait_probe("IOC", &ioc, "ORACLE:AI", || Err::<(), _>("not found"))
            .expect_err("must fail");
        assert!(e.retryable, "{e:?}");
    }

    /// A pair that comes up on the second draw is a booted pair, not an ERROR.
    #[test]
    fn a_pair_that_needs_a_second_draw_still_boots() {
        let mut n = 0;
        let got = retry_pair("CA", || {
            n += 1;
            if n < 2 {
                Err(BootError::retryable(Side::Rust, "unreachable"))
            } else {
                Ok(n)
            }
        });
        assert_eq!(got.expect("boots on the retry"), 2);
    }

    /// Exhaustion names the attempt count and **every** attempt's reason, keeps
    /// the failing side's attribution, and is itself terminal — a caller above
    /// must not retry a pair that already spent its whole budget.
    ///
    /// Every reason, not the last one: five attempts that failed five
    /// different ways used to be reported as the fifth, and which of them the
    /// reader saw was decided by ordering.
    #[test]
    fn a_pair_that_never_comes_up_exhausts_and_stops_being_retryable() {
        let mut n = 0;
        let e = retry_pair("CA", || {
            n += 1;
            Err::<(), _>(BootError::retryable(
                Side::Rust,
                format!("draw {n} unreachable"),
            ))
        })
        .expect_err("must fail");
        assert_eq!(n, BOOT_ATTEMPTS);
        assert_eq!(e.side, Some(Side::Rust));
        assert!(!e.retryable, "{e:?}");
        let each = (1..=BOOT_ATTEMPTS)
            .map(|i| format!("attempt {i}: draw {i} unreachable"))
            .collect::<Vec<_>>()
            .join("; ");
        assert_eq!(
            e.message,
            format!(
                "CA pair did not come up in {BOOT_ATTEMPTS} attempts, \
                 each on fresh ports: {each}"
            )
        );
    }

    /// A failure fresh ports cannot fix — a missing binary, a `.db` the IOC
    /// will not load — is reported on the first attempt, not five times.
    #[test]
    fn a_failure_ports_cannot_fix_is_not_retried() {
        let mut n = 0;
        let e = retry_pair("PVA", || {
            n += 1;
            Err::<(), _>(BootError::new(
                Side::Rust,
                "cannot find the `oracle-ioc` binary",
            ))
        })
        .expect_err("must fail");
        assert_eq!(
            n, 1,
            "a non-retryable failure must cost exactly one attempt"
        );
        assert_eq!(e.message, "cannot find the `oracle-ioc` binary");
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    /// An asyn db MUST go through the st.cmd route, and every other db MUST NOT.
    ///
    /// The boundary is worth pinning because the wrong branch fails *silently*:
    /// `-S -d asyn.db` still prints `iocRun: All initialization complete`, so the
    /// IOC boots and serves — with a record that never found its port
    /// (`connectDevice port ORACLEASYN not found`). Nothing downstream would
    /// report that as anything but a diff against the Rust side.
    #[test]
    fn only_an_asyn_db_is_staged_through_a_script() {
        let dir = crate::runner::workdir(None).expect("workdir");

        let plain = dir.join("plain.db");
        std::fs::write(&plain, crate::record_stmt("ai", "ORACLE:AI")).expect("write");
        let mut cmd = Command::new("softIoc");
        load_db_into(&mut cmd, &plain).expect("load");
        let a = args_of(&cmd);
        assert!(
            a.contains(&"-d".to_string()),
            "a plain db loads with -d: {a:?}"
        );
        assert!(
            !a.iter().any(|x| x.ends_with("st.cmd")),
            "a plain db needs no script: {a:?}",
        );

        let asyn = dir.join("asyn.db");
        std::fs::write(&asyn, crate::record_stmt("asyn", "ORACLE:ASYN")).expect("write");
        let mut cmd = Command::new("softIoc");
        load_db_into(&mut cmd, &asyn).expect("load");
        let a = args_of(&cmd);
        assert!(
            !a.contains(&"-d".to_string()),
            "-d would defer the script past iocInit, so the port would not exist \
             when the record connects: {a:?}",
        );
        let script = a.iter().find(|x| x.ends_with("st.cmd")).expect("a script");

        // The script must create the port BEFORE it loads the db and inits.
        let text = std::fs::read_to_string(script).expect("read script");
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {text}"))
        };
        assert!(
            at(crate::ORACLE_ASYN_PORT) < at("dbLoadRecords")
                && at("dbLoadRecords") < at("iocInit"),
            "order must be port -> load -> init, got:\n{text}",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn allocated_port_is_free_on_both_protocols() {
        let p = alloc_free_port().expect("alloc");
        assert_ne!(p, 0);
        // Never the CA default: the harness must not be able to collide with a
        // real IOC on this host, and must never be tempted to hard-code it.
        assert_ne!(p, 5064);
        // Both must be re-bindable now that the prober dropped them.
        UdpSocket::bind(("127.0.0.1", p)).expect("udp free");
        TcpListener::bind(("127.0.0.1", p)).expect("tcp free");
    }

    #[test]
    fn successive_allocations_do_not_repeat() {
        let a = alloc_free_port().unwrap();
        let b = alloc_free_port().unwrap();
        // Not a hard guarantee from the kernel, but a repeat would mean the
        // allocator is handing out a port it already gave away.
        assert_ne!(a, b);
    }

    /// A stand-in IOC: `wait_probe` reads a port, a side and the child's own
    /// last words, and nothing else, so the retry policy is testable without a
    /// process. `said` is what a real `OutputTail` would have collected.
    struct FakeIoc {
        port: u16,
        side: Side,
        said: &'static str,
    }

    impl Ioc for FakeIoc {
        fn port(&self) -> u16 {
            self.port
        }
        fn side(&self) -> Side {
            self.side
        }
        fn recent_output(&self) -> String {
            self.said.to_string()
        }
    }

    /// The gate is an attempt count, so a probe that needs the LAST attempt
    /// still succeeds — and it succeeds no matter how long the earlier probes
    /// took. That is the whole point of replacing the wall clock: under load a
    /// single killed `cainfo` used to eat the budget and the record type came
    /// back `errored` for want of one more try.
    #[test]
    fn a_probe_that_succeeds_on_the_last_attempt_is_reachable() {
        let mut n = 0;
        let ioc = FakeIoc {
            port: 5064,
            side: Side::Rust,
            said: "iocRun: All initialization complete",
        };
        let r = wait_probe("IOC", &ioc, "ORACLE:AI", || {
            n += 1;
            if n < REACHABLE_ATTEMPTS {
                Err("not found")
            } else {
                Ok(())
            }
        });
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(n, REACHABLE_ATTEMPTS);
    }

    /// Exhaustion is a named boot failure carrying the attempt count and the
    /// last probe error — never a silent per-case timeout.
    #[test]
    fn exhausting_the_attempts_names_the_count_and_the_last_error() {
        let mut n = 0;
        let ioc = FakeIoc {
            port: 34567,
            side: Side::C,
            said: "iocRun: All initialization complete",
        };
        let e = wait_probe("PVA server", &ioc, "ORACLE:AI", || {
            n += 1;
            Err::<(), _>(format!("probe {n} refused"))
        })
        .expect_err("must fail");
        assert_eq!(n, REACHABLE_ATTEMPTS);
        assert_eq!(
            e.message,
            format!(
                "C PVA server did not become reachable after {REACHABLE_ATTEMPTS} attempts \
                 on port 34567 (probe ORACLE:AI: probe {REACHABLE_ATTEMPTS} refused); \
                 it said: iocRun: All initialization complete"
            )
        );
        // The side the probe was aimed at rides WITH the failure: the case
        // builders have no other way to know which IOC did not answer.
        assert_eq!(e.side, Some(Side::C));
        assert_eq!(
            e.tool_errors("boot")
                .iter()
                .map(|t| t.side)
                .collect::<Vec<_>>(),
            vec![Side::C],
            "one failed side, one error -- not one error per side"
        );
    }

    /// The other boundary of the same message: an IOC that printed nothing at
    /// all still produces a readable failure. Before `recent_output` existed
    /// every exhaustion read as a bare budget, and a `softIoc` booting a
    /// zero-byte `.db` — which prints `iocRun: All initialization complete`
    /// and serves no records — was indistinguishable from one that never
    /// started.
    #[test]
    fn a_silent_ioc_is_quoted_as_silent_not_omitted() {
        let ioc = FakeIoc {
            port: 5075,
            side: Side::Rust,
            said: "(the IOC printed nothing)",
        };
        let e = wait_probe("IOC", &ioc, "ORACLE:AI", || Err::<(), _>("refused"))
            .expect_err("must fail");
        assert!(
            e.message.ends_with("it said: (the IOC printed nothing)"),
            "{}",
            e.message
        );
    }

    /// The exclusivity proof quotes the IOC it is about.
    ///
    /// This is the arm that used to report a bare count. It is the LAST step
    /// of a PVA pair boot, so its message is the whole of what a reader gets
    /// when a sweep comes back 56/56 errored — and it could not reach the one
    /// witness that names a cause, because it took a port and a [`Side`] and
    /// never saw the [`Ioc`] that owns the words. Driven here against a port
    /// nothing serves, so `pvxlist` finds nothing and the `Err` arm runs.
    #[test]
    fn an_unprovable_port_names_what_the_ioc_said() {
        let tools = PvxTools::discover().expect(
            "the pvxs tree must be built for the PVA oracle to have ground truth; \
             set PVXS_BIN if it is not at the default path",
        );
        let dead = alloc_free_port().expect("a port to aim at");
        let ioc = FakeIoc {
            port: dead,
            side: Side::Rust,
            said: "ORACLE_IOC_PVA_PORT 1234",
        };
        let e = verify_sole_server(&tools, &ioc).expect_err("nothing serves that port");
        assert_eq!(e.side, Some(Side::Rust));
        assert!(
            e.message.ends_with("it said: ORACLE_IOC_PVA_PORT 1234"),
            "{}",
            e.message
        );
    }

    /// The only failure recorded against both sides is one that belongs to
    /// neither.
    #[test]
    fn a_failure_belonging_to_neither_side_is_the_only_two_entry_one() {
        let both = BootError::neither("no free port on this host");
        assert_eq!(
            both.tool_errors("boot")
                .iter()
                .map(|t| t.side)
                .collect::<Vec<_>>(),
            vec![Side::C, Side::Rust]
        );
    }
}
