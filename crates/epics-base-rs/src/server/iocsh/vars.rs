// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the exec-backend suite.
//! The iocsh *variable* table and the `var` command — C
//! `iocshRegisterVariable` (`iocsh.cpp:715-765`) and `varCallFunc` /
//! `varHandler` (`iocsh.cpp:1388-1471`).
//!
//! C keeps variables in a table separate from the command table: an
//! entry is a name, a type and a pointer straight at a process global,
//! and `var` reads or writes through that pointer. A knob that C
//! registers this way is spelled `var <name> <value>` in a startup
//! script, never as a command of its own.
//!
//! # What a softIoc has here that this table does not
//!
//! Measured on `softIoc` R7.0.10: a bare `var` lists 24 names, and the
//! same command here lists one. The 24 arrive by two routes —
//! `registerAllRecordDeviceDrivers.cpp` copies the 22 `variable()` lines
//! of `softIoc.dbd` into the iocsh table, and `libComRegister.c:518-520`
//! adds `asCheckClientIP` and `freeListBypass` directly. The gap is not a
//! registration oversight; it is that a name here must be backed by a
//! settable global, and for most of C's it names something this port
//! either implements with the default hard-wired or does not have at all.
//! Each of the 24, with its reason:
//!
//! **Registered here (11).** `asCheckClientIP`, from
//! `access_commands`; `dbRecordsOnceOnly` and `dbQuietMacroWarnings`,
//! from `commands`; and the eight knobs `seeded_knobs` seeds:
//! `boHIGHlimit`, `boHIGHprecision`, `calcoutODLYlimit`,
//! `calcoutODLYprecision`, `histogramSDELprecision`, `seqDLYlimit`,
//! `seqDLYprecision` and `callbackParallelThreadsDefault`. All eight were
//! a `const` or a computing function until they were registered; a name
//! in this table has to reach a global something actually reads, or `var
//! seqDLYlimit 5000` lists and echoes a value nothing uses, which looks
//! like it worked. Three of them are C `double`s, which is what
//! [`crate::server::iocsh::vars::VarAccess::Double`] is for.
//!
//! **C `printf` debug switches with no counterpart here (7).**
//! `asCaDebug` (`asCa.c:102`), `atExitDebug` (`epicsExit.c:91`),
//! `CASDEBUG` (`camessage.c:328`), `dbAccessDebugPUTF`
//! (`dbAccess.c:1269`), `dbJLinkDebug` (`dbJLink.c:33`), `lnkDebug_debug`
//! (`lnkDebug.c:33`), `logClientDebug` (`logClient.c:73`). Every one
//! guards `printf`/`errlogPrintf` statements in its own C file, and this
//! port carries no equivalent statements for the knob to switch on.
//!
//! **Behaviour this port does not have (6).** `dbBptNotMonotonic` — the
//! lax breakpoint-table rules it opts into are not modelled, so the
//! strict ones always apply ([`crate::server::cvt_bpt`]).
//! `dbConvertStrict` — it picks between two string→field parsers
//! (`dbStaticRun.c:409-430`) and there is only one here.
//! `dbRecordsAbcSorted` — it sorts the record list
//! as the database loads (`dbLexRoutines.c:334`). `dbTemplateMaxVars` —
//! it bounds a fixed-size array in `dbLoadTemplate.y:159`, and the
//! substitution loader here has no such array. `dbThreadRealtimeLock` —
//! it gates the `mlockall` at `iocInit.c:225`, and nothing in this
//! workspace calls `mlockall`. `freeListBypass` — it turns off the
//! recycling pool in `freeListLib.c:61`, which Rust allocation replaces.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

use super::registry::*;

/// How `var` reaches one knob. C's `iocshVarDef` carries a raw `pval`
/// into the global; a Rust port cannot hand out that pointer, so an
/// entry carries the accessor pair for the same global instead. The
/// knob therefore stays the single owner of its value — the table
/// holds no copy that could drift out of step with it.
///
/// C's `varHandler` also has a runtime `default:` arm refusing every
/// `iocshArg*` type that is not int or double; here the type is the
/// enum, so an unhandled type cannot be constructed.
pub enum VarAccess {
    /// C `iocshArgInt`, a knob whose global is a C `int`. The accessors
    /// widen to `i64` so one arm serves every int-shaped knob; the
    /// truncation back to the knob's own width happens in its setter,
    /// as C's `*(int *)v->pval = ltmp` truncates a `long`.
    Int { get: fn() -> i64, set: fn(i64) },
    /// C `iocshArgDouble`.
    Double { get: fn() -> f64, set: fn(f64) },
}

