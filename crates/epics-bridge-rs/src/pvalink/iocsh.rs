//! iocsh commands for pvalink — `pvxr`, `pvalinkrefdiff`, `dbpvar`,
//! `dbpvxr`, and the `pvaLinkNWorkers` variable.
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

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the exec-backend
// suite.

use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};
use epics_base_rs::server::iocsh::vars::{VarAccess, VarDef};

use super::config::{ProcMode, SevrMode};
use super::integration::PvaLinkResolver;
use super::registry::ChannelDiag;

/// `pvxr <pv_name>` — pre-open a link in INP+monitor mode so the
/// resolver returns cached values for that PV without a blocking GET
/// on first access. Mirrors pvxs `pvalinkOpen` (pvxs/ioc/pvalink_channel.cpp).
pub fn db_pvxr_command(resolver: PvaLinkResolver) -> CommandDef {
    CommandDef::new(
        "pvxr",
        vec![ArgDesc {
            name: "pv_name",
            arg_type: ArgType::String,
        }],
        "pvxr <pv_name>",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let name = match args.first() {
                Some(ArgValue::String(s)) => s.to_str()?.to_owned(),
                _ => return Err("pvxr: missing pv_name".into()),
            };
            let resolver = resolver.clone();
            let bridge = ctx.bridge().clone();
            let result = std::thread::spawn(move || {
                bridge.block_on(async move { resolver.open(&name).await })
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
/// `pvxsBaseRegistrar` (`pvxs/ioc/iochooks.cpp:247-312,479-484`) — a
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
pub(super) fn proc_label(p: ProcMode) -> &'static str {
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
pub(super) fn sevr_label(s: SevrMode) -> &'static str {
    match s {
        SevrMode::Nms => "NMS",
        SevrMode::Ms => "MS",
        SevrMode::Msi => "MSI",
    }
}

/// Port of EPICS Base `epicsStrGlobMatch` (`misc/epicsString.c`), the
/// matcher pvxs's `dbpvxr` applies to each attached link's record name
/// (`epicsStrGlobMatch(pval->plink->precord->name, precordname)`,
/// `pvxs/ioc/pvalink.cpp:224,269`): `*` matches any run of bytes, `?`
/// matches exactly one, every other byte is literal.
fn glob_match(text: &[u8], pat: &[u8]) -> bool {
    let mut i = 0usize;
    let mut p = 0usize;
    let mut mp: Option<usize> = None;
    let mut cp = 0usize;

    while i < text.len() && p < pat.len() && pat[p] != b'*' {
        if pat[p] != text[i] && pat[p] != b'?' {
            return false;
        }
        p += 1;
        i += 1;
    }
    while i < text.len() {
        if p < pat.len() && pat[p] == b'*' {
            p += 1;
            if p >= pat.len() {
                return true;
            }
            mp = Some(p);
            cp = i + 1;
        } else if p < pat.len() && (pat[p] == text[i] || pat[p] == b'?') {
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
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p >= pat.len()
}

/// One channel of pvxs `dbpvxr` (`pvxs/ioc/pvalink.cpp:243-303`): its row
/// and, at `level >= 5`, a line per link whose record `glob` matches.
fn print_channel(ctx: &CommandContext, d: &ChannelDiag, level: i64, glob: Option<&[u8]>) {
    use std::fmt::Write;
    let tf = |b: bool| if b { 'T' } else { 'F' };
    // `%28s ` pads bytes, as `key.first.size()` counts them.
    let mut row = match 28usize.checked_sub(d.pv_name.len()) {
        Some(pad) => format!("{:pad$}{} ", "", d.pv_name),
        None => format!("{}\t", d.pv_name),
    };
    let _ = write!(
        row,
        "conn={} {} disconnects, {} type changes",
        tf(d.connected),
        d.num_disconnect,
        d.num_type_change
    );
    if d.put {
        row.push_str(" Put");
    }
    ctx.println(&row);
    if level < 5 {
        return;
    }
    for l in &d.links {
        if !glob.is_none_or(|g| glob_match(&l.record, g)) {
            continue;
        }
        let c = &l.config;
        let mut line = vec![b' '; 30];
        line.extend_from_slice(&l.record);
        line.extend_from_slice(
            format!(
                ".{} {} {} Q={} pipe={} defer={} time={} retry={} morder={}",
                l.field,
                proc_label(c.proc),
                sevr_label(c.sevr),
                c.queue_size,
                tf(c.pipeline),
                tf(c.defer),
                tf(c.time),
                tf(c.retry),
                c.monorder,
            )
            .as_bytes(),
        );
        ctx.println_bytes(&line);
    }
    ctx.println("");
}

/// `dbpvar [<recordNameGlob>] [<level>]` — print pvalink diagnostics.
///
/// The Rust counterpart of pvxs `dbpvxr`, which the upstream IOC
/// registers under the shell name `dbpvar`
/// (`pvxs/ioc/pvalink.cpp:184-316`). The command is channel-centric:
/// it snapshots the cached channel map and, for both the all-records
/// and the record-filtered call, filters channels by glob-matching the
/// names of the records whose links attached to each channel
/// (`epicsStrGlobMatch`, `pvxs/ioc/pvalink.cpp:224,229-230,269`). The first argument
/// is therefore a record-name GLOB (`REC:*`, `IOC:AI?`), not an exact
/// record name; empty / missing / `"*"` selects every channel.
///
/// Level semantics follow pvxs (`pvxs/ioc/pvalink.cpp:240-311`):
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
            },
            ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
            },
        ],
        // pvxs usage is "record name" (a glob), "level".
        match name {
            "dbpvxr" => "dbpvxr [<recordNameGlob>] [<level>]",
            _ => "dbpvar [<recordNameGlob>] [<level>]",
        },
        move |args: &[ArgValue], ctx: &CommandContext| {
            // First arg is a record-name GLOB; empty / missing / "*" =>
            // every channel (`pvxs/ioc/pvalink.cpp:193`).
            let glob = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() && s != "*" => {
                    Some(s.as_bytes().to_vec())
                }
                _ => None,
            };
            let level = match args.get(1) {
                Some(ArgValue::Int(n)) => *n,
                _ => 0,
            };

            match &glob {
                None => ctx.println("PVA links in all records\n"),
                Some(g) => {
                    let mut line = b"PVA links in record named '".to_vec();
                    line.extend_from_slice(g);
                    line.extend_from_slice(b"'\n");
                    ctx.println_bytes(&line);
                }
            }

            let (mut nchans, mut nlinks, mut nconn) = (0usize, 0usize, 0usize);
            for d in &resolver.channel_diagnostics() {
                match &glob {
                    // A channel shows when one of its links' records matches.
                    Some(g) => {
                        let n = d.links.iter().filter(|l| glob_match(&l.record, g)).count();
                        if n == 0 {
                            continue;
                        }
                        nlinks += n;
                    }
                    None => nlinks += d.links.len(),
                }
                nchans += 1;
                if d.connected {
                    nconn += 1;
                }
                if level >= 2 || (level == 1 && !d.connected) {
                    print_channel(ctx, d, level, glob.as_deref());
                }
            }
            ctx.println(&format!(
                "  {nconn}/{nchans} channels connected used by {nlinks} links"
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// pvxs keeps `pvaLinkNWorkers` in the iocsh *variable* table, not the
/// command table: `pvalink_enable` calls `iocshRegisterVariable` on a
/// `iocshVarDef` pointing straight at the `int pvxLinkNWorkers` global
/// (`pvxs/ioc/pvalink.cpp:318-334`), so a startup script spells it
/// `var pvaLinkNWorkers 4` and `var` alone prints it. Registering it as a command made that spelling fail.
///
/// The value is inert here, as it is in pvxs: `pvxLinkNWorkers` starts
/// at 1 (`ioc/pvalink_channel.cpp:20`) and the only place that would
/// consume it carries pvxs's own `// TODO respect pvxLinkNWorkers?`
/// (`ioc/pvalink_channel.cpp:45`). The Rust pvalink likewise drives one
/// async monitor task per link rather than a fixed pool.
static NWORKERS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

fn pvalink_nworkers_var() -> VarDef {
    VarDef {
        name: "pvaLinkNWorkers",
        access: VarAccess::Int {
            get: || NWORKERS.load(std::sync::atomic::Ordering::Relaxed),
            set: |v| NWORKERS.store(v, std::sync::atomic::Ordering::Relaxed),
        },
    }
}

/// `pvalink_enable` / `pvalink_disable` — master switch for pvalink
/// resolution. When disabled, the resolver returns None for every
/// lookup. An epics-rs addition: pvxs has no `pvalink_disable`, and
/// its `pvalink_enable` (`pvxs/ioc/pvalink.cpp:328`) registers `dbpvar`.
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

/// The pvalink iocsh commands pvxs has no counterpart of, bound to
/// `resolver`; [`pvalink_enable_commands`] is the set pvxs gates.
pub fn register_pvalink_commands(resolver: PvaLinkResolver) -> Vec<CommandDef> {
    vec![
        db_pvxr_command(resolver.clone()),
        pvalinkrefdiff_command(resolver.clone()),
        pvalink_enable_command(resolver.clone()),
        pvalink_disable_command(resolver),
    ]
}

/// pvxs `pvalink_enable()`, which `pvxsBaseRegistrar` calls only when
/// `enable2()` is true (`pvxs/ioc/pvalink.cpp:328-334`,
/// `ioc/iochooks.cpp:503-507`): the `pvaLinkNWorkers` variable and `dbpvar`,
/// with its `dbpvxr` alias.
pub fn pvalink_enable_commands(resolver: PvaLinkResolver) -> Vec<CommandDef> {
    epics_base_rs::server::iocsh::vars::register_variable(pvalink_nworkers_var());
    vec![dbpvar_command(resolver.clone()), dbpvxr_command(resolver)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_resolver() -> PvaLinkResolver {
        PvaLinkResolver::new(crate::test_reactor())
    }

    #[tokio::test]
    async fn register_pvalink_commands_exports_pvxs_compatible_set() {
        let r = dummy_resolver();
        let enabled = pvalink_enable_commands(r.clone());
        let enabled: Vec<&str> = enabled.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(enabled, ["dbpvar", "dbpvxr"]);
        let mut cmds = register_pvalink_commands(r.clone());
        cmds.extend(pvalink_enable_commands(r));
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        // pvxs registers the diagnostic under the name `dbpvar` and the
        // `pvaLinkNWorkers` variable (`pvxs/ioc/pvalink.cpp:328-334`);
        // `dbpvxr` is kept as a Rust back-compat alias.
        assert!(names.contains(&"pvxr"));
        assert!(names.contains(&"pvalinkrefdiff"));
        assert!(names.contains(&"dbpvar"));
        assert!(names.contains(&"dbpvxr"));
        // `pvaLinkNWorkers` is NOT among them: pvxs puts it in the
        // variable table (`pvxs/ioc/pvalink.cpp:318-334`), which is
        // what `var pvaLinkNWorkers` reads.
        assert!(!names.contains(&"pvaLinkNWorkers"));
        assert!(
            epics_base_rs::server::iocsh::vars::variable_names().contains(&"pvaLinkNWorkers"),
            "pvalink_enable_commands must publish the variable"
        );
        // pvxs starts `pvxLinkNWorkers` at 1
        // (`pvxs/ioc/pvalink_channel.cpp:20`).
        assert_eq!(NWORKERS.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(names.contains(&"pvalink_enable"));
        assert!(names.contains(&"pvalink_disable"));
        assert_eq!(cmds.len(), 6);

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
        }
    }

    /// `dbpvar`'s record-name filter is an `epicsStrGlobMatch` glob, not
    /// an exact match: `*` spans any run, `?` one char, the rest literal
    /// (`pvxs/ioc/pvalink.cpp:224,269`).
    #[test]
    fn dbpvar_glob_match_semantics() {
        assert!(glob_match(b"TST:AI1", b"TST:*"));
        assert!(glob_match(b"TST:AI1", b"*AI1"));
        assert!(glob_match(b"TST:AI1", b"TST:AI?"));
        assert!(glob_match(b"TST:AI1", b"*"));
        assert!(glob_match(b"TST:AI1", b"TST:AI1"));
        // Non-matches: a literal `TST:*` is NOT how a glob is matched.
        assert!(!glob_match(b"OTH:AI1", b"TST:*"));
        assert!(!glob_match(b"TST:AI11", b"TST:AI?"));
        assert!(!glob_match(b"TST:AI1", b"TST:AI1X"));
        // `?` is one byte, as in C.
        assert!(glob_match(b"caf\xe9", b"caf?"));
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
