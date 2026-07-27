//! `procserv-rs` — Rust port of the EPICS procServ daemon.
//!
//! Drop-in for `epics-modules/procServ` deployments: same flag set,
//! same `PROCSERV_INFO` env contract, same per-line PTY behaviour.
//! See [`epics_tools_rs::procserv`] for architectural notes.

// Reached on Windows alone: a unix target that is not a host platform never
// gets here, because `build.rs` refuses the build outright. So the message
// names the port that is missing rather than a platform class.
#[cfg(not(procserv_host_platform))]
fn main() {
    eprintln!("procserv-rs needs forkpty(3); no Windows (ConPTY) backend exists yet");
    std::process::exit(2);
}

#[cfg(procserv_host_platform)]
mod app {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::Duration;

    use clap::Parser;

    use epics_tools_rs::procserv::{
        ProcServ, ProcServConfig,
        config::{ChildConfig, KeyBindings, ListenConfig, LoggingConfig},
        console,
        daemon::{DaemonParent, fork_and_go, install_signal_handlers},
        endpoint::{Endpoint, UnixEndpoint, parse_endpoint},
        listener::bind_endpoints,
        restart::{RestartMode, RestartPolicy},
    };

    /// CLI flags chosen to match C procServ verbatim where possible.
    /// Operators porting wrapper scripts should not need to relearn
    /// the surface.
    #[derive(Parser, Debug)]
    #[command(
        name = "procserv-rs",
        about = "Pure-Rust port of the EPICS procServ process supervisor",
        version
    )]
    struct Args {
        /// Control endpoint(s) to listen on (`-P` / `--port` in C procServ,
        /// procServ.cc:251,264,386). Repeatable — each `-P` adds a control
        /// listener (C `ctlSpecs`). Each value is an `acceptFactory` spec:
        /// a bare TCP port (`4051`), an interface bind (`192.168.1.5:4051`,
        /// honored only with `--allow`), or a UNIX socket
        /// (`unix:/run/ioc.sock`, `unix:user:grp:0660:/run/ioc.sock`, or
        /// abstract `unix:@name`). Note `-P` (uppercase): C binds the
        /// control port to `-P` and the PID file to `-p` (lowercase).
        #[arg(short = 'P', long = "port", value_name = "ENDPOINT")]
        port: Vec<String>,

        /// Bind to all interfaces; default is localhost only.
        /// (C procServ `--allow`).
        #[arg(long)]
        allow: bool,

        /// Read-only viewer/log endpoint (`-l` / `--logport` in C). Clients
        /// here see all output but cannot inject input. Same `acceptFactory`
        /// spec grammar as `-P`; the log port binds all interfaces by
        /// default (C `logPortLocal` defaults false), `--restrict` flips it
        /// to localhost.
        #[arg(short = 'l', long = "logport", value_name = "ENDPOINT")]
        log_port: Option<String>,

        /// Restrict the log/viewer port to localhost (C `--restrict`).
        /// Unlike the control port, the log port binds all interfaces by
        /// default (C `logPortLocal` defaults false); this flag mirrors
        /// C flipping it to localhost-only.
        #[arg(long)]
        restrict: bool,

        /// UNIX-domain socket path (`--unixpath`).
        #[arg(long = "unixpath")]
        unix_path: Option<PathBuf>,

        /// Run in foreground (don't daemonize) — `-f` in C.
        #[arg(short = 'f', long)]
        foreground: bool,

        /// Debug mode (`-d` / `--debug`, C `inDebugMode`,
        /// procServ.cc:291-292). Keeps the child in the foreground (the
        /// daemonize gate is `!inFgMode && !inDebugMode`, procServ.cc:549)
        /// and raises diagnostic verbosity — C's `PRINTF` macro is
        /// `if (inDebugMode) printf` (procServ.h:30), mapped here to a
        /// debug-level tracing default. Also enabled by a present
        /// `PROCSERV_DEBUG` env var (procServ.cc:225).
        #[arg(short = 'd', long)]
        debug: bool,

        /// Suppress informational server output (`-q` / `--quiet`, C
        /// `quiet`, procServ.cc:390-391): the no-log-file warning emitted
        /// when daemonizing (procServ.cc:889-893). Does not affect the
        /// child or the party-line, only the server's own stderr notices.
        #[arg(short = 'q', long)]
        quiet: bool,

        /// Log file (`-L` / `--logfile`). The value `-` logs to stdout
        /// (C `openLogFile`, procServ.cc:920-922) rather than creating a
        /// file literally named `-`.
        #[arg(short = 'L', long)]
        logfile: Option<PathBuf>,

        /// Prefix log lines with a timestamp (`-S` / `--logstamp` in C).
        /// Off by default (C `stampLog` defaults false → log written
        /// verbatim). The flag alone uses the bracketed default stamp
        /// format; an optional strftime value (`--logstamp=<FMT>`) is used
        /// raw. Mirrors C's `optional_argument` getopt: the value must be
        /// attached with `=`, never space-separated.
        #[arg(
            short = 'S',
            long = "logstamp",
            value_name = "FMT",
            require_equals = true
        )]
        logstamp: Option<Option<String>>,

        /// strftime format for banner / start-time lines (`-F` /
        /// `--timefmt` in C, default `%c`). Sets C `timeFormat`
        /// (procServ.cc:80,254,303-305) and is also the base for the
        /// default log stamp, "[" + timefmt + "] " (procServ.cc:464-468).
        /// C exposes this long-only (`--timefmt`); `-F` is a Rust
        /// convenience, as 'F' is absent from C's getopt optstring
        /// (procServ.cc:264).
        #[arg(short = 'F', long = "timefmt", value_name = "FMT")]
        timefmt: Option<String>,

        /// PID file (`-p` / `--pidfile` in C procServ,
        /// procServ.cc:250,264). Note `-p` (lowercase): C binds the PID
        /// file to `-p` and the control port to `-P` (uppercase). When
        /// absent, the path falls back to the `PROCSERV_PID` env var
        /// (procServ.cc:224); an empty value means no pidfile.
        #[arg(short = 'p', long)]
        pidfile: Option<PathBuf>,

        /// Info file (`-I` / `--info-file`, C procServ.cc:241,264). Unlike
        /// the convenience shorts, `I` *is* in C's getopt optstring, so the
        /// short is C-faithful, not a Rust addition.
        #[arg(short = 'I', long)]
        info_file: Option<PathBuf>,

        /// Hold-off time between restarts in seconds (`--holdoff`).
        #[arg(long, default_value_t = 15)]
        holdoff: u64,

        /// Wait for manual start (don't launch the child until a
        /// connected user issues the restart key).
        #[arg(short = 'w', long)]
        wait: bool,

        /// Start with auto-restart disabled (C `-N` / `--noautorestart`,
        /// procServ.cc:369-371): the child is not relaunched on exit, but
        /// the supervisor stays up; toggle back at runtime with `^T`.
        /// `--noautorestart` is long-only in C; `-N` is a Rust convenience
        /// short like `-F`/`-S`.
        #[arg(short = 'N', long = "noautorestart", conflicts_with = "oneshot")]
        noautorestart: bool,

        /// Start in one-shot mode (C `-o` / `--oneshot`,
        /// procServ.cc:373-375): the supervisor exits when the child
        /// exits.
        #[arg(short = 'o', long = "oneshot")]
        oneshot: bool,

        /// chdir to this directory before exec'ing the child (`-c` /
        /// `--chdir`, C procServ.cc:234,264 — `c` is in C's optstring, so
        /// the short is C-faithful).
        #[arg(short = 'c', long)]
        chdir: Option<PathBuf>,

        /// Display name for the child in banners (`-n` / `--name`, C
        /// procServ.cc:247,264 — `n` is in C's optstring, so the short is
        /// C-faithful).
        #[arg(short = 'n', long)]
        name: Option<String>,

        /// Executable to launch instead of the positional command
        /// (`-e` / `--exec`, C `childExec`, procServ.cc:295-296,459-462).
        /// The child still presents argv[0] = the command, so a wrapper
        /// runs under the original command line. Default: exec the
        /// command itself.
        #[arg(short = 'e', long = "exec", value_name = "PATH")]
        exec: Option<PathBuf>,

        /// Restart-policy max attempts inside `--restart-window`.
        /// Unset means unlimited: C procServ has no restart-count cap —
        /// `processFactoryNeedsRestart()` (processFactory.cc:48-56) gates
        /// only on mode + holdoff, and the main loop `while(!shutdownServer)`
        /// (procServ.cc:599) never gives up on a count. The count limiter
        /// is a Rust-only opt-in (shared `RestartPolicy` with ca-gateway);
        /// leaving it unset preserves C's never-give-up behaviour.
        #[arg(long)]
        max_restarts: Option<u32>,

        /// Restart-policy sliding window in seconds.
        #[arg(long, default_value_t = 600)]
        restart_window: u64,

        /// Kill command key (`-k` / `--killcmd`, C `killChar`,
        /// procServ.cc:342-344). Accepts C caret notation via
        /// `getOptionChar` (procServ.cc:142-152): `^X` → 0x18, `^^` → `^`,
        /// a bare char verbatim, empty string → disabled. Default `^X`.
        #[arg(short = 'k', long = "killcmd", value_name = "CHAR")]
        killcmd: Option<String>,

        /// Toggle-autorestart command key (`--autorestartcmd`, C
        /// `toggleRestartChar`, procServ.cc:406-407). Same `getOptionChar`
        /// caret parsing. C exposes this long-only (val 'T' absent from the
        /// optstring); `-T` is a Rust convenience short like `-F`/`-S`.
        /// Default `^T`.
        #[arg(short = 'T', long = "autorestartcmd", value_name = "CHAR")]
        autorestartcmd: Option<String>,

        /// Logout command key (`-x` / `--logoutcmd`, C `logoutChar`,
        /// procServ.cc:402-403). Same `getOptionChar` caret parsing.
        /// Default disabled (C `logoutChar = 0x00`). Caret notation only
        /// covers `^A`..`^Z` (→ 0x01..0x1a); Ctrl-] (0x1d), the value the
        /// C docs suggest, is NOT `^]` (which parses to `^`, since `]` is
        /// above `Z`) — pass the raw 0x1d byte, e.g. `--logoutcmd=$'\x1d'`.
        #[arg(short = 'x', long = "logoutcmd", value_name = "CHAR")]
        logoutcmd: Option<String>,

        /// Core-dump size limit (bytes) applied to the child's
        /// `RLIMIT_CORE` soft limit before exec (C `--coresize`,
        /// long-only; `-C` is a Rust convenience short like `-F`/`-S`).
        /// Honored only for a non-negative value (C `l >= 0` gate,
        /// procServ.cc:279-285); omitted ⇒ inherit the existing limit.
        #[arg(short = 'C', long = "coresize", value_name = "BYTES")]
        coresize: Option<i64>,

        /// Signal sent to the child on the `^X` kill keystroke and on
        /// shutdown teardown (C `--killsig`, default SIGKILL = 9). C
        /// exposes this long-only; `-K` is a Rust convenience short like
        /// `-F`/`-S`, as 'K' is absent from C's getopt optstring
        /// (procServ.cc:264). A value ≥ 32 is rejected with a warning and
        /// the SIGKILL default kept; negative values are abs'd
        /// (procServ.cc:346-355).
        #[arg(short = 'K', long = "killsig", value_name = "SIGNUM")]
        killsig: Option<i32>,

        /// Characters to strip from the child's stdin (`-i` / `--ignore`).
        /// `^`-escaping mirrors C (procServ.cc:322-336): `^A`..`^Z` → the
        /// control byte (letter − 64), `^^` → a literal `^`, any other byte
        /// verbatim. The always-active command keys (kill / toggle-restart /
        /// logout) are auto-added on top by the supervisor, so this is only
        /// for EXTRA bytes the child should never see.
        #[arg(short = 'i', long = "ignore", value_name = "CHARS")]
        ignore: Option<String>,

        /// Program to launch + its arguments. Everything after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    }

    /// Parse a `--ignore` value into the bytes to strip, applying C's
    /// `^`-escaping (`procServ.cc:322-336`): `^A`..`^Z` → control byte,
    /// `^^` → literal `^`, anything else verbatim. Lowercase after `^`
    /// is NOT a control escape (C gates on `>= 'A' && <= 'Z'`), so `^a`
    /// yields the two bytes `^`, `a`.
    fn parse_ignore_chars(s: &str) -> Vec<u8> {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'^' && i + 1 < bytes.len() {
                let next = bytes[i + 1];
                if next == b'^' {
                    out.push(b'^');
                    i += 2;
                    continue;
                }
                if next.is_ascii_uppercase() {
                    out.push(next - 64);
                    i += 2;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        out
    }

    /// Resolve the PID-file path with C's precedence: `-p`/`--pidfile`
    /// overrides the `PROCSERV_PID` env var, which provides the default
    /// when the flag is absent (`procServ.cc:224,382`). An empty value
    /// (flag or env) means "no pidfile", matching C's
    /// `!pidFile || strlen(pidFile)==0` guard (`procServ.cc:125`).
    fn resolve_pidfile(flag: Option<PathBuf>, env: Option<std::ffi::OsString>) -> Option<PathBuf> {
        flag.or_else(|| env.map(PathBuf::from))
            .filter(|p| !p.as_os_str().is_empty())
    }

    /// Parse a single command-key character with C's `getOptionChar`
    /// semantics (`procServ.cc:142-152`): empty → `0` (disabled), `^^` →
    /// `^`, `^A`..`^Z` → the control byte (letter − 64), anything else →
    /// the first byte verbatim. Only the first one or two bytes matter,
    /// exactly like C reading `buf[0]`/`buf[1]`.
    fn get_option_char(buf: &str) -> u8 {
        match buf.as_bytes() {
            [] => 0,
            [b'^', b'^', ..] => b'^',
            [b'^', c, ..] if c.is_ascii_uppercase() => c - 64,
            [first, ..] => *first,
        }
    }

    fn build_config(args: Args) -> Result<ProcServConfig, String> {
        if args.cmd.is_empty() {
            return Err("missing child command".into());
        }
        let mut iter = args.cmd.into_iter();
        let program = PathBuf::from(iter.next().unwrap());
        let argv: Vec<String> = iter.collect();
        // C defaults `childName` to the WHOLE command string (argv[optind]),
        // not its basename: with no `-n`, `childName == command`, so the
        // banner shows the bare `@@@ Child started as: <cmd>` /
        // `@@@ Child "<cmd>" PID: N` form (procServ.cc:455-457,579-586).
        // Defaulting to `file_name()` would make `name != command` for any
        // path with a directory component (the normal IOC case), flipping
        // every name-bearing line to the wrong "named" form. Keep the full
        // program string so the identity matches C.
        let display_name = args.name.unwrap_or_else(|| program.display().to_string());

        // Control endpoints: each `-P <spec>` is parsed via `acceptFactory`
        // (C `ctlSpecs`, procServ.cc:386,515-518), plus the `--unixpath`
        // convenience — a plain filesystem UNIX control socket, equivalent
        // to `-P unix:<path>`. C `ctlPortLocal` is localhost unless
        // `--allow`; that decision is baked into each TCP endpoint here.
        let ctl_local = !args.allow;
        let mut control = Vec::new();
        for spec in &args.port {
            control.push(
                parse_endpoint(spec, ctl_local)
                    .map_err(|e| format!("control endpoint '{spec}': {e}"))?,
            );
        }
        if let Some(path) = args.unix_path {
            control.push(Endpoint::Unix(UnixEndpoint::filesystem(path)));
        }
        // Log endpoint: `logPortLocal` binds all interfaces unless
        // `--restrict` flips it to localhost (acceptFactory.cc:116,
        // procServ.cc:377), the inverse of the control port's default.
        let log = match &args.log_port {
            Some(spec) => Some(
                parse_endpoint(spec, args.restrict)
                    .map_err(|e| format!("log endpoint '{spec}': {e}"))?,
            ),
            None => None,
        };
        let listen = ListenConfig { control, log };

        // C `--killsig` (procServ.cc:346-355): `i = abs(atoi(optarg))`;
        // accept only `i < 32`, else warn to stderr and keep the SIGKILL
        // default. Validating here keeps the rule at the single config
        // build, so the supervisor always receives a vetted signal.
        let kill_signal = match args.killsig {
            Some(n) => {
                let i = n.abs();
                if i < 32 {
                    i
                } else {
                    tracing::warn!(
                        "procserv-rs: invalid kill signal {i} (>31) - using default ({})",
                        libc::SIGKILL
                    );
                    libc::SIGKILL
                }
            }
            None => libc::SIGKILL,
        };

        // C `--coresize` (procServ.cc:279-285): `l = atol(optarg)`; only
        // `l >= 0` is honored (`setCoreSize = true`), a negative value
        // leaves the flag inert. Map non-negative → Some(limit),
        // negative/absent → None (inherit the existing RLIMIT_CORE).
        let core_size = args.coresize.filter(|&l| l >= 0).map(|l| l as u64);

        let child = ChildConfig {
            name: display_name,
            program,
            args: argv,
            cwd: args.chdir,
            kill_signal,
            core_size,
            child_exec: args.exec,
            // Explicit `--ignore` bytes only; the supervisor auto-adds the
            // command keys (kill/toggle/logout) on top at spawn time.
            ignore_chars: args
                .ignore
                .as_deref()
                .map(parse_ignore_chars)
                .unwrap_or_default(),
        };

        // C `timeFormat` defaults to "%c" (procServ.cc:80), overridable by
        // -F/--timefmt (procServ.cc:254,303-305). Used for banner /
        // start-time rendering and as the base for the default log stamp.
        let time_format = args.timefmt.unwrap_or_else(|| "%c".into());

        // C `--logstamp [<FMT>]` (procServ.cc:307-311): the flag sets
        // stampLog=true; an attached value overrides stampFormat (raw),
        // the bare flag keeps the default. Absent → unstamped.
        let stamp_log = args.logstamp.is_some();
        let stamp_format = match args.logstamp {
            Some(Some(fmt)) => fmt,
            // C's default stampFormat = "[" + timeFormat + "] ", built from
            // the (possibly --timefmt-overridden) timeFormat so the flag
            // propagates to the log stamp exactly as in C (procServ.cc:464-468).
            _ => format!("[{time_format}] "),
        };

        let logging = LoggingConfig {
            log_path: args.logfile,
            pid_path: resolve_pidfile(args.pidfile, std::env::var_os("PROCSERV_PID")),
            info_path: args.info_file,
            stamp_log,
            time_format,
            stamp_format,
        };

        let nz = |c: u8| if c == 0 { None } else { Some(c) };

        // Command-key characters, parsed with C's `getOptionChar` caret
        // notation. Absent → the C default byte (kill ^X, toggle ^T,
        // logout disabled, procServ.cc:66-70); an explicit empty value or
        // a value that parses to 0 disables the key (nz → None).
        let kill_char = args.killcmd.as_deref().map(get_option_char).unwrap_or(0x18);
        let toggle_char = args
            .autorestartcmd
            .as_deref()
            .map(get_option_char)
            .unwrap_or(0x14);
        let logout_char = args
            .logoutcmd
            .as_deref()
            .map(get_option_char)
            .unwrap_or(0x00);

        // Startup restart mode (C `restartMode = restart` default,
        // overridden by `-N` → norestart / `-o` → oneshot,
        // procServ.cc:58,369-375). The two flags conflict at the clap
        // layer, so at most one is set here.
        let restart_mode = if args.oneshot {
            RestartMode::OneShot
        } else if args.noautorestart {
            RestartMode::Disabled
        } else {
            RestartMode::OnExit
        };

        Ok(ProcServConfig {
            // Debug mode keeps the child in the foreground — C's daemonize
            // gate is `!inFgMode && !inDebugMode` (procServ.cc:549). The
            // env-driven half of `inDebugMode` (PROCSERV_DEBUG) is OR'd in
            // by `entry`, which also reads the env for the tracing default.
            foreground: args.foreground || args.debug,
            listen,
            keys: KeyBindings {
                kill: nz(kill_char),
                toggle_restart: nz(toggle_char),
                restart: Some(0x12), // Ctrl-R when child dead
                // C `quitChar` = ^Q (0x11), hardwired and never exposed
                // on the CLI (procServ.cc:69; absent from long_options).
                // Fires only while the child is shut down
                // (clientFactory.cc:214-217); the menu scan gates it.
                quit: Some(0x11),
                logout: nz(logout_char),
            },
            child,
            logging,
            restart: RestartPolicy {
                // Unlimited unless the operator opts in: matches C procServ
                // which never caps restarts on a count (processFactory.cc:48-56,
                // procServ.cc:599). `u32::MAX` is the "no cap" sentinel the
                // shared RestartPolicy already treats as effectively unbounded.
                max_restarts: args.max_restarts.unwrap_or(u32::MAX),
                window: Duration::from_secs(args.restart_window),
                delay: Duration::from_secs(1),
            },
            restart_mode,
            holdoff: Duration::from_secs(args.holdoff),
            wait_for_manual_start: args.wait,
        })
    }

    pub fn entry() -> ExitCode {
        let args = Args::parse();

        // C `inDebugMode`: `-d`/`--debug` OR a present `PROCSERV_DEBUG`
        // env var (procServ.cc:225,291-292). It raises the diagnostic
        // verbosity (C's `PRINTF` = `if (inDebugMode) printf`,
        // procServ.h:30 → a debug-level tracing default, still overridable
        // by RUST_LOG) and keeps the child in the foreground.
        let debug = args.debug || std::env::var_os("PROCSERV_DEBUG").is_some();
        let quiet = args.quiet;

        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(if debug { "debug" } else { "info" })
                }),
            )
            .try_init();

        let mut cfg = match build_config(args) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "procserv-rs: invalid config");
                return ExitCode::FAILURE;
            }
        };
        // The `-d` flag already forced foreground in `build_config`; OR in
        // the env-only half of `inDebugMode` here.
        cfg.foreground = cfg.foreground || debug;
        let foreground = cfg.foreground;

        // Reject a config that cannot run (no control endpoint, empty
        // program) HERE in the foreground process, before binding or
        // forking. `ProcServ::new`'s own `validate()` runs post-fork in the
        // daemon child (procserv_rs.rs build below), where the parent has
        // already written the pidfile and `exit(0)`'d — a failure there is a
        // headless-daemon false-success (PS-51, sibling of PS-49's pre-fork
        // bind). C makes the control port a required positional, rejected
        // during getopt before `forkAndGo` (procServ.cc:551). Same
        // validation owner (`ProcServConfig::validate`) as the library path,
        // run at the foreground gate here for both modes.
        if let Err(e) = cfg.validate() {
            eprintln!("procserv-rs: {e}");
            return ExitCode::FAILURE;
        }

        // Bind every control + log listener in THIS (foreground) process,
        // before daemonizing, so a bind failure (EADDRINUSE, abstract socket
        // on a non-Linux host, bad UNIX path) aborts startup with a real
        // non-zero exit instead of leaving a daemonized-but-headless IOC
        // (PS-49). C binds every acceptItem before `forkAndGo` and
        // `exit(error)`s on failure (procServ.cc:513-543,551). A short-lived
        // current-thread runtime supplies the reactor the bind helpers need;
        // it is fully dropped (no tokio worker thread) before the fork, and
        // the bound fds are inherited by the daemon child across it.
        let prebound = {
            let bind_rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("procserv-rs: bind runtime build failed: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let result = bind_rt.block_on(async { bind_endpoints(&cfg.listen) });
            // `bind_rt` drops here, before `fork_and_go`.
            match result {
                Ok(listeners) => listeners,
                Err(e) => {
                    eprintln!("procserv-rs: {e}");
                    return ExitCode::FAILURE;
                }
            }
        };

        // Daemonize BEFORE starting the tokio runtime — the runtime's
        // worker threads don't survive fork(). After fork_and_go returns
        // we're in the daemon child (or directly in foreground mode) and
        // safe to start tokio. The foreground parent prints the spawn
        // notice + no-log-file warning (unless `quiet`) and writes the pid
        // file with the daemon's pid before exiting (procServ.cc:887-899).
        // The listeners bound above are inherited by the daemon child here.
        if !foreground {
            let parent = DaemonParent {
                name: "procserv-rs",
                pid_path: cfg.logging.pid_path.as_deref(),
                quiet,
                has_logfile: cfg.logging.log_path.is_some(),
                has_logport: cfg.listen.log.is_some(),
            };
            if let Err(e) = fork_and_go(parent) {
                eprintln!("procserv-rs: daemonize failed: {e}");
                return ExitCode::FAILURE;
            }
        }

        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "procserv-rs: tokio runtime build failed");
                return ExitCode::FAILURE;
            }
        };

        // C `procServ.cc:566-569`: in foreground mode the launching
        // terminal becomes a client — UNLESS the log goes to stdout
        // (`--logfile -`), where the child's output already lands on the
        // terminal and a console client would double it.
        let attach_console = foreground
            && cfg
                .logging
                .log_path
                .as_deref()
                .is_none_or(|p| p.as_os_str() != "-");

        runtime.block_on(async move {
            // The `TtyGuard` must outlive the session: dropping it restores
            // the terminal's echo/canonical mode (C `ttySetCharNoEcho(false)`
            // after the select loop, procServ.cc:684).
            let (console, _tty_guard) = if attach_console {
                match console::attach_console() {
                    Ok((client, guard)) => (Some(client), Some(guard)),
                    Err(e) => {
                        // C never checks; a terminal we cannot attach is not
                        // a reason to refuse to supervise the child — the
                        // control port still works.
                        tracing::error!(error = %e, "procserv-rs: unable to attach the console; continuing without it");
                        (None, None)
                    }
                }
            } else {
                (None, None)
            };

            let server = match ProcServ::new(cfg) {
                Ok(s) => {
                    let s = s.with_prebound(prebound);
                    match console {
                        Some(c) => s.with_console(c),
                        None => s,
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "procserv-rs: build failed");
                    return ExitCode::FAILURE;
                }
            };

            // Registration is non-fatal (PS-50): a failed handler is logged
            // and dropped inside `install_signal_handlers`, never aborting
            // the already-daemonized child. Matches C's unchecked sigaction
            // (procServ.cc:496-509).
            let shutdown = install_signal_handlers(foreground).await;

            // Race the supervisor against the shutdown signal. The
            // supervisor's own `quit` keystroke also returns from
            // .run(), so either branch ends the process cleanly.
            tokio::select! {
                res = server.run() => match res {
                    // Return the child's last exit code as our own, like
                    // C procServ (childExitCode, procServ.cc:701). Exit
                    // codes are 0-255; clamp the byte cast accordingly.
                    Ok(code) => ExitCode::from(code as u8),
                    Err(e) => {
                        tracing::error!(error = %e, "procserv-rs: runtime error");
                        ExitCode::FAILURE
                    }
                },
                reason = shutdown.wait() => {
                    tracing::info!(reason = ?reason.ok(), "procserv-rs: shutdown signal");
                    ExitCode::SUCCESS
                }
            }
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Unwrap an [`Endpoint`] expected to be TCP.
        fn as_tcp(ep: &Endpoint) -> std::net::SocketAddr {
            match ep {
                Endpoint::Tcp(a) => *a,
                other => panic!("expected TCP endpoint, got {other:?}"),
            }
        }

        /// Unwrap a config's single control endpoint as a TCP address.
        fn only_control_tcp(cfg: &ProcServConfig) -> std::net::SocketAddr {
            assert_eq!(
                cfg.listen.control.len(),
                1,
                "expected exactly one control endpoint"
            );
            as_tcp(&cfg.listen.control[0])
        }

        /// `-P` is repeatable: each occurrence adds a control endpoint, in
        /// CLI order (C `ctlSpecs`, procServ.cc:386). A bare port binds
        /// loopback by default; an `iface:port` is honored only with
        /// `--allow`.
        #[test]
        fn port_flag_is_repeatable_into_control_endpoints() {
            let args =
                Args::try_parse_from(["procserv-rs", "-P", "4051", "-P", "4052", "/bin/echo"])
                    .unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.listen.control.len(), 2);
            assert_eq!(
                as_tcp(&cfg.listen.control[0]),
                "127.0.0.1:4051".parse().unwrap()
            );
            assert_eq!(
                as_tcp(&cfg.listen.control[1]),
                "127.0.0.1:4052".parse().unwrap()
            );
        }

        /// `--allow` clears `ctlPortLocal`, so a bare port binds all
        /// interfaces and an `iface:port` binds the named interface
        /// (acceptFactory.cc:116,129).
        #[test]
        fn allow_binds_any_interface_and_honors_explicit_interface() {
            let bare = Args::try_parse_from(["procserv-rs", "--allow", "-P", "4051", "/bin/echo"])
                .unwrap();
            assert_eq!(
                only_control_tcp(&build_config(bare).unwrap()),
                "0.0.0.0:4051".parse().unwrap()
            );
            let iface = Args::try_parse_from([
                "procserv-rs",
                "--allow",
                "-P",
                "192.168.1.5:4051",
                "/bin/echo",
            ])
            .unwrap();
            assert_eq!(
                only_control_tcp(&build_config(iface).unwrap()),
                "192.168.1.5:4051".parse().unwrap()
            );
        }

        /// Without `--allow`, an explicit interface is ignored and the port
        /// binds loopback — C's `local ? INADDR_LOOPBACK : A.B.C.D`.
        #[test]
        fn interface_bind_forced_to_loopback_without_allow() {
            let args = Args::try_parse_from(["procserv-rs", "-P", "192.168.1.5:4051", "/bin/echo"])
                .unwrap();
            assert_eq!(
                only_control_tcp(&build_config(args).unwrap()),
                "127.0.0.1:4051".parse().unwrap()
            );
        }

        /// `-P unix:<spec>` and `--unixpath <path>` both produce a UNIX
        /// control endpoint; the spec form carries the full access-control
        /// surface.
        #[test]
        fn unix_endpoints_via_port_spec_and_unixpath() {
            let spec = Args::try_parse_from([
                "procserv-rs",
                "-P",
                "unix:::0660:/run/ioc.sock",
                "/bin/echo",
            ])
            .unwrap();
            let cfg = build_config(spec).unwrap();
            match &cfg.listen.control[0] {
                Endpoint::Unix(u) => {
                    assert_eq!(u.name, PathBuf::from("/run/ioc.sock"));
                    assert_eq!(u.perms, 0o660);
                    assert!(!u.abstract_socket);
                }
                other => panic!("expected UNIX endpoint, got {other:?}"),
            }

            let path =
                Args::try_parse_from(["procserv-rs", "--unixpath", "/run/ioc.sock", "/bin/echo"])
                    .unwrap();
            let cfg = build_config(path).unwrap();
            match &cfg.listen.control[0] {
                Endpoint::Unix(u) => {
                    assert_eq!(u.name, PathBuf::from("/run/ioc.sock"));
                    assert_eq!(u.perms, 0o666);
                }
                other => panic!("expected UNIX endpoint, got {other:?}"),
            }
        }

        /// A `-P` and a `--unixpath` together yield two control endpoints,
        /// `-P` first then the unix path (the build order).
        #[test]
        fn port_and_unixpath_combine_into_two_control_endpoints() {
            let args = Args::try_parse_from([
                "procserv-rs",
                "-P",
                "4051",
                "--unixpath",
                "/run/ioc.sock",
                "/bin/echo",
            ])
            .unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.listen.control.len(), 2);
            assert_eq!(
                as_tcp(&cfg.listen.control[0]),
                "127.0.0.1:4051".parse().unwrap()
            );
            assert!(matches!(&cfg.listen.control[1], Endpoint::Unix(_)));
        }

        /// An unparsable endpoint spec is a build error, mirroring C's
        /// `exit(1)` on a bad spec (acceptFactory.cc:147-148).
        #[test]
        fn bad_endpoint_spec_is_a_build_error() {
            let args =
                Args::try_parse_from(["procserv-rs", "-P", "not-a-port", "/bin/echo"]).unwrap();
            let err = build_config(args).unwrap_err();
            assert!(err.contains("Invalid socket spec"), "{err}");
        }

        /// With no `--max-restarts`, the count limiter is unlimited
        /// (`u32::MAX`) so the supervisor never gives up on a count —
        /// matching C procServ (processFactory.cc:48-56, procServ.cc:599).
        #[test]
        fn max_restarts_defaults_to_unlimited() {
            let args = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.restart.max_restarts, u32::MAX);
        }

        /// C hardwires the quit key to `^Q` (`0x11`) and never exposes it
        /// on the CLI (procServ.cc:69; absent from `long_options`); the
        /// port must default it enabled, not `None`.
        #[test]
        fn quit_key_defaults_to_ctrl_q() {
            let args = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.keys.quit, Some(0x11));
        }

        /// PS-28: `-d`/`--debug` and `-q`/`--quiet` must be accepted (C
        /// rejected wrappers passing them under clap previously).
        #[test]
        fn debug_and_quiet_flags_parse() {
            let d = Args::try_parse_from(["procserv-rs", "-d", "/bin/echo"]).unwrap();
            assert!(d.debug && !d.quiet);
            let d_long = Args::try_parse_from(["procserv-rs", "--debug", "/bin/echo"]).unwrap();
            assert!(d_long.debug);
            let q = Args::try_parse_from(["procserv-rs", "-q", "/bin/echo"]).unwrap();
            assert!(q.quiet && !q.debug);
            let q_long = Args::try_parse_from(["procserv-rs", "--quiet", "/bin/echo"]).unwrap();
            assert!(q_long.quiet);
        }

        /// C's daemonize gate is `!inFgMode && !inDebugMode`
        /// (procServ.cc:549), so `-d` keeps the child in the foreground
        /// exactly like `-f`.
        #[test]
        fn debug_forces_foreground() {
            let plain = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            assert!(!build_config(plain).unwrap().foreground);
            let dbg = Args::try_parse_from(["procserv-rs", "-d", "/bin/echo"]).unwrap();
            assert!(build_config(dbg).unwrap().foreground);
        }

        /// PS-30: C pidfile precedence is `-p` over `PROCSERV_PID` env over
        /// none, with an empty value meaning no pidfile
        /// (procServ.cc:125,224,382).
        #[test]
        fn pidfile_resolves_flag_over_env_over_none() {
            use std::ffi::OsString;
            // Flag wins over env.
            assert_eq!(
                resolve_pidfile(
                    Some(PathBuf::from("/run/flag.pid")),
                    Some(OsString::from("/env"))
                ),
                Some(PathBuf::from("/run/flag.pid"))
            );
            // Env provides the default when the flag is absent.
            assert_eq!(
                resolve_pidfile(None, Some(OsString::from("/run/env.pid"))),
                Some(PathBuf::from("/run/env.pid"))
            );
            // Neither → no pidfile.
            assert_eq!(resolve_pidfile(None, None), None);
            // Empty value (flag or env) → no pidfile.
            assert_eq!(resolve_pidfile(None, Some(OsString::from(""))), None);
            assert_eq!(resolve_pidfile(Some(PathBuf::from("")), None), None);
        }

        /// With no `-n`, C sets `childName == command` (the whole argv
        /// string), so the display name must default to the full program
        /// path — not its basename — and stay equal to the command
        /// (procServ.cc:455-457).
        #[test]
        fn child_name_defaults_to_whole_command_not_basename() {
            let args = Args::try_parse_from(["procserv-rs", "/opt/ioc/st.cmd"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.name, "/opt/ioc/st.cmd");
            assert_eq!(cfg.child.name, cfg.child.program.display().to_string());
        }

        /// `--ignore` applies C's `^`-escaping: `^A`..`^Z` → control byte,
        /// `^^` → literal `^`, anything else verbatim; lowercase after `^`
        /// is not an escape (procServ.cc:322-336).
        #[test]
        fn ignore_chars_apply_caret_escaping() {
            // ^X → 0x18, ^^ → '^', plain bytes verbatim.
            assert_eq!(parse_ignore_chars("^X"), vec![0x18]);
            assert_eq!(parse_ignore_chars("^^"), vec![b'^']);
            assert_eq!(parse_ignore_chars("ab"), vec![b'a', b'b']);
            // Two control escapes in a row: ^X → 0x18, ^A → 0x01.
            assert_eq!(parse_ignore_chars("^X^A"), vec![0x18, 0x01]);
            // Only A..Z map to control bytes — C gates on `>= 'A' && <= 'Z'`,
            // so `^]` (']' = 0x5d, above 'Z') is a literal '^' then ']', NOT
            // the 0x1d logout byte. (The logout key is stripped anyway via
            // the supervisor auto-append, independent of this parser.)
            assert_eq!(parse_ignore_chars("^]"), vec![b'^', b']']);
            // Lowercase after ^ is NOT a control escape (C gates on A..Z).
            assert_eq!(parse_ignore_chars("^a"), vec![b'^', b'a']);
            // A trailing lone ^ is a literal caret.
            assert_eq!(parse_ignore_chars("a^"), vec![b'a', b'^']);
        }

        /// `--ignore` flows into the child's explicit ignore set; the
        /// command keys are added later by the supervisor, so the config
        /// holds only the operator's bytes here.
        #[test]
        fn ignore_flag_populates_explicit_child_ignore_chars() {
            let args =
                Args::try_parse_from(["procserv-rs", "--ignore", "^X", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.ignore_chars, vec![0x18]);
        }

        /// PS-51: a config with a command but no control endpoint
        /// (`-P`/`--unixpath`) is accepted by `build_config` (control ends
        /// up empty) yet must be rejected by the validation owner — so the
        /// binary's pre-fork `cfg.validate()` gate fail-fasts in the
        /// foreground process rather than daemonizing into a headless child
        /// that dies post-fork after the parent already wrote the pidfile.
        #[test]
        fn no_control_endpoint_is_rejected_by_validate() {
            let args = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert!(
                cfg.listen.control.is_empty(),
                "no -P/--unixpath ⟹ empty control"
            );
            assert!(
                cfg.validate().is_err(),
                "a no-control-endpoint config must be rejected pre-fork"
            );
        }

        /// No `--killsig` → C default SIGKILL (procServ.cc:71).
        #[test]
        fn killsig_defaults_to_sigkill() {
            let args = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.kill_signal, libc::SIGKILL);
        }

        /// `--killsig <n>` sets the child's kill signal (e.g. SIGINT=2 for
        /// a graceful IOC shutdown), the use case config.rs documents.
        #[test]
        fn killsig_sets_signal_number() {
            let args =
                Args::try_parse_from(["procserv-rs", "--killsig", "2", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.kill_signal, 2);
        }

        /// `-K` is the convenience short for `--killsig`.
        #[test]
        fn killsig_short_flag_is_accepted() {
            let args = Args::try_parse_from(["procserv-rs", "-K", "15", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.kill_signal, 15);
        }

        /// A signal ≥ 32 is rejected and the SIGKILL default kept
        /// (C `i < 32` gate, procServ.cc:348-354).
        #[test]
        fn killsig_above_31_falls_back_to_default() {
            let args =
                Args::try_parse_from(["procserv-rs", "--killsig", "99", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.kill_signal, libc::SIGKILL);
        }

        /// A negative signal is abs'd before validation (C `abs(atoi(...))`,
        /// procServ.cc:347): `--killsig=-2` → 2.
        #[test]
        fn killsig_negative_is_abs() {
            let args = Args::try_parse_from(["procserv-rs", "--killsig=-2", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.kill_signal, 2);
        }

        /// `getOptionChar` caret notation (procServ.cc:142-152): empty →
        /// 0, `^^` → `^`, `^A`..`^Z` → byte−64, else the first byte.
        /// `^]`/lowercase are NOT escapes (C gates `^`-notation to A–Z).
        #[test]
        fn get_option_char_matches_c_semantics() {
            assert_eq!(get_option_char(""), 0); // disabled
            assert_eq!(get_option_char("^X"), 0x18); // Ctrl-X
            assert_eq!(get_option_char("^T"), 0x14); // Ctrl-T
            assert_eq!(get_option_char("^^"), b'^'); // literal caret
            assert_eq!(get_option_char("q"), b'q'); // bare char verbatim
            // `]` is above `Z`, so `^]` is NOT 0x1d — C returns buf[0]='^'.
            assert_eq!(get_option_char("^]"), b'^');
            // Lowercase after `^` is not an escape either.
            assert_eq!(get_option_char("^a"), b'^');
            // Only the first one/two bytes matter (C reads buf[0]/buf[1]).
            assert_eq!(get_option_char("abc"), b'a');
        }

        /// Defaults match C (`killChar=^X`, `toggleRestartChar=^T`,
        /// `logoutChar=0x00`, procServ.cc:66-70) when no command-key flag
        /// is given.
        #[test]
        fn command_keys_default_to_c_values() {
            let args = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.keys.kill, Some(0x18));
            assert_eq!(cfg.keys.toggle_restart, Some(0x14));
            assert_eq!(cfg.keys.logout, None);
        }

        /// `-k`/`--killcmd` accepts caret notation; an empty value
        /// disables the key (C getOptionChar → 0 → nz → None).
        #[test]
        fn killcmd_parses_caret_and_disable() {
            let caret =
                Args::try_parse_from(["procserv-rs", "--killcmd", "^X", "/bin/echo"]).unwrap();
            assert_eq!(build_config(caret).unwrap().keys.kill, Some(0x18));
            let bare = Args::try_parse_from(["procserv-rs", "-k", "q", "/bin/echo"]).unwrap();
            assert_eq!(build_config(bare).unwrap().keys.kill, Some(b'q'));
            let off = Args::try_parse_from(["procserv-rs", "--killcmd=", "/bin/echo"]).unwrap();
            assert_eq!(build_config(off).unwrap().keys.kill, None);
        }

        /// `--autorestartcmd` (long-only in C; `-T` convenience short)
        /// sets the toggle-restart key.
        #[test]
        fn autorestartcmd_sets_toggle_key() {
            let long = Args::try_parse_from(["procserv-rs", "--autorestartcmd", "^G", "/bin/echo"])
                .unwrap();
            assert_eq!(build_config(long).unwrap().keys.toggle_restart, Some(0x07));
            let short = Args::try_parse_from(["procserv-rs", "-T", "x", "/bin/echo"]).unwrap();
            assert_eq!(build_config(short).unwrap().keys.toggle_restart, Some(b'x'));
        }

        /// `-x`/`--logoutcmd` enables the per-client logout key via caret
        /// notation; `^G` → 0x07 (a real control char in the A–Z range).
        #[test]
        fn logoutcmd_enables_logout_key() {
            let args =
                Args::try_parse_from(["procserv-rs", "--logoutcmd", "^G", "/bin/echo"]).unwrap();
            assert_eq!(build_config(args).unwrap().keys.logout, Some(0x07));
            let short = Args::try_parse_from(["procserv-rs", "-x", "z", "/bin/echo"]).unwrap();
            assert_eq!(build_config(short).unwrap().keys.logout, Some(b'z'));
        }

        /// PS-40: `-c`/`-I`/`-n` are C's real short options (present in the
        /// getopt optstring, procServ.cc:264), so a wrapper invoking
        /// `-c <dir>` / `-I <file>` / `-n <name>` — inert on clap before —
        /// must now parse to the same config as the long forms.
        #[test]
        fn c_faithful_short_aliases_parse() {
            let short = Args::try_parse_from([
                "procserv-rs",
                "-c",
                "/work",
                "-I",
                "/run/x.info",
                "-n",
                "myioc",
                "/bin/echo",
            ])
            .unwrap();
            let cfg = build_config(short).unwrap();
            assert_eq!(cfg.child.cwd, Some(PathBuf::from("/work")));
            assert_eq!(cfg.logging.info_path, Some(PathBuf::from("/run/x.info")));
            assert_eq!(cfg.child.name, "myioc");

            // The long forms still produce the identical config.
            let long = Args::try_parse_from([
                "procserv-rs",
                "--chdir",
                "/work",
                "--info-file",
                "/run/x.info",
                "--name",
                "myioc",
                "/bin/echo",
            ])
            .unwrap();
            let cfg_long = build_config(long).unwrap();
            assert_eq!(cfg_long.child.cwd, Some(PathBuf::from("/work")));
            assert_eq!(
                cfg_long.logging.info_path,
                Some(PathBuf::from("/run/x.info"))
            );
            assert_eq!(cfg_long.child.name, "myioc");
        }

        /// C binds `-P` (uppercase) to the control port and `-p`
        /// (lowercase) to the PID file (procServ.cc:250-251,264). The
        /// Rust shorts must match: `-P 4051` → port, `-p /run/x.pid` →
        /// pidfile — the inverse of the old port-on-`-p` assignment.
        #[test]
        fn short_p_is_pidfile_and_short_uppercase_p_is_port() {
            let args = Args::try_parse_from([
                "procserv-rs",
                "-P",
                "4051",
                "-p",
                "/run/ioc.pid",
                "/bin/echo",
            ])
            .unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(only_control_tcp(&cfg), "127.0.0.1:4051".parse().unwrap());
            assert_eq!(cfg.logging.pid_path, Some(PathBuf::from("/run/ioc.pid")));
        }

        /// `-p` no longer parses as the port: a path value is rejected by
        /// the u16 port parser only if mis-routed, but here `-p` must take
        /// the pidfile path and leave the control endpoints empty.
        #[test]
        fn short_p_does_not_set_the_port() {
            let args =
                Args::try_parse_from(["procserv-rs", "-p", "/run/ioc.pid", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert!(cfg.listen.control.is_empty());
            assert_eq!(cfg.logging.pid_path, Some(PathBuf::from("/run/ioc.pid")));
        }

        /// No `--exec` → exec the positional command itself
        /// (C default `childExec = command`, procServ.cc:461).
        #[test]
        fn exec_absent_runs_the_command_itself() {
            let args = Args::try_parse_from(["procserv-rs", "/opt/ioc/st.cmd"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.child_exec, None);
            assert_eq!(cfg.child.program, PathBuf::from("/opt/ioc/st.cmd"));
        }

        /// `-e`/`--exec <path>` overrides the exec target while leaving the
        /// positional command as the displayed program / argv[0]
        /// (C `childExec`, procServ.cc:295-296,459-462).
        #[test]
        fn exec_overrides_exec_target_only() {
            let args = Args::try_parse_from([
                "procserv-rs",
                "--exec",
                "/usr/bin/softIoc",
                "/opt/ioc/st.cmd",
            ])
            .unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(
                cfg.child.child_exec,
                Some(PathBuf::from("/usr/bin/softIoc"))
            );
            assert_eq!(cfg.child.program, PathBuf::from("/opt/ioc/st.cmd"));
            // Display name still defaults to the command, not the exec.
            assert_eq!(cfg.child.name, "/opt/ioc/st.cmd");
        }

        /// No mode flag → C default `restart` (Rust `OnExit`,
        /// procServ.cc:58).
        #[test]
        fn restart_mode_defaults_to_on_exit() {
            let args = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.restart_mode, RestartMode::OnExit);
        }

        /// `-N`/`--noautorestart` starts in no-restart mode (C `norestart`,
        /// procServ.cc:369-371): server stays up, child not relaunched.
        #[test]
        fn noautorestart_starts_in_disabled_mode() {
            let long =
                Args::try_parse_from(["procserv-rs", "--noautorestart", "/bin/echo"]).unwrap();
            assert_eq!(
                build_config(long).unwrap().restart_mode,
                RestartMode::Disabled
            );
            let short = Args::try_parse_from(["procserv-rs", "-N", "/bin/echo"]).unwrap();
            assert_eq!(
                build_config(short).unwrap().restart_mode,
                RestartMode::Disabled
            );
        }

        /// `-o`/`--oneshot` starts in one-shot mode (C `oneshot`,
        /// procServ.cc:373-375): server exits when the child exits.
        #[test]
        fn oneshot_starts_in_oneshot_mode() {
            let long = Args::try_parse_from(["procserv-rs", "--oneshot", "/bin/echo"]).unwrap();
            assert_eq!(
                build_config(long).unwrap().restart_mode,
                RestartMode::OneShot
            );
            let short = Args::try_parse_from(["procserv-rs", "-o", "/bin/echo"]).unwrap();
            assert_eq!(
                build_config(short).unwrap().restart_mode,
                RestartMode::OneShot
            );
        }

        /// `-N` and `-o` are mutually exclusive (clap rejects the combo);
        /// the two modes set the same C `restartMode` field.
        #[test]
        fn noautorestart_and_oneshot_conflict() {
            let res = Args::try_parse_from(["procserv-rs", "-N", "-o", "/bin/echo"]);
            assert!(res.is_err(), "-N and -o must conflict");
        }

        /// No `--coresize` → no RLIMIT_CORE change (C `setCoreSize`
        /// stays false, procServ.cc:72).
        #[test]
        fn coresize_absent_leaves_core_limit_untouched() {
            let args = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.core_size, None);
        }

        /// `--coresize <n>` for `n >= 0` sets the limit; `-C` is the
        /// convenience short. Zero is a valid limit (disable core dumps).
        #[test]
        fn coresize_nonnegative_sets_limit() {
            let args = Args::try_parse_from(["procserv-rs", "--coresize", "1048576", "/bin/echo"])
                .unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.core_size, Some(1_048_576));

            let zero = Args::try_parse_from(["procserv-rs", "-C", "0", "/bin/echo"]).unwrap();
            assert_eq!(build_config(zero).unwrap().child.core_size, Some(0));
        }

        /// A negative `--coresize` is inert (C honors only `l >= 0`,
        /// procServ.cc:281-284), leaving the inherited limit untouched.
        #[test]
        fn coresize_negative_is_inert() {
            let args = Args::try_parse_from(["procserv-rs", "--coresize=-1", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.core_size, None);
        }

        /// `-n`/`--name` still overrides the default.
        #[test]
        fn child_name_honors_explicit_name() {
            let args =
                Args::try_parse_from(["procserv-rs", "--name", "ioc", "/opt/ioc/st.cmd"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.child.name, "ioc");
        }

        /// C starts `logoutChar` at `0x00` (disabled) and enables it only
        /// via `--logoutcmd` (procServ.cc:70,403); a stock server must not
        /// disconnect a client on `0x1d`, a byte many telnet clients send.
        #[test]
        fn logout_key_disabled_by_default() {
            let args = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.keys.logout, None);
        }

        /// Opting in caps the count at the requested value.
        #[test]
        fn max_restarts_opt_in_is_honored() {
            let args =
                Args::try_parse_from(["procserv-rs", "--max-restarts", "5", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.restart.max_restarts, 5);
        }

        /// The log/viewer port binds all interfaces by default (C
        /// `logPortLocal` defaults false), the inverse of the control
        /// port's localhost default.
        #[test]
        fn log_port_binds_all_interfaces_by_default() {
            let args =
                Args::try_parse_from(["procserv-rs", "--logport", "7000", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(
                as_tcp(cfg.listen.log.as_ref().unwrap()),
                std::net::SocketAddr::from(([0, 0, 0, 0], 7000))
            );
        }

        /// `--restrict` flips the log port to localhost-only (C 'R').
        #[test]
        fn restrict_binds_log_port_to_localhost() {
            let args = Args::try_parse_from([
                "procserv-rs",
                "--logport",
                "7000",
                "--restrict",
                "/bin/echo",
            ])
            .unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(
                as_tcp(cfg.listen.log.as_ref().unwrap()),
                std::net::SocketAddr::from(([127, 0, 0, 1], 7000))
            );
        }

        /// No `--logstamp` → unstamped log (C `stampLog` defaults false).
        #[test]
        fn logstamp_absent_is_unstamped_by_default() {
            let args = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert!(!cfg.logging.stamp_log);
        }

        /// `--logstamp` alone enables stamping with the bracketed default
        /// stamp format, built from the default timeFormat "%c"
        /// (C 'S' with no optarg, procServ.cc:307-311,464-468).
        #[test]
        fn logstamp_flag_enables_stamping_with_default_format() {
            let args = Args::try_parse_from(["procserv-rs", "--logstamp", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert!(cfg.logging.stamp_log);
            assert_eq!(cfg.logging.stamp_format, "[%c] ");
        }

        /// `--logstamp=<FMT>` enables stamping and uses FMT raw (C 'S'
        /// with optarg sets stampFormat, procServ.cc:309-310).
        #[test]
        fn logstamp_with_value_sets_raw_format() {
            let args =
                Args::try_parse_from(["procserv-rs", "--logstamp=%H:%M:%S ", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert!(cfg.logging.stamp_log);
            assert_eq!(cfg.logging.stamp_format, "%H:%M:%S ");
        }

        /// No `--timefmt` → C's default timeFormat "%c" (procServ.cc:80).
        #[test]
        fn timefmt_absent_uses_c_default() {
            let args = Args::try_parse_from(["procserv-rs", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.logging.time_format, "%c");
        }

        /// `--timefmt <FMT>` sets the banner format and (per C
        /// procServ.cc:464-468) also rebases the default log stamp
        /// "[" + timeFormat + "] ".
        #[test]
        fn timefmt_sets_banner_and_rebases_default_stamp() {
            let args = Args::try_parse_from([
                "procserv-rs",
                "--timefmt",
                "%H:%M",
                "--logstamp",
                "/bin/echo",
            ])
            .unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.logging.time_format, "%H:%M");
            assert_eq!(cfg.logging.stamp_format, "[%H:%M] ");
        }

        /// An explicit `--logstamp=<FMT>` still wins over the
        /// timefmt-derived default stamp (C uses the optarg verbatim).
        #[test]
        fn logstamp_value_overrides_timefmt_derived_stamp() {
            let args = Args::try_parse_from([
                "procserv-rs",
                "--timefmt",
                "%H:%M",
                "--logstamp=RAW ",
                "/bin/echo",
            ])
            .unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.logging.time_format, "%H:%M");
            assert_eq!(cfg.logging.stamp_format, "RAW ");
        }
    }
}

#[cfg(procserv_host_platform)]
fn main() -> std::process::ExitCode {
    app::entry()
}
