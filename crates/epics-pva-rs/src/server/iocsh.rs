//! iocsh commands for the pvAccess server — currently `pvxsr`.
//!
//! Mirrors pvxs `pvxsr(int detail)` (`ioc/iochooks.cpp:188-195`), which
//! streams the running `server()` through `operator<<(Server)` at the
//! raw detail level (`src/server.cpp:281-379`). The detail thresholds
//! follow that operator exactly:
//!   - **always**: the server-config summary (pvxs prints `config()` plus
//!     the registered-source list, `src/server.cpp:289-305`);
//!   - **`detail >= 2`**: the per-connection block (`src/server.cpp:308`
//!     `if(detail<2) return`), one line per peer with its auth *method*
//!     (`src/server.cpp:335-338`);
//!   - **`detail >= 3`**: the full per-peer credentials (`src/server.cpp:339`
//!     `if(detail>2)`) and the per-channel tx/rx listing
//!     (`src/server.cpp:342` `if(detail<=2) continue`).
//!
//! `ServerReport` does not model pvxs's env dump, registered-source list,
//! or per-peer backlog/state, so the config level here is the
//! bound-endpoint summary and those pvxs lines are omitted.
//!
//! Wiring is pvxs's, and pvxs's is a registrar: `pvxsBaseRegistrar`
//! (`ioc/iochooks.cpp:461-476`, exported with `epicsExportRegistrar`) runs
//! while `dbLoadDatabase` expands the `.dbd`, so `pvxsr` is a known command
//! from the first `st.cmd` line, long before a server exists. It answers for
//! the absent server rather than erroring — `pvxsr` guards its whole body
//! with `if (auto srv = server())` (`iochooks.cpp:188-193`).
//!
//! [`register_pvxs_commands`] is that registrar for an
//! [`epics_base_rs::server::ioc_app::IocApplication`]; the native server
//! publishes what the command reads through [`publish_pvxs_report`] the
//! instant it starts serving. `PvaServer::run_with_shell` and
//! `run_with_source_and_shell` still register the same command on the
//! interactive shell, which is the half a registrar at the head cannot
//! reach for a caller that has no `IocApplication` at all.

use std::sync::{OnceLock, RwLock};

use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use crate::server_native::{BoundPvaServer, PvaServerConfig, ServerReportHandle};

/// pvxs's `server()` singleton as this port can spell it: `None` until the
/// native server starts, which is the state `pvxsr` is registered
/// in and must answer from.
fn report_cell() -> &'static RwLock<Option<ServerReportHandle>> {
    static CELL: OnceLock<RwLock<Option<ServerReportHandle>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(None))
}

/// The single writer — pvxs `initialisePvxsServer` filling the singleton the
/// already-registered `pvxsr` reads. Called from the one place that has the
/// handle and knows the server is serving
/// (`server_native::runtime::serve_reporting`'s start callback).
/// Last write wins: a process that stands up a second PVA server has
/// replaced the first, which is the only state pvxs can be in at all.
pub fn publish_pvxs_report(handle: ServerReportHandle) {
    *report_cell().write().unwrap() = Some(handle);
}

/// The singleton's only reader — pvxs's `if (auto srv = server())`, which
/// guards every hook that needs the running server (`pvxsr`
/// (`iochooks.cpp:188`), `qReport` and `qStats`
/// (`singlesourcehooks.cpp:76`, `:88`)). `None` until the server starts.
///
/// Returns the handle by clone so no caller holds the lock across its report.
pub fn pvxs_report() -> Option<ServerReportHandle> {
    report_cell().read().unwrap().clone()
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
}

/// The name [`pvxs_base_registrar`] hands its server to the runner under.
pub const PVXS_SERVER: &str = "pvxs";

/// What `pvxsBaseRegistrar` left in pvxs's `server()` singleton: the server it
/// bound, or nothing when the bind threw and the IOC runs without pvAccess.
/// The runner takes it from [`IocRunConfig::servers`](epics_base_rs::server::ioc_app::IocRunConfig::servers)
/// through `PvaServer::bound_at_registrar`.
pub struct PvxsServer(pub(crate) Option<BoundPvaServer>);

