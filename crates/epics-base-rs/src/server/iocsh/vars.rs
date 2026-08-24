// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the feature-ON suite.
//! The iocsh *variable* table and the `var` command — C
//! `iocshRegisterVariable` (`iocsh.cpp:721-771`) and `varCallFunc` /
//! `varHandler` (`iocsh.cpp:1394-1473`).
//!
//! C keeps variables in a table separate from the command table: an
//! entry is a name, a type and a pointer straight at a process global,
//! and `var` reads or writes through that pointer. A knob that C
//! registers this way is spelled `var <name> <value>` in a startup
//! script, never as a command of its own.

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
/// enum, so an unhandled type cannot be constructed. Only the int form
/// exists because no knob in this port is a double.
pub(crate) enum VarAccess {
    Int { get: fn() -> i64, set: fn(i64) },
}

/// One entry of the iocsh variable table.
pub(crate) struct VarDef {
    pub name: &'static str,
    pub access: VarAccess,
}

/// C's `iocshVariableHead` is a process global kept sorted by name, and
/// so is this — `var` with no argument lists in name order.
static VARIABLES: LazyLock<Mutex<BTreeMap<&'static str, VarDef>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// C `iocshRegisterVariable`. Re-registering the same name is a no-op,
/// which is what C's `found` branch amounts to for a table whose entries
/// all point at their own global.
pub(crate) fn register_variable(def: VarDef) {
    VARIABLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(def.name)
        .or_insert(def);
}

/// Register the `var` command. C registers it from
/// `iocshRegisterVariable` the first time a variable appears
/// (`iocsh.cpp:731-732`), so a shell with an empty table has no `var`.
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
/// `varHandler` applies: an optional sign, then `0x` hex, a leading `0`
/// octal, or decimal.
fn strtol_base0(s: &str) -> Option<i64> {
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

/// C `varHandler(v, NULL)` — `int %s = %d` on stdout.
fn show(ctx: &CommandContext, def: &VarDef) {
    match &def.access {
        VarAccess::Int { get, .. } => ctx.println(&format!("int {} = {}", def.name, get())),
    }
}

fn cmd_var() -> CommandDef {
    CommandDef::new(
        "var",
        vec![
            ArgDesc {
                name: "[variable",
                arg_type: ArgType::String,
                optional: true,
            },
            ArgDesc {
                name: "value]]",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "var [variable [value]] — print all variables, print one, or set one",
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
                    None => eprintln!("Invalid integer, var '{}' not changed.", def.name),
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
    /// (`libComRegister.c:491-495`, `:535-537`) and reaches it with
    /// `var`, which prints nothing when setting and `int NAME = V` when
    /// reading (`iocsh.cpp:1394-1473`).
    #[test]
    fn var_reads_and_writes_as_check_client_ip() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        super::super::commands::register_builtins(&mut reg);
        assert!(
            reg.get("var").is_some(),
            "C registers `var` (iocsh.cpp:714)"
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
