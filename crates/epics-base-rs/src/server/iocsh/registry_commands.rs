//! The symbol-registry iocsh commands.
//!
//! C keeps ONE hash table for every kind of registered symbol, keyed by a
//! per-kind `registryID` — `registry.c:25` `static struct gphPvt *gphPvt`,
//! with `registryAdd`/`registryFind` taking the id as their first argument
//! (`registry.c:45-77`). Four thin wrappers give four of the ids their own
//! `Add`/`Find` pair (`"record type"`, `"device support"`,
//! `"driver support"`, `"function"`) and `registryIocRegister.c:70-76`
//! registers a `Find` command for each; iocsh puts two more ids in the same
//! table without wrappers, `iocshCmd` for every registered command
//! (`iocsh.cpp:171`) and `iocshVar` for every registered variable (`:745`).
//! `libComRegister.c:495` adds `registryDump` over the whole table, so all
//! six appear in it. `registerAllRecordDeviceDrivers`
//! (`iocshRegisterCommon.c:44-47`) is the command that fills the first four.
//!
//! # What the port has to register from
//!
//! The port has no single table to walk, because it has no dynamic loading
//! to need one: what a C IOC learns at run time from a generated
//! `<name>_registerRecordDeviceDriver.cpp` is, here, either linked in at
//! build time or handed to the [`IocBuilder`](crate::server::ioc_builder::IocBuilder)
//! before the shell exists. So these commands compute the registry as a
//! VIEW over the tables that already own each kind, rather than mirroring
//! them into a second one that could drift — with one exception, driver
//! support, which has no other owner and so keeps its own table:
//!
//! - record type — [`crate::server::record::dbd_generated::RECORD_TYPES`] (the built-in match in
//!   `create_record_raw`) plus [`db_loader::registered_record_type_entries`],
//!   the process-global map a downstream crate overrides through
//!   [`db_loader::register_record_type`]. Lookup order matches
//!   `create_record_raw`: the external map first.
//! - device support — the DTYP names, which the port keeps per record type
//!   as C does (`dbDeviceMenu`): the union of
//!   [`merged_device_menu`]
//!   over every record type.
//! - function — the database's subroutine registry, which
//!   `PvDatabase::find_subroutine_named` already documents as the C
//!   `registryFunctionFind` surface.
//! - driver support — [`driver_support::driver_supports`], the process-global
//!   table [`driver_support::register_driver_support`] fills. This is the one
//!   kind the port keeps as its own registry rather than as a view, because it
//!   has no other owner: C's `drvet` pointers live only in the registry, and
//!   the `.dbd` `driver(...)` names the parser collects into
//!   `DbdDefs::drivers` (`db_loader/mod.rs:540`) are dropped after the parse.
//! - `iocshCmd` — the shell's own `CommandRegistry`. C has no separate
//!   command table at all: `registryFind(iocshCmdID, name)` IS its lookup
//!   (`iocsh.cpp:200`), so its dump shows the commands for free. Here the
//!   table hangs off the shell, and the context carries a weak handle to it
//!   ([`CommandContext::command_entries`]) so this one command can walk it.
//!   A context with no shell reports none, which is accurate rather than
//!   missing — nothing has registered a command into it.
//! - `iocshVar` — [`crate::server::iocsh::vars::variable_names`], a process global already kept
//!   sorted the way C keeps `iocshVariableHead`.
//!
//! No id in C's table is unrepresentable here, so none is documented away.
//! `registryJLinkAdd` is the near miss worth naming: it looks like a seventh
//! registry and never touches the hash table, writing the `jlif` straight
//! into `pdbbase`'s `linkSup` (`registryJLinks.c:16-23`), so C's own dump
//! omits link types too.
//!
//! # What the `Find` commands print
//!
//! C prints `%p` of the pointer `registryFind` returned — the address of
//! the support structure — so the line is `(nil)` when the name is not
//! registered and a non-null address when it is. That null/non-null split
//! is what the command is read for; the address itself only says "this
//! entry, not that one". The port prints the address of the registry entry
//! it found (the factory, the `Arc`'d subroutine, or the `&'static str`
//! naming the entry), which has both of those properties, and `(nil)` on a
//! miss. A record type registered as a built-in has no support structure to
//! take an address of, so the entry it names is what answers.