/// pvxs `pvxsBaseRegistrar` (`ioc/iochooks.cpp:461-511`): the commands, then
/// `initialisePvxsServer`, which binds the server from the environment and
/// publishes its TCP port before the startup script runs. pvxs starts it at
/// `initHookAfterIocRunning` (`:374-379`), which here is the runner. A bind
/// that fails prints pvxs's catch line and leaves the IOC without pvAccess.
///
/// Must be called on the IOC's runtime, which the bound sockets register with.
pub fn pvxs_base_registrar(app: IocApplication) -> IocApplication {
    let app = register_pvxs_commands(app);
    let bound = match crate::server_native::PvaServer::bind(PvaServerConfig::default().with_env()) {
        Ok(bound) => {
            publish_server_port(bound.bound_tcp_port());
            Some(bound)
        }
        Err(e) => {
            eprintln!("Error in pvxsBaseRegistrar : {}", pvxs_what(&e));
            None
        }
    };
    app.register_server_init(PVXS_SERVER, move |_| async move {
        Ok::<_, epics_base_rs::error::CaError>(PvxsServer(bound))
    })
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
/// (`iochooks.cpp:188-195`): `detail < 2` prints only the server-config
/// summary; `detail >= 2` adds one line per live connection (channels,
/// byte counters, auth method); `detail >= 3` adds the full credentials
/// and each channel's tx/rx bytes.
///
/// The handle is read through [`publish_pvxs_report`], filled the moment the
/// server starts. Before that — which is the whole startup script, since
/// the protocol runner is Phase 3 — the command reports the server as not
/// yet started. pvxs prints nothing at all there; the line is kept because
/// an operator who typed `pvxsr` is better served by being told why the
/// report is empty than by silence.
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

            let report = match pvxs_report() {
                Some(handle) => handle.report(),
                None => {
                    ctx.println("pvAccess server: not yet started");
                    return Ok(CommandOutcome::Continue);
                }
            };

            let updown = |alive: bool| if alive { "up" } else { "down" };

            // Config-level summary — pvxs prints config()+source list at
            // every level (src/server.cpp:289-305); this is the modeled subset.
            ctx.println(&format!(
                "pvAccess server: tcp :{} ({}), udp :{} ({}), beacon period {}s",
                report.tcp_port,
                updown(report.tcp_alive),
                report.udp_port,
                updown(report.udp_alive),
                report.beacon_period_secs,
            ));
            if report.tls_enabled {
                ctx.println(&format!("    tls :{} (enabled)", report.tls_port));
            }
            if report.udp_v6_alive {
                ctx.println("    udp/IPv6: up");
            }
            if report.ignore_addrs > 0 {
                ctx.println(&format!("    ignoring {} address(es)", report.ignore_addrs));
            }

            // Per-connection block — pvxs gates this at detail >= 2
            // (src/server.cpp:308 `if(detail<2) return`). The connection count
            // and peer lines are part of this block, not the config level.
            if detail >= 2 {
                ctx.println(&format!("{} active connection(s)", report.peer_count));
                for (addr, peer) in &report.peers {
                    // Peer line carries only the auth *method* at this level
                    // (src/server.cpp:338 `auth=<method>`); the account is part
                    // of the full credential dump gated below.
                    let method = peer
                        .credentials
                        .as_ref()
                        .map(|(_account, method)| method.as_str())
                        .unwrap_or("unvalidated");
                    ctx.println(&format!(
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
                            ctx.println_bytes(
                                &[
                                    b"    credentials: ".as_slice(),
                                    account.as_bytes(),
                                    b"/",
                                    method.as_bytes(),
                                ]
                                .concat(),
                            );
                        }
                        for ch in &peer.channels_detail {
                            ctx.println(&format!("      {} tx={} rx={}", ch.name, ch.tx, ch.rx));
                        }
                    }
                }
            }

            Ok(CommandOutcome::Continue)
        },
    )
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
}
