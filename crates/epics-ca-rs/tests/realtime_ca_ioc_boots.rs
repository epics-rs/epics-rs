//! `realtime-ca-ioc` must complete its whole init path on the host under
//! `EPICS_RS_BUILD_EXEC_BACKEND=thread` without a panic.
//!
//! # Why a process test and not a unit test
//!
//! The defect this pins was invisible to every in-process test in this crate.
//! Under `exec_backend` the `runtime::task` seam routes `spawn` to the
//! std-thread background executor, and a future that lands on a callback-pool
//! worker runs with **no tokio reactor entered** — even in a process that has
//! a tokio runtime elsewhere, because the runtime is not entered on *that*
//! thread. A `#[tokio::test]` therefore has a reactor available on the test's
//! own thread and never reproduces it.
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

#[cfg(tokio_backend)]
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
    // `runtime::task::refuse_backend`: the text on stderr, exit status 1.
    assert_eq!(out.status.code(), Some(1), "a refusal is a failed start");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "realtime-ca-ioc: built with the tokio task backend, which this entry point \
         does not start a runtime for.\n\
         Build it for `armv7-rtems-eabihf` or a VxWorks target, or on a host \
         rebuild with EPICS_RS_BUILD_EXEC_BACKEND=thread.\n"
    );
}

/// Everything below needs the binary to actually boot, which it only does
/// on the exec backend. Grouped under one gate rather than nine so the
/// helpers cannot drift out of step with the test that uses them.
#[cfg(exec_backend)]
mod exec_model {
    use crate::softioc_verdict::rs_ioc_port_was_taken;
    use std::io::Read as _;
    use std::net::TcpListener;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// How long the IOC gets to reach the "resolver installed" line.
    const INIT_BUDGET: Duration = crate::budget::FACT_BUDGET;

    /// How long the IOC then gets to attempt its first name-server dial. That
    /// happens on the first search tick, well under a second; the margin is for a
    /// loaded CI box.
    const DIAL_BUDGET: Duration = crate::budget::FACT_BUDGET;

    /// An IOC that came up on the port it was told to use, and the console it
    /// is still writing to.
    struct Booted {
        child: Killed,
        log_path: std::path::PathBuf,
        port: u16,
    }

    impl Booted {
        fn console(&self) -> String {
            let mut s = String::new();
            std::fs::File::open(&self.log_path)
                .expect("open the console log")
                .read_to_string(&mut s)
                .expect("read the console log");
            s
        }
    }

