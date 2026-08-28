//! The IOC's registry of protocol server layers — C `dbServer.c`.
//!
//! C keeps one process-global list of the servers that speak a protocol on
//! the IOC's behalf (`static ELLLIST serverList`, `dbServer.c:23`) plus the
//! phase the whole set is in (`static ... state`, `:24-27`). A server layer
//! adds itself with `dbRegisterServer` — RSRV does exactly that from
//! `rsrv_register_server` (`caservertask.c:1573-1575`), handing over its own
//! `casr` as the report function — and `dbsr` then walks the list.
//!
//! That indirection is the whole point of the design: `dbsr` is *not* a CA
//! command and knows nothing about channels or clients. It prints the phase,
//! then delegates to each registered layer. Porting it as anything else makes
//! it a different command; the port's `dbsr` used to print the database's own
//! record/alias/PV population, which is a number no C `dbsr` has ever shown.
//!
//! Deliberately not ported, because nothing in this workspace calls them:
//! `dbUnregisterServer`, `dbServerClient` and `dbServerStats` (C's iocStats
//! hook), and the `initialized` phase with its `dbInitServers` transition —
//! the port's servers are stood up by the protocol runner, which has no
//! separate "built but not started" step for `dbInitServers` to mark. The
//! `paused` phase IS ported: `iocPause` reaches it (see
//! [`crate::server::ioc_app::ioc_pause`]), so a `Server state: paused` line
//! now names a phase this IOC can be in.

use std::sync::{Mutex, OnceLock};

/// What a server layer prints for `dbsr <level>` — C's `dbServer.report`
/// (`dbServer.h:96`), which RSRV fills with `casr`.
///
/// The sink is the caller's, not `println!`: `dbsr` runs inside an iocsh
/// command whose stdout may be redirected (`>` / `>>`), and C gets the same
/// effect because its `printf` goes through the FILE* `startRedirect`
/// installed on the thread.
pub type ReportFn = Box<dyn Fn(u32, &dyn Fn(&str)) + Send + Sync>;

/// One registered protocol server layer — C `struct dbServer`
/// (`dbServer.h:73-113`), less the members no caller here uses.
pub struct DbServer {
    /// C `dbServer.name` — must contain no space (`dbServer.c:37-41`), and is
    /// what `EPICS_IOC_IGNORE_SERVERS` names.
    pub name: &'static str,
    /// C `dbServer.report`, called by `dbsr` only while the set is running.
    pub report: Option<ReportFn>,
}

/// The phase the whole server set is in — C's `state` and the `stateNames[]`
/// it is printed through (`dbServer.c:24-27`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServerState {
    Registering,
    Running,
    Paused,
    Stopped,
}

impl ServerState {
    /// C `stateNames[]` — the exact strings `dbsr` prints.
    fn name(self) -> &'static str {
        match self {
            ServerState::Registering => "registering",
            ServerState::Running => "running",
            ServerState::Paused => "paused",
            ServerState::Stopped => "stopped",
        }
    }
}

struct Registry {
    servers: Vec<DbServer>,
    state: ServerState,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            servers: Vec::new(),
            state: ServerState::Registering,
        })
    })
}

/// Is `name` listed in `EPICS_IOC_IGNORE_SERVERS`?
///
/// C matches on whitespace boundaries inside one space-separated string, not
/// on a bare substring — `dbServer.c:45-58` walks `strstr` hits and accepts
/// one only when the character before is the start or a space and the
/// character after is the end or a space. So `EPICS_IOC_IGNORE_SERVERS=qsrv2`
/// does not suppress `qsrv`, and `...=rsrv qsrv2` suppresses both.
fn ignored_by_env(name: &str) -> bool {
    let Ok(ignore) = std::env::var("EPICS_IOC_IGNORE_SERVERS") else {
        return false;
    };
    ignore.split(' ').any(|tok| tok == name)
}

/// C `dbRegisterServer` (`dbServer.c:30-72`). Returns whether the layer was
/// accepted; a name suppressed by `EPICS_IOC_IGNORE_SERVERS` is a success in
/// C too (it returns 0), so the caller must not treat that as a failure.
///
/// Refuses outside the `registering` phase, on a name containing a space, and
/// on a duplicate name — each with C's own diagnostic.
pub fn db_register_server(server: DbServer) -> bool {
    admit(&mut registry().lock().unwrap(), server)
}

