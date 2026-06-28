//! End-to-end tests for the procserv supervisor.
//!
//! Spins up an in-process [`ProcServ`] wrapping a real child program
//! (`/bin/cat`, `/bin/echo`) and connects to it via a real TCP
//! socket. Exercises the same code paths the daemon binary uses,
//! minus the daemonize step.
//!
//! These tests are gated to `cfg(unix)` (forkpty) and depend on
//! `/bin/cat` / `/bin/echo` being present.

#![cfg(unix)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use epics_tools_rs::procserv::{
    ProcServ, ProcServConfig,
    config::{ChildConfig, KeyBindings, ListenConfig, LoggingConfig},
    restart::{RestartMode, RestartPolicy},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep, timeout};

/// Build a config wrapping `/bin/cat` on a random localhost port.
fn cat_config(port: u16) -> ProcServConfig {
    ProcServConfig {
        foreground: true,
        listen: ListenConfig {
            tcp_port: Some(port),
            tcp_bind: Some(SocketAddr::from(([127, 0, 0, 1], port))),
            log_port: None,
            log_bind: None,
            unix_path: None,
        },
        keys: KeyBindings {
            kill: Some(0x18),
            toggle_restart: Some(0x14),
            restart: Some(0x12),
            quit: None,
            logout: Some(0x1d),
        },
        child: ChildConfig {
            name: "cat".into(),
            program: PathBuf::from("/bin/cat"),
            args: vec![],
            cwd: None,
            kill_signal: 9,
            ignore_chars: Vec::new(),
        },
        logging: LoggingConfig {
            log_path: None,
            pid_path: None,
            info_path: None,
            stamp_log: false,
            time_format: "%Y-%m-%dT%H:%M:%S".into(),
            stamp_format: "[%Y-%m-%dT%H:%M:%S] ".into(),
        },
        restart: RestartPolicy::default(),
        restart_mode: RestartMode::Disabled, // don't auto-restart in tests
        holdoff: Duration::from_millis(50),
        wait_for_manual_start: false,
    }
}

/// Allocate an OS-assigned localhost port: bind to :0, query, drop.
async fn pick_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

/// Read up to `deadline` and return everything that arrived.
async fn read_for(stream: &mut TcpStream, dur: Duration) -> Vec<u8> {
    let deadline = Instant::now() + dur;
    let mut buf = Vec::new();
    let mut tmp = vec![0u8; 1024];
    while Instant::now() < deadline {
        match timeout(Duration::from_millis(100), stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break, // EOF
            Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
            Ok(Err(_)) => break,
            Err(_) => continue, // timeout — keep waiting
        }
    }
    buf
}

/// Read (stripping IAC) until `needle` appears in the accumulated,
/// decoded output or `dur` elapses. Returns the text seen so far —
/// callers assert on the returned string. Unlike [`read_for`] this
/// returns as soon as the marker shows up, so timing assertions
/// measure the real latency rather than a fixed window.
async fn read_until(stream: &mut TcpStream, needle: &str, dur: Duration) -> String {
    let deadline = Instant::now() + dur;
    let mut buf = Vec::new();
    let mut tmp = vec![0u8; 1024];
    while Instant::now() < deadline {
        match timeout(Duration::from_millis(100), stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break, // EOF
            Ok(Ok(n)) => {
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&strip_iac(&buf)).to_string();
                if text.contains(needle) {
                    return text;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => continue, // timeout — keep waiting
        }
    }
    String::from_utf8_lossy(&strip_iac(&buf)).to_string()
}

/// Strip telnet IAC sequences from a stream of bytes (just enough to
/// make the test assertions readable). Mirrors the parser in
/// procserv::telnet but without the supervisor overhead.
fn strip_iac(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b != 0xFF {
            out.push(b);
            i += 1;
            continue;
        }
        // IAC ...
        if i + 1 >= input.len() {
            break;
        }
        let cmd = input[i + 1];
        match cmd {
            0xFF => {
                out.push(0xFF);
                i += 2;
            }
            0xFB..=0xFE => {
                // WILL/WONT/DO/DONT — 3-byte
                i += 3;
            }
            0xFA => {
                // SB ... SE
                i += 2;
                while i + 1 < input.len() && !(input[i] == 0xFF && input[i + 1] == 0xF0) {
                    i += 1;
                }
                i += 2;
            }
            _ => {
                i += 2;
            }
        }
    }
    out
}

