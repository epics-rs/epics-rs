//! iocsh commands for the pvAccess server — currently `pvxsr`.
//!
//! Mirrors pvxs `pvxsr(int detail)` (`ioc/iochooks.cpp:188-214`), which
//! streams the running `server()` with `Detailed(strm, detail)`: a
//! summary of the server's bound endpoints plus, at higher detail, the
//! live per-connection channel/byte counters.
//!
//! Wiring is automatic — [`PvaServer::run_with_shell`] and
//! [`PvaServer::run_with_source_and_shell`] register `pvxsr` themselves
//! (the way they already register the autosave commands), fed by the
//! [`ServerReportHandle`] the native server publishes once it has bound.

use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};
use tokio::sync::watch;

use crate::server_native::ServerReportHandle;

/// `pvxsr [<detail>]` — report the running pvAccess server.
///
/// With no argument prints the summary line (bound endpoints, beacon
/// period, active-connection count). `detail >= 1` adds one line per live
/// connection (channels, byte counters, validated credentials);
/// `detail >= 2` additionally lists each channel's name and tx/rx bytes.
/// Mirrors pvxs `pvxsr`'s `Detailed` levels (`ioc/iochooks.cpp:188-214`).
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

            ctx.println(&format!(
                "pvAccess server: {} active connection(s)",
                report.peer_count
            ));
            ctx.println(&format!(
                "    tcp :{} ({}), udp :{} ({}), beacon period {}s",
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

            if detail >= 1 {
                for (addr, peer) in &report.peers {
                    let creds = peer
                        .credentials
                        .as_ref()
                        .map(|(account, method)| format!("{account}/{method}"))
                        .unwrap_or_else(|| "unvalidated".to_string());
                    ctx.println(&format!(
                        "  peer {addr}: {} channel(s), bytes in={} out={}, {}{}",
                        peer.channels,
                        peer.bytes_in,
                        peer.bytes_out,
                        creds,
                        if peer.tls { ", tls" } else { "" },
                    ));
                    if detail >= 2 {
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
