//! iocsh commands for the pvAccess server — currently `pvxsr`.
//!
//! Mirrors pvxs `pvxsr(int detail)` (`ioc/iochooks.cpp:188-195`), which
//! streams the running `server()` through `operator<<(Server)` at the
//! raw detail level (`src/server.cpp:281-379`). The detail thresholds
//! follow that operator exactly:
//!   - **always**: `config()`'s `EPICS_PVAS_*` lines and the source list
//!     (`src/server.cpp:289-305`);
//!   - **`detail >= 2`**: the `State:` line and the per-connection block
//!     (`src/server.cpp:308` `if(detail<2) return`), one line per peer
//!     with its auth *method* (`src/server.cpp:335-338`);
//!   - **`detail >= 3`**: the full per-peer credentials (`src/server.cpp:339`
//!     `if(detail>2)`) and the per-channel tx/rx listing
//!     (`src/server.cpp:342` `if(detail<=2) continue`).
//!
//! The peer and channel lines are this port's: `ServerReport` does not
//! model pvxs's per-peer backlog or per-operation state.
//!
//! Wiring is pvxs's, and pvxs's is a registrar: `pvxsBaseRegistrar`
//! (`ioc/iochooks.cpp:461-476`, exported with `epicsExportRegistrar`) runs
//! while `dbLoadDatabase` expands the `.dbd`, so `pvxsr` is a known command
//! from the first `st.cmd` line, long before a server exists. It answers for
//! the absent server rather than erroring — `pvxsr` guards its whole body
//! with `if (auto srv = server())` (`iochooks.cpp:188-193`).
//!
//! [`register_pvxs_commands`] is that registrar for an
//! [`epics_base_rs::server::ioc_app::IocApplication`]; the registrar
//! publishes the bound server through [`publish_pvxs_report`], and the
//! native server publishes again the instant it starts serving.
//! `PvaServer::run_with_shell` and
//! `run_with_source_and_shell` still register the same command on the
//! interactive shell, which is the half a registrar at the head cannot
//! reach for a caller that has no `IocApplication` at all.

use std::sync::{OnceLock, RwLock};

use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use crate::server_native::PvaServerConfig;
use crate::server_native::ServerReportHandle;
use crate::server_native::SharedPV;

/// pvxs's `server()` singleton as this port can spell it: `None` until a
/// server is bound, which is the state `pvxsr` is registered in.
fn report_cell() -> &'static RwLock<Option<ServerReportHandle>> {
    static CELL: OnceLock<RwLock<Option<ServerReportHandle>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(None))
}

/// The single writer — pvxs `initialisePvxsServer` filling the singleton the
/// already-registered `pvxsr` reads: [`pvxs_base_registrar`] with the bound
/// server, then `server_native::runtime::serve_reporting`'s start callback
/// with the started one. Last write wins: a process that stands up a second
/// PVA server has replaced the first, which is the only state pvxs can be in
/// at all.
pub fn publish_pvxs_report(handle: ServerReportHandle) {
    *report_cell().write().unwrap() = Some(handle);
}

/// The singleton's only reader — pvxs's `if (auto srv = server())`, which
/// guards every hook that needs the running server (`pvxsr`
/// (`iochooks.cpp:188`), `qReport` and `qStats`
/// (`singlesourcehooks.cpp:76`, `:88`)). `None` until a server is bound.
///
/// Returns the handle by clone so no caller holds the lock across its report.
pub fn pvxs_report() -> Option<ServerReportHandle> {
    report_cell().read().unwrap().clone()
}

/// pvxs `ioc::server().addPV(name, pv)` (`iochooks.cpp:80-87`,
/// `server.cpp:174-180`): `pv` joins the singleton's `__builtin` store,
/// before `start` as after.
pub fn pvxs_add_pv(name: &str, pv: SharedPV) -> Result<(), AddPvRefused> {
    let handle = pvxs_report().ok_or(AddPvRefused::NoInstance)?;
    (handle.builtin().try_add(name, pv)).map_err(|_| AddPvRefused::Duplicate)
}