#[tokio::test]
async fn cat_round_trip_via_tcp_console() {
    let port = pick_port().await;
    let cfg = cat_config(port);
    let server = ProcServ::new(cfg).expect("build");

    // Run server in a background task; we'll abort it at the end.
    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Connect to the supervisor's TCP console.
    let mut conn = {
        // Listener is set up async; retry briefly.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("could not connect: {e}"),
            }
        }
    };

    // Drain initial banner (PTY may not have output yet, but the
    // welcome banner from the supervisor is sent on connect).
    let initial = read_for(&mut conn, Duration::from_millis(500)).await;
    let cleaned = String::from_utf8_lossy(&strip_iac(&initial)).to_string();
    assert!(
        cleaned.contains("Welcome to procServ ("),
        "missing welcome banner; got: {cleaned:?}"
    );
    // The banner reports start times and connected-peer counts
    // (C clientFactory.cc:131-145 / 143-144). The child is up at connect
    // time, so its "started at" line is present too.
    assert!(
        cleaned.contains("procServ server started at:"),
        "banner missing server start time; got: {cleaned:?}"
    );
    assert!(
        cleaned.contains("Child \"cat\" started at:"),
        "banner missing child start time; got: {cleaned:?}"
    );
    assert!(
        cleaned.contains("user(s) and 0 logger(s) connected (plus you)"),
        "banner missing connected-peer counts; got: {cleaned:?}"
    );

    // Type a line — `cat` will echo it back. Through the party-line:
    // our typed bytes go to the supervisor → forwarded to PTY stdin
    // (writes via processClass equivalent) AND echoed to other
    // clients (none here besides us, but the echo to ourselves is
    // suppressed because we're the sender).
    conn.write_all(b"hello world\n").await.unwrap();

    // The PTY (cat) will echo "hello world" back; that arrives via
    // the SendToAll fanout (PTY is the sender, we're the recipient).
    // Allow up to 1s; 50ms is usually enough on macOS.
    let out = read_for(&mut conn, Duration::from_secs(2)).await;
    let cleaned_out = String::from_utf8_lossy(&strip_iac(&out)).to_string();
    assert!(
        cleaned_out.contains("hello world"),
        "expected echo of 'hello world', got: {cleaned_out:?} (raw {out:?})"
    );

    server_task.abort();
}

#[tokio::test]
async fn kill_keystroke_signals_child() {
    let port = pick_port().await;
    let cfg = cat_config(port);
    let server = ProcServ::new(cfg).expect("build");

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut conn = {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("could not connect: {e}"),
            }
        }
    };

    // Drain banner.
    let _ = read_for(&mut conn, Duration::from_millis(300)).await;

    // Send Ctrl-X (0x18) — kills the child.
    conn.write_all(&[0x18]).await.unwrap();

    // Within 2s we should see the "@@@ Child exited" banner from
    // the supervisor. With `RestartMode::Disabled` configured, no
    // respawn follows.
    let out = read_for(&mut conn, Duration::from_secs(3)).await;
    let cleaned = String::from_utf8_lossy(&strip_iac(&out)).to_string();
    assert!(
        cleaned.contains("Child exited"),
        "expected 'Child exited' banner, got: {cleaned:?}"
    );
    // C broadcasts a kill notice to all clients before signalling
    // (clientFactory.cc:236-239).
    assert!(
        cleaned.contains("Got a kill command"),
        "expected '@@@ Got a kill command' broadcast, got: {cleaned:?}"
    );

    server_task.abort();
}

