//! iocsh commands for `calink` — `caxr`, `dbcaxr`.
//!
//! The CA-link counterpart of the bridge `pvalink::iocsh`. Mirrors C
//! `dbCa.c` debug surface: pre-warm a CA link so the synchronous
//! record-link resolver reads cached monitor values without a blocking
//! GET (`caxr`), and dump CA-link state for a record (`dbcaxr`).

use epics_base_rs::server::database::LinkSet;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::resolver::CaLinkResolver;

/// `caxr <pv_name>` — pre-open a CA link in monitor mode so the
/// resolver returns cached values for that PV without a blocking GET
/// on first access. The CA-link analogue of `pvxr`; mirrors the
/// C `dbCaAddLink` pre-warm done at `iocInit`.
pub fn ca_caxr_command(resolver: CaLinkResolver) -> CommandDef {
    CommandDef::new(
        "caxr",
        vec![ArgDesc {
            name: "pv_name",
            arg_type: ArgType::String,
            optional: false,
        }],
        "caxr <pv_name>",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let name = match args.first() {
                Some(ArgValue::String(s)) => s.clone(),
                _ => return Err("caxr: missing pv_name".into()),
            };
            let resolver = resolver.clone();
            let handle = ctx.runtime_handle().clone();
            let result = std::thread::spawn(move || {
                handle.block_on(async move { resolver.open(&name).await })
            })
            .join();
            match result {
                Ok(Ok(_link)) => {
                    ctx.println("caxr: opened (monitor active)");
                    Ok(CommandOutcome::Continue)
                }
                Ok(Err(e)) => Err(format!("caxr: open failed: {e}")),
                Err(_) => Err("caxr: panic in runtime thread".into()),
            }
        },
    )
}

/// `dbcaxr [<recordName>]` — print CA-link debug info. With no
/// argument prints resolver-level stats (open-link count); with a
/// record name walks every link-shaped String field on that record
/// and dumps connection / value / alarm / time state for each
/// `ca://...` (or bare ` CA`-modified) link via the registered
/// [`epics_base_rs::server::database::LinkSet`]. The CA-link
/// counterpart of `dbpvxr`.
pub fn db_dbcaxr_command(resolver: CaLinkResolver) -> CommandDef {
    CommandDef::new(
        "dbcaxr",
        vec![ArgDesc {
            name: "record",
            arg_type: ArgType::String,
            optional: true,
        }],
        "dbcaxr [<recordName>]",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let target = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            ctx.println(&format!(
                "dbcaxr: {} cached CA link(s)",
                resolver.link_count()
            ));
            if let Some(rec) = target {
                let db = ctx.db().clone();
                let handle = ctx.runtime_handle().clone();
                let rec_clone = rec.clone();
                let links = std::thread::spawn(move || {
                    handle.block_on(async move { db.record_link_fields(&rec_clone).await })
                })
                .join()
                .unwrap_or_default();
                if links.is_empty() {
                    ctx.println(&format!(
                        "  '{rec}': no link fields found (or record missing)"
                    ));
                } else {
                    ctx.println(&format!("  '{rec}': {} link field(s)", links.len()));
                    for (field, raw, parsed) in links {
                        if let epics_base_rs::server::record::ParsedLink::Ca(ca) = parsed {
                            let name = ca.pv;
                            // The lset is async; this iocsh command is sync, so
                            // its state is read through the same off-runtime
                            // thread the record-field lookup above uses.
                            let r = resolver.clone();
                            let n = name.clone();
                            let h = ctx.runtime_handle().clone();
                            let (connected, value, alarm, ts) = std::thread::spawn(move || {
                                h.block_on(async move {
                                    (
                                        <CaLinkResolver as LinkSet>::is_connected(&r, &n).await,
                                        <CaLinkResolver as LinkSet>::get_value(&r, &n).await,
                                        <CaLinkResolver as LinkSet>::alarm_severity(&r, &n).await,
                                        <CaLinkResolver as LinkSet>::time_stamp(&r, &n).await,
                                    )
                                })
                            })
                            .join()
                            .unwrap_or((false, None, None, None));
                            ctx.println(&format!(
                                "    {field}={raw:?}  ca://{name}  connected={connected}"
                            ));
                            if let Some(v) = value {
                                ctx.println(&format!("        value={v}"));
                            }
                            if let Some(sev) = alarm {
                                ctx.println(&format!("        alarmSeverity={sev}"));
                            }
                            if let Some((s, n, _)) = ts {
                                ctx.println(&format!("        timeStamp={s}.{n:09}"));
                            }
                        }
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Convenience: build the full `calink` iocsh command set bound to
/// `resolver`. Drop the result into
/// [`epics_base_rs::server::ioc_app::IocRunConfig::shell_commands`].
pub fn register_calink_commands(resolver: CaLinkResolver) -> Vec<CommandDef> {
    vec![
        ca_caxr_command(resolver.clone()),
        db_dbcaxr_command(resolver),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_resolver() -> CaLinkResolver {
        CaLinkResolver::new(tokio::runtime::Handle::current())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn register_calink_commands_returns_two() {
        let r = dummy_resolver();
        let cmds = register_calink_commands(r);
        assert_eq!(cmds.len(), 2);
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"caxr"));
        assert!(names.contains(&"dbcaxr"));
    }
}
