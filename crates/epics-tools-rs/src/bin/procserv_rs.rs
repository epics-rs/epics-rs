//! `procserv-rs` — Rust port of the EPICS procServ daemon.
//!
//! Drop-in for `epics-modules/procServ` deployments: same flag set,
//! same `PROCSERV_INFO` env contract, same per-line PTY behaviour.
//! See [`epics_tools_rs::procserv`] for architectural notes.

#[cfg(not(unix))]
fn main() {
    eprintln!("procserv-rs is Unix-only (requires forkpty)");
    std::process::exit(2);
}

#[cfg(unix)]
mod app {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::Duration;

    use clap::Parser;

    use epics_tools_rs::procserv::{
        ProcServ, ProcServConfig,
        config::{ChildConfig, KeyBindings, ListenConfig, LoggingConfig},
        daemon::{fork_and_go, install_signal_handlers},
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
        /// TCP port to listen on (`-P` / `--port` in C procServ,
        /// procServ.cc:251,264). Note `-P` (uppercase): C binds the
        /// control port to `-P` and the PID file to `-p` (lowercase).
        #[arg(short = 'P', long)]
        port: Option<u16>,

        /// Bind to all interfaces; default is localhost only.
        /// (C procServ `--allow`).
        #[arg(long)]
        allow: bool,

        /// Read-only viewer/log TCP port (`-l` / `--logport` in C).
        /// Clients here see all output but cannot inject input.
        #[arg(short = 'l', long = "logport")]
        log_port: Option<u16>,

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
        /// file to `-p` and the control port to `-P` (uppercase).
        #[arg(short = 'p', long)]
        pidfile: Option<PathBuf>,

        /// Info file (`--info-file`).
        #[arg(long)]
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

        /// chdir to this directory before exec'ing the child.
        #[arg(long)]
        chdir: Option<PathBuf>,

        /// Display name for the child in banners.
        #[arg(long)]
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

        /// Kill character (Ctrl-X = 24). Use 0 to disable.
        #[arg(long, default_value_t = 24)]
        kill_char: u8,

        /// Toggle restart-mode character (Ctrl-T = 20). 0 to disable.
        #[arg(long, default_value_t = 20)]
        toggle_restart_char: u8,

        /// Logout character. Disabled by default to match C, whose
        /// `logoutChar` starts at `0x00` and is enabled only by
        /// `--logoutcmd` (procServ.cc:70,403). Give a non-zero byte to
        /// enable `^]`-style client logout; `0` keeps it off.
        #[arg(long, default_value_t = 0)]
        logout_char: u8,

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

        let listen = ListenConfig {
            tcp_port: args.port,
            tcp_bind: args.port.map(|p| {
                if args.allow {
                    std::net::SocketAddr::from(([0, 0, 0, 0], p))
                } else {
                    std::net::SocketAddr::from(([127, 0, 0, 1], p))
                }
            }),
            log_port: args.log_port,
            // C `logPortLocal` defaults false → all interfaces; `--restrict`
            // flips it to localhost (acceptFactory.cc:116, procServ.cc:377).
            log_bind: args.log_port.map(|p| {
                if args.restrict {
                    std::net::SocketAddr::from(([127, 0, 0, 1], p))
                } else {
                    std::net::SocketAddr::from(([0, 0, 0, 0], p))
                }
            }),
            unix_path: args.unix_path,
        };

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
            pid_path: args.pidfile,
            info_path: args.info_file,
            stamp_log,
            time_format,
            stamp_format,
        };

        let nz = |c: u8| if c == 0 { None } else { Some(c) };

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
            foreground: args.foreground,
            listen,
            keys: KeyBindings {
                kill: nz(args.kill_char),
                toggle_restart: nz(args.toggle_restart_char),
                restart: Some(0x12), // Ctrl-R when child dead
                // C `quitChar` = ^Q (0x11), hardwired and never exposed
                // on the CLI (procServ.cc:69; absent from long_options).
                // Fires only while the child is shut down
                // (clientFactory.cc:214-217); the menu scan gates it.
                quit: Some(0x11),
                logout: nz(args.logout_char),
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
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .try_init();

        let args = Args::parse();
        let foreground = args.foreground;
        let cfg = match build_config(args) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "procserv-rs: invalid config");
                return ExitCode::FAILURE;
            }
        };

        // Daemonize BEFORE starting the tokio runtime — the runtime's
        // worker threads don't survive fork(). After fork_and_go
        // returns we're in the grandchild (or directly in foreground
        // mode) and safe to start tokio.
        if !foreground && let Err(e) = fork_and_go() {
            eprintln!("procserv-rs: daemonize failed: {e}");
            return ExitCode::FAILURE;
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

        runtime.block_on(async move {
            let server = match ProcServ::new(cfg) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "procserv-rs: build failed");
                    return ExitCode::FAILURE;
                }
            };

            let shutdown = match install_signal_handlers().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "procserv-rs: signal handler install failed");
                    return ExitCode::FAILURE;
                }
            };

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
            assert_eq!(cfg.listen.tcp_port, Some(4051));
            assert_eq!(cfg.logging.pid_path, Some(PathBuf::from("/run/ioc.pid")));
        }

        /// `-p` no longer parses as the port: a path value is rejected by
        /// the u16 port parser only if mis-routed, but here `-p` must take
        /// the pidfile path and leave the port unset.
        #[test]
        fn short_p_does_not_set_the_port() {
            let args =
                Args::try_parse_from(["procserv-rs", "-p", "/run/ioc.pid", "/bin/echo"]).unwrap();
            let cfg = build_config(args).unwrap();
            assert_eq!(cfg.listen.tcp_port, None);
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
            assert_eq!(cfg.listen.log_port, Some(7000));
            assert_eq!(
                cfg.listen.log_bind,
                Some(std::net::SocketAddr::from(([0, 0, 0, 0], 7000)))
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
                cfg.listen.log_bind,
                Some(std::net::SocketAddr::from(([127, 0, 0, 1], 7000)))
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

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    app::entry()
}
