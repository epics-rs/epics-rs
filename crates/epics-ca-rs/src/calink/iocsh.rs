//! iocsh commands for `calink` — `caxr`, `dbcaxr`.
//!
//! The CA-link counterpart of the bridge `pvalink::iocsh`. Mirrors C
//! `dbCa.c` debug surface: pre-warm a CA link so the synchronous
//! record-link resolver reads cached monitor values without a blocking
//! GET (`caxr`), and dump CA-link state for a record (`dbcaxr`).

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

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
///
/// `FLNK` is included, and must be: C `dbcar` walks
/// `pdbRecordType->link_ind[j]` — every link field of the record type, with
/// no `dbfType` filter — and prints each one whose `plink->type` is
/// `CA_LINK` (`dbCaTest.c:88-136`). An `FLNK="ca://OTHER.PROC"` is such a
/// link (`dbLink.c:118-136` reaches `dbCaAddLink` for `DBF_FWDLINK` too), so
/// hiding it here would under-report exactly the links `dbcar` exists to
/// show.
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
                let links = ctx.db().record_link_fields(&rec);
                if links.is_empty() {
                    ctx.println(&format!(
                        "  '{rec}': no link fields found (or record missing)"
                    ));
                } else {
                    ctx.println(&format!("  '{rec}': {} link field(s)", links.len()));
                    for (field, raw, parsed) in links {
                        if let epics_base_rs::server::record::ParsedLink::Ca(ca) = parsed {
                            let name = ca.pv;
                            // Only `get_value` is async now (it may open the
                            // link); the cached accessors are plain `fn` and
                            // are read inline. The off-runtime thread stays
                            // for the one remaining blocking call.
                            let r = resolver.clone();
                            let n = name.clone();
                            let h = ctx.runtime_handle().clone();
                            let connected = <CaLinkResolver as LinkSet>::is_connected(&r, &n);
                            let alarm = <CaLinkResolver as LinkSet>::alarm_severity(&r, &n);
                            let ts = <CaLinkResolver as LinkSet>::time_stamp(&r, &n);
                            let value = std::thread::spawn(move || {
                                h.block_on(async move {
                                    <CaLinkResolver as LinkSet>::get_value(&r, &n).await
                                })
                            })
                            .join()
                            .unwrap_or(None);
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
        CaLinkResolver::new()
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
