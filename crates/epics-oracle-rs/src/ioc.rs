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
//! - **Rust side:** true bind-`:0`-and-read-back. `CaServer::from_parts(db, 0)`
//!   binds the sockets and reports the port it actually got, so no number is
//!   ever guessed and nothing can take it in between.
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

use std::io::{BufRead, BufReader, Read};
use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::catool::CaTools;

/// How long to wait for an IOC to report itself up before calling the boot an
/// ERROR. Generous: a slow boot must surface as an error, never as a case that
/// quietly "agreed".
const BOOT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to keep re-probing a booted IOC for **observed** reachability
/// before declaring it a genuine boot failure.
///
/// A process reporting itself "up" is not the same as its CA layer answering a
/// search. The Rust `oracle-ioc` prints its port right after `from_parts` binds
/// the sockets — but the UDP search responder is not spawned until `run()`,
/// which happens *after* that line. Under full-run load the harness reads the
/// port and drives cases in that window, the client's search retries exhaust
/// before `run()` starts answering, and every case of the type comes back
/// "not found" — a whole record type turned `errored` by a boot race rather
/// than by any real defect. So readiness is **observed**, not assumed: poll a
/// known channel through the real C client until it connects.
const REACHABLE_TIMEOUT: Duration = Duration::from_secs(15);
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
#[derive(Debug)]
pub struct BootError(pub String);

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for BootError {}

fn err<T>(msg: impl Into<String>) -> Result<T, BootError> {
    Err(BootError(msg.into()))
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
    err("could not find a port free on both UDP and TCP after 64 tries")
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
        .map_err(|e| BootError(format!("read db {}: {e}", db.display())))?;
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
        .map_err(|e| BootError(format!("write st.cmd {}: {e}", st_cmd.display())))?;
    cmd.arg(&st_cmd);
    Ok(())
}

/// A booted IOC serving a `.db` on a known-exclusive CA port.
pub trait Ioc {
    /// The CA server port: both the UDP search port and (for a healthy boot)
    /// the advertised TCP port.
    fn port(&self) -> u16;
    /// Which side of the differential pair this is — used to label diffs.
    fn side(&self) -> Side;
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
            return err(format!(
                "C EPICS bin dir not found at {}. Set EPICS_BASE_BIN to the built \
                 linux-x86_64 bin dir. The oracle cannot run without ground truth.",
                bin.display()
            ));
        }
        for tool in ["caget", "caput", "cainfo", "camonitor"] {
            if !bin.join(tool).is_file() {
                return err(format!("missing C tool `{tool}` in {}", bin.display()));
            }
        }
        let ioc_bin = std::env::var("EPICS_ORACLE_IOC_BIN")
            .unwrap_or_else(|_| Self::DEFAULT_IOC_BIN.to_string());
        let ioc_bin = PathBuf::from(ioc_bin);
        if !ioc_bin.is_file() {
            return err(format!(
                "fat C softIoc not found at {}. Build it under oracle-ioc/ (or set \
                 EPICS_ORACLE_IOC_BIN). The oracle cannot run without ground truth.",
                ioc_bin.display()
            ));
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
}