#[tokio::test]
async fn server_messages_are_written_to_the_log() {
    // C `SendToAll` logs every message whose sender is NULL or the
    // child process (procServ.cc:725), so supervisor `@@@` annotations
    // land in the log alongside child output. The "@@@ Child started"
    // banner is emitted from the supervisor (sender == server) at spawn,
    // so it must appear in the configured log file.
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("procserv.log");

    let port = pick_port().await;
    let mut cfg = cat_config(port);
    cfg.logging.log_path = Some(log_path.clone());
    let server = ProcServ::new(cfg).expect("build");
    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Allow the supervisor to spawn the child and flush the start banner.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut contents = String::new();
    while Instant::now() < deadline {
        contents = std::fs::read_to_string(&log_path).unwrap_or_default();
        if contents.contains("@@@ Child started") {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(
        contents.contains("@@@ Child started"),
        "supervisor start banner must be logged; got: {contents:?}"
    );

    server_task.abort();
}

#[tokio::test]
async fn log_port_client_is_readonly_but_receives_output() {
    // C procServ's log port is created `acceptFactory(logPort, local,
    // /*readonly=*/true)` (procServ.cc:533): clients there see all
    // output but their input is discarded (procServ.h:100,
    // acceptFactory.cc:395). Verify a client on the log port (a) sees
    // child/party-line output and (b) cannot inject input — bytes it
    // sends never reach the child or the control client.
    let ctl_port = pick_port().await;
    let log_port = pick_port().await;
    let mut cfg = cat_config(ctl_port);
    cfg.listen.log_port = Some(log_port);
    cfg.listen.log_bind = Some(SocketAddr::from(([127, 0, 0, 1], log_port)));

    let server = ProcServ::new(cfg).expect("build");
    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Connect a control (read/write) client and a log (read-only) client.
    let connect = |port: u16| async move {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(50)).await,
                Err(e) => panic!("could not connect to {port}: {e}"),
            }
        }
    };
    let mut ctl = connect(ctl_port).await;
    let mut log = connect(log_port).await;

    // Drain both welcome banners.
    let _ = read_for(&mut ctl, Duration::from_millis(400)).await;
    let _ = read_for(&mut log, Duration::from_millis(400)).await;

    // (a) Control types; the log viewer must observe the echoed output.
    ctl.write_all(b"hello from ctl\n").await.unwrap();
    let log_seen = read_until(&mut log, "hello from ctl", Duration::from_secs(2)).await;
    assert!(
        log_seen.contains("hello from ctl"),
        "log-port client must receive output, got: {log_seen:?}"
    );

    // (b) The log client tries to inject; then control sends a marker.
    // The marker round-trips through the child, giving the injected
    // bytes ample time to appear too — they must not, because a
    // read-only client's input is dropped before it reaches the
    // supervisor (so it is never echoed to control or fed to the child).
    log.write_all(b"INJECTED_BY_LOG\n").await.unwrap();
    ctl.write_all(b"CTL_MARKER\n").await.unwrap();
    let ctl_seen = read_until(&mut ctl, "CTL_MARKER", Duration::from_secs(2)).await;
    assert!(
        ctl_seen.contains("CTL_MARKER"),
        "control client must see its own marker echo, got: {ctl_seen:?}"
    );
    assert!(
        !ctl_seen.contains("INJECTED_BY_LOG"),
        "read-only log client input must not reach the party line, got: {ctl_seen:?}"
    );

    server_task.abort();
}

