//! iocsh commands for pvalink — `pvxr`, `pvxrefdiff`, `dbpvxr`.
//!
//! Mirrors pvxs `ioc/pvalink.cpp` (`dbpvxr`, `pvxrefdiff`,
//! `testqsrvWaitForLinkConnected`). Pre-warms link entries so the
//! synchronous record-link resolver can read cached monitor values
//! without `block_on(GET)`.

use epics_base_rs::server::database::LinkSet;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::integration::PvaLinkResolver;

/// `pvxr <pv_name>` — pre-open a link in INP+monitor mode so the
/// resolver returns cached values for that PV without a blocking GET
/// on first access. Mirrors pvxs `pvalinkOpen` (pvalink_channel.cpp).
pub fn db_pvxr_command(resolver: PvaLinkResolver) -> CommandDef {
    CommandDef::new(
        "pvxr",
        vec![ArgDesc {
            name: "pv_name",
            arg_type: ArgType::String,
            optional: false,
        }],
        "pvxr <pv_name>",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let name = match args.first() {
                Some(ArgValue::String(s)) => s.clone(),
                _ => return Err("pvxr: missing pv_name".into()),
            };
            let resolver = resolver.clone();
            let handle = ctx.runtime_handle().clone();
            let result = std::thread::spawn(move || {
                handle.block_on(async move { resolver.open(&name).await })
            })
            .join();
            match result {
                Ok(Ok(_link)) => {
                    ctx.println("pvxr: opened (monitor active)");
                    Ok(CommandOutcome::Continue)
                }
                Ok(Err(e)) => Err(format!("pvxr: open failed: {e}")),
                Err(_) => Err("pvxr: panic in runtime thread".into()),
            }
        },
    )
}