    /// Boot `realtime-ca-ioc` on a port it is NAMED, retrying on a fresh
    /// candidate when that number was taken before the child could bind it.
    ///
    /// The tests below cannot ask the kernel for the port: two of them assert
    /// that the number the boot line named is the number served, which no
    /// kernel pick can express. So the number is a candidate, and losing it is
    /// a retry — see the `named-port` crate. [`rs_ioc_port_was_taken`] is the only
    /// thing that retries; a panic, any other exit, and the budget expiring all
    /// fail here, with the console, because none of them gets better on a
    /// different port.
    ///
    /// `argv` receives the candidate and puts it wherever the test's subject
    /// requires — an `.env()` in one case, a `NAME=VALUE` boot argument in the
    /// others — which is what makes this one helper rather than two.
    fn boot_on_a_named_port(
        dir: &std::path::Path,
        argv: impl Fn(&mut Command, u16),
        ready: impl Fn(&str) -> bool,
    ) -> Booted {
        named_port::on_a_named_port(|port| {
            // A log per candidate: a previous attempt's steal must not be
            // readable as this attempt's console.
            let log_path = dir.join(format!("console-{port}.log"));
            let log = std::fs::File::create(&log_path).expect("create the console log");
            let log_err = log.try_clone().expect("clone the console log handle");
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_realtime-ca-ioc"));
            argv(&mut cmd, port);
            let child = cmd
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err))
                .spawn()
                .expect("spawn realtime-ca-ioc");
            let mut booted = Booted {
                child: Killed(child),
                log_path,
                port,
            };

            let deadline = Instant::now() + INIT_BUDGET;
            loop {
                let console = booted.console();
                if ready(&console) {
                    return Some(booted);
                }
                assert!(
                    !console.contains("panic"),
                    "realtime-ca-ioc panicked during init:\n{console}"
                );
                if booted.child.0.try_wait().expect("poll the child").is_some() {
                    if rs_ioc_port_was_taken(&console, port) {
                        return None;
                    }
                    panic!("realtime-ca-ioc exited during init:\n{console}");
                }
                assert!(
                    Instant::now() < deadline,
                    "realtime-ca-ioc did not finish init within {INIT_BUDGET:?}:\n{console}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        })
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

    /// What the IOC reports about the server it started, or `None` while that
    /// report is still incomplete.
    ///
    /// Every field is the IOC's own reading of its own state, taken from the
    /// console file the child writes and nothing else does — that file is this
    /// test's private temp path, handed to `Command::stdout`, so no other
    /// process on the box can put a line in it. That is what makes a report an
    /// assertion: a port number here is a bind that returned inside the child,
    /// where the same number reached from outside is only a socket somebody
    /// owns.
    #[derive(Debug, PartialEq, Eq)]
    struct BootReport<'a> {
        /// The CA port the server came up on. `realtime-ca-ioc` prints the
        /// header only after `BlockingCaServer::bind` and `bind_udp_search`
        /// have both returned on this number (`realtime-ca-ioc.rs:1206`,
        /// `:1216`, `:1313`), and every failure before that point is an
        /// `eprintln!` and `ExitCode::FAILURE`.
        port: u16,
        /// The record names, at least as many as the header claims.
        names: Vec<&'a str>,
    }

    /// Parse that report.
    ///
    /// The report is one `realtime-ca-ioc: serving {n} records ...` header
    /// followed by exactly `n` lines of `realtime-ca-ioc: {name}`
    /// (`realtime-ca-ioc.rs:1314` and `:1375`). Reading the record set out of
    /// the raw console instead is wrong twice over. Nothing forces a record
    /// name onto a prefixed line: the `bringup-probes` rig prints
    /// `UDPSEARCH sent bytes=.. pv={pv} ...` (`:735`), `C6 seq=.. link
    /// pv={pv} ...` (`:814`) and `C6 seq=.. record {rec} ...` (`:827`) with
    /// no prefix at all, and the first names the demo PV whatever database
    /// actually booted, so a whole-console grep for `RTEMS:AO` reads the
    /// probe's search target as the record set. And the header precedes the
    /// names rather than following them, so no substring of the console says
    /// the list is complete either — that half is what fails today, with the
    /// probe rig winning the race to the log and the names not yet written.
    /// Withholding the set until `n` names have arrived makes both properties
    /// hold by construction.
    fn boot_report(console: &str) -> Option<BootReport<'_>> {
        const PREFIX: &str = "realtime-ca-ioc: ";
        let mut own = console.lines().filter_map(|l| l.strip_prefix(PREFIX));
        let (count, port) = loop {
            if let Some(rest) = own.next()?.strip_prefix("serving ") {
                // Split on the literal rather than counting words: the header
                // is one `println!` and the count and the port are the only
                // two things in it that vary.
                let (count, rest) = rest.split_once(" records on CA port ")?;
                break (
                    count.parse::<usize>().ok()?,
                    rest.split(' ').next()?.parse::<u16>().ok()?,
                );
            }
        };
        // A record name carries no whitespace and every other line the IOC
        // prefixes is a sentence, so the names need no positional bookkeeping.
        let names: Vec<&str> = own.filter(|l| !l.contains(' ')).collect();
        (names.len() >= count).then_some(BootReport { port, names })
    }

    /// The boundaries `boot_report` exists to hold, one case each.
    /// Console fixtures rather than a spawned IOC because the point is what
    /// the parse does with a console it is *handed*: the real binary cannot
    /// be made to emit a truncated report on demand.
    mod boot_report_boundaries {
        use super::boot_report;

        const HEADER: &str = "realtime-ca-ioc: serving 2 records on CA port 38999 \
                              (TCP + UDP search), RTEMS execution model, no tokio runtime";

        #[test]
        fn a_report_short_of_its_own_count_is_not_readable() {
            let truncated = format!("{HEADER}\nrealtime-ca-ioc: SITE:ONLY\n");
            assert_eq!(boot_report(&truncated), None);
            let complete = format!("{truncated}realtime-ca-ioc: SITE:TWO\n");
            assert_eq!(
                boot_report(&complete).expect("a complete report").names,
                vec!["SITE:ONLY", "SITE:TWO"]
            );
        }

        /// The port rides in the same header as the count, so a console that
        /// does not yet carry a whole report carries no port either: a caller
        /// cannot be handed half of what the IOC said about itself.
        #[test]
        fn the_port_comes_from_the_header_or_not_at_all() {
            assert_eq!(boot_report(HEADER), None);
            let complete =
                format!("{HEADER}\nrealtime-ca-ioc: SITE:ONLY\nrealtime-ca-ioc: SITE:TWO\n");
            assert_eq!(
                boot_report(&complete).expect("a complete report").port,
                38999
            );
        }

        #[test]
        fn an_unprefixed_probe_line_is_not_a_record() {
            // The line that made the whole-console grep read the demo PV out
            // of a boot that never loaded the demo database.
            let with_probe = format!(
                "{HEADER}\nrealtime-ca-ioc: SITE:ONLY\nrealtime-ca-ioc: SITE:TWO\n\
                 UDPSEARCH sent bytes=48 pv=RTEMS:AO dest=192.168.2.255:38999\n"
            );
            let names = boot_report(&with_probe).expect("a complete report").names;
            assert!(!names.iter().any(|n| n.contains("RTEMS:AO")), "{names:?}");
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
        let db = dir.path().join("calink.db");
        // Both link spellings the resolver owns: the ` CA` modifier and the
        // `ca://` scheme. Each one makes the record a client of a server this
        // IOC has to search for, which is what drives the search engine and
        // the dial.
        std::fs::write(
            &db,
            "record(ai, \"LOCAL:AI\") { field(INP, \"UPSTREAM:AI CP\") }\n\
             record(ai, \"LOCAL:AI2\") { field(INP, \"ca://UPSTREAM:AI CP\") }\n",
        )
        .expect("write the db");

        // Phase 1 — reach the end of init. "calink resolver installed" is printed
        // after the database is loaded, the CA server is up and the resolver has
        // been handed its client, so seeing it means the whole init path ran.
        let mut booted = boot_on_a_named_port(
            dir.path(),
            |cmd, port| {
                cmd.arg(&db)
                    // The exec backend's search engine is name-servers-only — it
                    // binds no UDP socket — and it refuses an empty list at
                    // construction, because it could then reach nothing at all.
                    // Point it at a closed port: the dial is attempted and
                    // refused, which is the path under test.
                    .env(
                        "EPICS_CA_NAME_SERVERS",
                        format!("127.0.0.1:{}", closed_port()),
                    )
                    .env("EPICS_CA_SERVER_PORT", port.to_string())
                    .env("EPICS_CAS_SERVER_PORT", port.to_string())
                    .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
                    .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
                    .env("EPICS_CA_AUTO_ADDR_LIST", "NO");
            },
            |console| console.contains("calink resolver installed"),
        );
        let mut console = booted.console();

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
                booted.child.0.try_wait().expect("poll the child").is_none(),
                "realtime-ca-ioc exited after init:\n{console}"
            );
            assert!(
                Instant::now() < deadline,
                "realtime-ca-ioc never attempted the name-server dial within \
                 {DIAL_BUDGET:?}:\n{console}"
            );
            std::thread::sleep(Duration::from_millis(50));
            console = booted.console();
        }

        assert!(
            !console.contains("panic"),
            "realtime-ca-ioc panicked after init:\n{console}"
        );
        assert!(
            booted.child.0.try_wait().expect("poll the child").is_none(),
            "realtime-ca-ioc exited after init:\n{console}"
        );

        // Phase 3 — the measurement rig is a build-time choice: the
        // `bringup-probes` build starts the C6 probe threads and announces
        // them; the default build must start neither. Both announcements are
        // printed from `main` right after the record listing, so by the end of
        // phase 2 — which waited on a background thread's dial, well after
        // `main` finished printing — an absent line is a gated line, not a
        // race.
        #[cfg(feature = "bringup-probes")]
        {
            let deadline = Instant::now() + crate::budget::FACT_BUDGET;
            while !console.contains("C6 probe") {
                assert!(
                    Instant::now() < deadline,
                    "a bringup-probes build must announce its probe threads:\n{console}"
                );
                std::thread::sleep(Duration::from_millis(50));
                console = booted.console();
            }
        }
        #[cfg(not(feature = "bringup-probes"))]
        {
            // No settle window: phase 2's barrier already closed this claim.
            // It waited on a line a background thread prints after the dial,
            // which `main` reached only after printing both announcements, and
            // stdout and stderr are the same file with per-line flushing — so
            // a line that is absent now was never written. A sleep here would
            // have proved only that the announcement was slower than it.
            console = booted.console();
            assert!(
                !console.contains("C6 probe"),
                "the default build must not start the C6 measurement rig; \
                 it belongs behind `--features bringup-probes`:\n{console}"
            );
        }
    }