#[tokio::test]
async fn timefmt_controls_banner_timestamp_format() {
    // C `--timefmt` sets `timeFormat` (procServ.cc:254,303-305), used to
    // render the banner start-time lines (clientFactory.cc:124-130). A
    // format with no `%` specifiers is emitted as a literal, so a custom
    // timefmt shows up verbatim in the banner; the default ("%c") would
    // render a real calendar time instead.
    let port = pick_port().await;
    let mut cfg = cat_config(port);
    cfg.logging.time_format = "TIMEFMT_MARKER".into();

    let server = ProcServ::new(cfg).expect("build");
    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut conn = {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(50)).await,
                Err(e) => panic!("could not connect: {e}"),
            }
        }
    };

    let initial = read_for(&mut conn, Duration::from_millis(500)).await;
    let cleaned = String::from_utf8_lossy(&strip_iac(&initial)).to_string();
    assert!(
        cleaned.contains("server started at: TIMEFMT_MARKER"),
        "banner must render the start time with the configured timefmt; got: {cleaned:?}"
    );

    server_task.abort();
}

#[tokio::test]
async fn manual_restart_preempts_active_holdoff() {
    // C procServ's main poll loop keeps running during the crash-loop
    // holdoff, so a manual restart keystroke (`restartOnce()` zeros
    // `_restartTime`, processFactory.cc:289-291) relaunches the child
    // immediately instead of waiting the holdoff out. The Rust
    // supervisor must likewise service input while the restart deadline
    // is pending — if the holdoff were a blocking `sleep`, the byte
    // would queue until the deadline and then pass through as a no-op
    // (the child would already have auto-restarted and be alive), so the
    // "@@@ Manual restart" banner would never appear.
    let port = pick_port().await;
    let mut cfg = cat_config(port);
    cfg.restart_mode = RestartMode::OnExit;
    cfg.holdoff = Duration::from_secs(3); // long enough to observe the wait

    let server = ProcServ::new(cfg).expect("build");
    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut conn = {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("could not connect: {e}"),
            }
        }
    };

    // Drain the initial banner + first "@@@ Child started".
    let _ = read_for(&mut conn, Duration::from_millis(500)).await;

    // Kill the child (Ctrl-X). Under OnExit this schedules an auto
    // restart 3s out; "@@@ Child exited" confirms the child died and the
    // holdoff deadline is now pending.
    conn.write_all(&[0x18]).await.unwrap();
    let exited = read_until(&mut conn, "Child exited", Duration::from_secs(3)).await;
    assert!(
        exited.contains("Child exited"),
        "child should exit on kill keystroke, got: {exited:?}"
    );

    // Well within the 3s holdoff, press the manual restart key
    // (Ctrl-R = 0x12). The non-blocking deadline lets this fire now.
    let t0 = Instant::now();
    conn.write_all(&[0x12]).await.unwrap();
    let restarted = read_until(&mut conn, "Manual restart", Duration::from_secs(2)).await;
    let elapsed = t0.elapsed();
    assert!(
        restarted.contains("Manual restart"),
        "manual restart key must fire during the active holdoff, got: {restarted:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "manual restart must preempt the 3s holdoff; took {elapsed:?}"
    );

    server_task.abort();
}

#[tokio::test]
async fn two_clients_share_same_party_line() {
    let port = pick_port().await;
    let cfg = cat_config(port);
    let server = ProcServ::new(cfg).expect("build");

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Connect client A.
    let mut a = {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("connect A: {e}"),
            }
        }
    };
    let _ = read_for(&mut a, Duration::from_millis(300)).await;

    // Connect client B.
    let mut b = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let _ = read_for(&mut b, Duration::from_millis(300)).await;

    // A types — both A and B see PTY output. Plus B sees A's bytes
    // forwarded (echo to other clients).
    a.write_all(b"shared input\n").await.unwrap();

    let a_out = read_for(&mut a, Duration::from_secs(2)).await;
    let b_out = read_for(&mut b, Duration::from_secs(2)).await;

    let a_clean = String::from_utf8_lossy(&strip_iac(&a_out)).to_string();
    let b_clean = String::from_utf8_lossy(&strip_iac(&b_out)).to_string();

    // A should see the PTY echo (from cat), but NOT its own typed
    // bytes echoed back through SendToAll (sender is excluded).
    assert!(
        a_clean.contains("shared input"),
        "A should see PTY echo: {a_clean:?}"
    );

    // B should see both the PTY echo AND the bytes forwarded from A.
    // In practice both contain "shared input" so we just check
    // presence.
    assert!(
        b_clean.contains("shared input"),
        "B should see A's input + PTY echo: {b_clean:?}"
    );

    server_task.abort();
}