/// Why [`pvxs_add_pv`] refused; `Display` is the `what()` pvxs throws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AddPvRefused {
    /// No server bound (`iochooks.cpp:86`).
    #[error("No Instance")]
    NoInstance,
    /// The name is in `__builtin` already (`sharedpv.cpp:588`).
    #[error("add() will not create duplicate PV")]
    Duplicate,
}

/// pvxs `pvxsBaseRegistrar`'s `pvxsr` registration
/// (`ioc/iochooks.cpp:473-476`) applied to an [`IocApplication`]: the
/// command exists before the startup script, as it does in pvxs, because
/// pvxs registers it out of the `.dbd` expansion and not out of a running
/// server.
///
/// Measured on `scope-ioc` before this existed: `pvxsr` on the first
/// `st.cmd` line was `ERROR st.cmd line 1: Command 'pvxsr' not registered.`,
/// which aborts the script.
///
/// Both shells, as pvxs's single command table is read by `st.cmd` and the
/// prompt alike. `PvaServer::run_with_shell` also registers it, for the
/// callers that never build an `IocApplication`; the two copies cannot
/// disagree, because neither captures anything and both read
/// [`publish_pvxs_report`].
pub fn register_pvxs_commands(app: IocApplication) -> IocApplication {
    app.register_startup_command(pvxsr_command())
        .register_shell_command(pvxsr_command())
        .register_startup_command(pvxs_log_config_command())
        .register_shell_command(pvxs_log_config_command())
        .register_startup_command(pvxs_log_clear_command())
        .register_shell_command(pvxs_log_clear_command())
}