use super::registry::*;
use crate::server::db_loader;
use crate::server::driver_support;
use crate::server::record::dbd_generated::RECORD_TYPES;
use crate::server::record::merged_device_menu;

/// Register the symbol-registry command set on `registry`.
pub(crate) fn register(registry: &mut CommandRegistry) {
    registry.register(cmd_registry_record_type_find());
    registry.register(cmd_registry_device_support_find());
    registry.register(cmd_registry_driver_support_find());
    registry.register(cmd_registry_function_find());
    registry.register(cmd_registry_dump());
    registry.register(cmd_register_all_record_device_drivers());
}

/// Every `registryID` that reaches C's one hash table, settled at R7.0.10 by
/// enumerating `registryAdd` callers: four from `modules/database/src/ioc/
/// registry` (`registryRecordType.c:17`, `registryDeviceSupport.c:17`,
/// `registryDriverSupport.c:17`, `registryFunction.c:19`) and two from iocsh
/// itself (`iocsh.cpp:83` `iocshCmd`, `:89` `iocshVar`). The id is what makes
/// one hash table hold six registries, and it is the grouping `registryDump`
/// prints beside each name.
///
/// `registryJLinkAdd` looks like a seventh and is not: it writes the `jlif`
/// into `pdbbase`'s `linkSup` and never calls `registryAdd`
/// (`registryJLinks.c:16-23`), so C's dump does not show link types either.
const RECORD_TYPE_ID: &str = "record type";
const DEVICE_SUPPORT_ID: &str = "device support";
const DRIVER_SUPPORT_ID: &str = "driver support";
const FUNCTION_ID: &str = "function";
const IOCSH_CMD_ID: &str = "iocshCmd";
const IOCSH_VAR_ID: &str = "iocshVar";

/// Every record type this IOC can create, as `(name, entry address)`.
///
/// External registrations first, as in `create_record_raw`, so a downstream
/// override answers with its own factory rather than the built-in it
/// replaced.
fn record_type_entries() -> Vec<(String, usize)> {
    let mut entries = db_loader::registered_record_type_entries();
    for name in RECORD_TYPES {
        if !entries.iter().any(|(n, _)| n.as_str() == *name) {
            entries.push(((*name).to_string(), name.as_ptr() as usize));
        }
    }
    entries
}

/// Every DTYP name any record type accepts, as `(name, entry address)`.
///
/// C registers device support once per `device()` line and the same
/// `dset` can serve several record types; the port keeps the names per
/// record type (`dbDeviceMenu`), so the registry is their union and a name
/// offered by two record types appears once, as it does in C.
fn device_support_entries() -> Vec<(String, usize)> {
    let mut entries: Vec<(String, usize)> = Vec::new();
    for record_type in RECORD_TYPES {
        for dtyp in merged_device_menu(record_type) {
            if !entries.iter().any(|(n, _)| n.as_str() == dtyp) {
                entries.push((dtyp.to_string(), dtyp.as_ptr() as usize));
            }
        }
    }
    entries
}

/// Every registered driver's entry table, as `(name, entry address)` —
/// [`crate::server::driver_support`], this port's `registryDriverSupportAdd`
/// table.
fn driver_support_entries() -> Vec<(String, usize)> {
    driver_support::driver_support_entries()
}

/// C's `iocshVar` registry as `(name, entry address)` — every name
/// `iocshRegisterVariable` added (`iocsh.cpp:745`), which here is
/// [`crate::server::iocsh::vars::variable_names`], already a process global kept sorted as C's
/// `iocshVariableHead` is.
fn iocsh_var_entries() -> Vec<(String, usize)> {
    super::vars::variable_names()
        .into_iter()
        .map(|name| (name.to_string(), name.as_ptr() as usize))
        .collect()
}

/// C `printf("%p\n", ptr)`: glibc renders a null pointer as `(nil)`.
fn format_entry_address(address: Option<usize>) -> String {
    match address {
        Some(a) => format!("{:#x}", a),
        None => "(nil)".to_string(),
    }
}

