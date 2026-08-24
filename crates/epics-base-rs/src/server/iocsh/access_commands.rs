// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the feature-ON suite.
//! Access-security iocsh commands — the `as*` family.
//!
//! C registers the whole `as*` command family in
//! `asIocRegister.c`. The Rust port previously registered none of
//! them, so an ACF could not be loaded or inspected from the shell.
//!
//! The deferred filename + substitutions strings set by
//! `asSetFilename` / `asSetSubstitutions` live in a process-global
//! holder ([`as_state`]) mirroring the C `asDbLib.c` globals. The
//! parsed [`AccessSecurityConfig`] that `asInit` activates does NOT —
//! it is stored into the shell context's live
//! [`AcfCell`](crate::server::access_security::AcfCell), the same
//! cell the IOC's protocol servers gate on, so a script-driven
//! `asInit` enforces exactly as C's does. The C flow is otherwise
//! identical: `asSetFilename` has "no immediate effect", `asInit`
//! does the (re)load.

use std::sync::Mutex;

use super::registry::*;
use super::vars::{VarAccess, VarDef};
use crate::server::access_security::{
    AccessLevel, AccessSecurityConfig, as_check_client_ip, parse_acf, set_as_check_client_ip,
};

/// Process-global access-security shell state. Mirrors the
/// `asDbLib.c` file-scope globals (`acf`, `substitutions`) — the
/// *deferred load parameters* only. The active parsed configuration
/// lives in the shell context's
/// [`AcfCell`](crate::server::access_security::AcfCell)
/// ([`CommandContext::acf`]), the same cell the IOC's servers gate
/// on, so `asInit` here is a live (re)load exactly as C's
/// `asInitCommon` swaps the process `asBase`.
#[derive(Default)]
struct AsState {
    /// Path to the ACF file, set by `asSetFilename`. `None` until set.
    filename: Option<String>,
    /// Macro substitutions applied when reading the ACF, set by
    /// `asSetSubstitutions`.
    substitutions: Option<String>,
}

fn as_state() -> &'static Mutex<AsState> {
    static STATE: std::sync::OnceLock<Mutex<AsState>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| Mutex::new(AsState::default()))
}

/// `as_state()` is process-global, shared by every test in the crate that
/// exercises the `as*` iocsh family — this module's own tests, and
/// `iocsh::tests::test_as_commands_registered`. Locking [`as_state`] only
/// protects one field access at a time, not a whole `asSetFilename` +
/// `asInit` scenario, so under Rust's default concurrent test runner one
/// test's `asSetFilename` (to a tempfile that is later deleted, or to a
/// deliberately malformed ACF) can land in `as_state` while a sibling test
/// that never touches `as*` itself runs `asInit` and reads that stale
/// filename — turning its expected `Ok(Continue)` into a file-read or
/// parse `Err`. Every test that touches `as_state`, directly or through
/// an `as*` iocsh command, takes this lock for its whole body.
#[cfg(test)]
pub(crate) fn as_state_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reset `as_state` to its startup default. A test that assumes "no
/// filename set" (or any other default-state precondition) must call this
/// itself rather than rely on process start order — the lock above only
/// stops *concurrent* corruption, not a stale value left behind by
/// whichever test happened to run earlier in the same process.
#[cfg(test)]
pub(crate) fn reset_as_state_for_test() {
    *as_state().lock().unwrap() = AsState::default();
}

/// Register the `as*` command family on `registry`.
pub(crate) fn register(registry: &mut CommandRegistry) {
    super::vars::register_variable(as_check_client_ip_var());
    registry.register(cmd_as_set_filename());
    registry.register(cmd_as_set_substitutions());
    registry.register(cmd_as_init());
    registry.register(cmd_asdbdump());
    registry.register(cmd_aspuag());
    registry.register(cmd_asphag());
    registry.register(cmd_asprules());
    registry.register(cmd_aspmem());
    registry.register(cmd_astac());
    registry.register(cmd_ascar());
}

/// C registers `asCheckClientIP` as an iocsh *variable*
/// (`libComRegister.c:491-495`, `:535-537`), so a startup script says
/// `var asCheckClientIP 1` and the shell prints nothing. It is not a
/// command in C and is no longer one here.
///
/// `0` (the default, and C's) — HAG members are host *names* and the CA
/// server trusts the hostname the client claims over `CA_PROTO_HOST_NAME`.
/// `1` — HAG members are resolved to IPs at ACF-load time and the CA
/// server uses the peer IP, ignoring the claimed name.
///
/// Order matters exactly as in C: the HAG storage form is chosen when the
/// ACF is parsed, so this must be set **before** `asInit`.
fn as_check_client_ip_var() -> VarDef {
    VarDef {
        name: "asCheckClientIP",
        access: VarAccess::Int {
            get: || i64::from(as_check_client_ip()),
            set: |v| set_as_check_client_ip(v != 0),
        },
    }
}