/// `pvxrefdiff` — print "links touched since last call" delta.
/// Mirrors pvxs `pvxrefdiff` (iochooks.cpp:270). Uses interior counter
/// state on the [`PvaLinkResolver`] — the first call shows the
/// running total, subsequent calls show deltas vs. the previous call.
pub fn pvxrefdiff_command(resolver: PvaLinkResolver) -> CommandDef {
    let last = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    CommandDef::new(
        "pvxrefdiff",
        vec![],
        "pvxrefdiff",
        move |_args: &[ArgValue], ctx: &CommandContext| {
            let now = resolver.read_count();
            let prev = last.swap(now, std::sync::atomic::Ordering::Relaxed);
            let delta = now.wrapping_sub(prev);
            ctx.println(&format!(
                "pvxrefdiff: {delta} read(s) since last call (total {now}, {} cached link(s))",
                resolver.link_count()
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbpvxr <recordName>` — print pvalink debug info for the named
/// record. Mirrors pvxs `dbpvxr` (pvalink.cpp:185). With no
/// argument prints resolver-level stats; with a record name walks
/// every link-shaped String field on that record (INP/OUT/DOL/...)
/// and dumps connection / value / alarm / time state for each
/// `pva://...` or `ca://...` link via the registered
/// [`epics_base_rs::server::database::LinkSet`].
#[allow(unused_variables)]
pub fn dbpvxr_command(resolver: PvaLinkResolver) -> CommandDef {
    CommandDef::new(
        "dbpvxr",
        vec![ArgDesc {
            name: "record",
            arg_type: ArgType::String,
            optional: true,
        }],
        "dbpvxr [<recordName>]",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let target = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            ctx.println(&format!(
                "dbpvxr: {} cached link(s), {} total reads, enabled={}",
                resolver.link_count(),
                resolver.read_count(),
                resolver.is_enabled()
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
                        match parsed {
                            epics_base_rs::server::record::ParsedLink::Pva(name) => {
                                let connected =
                                    <PvaLinkResolver as LinkSet>::is_connected(&resolver, &name);
                                let value =
                                    <PvaLinkResolver as LinkSet>::get_value(&resolver, &name);
                                let alarm =
                                    <PvaLinkResolver as LinkSet>::alarm_message(&resolver, &name);
                                let ts = <PvaLinkResolver as LinkSet>::time_stamp(&resolver, &name);
                                ctx.println(&format!(
                                    "    {field}={raw:?}  pva://{name}  connected={connected}"
                                ));
                                if let Some(v) = value {
                                    ctx.println(&format!("        value={v}"));
                                }
                                if let Some(a) = alarm {
                                    ctx.println(&format!("        alarm={a:?}"));
                                }
                                if let Some((s, n, _)) = ts {
                                    ctx.println(&format!("        timeStamp={s}.{n:09}"));
                                }
                            }
                            epics_base_rs::server::record::ParsedLink::Ca(ca) => {
                                let name = &ca.pv;
                                ctx.println(&format!(
                                    "    {field}={raw:?}  ca://{name}  (CA link — see camonitor)"
                                ));
                            }
                            epics_base_rs::server::record::ParsedLink::Db(db) => {
                                ctx.println(&format!(
                                    "    {field}={raw:?}  db link → {}.{}",
                                    db.record, db.field
                                ));
                            }
                            epics_base_rs::server::record::ParsedLink::Constant(c) => {
                                ctx.println(&format!("    {field}={raw:?}  constant {c:?}"));
                            }
                            epics_base_rs::server::record::ParsedLink::None => {}
                            epics_base_rs::server::record::ParsedLink::Hw(hw) => {
                                ctx.println(&format!(
                                    "    {field}={raw:?}  hw link {:?} args={:?}",
                                    hw.kind, hw.args
                                ));
                            }
                            epics_base_rs::server::record::ParsedLink::Calc(calc) => {
                                ctx.println(&format!(
                                    "    {field}={raw:?}  calc link expr={:?} args={:?}",
                                    calc.expr, calc.args
                                ));
                            }
                        }
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `pvalink_enable` / `pvalink_disable` — master switch for pvalink
/// resolution. When disabled, the resolver returns None for every
/// lookup. Mirrors pvxs `pvalink_enable` / `pvalink_disable`
/// (pvalink.cpp:328).
pub fn pvalink_enable_command(resolver: PvaLinkResolver) -> CommandDef {
    CommandDef::new(
        "pvalink_enable",
        vec![],
        "pvalink_enable",
        move |_args: &[ArgValue], ctx: &CommandContext| {
            resolver.set_enabled(true);
            ctx.println("pvalink_enable: pvalink resolution ENABLED");
            Ok(CommandOutcome::Continue)
        },
    )
}

pub fn pvalink_disable_command(resolver: PvaLinkResolver) -> CommandDef {
    CommandDef::new(
        "pvalink_disable",
        vec![],
        "pvalink_disable",
        move |_args: &[ArgValue], ctx: &CommandContext| {
            resolver.set_enabled(false);
            ctx.println("pvalink_disable: pvalink resolution DISABLED");
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Convenience: build the full pvalink iocsh command set bound to
/// `resolver`. Drop the result into [`epics_base_rs::server::ioc_app::IocRunConfig::shell_commands`].
pub fn register_pvalink_commands(resolver: PvaLinkResolver) -> Vec<CommandDef> {
    vec![
        db_pvxr_command(resolver.clone()),
        pvxrefdiff_command(resolver.clone()),
        dbpvxr_command(resolver.clone()),
        pvalink_enable_command(resolver.clone()),
        pvalink_disable_command(resolver),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_resolver() -> PvaLinkResolver {
        PvaLinkResolver::new(tokio::runtime::Handle::current())
    }

    #[tokio::test]
    async fn register_pvalink_commands_returns_five() {
        let r = dummy_resolver();
        let cmds = register_pvalink_commands(r);
        assert_eq!(cmds.len(), 5);
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"pvxr"));
        assert!(names.contains(&"pvxrefdiff"));
        assert!(names.contains(&"dbpvxr"));
        assert!(names.contains(&"pvalink_enable"));
        assert!(names.contains(&"pvalink_disable"));
    }

    #[tokio::test]
    async fn enable_flag_round_trip() {
        let r = dummy_resolver();
        assert!(r.is_enabled());
        r.set_enabled(false);
        assert!(!r.is_enabled());
        r.set_enabled(true);
        assert!(r.is_enabled());
    }
}
