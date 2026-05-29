//! iocsh commands for pvalink — `pvxr`, `pvalinkrefdiff`, `dbpvar`,
//! `dbpvxr`, `pvaLinkNWorkers`.
//!
//! Mirrors pvxs `ioc/pvalink.cpp` (`dbpvxr` registered under the IOC
//! shell name `dbpvar`, `testqsrvWaitForLinkConnected`, and the
//! `pvaLinkNWorkers` variable). Pre-warms link entries so the
//! synchronous record-link resolver can read cached monitor values
//! without `block_on(GET)`.
//!
//! NOTE: `pvalinkrefdiff` is a Rust-specific pvalink read-activity
//! counter, deliberately NOT named `pvxrefdiff` — the pvxs
//! `pvxrefshow`/`pvxrefsave`/`pvxrefdiff` family is a PVA-library
//! reference-leak diagnostic (`pvxs/ioc/iochooks.cpp:479-484`), not a
//! pvalink command.

use epics_base_rs::server::database::LinkSet;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::config::{ProcMode, SevrMode};
use super::integration::PvaLinkResolver;
use super::registry::ChannelDiag;

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

/// `pvalinkrefdiff` — print pvalink "reads since last call" delta.
///
/// This is a Rust-specific pvalink read-activity counter; it is NOT the
/// pvxs `pvxrefdiff` reference-leak diagnostic and must NOT be
/// registered under that name. pvxs `pvxrefshow` / `pvxrefsave` /
/// `pvxrefdiff` are a matched library-wide diagnostic set over
/// `instanceSnapshot()` per-class object counts, registered by
/// `pvxsBaseRegistrar` (`pvxs/ioc/iochooks.cpp:247-310,479-484`) — a
/// PVA-library reference-leak facility, not a pvalink concern. Squatting
/// the `pvxrefdiff` name here gave operators running the known pvxs
/// command unrelated pvalink read totals and silently broke log checks
/// keyed to pvxs counter classes. The name is therefore freed for the
/// PVA library to register correctly; the pvalink read-activity delta
/// lives under its own honest name.
///
/// Uses interior counter state on the [`PvaLinkResolver`] — the first
/// call shows the running total, subsequent calls show deltas vs. the
/// previous call.
pub fn pvalinkrefdiff_command(resolver: PvaLinkResolver) -> CommandDef {
    let last = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    CommandDef::new(
        "pvalinkrefdiff",
        vec![],
        "pvalinkrefdiff",
        move |_args: &[ArgValue], ctx: &CommandContext| {
            let now = resolver.read_count();
            let prev = last.swap(now, std::sync::atomic::Ordering::Relaxed);
            let delta = now.wrapping_sub(prev);
            ctx.println(&format!(
                "pvalinkrefdiff: {delta} read(s) since last call (total {now}, {} cached link(s))",
                resolver.link_count()
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// pvxs `proc`-mode label as printed by `dbpvxr`
/// (`pvxs/ioc/pvalink.cpp:284-290`).
fn proc_label(p: ProcMode) -> &'static str {
    match p {
        ProcMode::Default => "Def",
        ProcMode::Pp => "PP",
        ProcMode::Npp => "NPP",
        ProcMode::Cp => "CP",
        ProcMode::Cpp => "CPP",
    }
}

/// pvxs `sevr`-mode label as printed by `dbpvxr`
/// (`pvxs/ioc/pvalink.cpp:291-295`).
fn sevr_label(s: SevrMode) -> &'static str {
    match s {
        SevrMode::Nms => "NMS",
        SevrMode::Ms => "MS",
        SevrMode::Msi => "MSI",
    }
}

/// Print one channel row + (at `level >= 5`) its config detail, mirroring
/// pvxs `dbpvxr`'s per-channel block (`pvxs/ioc/pvalink.cpp:243-306`).
fn print_channel_row(ctx: &CommandContext, d: &ChannelDiag, level: i64) {
    // pvxs right-justifies the channel name in a 28-wide column.
    ctx.println(&format!(
        "{:>28} conn={}",
        d.pv_name,
        if d.connected { 'T' } else { 'F' }
    ));
    if level >= 5 {
        // pvxs prints num_disconnect / num_type_change / Put here too;
        // the Rust link does not instrument those transitions, so the
        // detail line carries only the per-link config it does track.
        ctx.println(&format!(
            "{:30}{} {} Q={} pipe={} defer={} time={} retry={} morder={} field={:?}",
            "",
            proc_label(d.proc),
            sevr_label(d.sevr),
            d.queue_size,
            if d.pipeline { 'T' } else { 'F' },
            if d.defer { 'T' } else { 'F' },
            if d.time { 'T' } else { 'F' },
            if d.retry { 'T' } else { 'F' },
            d.monorder,
            d.field,
        ));
    }
}

/// `dbpvar [<recordName>] [<level>]` — print pvalink diagnostics.
///
/// The Rust counterpart of pvxs `dbpvxr`, which the upstream IOC
/// registers under the shell name `dbpvar`
/// (`pvxs/ioc/pvalink.cpp:184-331`). An empty or `"*"` record selects
/// all channels; a named record selects that record's link fields.
/// Level semantics follow pvxs (`pvalink.cpp:240-311`):
///
/// - `level <= 0` — summary only (connected/total channel counts).
/// - `level == 1` — additionally list disconnected channels.
/// - `level >= 2` — list every channel.
/// - `level >= 5` — additionally dump each channel's link config.
///
/// `dbpvxr` is registered as an alias of the same handler for
/// compatibility with existing Rust startup scripts.
///
/// The record-named path walks the database's link fields (record →
/// link) because the Rust registry, unlike pvxs's `pvaLinkChannel`,
/// keeps no channel → record back-index; it therefore reports per-field
/// connection / value / alarm / time rather than filtering the channel
/// list by record glob.
pub fn dbpvar_command(resolver: PvaLinkResolver) -> CommandDef {
    dbpvar_like("dbpvar", resolver)
}

/// `dbpvxr` — alias of [`dbpvar_command`] kept for back-compat.
pub fn dbpvxr_command(resolver: PvaLinkResolver) -> CommandDef {
    dbpvar_like("dbpvxr", resolver)
}

fn dbpvar_like(name: &'static str, resolver: PvaLinkResolver) -> CommandDef {
    CommandDef::new(
        name,
        vec![
            ArgDesc {
                name: "record",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
                optional: true,
            },
        ],
        // pvxs usage is "record name", "level".
        match name {
            "dbpvxr" => "dbpvxr [<recordName>] [<level>]",
            _ => "dbpvar [<recordName>] [<level>]",
        },
        move |args: &[ArgValue], ctx: &CommandContext| {
            // empty / missing / "*" => all records (pvxs pvalink.cpp:193).
            let target = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() && s != "*" => Some(s.clone()),
                _ => None,
            };
            let level = match args.get(1) {
                Some(ArgValue::Int(n)) => *n,
                _ => 0,
            };

            match &target {
                None => ctx.println("PVA links in all records\n"),
                Some(rec) => ctx.println(&format!("PVA links in record named '{rec}'\n")),
            }

            if let Some(rec) = target {
                // Record-named path: record → link-field walk (the Rust
                // registry has no channel → record back-index).
                dump_record_link_fields(&resolver, ctx, &rec);
            } else {
                // All-records path: channel-centric listing with pvxs
                // level semantics.
                let diags = resolver.channel_diagnostics();
                let nchans = diags.len();
                let nconn = diags.iter().filter(|d| d.connected).count();
                if level >= 1 {
                    for d in &diags {
                        // level 1 shows only disconnected channels;
                        // level >= 2 shows every channel.
                        if level >= 2 || !d.connected {
                            print_channel_row(ctx, d, level);
                        }
                    }
                }
                ctx.println(&format!(
                    "  {nconn}/{nchans} channels connected ({} cached link(s), {} total reads, enabled={})",
                    resolver.link_count(),
                    resolver.read_count(),
                    resolver.is_enabled()
                ));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Walk every link-shaped String field on `rec` and dump connection /
/// value / alarm / time state for each `pva://` / `ca://` link via the
/// registered [`LinkSet`]. The record-named branch of `dbpvar`.
fn dump_record_link_fields(resolver: &PvaLinkResolver, ctx: &CommandContext, rec: &str) {
    let db = ctx.db().clone();
    let handle = ctx.runtime_handle().clone();
    let rec_clone = rec.to_string();
    let links = std::thread::spawn(move || {
        handle.block_on(async move { db.record_link_fields(&rec_clone).await })
    })
    .join()
    .unwrap_or_default();
    if links.is_empty() {
        ctx.println(&format!(
            "  '{rec}': no link fields found (or record missing)"
        ));
        return;
    }
    ctx.println(&format!("  '{rec}': {} link field(s)", links.len()));
    for (field, raw, parsed) in links {
        match parsed {
            epics_base_rs::server::record::ParsedLink::Pva(name) => {
                let connected = <PvaLinkResolver as LinkSet>::is_connected(resolver, &name);
                let value = <PvaLinkResolver as LinkSet>::get_value(resolver, &name);
                let alarm = <PvaLinkResolver as LinkSet>::alarm_message(resolver, &name);
                let ts = <PvaLinkResolver as LinkSet>::time_stamp(resolver, &name);
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

/// `pvaLinkNWorkers [<n>]` — pvxs registers `pvaLinkNWorkers` as an IOC
/// shell *variable* tuning the size of its shared pvalink worker pool
/// (`pvxs/ioc/pvalink.cpp:318-333`). The Rust pvalink implementation has
/// no fixed worker pool — each link drives its own async monitor task on
/// the shared tokio runtime — so there is no equivalent to tune. Rather
/// than silently accept a setting that does nothing, this command
/// rejects it with an explicit diagnostic (the
/// `iocshRegisterVariable` fallback called for in the review's fix
/// direction; the base iocsh layer registers commands, not variables).
pub fn pvalink_nworkers_command() -> CommandDef {
    CommandDef::new(
        "pvaLinkNWorkers",
        vec![ArgDesc {
            name: "n",
            arg_type: ArgType::Int,
            optional: true,
        }],
        "pvaLinkNWorkers [<n>]",
        move |_args: &[ArgValue], ctx: &CommandContext| {
            ctx.println(
                "pvaLinkNWorkers: no effect — the Rust pvalink uses one async \
                 monitor task per link on the shared runtime, not a fixed \
                 worker pool, so there is no worker count to tune.",
            );
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
        pvalinkrefdiff_command(resolver.clone()),
        dbpvar_command(resolver.clone()),
        dbpvxr_command(resolver.clone()),
        pvalink_nworkers_command(),
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
    async fn register_pvalink_commands_exports_pvxs_compatible_set() {
        let r = dummy_resolver();
        let cmds = register_pvalink_commands(r);
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        // pvxs registers the diagnostic under the name `dbpvar` and the
        // `pvaLinkNWorkers` variable (`pvxs/ioc/pvalink.cpp:328-333`);
        // `dbpvxr` is kept as a Rust back-compat alias.
        assert!(names.contains(&"pvxr"));
        assert!(names.contains(&"pvalinkrefdiff"));
        assert!(names.contains(&"dbpvar"));
        assert!(names.contains(&"dbpvxr"));
        assert!(names.contains(&"pvaLinkNWorkers"));
        assert!(names.contains(&"pvalink_enable"));
        assert!(names.contains(&"pvalink_disable"));
        assert_eq!(cmds.len(), 7);

        // The pvxs `pvxrefshow`/`pvxrefsave`/`pvxrefdiff` reference-leak
        // diagnostic family belongs to the PVA library, not pvalink:
        // pvalink must NOT squat any of those names
        // (BRIDGE-RS-2026-05-28-83).
        assert!(!names.contains(&"pvxrefdiff"));
        assert!(!names.contains(&"pvxrefsave"));
        assert!(!names.contains(&"pvxrefshow"));

        // `dbpvar` and `dbpvxr` both take (record, level): two optional
        // args, matching pvxs `IOCShCommand<const char*, int>`.
        for cmd_name in ["dbpvar", "dbpvxr"] {
            let cmd = cmds.iter().find(|c| c.name == cmd_name).unwrap();
            assert_eq!(cmd.args.len(), 2, "{cmd_name} takes (record, level)");
            assert!(matches!(cmd.args[1].arg_type, ArgType::Int));
            assert!(cmd.args[1].optional);
        }
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
