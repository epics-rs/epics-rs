// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the exec-backend suite.
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
use crate::runtime::log::ERL_ERROR;
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
    /// C's `firstTime` (`asDbLib.c:113`), inverted so that `Default`
    /// carries C's initial value. `asInitCommon` flips it through an
    /// `epicsThreadOnce` at the TOP of the function (`:122`), before any
    /// branch, so it is set even on the early return — and the flip is what
    /// splits the silent "access security will NEVER be turned on" of a
    /// first bare `asInit` from the diagnosed refusal every later one makes.
    as_init_ran: bool,
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
    registry.register(cmd_as_dump_hash());
}

/// C registers `asCheckClientIP` as an iocsh *variable*
/// (`libComRegister.c:475-479`, `:518-520` at `R7.0.10`), so a startup script says
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
            // C `asSetFilenameArg0` (`asIocRegister.c:19`).
            name: "ascf",
            arg_type: ArgType::Path,
        }],
        "asSetFilename <ascf> — Set path+file of ACF file. Run asInit to (re)load.",
        |args: &[ArgValue], ctx: &CommandContext| {
            // C frees `pacf` unconditionally and only re-allocates when `acf`
            // is non-NULL (`asDbLib.c:70-89`), so an argument-less call CLEARS
            // the deferred path and returns 0 — no warning, no failed line.
            let ArgValue::String(path) = &args[0] else {
                as_state().lock().unwrap().filename = None;
                return Ok(CommandOutcome::Continue);
            };
            let path = path.clone();
            // C `asDbLib.c:79-83`: the ONLY thing this command prints, and
            // only for a path that will be resolved against the IOC's
            // working directory at `asInit` time rather than against the
            // startup script. `strchr(pacf, ':')` is C's, and it spares a
            // Windows drive letter as much as a `host:path`.
            if !path.starts_with('/') && !path.contains(':') {
                ctx.println("asSetFilename: Warning - relative paths won't usually work");
            }
            as_state().lock().unwrap().filename = Some(path);
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
        }],
        "asSetSubstitutions <subs> — Set substitutions used when reading the ACF file.",
        |args: &[ArgValue], _ctx: &CommandContext| {
            // Same shape as `asSetFilename` above: C stores NULL and returns 0
            // (`asDbLib.c:91-105`), which clears any substitutions a previous
            // call left behind.
            let ArgValue::String(subs) = &args[0] else {
                as_state().lock().unwrap().substitutions = None;
                return Ok(CommandOutcome::Continue);
            };
            as_state().lock().unwrap().substitutions = Some(subs.clone());
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Which of C `asInitCommon`'s four exits one `asInit` took
/// (`asDbLib.c:118-149`).
///
/// A type rather than a status integer because the two callers render the
/// same exit differently — the command through the shell's sink, the build
/// through the errlog — while the *decision* about it (did the call fail?
/// what does C print?) has to be one thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AsInitOutcome {
    /// C `:141` — `asInitFile` read the ACF and it is now live.
    Loaded,
    /// C `:127` — a first call with no `asSetFilename`. Status 0, silent:
    /// access security "will NEVER be turned on" and the caller runs on.
    NeverEnabled,
    /// C `:132-135` `S_asLib_asNotActive` — a RE-init of access security
    /// that never came up. The one case C diagnoses, with [`Self::message`].
    NotActive,
    /// C `:138` `S_asLib_badConfig` — a re-init with nothing to re-read.
    /// C leaves everything as it is and says nothing about it.
    BadConfig,
    /// C `:141` the other way — `asInitFile` returned non-zero. It reported
    /// the reason itself before returning ([`as_init_file`]), so like C's
    /// status this variant carries no text.
    LoadFailed,
}

impl AsInitOutcome {
    /// C's non-zero return, which fails the iocsh line through
    /// `iocshSetError` (`asIocRegister.c:49`) and fails the build through
    /// `iocBuild_2` (`iocInit.c:187`).
    pub(crate) fn failed(self) -> bool {
        !matches!(self, Self::Loaded | Self::NeverEnabled)
    }