/// One entry of the iocsh variable table.
pub struct VarDef {
    pub name: &'static str,
    pub access: VarAccess,
}

/// C's `iocshVariableHead` is a process global kept sorted by name, and
/// so is this — `var` with no argument lists in name order.
static VARIABLES: LazyLock<Mutex<BTreeMap<&'static str, VarDef>>> = LazyLock::new(|| {
    Mutex::new(
        seeded_knobs()
            .into_iter()
            .map(|def| (def.name, def))
            .collect(),
    )
});

/// The knobs C registers from a `.dbd` `variable()` line rather than from
/// a C file — seven out of the record `.dbd`s via the generated
/// `<app>_registerRecordDeviceDriver` (`registerRecordDeviceDriver.pl:270`)
/// and `callbackParallelThreadsDefault` out of `dbCore.dbd:32`.
/// This port resolves its vendored `.dbd`s at build time and has no such
/// runtime registrar to hang them off, so the table is born holding them;
/// that is what keeps it and
/// [`crate::server::record::dbd_generated::VARIABLES`] in step by
/// construction instead of by somebody calling a registration function in
/// the right order.
///
/// Each accessor pair reaches the same global its record reads, and reads
/// it where C does — inside the `get_precision` / `get_control_double`
/// equivalent, per request. So `var seqDLYlimit 5000` changes what the
/// NEXT client request reports about `DLYn`, on records that already
/// exist, exactly as C's `seqRecord.c:342-353` does by loading the global
/// on every call rather than caching it at init.
fn seeded_knobs() -> Vec<VarDef> {
    use crate::server::records::{bo, calcout, histogram, seq};
    vec![
        // C `callbackParallelThreadsDefault` — from `dbCore.dbd:32`
        // rather than a record `.dbd`, but the same shape of knob: a
        // process `int` that `callbackParallelThreads(0, ...)` reads at
        // the point of use (`callback.c:170`).
        VarDef {
            name: "callbackParallelThreadsDefault",
            access: VarAccess::Int {
                get: || {
                    crate::runtime::background::callback_executor::parallel_threads_default() as i64
                },
                set: |value| {
                    crate::runtime::background::callback_executor::set_parallel_threads_default(
                        value as i32,
                    )
                },
            },
        },
        VarDef {
            name: "boHIGHlimit",
            access: VarAccess::Double {
                get: bo::bo_high_limit,
                set: bo::set_bo_high_limit,
            },
        },
        VarDef {
            name: "boHIGHprecision",
            access: VarAccess::Int {
                get: || bo::bo_high_precision() as i64,
                set: |value| bo::set_bo_high_precision(value as i32),
            },
        },
        VarDef {
            name: "calcoutODLYlimit",
            access: VarAccess::Double {
                get: calcout::calcout_odly_limit,
                set: calcout::set_calcout_odly_limit,
            },
        },
        VarDef {
            name: "calcoutODLYprecision",
            access: VarAccess::Int {
                get: || calcout::calcout_odly_precision() as i64,
                set: |value| calcout::set_calcout_odly_precision(value as i32),
            },
        },
        VarDef {
            name: "histogramSDELprecision",
            access: VarAccess::Int {
                get: || histogram::histogram_sdel_precision() as i64,
                set: |value| histogram::set_histogram_sdel_precision(value as i32),
            },
        },
        VarDef {
            name: "seqDLYlimit",
            access: VarAccess::Double {
                get: seq::seq_dly_limit,
                set: seq::set_seq_dly_limit,
            },
        },
        VarDef {
            name: "seqDLYprecision",
            access: VarAccess::Int {
                get: || seq::seq_dly_precision() as i64,
                set: |value| seq::set_seq_dly_precision(value as i32),
            },
        },
    ]
}

