//! Operational tooling for EPICS deployments.
//!
//! ## First tenant: `procserv`
//!
//! Rust port of `epics-modules/procServ` — a PTY-based process
//! supervisor with multi-client telnet console. The C implementation
//! has these load-bearing pieces; their Rust equivalents in this
//! crate are listed alongside:
//!
//! | C source                              | Rust module                |
//! |---------------------------------------|----------------------------|
//! | `procServ.cc` (main, SendToAll)       | `procserv::supervisor`   |
//! | `processFactory.cc` (PTY child)       | `procserv::child`        |
//! | `acceptFactory.cc` (TCP/UNIX listen)  | `procserv::listener`     |
//! | `clientFactory.cc` (per-client conn)  | `procserv::client`       |
//! | libtelnet IAC parser/encoder          | `procserv::telnet`       |
//! | `processInput` command-key dispatch   | `procserv::menu`         |
//! | `processFactoryNeedsRestart` policy   | `procserv::restart`      |
//! | `forkAndGo` daemonize + signals       | `procserv::daemon`       |
//! | log/info/pid file + PROCSERV_INFO env | `procserv::sidecar`      |
//!
//! ## Architectural notes (from porting analysis)
//!
//! * **Hub-and-spoke fan-out**, not direct broadcast. The C version's
//!   `SendToAll(buf, count, sender)` excludes the sender from the
//!   party-line; we get the same semantics naturally with a single
//!   supervisor task that forwards each per-connection mpsc message
//!   to every other connection's mpsc. `tokio::sync::broadcast` would
//!   re-deliver to the sender — extra filtering required.
//!
//! * **No "master" role**. Permissions are per-connection
//!   (`readonly: bool`), set at construct time. Every non-readonly
//!   client can input. The PTY child is itself a connection, so
//!   client input flowing through the supervisor naturally reaches
//!   the child's stdin via the PTY-master fd. Matches C
//!   `connectionItem::_readonly` model.
//!
//! * **Stateless command-key dispatch**, not a menu FSM. Each input
//!   byte is matched against the configured `restartChar`/`killChar`/
//!   `toggleRestartChar`/`logoutChar`/`quitChar` and acted on
//!   immediately. The keys are still echoed to other connections.
//!
//! * **Narrow telnet usage**. Only `IAC WILL ECHO` + `IAC DO
//!   LINEMODE` negotiated; only DATA/SEND/ERROR events handled. The
//!   in-crate `procserv::telnet` parser is ~80 LOC, vendoring
//!   `libtelnet.c` is unnecessary.
//!
//! * **Host platforms only**. C procServ requires `forkpty(3)`,
//!   `execvp(3)` and POSIX signals, and declares it in its build
//!   system as `PROD_HOST`. The module is gated on
//!   `procserv_host_platform`, an allowlist emitted by `build.rs`,
//!   not on `cfg(unix)` — RTEMS and VxWorks are unix-family with no
//!   second process to supervise, and a unix target that is not on
//!   the list is refused at build time rather than compiled away.
//!   Cross-platform support (ConPTY on Windows) is future work; there
//!   the module compiles away so workspace builds succeed and the
//!   binary reports why.

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(all(feature = "procserv", procserv_host_platform))]
pub mod procserv;