/// [`db_register_server`] without the lock — the whole of C's checks, so the
/// gates can be tested against a private registry the way C's file static
/// cannot be.
fn admit(reg: &mut Registry, server: DbServer) -> bool {
    if reg.state != ServerState::Registering {
        return false;
    }
    if server.name.contains(' ') {
        eprintln!("dbRegisterServer: Bad server name '{}'", server.name);
        return false;
    }
    if ignored_by_env(server.name) {
        eprintln!(
            "dbRegisterServer: Ignoring '{}', per environment",
            server.name
        );
        return true;
    }
    if reg.servers.iter().any(|s| s.name == server.name) {
        eprintln!("dbRegisterServer: Can't redefine '{}'.", server.name);
        return false;
    }
    reg.servers.push(server);
    true
}

/// C `dbRunServers` (`dbServer.c:157-169`, the `STARTSTOP` macro) — the phase
/// `dbsr` needs before it will call any layer's report.
pub fn db_run_servers() {
    registry().lock().unwrap().state = ServerState::Running;
}

/// C `dbPauseServers` (`dbServer.c:157-169`) — the phase `iocPause` leaves
/// the set in, and the one `dbsr` suppresses layer reports in alongside
/// `registering` and `stopped`.
pub fn db_pause_servers() {
    registry().lock().unwrap().state = ServerState::Paused;
}

/// C `dbStopServers` (`dbServer.c:157-169`).
pub fn db_stop_servers() {
    registry().lock().unwrap().state = ServerState::Stopped;
}

/// How many times a protocol server layer has begun serving in this process.
///
/// C needs no such counter. `dbInitServers()` binds every registered layer in
/// `iocBuild` (`iocInit.c:222`) and `dbRunServers()` starts them in `iocRun`
/// (`:265-267`), and both are plain calls that RETURN, so by the time
/// `iocInit()` is done the sockets are up and RSRV's `casr` is on the command
/// table — the next line of the startup script cannot see a half-started IOC.
/// The port hands that work to a protocol runner the lifecycle SPAWNS, so the
/// same proof has to travel back: [`crate::server::ioc_app`] samples this
/// counter before it starts the runner and does not let `iocInit` return until
/// the runner has moved it.
///
/// A counter and not a flag, so a second IOC in one process waits for its own
/// layer rather than reading the previous one's announcement.
fn serving() -> &'static (std::sync::atomic::AtomicU64, tokio::sync::Notify) {
    static SERVING: OnceLock<(std::sync::atomic::AtomicU64, tokio::sync::Notify)> = OnceLock::new();
    SERVING.get_or_init(|| {
        (
            std::sync::atomic::AtomicU64::new(0),
            tokio::sync::Notify::new(),
        )
    })
}

/// The generation to wait past — sample it BEFORE the runner is started.
pub fn serving_generation() -> u64 {
    serving().0.load(std::sync::atomic::Ordering::Acquire)
}

/// Announce that this layer has bound, registered and begun serving: the
/// port's stand-in for `rsrv_run` having returned (`caservertask.c:766-771`).
///
/// **The single owner is each protocol server's serve entry** — the one
/// function that starts accepting, `CaServer::run` and the PVA server's
/// `run_with_source_inner` bind callback. Announcing there and nowhere else is
/// what keeps "a layer is serving" and "the generation moved" from coming
/// apart: a caller cannot start a server by another route, because there is no
/// other route to accepting a client.
pub fn announce_serving() {
    serving()
        .0
        .fetch_add(1, std::sync::atomic::Ordering::Release);
    serving().1.notify_waiters();
}

/// Resolve once [`announce_serving`] has been called since `generation` was
/// sampled. Returns immediately if it already has.
pub async fn serving_after(generation: u64) {
    loop {
        // Registered before the load, so an announcement between the two is
        // delivered rather than missed.
        let notified = serving().1.notified();
        if serving_generation() != generation {
            return;
        }
        notified.await;
    }
}

/// C `dbsr` (`dbServer.c:95-112`), verbatim in shape: the no-layers line and
/// an early return, else the state line and then one `Server '<name>'` line
/// per layer, each followed by that layer's report — and the report ONLY
/// while running.
pub fn dbsr(level: u32, out: &dyn Fn(&str)) {
    render(&registry().lock().unwrap(), level, out);
}

