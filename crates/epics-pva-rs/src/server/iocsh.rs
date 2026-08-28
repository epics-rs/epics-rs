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
//! instant its listeners bind. `PvaServer::run_with_shell` and
//! `run_with_source_and_shell` still register the same command on the
//! interactive shell, which is the half a registrar at the head cannot
//! reach for a caller that has no `IocApplication` at all.

use std::sync::{OnceLock, RwLock};

use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use crate::server_native::ServerReportHandle;

/// pvxs's `server()` singleton as this port can spell it: `None` until the
/// native server's listeners bind, which is the state `pvxsr` is registered
/// in and must answer from.
fn report_cell() -> &'static RwLock<Option<ServerReportHandle>> {
    static CELL: OnceLock<RwLock<Option<ServerReportHandle>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(None))
}

/// The single writer — pvxs `initialisePvxsServer` filling the singleton the
/// already-registered `pvxsr` reads. Called from the one place that has the
/// handle and knows the listeners are up
/// (`server_native::runtime::run_pva_server_reporting`'s bind callback).
/// Last write wins: a process that stands up a second PVA server has
/// replaced the first, which is the only state pvxs can be in at all.
pub fn publish_pvxs_report(handle: ServerReportHandle) {
    *report_cell().write().unwrap() = Some(handle);
}

/// pvxs `pvxsBaseRegistrar`'s `pvxsr` registration
/// (`ioc/iochooks.cpp:473-476`) applied to an [`IocApplication`]: the
/// command exists before the startup script, as it does in pvxs, because
/// pvxs registers it out of the `.dbd` expansion and not out of a running
/// server.
///
/// Measured on `scope_ioc` before this existed: `pvxsr` on the first
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
/// listeners bind. Before that — which is the whole startup script, since
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

            // Snapshot through the shared handle. Borrow is released at the
            // end of the match arm so the report is owned independently of
            // the watch lock.
            let report = match report_cell().read().unwrap().as_ref() {
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
                            ctx.println(&format!("    credentials: {account}/{method}"));
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
    /// shell that runs `st.cmd`. Measured on `scope_ioc` before this fix:
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