    /// A3: the boot command line configures the IOC, and a variable it sets is
    /// the one that takes effect.
    ///
    /// The target has no iocsh and no startup script, so argv is the only
    /// configuration surface an image has; before this the shim passed a fixed
    /// one-element argv and called `setenv` zero times, so `EPICS_CA_ADDR_LIST`
    /// and every other tuning variable a site set was ignored with no error.
    ///
    /// The assertion is the port the IOC reports it came up on: it is told that
    /// port ONLY through a `NAME=VALUE` boot argument and nothing in this
    /// process's environment names it, so a `serving ... on CA port {n}` line
    /// carrying that number can only mean the boot argument reached
    /// `cas_server_port`.
    ///
    /// Connecting to the number was the earlier form of this assertion and it
    /// proved nothing about the IOC. A candidate port's probe sockets are
    /// closed before it is handed out — the child could not otherwise bind
    /// them — so the number is unowned from then until the child's own bind,
    /// and a sibling test or another checkout on this box that takes it in the
    /// meantime answers the connect in milliseconds against a socket the IOC
    /// never opened. Holding the probe listener until `spawn` does not close
    /// that window, it only moves it. What removes it is refusing to accept
    /// evidence from anywhere but the process under test — and, for the window
    /// itself, [`boot_on_a_named_port`], which spends a fresh candidate when
    /// the child reports the one it was given was taken.
    #[test]
    fn a_boot_argument_sets_the_environment_the_ioc_reads() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("site.db");
        std::fs::write(&db, "record(ai, \"SITE:AI\") { field(VAL, \"1\") }\n")
            .expect("write the db");

