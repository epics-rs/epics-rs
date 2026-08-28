//! `realtime-pva-ioc` must complete its whole init path on the host under
//! `EPICS_RS_BUILD_EXEC_BACKEND=thread` without a panic.
//!
//! The reasoning is `epics-ca-rs`'s `tests/realtime_ca_ioc_boots.rs`, applied
//! to the other IOC binary: under `exec_backend` the `runtime::task` seam
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
//! up and had panicked.

#[cfg(tokio_backend)]
#[test]
fn entry_point_refuses_a_build_with_no_runtime_to_run_on() {
    // The other arm of the same entry point — see the CA counterpart.
    use std::process::{Command, Stdio};

    let out = Command::new(env!("CARGO_BIN_EXE_realtime-pva-ioc"))
        .stdin(Stdio::null())
        .output()
        .expect("spawn realtime-pva-ioc");
    assert!(!out.status.success(), "expected a refusal exit status");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("EPICS_RS_BUILD_EXEC_BACKEND"),
        "the refusal must name the variable that fixes it; got: {stderr}"
    );
}

/// Everything below needs the binary to actually boot, which it only does
/// on the exec backend. Grouped under one gate rather than nine so the
/// helpers cannot drift out of step with the test that uses them.
#[cfg(exec_backend)]
mod exec_model {
    use std::io::Read as _;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// How long the IOC gets to reach the "resolver installed" line.
    const INIT_BUDGET: Duration = Duration::from_secs(30);

    /// How long the IOC then gets to report that its client is searching. The
    /// binary's STAGE-5 probe reports every 10 s, so this has to clear one full
    /// reporting period with margin.
    #[cfg(feature = "bringup-probes")]
    const SEARCH_BUDGET: Duration = Duration::from_secs(45);

    /// What every port this test hands the IOC is set to: `0` is the
    /// kernel's "pick one", and the process that binds is the one that picks.
    ///
    /// The helpers this replaces bound an ephemeral port, read the number,
    /// dropped the socket and returned the number — so between that drop and
    /// the child's own bind the number belonged to whoever asked next, and a
    /// second suite running ephemeral probes on the same box took it often
    /// enough to fail this test in 0.055 s under load. Nothing here needs the
    /// number: the IOC reports the ports it actually bound
    /// (`realtime-pva-ioc: serving N records on PVA TCP port P (UDP search on
    /// U)`), so a test that wants them reads them from the child rather than
    /// predicting them.
    const KERNEL_PICKS: &str = "0";

    /// A port with nothing listening on it, for the name server the default
    /// build is pointed at — the compiled-in SLIRP address is part of the
    /// probe rig, so a clean build must be told where to dial.
    ///
    /// This one keeps the probe-and-drop shape on purpose: what it wants is
    /// the ABSENCE of a listener, and absence is the one property no bind can
    /// reserve — a socket held open to keep the number would be the very
    /// listener the dial must not find. A steal here cannot fail the test the
    /// way a stolen server port does; it can only make the IOC's dial reach a
    /// stranger instead of being refused, and what is asserted is that the
    /// dial happens at all.
    #[cfg(not(feature = "bringup-probes"))]
    fn closed_port() -> u16 {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral TCP port");
        let port = tcp.local_addr().unwrap().port();
        drop(tcp);
        port
    }

    /// The record set the IOC reports, or `None` while that report is still
    /// incomplete.
    ///
    /// The report is one `realtime-pva-ioc: serving {n} records ...` header
    /// followed by exactly `n` lines of `realtime-pva-ioc: {name}`
    /// (`realtime-pva-ioc.rs:898` and `:928`). Reading the record set out of
    /// the raw console instead is wrong twice over. Nothing forces a record
    /// name onto a prefixed line: the `bringup-probes` rig prints
    /// `UDPSEARCH ... pv={pv}` (`:480`) and `STAGE5 seq=.. record {rec} ...`
    /// (`:564`) with no prefix at all, and both name the built-in demo PV
    /// whatever database actually booted, so a whole-console grep for
    /// `RTEMS:PVA:AO` reads the probe's search target as the record set. And
    /// the header precedes the names rather than following them, while the
    /// UDP-responder failure path prints a second line carrying the word
    /// `serving` (`:812`), so no substring of the console tells a caller that
    /// the list is complete either. Withholding the set until `n` names have
    /// arrived makes both properties hold by construction.
    fn record_report(console: &str) -> Option<Vec<&str>> {
        const PREFIX: &str = "realtime-pva-ioc: ";
        let mut own = console.lines().filter_map(|l| l.strip_prefix(PREFIX));
        let count: usize = loop {
            if let Some(rest) = own.next()?.strip_prefix("serving ") {
                break rest.split(' ').next()?.parse().ok()?;
            }
        };
        // A record name carries no whitespace and every other line the IOC
        // prefixes is a sentence, so the names need no positional bookkeeping.
        let names: Vec<&str> = own.filter(|l| !l.contains(' ')).collect();
        (names.len() >= count).then_some(names)
    }

