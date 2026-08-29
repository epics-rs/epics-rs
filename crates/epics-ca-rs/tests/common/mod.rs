#![allow(dead_code)] // Helpers are conditionally used across test files.

//! Shared helpers for CA interop and soak tests.
//!
//! These tests exercise epics-ca-rs against the reference EPICS C
//! implementation (`softIoc`, `caget`, `caput`, `camonitor`). They are
//! gated on the C tools being available so that CI environments without
//! a full EPICS install can still run the rest of the suite.

#[path = "budget.rs"]
pub mod budget;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Returns true when the named binary resolves on PATH or in the local
/// EPICS install. Used by interop tests to early-exit on hosts that
/// lack a C reference implementation.
pub fn have_tool(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Skip the test (printed to stderr) when a required C tool is missing.
/// Returns `true` when the test should proceed.
pub fn require_tool(name: &str) -> bool {
    if have_tool(name) {
        true
    } else {
        eprintln!("SKIP: `{name}` not found on PATH; install EPICS base to run this test");
        false
    }
}

/// Collects a child's stream line-by-line on a background thread so tests
/// can wait for the OUTPUT THEY ASSERT ON instead of sleeping a fixed
/// interval and hoping the child got there. A fixed sleep is a load-sensitive
/// guess (the cli_monitor_separator CI flake); waiting on the line itself
/// only leaves a generous outer deadline that turns a hung child into a
/// named failure.
pub struct LineCollector {
    state: Arc<(Mutex<(String, bool)>, Condvar)>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl LineCollector {
    /// Start collecting from `stream` (a piped child stdout/stderr).
    pub fn spawn<R: std::io::Read + Send + 'static>(stream: R) -> Self {
        let state = Arc::new((Mutex::new((String::new(), false)), Condvar::new()));
        let thread_state = state.clone();
        let handle = std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                let eof =
                    !matches!(std::io::BufRead::read_line(&mut reader, &mut line), Ok(n) if n > 0);
                let (lock, cvar) = &*thread_state;
                let mut guard = lock.lock().unwrap();
                if eof {
                    guard.1 = true;
                    cvar.notify_all();
                    return;
                }
                guard.0.push_str(&line);
                cvar.notify_all();
            }
        });
        Self {
            state,
            handle: Some(handle),
        }
    }

    /// Block until `pred(text-so-far)` holds; `false` when the stream hit
    /// EOF or `deadline` elapsed with the predicate never satisfied.
    pub fn wait_for(&self, deadline: Duration, pred: impl Fn(&str) -> bool) -> bool {
        let end = Instant::now() + deadline;
        let (lock, cvar) = &*self.state;
        let mut guard = lock.lock().unwrap();
        loop {
            if pred(&guard.0) {
                return true;
            }
            if guard.1 {
                return false;
            }
            let now = Instant::now();
            if now >= end {
                return false;
            }
            let (g, timeout) = cvar.wait_timeout(guard, end - now).unwrap();
            guard = g;
            if timeout.timed_out() {
                return pred(&guard.0);
            }
        }
    }

    /// Snapshot of everything read so far (for failure messages).
    pub fn text(&self) -> String {
        let (lock, _) = &*self.state;
        lock.lock().unwrap().0.clone()
    }

    /// Everything the stream produced. Call after the child is killed or
    /// exited — joins the reader thread, which ends at pipe EOF.
    pub fn into_text(mut self) -> String {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let (lock, _) = &*self.state;
        let guard = lock.lock().unwrap();
        guard.0.clone()
    }
}

/// A child process that's killed when dropped, plus the `EPICS_CA_*`
/// environment overrides callers should propagate to clients/IOCs that
/// need to talk to it.
///
/// The console readers live as long as the child: an undrained pipe stops a
/// chatty IOC at the first full buffer, and the text they collect is what a
/// failure here reports instead of a bare timeout. There is no `tcp_port` —
/// C binds its TCP listener on `EPICS_CAS_SERVER_PORT` too, so the second
/// number this used to carry was one nothing ever bound and nothing ever read.
pub struct ManagedIoc {
    child: Child,
    pub udp_port: u16,
    out: LineCollector,
    err: LineCollector,
}

impl Drop for ManagedIoc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl ManagedIoc {
    pub fn ca_addr_list(&self) -> String {
        format!("127.0.0.1:{}", self.udp_port)
    }

    /// Everything the IOC has printed so far, both streams, for a failure
    /// message.
    pub fn console(&self) -> String {
        format!("{}{}", self.out.text(), self.err.text())
    }
}

mod softioc_verdict;
pub use softioc_verdict::{SoftIocVerdict, softioc_verdict};

/// Write `db_content` where a child can read it, for the child's lifetime.
fn db_file(db_content: &str) -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("test.db");
    std::fs::write(&db_path, db_content).expect("write db");
    // Keep the tempdir alive by leaking — the child has the .db open.
    std::mem::forget(dir);
    db_path
}

