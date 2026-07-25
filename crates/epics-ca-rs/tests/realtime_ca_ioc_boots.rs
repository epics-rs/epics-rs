//! `realtime-ca-ioc` must complete its whole init path on the host under
//! `--features rtems-exec-model` without a panic.
//!
//! # Why a process test and not a unit test
//!
//! The defect this pins (`doc/calink-rtems-design.md` §10.10 item 2) was
//! invisible to every in-process test in this crate. Under
//! `rtems-exec-model` the `runtime::task` seam routes `spawn` to the
//! std-thread background executor, and a future that lands on a
//! callback-pool worker runs with **no tokio reactor entered** — even in a
//! process that has a tokio runtime elsewhere, because the runtime is not
//! entered on *that* thread. A `#[tokio::test]` therefore has a reactor
//! available on the test's own thread and never reproduces it.
//!
//! Only the real binary does: `realtime-ca-ioc` starts no tokio runtime at all,
//! so the client tasks it spawns are the exact configuration the target has.
//! Booting it as a child process is the only way to assert on the whole init
//! path — database load, calink resolver install, CA client start, first
//! search and first dial — end to end.
//!
//! # What "without a panic" means here
//!
//! A panic on a callback-pool worker kills that worker and leaves the IOC
//! running: it keeps serving, answering searches and looking healthy from the
//! outside, forever. So process liveness alone proves nothing and the console
//! is the evidence. The assertion is both — the process is still up *and* it
//! printed no panic. On the pre-fix tree the process was also still up, and
//! its console carried three of them.

#[cfg(not(feature = "rtems-exec-model"))]
#[test]
fn entry_point_refuses_a_build_with_no_runtime_to_run_on() {
    // The other arm of the same entry point. `realtime-ca-ioc` does not start a
    // tokio runtime — that is the whole point of it — so a build whose
    // `runtime::task` seam routes to tokio has nothing for its tasks to run
    // on. It says so and exits rather than booting into a hang.
    use std::process::{Command, Stdio};

    let out = Command::new(env!("CARGO_BIN_EXE_realtime-ca-ioc"))
        .stdin(Stdio::null())
        .output()
        .expect("spawn realtime-ca-ioc");
    assert!(!out.status.success(), "expected a refusal exit status");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("rtems-exec-model"),
        "the refusal must name the feature that fixes it; got: {stderr}"
    );
}

/// Everything below needs the binary to actually boot, which it only does
/// on the exec backend. Grouped under one gate rather than nine so the
/// helpers cannot drift out of step with the test that uses them.
#[cfg(feature = "rtems-exec-model")]
mod exec_model {
    use std::io::Read as _;
    use std::net::{TcpListener, UdpSocket};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// How long the IOC gets to reach the "resolver installed" line.
    const INIT_BUDGET: Duration = Duration::from_secs(30);

    /// How long the IOC then gets to attempt its first name-server dial. That
    /// happens on the first search tick, well under a second; the margin is for a
    /// loaded CI box.
    const DIAL_BUDGET: Duration = Duration::from_secs(30);

    /// A port free for both TCP and UDP: the CA server binds its TCP listener
    /// and its UDP search listener on the same number, so probing only one of
    /// them leaves the other free to collide.
    fn free_ca_port() -> u16 {
        for _ in 0..64 {
            let tcp = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral TCP port");
            let port = tcp.local_addr().unwrap().port();
            if UdpSocket::bind(("127.0.0.1", port)).is_ok() {
                return port;
            }
        }
        panic!("no port was free for both TCP and UDP after 64 attempts");
    }

    /// A port with nothing listening on it, for the name server the IOC is
    /// pointed at. The client is expected to fail to reach it — what is under
    /// test is that it fails by refusing a connection rather than by panicking.
    fn closed_port() -> u16 {
        let tcp = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral TCP port");
        let port = tcp.local_addr().unwrap().port();
        drop(tcp);
        port
    }

    struct Killed(Child);

    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn init_path_runs_without_a_panic() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("calink.db");
        // Both link spellings the resolver owns: the ` CA` modifier and the
        // `ca://` scheme (`doc/calink-rtems-design.md` §2.1). Each one makes the
        // record a client of a server this IOC has to search for, which is what
        // drives the search engine and the dial.
        std::fs::write(
            &db,
            "record(ai, \"LOCAL:AI\") { field(INP, \"UPSTREAM:AI CP\") }\n\
             record(ai, \"LOCAL:AI2\") { field(INP, \"ca://UPSTREAM:AI CP\") }\n",
        )
        .expect("write the db");