        let booted = boot_on_a_named_port(
            dir.path(),
            |cmd, port| {
                // Every one of these is a boot argument, not an `.env()` — that
                // is the whole point of the test. The db path rides on the same
                // line, so the assignment rule is exercised against a real load
                // argument rather than against assignments alone.
                cmd.arg(format!("EPICS_CA_NAME_SERVERS=127.0.0.1:{}", closed_port()))
                    .arg(format!("EPICS_CA_SERVER_PORT={port}"))
                    .arg(format!("EPICS_CAS_SERVER_PORT={port}"))
                    .arg("EPICS_CAS_INTF_ADDR_LIST=127.0.0.1")
                    .arg("EPICS_CAS_BEACON_ADDR_LIST=127.0.0.1")
                    .arg("EPICS_CA_AUTO_ADDR_LIST=NO")
                    .arg(&db)
                    // Cleared in the child's inherited environment so the boot
                    // line is the only thing that can name the port.
                    // `env_remove` rather than trusting the harness: a developer
                    // with EPICS_CAS_SERVER_PORT exported would otherwise get a
                    // green run that proves nothing.
                    .env_remove("EPICS_CA_SERVER_PORT")
                    .env_remove("EPICS_CAS_SERVER_PORT")
                    .env_remove("EPICS_CAS_INTF_ADDR_LIST");
            },
            |console| boot_report(console).is_some(),
        );
        let port = booted.port;
        let console = booted.console();
        let served = boot_report(&console)
            .expect("the boot helper returns only on a complete report")
            .port;