impl CIoc {
    pub fn boot(tools: &CTools, db: &Path) -> Result<Self, BootError> {
        let mut last = String::new();
        for _ in 0..BOOT_ATTEMPTS {
            let port = alloc_free_port()?;
            match Self::try_boot(tools, db, port) {
                Ok(ioc) => return Ok(ioc),
                Err(BootError(e)) if e.contains("PORT_COLLISION") => {
                    // Racy by nature: someone took the port between our probe
                    // and softIoc's bind. Discard this port entirely and get a
                    // new one — never re-use or re-probe the losing number.
                    last = e;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        err(format!(
            "C softIoc could not get an exclusive port in {BOOT_ATTEMPTS} attempts: {last}"
        ))
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
            .map_err(|e| BootError(format!("spawn softIoc: {e}")))?;

        // softIoc announces readiness with "iocRun: All initialization
        // complete". It announces a *port collision* with a `cas WARNING`
        // about the TCP port being unavailable -- and then keeps running and
        // serving, which is the silent-wrong-answer we must never accept. So
        // we watch both streams and let the collision win over the ready line.
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let (tx, rx) = mpsc::channel::<BootSignal>();
        spawn_watcher(stdout, tx.clone());
        spawn_watcher(stderr, tx);

        let deadline = Instant::now() + BOOT_TIMEOUT;
        let mut ready = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let _ = child.kill();
                return err(format!(
                    "C softIoc did not report ready within {BOOT_TIMEOUT:?} on port {port}"
                ));
            }
            match rx.recv_timeout(left) {
                Ok(BootSignal::PortCollision(line)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return err(format!("PORT_COLLISION on {port}: {line}"));
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
            return err(format!(
                "C softIoc exited during boot (status {status:?}) — db rejected?"
            ));
        }
        Ok(Self { port, child })
    }
}

enum BootSignal {
    Ready,
    PortCollision(String),
    /// Any other output line. Carries no payload we act on, but it must still
    /// be a distinct signal so the boot loop keeps waiting rather than treating
    /// a chatty startup as silence.
    Other,
}

fn spawn_watcher(stream: impl Read + Send + 'static, tx: mpsc::Sender<BootSignal>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            // Order matters: a collision line must not be mistaken for noise,
            // and it can appear *before* the ready line.
            let sig = if line.contains("Configured TCP port was unavailable")
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
            if tx.send(sig).is_err() {
                return;
            }
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
            .map_err(|e| BootError(format!("current_exe: {e}")))?
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
            BootError(
                "cannot find the `oracle-ioc` binary. Build it \
                 (`cargo build -p epics-oracle-rs`) or set ORACLE_IOC_BIN."
                    .into(),
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
            .map_err(|e| BootError(format!("spawn {}: {e}", bin.display())))?;
        let port_line = mode.port_line();

        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        // The Rust IOC prints `<port_line> <p>` after the socket is bound, so
        // the number is read back from the bind, never predicted.
        let (tx, rx) = mpsc::channel::<Result<u16, String>>();
        let ttx = tx.clone();
        let prefix = format!("{port_line} ");
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(p) = line.strip_prefix(&prefix) {
                    let parsed = p
                        .trim()
                        .parse::<u16>()
                        .map_err(|e| format!("bad port line `{line}`: {e}"));
                    let _ = ttx.send(parsed);
                    return;
                }
            }
            let _ = ttx.send(Err(format!(
                "oracle-ioc closed stdout without reporting a `{port_line}` port"
            )));
        });
        // Capture stderr so a panic/failure has a diagnosable message rather
        // than a bare timeout.
        let (etx, erx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut buf);
            let _ = etx.send(buf);
        });

        // The reader thread reports the bound port, or explains why there is
        // none. Either way the IOC's own stderr is the useful diagnostic, so a
        // failure carries it rather than a bare "boot failed".
        let outcome = rx.recv_timeout(BOOT_TIMEOUT);
        let reason = match outcome {
            Ok(Ok(port)) => return Ok(Self { port, child }),
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
        err(format!(
            "Rust IOC failed to boot: {reason}: {}",
            first_useful_line(&detail)
        ))
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
        let c = CIoc::boot(tools, db)?;
        let rust = RustIoc::boot(db)?;
        if c.port() == rust.port() {
            return err(
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
        wait_reachable(tools, c.port(), Side::C, probe_pv)?;
        wait_reachable(tools, rust.port(), Side::Rust, probe_pv)?;
        Ok(Self { c, rust })
    }
}

/// Poll a known channel through the real C client until it connects, with
/// bounded retries and exponential backoff.
///
/// Uses `cainfo`, the same instrument the harness measures with, so "reachable"
/// means exactly "a C client can create this channel" — the property the cases
/// depend on. Returns `Ok` on the first successful connect; a
/// [`REACHABLE_TIMEOUT`] with no connect is a genuine boot failure carrying the
/// last probe error, never masked as a per-case timeout.
fn wait_reachable(tools: &CTools, port: u16, side: Side, probe_pv: &str) -> Result<(), BootError> {
    let t = CaTools::new(tools, port, side);
    let deadline = Instant::now() + REACHABLE_TIMEOUT;
    let mut backoff = REACHABLE_BACKOFF_START;
    loop {
        let last = match t.cainfo(probe_pv) {
            Ok(_) => return Ok(()),
            Err(e) => e.message,
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return err(format!(
                "{side} IOC did not become reachable within {REACHABLE_TIMEOUT:?} \
                 (probe {probe_pv}: {last})"
            ));
        }
        std::thread::sleep(backoff.min(left));
        backoff = (backoff * 2).min(REACHABLE_BACKOFF_MAX);
    }
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
            return err(format!(
                "pvxs bin dir not found at {}. Set PVXS_BIN to the built \
                 linux-x86_64 bin dir. The PVA oracle cannot run without ground truth.",
                bin.display()
            ));
        }
        for tool in Self::REQUIRED {
            if !bin.join(tool).is_file() {
                return err(format!("missing pvxs tool `{tool}` in {}", bin.display()));
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
            return err(format!(
                "fat `softIocPVX` not found at {}. Build it under oracle-ioc/ (or set \
                 EPICS_ORACLE_PVX_IOC_BIN). The PVA oracle cannot run without ground truth.",
                ioc_bin.display()
            ));
        }
        Ok(Self { bin, ioc_bin })
    }
}

/// pvxs QSRV2's `softIocPVX`, booted on the given `.db`. PVA ground truth.
pub struct PvxIoc {
    port: u16,
    child: Child,
}

