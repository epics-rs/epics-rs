//! A console line, and a `stderr_println!` line, reaches its file in one
//! `write`, so a stdout line written from another thread cannot land inside
//! it. Both streams share one file, as an IOC's console does when its stdout
//! and stderr go to the same log.

use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};

const CHILD: &str = "EPICS_CONSOLE_LINES_CHILD";
const N: usize = 3000;

fn console_line(n: usize) -> String {
    format!(
        "INFO  epics_ca_rs::client::transport: DIALPROBE submit n={n} target=127.0.0.1:40469 \
         workers=1 queued=1 dialing=0"
    )
}

fn stdout_line(n: usize) -> String {
    format!("realtime-ca-ioc: serving {n} records on CA port 44245")
}

/// The child: `stderr_line` on stderr and `println!` lines on stdout, from
/// two threads at once.
fn writers(stderr_line: fn(usize)) {
    // libtest has printed `test <name> ... ` with no newline; end it so the
    // first line below starts a line of its own.
    println!();
    let start = Arc::new(Barrier::new(2));
    let console = {
        let start = start.clone();
        std::thread::spawn(move || {
            start.wait();
            for n in 0..N {
                stderr_line(n);
            }
        })
    };
    start.wait();
    for n in 0..N {
        println!("{}", stdout_line(n));
    }
    console.join().unwrap();
}

/// Run `test` as a child with stdout and stderr on one file, and require every
/// line both writers wrote to be whole in it.
fn run_child(test: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("console.log");
    let out = std::fs::File::create(&path).unwrap();
    let err = out.try_clone().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .status()
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(status.success(), "{text}");

    let lines: std::collections::HashSet<&str> = text.lines().collect();
    let missing: Vec<String> = (0..N)
        .flat_map(|n| [console_line(n), stdout_line(n)])
        .filter(|l| !lines.contains(l.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "{} of {} lines torn, first {:?}",
        missing.len(),
        2 * N,
        missing.first()
    );
}

#[test]
fn console_lines_survive_a_concurrent_stdout_writer() {
    if std::env::var_os(CHILD).is_some() {
        assert!(epics_libcom_rs::runtime::log::install_console_subscriber());
        return writers(|n| {
            tracing::info!(
                target: "epics_ca_rs::client::transport",
                "DIALPROBE submit n={n} target=127.0.0.1:40469 workers=1 queued=1 dialing=0"
            )
        });
    }
    run_child("console_lines_survive_a_concurrent_stdout_writer");
}

#[test]
fn stderr_println_lines_survive_a_concurrent_stdout_writer() {
    if std::env::var_os(CHILD).is_some() {
        // The same bytes as the console line, in as many format pieces.
        return writers(|n| {
            epics_libcom_rs::stderr_println!(
                "INFO  epics_ca_rs::client::transport: DIALPROBE submit n={n} \
                 target={}:{} workers={} queued={} dialing={}",
                "127.0.0.1",
                40469,
                1,
                1,
                0
            )
        });
    }
    run_child("stderr_println_lines_survive_a_concurrent_stdout_writer");
}
