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
        /// TCP port to listen on (`-p` / `--port` in C procServ).
        #[arg(short = 'p', long)]
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

        /// Log file (`-L` / `--logfile`).
        #[arg(short = 'L', long)]
        logfile: Option<PathBuf>,

        /// PID file (`--pidfile`).
        #[arg(long)]
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

        /// chdir to this directory before exec'ing the child.
        #[arg(long)]
        chdir: Option<PathBuf>,

        /// Display name for the child in banners.
        #[arg(long)]
        name: Option<String>,

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

        /// Logout character (Ctrl-] = 29). 0 to disable.
        #[arg(long, default_value_t = 29)]
        logout_char: u8,

        /// Program to launch + its arguments. Everything after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    }

    fn build_config(args: Args) -> Result<ProcServConfig, String> {
        if args.cmd.is_empty() {
            return Err("missing child command".into());
        }
        let mut iter = args.cmd.into_iter();
        let program = PathBuf::from(iter.next().unwrap());
        let argv: Vec<String> = iter.collect();
        let display_name = args.name.unwrap_or_else(|| {
            program
                .file_name()
                .map_or("child".into(), |s| s.to_string_lossy().into())
        });

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

        let child = ChildConfig {
            name: display_name,
            program,
            args: argv,
            cwd: args.chdir,
            kill_signal: 9, // SIGKILL — match C default
            ignore_chars: Vec::new(),
        };

        let logging = LoggingConfig {
            log_path: args.logfile,
            pid_path: args.pidfile,
            info_path: args.info_file,
            time_format: "%Y-%m-%d %H:%M:%S".into(),
        };

        let nz = |c: u8| if c == 0 { None } else { Some(c) };

        Ok(ProcServConfig {
            foreground: args.foreground,
            listen,
            keys: KeyBindings {
                kill: nz(args.kill_char),
                toggle_restart: nz(args.toggle_restart_char),
                restart: Some(0x12), // Ctrl-R when child dead
                quit: None,
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
            restart_mode: RestartMode::OnExit,
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
            // supervisor's own `quit` keystroke also returns Ok(())
            // from .run(), so either branch ends the process cleanly.
            tokio::select! {
                res = server.run() => match res {
                    Ok(()) => ExitCode::SUCCESS,
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
    }
}

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    app::entry()
}
