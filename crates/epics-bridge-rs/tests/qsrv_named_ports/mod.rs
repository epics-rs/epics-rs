//! Boot `qsrv-rs` on a TCP port the test names, and retry on a stolen one.
//!
//! The IOC's own pvalink client finds the server by `port + 1` on UDP, so the
//! child cannot be handed port 0 and asked to report: the number has to be
//! named before it starts. A candidate lost between the probe and the bind is
//! a retry (`named_port::verdict::qsrv_rs_port_was_taken`), anything else is
//! a failure.

use std::future::Future;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Run `qsrv-rs --db-file db` plus `args(tcp)` with `env(tcp)` on candidate
/// ports until `probe(tcp)` returns `Some` — the subject answered on the number it was
/// told. The probe runs after `iocRun`.
pub async fn on_a_qsrv_port<T, F, Fut>(
    db: &Path,
    args: impl Fn(u16) -> Vec<String>,
    env: impl Fn(u16) -> Vec<(&'static str, String)>,
    probe: F,
) -> T
where
    F: Fn(u16) -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let mut candidates = named_port::Candidates::new(named_port::free_tcp_with_udp_successor);
    for tcp in candidates.by_ref() {
        let mut child = Command::new(env!("CARGO_BIN_EXE_qsrv-rs"))
            .arg("--db-file")
            .arg(db)
            .args(args(tcp))
            .envs(env(tcp))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn qsrv-rs");
        let console = Arc::new(Mutex::new(String::new()));
        let up = Arc::new(Mutex::new(false));
        let reader = {
            let (console, up) = (console.clone(), up.clone());
            let stderr = child.stderr.take().unwrap();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut c = console.lock().unwrap();
                    c.push_str(&line);
                    c.push('\n');
                    if line.starts_with("iocRun: ") {
                        *up.lock().unwrap() = true;
                    }
                }
            })
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !*up.lock().unwrap()
            && Instant::now() < deadline
            && child.try_wait().unwrap().is_none()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // A child that already exited cannot answer; the probe would only
        // spend its timeout.
        let won = if child.try_wait().unwrap().is_none() {
            probe(tcp).await
        } else {
            None
        };
        let _ = child.kill();
        let _ = child.wait();
        reader.join().unwrap();
        let console = console.lock().unwrap().clone();
        if let Some(won) = won {
            return won;
        }
        let udp_held = named_port::udp_port_is_held(tcp + 1);
        if !named_port::verdict::qsrv_rs_port_was_taken(&console, tcp, udp_held) {
            panic!(
                "qsrv-rs on tcp {tcp} never answered, and its console does not name a stolen port:\n{console}"
            );
        }
    }
    candidates.exhausted()
}