/// The shared body of the four `registry*Find` commands: one string
/// argument, one line of output. C generates them from one
/// `registryXxxFindArgs` and four near-identical call funcs
/// (`registryIocRegister.c:19-68`).
fn find_command(
    name: &'static str,
    usage: &'static str,
    lookup: fn(&CommandContext, &str) -> Option<usize>,
) -> CommandDef {
    CommandDef::new(
        name,
        vec![ArgDesc {
            // C `registryXxxFindArg0` (`registryIocRegister.c:19`).
            name: "name",
            arg_type: ArgType::String,
        }],
        usage,
        move |args: &[ArgValue], ctx: &CommandContext| {
            // `registryFind` returns 0 for a NULL name (`iocsh/registry.c:71`),
            // so each C call func prints `(nil)` and the line succeeds
            // (`registryIocRegister.c:30-68`). An absent name is a lookup that
            // finds nothing, not an arity refusal.
            let found = match &args[0] {
                ArgValue::String(s) => lookup(ctx, s),
                _ => None,
            };
            ctx.println(&format_entry_address(found));
            Ok(CommandOutcome::Continue)
        },
    )
}

fn cmd_registry_record_type_find() -> CommandDef {
    find_command(
        "registryRecordTypeFind",
        "registryRecordTypeFind <name> — Prints the registry address of the \
         record type given as first argument.",
        |_ctx, wanted| {
            record_type_entries()
                .into_iter()
                .find(|(n, _)| n == wanted)
                .map(|(_, a)| a)
        },
    )
}

fn cmd_registry_device_support_find() -> CommandDef {
    find_command(
        "registryDeviceSupportFind",
        "registryDeviceSupportFind <name> — Prints the registry address of \
         the device support given as first argument.",
        |_ctx, wanted| {
            device_support_entries()
                .into_iter()
                .find(|(n, _)| n == wanted)
                .map(|(_, a)| a)
        },
    )
}

fn cmd_registry_driver_support_find() -> CommandDef {
    find_command(
        "registryDriverSupportFind",
        "registryDriverSupportFind <name> — Prints the registry address of \
         the driver support given as first argument.",
        |_ctx, wanted| {
            driver_support_entries()
                .into_iter()
                .find(|(n, _)| n == wanted)
                .map(|(_, a)| a)
        },
    )
}

fn cmd_registry_function_find() -> CommandDef {
    find_command(
        "registryFunctionFind",
        "registryFunctionFind <name> — Prints the registry address of the \
         registered function given as first argument.",
        |ctx, wanted| {
            ctx.db()
                .subroutine_entries()
                .into_iter()
                .find(|(n, _)| n == wanted)
                .map(|(_, a)| a)
        },
    )
}