impl PvxIoc {
    /// Boot `softIocPVX` serving `db`, loopback-isolated, on a UDP search port
    /// nothing else holds.
    pub fn boot(tools: &PvxTools, db: &Path) -> Result<Self, BootError> {
        let mut last = String::new();
        for _ in 0..BOOT_ATTEMPTS {
            let udp = alloc_free_port()?;
            let ca = alloc_free_port()?;
            match Self::try_boot(tools, db, udp, ca) {
                Ok(ioc) => return Ok(ioc),
                Err(BootError(e)) if e.contains("PORT_COLLISION") => {
                    last = e;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        err(format!(
            "softIocPVX could not get an exclusive port in {BOOT_ATTEMPTS} attempts: {last}"
        ))
    }

    fn try_boot(tools: &PvxTools, db: &Path, udp: u16, ca: u16) -> Result<Self, BootError> {
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
            .env("EPICS_PVAS_SERVER_PORT", "0")
            // --- CA server. softIocPVX links base's rsrv, which binds CA
            // 5064 by default: left alone it would fight the host's real IOCs
            // and both sides of this pair would collide on it (measured — the
            // `cas WARNING` in a two-IOC PVA run is CA's, not PVA's). No CA is
            // measured in this phase; the ports exist only to keep it off
            // 5064. ---
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CA_SERVER_PORT", ca.to_string())
            .env("EPICS_CAS_SERVER_PORT", ca.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BootError(format!("spawn softIocPVX: {e}")))?;

        // Same ready/collision watch as `CIoc`: softIocPVX prints base's
        // `iocRun: All initialization complete`. Note what this does NOT buy
        // us — a *PVA* search-port collision prints nothing at all (see
        // `PvaPair::boot`), so this watch covers the CA side only.
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let (tx, rx) = mpsc::channel::<BootSignal>();
        spawn_watcher(stdout, tx.clone());
        spawn_watcher(stderr, tx);

        let deadline = Instant::now() + BOOT_TIMEOUT;
        let mut ready = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let _ = child.kill();
                return err(format!(
                    "softIocPVX did not report ready within {BOOT_TIMEOUT:?} on UDP {udp}"
                ));
            }
            match rx.recv_timeout(left) {
                Ok(BootSignal::PortCollision(line)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return err(format!("PORT_COLLISION on CA {ca}: {line}"));
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
            return err(format!(
                "softIocPVX exited during boot (status {status:?}) — db rejected?"
            ));
        }
        Ok(Self { port: udp, child })
    }
}

impl Ioc for PvxIoc {
    fn port(&self) -> u16 {
        self.port
    }
    fn side(&self) -> Side {
        Side::C
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
    /// one server ([`crate::pvatool::PvaTools::server_count`]).
    pub fn boot(tools: &PvxTools, db: &Path, probe_pv: &str) -> Result<Self, BootError> {
        let c = PvxIoc::boot(tools, db)?;
        let rust = RustIoc::boot_pva(db)?;
        if c.port() == rust.port() {
            return err(
                "pvxs and Rust PVA servers landed on the same UDP search port — \
                 refusing to run, answers would not be attributable",
            );
        }
        wait_pva_reachable(tools, c.port(), Side::C, probe_pv)?;
        wait_pva_reachable(tools, rust.port(), Side::Rust, probe_pv)?;
        verify_sole_server(tools, c.port(), Side::C)?;
        verify_sole_server(tools, rust.port(), Side::Rust)?;
        Ok(Self { c, rust })
    }
}

/// Poll a known channel through the real pvxs client until it reads, with
/// bounded retries and exponential backoff. The PVA sibling of
/// [`wait_reachable`].
fn wait_pva_reachable(
    tools: &PvxTools,
    port: u16,
    side: Side,
    probe_pv: &str,
) -> Result<(), BootError> {
    let t = crate::pvatool::PvaTools::new(tools, port, side);
    let deadline = Instant::now() + REACHABLE_TIMEOUT;
    let mut backoff = REACHABLE_BACKOFF_START;
    loop {
        let last = match t.pvxget(probe_pv) {
            Ok(_) => return Ok(()),
            Err(e) => e.message,
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return err(format!(
                "{side} PVA server did not become reachable within {REACHABLE_TIMEOUT:?} \
                 (probe {probe_pv}: {last})"
            ));
        }
        std::thread::sleep(backoff.min(left));
        backoff = (backoff * 2).min(REACHABLE_BACKOFF_MAX);
    }
}

/// Prove exactly one PVA server answers on `port`. See [`PvaPair::boot`] for
/// why this is mandatory rather than defensive.
fn verify_sole_server(tools: &PvxTools, port: u16, side: Side) -> Result<(), BootError> {
    let t = crate::pvatool::PvaTools::new(tools, port, side);
    match t.server_count() {
        Ok(1) => Ok(()),
        Ok(n) => err(format!(
            "{n} PVA servers answer on the {side} side's UDP port {port} — refusing to \
             run. A PVA port collision binds silently (SO_REUSEPORT), and both sides \
             serve the same PV names, so readings here would be misattributed rather \
             than failed."
        )),
        // Not "assume one": the side was reachable a moment ago, so a
        // countless port is an unexplained change, and this harness does not
        // score what it could not measure.
        Err(e) => err(format!(
            "could not count the PVA servers on the {side} side's UDP port {port}, so \
             exclusivity is unproven and no reading from it is attributable: {e}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = std::env::temp_dir().join(format!("oracle_load_route_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("workdir");

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
}