    /// The three boundaries `record_report` exists to hold, one case each.
    /// They are console fixtures rather than a spawned IOC because the point
    /// is what the parse does with a console it is *handed*: the real binary
    /// cannot be made to emit a truncated report on demand.
    mod record_report_boundaries {
        use super::record_report;

        const HEADER: &str = "realtime-pva-ioc: serving 2 records on PVA TCP port 43757 \
                              (UDP search on 49266), GUID 01bb, RTEMS execution model, \
                              no tokio runtime";

        #[test]
        fn a_report_short_of_its_own_count_is_not_readable() {
            let truncated = format!("{HEADER}\nrealtime-pva-ioc: SITE:ONLY\n");
            assert_eq!(record_report(&truncated), None);
            let complete = format!("{truncated}realtime-pva-ioc: SITE:TWO\n");
            assert_eq!(
                record_report(&complete),
                Some(vec!["SITE:ONLY", "SITE:TWO"])
            );
        }

        #[test]
        fn the_udp_failure_notice_is_not_the_header() {
            // `realtime-pva-ioc.rs:812` carries the word `serving` and is
            // printed long before the record list; taking it for the header
            // would make the parse report an empty record set as complete.
            let notice = "realtime-pva-ioc: the server is still serving on TCP 43757; reach it \
                          by name server\n";
            assert_eq!(record_report(notice), None);
        }

        #[test]
        fn an_unprefixed_probe_line_is_not_a_record() {
            // The line that made the whole-console grep read the demo PV out
            // of a boot that never loaded the demo database.
            let with_probe = format!(
                "{HEADER}\nrealtime-pva-ioc: SITE:ONLY\nrealtime-pva-ioc: SITE:TWO\n\
                 UDPSEARCH broadcast_port=49266 pv=RTEMS:PVA:AO\n"
            );
            let names = record_report(&with_probe).expect("a complete report");
            assert!(
                !names.iter().any(|n| n.contains("RTEMS:PVA:AO")),
                "{names:?}"
            );
        }
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

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_realtime-pva-ioc"));
        cmd.arg(&db)
            .env("EPICS_PVA_SERVER_PORT", KERNEL_PICKS)
            .env("EPICS_PVAS_SERVER_PORT", KERNEL_PICKS)
            .env("EPICS_PVA_BROADCAST_PORT", KERNEL_PICKS)
            .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        // With the probe rig compiled in, no `EPICS_PVA_NAME_SERVERS` here on
        // purpose: the binary compiles its own default in and, with the
        // variable unset, uses it before it builds the client — the same path
        // the target takes when its boot line names no name server
        // (`realtime-pva-ioc.rs` `STAGE5_NAME_SERVER`). Whatever it points at,
        // the dial to it is the seam under test. The clean build compiles no
        // default in (the SLIRP address is rig topology), so it is pointed at
        // a closed local port, like the CA boot test always is.
        #[cfg(not(feature = "bringup-probes"))]
        cmd.env(
            "EPICS_PVA_NAME_SERVERS",
            format!("127.0.0.1:{}", closed_port()),
        );
        let child = cmd.spawn().expect("spawn realtime-pva-ioc");
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
                "realtime-pva-ioc panicked during init:\n{console}"
            );
            assert!(
                child.0.try_wait().expect("poll the child").is_none(),
                "realtime-pva-ioc exited during init:\n{console}"
            );
            assert!(
                Instant::now() < deadline,
                "realtime-pva-ioc did not finish init within {INIT_BUDGET:?}:\n{console}"
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
        //
        // Probe-rig builds only: the reporter that prints `searching=` IS the
        // stage-5 probe, so the clean build has no console line to wait on —
        // the NS dial logs at debug, below the console subscriber's INFO
        // floor. The seam is still crossed there (the client and its dial are
        // production code, driven by the same `pva://` link); the *evidence*
        // is only asserted in the `bringup-probes` run of this test, which is
        // why the exec-backend gate runs both configurations.
        #[cfg(feature = "bringup-probes")]
        {
            let deadline = Instant::now() + SEARCH_BUDGET;
            while !console.contains("searching=1") {
                assert!(
                    !console.contains("panic"),
                    "realtime-pva-ioc panicked after init:\n{console}"
                );
                assert!(
                    child.0.try_wait().expect("poll the child").is_none(),
                    "realtime-pva-ioc exited after init:\n{console}"
                );
                assert!(
                    Instant::now() < deadline,
                    "realtime-pva-ioc never started searching for its pva:// link \
                     within {SEARCH_BUDGET:?}:\n{console}"
                );
                std::thread::sleep(Duration::from_millis(50));
                console = read_log();
            }
        }

        // Clean builds — wait for the record listing instead, which `main`
        // prints after everything the probe rig would have announced, then
        // assert the rig's reporter is absent. (Record-level absence is pinned
        // by the bin's own parse tests: this process test loads its own `.db`,
        // so the built-in database is not in this listing either way.)
        #[cfg(not(feature = "bringup-probes"))]
        {
            let deadline = Instant::now() + INIT_BUDGET;
            while !console.contains("realtime-pva-ioc: LOCAL:PAI") {
                assert!(
                    !console.contains("panic"),
                    "realtime-pva-ioc panicked after init:\n{console}"
                );
                assert!(
                    child.0.try_wait().expect("poll the child").is_none(),
                    "realtime-pva-ioc exited after init:\n{console}"
                );
                assert!(
                    Instant::now() < deadline,
                    "realtime-pva-ioc never listed its records within \
                     {INIT_BUDGET:?}:\n{console}"
                );
                std::thread::sleep(Duration::from_millis(50));
                console = read_log();
            }
            // One settle read: `main` prints sequentially, so anything it was
            // ever going to print lands within this margin of the listing.
            std::thread::sleep(Duration::from_secs(1));
            console = read_log();
            assert!(
                !console.contains("STAGE5"),
                "the default build must not start the stage-5 measurement rig; \
                 it belongs behind `--features bringup-probes`:\n{console}"
            );
        }

        assert!(
            !console.contains("panic"),
            "realtime-pva-ioc panicked after init:\n{console}"
        );
        assert!(
            child.0.try_wait().expect("poll the child").is_none(),
            "realtime-pva-ioc exited after init:\n{console}"
        );
    }