    /// What C prints for this exit, if anything. `asInitCommon` has exactly
    /// one `printf` in its whole body; owning the sentence here is what
    /// keeps the shell and the build from spelling it differently.
    pub(crate) fn message(self) -> Option<&'static str> {
        match self {
            Self::NotActive => Some(
                "Access security is NOT enabled. \
                 Was asSetFilename specified before iocInit?",
            ),
            Self::Loaded | Self::NeverEnabled | Self::BadConfig | Self::LoadFailed => None,
        }
    }
}

/// C `asInitCommon` (`asDbLib.c:118-149`) with no shell around it.
///
/// Its own function because C has TWO callers — the iocsh command
/// (`asIocRegister.c:49`) and `iocBuild_2` (`iocInit.c:187`) — and this
/// port had only the first. `softioc-rs` stood in for the second by queuing
/// a literal `asInit` line after the argv-derived ones, which ran it BEFORE
/// the startup script instead of after it, so an `st.cmd` that named its
/// own ACF and left the load to `iocInit` ran with access security off.
///
/// Infallible from the caller's side, as C's `asInitCommon` is: the file
/// being unreadable or unparseable is [`AsInitOutcome::LoadFailed`], and
/// [`as_init_file`] has already written the reason where C writes it.
pub(crate) fn as_init(acf: &crate::server::access_security::AcfCell) -> AsInitOutcome {
    // Snapshot the deferred config under the lock, release it before file
    // I/O so the lock is not held across syscalls. The `firstTime` flip goes
    // with that snapshot, as C's `epicsThreadOnce` does.
    let (was_first_time, filename, substitutions) = {
        let mut st = as_state().lock().unwrap();
        let was_first_time = !st.as_init_ran;
        st.as_init_ran = true;
        (
            was_first_time,
            st.filename.clone(),
            st.substitutions.clone(),
        )
    };
    if !was_first_time && acf.load().is_none() {
        return AsInitOutcome::NotActive;
    }
    let Some(filename) = filename else {
        return if was_first_time {
            AsInitOutcome::NeverEnabled
        } else {
            AsInitOutcome::BadConfig
        };
    };
    let Some(config) = as_init_file(&filename, substitutions.as_deref()) else {
        return AsInitOutcome::LoadFailed;
    };
    // Publish into the IOC's live policy cell — the store fires the
    // process-wide change notification, so live CA clients re-evaluate their
    // ACCESS_RIGHTS and policy caches drop (C: `asInitialize` swaps `pasbase`
    // and re-computes every ASGCLIENT, asLibRoutines.c `asInitCommon`).
    acf.store(Some(std::sync::Arc::new(config)));
    AsInitOutcome::Loaded
}

