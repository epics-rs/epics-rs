//! An IOC told to stop by SIGTERM or SIGINT ends killed by that signal, as
//! C softIoc does (status 143 / 130 to a shell), after the exit callbacks
//! have run.
//!
//! The exit callbacks are the port's: C installs no handler and runs none.
//! A reactor-free build installs no handler either, so there the signal's
//! default action ends the process with no callbacks, which is C exactly.
//!
//! Linux only: whether the child has a handler yet is read from
//! `/proc/<pid>/status`.
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "IOC_SIGNAL_EXIT_CHILD";
const READY: &str = "iocRun: All initialization complete";
const AT_EXIT: &str = "ioc-signal-exit: at exit";

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

/// This test run again as a child IOC. `ignore` names a signal the child
/// starts with at `SIG_IGN`, as a background job starts with SIGINT.
fn spawn_child(test: &str, ignore: Option<i32>) -> Ioc {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        // Held open: at EOF the shell ends and the IOC exits, as C's does.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    Ioc(cmd.spawn().unwrap())
}

/// The child's stdout and stderr lines, once iocInit is done.
fn wait_ready(child: &mut Child) -> std::sync::mpsc::Receiver<String> {
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
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    loop {
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the child IOC never reported ready");
        if line.contains(READY) {
            return rx;
        }
    }
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

/// Wait until the child catches `sig`: the handler is installed when the
/// post-init wait first polls, after iocInit has reported.
fn wait_caught(child: &Child, sig: i32) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
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

fn rest(rx: &std::sync::mpsc::Receiver<String>) -> Vec<String> {
    std::thread::sleep(Duration::from_millis(100));
    rx.try_iter().collect()
}

/// The child: an IOC whose server never stops, with one exit callback.
async fn child_ioc() {
    epics_base_rs::runtime::exit::at_exit("ioc_signal_exit", || println!("{AT_EXIT}"));
    IocApplication::new()
        .port(0)
        .run(|_config| async move {
            // As a server's `rsrv_run` would: iocInit waits for it.
            epics_base_rs::server::db_server::announce_serving("probe");
            // Waits on a waker someone holds, as a server's socket does; the
            // exec backend cancels a task nothing can wake.
            let (keep, never) = epics_base_rs::runtime::sync::oneshot::channel::<()>();
            std::mem::forget(keep);
            let _ = never.await;
            Ok(())
        })
        .await
        .unwrap();
    unreachable!("the runner never returns");
}

/// Stop a child IOC with `sig` and check it ends killed by it.
fn stopped_by(test: &str, sig: i32) {
    let mut child = spawn_child(test, None);
    let rx = wait_ready(&mut child);
    if cfg!(tokio_backend) {
        wait_caught(&child, sig);
    }
    kill(&child, sig);
    let st = ended(&mut child, fact_budget::FACT_BUDGET).expect("the IOC outlived the signal");
    assert_eq!(st.signal(), Some(sig), "{st:?}");
    let ran = rest(&rx).iter().any(|l| l.contains(AT_EXIT));
    assert_eq!(ran, cfg!(tokio_backend), "exit callbacks ran: {ran}");
}

#[epics_base_rs::epics_test]
async fn sigterm_ends_the_ioc_killed_by_sigterm() {
    if std::env::var_os(CHILD).is_some() {
        return child_ioc().await;
    }
    stopped_by("sigterm_ends_the_ioc_killed_by_sigterm", libc::SIGTERM);
}

#[epics_base_rs::epics_test]
async fn sigint_ends_the_ioc_killed_by_sigint() {
    if std::env::var_os(CHILD).is_some() {
        return child_ioc().await;
    }
    stopped_by("sigint_ends_the_ioc_killed_by_sigint", libc::SIGINT);
}

/// Start a child IOC with `ignored` at `SIG_IGN`: it keeps ignoring it, as
/// C does, and `other` still ends it.
fn ignores_inherited(test: &str, ignored: i32, other: i32) {
    let mut child = spawn_child(test, Some(ignored));
    let _rx = wait_ready(&mut child);
    if cfg!(tokio_backend) {
        wait_caught(&child, other);
    }
    assert!(
        !sig_bit(&child, "SigCgt", ignored),
        "a handler replaced SIG_IGN"
    );
    assert!(sig_bit(&child, "SigIgn", ignored));
    kill(&child, ignored);
    assert_eq!(ended(&mut child, Duration::from_millis(500)), None);
    kill(&child, other);
    let st = ended(&mut child, fact_budget::FACT_BUDGET).expect("the IOC outlived the signal");
    assert_eq!(st.signal(), Some(other), "{st:?}");
}

#[epics_base_rs::epics_test]
async fn an_inherited_sig_ign_sigint_is_kept() {
    if std::env::var_os(CHILD).is_some() {
        return child_ioc().await;
    }
    ignores_inherited(
        "an_inherited_sig_ign_sigint_is_kept",
        libc::SIGINT,
        libc::SIGTERM,
    );
}

#[epics_base_rs::epics_test]
async fn an_inherited_sig_ign_sigterm_is_kept() {
    if std::env::var_os(CHILD).is_some() {
        return child_ioc().await;
    }
    ignores_inherited(
        "an_inherited_sig_ign_sigterm_is_kept",
        libc::SIGTERM,
        libc::SIGINT,
    );
}