/// `asSetFilename <ascf>` — record the ACF path. No immediate effect;
/// `asInit` performs the (re)load. Mirrors C `asSetFilename`.
fn cmd_as_set_filename() -> CommandDef {
    CommandDef::new(
        "asSetFilename",
        vec![ArgDesc {
            name: "ascf",
            arg_type: ArgType::String,
            optional: false,
        }],
        "asSetFilename <ascf> — Set path+file of ACF file. Run asInit to (re)load.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let path = match &args[0] {
                ArgValue::String(s) => s.clone(),
                _ => return Err("asSetFilename: missing ascf path".into()),
            };
            as_state().lock().unwrap().filename = Some(path.clone());
            ctx.println(&format!("asSetFilename: ACF path set to '{path}'"));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `asSetSubstitutions <substitutions>` — record macro substitutions
/// applied when reading the ACF. Mirrors C `asSetSubstitutions`.
fn cmd_as_set_substitutions() -> CommandDef {
    CommandDef::new(
        "asSetSubstitutions",
        vec![ArgDesc {
            name: "substitutions",
            arg_type: ArgType::String,
            optional: false,
        }],
        "asSetSubstitutions <subs> — Set substitutions used when reading the ACF file.",
        |args: &[ArgValue], _ctx: &CommandContext| {
            let subs = match &args[0] {
                ArgValue::String(s) => s.clone(),
                _ => return Err("asSetSubstitutions: missing substitutions".into()),
            };
            as_state().lock().unwrap().substitutions = Some(subs);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `asInit` — (re)load the ACF file named by `asSetFilename`,
/// applying any `asSetSubstitutions`. Mirrors C `asInit`.
fn cmd_as_init() -> CommandDef {
    CommandDef::new(
        "asInit",
        vec![],
        "asInit — (Re)load the ACF file set by asSetFilename.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            // Snapshot the deferred config under the lock, release it
            // before file I/O so the lock is not held across syscalls.
            let (filename, substitutions) = {
                let st = as_state().lock().unwrap();
                (st.filename.clone(), st.substitutions.clone())
            };
            let Some(filename) = filename else {
                // C `asInitCommon` (asDbLib.c:127-128): on the first call with
                // no ACF file set (`!pacf`), it `return(0)` — success — leaving
                // access security disabled ("will NEVER be turned on"). It is
                // not an error: a startup script that runs `asInit` without a
                // prior `asSetFilename` must continue, so this returns a no-op
                // Continue rather than aborting the script under `on error break`.
                ctx.println(
                    "asInit: no ACF file set — access security not enabled \
                     (call asSetFilename first to enable it)",
                );
                return Ok(CommandOutcome::Continue);
            };
            let raw = std::fs::read_to_string(&filename)
                .map_err(|e| format!("asInit: cannot read '{filename}': {e}"))?;
            let content = match &substitutions {
                Some(subs) if !subs.is_empty() => {
                    let macros = super::commands::parse_macro_string(subs);
                    // Per line, as C `asInitFile` feeds `macExpandString` one
                    // `fgets` line at a time (`asLibRoutines.c:202-219`) — a
                    // quote in one ACF comment must not suppress `$(...)` on
                    // the lines after it.
                    crate::server::db_loader::substitute_macros_per_line(&raw, &macros)
                }
                _ => raw,
            };
            let config = parse_acf(&content)
                .map_err(|e| format!("asInit: parse error in '{filename}': {e}"))?;
            let summary = format!(
                "asInit: loaded '{filename}' — {} UAG, {} HAG, {} ASG",
                config.uag.len(),
                config.hag.len(),
                config.asg.len()
            );
            // Publish into the IOC's live policy cell — the store fires
            // the process-wide change notification, so live CA clients
            // re-evaluate their ACCESS_RIGHTS and policy caches drop
            // (C: `asInitialize` swaps `pasbase` and re-computes every
            // ASGCLIENT, asLibRoutines.c `asInitCommon`).
            ctx.acf().store(Some(std::sync::Arc::new(config)));
            ctx.println(&summary);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Helper: run `f` with the active config, or report "not loaded".
/// Reads the shell's live
/// [`AcfCell`](crate::server::access_security::AcfCell) — the same
/// policy the servers enforce — so the `as*` inspection commands can
/// never show a config the gates are not actually using.
fn with_config<F: FnOnce(&AccessSecurityConfig)>(ctx: &CommandContext, f: F) -> CommandResult {
    match &*ctx.acf().load() {
        Some(cfg) => {
            f(cfg);
            Ok(CommandOutcome::Continue)
        }
        None => {
            ctx.println("Access security not loaded — run asInit.");
            Ok(CommandOutcome::Continue)
        }
    }
}

/// `asdbdump` — dump the processed ACF (UAGs, HAGs, ASGs, rules).
/// Mirrors C `asdbdump` / `asDumpFP`.
fn cmd_asdbdump() -> CommandDef {
    CommandDef::new(
        "asdbdump",
        vec![],
        "asdbdump — Dump the processed ACF file (as read).",
        |_args: &[ArgValue], ctx: &CommandContext| {
            // Delegate to the single dump-format owner in
            // `access_security.rs` (shared with the CA gateway R3 report),
            // printing each rendered line through the shell.
            with_config(ctx, |cfg| {
                for line in cfg.dump_report().lines() {
                    ctx.println(line);
                }
            })
        },
    )
}

/// Print the INP links and RULEs of one ASG (used by `asprules`).
/// Delegates to [`AccessSecurityConfig::fmt_asg`] — the single owner of
/// the rule-dump format shared with `asdbdump` and the CA gateway R3
/// report — then prints each rendered line through the shell.
fn print_asg(ctx: &CommandContext, cfg: &AccessSecurityConfig, name: &str) {
    let mut buf = String::new();
    cfg.fmt_asg(name, &mut buf);
    for line in buf.lines() {
        ctx.println(line);
    }
}

/// `aspuag [uagname]` — show members of a UAG, or every UAG.
/// Mirrors C `aspuag` / `asDumpUagFP`.
fn cmd_aspuag() -> CommandDef {
    CommandDef::new(
        "aspuag",
        vec![ArgDesc {
            name: "uagname",
            arg_type: ArgType::String,
            optional: true,
        }],
        "aspuag [uagname] — Show members of a User Access Group (all if omitted).",
        |args: &[ArgValue], ctx: &CommandContext| {
            let filter = match &args[0] {
                ArgValue::String(s) => Some(s.as_str()),
                _ => None,
            };
            with_config(ctx, |cfg| {
                let mut names: Vec<_> = cfg.uag.keys().collect();
                names.sort();
                for name in names {
                    if filter.is_some_and(|f| f != name) {
                        continue;
                    }
                    ctx.println(&format!("UAG({name})"));
                    for m in &cfg.uag[name] {
                        ctx.println(&format!("\t{m}"));
                    }
                }
            })
        },
    )
}

/// `asphag [hagname]` — show members of a HAG, or every HAG.
/// Mirrors C `asphag` / `asDumpHagFP`.
fn cmd_asphag() -> CommandDef {
    CommandDef::new(
        "asphag",
        vec![ArgDesc {
            name: "hagname",
            arg_type: ArgType::String,
            optional: true,
        }],
        "asphag [hagname] — Show members of a Host Access Group (all if omitted).",
        |args: &[ArgValue], ctx: &CommandContext| {
            let filter = match &args[0] {
                ArgValue::String(s) => Some(s.as_str()),
                _ => None,
            };
            with_config(ctx, |cfg| {
                let mut names: Vec<_> = cfg.hag.keys().collect();
                names.sort();
                for name in names {
                    if filter.is_some_and(|f| f != name) {
                        continue;
                    }
                    ctx.println(&format!("HAG({name})"));
                    for h in &cfg.hag[name] {
                        ctx.println(&format!("\t{h}"));
                    }
                }
            })
        },
    )
}

/// `asprules [asgname]` — list rules of an ASG, or every ASG.
/// Mirrors C `asprules` / `asDumpRulesFP`.
fn cmd_asprules() -> CommandDef {
    CommandDef::new(
        "asprules",
        vec![ArgDesc {
            name: "asgname",
            arg_type: ArgType::String,
            optional: true,
        }],
        "asprules [asgname] — List rules of an Access Security Group (all if omitted).",
        |args: &[ArgValue], ctx: &CommandContext| {
            let filter = match &args[0] {
                ArgValue::String(s) => Some(s.as_str()),
                _ => None,
            };
            with_config(ctx, |cfg| {
                let mut names: Vec<_> = cfg.asg.keys().collect();
                names.sort();
                for name in names {
                    if filter.is_some_and(|f| f != name) {
                        continue;
                    }
                    ctx.println(&format!("ASG({name})"));
                    print_asg(ctx, cfg, name);
                }
            })
        },
    )
}

/// `aspmem [asgname] [clients]` — list members (records) of an ASG.
///
/// C `aspmem` walks the live `asgMemberList`. This crate has no
/// AS-member registry; it derives membership by scanning every record
/// for its `ASG` field — the same record→ASG mapping. The `clients`
/// flag (show attached CA clients) is accepted for syntax parity but
/// has no effect: per-member CA-client tracking is not modelled here.
fn cmd_aspmem() -> CommandDef {
    CommandDef::new(
        "aspmem",
        vec![
            ArgDesc {
                name: "asgname",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "clients",
                arg_type: ArgType::Int,
                optional: true,
            },
        ],
        "aspmem [asgname] [clients] — List records that are members of an ASG.",
        |args: &[ArgValue], ctx: &CommandContext| {
            let filter = match &args[0] {
                ArgValue::String(s) => Some(s.clone()),
                _ => None,
            };
            if matches!(&args[1], ArgValue::Int(n) if *n != 0) {
                ctx.println("aspmem: per-member CA-client listing is not available in this IOC");
            }
            // Group every record by its ASG field.
            let names = ctx.block_on(ctx.db().all_record_names());
            let mut by_asg: std::collections::BTreeMap<String, Vec<String>> =
                std::collections::BTreeMap::new();
            for rec_name in &names {
                if let Some(rec) = ctx.db().get_record(rec_name) {
                    let inst = rec.read();
                    by_asg
                        .entry(inst.common.access_group().to_string())
                        .or_default()
                        .push(rec_name.clone());
                }
            }
            for (asg, members) in &by_asg {
                if filter.as_deref().is_some_and(|f| f != asg) {
                    continue;
                }
                ctx.println(&format!("ASG({asg})"));
                for m in members {
                    ctx.println(&format!("\t{m}"));
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `astac <record> <user> <host>` — show the read/write permission
/// `user:host` would have on `record`. Mirrors C `astac`.
fn cmd_astac() -> CommandDef {
    CommandDef::new(
        "astac",
        vec![
            ArgDesc {
                name: "recordname",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "user",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "host",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "astac <record> <user> <host> — Show the access user:host would have on a PV.",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C `asDbLib.c:243-246`: any argument the shell did not
            // supply is a usage line on stdout, not a shell error.
            let (record, user, host) = match (&args[0], &args[1], &args[2]) {
                (ArgValue::String(r), ArgValue::String(u), ArgValue::String(h)) => {
                    (r.clone(), u.clone(), h.clone())
                }
                _ => {
                    ctx.println("Usage: astac \"record name\", \"user\", \"host\"");
                    return Ok(CommandOutcome::Continue);
                }
            };
            // Resolve the record's ASG and ASL.
            let (asg, asl) = match ctx.db().get_record(&record) {
                Some(rec) => {
                    let inst = rec.read();
                    (inst.common.access_group().to_string(), inst.common.asl)
                }
                None => {
                    ctx.println(&format!("astac: record '{record}' not found"));
                    return Ok(CommandOutcome::Continue);
                }
            };
            with_config(ctx, |cfg| {
                let level = cfg.check_access_asl(&asg, &host, &user, asl);
                let perm = match level {
                    AccessLevel::NoAccess => "NoAccess",
                    AccessLevel::Read => "Read",
                    AccessLevel::ReadWrite => "ReadWrite",
                };
                ctx.println(&format!(
                    "{record} ASG({asg}) ASL={asl} {user}@{host}: {perm}"
                ));
            })
        },
    )
}

/// `ascar <level>` — report on the PVs used in `INP*()` rules.
///
/// C `ascar` walks the CA channels opened for `INP*` links. This
/// crate stores the `INP*` link strings but does not open CA channels
/// for them (CALC rules are disabled — see access_security.rs), so
/// the report lists the declared INP links and notes they are not
/// connected. The `level` argument is accepted for syntax parity.
fn cmd_ascar() -> CommandDef {
    CommandDef::new(
        "ascar",
        vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
            optional: true,
        }],
        "ascar [level] — Report status of PVs used in INP*() Access Security rules.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            with_config(ctx, |cfg| {
                let mut total = 0usize;
                let mut asgs: Vec<_> = cfg.asg.keys().collect();
                asgs.sort();
                for name in asgs {
                    for inp in &cfg.asg[name].inp {
                        let letter = (b'A' + inp.index) as char;
                        ctx.println(&format!(
                            "ASG({name}) INP{letter} \"{}\" — not connected",
                            inp.link
                        ));
                        total += 1;
                    }
                }
                ctx.println(&format!(
                    "ascar: {total} INP link(s) declared; \
                     CALC-rule channels are not opened by this IOC"
                ));
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::database::PvDatabase;
    use std::io::Write;
    use std::sync::Arc;

    fn make_ctx() -> (Arc<PvDatabase>, CommandContext) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(PvDatabase::new());
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        let ctx = CommandContext::new(db.clone(), bridge);
        std::mem::forget(rt);
        (db, ctx)
    }

    /// asInit before asSetFilename is a success no-op: C `asInitCommon`
    /// (asDbLib.c:127-128) returns 0 with no filename, leaving access
    /// security disabled rather than failing. It must return Continue so
    /// a startup script under `on error break` is not aborted, and must
    /// not install any config.
    #[test]
    fn as_init_without_filename_is_noop_success() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get("asInit").unwrap();
        let args = parse_args(&[], &cmd.args).unwrap();
        let outcome = cmd.handler.call(&args, &ctx);
        assert!(
            matches!(outcome, Ok(CommandOutcome::Continue)),
            "asInit with no filename must be a Continue no-op, not an error"
        );
        assert!(
            ctx.acf().load().is_none(),
            "no filename must leave access security disabled (no config)"
        );
    }

    /// asSetFilename + asInit loads an ACF; asprules then dumps it.
    #[test]
    fn as_set_filename_then_init_loads_acf() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);

        let tmp = tempfile::Builder::new().suffix(".acf").tempfile().unwrap();
        writeln!(
            tmp.as_file(),
            "UAG(ops) {{ alice }}\nASG(DEFAULT) {{ RULE(1, WRITE) {{ UAG(ops) }} }}"
        )
        .unwrap();

        let set = reg.get("asSetFilename").unwrap();
        let a = parse_args(&[tmp.path().to_string_lossy().into()], &set.args).unwrap();
        assert!(set.handler.call(&a, &ctx).is_ok());

        let init = reg.get("asInit").unwrap();
        let a = parse_args(&[], &init.args).unwrap();
        assert!(init.handler.call(&a, &ctx).is_ok());

        // Config is now active in the context's live cell — the one
        // the IOC's servers gate on.
        assert!(ctx.acf().load().is_some());
    }

    /// ACF substitutions expand per line (C `asLibRoutines.c:202-219` feeds
    /// `macExpandString` one `fgets` line at a time): an apostrophe in a
    /// comment must not suppress `$(...)` on the lines after it.
    #[test]
    fn as_init_expands_substitutions_after_a_comment_apostrophe() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);

        let tmp = tempfile::Builder::new().suffix(".acf").tempfile().unwrap();
        writeln!(
            tmp.as_file(),
            "# the operators' group\nUAG($(G)) {{ alice }}\n\
             ASG(DEFAULT) {{ RULE(1, WRITE) {{ UAG($(G)) }} }}"
        )
        .unwrap();

        let set = reg.get("asSetFilename").unwrap();
        let a = parse_args(&[tmp.path().to_string_lossy().into()], &set.args).unwrap();
        assert!(set.handler.call(&a, &ctx).is_ok());

        let subs = reg.get("asSetSubstitutions").unwrap();
        let a = parse_args(&["G=ops".into()], &subs.args).unwrap();
        assert!(subs.handler.call(&a, &ctx).is_ok());

        let init = reg.get("asInit").unwrap();
        let a = parse_args(&[], &init.args).unwrap();
        assert!(
            init.handler.call(&a, &ctx).is_ok(),
            "the comment's apostrophe must not leave $(G) unexpanded"
        );
        let config = ctx.acf().load();
        let config = config.as_ref().expect("config must be active");
        assert!(
            config.uag.contains_key("ops"),
            "UAG($(G)) must have expanded to UAG(ops)"
        );
    }

    /// asInit on a malformed ACF surfaces the parse error.
    #[test]
    fn as_init_bad_acf_errors() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);

        let tmp = tempfile::Builder::new().suffix(".acf").tempfile().unwrap();
        writeln!(tmp.as_file(), "ASG(X) {{ RULE(-1, READ) }}").unwrap();

        let set = reg.get("asSetFilename").unwrap();
        let a = parse_args(&[tmp.path().to_string_lossy().into()], &set.args).unwrap();
        set.handler.call(&a, &ctx).unwrap();

        let init = reg.get("asInit").unwrap();
        let a = parse_args(&[], &init.args).unwrap();
        assert!(
            init.handler.call(&a, &ctx).is_err(),
            "negative RULE level must fail asInit"
        );
    }
}