/// [`dbsr`] without the lock.
fn render(reg: &Registry, level: u32, out: &dyn Fn(&str)) {
    if reg.servers.is_empty() {
        out("No server layers registered with IOC");
        return;
    }
    out(&format!("Server state: {}", reg.state.name()));
    for srv in &reg.servers {
        out(&format!("Server '{}'", srv.name));
        if reg.state == ServerState::Running {
            if let Some(report) = &srv.report {
                report(level, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env matcher is C's whitespace-bounded one, not `contains`.
    #[test]
    fn ignore_servers_matches_whole_names_only() {
        // SAFETY: single-threaded test process for this variable; the two
        // cases below are the boundaries C's strstr walk exists to separate.
        unsafe { std::env::set_var("EPICS_IOC_IGNORE_SERVERS", "qsrv2") };
        assert!(ignored_by_env("qsrv2"));
        assert!(!ignored_by_env("qsrv"), "a prefix must not be suppressed");
        assert!(!ignored_by_env("rsrv"));
        unsafe { std::env::set_var("EPICS_IOC_IGNORE_SERVERS", "rsrv qsrv2") };
        assert!(ignored_by_env("rsrv"));
        assert!(ignored_by_env("qsrv2"));
        unsafe { std::env::remove_var("EPICS_IOC_IGNORE_SERVERS") };
        assert!(!ignored_by_env("rsrv"));
    }

    /// The whole of C's `dbsr` with nothing registered — one line, and NOT
    /// the state line (`dbServer.c:99-102` returns before printing it).
    #[test]
    fn dbsr_with_no_layers_is_one_line() {
        let out = std::cell::RefCell::new(Vec::<String>::new());
        let reg = Registry {
            servers: Vec::new(),
            state: ServerState::Running,
        };
        render(&reg, 0, &|s: &str| out.borrow_mut().push(s.to_string()));
        assert_eq!(*out.borrow(), vec!["No server layers registered with IOC"]);
    }

    /// BOUNDARY: `paused`. C prints the phase from `stateNames[]`
    /// (`dbServer.c:26-27`) and suppresses the layer report in every phase
    /// but `running` (`:107`), so a paused IOC's `dbsr` names the phase and
    /// stops there. Reachable since `iocPause` exists.
    #[test]
    fn dbsr_names_the_paused_phase_and_suppresses_the_report() {
        let out = std::cell::RefCell::new(Vec::<String>::new());
        let reg = Registry {
            servers: vec![DbServer {
                name: "rsrv",
                report: Some(Box::new(|_level, o: &dyn Fn(&str)| {
                    o("Channel Access Server");
                })),
            }],
            state: ServerState::Paused,
        };
        render(&reg, 0, &|s: &str| out.borrow_mut().push(s.to_string()));
        assert_eq!(
            *out.borrow(),
            vec!["Server state: paused", "Server 'rsrv'"],
            "C prints no report outside the running phase"
        );
    }

    /// State line, one `Server '<name>'` per layer, report only while
    /// running — C `dbServer.c:100-111`.
    #[test]
    fn dbsr_prints_the_state_then_each_layer() {
        let out = std::cell::RefCell::new(Vec::<String>::new());
        let sink = |s: &str| out.borrow_mut().push(s.to_string());

        let mut reg = Registry {
            servers: vec![DbServer {
                name: "rsrv",
                report: Some(Box::new(|level, o: &dyn Fn(&str)| {
                    o("Channel Access Server");
                    if level >= 1 {
                        o("    detail");
                    }
                })),
            }],
            state: ServerState::Registering,
        };
        render(&reg, 0, &sink);
        assert_eq!(
            *out.borrow(),
            vec!["Server state: registering", "Server 'rsrv'"],
            "a layer that is not running yet contributes no report"
        );

        out.borrow_mut().clear();
        reg.state = ServerState::Running;
        render(&reg, 0, &sink);
        assert_eq!(
            *out.borrow(),
            vec![
                "Server state: running",
                "Server 'rsrv'",
                "Channel Access Server"
            ]
        );

        out.borrow_mut().clear();
        render(&reg, 1, &sink);
        assert_eq!(
            *out.borrow(),
            vec![
                "Server state: running",
                "Server 'rsrv'",
                "Channel Access Server",
                "    detail"
            ],
            "the interest level reaches the layer's own report"
        );
    }

    /// A name with a space is refused, as is a duplicate — and the phase gate
    /// closes registration once the servers are running.
    #[test]
    fn registration_gates() {
        let mut reg = Registry {
            servers: Vec::new(),
            state: ServerState::Registering,
        };
        assert!(admit(
            &mut reg,
            DbServer {
                name: "rsrv",
                report: None
            }
        ));
        assert!(
            !admit(
                &mut reg,
                DbServer {
                    name: "rsrv",
                    report: None
                }
            ),
            "a duplicate name is refused"
        );
        assert!(
            !admit(
                &mut reg,
                DbServer {
                    name: "two words",
                    report: None
                }
            ),
            "a name with a space is refused"
        );
        reg.state = ServerState::Running;
        assert!(
            !admit(
                &mut reg,
                DbServer {
                    name: "qsrv2",
                    report: None
                }
            ),
            "registration closes once the set is running"
        );
    }
}