        let log_path = dir.path().join("console.log");
        let log = std::fs::File::create(&log_path).expect("create the console log");
        let log_err = log.try_clone().expect("clone the console log handle");

        let port = free_ca_port();
        let child = Command::new(env!("CARGO_BIN_EXE_realtime-ca-ioc"))
            .arg(&db)
            // The exec backend's search engine is name-servers-only — it binds no
            // UDP socket — and it refuses an empty list at construction, because
            // it could then reach nothing at all. Point it at a closed port: the
            // dial is attempted and refused, which is the path under test.
            .env(
                "EPICS_CA_NAME_SERVERS",
                format!("127.0.0.1:{}", closed_port()),
            )
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .env("EPICS_CAS_SERVER_PORT", port.to_string())
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .expect("spawn realtime-ca-ioc");
        let mut child = Killed(child);

        let read_log = || {
            let mut s = String::new();
            std::fs::File::open(&log_path)
                .expect("open the console log")
                .read_to_string(&mut s)
                .expect("read the console log");
            s
        };

        // Phase 1 — reach the end of init. "calink resolver installed" is printed
        // after the database is loaded, the CA server is up and the resolver has
        // been handed its client, so seeing it means the whole init path ran.
        let deadline = Instant::now() + INIT_BUDGET;
        let mut console = read_log();
        while !console.contains("calink resolver installed") {
            assert!(
                !console.contains("panic"),
                "realtime-ca-ioc panicked during init:\n{console}"
            );
            assert!(
                child.0.try_wait().expect("poll the child").is_none(),
                "realtime-ca-ioc exited during init:\n{console}"
            );
            assert!(
                Instant::now() < deadline,
                "realtime-ca-ioc did not finish init within {INIT_BUDGET:?}:\n{console}"
            );
            std::thread::sleep(Duration::from_millis(50));
            console = read_log();
        }

        // Phase 2 — wait for the first name-server dial, which is where the seam
        // this test exists for is crossed. The refusal the closed port produces is
        // the evidence that the client got that far; waiting for it rather than
        // for a timer means a future change that stops the client short of the
        // seam cannot pass this test by simply not panicking.
        let deadline = Instant::now() + DIAL_BUDGET;
        while !console.contains("TCP connect failed") {
            assert!(
                !console.contains("panic"),
                "realtime-ca-ioc panicked after init:\n{console}"
            );
            assert!(
                child.0.try_wait().expect("poll the child").is_none(),
                "realtime-ca-ioc exited after init:\n{console}"
            );
            assert!(
                Instant::now() < deadline,
                "realtime-ca-ioc never attempted the name-server dial within \
                 {DIAL_BUDGET:?}:\n{console}"
            );
            std::thread::sleep(Duration::from_millis(50));
            console = read_log();
        }

        assert!(
            !console.contains("panic"),
            "realtime-ca-ioc panicked after init:\n{console}"
        );
        assert!(
            child.0.try_wait().expect("poll the child").is_none(),
            "realtime-ca-ioc exited after init:\n{console}"
        );

        // Phase 3 — the measurement rig is a build-time choice
        // (`doc/calink-rtems-design.md` §11.7 item 3): the `bringup-probes`
        // build starts the C6 probe threads and announces them; the default
        // build must start neither. Both announcements are printed from
        // `main` right after the record listing, so by the end of phase 2 —
        // which waited on a background thread's dial, well after `main`
        // finished printing — an absent line is a gated line, not a race.
        #[cfg(feature = "bringup-probes")]
        {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !console.contains("C6 probe") {
                assert!(
                    Instant::now() < deadline,
                    "a bringup-probes build must announce its probe threads:\n{console}"
                );
                std::thread::sleep(Duration::from_millis(50));
                console = read_log();
            }
        }
        #[cfg(not(feature = "bringup-probes"))]
        {
            // One settle read: `main` prints sequentially and finished long
            // ago, so anything it was ever going to print is on disk by now.
            std::thread::sleep(Duration::from_secs(1));
            console = read_log();
            assert!(
                !console.contains("C6 probe"),
                "the default build must not start the C6 measurement rig; \
                 it belongs behind `--features bringup-probes`:\n{console}"
            );
        }
    }
}
