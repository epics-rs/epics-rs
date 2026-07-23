//! `rtems-pva-ioc` must complete its whole init path on the host under
//! `--features rtems-exec-model` without a panic.
//!
//! The reasoning is `epics-ca-rs`'s `tests/rtems_ca_ioc_boots.rs`, applied to
//! the other IOC binary: under `rtems-exec-model` the `runtime::task` seam
//! routes `spawn` to the std-thread background executor, a future that lands
//! on a callback-pool worker runs with **no tokio reactor entered**, and a
//! `#[tokio::test]` cannot reproduce that because it has a reactor on its own
//! thread. Only the real binary — which starts no tokio runtime at all — is
//! the configuration the target has, so only a process test sees the whole
//! init path: database load, QSRV2 install, pvalink resolver install, PVA
//! client start, first search and first dial.
//!
//! And as there, the assertion is liveness *and* a clean console: a panic on
//! a callback-pool worker kills that worker and leaves the IOC serving, so
//! liveness alone proves nothing. On the pre-fix tree this binary was still
//! up and had panicked (`doc/calink-rtems-design.md` §10.10 item 2).

#[cfg(not(feature = "rtems-exec-model"))]
#[test]
fn entry_point_refuses_a_build_with_no_runtime_to_run_on() {
    // The other arm of the same entry point — see the CA counterpart.
    use std::process::{Command, Stdio};

    let out = Command::new(env!("CARGO_BIN_EXE_rtems-pva-ioc"))
        .stdin(Stdio::null())
        .output()
        .expect("spawn rtems-pva-ioc");
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

    /// How long the IOC then gets to report that its client is searching. The
    /// binary's STAGE-5 probe reports every 10 s, so this has to clear one full
    /// reporting period with margin.
    const SEARCH_BUDGET: Duration = Duration::from_secs(45);

    fn free_tcp_port() -> u16 {
        let tcp = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral TCP port");
        tcp.local_addr().unwrap().port()
    }

    fn free_udp_port() -> u16 {
        let udp = UdpSocket::bind("127.0.0.1:0").expect("bind an ephemeral UDP port");
        udp.local_addr().unwrap().port()
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
        let db = dir.path().join("pvalink.db");
        // A `pva://` INP makes the record a client of a server this IOC has to
        // search for, which is what drives the search engine and the dial.
        std::fs::write(
            &db,
            "record(ai, \"LOCAL:PAI\") { field(INP, \"pva://UPSTREAM:AI CP\") }\n",
        )
        .expect("write the db");

        let log_path = dir.path().join("console.log");
        let log = std::fs::File::create(&log_path).expect("create the console log");
        let log_err = log.try_clone().expect("clone the console log handle");

        // No `EPICS_PVA_NAME_SERVERS` here on purpose: the binary compiles its own
        // in and overwrites the variable before it builds the client, because on
        // target nothing outside the image can configure it (`rtems-pva-ioc.rs`
        // `STAGE5_NAME_SERVER`). Whatever it points at, the dial to it is the seam
        // under test.
        let child = Command::new(env!("CARGO_BIN_EXE_rtems-pva-ioc"))
            .arg(&db)
            .env("EPICS_PVA_SERVER_PORT", free_tcp_port().to_string())
            .env("EPICS_PVAS_SERVER_PORT", free_tcp_port().to_string())
            .env("EPICS_PVA_BROADCAST_PORT", free_udp_port().to_string())
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .expect("spawn rtems-pva-ioc");
        let mut child = Killed(child);

        let read_log = || {
            let mut s = String::new();
            std::fs::File::open(&log_path)
                .expect("open the console log")
                .read_to_string(&mut s)
                .expect("read the console log");
            s
        };

        // Phase 1 — reach the end of init. "pvalink resolver installed" is printed
        // after the database is loaded, the PVA server is up and the resolver has
        // been handed its client.
        let deadline = Instant::now() + INIT_BUDGET;
        let mut console = read_log();
        while !console.contains("pvalink resolver installed") {
            assert!(
                !console.contains("panic"),
                "rtems-pva-ioc panicked during init:\n{console}"
            );
            assert!(
                child.0.try_wait().expect("poll the child").is_none(),
                "rtems-pva-ioc exited during init:\n{console}"
            );
            assert!(
                Instant::now() < deadline,
                "rtems-pva-ioc did not finish init within {INIT_BUDGET:?}:\n{console}"
            );
            std::thread::sleep(Duration::from_millis(50));
            console = read_log();
        }

        // Phase 2 — wait for the client to report that it is searching. The
        // search engine's first tick and the first name-server dial happen after
        // the line above, and the dial is where the seam this test exists for is
        // crossed; the binary's own STAGE-5 probe is the evidence that the client
        // got that far. Waiting for it rather than for a timer means a future
        // change that stops the client short of the seam cannot pass this test by
        // simply not panicking.
        let deadline = Instant::now() + SEARCH_BUDGET;
        while !console.contains("searching=1") {
            assert!(
                !console.contains("panic"),
                "rtems-pva-ioc panicked after init:\n{console}"
            );
            assert!(
                child.0.try_wait().expect("poll the child").is_none(),
                "rtems-pva-ioc exited after init:\n{console}"
            );
            assert!(
                Instant::now() < deadline,
                "rtems-pva-ioc never started searching for its pva:// link \
                 within {SEARCH_BUDGET:?}:\n{console}"
            );
            std::thread::sleep(Duration::from_millis(50));
            console = read_log();
        }

        assert!(
            !console.contains("panic"),
            "rtems-pva-ioc panicked after init:\n{console}"
        );
        assert!(
            child.0.try_wait().expect("poll the child").is_none(),
            "rtems-pva-ioc exited after init:\n{console}"
        );
    }
}
