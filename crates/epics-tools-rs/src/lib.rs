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
//!
//! # C reference pins
//!
//! Every `file.c:NNN` citation in this crate resolves at the tree and revision
//! below, not at whatever that tree's working copy holds today. These trees are
//! checked out on local branches here and run ahead of their pins.
//!
//! | tree | pinned revision |
//! | --- | --- |
//! | `procServ` | `v2.8.0-50-ge106eb8` |
//!
//! **Resolve by symbol at the pin; the line is a hint.** Find the named
//! function, struct, macro or field first, and treat the line number as a hint
//! that has to land inside that construct. Three cases follow:
//!
//! 1. Construct at the pin, line lands in it — the citation is exact. A
//!    reference checkout ahead of the pin will disagree; that disagreement is
//!    the checkout's, not the citation's.
//! 2. Construct at the pin, line lands outside it — line drift. Keep the
//!    symbol and move the line to the pin's.
//! 3. Construct absent at the pin — the citation means code added after it,
//!    and is NOT moved onto the pin, where it would point at lines that do not
//!    exist. It names the revision it means inline, beside the line span: the
//!    upstream PR and commit, and that both are later than the pin this table
//!    gives. `epics-libcom-rs` already carries that form.
//!
//! Every pin above passes `git merge-base --is-ancestor <pin> origin/<default>`
//! in its own tree, which is the test a pin has to meet. A `git describe`
//! string names an exact commit and is worth as much as a tag; what
//! disqualifies a revision is being reachable only from a fork branch or an
//! unmerged PR, because then it names nothing a reader outside this workspace
//! can fetch.
//!
//! Resolve each citation on its own. One sentence can cite two lines that are
//! right at different revisions, and a check run at either revision then
//! reports a single tidy error while vouching for the very citation the other
//! condemns.
//!
//! A row reading *no settled pin* means no revision has been agreed for that
//! tree: say which revision you read, and do not take its `HEAD` for the pin.
//! Citations into non-EPICS sources (libc, RTEMS, `rtems-libbsd`, VxWorks,
//! vendored third-party) are outside this table and carry no pin.

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(all(feature = "procserv", procserv_host_platform))]
pub mod procserv;