/// `pvxs_log_config "KEY=VAL,…"` (`ioc/iochooks.cpp:478-480`).
fn pvxs_log_config_command() -> CommandDef {
    CommandDef::new(
        "pvxs_log_config",
        vec![ArgDesc {
            name: "KEY=VAL,KEY=VAL,...",
            arg_type: ArgType::String,
        }],
        "Append logger configuration.  eg. pvxs_log_config \"pvxs.*=DEBUG\"",
        |args: &[ArgValue], _: &CommandContext| {
            if let Some(ArgValue::String(config)) = args.first() {
                crate::pvxs_log::logger_config_str(&String::from_utf8_lossy(config.as_bytes()));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `pvxs_log_clear` (`ioc/iochooks.cpp:481-483`).
fn pvxs_log_clear_command() -> CommandDef {
    CommandDef::new(
        "pvxs_log_clear",
        vec![],
        "Reset logger configuration to defaults",
        |_: &[ArgValue], _: &CommandContext| {
            crate::pvxs_log::logger_level_clear();
            Ok(CommandOutcome::Continue)
        },
    )
}

/// The name [`pvxs_base_registrar`] hands its server to the runner under.
pub const PVXS_SERVER: &str = "pvxs";

/// What `pvxsBaseRegistrar` left in pvxs's `server()` singleton: the server it
/// bound, or nothing when the bind threw and the IOC runs without pvAccess.
/// The runner takes it from [`IocRunConfig::servers`](epics_base_rs::server::ioc_app::IocRunConfig::servers)
/// through `PvaServer::bound_at_registrar`.
pub struct PvxsServer(pub(crate) Option<BoundPvxsServer>);

/// A PVA server bound and not yet serving, on this backend.
#[cfg(tokio_backend)]
pub(crate) type BoundPvxsServer = crate::server_native::BoundPvaServer;
#[cfg(exec_backend)]
pub(crate) type BoundPvxsServer = crate::server_native::serving::PvaSockets;

/// Bind a PVA server for an IOC on `config`, a held search port included.
pub(crate) fn bind_pvxs_server(
    config: PvaServerConfig,
) -> crate::error::PvaResult<BoundPvxsServer> {
    #[cfg(tokio_backend)]
    return crate::server_native::PvaServer::bind(config);
    #[cfg(exec_backend)]
    crate::server_native::serving::PvaSockets::bind_ioc(config)
}

/// pvxs `pvxsBaseRegistrar` (`ioc/iochooks.cpp:461-511`): the commands, then
/// `initialisePvxsServer`, which binds the server from the environment and
/// publishes its TCP port before the startup script runs. pvxs starts it at
/// `initHookAfterIocRunning` (`:374-379`), which here is the runner. A bind
/// that fails prints pvxs's catch line and leaves the IOC without pvAccess.
///
/// Must be called on the IOC's runtime, which the bound sockets register with.
pub fn pvxs_base_registrar(app: IocApplication) -> IocApplication {
    pvxs_base_registrar_with_enable2(app, || ())
}

/// [`pvxs_base_registrar`] with QSRV's `enable2()` decision run where pvxs
/// runs it: after `logger_config_env()`, before the commands and the bind
/// (`ioc/iochooks.cpp:461-476`).
pub fn pvxs_base_registrar_with_enable2(
    app: IocApplication,
    enable2: impl FnOnce(),
) -> IocApplication {
    crate::pvxs_log::logger_config_env();
    enable2();
    let app = register_pvxs_commands(app);
    let config = PvaServerConfig::default().with_env();
    let bound = match config.map_err(Into::into).and_then(bind_pvxs_server) {
        Ok(bound) => {
            publish_server_port(bound.bound_tcp_port());
            publish_pvxs_report(bound.report_handle());
            register_pvxs_init_hook();
            Some(bound)
        }
        Err(e) => {
            epics_libcom_rs::stderr_println!("Error in pvxsBaseRegistrar : {}", pvxs_what(&e));
            None
        }
    };
    app.register_server_init(PVXS_SERVER, move |_| async move {
        Ok::<_, epics_base_rs::error::CaError>(PvxsServer(bound))
    })
}

/// pvxs `initHookRegister(&pvxsInitHook)` (`ioc/iochooks.cpp:501`), which a
/// failed bind's throw skips. C dedups the registration by function pointer;
/// a closure cannot be compared, so a second registrar call is a no-op here.
fn register_pvxs_init_hook() {
    use epics_base_rs::server::ioc_app::{InitHookState, init_hook_register};
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        init_hook_register(std::sync::Arc::new(|state| {
            if matches!(state, InitHookState::AtBeginning) {
                register_qsrv2_db_server();
            }
        }));
    });
}

/// pvxs `dbRegisterQSRV2` (`ioc/singlesourcehooks.cpp:138-154`): QSRV's
/// `dbServer`, whose `client` names the PVA peer behind a put.
pub fn register_qsrv2_db_server() {
    use epics_base_rs::server::db_server::{DbServer, db_register_server};
    db_register_server(DbServer {
        name: "qsrv2",
        report: Some(Box::new(|level, out: &dyn Fn(&str)| {
            report_server(level as i64, &|line| out(&String::from_utf8_lossy(line)))
        })),
        client: Some(super::current_op::q_client),
    });
}

/// pvxs's `e.what()` for a failed server: a bind throws `std::system_error`,
/// whose text is `strerror` alone, without Rust's ` (os error N)`.
pub(crate) fn pvxs_what(e: &crate::error::PvaError) -> String {
    match e {
        crate::error::PvaError::Io(e) => epics_base_rs::net::sock_err_string(e),
        e => e.to_string(),
    }
}

/// pvxs `initialisePvxsServer` publishing the effective TCP port: recsync and
/// any later `$(PVAS_SERVER_PORT)` expansion in `st.cmd` read it from there.
/// The bound port, because an ephemeral or fallen-back bind is exactly when a
/// reader cannot derive it. The second name is pvAccessCPP's, which pvxs sets
/// too so either stack answers.
pub(crate) fn publish_server_port(port: u16) {
    let portstr = port.to_string();
    for name in ["PVXS_SERVER_PORT", "PVAS_SERVER_PORT"] {
        epics_base_rs::runtime::env::set(name, &portstr);
    }
}

/// `pvxsr [<detail>]` — report the running pvAccess server.
///
/// Detail thresholds track pvxs `operator<<(Server)`
/// (`src/server.cpp:281-379`), which `pvxsr` invokes with the raw detail
/// (`iochooks.cpp:188-195`): `detail < 2` prints `config()` and the source
/// list; `detail >= 2` adds the `State:` line and one line per live
/// connection (channels, byte counters, auth method); `detail >= 3` adds
/// the full credentials and each channel's tx/rx bytes.
///
/// From [`pvxs_base_registrar`] on, the server is bound and `Stopped`, so
/// `pvxsr` in `st.cmd` reports it; with no server, as after a failed bind,
/// it prints nothing.
pub fn pvxsr_command() -> CommandDef {
    CommandDef::new(
        "pvxsr",
        vec![ArgDesc {
            name: "detail",
            arg_type: ArgType::Int,
        }],
        "pvxsr [<detail>]",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let detail = match args.first() {
                Some(ArgValue::Int(n)) => *n,
                _ => 0,
            };

            report_server(detail, &|line| ctx.println_bytes(line));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// pvxs `operator<<(Server)` (`src/server.cpp:281-379`) at `detail`, one line
/// per `out` call: what `pvxsr` prints, and QSRV2's `dbServer.report`
/// (`qReport`, `ioc/singlesourcehooks.cpp:75-86`) under `dbsr`. Both print
/// nothing without a server.
fn report_server(detail: i64, out: &dyn Fn(&[u8])) {
    let Some(handle) = pvxs_report() else {
        return;
    };
    let line = |s: &str| out(s.as_bytes());
    for config in handle.pvxs_config().lines() {
        line(config);
    }
    // From detail 1 each source's `show` follows its `prio=` under one
    // `Indented` (`src/server.cpp:297-302`).
    if detail > 0 {
        for (name, order, show) in handle.source_shows() {
            line(&format!("Source: {name} prio={order} {}", show.kind));
            let indent: &[u8] = if show.nested { b"        " } else { b"    " };
            let mut names = show.names;
            names.sort();
            names.dedup();
            for pv in names {
                out(&[indent, pv.as_bytes()].concat());
            }
        }
    } else {
        for (name, order) in handle.source_list() {
            line(&format!("Source: {name} prio={order} "));
        }
    }

    // pvxs's `if(detail<2) return` (src/server.cpp:308).
    if detail >= 2 {
        let report = handle.report();
        line(&format!(
            "State: {} TCP_Port: {}",
            if handle.is_running() {
                "Running"
            } else {
                "Stopped"
            },
            report.tcp_port,
        ));
        for (addr, peer) in &report.peers {
            // Peer line carries only the auth *method* at this level
            // (src/server.cpp:338 `auth=<method>`); the account is part
            // of the full credential dump gated below.
            let method = peer
                .credentials
                .as_ref()
                .map(|(_account, method)| method.as_str())
                .unwrap_or("unvalidated");
            line(&format!(
                "  peer {addr}: {} channel(s), bytes in={} out={}, auth={}{}",
                peer.channels,
                peer.bytes_in,
                peer.bytes_out,
                method,
                if peer.tls { ", tls" } else { "" },
            ));
            // Full credentials + per-channel listing — pvxs gates
            // both at detail > 2 (src/server.cpp:339 full cred,
            // src/server.cpp:342 per-channel).
            if detail >= 3 {
                if let Some((account, method)) = &peer.credentials {
                    // pvxs streams `cred->account` as its bytes.
                    out(&[
                        b"    credentials: ".as_slice(),
                        account.as_bytes(),
                        b"/",
                        method.as_bytes(),
                    ]
                    .concat());
                }
                for ch in &peer.channels_detail {
                    line(&format!("      {} tx={} rx={}", ch.name, ch.tx, ch.rx));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pvxsr_command_is_named_with_optional_detail() {
        let cmd = pvxsr_command();
        assert_eq!(cmd.name, "pvxsr");
        assert_eq!(cmd.args.len(), 1, "one optional detail arg");
    }

    /// pvxs registers `pvxsr` from `pvxsBaseRegistrar`, so it is on the
    /// shell that runs `st.cmd`. Measured on `scope-ioc` before this fix:
    /// `ERROR st.cmd line 1: Command 'pvxsr' not registered.`
    #[test]
    fn pvxsr_is_registered_before_the_startup_script_runs() {
        let app = register_pvxs_commands(IocApplication::new());
        assert!(
            app.startup_commands().iter().any(|c| c.name == "pvxsr"),
            "`pvxsr` must be on the startup shell"
        );
    }

    /// The base `Source::show` names sit one indent deeper than the IOC
    /// sources' (`src/server.cpp:841`).
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pvxsr_1_nests_the_names_of_a_plain_source() {
        assert_eq!(
            pvxsr_1_sources(),
            [
                "Source: __builtin prio=-1 StaticProvider",
                "Source: __server prio=-1 Source",
                "Source: __user prio=0 Source",
                "        A",
                "        B",
            ]
        );
    }

    /// pvxs `onList` is a plain call (`src/server.cpp:837-847`), so the
    /// names print on a thread that may not block on the runtime — here the
    /// only thread of the runtime, busy running the command.
    #[cfg(tokio_backend)]
    #[tokio::test(flavor = "current_thread")]
    async fn pvxsr_1_lists_names_on_the_runtime_s_only_thread() {
        assert_eq!(
            &pvxsr_1_sources()[2..],
            ["Source: __user prio=0 Source", "        A", "        B"]
        );
    }

    /// `pvxsr 1`'s `Source:` lines and the names under them, for a server
    /// on a plain source serving `A` and `B`.
    #[cfg(tokio_backend)]
    fn pvxsr_1_sources() -> Vec<String> {
        use crate::pvdata::{FieldDesc, PvField};
        use crate::server_native::{ChannelSource, MonitorStream, OpError, PvaServer};
        use epics_base_rs::types::RecordName;

        struct Plain;
        impl ChannelSource for Plain {
            fn list_pvs(&self) -> Vec<RecordName> {
                vec!["B".into(), "A".into()]
            }
            async fn has_pv(&self, _: &[u8]) -> bool {
                false
            }
            async fn get_introspection(&self, _: &[u8]) -> Option<FieldDesc> {
                None
            }
            async fn get_value(&self, _: &[u8]) -> Option<PvField> {
                None
            }
            async fn put_value(&self, _: &[u8], _: PvField) -> Result<(), OpError> {
                Err("read-only".into())
            }
            async fn is_writable(&self, _: &[u8]) -> bool {
                false
            }
            async fn subscribe(&self, _: &[u8]) -> Option<MonitorStream<PvField>> {
                None
            }
        }

        let config = PvaServerConfig {
            tcp_port: 0,
            udp_port: 0,
            bind_ip: std::net::Ipv4Addr::LOCALHOST.into(),
            auto_beacon: false,
            beacon_destinations: Vec::new(),
            ..Default::default()
        };
        let server = PvaServer::start(std::sync::Arc::new(Plain), config).unwrap();
        publish_pvxs_report(server.report_handle());
        let lines = std::cell::RefCell::new(Vec::new());
        report_server(1, &|l| {
            lines
                .borrow_mut()
                .push(String::from_utf8_lossy(l).into_owned())
        });
        lines
            .into_inner()
            .into_iter()
            .filter(|l| l.starts_with("Source: ") || l.starts_with(' '))
            .collect()
    }
}
