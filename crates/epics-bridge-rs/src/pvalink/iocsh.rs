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

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::config::{LinkDirection, ProcMode, SevrMode};
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

/// Port of EPICS Base `epicsStrGlobMatch` (`misc/epicsString.c`), the
/// matcher pvxs's `dbpvxr` applies to each attached link's record name
/// (`epicsStrGlobMatch(pval->plink->precord->name, precordname)`,
/// `pvxs/ioc/pvalink.cpp:224,233`): `*` matches any run of characters,
/// `?` matches exactly one, every other character is literal.
fn glob_match(s: &str, pattern: &str) -> bool {
    let text: Vec<char> = s.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();
    let mut i = 0usize;
    let mut p = 0usize;
    let mut mp: Option<usize> = None;
    let mut cp = 0usize;

    while i < text.len() && p < pat.len() && pat[p] != '*' {
        if pat[p] != text[i] && pat[p] != '?' {
            return false;
        }
        p += 1;
        i += 1;
    }
    while i < text.len() {
        if p < pat.len() && pat[p] == '*' {
            p += 1;
            if p >= pat.len() {
                return true;
            }
            mp = Some(p);
            cp = i + 1;
        } else if p < pat.len() && (pat[p] == text[i] || pat[p] == '?') {
            p += 1;
            i += 1;
        } else if let Some(m) = mp {
            p = m;
            i = cp;
            cp += 1;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p >= pat.len()
}

/// Print one channel row + (at `level >= 5`) its per-link config detail,
/// mirroring pvxs `dbpvxr`'s per-channel block
/// (`pvxs/ioc/pvalink.cpp:243-306`). When `glob` is set, only attached
/// records whose name matches it are printed (pvxs `pvalink.cpp:269`).
fn print_channel_row(ctx: &CommandContext, d: &ChannelDiag, level: i64, glob: Option<&str>) {
    // pvxs right-justifies the channel name in a 28-wide column.
    ctx.println(&format!(
        "{:>28} conn={} dir={} Q={} pipe={}",
        d.pv_name,
        if d.connected { 'T' } else { 'F' },
        match d.direction {
            LinkDirection::Inp => "INP",
            LinkDirection::Out => "OUT",
        },
        d.queue_size,
        if d.pipeline { 'T' } else { 'F' },
    ));
    if level >= 5 {
        // pvxs prints one `precord->name.fldname` row per attached
        // `pvaLink`, glob-filtered by record name. The Rust registry
        // folds field/sevr-only variants onto one channel, so it cannot
        // recover each link's record-field name or per-link proc/sevr;
        // it prints one row per owning record carrying the channel's
        // folded config. (pvxs num_disconnect / num_type_change / Put
        // are not instrumented by the Rust link and are omitted rather
        // than fabricated.)
        let detail = format!(
            "{} {} Q={} pipe={} defer={} time={} retry={} morder={} field={:?}",
            proc_label(d.proc),
            sevr_label(d.sevr),
            d.queue_size,
            if d.pipeline { 'T' } else { 'F' },
            if d.defer { 'T' } else { 'F' },
            if d.time { 'T' } else { 'F' },
            if d.retry { 'T' } else { 'F' },
            d.monorder,
            d.field,
        );
        let mut printed_any = false;
        for rec in &d.records {
            if glob.map(|g| glob_match(rec, g)).unwrap_or(true) {
                ctx.println(&format!("{:30}{rec} {detail}", ""));
                printed_any = true;
            }
        }
        if !printed_any {
            // Channel with no owning record (e.g. a `pvxr`-opened link):
            // still surface the folded config.
            ctx.println(&format!("{:30}{detail}", ""));
        }
    }
}

/// `dbpvar [<recordNameGlob>] [<level>]` — print pvalink diagnostics.
///
/// The Rust counterpart of pvxs `dbpvxr`, which the upstream IOC
/// registers under the shell name `dbpvar`
/// (`pvxs/ioc/pvalink.cpp:184-316`). The command is channel-centric:
/// it snapshots the cached channel map and, for both the all-records
/// and the record-filtered call, filters channels by glob-matching the
/// names of the records whose links attached to each channel
/// (`epicsStrGlobMatch`, `pvalink.cpp:224,233,269`). The first argument
/// is therefore a record-name GLOB (`REC:*`, `IOC:AI?`), not an exact
/// record name; empty / missing / `"*"` selects every channel.
///
/// Level semantics follow pvxs (`pvalink.cpp:240-311`):
///
/// - `level <= 0` — summary only (connected/total channel counts).
/// - `level == 1` — additionally list disconnected matching channels.
/// - `level >= 2` — list every matching channel.
/// - `level >= 5` — additionally dump per-link rows, glob-filtered by
///   record name.
///
/// `dbpvxr` is registered as an alias of the same handler for
/// compatibility with existing Rust startup scripts.
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
                name: "recordNameGlob",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
                optional: true,
            },
        ],
        // pvxs usage is "record name" (a glob), "level".
        match name {
            "dbpvxr" => "dbpvxr [<recordNameGlob>] [<level>]",
            _ => "dbpvar [<recordNameGlob>] [<level>]",
        },
        move |args: &[ArgValue], ctx: &CommandContext| {
            // First arg is a record-name GLOB; empty / missing / "*" =>
            // every channel (pvxs `pvalink.cpp:193`).
            let glob = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() && s != "*" => Some(s.clone()),
                _ => None,
            };
            let level = match args.get(1) {
                Some(ArgValue::Int(n)) => *n,
                _ => 0,
            };

            match &glob {
                None => ctx.println("PVA links in all records\n"),
                Some(g) => ctx.println(&format!("PVA links in records matching '{g}'\n")),
            }

            // Channel-centric listing for BOTH all-records and
            // record-filtered calls, mirroring pvxs `dbpvxr`: snapshot
            // the channel map, filter each channel by glob-matching the
            // names of the records whose links attached to it, then apply
            // the level gates (`pvalink.cpp:208-311`).
            let diags = resolver.channel_diagnostics();
            let mut nchans = 0usize;
            let mut nconn = 0usize;
            let mut nlinks = 0usize;
            for d in &diags {
                let nmatched = match &glob {
                    Some(g) => d.records.iter().filter(|r| glob_match(r, g)).count(),
                    None => d.records.len(),
                };
                // A glob-filtered call skips channels no matching record
                // uses (pvxs `pvalink.cpp:229-231`).
                if glob.is_some() && nmatched == 0 {
                    continue;
                }
                nchans += 1;
                if d.connected {
                    nconn += 1;
                }
                nlinks += nmatched;
                if level <= 0 {
                    continue;
                }
                // level 1 lists only disconnected channels; level >= 2
                // lists every matching channel (pvxs `pvalink.cpp:243`).
                if level >= 2 || !d.connected {
                    print_channel_row(ctx, d, level, glob.as_deref());
                }
            }
            ctx.println(&format!(
                "  {nconn}/{nchans} channels connected used by {nlinks} link(s)"
            ));
            // Rust-specific resolver counters, clearly separated from the
            // pvxs-shaped summary above.
            ctx.println(&format!(
                "  ({} cached link(s), {} total reads, enabled={})",
                resolver.link_count(),
                resolver.read_count(),
                resolver.is_enabled()
            ));
            Ok(CommandOutcome::Continue)
        },
    )
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
        PvaLinkResolver::new()
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
        // pvalink must NOT squat any of those names.
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

    /// `dbpvar`'s record-name filter is an `epicsStrGlobMatch` glob, not
    /// an exact match: `*` spans any run, `?` one char, the rest literal
    /// (pvxs `pvalink.cpp:224,233,269`).
    #[test]
    fn dbpvar_glob_match_semantics() {
        assert!(glob_match("TST:AI1", "TST:*"));
        assert!(glob_match("TST:AI1", "*AI1"));
        assert!(glob_match("TST:AI1", "TST:AI?"));
        assert!(glob_match("TST:AI1", "*"));
        assert!(glob_match("TST:AI1", "TST:AI1"));
        // Non-matches: a literal `TST:*` is NOT how a glob is matched.
        assert!(!glob_match("OTH:AI1", "TST:*"));
        assert!(!glob_match("TST:AI11", "TST:AI?"));
        assert!(!glob_match("TST:AI1", "TST:AI1X"));
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
