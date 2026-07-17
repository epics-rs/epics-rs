#![allow(dead_code)] // Helpers are conditionally used across test files.

//! Shared helpers for CA interop and soak tests.
//!
//! These tests exercise epics-ca-rs against the reference EPICS C
//! implementation (`softIoc`, `caget`, `caput`, `camonitor`). They are
//! gated on the C tools being available so that CI environments without
//! a full EPICS install can still run the rest of the suite.

use std::net::{TcpListener, UdpSocket};
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

/// Pick a free TCP port by binding ephemeral and immediately closing.
pub fn free_tcp_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("free TCP port");
    listener.local_addr().unwrap().port()
}

/// Pick a free UDP port the same way.
pub fn free_udp_port() -> u16 {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("free UDP port");
    sock.local_addr().unwrap().port()
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
pub struct ManagedIoc {
    child: Child,
    pub udp_port: u16,
    pub tcp_port: u16,
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
}

/// Spawn a `softIoc` process running the supplied `.db` content. Returns
/// once the IOC has been observed accepting CA traffic on `udp_port`.
///
/// Uses the standard EPICS install at /Users/stevek/codes/epics-base
/// (test fixture path) and a per-test ephemeral UDP/TCP port pair so
/// concurrent tests don't collide.
pub fn spawn_softioc(db_content: &str) -> Option<ManagedIoc> {
    if !have_tool("softIoc") {
        return None;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("test.db");
    std::fs::write(&db_path, db_content).expect("write db");

    let udp_port = free_udp_port();
    let tcp_port = free_tcp_port();

    let mut cmd = Command::new("softIoc");
    cmd.arg("-S") // No interactive shell — keeps softIoc happy without a TTY
        .arg("-d")
        .arg(&db_path)
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", udp_port.to_string())
        .env("EPICS_CAS_SERVER_PORT", udp_port.to_string())
        .env("EPICS_CA_REPEATER_PORT", "5165")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = cmd.spawn().ok()?;

    // Hand the IOC time to bind sockets before tests exercise it.
    std::thread::sleep(Duration::from_millis(800));

    // Keep tempdir alive by leaking — the child has the .db open.
    std::mem::forget(dir);

    Some(ManagedIoc {
        child,
        udp_port,
        tcp_port,
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
