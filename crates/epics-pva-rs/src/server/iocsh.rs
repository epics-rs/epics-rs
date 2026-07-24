//! iocsh commands for the pvAccess server — currently `pvxsr`.
//!
//! Mirrors pvxs `pvxsr(int detail)` (`ioc/iochooks.cpp:188-195`), which
//! streams the running `server()` through `operator<<(Server)` at the
//! raw detail level (`src/server.cpp:281-379`). The detail thresholds
//! follow that operator exactly:
//!   - **always**: the server-config summary (pvxs prints `config()` plus
//!     the registered-source list, `server.cpp:289-305`);
//!   - **`detail >= 2`**: the per-connection block (`server.cpp:308`
//!     `if(detail<2) return`), one line per peer with its auth *method*
//!     (`server.cpp:335-338`);
//!   - **`detail >= 3`**: the full per-peer credentials (`server.cpp:339`
//!     `if(detail>2)`) and the per-channel tx/rx listing
//!     (`server.cpp:342` `if(detail<=2) continue`).
//!
//! `ServerReport` does not model pvxs's env dump, registered-source list,
//! or per-peer backlog/state, so the config level here is the
//! bound-endpoint summary and those pvxs lines are omitted.
//!
//! Wiring is automatic — [`crate::server::pva_server::PvaServer::run_with_shell`] and
//! [`crate::server::pva_server::PvaServer::run_with_source_and_shell`] register `pvxsr` themselves
//! (the way they already register the autosave commands), fed by the
//! [`ServerReportHandle`] the native server publishes once it has bound.

use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};
use tokio::sync::watch;

use crate::server_native::ServerReportHandle;

/// `pvxsr [<detail>]` — report the running pvAccess server.
///
/// Detail thresholds track pvxs `operator<<(Server)`
/// (`src/server.cpp:281-379`), which `pvxsr` invokes with the raw detail
/// (`iochooks.cpp:188-195`): `detail < 2` prints only the server-config
/// summary; `detail >= 2` adds one line per live connection (channels,
/// byte counters, auth method); `detail >= 3` adds the full credentials
/// and each channel's tx/rx bytes.
///
/// `report_rx` carries the server's [`ServerReportHandle`], published the
/// moment the listeners bind. In the brief startup window before that the
/// command reports the server as not yet started.
pub fn pvxsr_command(report_rx: watch::Receiver<Option<ServerReportHandle>>) -> CommandDef {
    CommandDef::new(
        "pvxsr",
        vec![ArgDesc {
            name: "detail",
            arg_type: ArgType::Int,
            optional: true,
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
            let report = match report_rx.borrow().as_ref() {
                Some(handle) => handle.report(),
                None => {
                    ctx.println("pvAccess server: not yet started");
                    return Ok(CommandOutcome::Continue);
                }
            };

            let updown = |alive: bool| if alive { "up" } else { "down" };

            // Config-level summary — pvxs prints config()+source list at
            // every level (server.cpp:289-305); this is the modeled subset.
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
            // (server.cpp:308 `if(detail<2) return`). The connection count
            // and peer lines are part of this block, not the config level.
            if detail >= 2 {
                ctx.println(&format!("{} active connection(s)", report.peer_count));
                for (addr, peer) in &report.peers {
                    // Peer line carries only the auth *method* at this level
                    // (server.cpp:338 `auth=<method>`); the account is part
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
                    // both at detail > 2 (server.cpp:339 full cred,
                    // server.cpp:342 per-channel).
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
        let (_tx, rx) = watch::channel::<Option<ServerReportHandle>>(None);
        let cmd = pvxsr_command(rx);
        assert_eq!(cmd.name, "pvxsr");
        assert_eq!(cmd.args.len(), 1, "one optional detail arg");
        assert!(cmd.args[0].optional);
    }
}