/// `registryDump` — C `registryDump()` (`registry.c:86-91`), which is
/// `gphDump` over the one table (`libComRegister.c:206-212`).
///
/// C prints the gpHash's own shape: `Hash table has N buckets`, then a line
/// per non-empty bucket carrying `  <name> <pvtid>` triples, then
/// `N buckets empty.` (`gpHashLib.c:210-242`). The pvtid printed beside each
/// name is the `registryID`, so C's dump is already "every entry, labelled
/// by which registry it belongs to" — scattered across buckets by a hash the
/// port does not have. Printing invented bucket numbers would say something
/// untrue about a table that does not exist, so the port groups by that same
/// `registryID` instead and keeps the `  <name> <address>` pairs and the
/// three-per-line wrap.
fn cmd_registry_dump() -> CommandDef {
    CommandDef::new(
        "registryDump",
        vec![],
        "registryDump — Dump a hash table of EPICS registry",
        |_args: &[ArgValue], ctx: &CommandContext| {
            let sections: [(&str, Vec<(String, usize)>); 6] = [
                (RECORD_TYPE_ID, record_type_entries()),
                (DEVICE_SUPPORT_ID, device_support_entries()),
                (DRIVER_SUPPORT_ID, driver_support_entries()),
                (FUNCTION_ID, ctx.db().subroutine_entries()),
                (IOCSH_CMD_ID, ctx.command_entries()),
                (IOCSH_VAR_ID, iocsh_var_entries()),
            ];
            let total: usize = sections.iter().map(|(_, e)| e.len()).sum();
            ctx.println(&format!("Registry has {total} entries"));
            for (id, mut entries) in sections {
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                ctx.println(&format!(" {:16} {:3}", id, entries.len()));
                // C `gpHashLib.c:234-237` breaks the line after every third
                // entry.
                for chunk in entries.chunks(3) {
                    let mut line = String::from("           ");
                    for (name, address) in chunk {
                        line.push_str(&format!("  {} {:#x}", name, address));
                    }
                    ctx.println(&line);
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `registerAllRecordDeviceDrivers pdbbase` — C `rrddCallFunc`
/// (`iocshRegisterCommon.c:44-47`).
///
/// C's job is to make every record type, device support and driver support
/// that is linked into the process present in the registry, and its three
/// helpers each SKIP a name that is already there —
/// `if (registryRecordTypeFind(...)) continue;` and its two siblings
/// (`registryCommon.c:34`, `:58`, `:72`) — printing only when an add fails.
/// On an IOC that already holds them all it is therefore a silent success,
/// which is what it is here on every IOC: the port's record types are linked
/// in or handed to the builder, and its device support is bound from the
/// DTYP menus, all of it before a shell exists to run this command. The
/// postcondition C guarantees holds; there is nothing left for it to add.
///
/// The one failure C has is the `pdbbase` argument itself: `cvtArg` refuses
/// anything that is not missing, `0`-prefixed or spelled `pdbbase`
/// (`iocsh.cpp:872-884`), which is the shared
/// [`check_pdbbase`](super::commands::check_pdbbase) every `pdbbase`-carrying
/// command in the port already uses.
fn cmd_register_all_record_device_drivers() -> CommandDef {
    CommandDef::new(
        "registerAllRecordDeviceDrivers",
        vec![ArgDesc {
            // C `rrddArg0` (`iocshRegisterCommon.c:29`), an `iocshArgPdbbase`.
            name: "pdbbase",
            arg_type: ArgType::String,
        }],
        "registerAllRecordDeviceDrivers pdbbase — Register all records, \
         devices, from all DBD available.",
        |args: &[ArgValue], _ctx: &CommandContext| {
            super::commands::check_pdbbase(&args[0])?;
            Ok(CommandOutcome::Continue)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::database::PvDatabase;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn make_ctx() -> CommandContext {
        // RTEMS-EXEC-MODEL-ALLOW(1): the runtime below is built only to give
        // `BlockingBridge::capture` something to capture, so that the one
        // command in this family that awaits — `registryFunctionFind`, through
        // `CommandContext::block_on` — has a bridge. Nothing here awaits the
        // ambient reactor. All eight tests were run under
        // `EPICS_RS_BUILD_EXEC_BACKEND=thread` and pass.
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

    /// Run one of this family's commands and return everything it printed.
    fn run(ctx: &CommandContext, name: &str, tokens: &[&str]) -> String {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get(name).unwrap_or_else(|| panic!("{name} registered"));
        let tokens: Vec<String> = tokens.iter().map(|t| (*t).to_string()).collect();
        let args = parse_args(&tokens, &cmd.args).expect("arguments parse");
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        ctx.with_output(std::fs::File::create(&path).unwrap(), || {
            let _ = cmd.handler.call(&args, ctx);
        });
        std::fs::read_to_string(&path).unwrap()
    }

    /// The six names `registryIocRegister()` (`registryIocRegister.c:70-76`),
    /// `libComRegister()` (`:495`) and `iocshRegisterCommon()`
    /// (`iocshRegisterCommon.c:76`) register between them. Asserted as a set
    /// because the family reaches `register_builtins` through a single
    /// appended line, and a lost append is otherwise silent.
    #[test]
    fn the_family_registers_every_name_c_does() {
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        for name in [
            "registryRecordTypeFind",
            "registryDeviceSupportFind",
            "registryDriverSupportFind",
            "registryFunctionFind",
            "registryDump",
            "registerAllRecordDeviceDrivers",
        ] {
            assert!(reg.get(name).is_some(), "C registers {name}");
        }
        assert!(
            reg.displaced().is_empty(),
            "the family must not claim one name twice: {:?}",
            reg.displaced()
        );
    }

    /// Registry id `"record type"`, both sides of the boundary. `ai` is a
    /// built-in `create_record_raw` arm and `RECORD_TYPES` member; the miss
    /// is C's `(nil)`.
    #[test]
    fn a_built_in_record_type_is_found_and_an_unknown_one_is_nil() {
        let ctx = make_ctx();
        assert_ne!(
            run(&ctx, "registryRecordTypeFind", &["ai"]).trim(),
            "(nil)",
            "ai is a registered record type"
        );
        assert_eq!(
            run(&ctx, "registryRecordTypeFind", &["nosuchrecordtype"]).trim(),
            "(nil)"
        );
    }

    /// A downstream crate's `register_record_type` override answers with its
    /// own factory address, as C answers with the `recordTypeLocation` the
    /// later `registryRecordTypeAdd` stored.
    #[test]
    fn an_externally_registered_record_type_is_found() {
        let ctx = make_ctx();
        let probe = "registryFindProbeType";
        assert_eq!(
            run(&ctx, "registryRecordTypeFind", &[probe]).trim(),
            "(nil)",
            "unregistered before the probe registers it"
        );
        crate::server::db_loader::register_record_type(
            probe,
            Box::new(|| Box::new(crate::server::records::ai::AiRecord::default())),
        );
        assert_ne!(
            run(&ctx, "registryRecordTypeFind", &[probe]).trim(),
            "(nil)"
        );
    }

    /// Registry id `"device support"`, both sides. `Soft Channel` is
    /// declared by base's own `device()` lines for `ai`.
    #[test]
    fn a_declared_device_support_is_found_and_an_unknown_one_is_nil() {
        let ctx = make_ctx();
        assert_ne!(
            run(&ctx, "registryDeviceSupportFind", &["Soft Channel"]).trim(),
            "(nil)",
            "base declares Soft Channel for ai"
        );
        assert_eq!(
            run(&ctx, "registryDeviceSupportFind", &["nosuchdevicesupport"]).trim(),
            "(nil)"
        );
    }

    /// Registry id `"driver support"`, both sides — the table
    /// `registryDriverSupportAdd` fills, which in this port is
    /// [`crate::server::driver_support`].
    #[test]
    fn a_registered_driver_support_is_found_and_an_unknown_one_is_nil() {
        let ctx = make_ctx();
        struct Probe;
        impl crate::server::driver_support::DriverSupport for Probe {
            fn report(&self, _level: i32) -> Option<String> {
                None
            }
        }
        let probe = "drvRegistryFindProbe";
        assert_eq!(
            run(&ctx, "registryDriverSupportFind", &[probe]).trim(),
            "(nil)",
            "unregistered before the probe registers it"
        );
        crate::server::driver_support::register_driver_support(probe, std::sync::Arc::new(Probe));
        assert_ne!(
            run(&ctx, "registryDriverSupportFind", &[probe]).trim(),
            "(nil)"
        );
        assert_eq!(
            run(&ctx, "registryDriverSupportFind", &["drvNoSuchDriver"]).trim(),
            "(nil)"
        );
    }

    /// Registry id `"function"`, both sides — the database's subroutine
    /// registry, which is what an aSub's `SNAM` re-resolves through
    /// (C `registryFunctionFind`).
    #[test]
    fn a_registered_subroutine_is_found_and_an_unknown_one_is_nil() {
        let ctx = make_ctx();
        assert_eq!(
            run(&ctx, "registryFunctionFind", &["mySub"]).trim(),
            "(nil)",
            "nothing is registered yet"
        );
        let mut registry: HashMap<String, Arc<crate::server::record::SubroutineFn>> =
            HashMap::new();
        registry.insert(
            "mySub".to_string(),
            Arc::new(Box::new(|_r: &mut dyn crate::server::record::Record| Ok(0))),
        );
        ctx.block_on(ctx.db().install_subroutine_registry(registry));
        assert_ne!(
            run(&ctx, "registryFunctionFind", &["mySub"]).trim(),
            "(nil)"
        );
        assert_eq!(
            run(&ctx, "registryFunctionFind", &["nosuchfunction"]).trim(),
            "(nil)"
        );
    }

    /// The dump names all six registry ids and its total is their sum, so an
    /// id whose table is empty is still visible as empty rather than absent —
    /// C's `gphDump` likewise prints every bucket's contents under the one
    /// header.
    #[test]
    fn registry_dump_names_every_id_and_totals_them() {
        let ctx = make_ctx();
        let out = run(&ctx, "registryDump", &[]);
        for id in [
            RECORD_TYPE_ID,
            DEVICE_SUPPORT_ID,
            DRIVER_SUPPORT_ID,
            FUNCTION_ID,
            IOCSH_CMD_ID,
            IOCSH_VAR_ID,
        ] {
            assert!(out.contains(id), "registryDump must name {id}: {out:.400}");
        }
        let expected = record_type_entries().len()
            + device_support_entries().len()
            + driver_support_entries().len()
            + ctx.db().subroutine_entries().len()
            + ctx.command_entries().len()
            + iocsh_var_entries().len();
        assert!(
            out.starts_with(&format!("Registry has {expected} entries")),
            "the header counts every entry the sections list: {out:.200}"
        );
        assert!(out.contains("  ai 0x"), "an entry prints name then address");
    }

    /// The `iocshCmd` half is the one that needed a seam: a context with no
    /// shell has no command table, and a shell's context has the live one.
    /// Both halves asserted, because a dump that silently reported zero
    /// commands on every IOC would pass the id-naming test above.
    #[test]
    fn the_iocsh_cmd_registry_follows_the_shell_that_owns_the_context() {
        let standalone = make_ctx();
        assert!(
            standalone.command_entries().is_empty(),
            "no shell owns this context, so it has registered no commands"
        );

        // RTEMS-EXEC-MODEL-ALLOW(1): same as `make_ctx` above — the runtime
        // exists only so `BlockingBridge::capture` has something to capture,
        // and `registryDump` awaits nothing. Run under
        // `EPICS_RS_BUILD_EXEC_BACKEND=thread` and passes.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bridge = {
            let _guard = rt.enter();
            crate::runtime::task::BlockingBridge::capture()
        };
        let shell = crate::server::iocsh::IocShell::new(Arc::new(PvDatabase::new()), bridge);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let _ = shell.execute_line(&format!("registryDump > {}", path.display()));
        std::mem::forget(rt);
        let out = std::fs::read_to_string(&path).unwrap();
        // Everything after the section header. `split_once`, not `split`,
        // because one of the entries listed under the header is itself a
        // command named `iocshCmd`.
        let (_, cmds) = out
            .split_once(IOCSH_CMD_ID)
            .expect("the dump has an iocshCmd section");
        assert!(
            cmds.contains(" registryDump 0x"),
            "a shell's own context must see the shell's commands: {cmds:.300}"
        );
        assert!(
            cmds.contains(" dbl 0x"),
            "and every other built-in, not just this family: {cmds:.300}"
        );
    }

    /// C's `iocshArgPdbbase` accepts the argument missing, `0`-prefixed or
    /// spelled `pdbbase`, and refuses anything else (`iocsh.cpp:872-884`).
    /// Success is silent because C's three helpers skip every name already
    /// registered and print only on failure (`registryCommon.c:34`, `:58`,
    /// `:72`), and the port has them all registered before a shell exists.
    #[test]
    fn register_all_record_device_drivers_takes_pdbbase_and_refuses_anything_else() {
        let ctx = make_ctx();
        let mut reg = CommandRegistry::new();
        register(&mut reg);
        let cmd = reg.get("registerAllRecordDeviceDrivers").unwrap();

        for accepted in ["pdbbase", "0"] {
            let args = parse_args(&[accepted.to_string()], &cmd.args).unwrap();
            assert!(
                matches!(cmd.handler.call(&args, &ctx), Ok(CommandOutcome::Continue)),
                "C accepts {accepted}"
            );
        }
        let args = parse_args(&["junk".to_string()], &cmd.args).unwrap();
        assert_eq!(
            cmd.handler.call(&args, &ctx).err(),
            Some("Expecting 'pdbbase' got 'junk'.".to_string())
        );
    }
}