    /// A4: the database named on the boot command line is the one that is
    /// served, and a boot that names none says which database it substituted.
    ///
    /// The CA binary's twin of this test carries the reasoning; both are here
    /// because both target IOCs fell back to their own `DEMO_DB` silently and
    /// a fix to one is not a fix to the other.
    #[test]
    fn a_named_database_displaces_the_built_in_demo_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("site.db");
        std::fs::write(&db, "record(ai, \"SITE:ONLY\") { field(VAL, \"1\") }\n")
            .expect("write the db");

        let run = |tag: &str, args: Vec<String>| -> (Vec<String>, String) {
            let log_path = dir.path().join(format!("console-{tag}.log"));
            let log = std::fs::File::create(&log_path).expect("create the console log");
            let log_err = log.try_clone().expect("clone the console log handle");
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_realtime-pva-ioc"));
            cmd.arg(format!("EPICS_PVA_SERVER_PORT={KERNEL_PICKS}"))
                .arg(format!("EPICS_PVAS_SERVER_PORT={KERNEL_PICKS}"))
                .arg(format!("EPICS_PVA_BROADCAST_PORT={KERNEL_PICKS}"))
                .arg("EPICS_PVA_AUTO_ADDR_LIST=NO");
            #[cfg(not(feature = "bringup-probes"))]
            cmd.arg(format!(
                "EPICS_PVA_NAME_SERVERS=127.0.0.1:{}",
                closed_port()
            ));
            for a in &args {
                cmd.arg(a);
            }
            let child = cmd
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err))
                .spawn()
                .expect("spawn realtime-pva-ioc");
            let mut child = Killed(child);

            let read_log = || {
                let mut s = String::new();
                std::fs::File::open(&log_path)
                    .expect("open the console log")
                    .read_to_string(&mut s)
                    .expect("read the console log");
                s
            };
            let deadline = Instant::now() + INIT_BUDGET;
            let mut console = read_log();
            while record_report(&console).is_none() {
                assert!(
                    !console.contains("panic"),
                    "realtime-pva-ioc panicked during init:\n{console}"
                );
                assert!(
                    child.0.try_wait().expect("poll the child").is_none(),
                    "realtime-pva-ioc exited during init:\n{console}"
                );
                assert!(
                    Instant::now() < deadline,
                    "realtime-pva-ioc did not report its record set within \
                     {INIT_BUDGET:?}:\n{console}"
                );
                std::thread::sleep(Duration::from_millis(50));
                console = read_log();
            }
            let names = record_report(&console)
                .expect("the loop above exits only on a complete report")
                .into_iter()
                .map(str::to_owned)
                .collect();
            (names, console)
        };

        let (named, named_log) = run("named", vec![db.to_string_lossy().into_owned()]);
        assert!(
            named.iter().any(|n| n == "SITE:ONLY"),
            "the named database's record is not being served:\n{named_log}"
        );
        assert!(
            !named.iter().any(|n| n == "RTEMS:PVA:AO"),
            "the built-in demo database was loaded as well as the named one:\n{named_log}"
        );
        // Not scoped to the record report: "BUILT-IN DEMO" is the substitution
        // notice, not a record name, and `load_database` (`:368`) is the only
        // thing in either binary that prints it.
        assert!(
            !named_log.contains("BUILT-IN DEMO"),
            "a boot that named a database must not report a substitution:\n{named_log}"
        );

        let (bare, bare_log) = run("bare", Vec::new());
        assert!(
            bare_log.contains("BUILT-IN DEMO"),
            "a boot that named no database must say which one it loaded \
             instead — otherwise it is indistinguishable from serving the \
             site's:\n{bare_log}"
        );
        assert!(
            bare.iter().any(|n| n == "RTEMS:PVA:AO"),
            "the fallback must actually be the demo database:\n{bare_log}"
        );
    }
}
