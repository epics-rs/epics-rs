//! SIGTERM/SIGINT end a CA IOC killed by the signal, as they end C softIoc,
//! once the exit callbacks have run. The rust-only SIGTERM drain runs only
//! when `EPICS_CAS_DRAIN_GRACE_SECS` is set.
//!
//! One case per boundary: a server run alone or under `IocApplication`,
//! drain set or unset, SIGTERM or SIGINT, and an inherited `SIG_IGN`.
//!
//! Linux only: whether the child has a handler yet is read from
//! `/proc/<pid>/status`. The CA server is `tokio_backend` only.
#![cfg(all(target_os = "linux", tokio_backend))]

use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const CHILD: &str = "CA_SIGNAL_EXIT_CHILD";
const DRAIN: &str = "EPICS_CAS_DRAIN_GRACE_SECS";
const AT_EXIT: &str = "ca-signal-exit: at exit";

/// The child IOC, killed when dropped so a failed assertion does not leave
/// it running.
struct Ioc(Child);

impl std::ops::Deref for Ioc {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Ioc {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for Ioc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// This test run again as the child, with the drain grace `drain` and
/// `ignore` at `SIG_IGN`.
fn spawn_child(test: &str, drain: Option<&str>, ignore: Option<i32>) -> (Ioc, Receiver<String>) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env_remove(DRAIN)
        // Held open: at EOF the shell ends and the IOC exits, as C's does.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(grace) = drain {
        cmd.env(DRAIN, grace);
    }
    if let Some(sig) = ignore {
        use std::os::unix::process::CommandExt;
        // SAFETY: `signal` is async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                libc::signal(sig, libc::SIG_IGN);
                Ok(())
            });
        }
    }
    let mut child = Ioc(cmd.spawn().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    fn forward(out: impl std::io::Read + Send + 'static, tx: std::sync::mpsc::Sender<String>) {
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    forward(child.stdout.take().unwrap(), tx.clone());
    forward(child.stderr.take().unwrap(), tx);
    (child, rx)
}

/// `sig`'s bit in the child's `field` line (`SigCgt`, `SigIgn`).
fn sig_bit(child: &Child, field: &str, sig: i32) -> bool {
    let status = std::fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap();
    let mask = status
        .lines()
        .find_map(|l| l.strip_prefix(field)?.strip_prefix(':'))
        .unwrap();
    u64::from_str_radix(mask.trim(), 16).unwrap() & (1 << (sig - 1)) != 0
}

/// Wait until the child catches `sig`.
fn wait_caught(child: &Child, sig: i32) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !sig_bit(child, "SigCgt", sig) {
        assert!(Instant::now() < deadline, "the child never caught {sig}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn kill(child: &Child, sig: i32) {
    // SAFETY: a signal to our own child's pid.
    assert_eq!(unsafe { libc::kill(child.id() as i32, sig) }, 0);
}

/// The child's status if it ends within `within`.
fn ended(child: &mut Child, within: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(st) = child.try_wait().unwrap() {
            return Some(st);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Check the child ends killed by `sig` within `within`, having run its
/// exit callbacks.
fn killed_by(mut child: Ioc, rx: &Receiver<String>, sig: i32, within: Duration) {
    let st = ended(&mut child, within).expect("the IOC outlived the signal");
    assert_eq!(st.signal(), Some(sig), "{st:?}");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        rx.try_iter().any(|l| l.contains(AT_EXIT)),
        "the exit callbacks did not run"
    );
}

fn at_exit() {
    epics_base_rs::runtime::exit::at_exit("ca_signal_exit", || println!("{AT_EXIT}"));
}

/// The child: a CA server run alone.
async fn standalone() {
    at_exit();
    let server = epics_ca_rs::server::CaServer::builder()
        .port(0)
        .tcp_port(0)
        .build()
        .await
        .unwrap();
    let _ = server.run().await;
    unreachable!("the server never stops");
}

/// A server alone, drain unset: `sig` ends it at once.
fn standalone_stopped_by(test: &str, sig: i32) {
    let (child, rx) = spawn_child(test, None, None);
    wait_caught(&child, sig);
    kill(&child, sig);
    killed_by(child, &rx, sig, Duration::from_secs(5));
}

#[epics_base_rs::epics_test]
async fn standalone_sigterm_without_drain_ends_at_once() {
    if std::env::var_os(CHILD).is_some() {
        return standalone().await;
    }
    standalone_stopped_by(
        "standalone_sigterm_without_drain_ends_at_once",
        libc::SIGTERM,
    );
}

#[epics_base_rs::epics_test]
async fn standalone_sigint_ends_at_once() {
    if std::env::var_os(CHILD).is_some() {
        return standalone().await;
    }
    standalone_stopped_by("standalone_sigint_ends_at_once", libc::SIGINT);
}

/// Drain set to 1 s: the server outlives the SIGTERM for the grace, then
/// ends killed by it.
fn drains_then_ends(child: Ioc, rx: Receiver<String>) {
    let mut child = child;
    wait_caught(&child, libc::SIGTERM);
    kill(&child, libc::SIGTERM);
    assert_eq!(ended(&mut child, Duration::from_millis(400)), None);
    killed_by(child, &rx, libc::SIGTERM, Duration::from_secs(10));
}

#[epics_base_rs::epics_test]
async fn standalone_sigterm_with_drain_waits_the_grace() {
    if std::env::var_os(CHILD).is_some() {
        return standalone().await;
    }
    let (child, rx) = spawn_child(
        "standalone_sigterm_with_drain_waits_the_grace",
        Some("1"),
        None,
    );
    drains_then_ends(child, rx);
}

#[epics_base_rs::epics_test]
async fn standalone_sigint_with_drain_ends_at_once() {
    if std::env::var_os(CHILD).is_some() {
        return standalone().await;
    }
    let (child, rx) = spawn_child(
        "standalone_sigint_with_drain_ends_at_once",
        Some("30"),
        None,
    );
    wait_caught(&child, libc::SIGINT);
    kill(&child, libc::SIGINT);
    killed_by(child, &rx, libc::SIGINT, Duration::from_secs(5));
}

#[epics_base_rs::epics_test]
async fn standalone_keeps_an_inherited_sig_ign() {
    if std::env::var_os(CHILD).is_some() {
        return standalone().await;
    }
    let (mut child, rx) = spawn_child(
        "standalone_keeps_an_inherited_sig_ign",
        None,
        Some(libc::SIGINT),
    );
    wait_caught(&child, libc::SIGTERM);
    assert!(!sig_bit(&child, "SigCgt", libc::SIGINT));
    kill(&child, libc::SIGINT);
    assert_eq!(ended(&mut child, Duration::from_millis(500)), None);
    kill(&child, libc::SIGTERM);
    killed_by(child, &rx, libc::SIGTERM, Duration::from_secs(5));
}

/// The child: a CA IOC under `IocApplication`.
async fn ioc() {
    at_exit();
    epics_ca_rs::server::run_ca_ioc_app(
        epics_base_rs::server::ioc_app::IocApplication::new().port(0),
    )
    .await
    .unwrap();
    unreachable!("the IOC never stops");
}

#[epics_base_rs::epics_test]
async fn ioc_sigterm_without_drain_ends_at_once() {
    if std::env::var_os(CHILD).is_some() {
        return ioc().await;
    }
    let (child, rx) = spawn_child("ioc_sigterm_without_drain_ends_at_once", None, None);
    wait_caught(&child, libc::SIGTERM);
    kill(&child, libc::SIGTERM);
    killed_by(child, &rx, libc::SIGTERM, Duration::from_secs(5));
}

#[epics_base_rs::epics_test]
async fn ioc_sigterm_with_drain_waits_the_grace() {
    if std::env::var_os(CHILD).is_some() {
        return ioc().await;
    }
    let (child, rx) = spawn_child("ioc_sigterm_with_drain_waits_the_grace", Some("1"), None);
    drains_then_ends(child, rx);
}

#[epics_base_rs::epics_test]
async fn ioc_sigint_with_drain_ends_at_once() {
    if std::env::var_os(CHILD).is_some() {
        return ioc().await;
    }
    let (child, rx) = spawn_child("ioc_sigint_with_drain_ends_at_once", Some("30"), None);
    wait_caught(&child, libc::SIGINT);
    kill(&child, libc::SIGINT);
    killed_by(child, &rx, libc::SIGINT, Duration::from_secs(5));
}