/// C `iocshRegisterVariable`. Re-registering the same name is a no-op,
/// which is what C's `found` branch amounts to for a table whose entries
/// all point at their own global.
pub fn register_variable(def: VarDef) {
    VARIABLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(def.name)
        .or_insert(def);
}

/// Every registered variable name, sorted — C's `iocshVariableHead`
/// walk in `iocsh_complete_variable` (`iocsh.cpp:479-500`), which the
/// completer offers for `var`'s first argument.
pub fn variable_names() -> Vec<&'static str> {
    VARIABLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .copied()
        .collect()
}

/// Register the `var` command. C registers it from
/// `iocshRegisterVariable` the first time a variable appears
/// (`iocsh.cpp:725-726`), so a shell with an empty table has no `var`.
/// Kept as C's rule rather than as a live guard: the table here is seeded
/// with [`seeded_knobs`], so it is never empty and `var` is always there,
/// which is also true of any C IOC that loaded a `.dbd` declaring one.
pub(crate) fn register(registry: &mut CommandRegistry) {
    if VARIABLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
    {
        return;
    }
    registry.register(cmd_var());
}

/// C `strtol(s, &endp, 0)` with the `*endp == '\0'` whole-string check
/// `varHandler` applies: leading whitespace is skipped, then an optional
/// sign, then `0x` hex, a leading `0` octal, or decimal. Anything left
/// over rejects the value.
fn strtol_base0(s: &str) -> Option<i64> {
    // C's `strtol` skips leading whitespace before it converts anything,
    // so `var X " 1"` sets X to 1 there and must here.
    let s = s.trim_start();
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let magnitude = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i64::from_str_radix(hex, 16).ok()?
    } else if digits.len() > 1 && digits.starts_with('0') {
        i64::from_str_radix(&digits[1..], 8).ok()?
    } else {
        digits.parse::<i64>().ok()?
    };
    Some(if neg { -magnitude } else { magnitude })
}

/// C `epicsStrtod(s, &endp)` under the same whole-string check
/// (`iocsh.cpp:1426-1435`), so the leading-whitespace rule is
/// [`strtol_base0`]'s and not a second one.
///
/// Rust's `f64` parser takes the decimal, exponent and `inf`/`nan`
/// spellings C's does. It does not take glibc's hex-float form
/// (`0x1p3`), which is the one input C would accept and this rejects as
/// `Invalid double`.
fn strtod_whole(s: &str) -> Option<f64> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    s.parse::<f64>().ok()
}

/// C `varHandler(v, NULL)` — `int %s = %d` / `double %s = %g` on stdout
/// (`iocsh.cpp:1400-1408`). The `%g` is a bare one, so six significant
/// digits, and [`crate::server::records::printf::format_g`] is the port's glibc-differenced printer for
/// it rather than a Rust float format that would spell `100000` as
/// `100000.0`.
fn show(ctx: &CommandContext, def: &VarDef) {
    match &def.access {
        VarAccess::Int { get, .. } => ctx.println(&format!("int {} = {}", def.name, get())),
        VarAccess::Double { get, .. } => ctx.println(&format!(
            "double {} = {}",
            def.name,
            crate::server::records::printf::format_g(get(), 6)
        )),
    }
}