/// C `asInitFile` (`asLibRoutines.c:174-190`): read the ACF, expand it, and
/// parse it — reporting its own failure on stderr and handing the caller
/// nothing but the status, exactly as C does.
///
/// The reason lives here rather than travelling out as a `String` because C
/// has two callers for it and they must not word it differently: the shell
/// command is `iocshSetError(asInit())` (`asIocRegister.c:49`), which prints
/// nothing of its own, and `iocBuild_2` prints only its own two lines
/// (`iocInit.c:188-190`). While the reason travelled out, each caller
/// invented a sentence C never writes.
///
/// `None` is C's `S_asLib_badConfig`, already reported.
fn as_init_file(filename: &str, substitutions: Option<&str>) -> Option<AccessSecurityConfig> {
    // C `asLibRoutines.c:179-182`: `fopen` failed. Straight to stderr and
    // not through errlog, as C's `fprintf(stderr, ...)` is — that is what
    // keeps `ERL_ERROR`'s escapes on the stream whether or not it is a
    // terminal (see [`ERL_ERROR`]) — and with no OS error appended, because
    // C prints none.
    //
    // C diagnoses only the `fopen`; a failure part-way through the read is
    // a NULL `fgets` to it, which `myInputFunction` reports as end of input
    // (`asLibRoutines.c:222`) and the parser then accepts as a short file.
    // One `read_to_string` cannot tell those apart, so a mid-read failure
    // takes this line too rather than silently activating a truncated ACF.
    // A directory named as the ACF lands here for the same reason (`Err`
    // from `read_to_string`), which is the answer an operator needs.
    //
    // C also has a second diagnostic this port has no place for:
    // `asLibRoutines.c:184-189` checks `fclose` and reports
    // `asInitFile: fclose failed!`. `read_to_string` closes the descriptor
    // as it drops the handle and Rust's `File` has no fallible close, so
    // reproducing it would mean opening the file through `libc` and closing
    // the raw fd by hand — new `unsafe` for a status the whole content has
    // already been read past. Deliberately absent.
    let raw = match std::fs::read_to_string(filename) {
        Ok(raw) => raw,
        Err(_) => {
            eprintln!("{}", cant_open_file(filename));
            return None;
        }
    };
    let content = match substitutions {
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
    match parse_acf(&content) {
        Ok(config) => Some(config),
        Err(e) => {
            // C's parse diagnostics come from the parser itself, on stderr
            // and behind `ERL_ERROR` (`asLib.y:336-344`, `asLib_lex.l`), not
            // from `asInitFile`. `parse_acf` reports nothing, so its message
            // stands in for `yyerror`'s body here: the prefix and the stream
            // are C's, the sentence and its `ACF line N:` location are this
            // port's — more specific than C's bare `syntax error`, and kept
            // for that reason.
            eprintln!("{ERL_ERROR} {e}");
            None
        }
    }
}

/// The exact bytes C `asInitFile` writes when `fopen` fails
/// (`asLibRoutines.c:180`), minus the trailing newline.
///
/// A function so the line can be pinned byte for byte against a measured
/// `softIoc` run without capturing the process stderr, the way
/// `format_show_error` pins `iocsh.cpp`'s.
fn cant_open_file(filename: &str) -> String {
    format!("{ERL_ERROR} asInitFile: Can't open file '{filename}'")
}

/// `asInit` — (re)load the ACF file named by `asSetFilename`,
/// applying any `asSetSubstitutions`. Mirrors C `asInit`.
fn cmd_as_init() -> CommandDef {
    CommandDef::new(
        "asInit",
        vec![],
        "asInit — (Re)load the ACF file set by asSetFilename.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            // C `asIocRegister.c:49` is `iocshSetError(asInit())`: the line
            // is marked failed and the command prints nothing beyond what
            // `asInitCommon` printed for itself — `CommandOutcome::Failed`,
            // never `Err`, which would make the shell say it a second time.
            let outcome = as_init(ctx.acf());
            if let Some(message) = outcome.message() {
                ctx.println(message);
            }
            Ok(if outcome.failed() {
                CommandOutcome::Failed
            } else {
                CommandOutcome::Continue
            })
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
            },
            ArgDesc {
                name: "clients",
                arg_type: ArgType::Int,
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
                // C `astacArg0` (`asIocRegister.c:118`).
                name: "recordname",
                arg_type: ArgType::Record,
            },
            ArgDesc {
                name: "user",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "host",
                arg_type: ArgType::String,
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
                    // C `asDbLib.c:249-252`: `errMessage(status,
                    // "dbNameToAddr error")`, which `errlog.h:86-87`
                    // expands to `errPrintf(status, __FILE__, __LINE__,
                    // " %s\n", ...)` and `errlog.c:503-508` renders as
                    // `<errSym> filename="<f>" line number=<n>  <msg>`
                    // on the errlog stream, not on stdout. Measured on
                    // `softIoc` R7.0.10-146: `Record Not Found
                    // filename="../as/asDbLib.c" line number=251
                    // dbNameToAddr error`. `Record Not Found` is
                    // `S_dbLib_recNotFound`'s errSym text
                    // (`dbStaticLib.h:257`), which is what
                    // `dbNameToAddr` returns for an unknown record
                    // (`dbAccess.c:669` -> `dbStaticLib.c:1464`).
                    //
                    // The shape is C's; the location is OURS. Printing
                    // `asDbLib.c:251` would name a file this binary
                    // does not contain, which is the one part of C's
                    // line that must not be copied.
                    crate::runtime::log::errlog_printf(&format!(
                        "Record Not Found filename=\"{}\" line number={}  dbNameToAddr error\n",
                        file!(),
                        line!()
                    ));
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

/// `asDumpHash` — show the contents of the hash table used to locate UAGs
/// and HAGs. C `asIocRegister.c:147-154` → `asDumpHashFP`
/// (`asLibRoutines.c:869-874`), which is
/// `if(!asActive) return 0; gphDumpFP(fp, pasbase->phash);`.
///
/// What that table holds is set at ACF load (`asLibRoutines.c:120-140`): one
/// entry per UAG *member user name* keyed by its UAG, and one per HAG
/// *member host* keyed by its HAG — 256 buckets, `epicsStrHash(name, …)`
/// masked to the table size.
///
/// Two departures from C's dump, both forced:
///
/// * C seeds each entry's hash with `epicsMemHash(&pvtid, sizeof(void*))`
///   — the ADDRESS of the owning UAG/HAG struct — so C's own bucket
///   assignment differs between runs of the same IOC on the same ACF. No
///   client can depend on it. The seed here is a fixed per-kind constant
///   so the dump is reproducible; the distribution is C's hash over C's
///   table size.
/// * C prints `pgphNode->pvtid`, a raw pointer. The port prints the owning
///   group's NAME, which is the thing the pointer identifies and the only
///   part of that column a reader can use.
///
/// C's silence when access security is not active is reproduced exactly:
/// `!asActive` returns before any output, so this prints nothing at all
/// rather than [`with_config`]'s "not loaded" line.
fn cmd_as_dump_hash() -> CommandDef {
    CommandDef::new(
        "asDumpHash",
        vec![],
        "asDumpHash — Show the contents of the hash table used to locate UAGs and HAGs.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            let Some(cfg) = ctx.acf().load().as_ref().cloned() else {
                // C `asDumpHashFP`: `if(!asActive) return(0);` — no output.
                return Ok(CommandOutcome::Continue);
            };
            for line in dump_as_hash(&cfg) {
                ctx.println(&line);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C `gphInitPvt(&pasbasenew->phash, 256)` (`asLibRoutines.c:120`).
const AS_HASH_BUCKETS: usize = 256;

/// Stand-ins for C's `epicsMemHash` of the owning list pointer — one seed
/// per group kind, so a user and a host that spell the same string still
/// land in different buckets as they do in C.
const AS_HASH_SEED_UAG: u32 = 0x7561_6700; // "uag\0"
const AS_HASH_SEED_HAG: u32 = 0x6861_6700; // "hag\0"

/// Render C `gphDumpFP` (`gpHashLib.c:210-242`) over the AS table: the
/// bucket count, then one line per non-empty bucket carrying its entry
/// count and up to three `name owner` pairs before wrapping, then the
/// empty-bucket tally.
fn dump_as_hash(cfg: &AccessSecurityConfig) -> Vec<String> {
    let mut buckets: Vec<Vec<(String, String)>> = vec![Vec::new(); AS_HASH_BUCKETS];
    let mask = (AS_HASH_BUCKETS - 1) as u32;
    let mut push = |member: &str, owner: String, seed: u32| {
        let h = (crate::runtime::stdlib::epics_str_hash(member, seed) & mask) as usize;
        buckets[h].push((member.to_string(), owner));
    };
    let mut uags: Vec<&String> = cfg.uag.keys().collect();
    uags.sort();
    for name in uags {
        for user in &cfg.uag[name] {
            push(user, format!("UAG({name})"), AS_HASH_SEED_UAG);
        }
    }
    let mut hags: Vec<&String> = cfg.hag.keys().collect();
    hags.sort();
    for name in hags {
        for host in &cfg.hag[name] {
            push(host, format!("HAG({name})"), AS_HASH_SEED_HAG);
        }
    }

    let mut out = vec![format!("Hash table has {AS_HASH_BUCKETS} buckets")];
    let mut empty = 0usize;
    for (h, entries) in buckets.iter().enumerate() {
        if entries.is_empty() {
            empty += 1;
            continue;
        }
        // C prints `\n [%3d] %3d  ` then `  %s %p` per entry, breaking the
        // line after every third (`if (!(++i % 3))`).
        let mut line = format!(" [{h:3}] {:3}  ", entries.len());
        for (i, (member, owner)) in entries.iter().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.push(line);
                line = "            ".to_string();
            }
            line.push_str(&format!("  {member} {owner}"));
        }
        out.push(line);
    }
    out.push(format!("{empty} buckets empty."));
    out
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

    /// Run one `as*` command and return `(outcome, everything it printed)`.
    fn run_as(ctx: &CommandContext, name: &str, argv: &[String]) -> (CommandResult, String) {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get(name).unwrap();
        let args = parse_args(argv, &cmd.args).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let mut outcome = None;
        ctx.with_output(std::fs::File::create(&path).unwrap(), || {
            outcome = Some(cmd.handler.call(&args, ctx));
        });
        (outcome.unwrap(), std::fs::read_to_string(&path).unwrap())
    }

    /// C `asSetFilename` (`asDbLib.c:70-88`) prints on exactly one path:
    /// a stored path that is neither rooted nor carries a colon. Both
    /// halves of `*pacf != '/' && !strchr(pacf, ':')` are boundaries here,
    /// and the acknowledgement this used to print on every call was not a
    /// line C has anywhere.
    #[test]
    fn as_set_filename_prints_c_s_relative_path_warning_and_nothing_else() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        const WARNING: &str = "asSetFilename: Warning - relative paths won't usually work\n";

        let (out, printed) = run_as(&ctx, "asSetFilename", &["/etc/acf/site.acf".into()]);
        assert!(out.is_ok());
        assert_eq!(printed, "", "a rooted path is silent");

        let (out, printed) = run_as(&ctx, "asSetFilename", &["host:/acf/site.acf".into()]);
        assert!(out.is_ok());
        assert_eq!(printed, "", "C spares any path holding a colon");

        let (out, printed) = run_as(&ctx, "asSetFilename", &["site.acf".into()]);
        assert!(out.is_ok());
        assert_eq!(printed, WARNING);
    }

    /// C `asInitCommon` (`asDbLib.c:124-142`) prints one line in its whole
    /// body, and only on a RE-init of access security that never came up.
    /// The boundaries are `wasFirstTime` and `asActive`, so both bare calls
    /// are exercised in order: the first is C's silent "will NEVER be
    /// turned on", the second is its refusal.
    #[test]
    fn a_second_bare_as_init_is_c_s_refusal_and_the_first_is_silent() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();

        let (out, printed) = run_as(&ctx, "asInit", &[]);
        assert!(matches!(out, Ok(CommandOutcome::Continue)));
        assert_eq!(printed, "", "C `:127` returns 0 without a word");

        let (out, printed) = run_as(&ctx, "asInit", &[]);
        assert!(
            matches!(out, Ok(CommandOutcome::Failed)),
            "C returns S_asLib_asNotActive into iocshSetError"
        );
        assert_eq!(
            printed,
            "Access security is NOT enabled. Was asSetFilename specified before iocInit?\n"
        );
    }

    /// A load that works says nothing: C `asInitCommon` reaches
    /// `asInitFile` and returns its status without printing, so the summary
    /// this used to write was ours alone.
    #[test]
    fn a_successful_as_init_prints_nothing() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        let tmp = tempfile::Builder::new().suffix(".acf").tempfile().unwrap();
        writeln!(
            tmp.as_file(),
            "UAG(ops) {{ alice }}\nASG(DEFAULT) {{ RULE(1, WRITE) {{ UAG(ops) }} }}"
        )
        .unwrap();

        let (out, printed) = run_as(
            &ctx,
            "asSetFilename",
            &[tmp.path().to_string_lossy().into()],
        );
        assert!(out.is_ok());
        assert_eq!(printed, "");

        let (out, printed) = run_as(&ctx, "asInit", &[]);
        assert!(matches!(out, Ok(CommandOutcome::Continue)));
        assert_eq!(printed, "");
        assert!(ctx.acf().load().is_some());
    }

    /// Run `asDumpHash` and return everything it printed.
    fn run_as_dump_hash(ctx: &CommandContext) -> String {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get("asDumpHash").unwrap();
        let args = parse_args(&[], &cmd.args).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        ctx.with_output(std::fs::File::create(&path).unwrap(), || {
            cmd.handler.call(&args, ctx).unwrap();
        });
        std::fs::read_to_string(&path).unwrap()
    }

    /// Load `acf` through asSetFilename/asInit.
    fn load_acf(ctx: &CommandContext, acf: &str) -> tempfile::NamedTempFile {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let tmp = tempfile::Builder::new().suffix(".acf").tempfile().unwrap();
        write!(tmp.as_file(), "{acf}").unwrap();
        let set = reg.get("asSetFilename").unwrap();
        let a = parse_args(&[tmp.path().to_string_lossy().into()], &set.args).unwrap();
        set.handler.call(&a, ctx).unwrap();
        let init = reg.get("asInit").unwrap();
        let a = parse_args(&[], &init.args).unwrap();
        init.handler.call(&a, ctx).unwrap();
        tmp
    }

    /// Boundary: access security not active. C `asDumpHashFP` is
    /// `if(!asActive) return(0);` before it touches the table, so the
    /// observable is a completely silent command — not the "not loaded"
    /// line every other AS dump command prints.
    #[test]
    fn as_dump_hash_prints_nothing_when_access_security_is_inactive() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        assert_eq!(
            run_as_dump_hash(&ctx),
            "",
            "C returns before any output when asActive is false"
        );
    }

    /// Boundary: a loaded ACF. `gphDumpFP` prints the table size, one line
    /// per non-empty bucket, and the empty tally — and the entries are the
    /// UAG member users and HAG member hosts, each against its owning group.
    #[test]
    fn as_dump_hash_lists_every_uag_user_and_hag_host_against_its_group() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        let _tmp = load_acf(
            &ctx,
            "UAG(ops) { alice, bob }\n\
             HAG(consoles) { host.example.org }\n\
             ASG(DEFAULT) { RULE(1, WRITE) { UAG(ops) HAG(consoles) } }\n",
        );
        let out = run_as_dump_hash(&ctx);
        assert!(
            out.starts_with(&format!("Hash table has {AS_HASH_BUCKETS} buckets\n")),
            "gpHashLib.c:214 prints the table size first: {out}"
        );
        for (member, owner) in [
            ("alice", "UAG(ops)"),
            ("bob", "UAG(ops)"),
            ("host.example.org", "HAG(consoles)"),
        ] {
            assert!(
                out.contains(&format!("  {member} {owner}")),
                "{member} must appear against {owner}: {out}"
            );
        }
        // The bucket lines and the tally must account for the whole table.
        let empty: usize = out
            .lines()
            .last()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let used = out
            .lines()
            .filter(|l| l.trim_start().starts_with('['))
            .count();
        assert_eq!(
            used + empty,
            AS_HASH_BUCKETS,
            "every bucket is either listed or counted empty: {out}"
        );
    }

    /// Boundary: the same spelling in both kinds of group. C keys each node
    /// on the address of its owning UAG/HAG list, so `ops` as a user and
    /// `ops` as a host are two distinct nodes in the table — never one
    /// deduplicated entry. Which buckets they land in is not asserted:
    /// C's own answer comes from two heap addresses, and two distinct keys
    /// may share a bucket in either implementation.
    #[test]
    fn a_name_used_as_both_a_user_and_a_host_is_two_entries() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        let _tmp = load_acf(
            &ctx,
            "UAG(u) { same }\nHAG(h) { same }\n\
             ASG(DEFAULT) { RULE(1, WRITE) { UAG(u) HAG(h) } }\n",
        );
        let out = run_as_dump_hash(&ctx);
        assert!(out.contains("  same UAG(u)"), "the user node: {out}");
        assert!(out.contains("  same HAG(h)"), "the host node: {out}");
        let entries: usize = out
            .lines()
            .filter(|l| l.trim_start().starts_with('['))
            .map(|l| {
                l.split(']')
                    .nth(1)
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            })
            .sum();
        assert_eq!(entries, 2, "two nodes, not one deduplicated one: {out}");
    }

    /// Boundary: `gphDumpFP` breaks the line after every third entry
    /// (`if (!(++i % 3))` at `gpHashLib.c:232`), so a bucket holding four
    /// entries prints two lines. Four colliding names are found by the same
    /// hash the dump uses rather than hard-coded, so the case survives a
    /// change of table size.
    #[test]
    fn a_bucket_with_four_entries_wraps_after_the_third() {
        let mask = (AS_HASH_BUCKETS - 1) as u32;
        let mut by_bucket: std::collections::HashMap<u32, Vec<String>> =
            std::collections::HashMap::new();
        let mut collide: Option<Vec<String>> = None;
        for i in 0..20_000u32 {
            let name = format!("u{i}");
            let h = crate::runtime::stdlib::epics_str_hash(&name, AS_HASH_SEED_UAG) & mask;
            let v = by_bucket.entry(h).or_default();
            v.push(name);
            if v.len() == 4 {
                collide = Some(v.clone());
                break;
            }
        }
        let names = collide.expect("four names must collide within 20000 candidates");

        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        let _tmp = load_acf(
            &ctx,
            &format!(
                "UAG(g) {{ {} }}\nASG(DEFAULT) {{ RULE(1, WRITE) {{ UAG(g) }} }}\n",
                names.join(", ")
            ),
        );
        let out = run_as_dump_hash(&ctx);
        let bucket_lines: Vec<&str> = out
            .lines()
            .filter(|l| names.iter().any(|n| l.contains(&format!("  {n} UAG(g)"))))
            .collect();
        assert_eq!(
            bucket_lines.len(),
            2,
            "four entries print on two lines: {out}"
        );
        assert!(
            bucket_lines[0].trim_start().starts_with("[") && bucket_lines[0].contains("  4  "),
            "the first line carries the bucket index and its count of 4: {out}"
        );
        assert!(
            bucket_lines[1].starts_with("            "),
            "the continuation line is indented past the count column: {out}"
        );
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

    /// asInit on a malformed ACF fails the line. C's wrapper is
    /// `iocshSetError(asInit())` (`asIocRegister.c:49`), so the failure
    /// arrives as `CommandOutcome::Failed` — the shell adds no sentence of
    /// its own on top of the parser's.
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
            matches!(init.handler.call(&a, &ctx), Ok(CommandOutcome::Failed)),
            "negative RULE level must fail asInit, silently"
        );
    }

    /// C `asLibRoutines.c:180`, byte for byte.
    ///
    /// Measured against `softIoc` R7.0.10.1-DEV (`-a /no/such.acf -d x.db`,
    /// stderr to a file, so the escapes below are the ones a NON-terminal
    /// stream receives — C's `fprintf(stderr, ...)` never passes errlog's
    /// `errlogStripANSI`):
    ///
    /// ```text
    /// ^[[31;1mERROR^[[0m asInitFile: Can't open file '/no/such.acf'
    /// ```
    ///
    /// The port said `asInit: cannot read '<f>': No such file or directory
    /// (os error 2)` — wrong function name, wrong wording, no severity word,
    /// and an OS error C does not print.
    #[test]
    fn the_unreadable_acf_line_is_c_s() {
        assert_eq!(
            cant_open_file("/no/such.acf"),
            "\u{1b}[31;1mERROR\u{1b}[0m asInitFile: Can't open file '/no/such.acf'"
        );
        // C's literal is `ERL_ERROR " asInitFile: ..."` — one space between
        // the severity word and the function name, not `": "`.
        assert_eq!(
            cant_open_file("x"),
            format!("{ERL_ERROR} asInitFile: Can't open file 'x'")
        );
    }

    /// An ACF that cannot be read leaves `asInit` failing with nothing for
    /// the caller to re-word: C's `asInitFile` printed the reason and
    /// returned `S_asLib_badConfig`, and both callers see only the status.
    #[test]
    fn an_unreadable_acf_fails_as_init_without_a_message() {
        let _guard = as_state_test_guard();
        reset_as_state_for_test();
        let (_db, ctx) = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);

        let set = reg.get("asSetFilename").unwrap();
        let a = parse_args(&["/no/such/directory/nope.acf".into()], &set.args).unwrap();
        set.handler.call(&a, &ctx).unwrap();

        let outcome = as_init(ctx.acf());
        assert_eq!(outcome, AsInitOutcome::LoadFailed);
        assert!(outcome.failed());
        assert_eq!(outcome.message(), None);
        assert!(
            ctx.acf().load().is_none(),
            "a refused file must not activate access security"
        );
    }
}