/// Spawn a `softIoc` on `port`, or `None` when that number was taken between
/// the probe and C's bind.
///
/// `None` means [`softioc_verdict::PORT_WAS_TAKEN`] and nothing else. Every other way this can
/// go wrong — the binary refusing to start, the IOC dying, neither line
/// arriving inside the budget — panics with the console, because none of them
/// gets better on a different port.
fn softioc_on(db_path: &Path, port: u16) -> Option<ManagedIoc> {
    let mut child = Command::new("softIoc")
        .arg("-S") // No interactive shell — keeps softIoc happy without a TTY
        .arg("-d")
        .arg(db_path)
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .env("EPICS_CAS_SERVER_PORT", port.to_string())
        .env("EPICS_CA_REPEATER_PORT", "5165")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            panic!("`softIoc` was found on PATH but would not start: {e}");
        });

    let out = LineCollector::spawn(child.stdout.take().expect("piped stdout"));
    let err = LineCollector::spawn(child.stderr.take().expect("piped stderr"));
    // Owned before it is judged, so every way out of here reaps it — the
    // panic below included. A `softIoc` that lost the port does NOT exit: it
    // suspends in `rsrv_init`, still holding whatever it did manage to bind,
    // so a losing attempt that is merely dropped keeps a UDP socket for the
    // rest of the run and the retry is racing its own leftovers.
    let ioc = ManagedIoc {
        child,
        udp_port: port,
        out,
        err,
    };
    // Both lines go through errlog, which is stderr; stdout is collected for
    // the panic text, because an IOC that fails some other way may say so
    // there and that belongs in the report.
    let settled = ioc.err.wait_for(budget::FACT_BUDGET, |t| {
        softioc_verdict(t) != SoftIocVerdict::Silent
    });
    assert!(
        settled,
        "`softIoc` neither came up nor reported the port taken within {:?}:\n{}",
        budget::FACT_BUDGET,
        ioc.console()
    );
    match softioc_verdict(&ioc.err.text()) {
        SoftIocVerdict::Up => Some(ioc),
        SoftIocVerdict::PortTaken => None,
        // `wait_for` only returned true because the verdict was not Silent, so
        // this arm is unreachable; it is spelled out rather than `unreachable!`
        // because a future third verdict must be decided here, not defaulted.
        SoftIocVerdict::Silent => unreachable!("wait_for returned on a Silent console"),
    }
}

/// Spawn a `softIoc` process running the supplied `.db` content. Returns
/// once the IOC has said it is serving.
///
/// The port is a candidate, not a reservation — see the `named-port` crate
/// for the rule and [`softioc_verdict::PORT_WAS_TAKEN`] for the evidence this one retries on.
///
/// Panics instead of reporting an absence. The absent-prerequisite skip is
/// owned by [`require_tool`] at the call site, so by the time this runs
/// `softIoc` has already been located on `PATH` and every remaining failure
/// is the located binary refusing to start. An `Option` return meant both
/// things at once, and every caller spelled the `None` arm as an early
/// return, which nextest scores as a pass — see `epics-base-rs`
/// `src/reference.rs:80-96`. It names no install directory at all — this
/// comment used to claim a hardcoded `/Users/...` macOS path, which was
/// never what the code did and could not have worked on this host.
pub fn spawn_softioc(db_content: &str) -> ManagedIoc {
    let db_path = db_file(db_content);
    named_port::on_a_named_port(|port| softioc_on(&db_path, port))
}

/// Spawn a `softIoc` on exactly `port`.
///
/// For the one caller whose subject is the number itself — an IOC restarting
/// where the first one was — so a fresh candidate would be a different test.
/// A steal is a failure here rather than a retry, and it fails saying so
/// instead of leaving a client to time out against a stranger's socket.
pub fn spawn_softioc_on(db_content: &str, port: u16) -> ManagedIoc {
    let db_path = db_file(db_content);
    softioc_on(&db_path, port).unwrap_or_else(|| {
        panic!("port {port} was taken before `softIoc` could re-bind it");
    })
}

/// Run a one-shot `caget` and return stdout (trimmed). Returns None on
/// non-zero exit.
pub fn run_caget(addr_list: &str, server_port: u16, pv: &str) -> Option<String> {
    let out = Command::new("caget")
        .arg("-w")
        .arg("3")
        .arg(pv)
        .env("EPICS_CA_ADDR_LIST", addr_list)
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", server_port.to_string())
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("caget failed: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run a one-shot `caput` and return success/failure.
pub fn run_caput(addr_list: &str, server_port: u16, pv: &str, value: &str) -> bool {
    Command::new("caput")
        .arg("-w")
        .arg("3")
        .arg(pv)
        .arg(value)
        .env("EPICS_CA_ADDR_LIST", addr_list)
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", server_port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