        assert_eq!(
            served, port,
            "the boot line named CA port {port} and the IOC came up on \
             {served}:\n{console}"
        );
        assert!(
            console.contains("epicsEnvSet EPICS_CAS_SERVER_PORT"),
            "the boot line's assignments must be echoed — the console is this \
             target's only report:\n{console}"
        );
    }

    /// A4: the database named on the boot command line is the one that is
    /// served, and a boot that names none says which database it substituted.
    ///
    /// The observable is what a client finds when it searches: before the boot
    /// line existed the target always fell back to `DEMO_DB`, so a site got the
    /// demo PVs and nothing on the console distinguished that from a healthy
    /// IOC serving the site's own records. Both halves are asserted here —
    /// the named database displaces the demo one, and the fallback names
    /// itself — because a substitution that is silent is the defect and a
    /// substitution that reports itself is not.
    #[test]
    fn a_named_database_displaces_the_built_in_demo_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("site.db");
        std::fs::write(&db, "record(ai, \"SITE:ONLY\") { field(VAL, \"1\") }\n")
            .expect("write the db");

        let run = |args: Vec<String>| -> (Vec<String>, String) {
            let booted = boot_on_a_named_port(
                dir.path(),
                |cmd, port| {
                    cmd.arg(format!("EPICS_CA_NAME_SERVERS=127.0.0.1:{}", closed_port()))
                        .arg(format!("EPICS_CA_SERVER_PORT={port}"))
                        .arg(format!("EPICS_CAS_SERVER_PORT={port}"))
                        .arg("EPICS_CAS_INTF_ADDR_LIST=127.0.0.1")
                        .arg("EPICS_CAS_BEACON_ADDR_LIST=127.0.0.1")
                        .arg("EPICS_CA_AUTO_ADDR_LIST=NO");
                    for a in &args {
                        cmd.arg(a);
                    }
                },
                |console| boot_report(console).is_some(),
            );
            let console = booted.console();
            let names = boot_report(&console)
                .expect("the boot helper returns only on a complete report")
                .names
                .into_iter()
                .map(str::to_owned)
                .collect();
            (names, console)
        };

        let (named, named_log) = run(vec![db.to_string_lossy().into_owned()]);
        assert!(
            named.iter().any(|n| n == "SITE:ONLY"),
            "the named database's record is not being served:\n{named_log}"
        );
        assert!(
            !named.iter().any(|n| n == "RTEMS:AO"),
            "the built-in demo database was loaded as well as the named one:\n{named_log}"
        );
        // Not scoped to the record report: "BUILT-IN DEMO" is the substitution
        // notice, not a record name, and `load_database` (`:975`) is the only
        // thing in either binary that prints it.
        assert!(
            !named_log.contains("BUILT-IN DEMO"),
            "a boot that named a database must not report a substitution:\n{named_log}"
        );

        let (bare, bare_log) = run(Vec::new());
        assert!(
            bare_log.contains("BUILT-IN DEMO"),
            "a boot that named no database must say which one it loaded \
             instead — otherwise it is indistinguishable from serving the \
             site's:\n{bare_log}"
        );
        assert!(
            bare.iter().any(|n| n == "RTEMS:AO"),
            "the fallback must actually be the demo database:\n{bare_log}"
        );
    }

    /// `realtime-ca-ioc` serving one record, booted for a raw client.
    fn boot_one_record(dir: &std::path::Path) -> Booted {
        let db = dir.join("one.db");
        std::fs::write(&db, "record(ai, \"MIS:A\") {}\n").expect("write the db");
        boot_on_a_named_port(
            dir,
            |cmd, port| {
                cmd.arg(format!("EPICS_CA_NAME_SERVERS=127.0.0.1:{}", closed_port()))
                    .arg(format!("EPICS_CA_SERVER_PORT={port}"))
                    .arg(format!("EPICS_CAS_SERVER_PORT={port}"))
                    .arg("EPICS_CAS_INTF_ADDR_LIST=127.0.0.1")
                    .arg("EPICS_CAS_BEACON_ADDR_LIST=127.0.0.1")
                    .arg("EPICS_CA_AUTO_ADDR_LIST=NO")
                    .arg(&db);
            },
            |console| boot_report(console).is_some(),
        )
    }

    fn ca_hdr(cmmd: u16, size: u16, dt: u16, count: u16, p1: u32, p2: u32) -> Vec<u8> {
        let mut h = Vec::with_capacity(16);
        for v in [cmmd, size, dt, count] {
            h.extend_from_slice(&v.to_be_bytes());
        }
        h.extend_from_slice(&p1.to_be_bytes());
        h.extend_from_slice(&p2.to_be_bytes());
        h
    }

    /// Send `handshake` on a fresh circuit and read `answered` bytes of reply,
    /// then send `frames` and read to the server's close; return the client's
    /// address and every console line naming it, once the disconnect line has
    /// been written.
    fn console_for_a_closed_circuit(
        booted: &Booted,
        handshake: &[u8],
        answered: usize,
        frames: &[u8],
    ) -> (String, String) {
        use std::io::Write as _;
        let mut c = std::net::TcpStream::connect(("127.0.0.1", booted.port)).expect("connect");
        c.set_read_timeout(Some(crate::budget::FACT_BUDGET))
            .unwrap();
        let peer = c.local_addr().unwrap().to_string();
        c.write_all(handshake).unwrap();
        let mut answer = vec![0u8; answered];
        c.read_exact(&mut answer).expect("the handshake's replies");
        c.write_all(frames).unwrap();
        let mut rest = Vec::new();
        c.read_to_end(&mut rest)
            .expect("the server closes the circuit");

        let disconnect = format!("CAS: forcing disconnect from {peer}\n");
        let deadline = Instant::now() + crate::budget::FACT_BUDGET;
        let console = loop {
            let console = booted.console();
            if console.contains(&disconnect) {
                break console;
            }
            assert!(Instant::now() < deadline, "no disconnect line:\n{console}");
            std::thread::sleep(Duration::from_millis(20));
        };
        let about_the_client = console
            .split_inclusive('\n')
            .filter(|l| l.contains(&peer))
            .collect();
        (peer, about_the_client)
    }

    /// The blocking driver's console for a misaligned request is softIoc's:
    /// `log_header`'s three lines and the disconnect (`camessage.c:2452-2463`,
    /// `camsgtask.c`), which softIoc 8f5015b66 printed on stderr for this
    /// frame after an ECHO round trip, and nothing else naming the client.
    #[test]
    fn a_misaligned_request_prints_softiocs_lines_and_no_others() {
        let dir = tempfile::tempdir().expect("temp dir");
        let booted = boot_one_record(dir.path());
        // VERSION, then an ECHO whose reply proves both were read: softIoc's
        // greeting VERSION and the ECHO reply are the 32 bytes before it.
        let handshake = [ca_hdr(0, 0, 0, 13, 0, 0), ca_hdr(23, 0, 0, 0, 0, 0)].concat();
        let mut frame = ca_hdr(23, 4, 7, 3, 0x1234, 0x55);
        frame.extend_from_slice(b"abcd");
        let (peer, console) = console_for_a_closed_circuit(&booted, &handshake, 32, &frame);
        assert_eq!(
            console,
            format!(
                "CAS: request from {peer} => CAS: Missaligned protocol rejected\n\
                 CAS: Request from {peer} => cmmd=23 cid=0x1234 type=7 count=3 postsize=4\n\
                 CAS: Request from {peer} =>   available=0x55 \tN=0 paddr=(nil)\n\
                 CAS: forcing disconnect from {peer}\n"
            )
        );
    }

    /// The receive gate's other arm, a refusal that keeps the circuit: a
    /// message before any VERSION is from a client too old to serve
    /// (`camessage.c:2491-2501`), and softIoc 8f5015b66 printed `log_header`'s
    /// three lines on stderr for this ECHO, then nothing once the client
    /// closed.
    #[test]
    fn a_too_old_client_prints_softiocs_lines_and_no_others() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let booted = boot_one_record(dir.path());
        let mut c = std::net::TcpStream::connect(("127.0.0.1", booted.port)).expect("connect");
        c.set_read_timeout(Some(crate::budget::FACT_BUDGET))
            .unwrap();
        let peer = c.local_addr().unwrap().to_string();
        c.write_all(&ca_hdr(23, 0, 0, 0, 0x1234, 0x55)).unwrap();
        // The greeting VERSION, then the ECA_DEFUNCT frame: its header, the
        // echoed request and "CAS: Client version 0 too old" padded to 32.
        let mut answer = [0u8; 80];
        c.read_exact(&mut answer).expect("VERSION and the refusal");
        drop(c);

        let last = format!("CAS: Request from {peer} =>   available=0x55 \tN=0 paddr=(nil)\n");
        let deadline = Instant::now() + crate::budget::FACT_BUDGET;
        let console = loop {
            let console = booted.console();
            if console.contains(&last) {
                break console;
            }
            assert!(Instant::now() < deadline, "no log_header:\n{console}");
            std::thread::sleep(Duration::from_millis(20));
        };
        let about_the_client: String = console
            .split_inclusive('\n')
            .filter(|l| l.contains(&peer))
            .collect();
        assert_eq!(
            about_the_client,
            format!(
                "CAS: request from {peer} => CAS: Client version too old\n\
                 CAS: Request from {peer} => cmmd=23 cid=0x1234 type=0 count=0 postsize=0\n\
                 {last}"
            )
        );
    }

    /// `tcp_version_action` refuses a minor version below 4 and a priority
    /// above 99 with `RSRV_ERROR` and no line (`camessage.c:368-375`, the
    /// first under `DLOG(2)` only): softIoc 8f5015b66 printed the disconnect
    /// alone for each.
    #[test]
    fn a_refused_version_prints_only_the_disconnect() {
        let dir = tempfile::tempdir().expect("temp dir");
        let booted = boot_one_record(dir.path());
        for (minor, priority) in [(3, 0), (13, 100)] {
            let (peer, console) =
                console_for_a_closed_circuit(&booted, &[], 0, &ca_hdr(0, 0, priority, minor, 0, 0));
            assert_eq!(
                console,
                format!("CAS: forcing disconnect from {peer}\n"),
                "minor {minor}, priority {priority}"
            );
        }
    }

    /// `build_ioc`'s link passes open the external links at init, before
    /// anything processes: `setup_cp_links` warms the CP link of a Passive
    /// holder, whose monitor is then the only thing that processes it, and
    /// `setup_external_link_opens` opens a link no record ever reads.
    ///
    /// The upstream is an in-process database behind [`BlockingCaServer`], so
    /// both facts are read off it: DOWN:CP's OUT lands the CP value in
    /// UPSTREAM:ECHO, and the open of DOWN:IN's link is a monitor on
    /// UPSTREAM:IN.
    #[test]
    fn build_ioc_opens_external_links_before_anything_processes() {
        use epics_base_rs::runtime::task::{background_init, block_on_sync};
        use epics_base_rs::server::ioc_builder::IocBuilder;
        use epics_base_rs::types::EpicsValue;
        use epics_ca_rs::server::blocking::BlockingCaServer;
        use std::sync::Arc;

        background_init();
        let (upstream, _) = block_on_sync(
            IocBuilder::new()
                .db_string(
                    "record(ao, \"UPSTREAM:CP\") { field(VAL, \"42.5\") }\n\
                     record(ao, \"UPSTREAM:IN\") { field(VAL, \"7\") }\n\
                     record(ao, \"UPSTREAM:ECHO\") {}\n",
                    &std::collections::HashMap::new(),
                )
                .expect("load the upstream db")
                .build(),
        )
        .expect("no async runtime entered on this test thread")
        .expect("build the upstream");
        let server = Arc::new(
            BlockingCaServer::bind(
                "127.0.0.1:0",
                Arc::clone(&upstream),
                epics_base_rs::server::access_security::new_acf_cell(None),
            )
            .expect("bind the upstream CA server"),
        );
        let upstream_port = server.local_addr().expect("local_addr").port();
        let srv = Arc::clone(&server);
        let accept = std::thread::spawn(move || srv.serve());

        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("down.db");
        std::fs::write(
            &db,
            "record(calcout, \"DOWN:CP\") { field(INPA, \"UPSTREAM:CP CP\") \
             field(CALC, \"A\") field(OUT, \"UPSTREAM:ECHO\") }\n\
             record(ai, \"DOWN:IN\") { field(INP, \"UPSTREAM:IN\") }\n",
        )
        .expect("write the db");
        let booted = boot_on_a_named_port(
            dir.path(),
            |cmd, port| {
                cmd.arg(&db)
                    .env(
                        "EPICS_CA_NAME_SERVERS",
                        format!("127.0.0.1:{upstream_port}"),
                    )
                    .env("EPICS_CA_SERVER_PORT", port.to_string())
                    .env("EPICS_CAS_SERVER_PORT", port.to_string())
                    .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
                    .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
                    .env("EPICS_CA_AUTO_ADDR_LIST", "NO");
            },
            |console| boot_report(console).is_some(),
        );

        let read = |name: &str| {
            let rec = upstream.get_record(name).expect("upstream record");
            let rec = rec.read();
            (
                rec.record.get_field("VAL"),
                !rec.field_subscribers().is_empty(),
            )
        };
        let deadline = Instant::now() + crate::budget::FACT_BUDGET;
        // The first monitor event can process DOWN:CP before its OUT link has
        // connected, which C drops too; each new CP value is another event.
        let mut sent = 42.5;
        loop {
            let (echo, _) = read("UPSTREAM:ECHO");
            let (_, in_opened) = read("UPSTREAM:IN");
            if echo == Some(EpicsValue::Double(sent)) && in_opened {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "UPSTREAM:ECHO is {echo:?} (CP value {sent}), UPSTREAM:IN \
                 monitored: {in_opened}:\n{}",
                booted.console()
            );
            std::thread::sleep(Duration::from_millis(200));
            // Only while nothing has crossed: once ECHO moves, the chain is
            // live and the last value sent must arrive.
            if echo.is_none() || echo == Some(EpicsValue::Double(0.0)) {
                sent += 1.0;
                block_on_sync(upstream.put_record_field_from_ca(
                    "UPSTREAM:CP",
                    "VAL",
                    EpicsValue::Double(sent),
                ))
                .expect("no async runtime entered on this test thread")
                .expect("put UPSTREAM:CP");
            }
        }

        drop(booted);
        server.shutdown();
        accept.join().expect("the accept thread exits on shutdown");
    }
}

#[path = "common/softioc_verdict.rs"]
mod softioc_verdict;

#[path = "common/budget.rs"]
mod budget;
