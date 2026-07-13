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

/// How long to wait for an IOC to report itself up before calling the boot an
/// ERROR. Generous: a slow boot must surface as an error, never as a case that
/// quietly "agreed".
const BOOT_TIMEOUT: Duration = Duration::from_secs(20);

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
    pub bin: PathBuf,
}

impl CTools {
    pub const DEFAULT_BIN: &'static str = "/home/stevek/work/epics-base/bin/linux-x86_64";
    pub const DEFAULT_DBD: &'static str = "/home/stevek/work/epics-base/dbd/softIoc.dbd";

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
        for tool in ["softIoc", "caget", "caput", "cainfo", "camonitor"] {
            if !bin.join(tool).is_file() {
                return err(format!("missing C tool `{tool}` in {}", bin.display()));
            }
        }
        Ok(Self { bin })
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
        let mut child = Command::new(tools.tool("softIoc"))
            .arg("-S") // no interactive shell
            .arg("-d")
            .arg(db)
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

    pub fn boot(db: &Path) -> Result<Self, BootError> {
        let bin = Self::binary()?;
        let mut child = Command::new(&bin)
            .arg("--db")
            .arg(db)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BootError(format!("spawn {}: {e}", bin.display())))?;

        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        // The Rust IOC prints `ORACLE_IOC_PORT <p>` after the socket is bound,
        // so the number is read back from the bind, never predicted.
        let (tx, rx) = mpsc::channel::<Result<u16, String>>();
        let ttx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(p) = line.strip_prefix("ORACLE_IOC_PORT ") {
                    let parsed = p
                        .trim()
                        .parse::<u16>()
                        .map_err(|e| format!("bad port line `{line}`: {e}"));
                    let _ = ttx.send(parsed);
                    return;
                }
            }
            let _ = ttx.send(Err(
                "oracle-ioc closed stdout without reporting a port".into()
            ));
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
    pub fn boot(tools: &CTools, db: &Path) -> Result<Self, BootError> {
        let c = CIoc::boot(tools, db)?;
        let rust = RustIoc::boot(db)?;
        if c.port() == rust.port() {
            return err(
                "C and Rust IOC landed on the same CA port — refusing to run, \
                        answers would not be attributable",
            );
        }
        Ok(Self { c, rust })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