fn cmd_var() -> CommandDef {
    CommandDef::new(
        "var",
        vec![
            ArgDesc {
                name: "[variable",
                arg_type: ArgType::String,
            },
            // C `varCmdArg1` (`iocsh.cpp:704`) carries the opening
            // bracket of the nested optional group, so the pair reads
            // `var [variable [value]]`.
            ArgDesc {
                name: "[value]]",
                arg_type: ArgType::String,
            },
        ],
        concat!(
            "Print all, print single variable or set value to single variable\n",
            "  (default) - print all variables and their values defined in database definitions files\n",
            "  variable  - if only parameter print value for this variable\n",
            "  value     - set the value to variable",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            let name = match args.first() {
                Some(ArgValue::String(s)) => Some(s.as_str()),
                _ => None,
            };
            let value = match args.get(1) {
                Some(ArgValue::String(s)) => Some(s.as_str()),
                _ => None,
            };
            let table = VARIABLES.lock().unwrap_or_else(|e| e.into_inner());

            let Some(value) = value else {
                // No value: the name is an `epicsStrGlobMatch` pattern.
                let mut found = false;
                for def in table.values() {
                    if name.is_none_or(|pattern| {
                        super::commands::epics_strn_glob_match(
                            def.name.as_bytes(),
                            def.name.len(),
                            pattern.as_bytes(),
                        )
                    }) {
                        show(ctx, def);
                        found = true;
                    }
                }
                if !found && let Some(name) = name {
                    return Err(format!("No known vars match '{name}'."));
                }
                return Ok(CommandOutcome::Continue);
            };

            // With a value the name is looked up exactly, not globbed.
            let Some(def) = name.and_then(|name| table.get(name)) else {
                return Err(format!("No known var '{}'.", name.unwrap_or_default()));
            };
            match &def.access {
                VarAccess::Int { set, .. } => match strtol_base0(value) {
                    Some(parsed) => set(parsed),
                    // C leaves the variable alone and prints, but does
                    // not fail the command (`varHandler` never calls
                    // `iocshSetError`).
                    None => {
                        ctx.eprintln(&format!("Invalid integer, var '{}' not changed.", def.name))
                    }
                },
                VarAccess::Double { set, .. } => match strtod_whole(value) {
                    Some(parsed) => set(parsed),
                    None => {
                        ctx.eprintln(&format!("Invalid double, var '{}' not changed.", def.name))
                    }
                },
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::access_security::{as_check_client_ip, set_as_check_client_ip};
    use crate::server::database::PvDatabase;
    use std::sync::Arc;

    fn make_ctx() -> CommandContext {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(PvDatabase::new());
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        let ctx = CommandContext::new(db, bridge);
        std::mem::forget(rt);
        ctx
    }

    fn run_var(ctx: &CommandContext, tokens: &[&str]) -> Result<String, String> {
        let mut reg = CommandRegistry::new();
        super::super::commands::register_builtins(&mut reg);
        let cmd = reg.get("var").expect("`var` must be registered").clone();
        let tokens: Vec<String> = tokens.iter().map(|t| (*t).to_string()).collect();
        let args = parse_args(&tokens, &cmd.args).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let mut failure = None;
        ctx.with_output(std::fs::File::create(&path).unwrap(), || {
            if let Err(e) = cmd.handler.call(&args, ctx) {
                failure = Some(e);
            }
        });
        match failure {
            Some(e) => Err(e),
            None => Ok(std::fs::read_to_string(&path).unwrap()),
        }
    }

    /// C keeps `asCheckClientIP` in the iocsh *variable* table
    /// (`libComRegister.c:475-479`, `:518-520` at `R7.0.10`) and reaches it with
    /// `var`, which prints nothing when setting and `int NAME = V` when
    /// reading (`iocsh.cpp:1388-1471`).
    #[test]
    fn var_reads_and_writes_as_check_client_ip() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        super::super::commands::register_builtins(&mut reg);
        assert!(
            reg.get("var").is_some(),
            "C registers `var` (iocsh.cpp:708)"
        );
        assert!(
            reg.get("asCheckClientIP").is_none(),
            "C registers asCheckClientIP as a variable, never as a command"
        );

        let restore = as_check_client_ip();
        assert_eq!(
            run_var(&ctx, &["asCheckClientIP", "1"]).unwrap(),
            "",
            "C `var` prints nothing when it sets"
        );
        assert!(as_check_client_ip(), "`var` must reach the real knob");
        assert_eq!(
            run_var(&ctx, &["asCheckClientIP"]).unwrap(),
            "int asCheckClientIP = 1\n"
        );
        // A bare name is an `epicsStrGlobMatch` pattern, and a bare
        // `var` lists the whole table.
        assert_eq!(
            run_var(&ctx, &["asCheck*"]).unwrap(),
            "int asCheckClientIP = 1\n"
        );
        assert!(
            run_var(&ctx, &[])
                .unwrap()
                .contains("int asCheckClientIP = 1")
        );

        assert_eq!(
            run_var(&ctx, &["noSuchVar"]).unwrap_err(),
            "No known vars match 'noSuchVar'."
        );
        assert_eq!(
            run_var(&ctx, &["noSuchVar", "1"]).unwrap_err(),
            "No known var 'noSuchVar'."
        );

        // C parses with `strtol(s, &endp, 0)`, and an unparsable value
        // leaves the variable alone without failing the command.
        assert_eq!(run_var(&ctx, &["asCheckClientIP", "0x0"]).unwrap(), "");
        assert!(!as_check_client_ip());
        assert_eq!(run_var(&ctx, &["asCheckClientIP", "nope"]).unwrap(), "");
        assert!(!as_check_client_ip());

        set_as_check_client_ip(restore);
    }

    /// The invariant that keeps this table honest: every name in it must
    /// reach a real global. C's entries hold a pointer straight at one so
    /// the question cannot arise there; here an entry holds an accessor
    /// pair, and nothing in the type stops a knob being registered with a
    /// getter reading a value no setter feeds. `var` would then list and
    /// echo a setting that changes nothing — worse than an absent name,
    /// because it looks like it worked, and it is exactly the shape the
    /// seven record knobs would take if they were registered while their
    /// values are still `const`. This walks whatever is registered rather
    /// than a fixed list, so a knob added later is covered without
    /// touching the test.
    #[test]
    fn every_registered_variable_round_trips_a_write() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        super::super::commands::register_builtins(&mut reg);
        let names = variable_names();
        assert!(!names.is_empty(), "an empty table makes this vacuous");
        for name in names {
            let before = run_var(&ctx, &[name]).unwrap();
            // 0 and 1 rather than two arbitrary numbers: a knob backed by
            // a bool stores every non-zero value identically.
            run_var(&ctx, &[name, "0"]).unwrap();
            let zero = run_var(&ctx, &[name]).unwrap();
            run_var(&ctx, &[name, "1"]).unwrap();
            let one = run_var(&ctx, &[name]).unwrap();
            assert_ne!(zero, one, "`var {name}` does not reach a real global");

            let original = before
                .rsplit_once(" = ")
                .expect("C prints `<type> <name> = <value>`")
                .1
                .trim();
            run_var(&ctx, &[name, original]).unwrap();
            assert_eq!(run_var(&ctx, &[name]).unwrap(), before, "{name} restored");
        }
    }

    /// The seven record knobs C's softIoc lists are exactly the ones this
    /// port's vendored record `.dbd` declares, with C's types. That is
    /// what makes them the representable group: the names and types are
    /// already right, and only the `const` values behind them stand
    /// between here and a working `var`.
    #[test]
    fn the_vendored_dbd_declares_cs_record_knobs() {
        let got: Vec<(&str, &str)> = crate::server::record::dbd_generated::VARIABLES.to_vec();
        assert_eq!(
            got,
            vec![
                ("boHIGHlimit", "double"),
                ("boHIGHprecision", "int"),
                ("calcoutODLYlimit", "double"),
                ("calcoutODLYprecision", "int"),
                ("histogramSDELprecision", "int"),
                ("seqDLYlimit", "double"),
                ("seqDLYprecision", "int"),
            ]
        );
        // And every one of them is in the table, under the arm its
        // declared type calls for. This is the join that keeps the two
        // lists from drifting: a knob added to the vendored `.dbd`
        // without a `seeded_knobs` entry, or entered with the wrong
        // arm, fails here.
        let table = VARIABLES.lock().unwrap_or_else(|e| e.into_inner());
        for (name, dtype) in &got {
            let def = table
                .get(name)
                .unwrap_or_else(|| panic!("{name} is declared by the .dbd but not registered"));
            let arm = match def.access {
                VarAccess::Int { .. } => "int",
                VarAccess::Double { .. } => "double",
            };
            assert_eq!(&arm, dtype, "{name}");
        }
    }

    /// `callbackParallelThreadsDefault` is the one seeded knob that is not
    /// a record `.dbd` variable, so the join above cannot cover it. Its
    /// default is C's post-registration value — `epicsThreadGetCPUs()`
    /// (`dbIocRegister.c:639`), not the `2` `callback.c:69` declares — and
    /// a write must move it without moving the processor count the
    /// negative arm of `callbackParallelThreads` reads.
    #[test]
    fn the_callback_default_is_the_cpu_count_and_is_settable() {
        use crate::runtime::background::callback_executor as cb;
        let ctx = make_ctx();
        let cpus = cb::cpu_count();
        assert!(cpus >= 1);
        assert_eq!(
            run_var(&ctx, &["callbackParallelThreadsDefault"]).unwrap(),
            format!("int callbackParallelThreadsDefault = {cpus}\n")
        );

        assert_eq!(
            run_var(&ctx, &["callbackParallelThreadsDefault", "7"]).unwrap(),
            ""
        );
        assert_eq!(cb::parallel_threads_default(), 7);
        assert_eq!(
            cb::cpu_count(),
            cpus,
            "the knob and the processor count are two globals, not one"
        );

        // C's `int`, so a negative is writable; `callback.c:171` is what
        // floors it, not the setter.
        assert_eq!(
            run_var(&ctx, &["callbackParallelThreadsDefault", "-2"]).unwrap(),
            ""
        );
        assert_eq!(cb::parallel_threads_default(), -2);
        cb::set_parallel_threads_default(cpus);
    }

    /// What registering them is for: `var` must move the value the
    /// record serves, on a record that already exists. C reads the
    /// global inside `get_precision` / `get_control_double`
    /// (`boRecord.c:304`, `:314`) rather than caching it at init, so a
    /// knob written after a record was created still reaches it.
    #[test]
    fn a_var_write_moves_what_an_existing_record_serves() {
        use crate::server::record::Record;
        let ctx = make_ctx();
        let rec = crate::server::records::bo::BoRecord::new(0);

        let before = rec.field_metadata_override("HIGH").expect("bo serves HIGH");
        assert_eq!(before.precision, Some(2));
        assert_eq!(before.ctrl_limits, Some((100000.0, 0.0)));

        assert_eq!(run_var(&ctx, &["boHIGHprecision", "4"]).unwrap(), "");
        assert_eq!(run_var(&ctx, &["boHIGHlimit", "5000.5"]).unwrap(), "");

        let after = rec.field_metadata_override("HIGH").expect("bo serves HIGH");
        assert_eq!(after.precision, Some(4));
        assert_eq!(after.ctrl_limits, Some((5000.5, 0.0)));
        // C prints a bare `%g`, so six significant digits.
        assert_eq!(
            run_var(&ctx, &["boHIGH*"]).unwrap(),
            "double boHIGHlimit = 5000.5\nint boHIGHprecision = 4\n"
        );

        crate::server::records::bo::set_bo_high_precision(2);
        crate::server::records::bo::set_bo_high_limit(100000.0);
    }

    /// C's double arm rejects the same shapes its int arm does and says
    /// so in its own words (`iocsh.cpp:1431-1434`), leaving the value
    /// alone without failing the command.
    #[test]
    fn an_unparsable_double_leaves_the_knob_alone() {
        let ctx = make_ctx();
        for token in ["nope", "1.0 ", "", "0x1p3"] {
            assert_eq!(run_var(&ctx, &["seqDLYlimit", token]).unwrap(), "");
            assert_eq!(
                run_var(&ctx, &["seqDLYlimit"]).unwrap(),
                "double seqDLYlimit = 100000\n",
                "{token:?}"
            );
        }
        // The shapes it does take, including C's leading-whitespace skip.
        for (token, want) in [("1e3", "1000"), (" 2.5", "2.5"), ("-0.5", "-0.5")] {
            assert_eq!(run_var(&ctx, &["seqDLYlimit", token]).unwrap(), "");
            assert_eq!(
                run_var(&ctx, &["seqDLYlimit"]).unwrap(),
                format!("double seqDLYlimit = {want}\n"),
                "{token:?}"
            );
        }
        crate::server::records::seq::set_seq_dly_limit(100000.0);
    }

    #[test]
    fn strtol_base0_takes_hex_octal_and_sign() {
        assert_eq!(strtol_base0("0x10"), Some(16));
        assert_eq!(strtol_base0("010"), Some(8));
        assert_eq!(strtol_base0("-3"), Some(-3));
        assert_eq!(strtol_base0("12"), Some(12));
        assert_eq!(strtol_base0(""), None);
        assert_eq!(strtol_base0("1 "), None);
        assert_eq!(strtol_base0("nope"), None);
    }
}