#[tokio::test]
async fn child_exit_sigkills_orphaned_process_group() {
    // C ~processClass SIGKILLs the child's process group on death so a
    // grandchild the child backgrounded does not survive
    // (processFactory.cc:117). The shell backgrounds a long `sleep`
    // (same process group — no job control in a non-interactive shell),
    // records its PID to a file, then exits; the group SIGKILL must reap
    // the sleep.
    let port = pick_port().await;
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("gpid");
    let pf = pidfile.to_str().unwrap().to_string();

    let mut cfg = cat_config(port); // Disabled: child exits, server stays up
    cfg.child.program = PathBuf::from("/bin/sh");
    cfg.child.args = vec!["-c".into(), format!("sleep 30 & echo $! > '{pf}'; exit 0")];
    let server = ProcServ::new(cfg).expect("build");
    let server_task = tokio::spawn(async move { server.run().await });

    // Wait for the grandchild PID to be recorded.
    let gpid: i32 = {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Some(p) = s.split_whitespace().next().and_then(|x| x.parse().ok())
            {
                break p;
            }
            assert!(Instant::now() < deadline, "grandchild pid never recorded");
            sleep(Duration::from_millis(50)).await;
        }
    };

    // Give the supervisor time to reap the child and SIGKILL the group,
    // and init/launchd time to reap the killed grandchild.
    sleep(Duration::from_millis(700)).await;

    let still_alive = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -0 {gpid} 2>/dev/null"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        !still_alive,
        "grandchild {gpid} must be SIGKILLed with the child's process group"
    );

    server_task.abort();
}

#[tokio::test]
async fn norestart_keeps_server_alive_after_child_exit() {
    // C `norestart` (Rust RestartMode::Disabled): the child exits but
    // the SERVER stays up — only `oneshot` sets shutdownServer. The
    // operator reconnects and ^R relaunches (processFactory.cc:51,
    // procServ.cc:654-669).
    let port = pick_port().await;
    let mut cfg = cat_config(port); // Disabled == norestart
    cfg.child.program = PathBuf::from("/bin/sh");
    cfg.child.args = vec!["-c".into(), "exit 0".into()];
    let server = ProcServ::new(cfg).expect("build");
    let server_task = tokio::spawn(async move { server.run().await });

    // Connect AFTER the child has already exited; the server must still
    // be accepting connections and serving a banner.
    sleep(Duration::from_millis(400)).await;
    let mut conn = {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(50)).await,
                Err(e) => panic!("connect: {e}"),
            }
        }
    };
    let banner = read_for(&mut conn, Duration::from_millis(300)).await;
    assert!(
        !banner.is_empty(),
        "server should still serve a banner after the child exited under norestart"
    );
    assert!(
        !server_task.is_finished(),
        "norestart must NOT shut the server down on child exit"
    );

    server_task.abort();
}

#[tokio::test]
async fn child_exit_code_becomes_server_exit_code() {
    // C procServ returns the child's last exit code as its own process
    // exit status (childExitCode → main return, procServ.cc:798,701).
    // Under one-shot the supervisor runs the child once then exits, so
    // `run()` resolves to the child's code. `sh -c 'exit 7'` → 7.
    let port = pick_port().await;
    let mut cfg = cat_config(port);
    cfg.child.program = PathBuf::from("/bin/sh");
    cfg.child.args = vec!["-c".into(), "exit 7".into()];
    cfg.restart_mode = RestartMode::OneShot;
    let server = ProcServ::new(cfg).expect("build");

    let code = timeout(Duration::from_secs(5), server.run())
        .await
        .expect("one-shot supervisor should exit promptly")
        .expect("run ok");
    assert_eq!(code, 7, "server exit code should mirror the child's");
}
