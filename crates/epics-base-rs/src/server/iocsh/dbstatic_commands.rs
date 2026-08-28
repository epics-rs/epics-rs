//! The `dbStaticIocRegister.c` iocsh commands.
//!
//! C registers 16 names from `modules/database/src/ioc/dbStatic`
//! (`dbStaticIocRegister.c:265-280` @R7.0.10, where the file is 281
//! lines). Three of them — `dbDumpPath`, `dbDumpBreaktable` and
//! `dbCreateAlias` — are registered by [`super::commands`]; this module
//! owns the rest.
//!
//! Every command here keeps C's registered name, argument count,
//! argument names and argument types, so a `st.cmd` line written for a C
//! IOC parses identically. What each one PRINTS is the port's answer to
//! C's question, in C's line shape, for the data the port actually has:
//! a line whose whole content is a C pointer (`rset *`, `pdset`,
//! `pdsxt`, `ftPvt`) or a C struct offset has no port meaning and is not
//! invented.
//!
//! The usage text of each command is C's `iocshFuncDef.usage` verbatim,
//! typos included (`recortTypeName`, `pddbase`), because `help` prints
//! it and an operator comparing the two shells reads the same words.
//! `dbPvdDump` is the sole exception and says why at its own definition.
//!
//! All sixteen are registered. The one that came closest to not being is
//! `dbPvdDump`, whose whole C body is bucket occupancy of the open-hash
//! process-variable directory (`dbPvdLib.c:193-234`); see
//! [`cmd_db_pvd_dump`] for what the port measures instead.

use super::registry::*;
use crate::server::record::dbd_generated::RECORD_TYPES;
use crate::server::record::{
    Asl, FieldDeclaration, FieldDesc, HwLink, HwLinkKind, ParsedLink, Special,
};
use crate::types::DbfCode;

/// Register the `dbStaticIocRegister.c` command set on `registry`.
pub(crate) fn register(registry: &mut CommandRegistry) {
    registry.register(cmd_db_dump_record());
    registry.register(cmd_db_dump_menu());
    registry.register(cmd_db_dump_record_type());
    registry.register(cmd_db_dump_field());
    registry.register(cmd_db_dump_device());
    registry.register(cmd_db_dump_driver());
    registry.register(cmd_db_dump_link());
    registry.register(cmd_db_dump_registrar());
    registry.register(cmd_db_dump_function());
    registry.register(cmd_db_dump_variable());
    registry.register(cmd_db_pvd_dump());
    registry.register(cmd_db_pvd_table_size());
    registry.register(cmd_db_report_device_config());
    registry.register(cmd_dbhcr());
}

/// C's leading `pdbbase` argument (`iocshArgPdbbase`, `dbStaticIocRegister.c:21`):
/// `cvtArg` (`iocsh.cpp:872-884`) accepts it missing, starting with `0`, or
/// spelled `pdbbase`, and refuses anything else.
///
/// [`super::commands`] has the same check for the three commands it kept from
/// this C file. Both are eight lines over one C rule; sharing one across the
/// module boundary would mean widening a private helper's visibility for no
/// behaviour, and the rule is defined by `iocsh.cpp`, not by either caller.
fn check_pdbbase(arg: &ArgValue) -> Result<(), String> {
    if let ArgValue::String(pdbbase) = arg
        && !(pdbbase.is_empty() || pdbbase.starts_with('0') || pdbbase == "pdbbase")
    {
        return Err(format!("Expecting 'pdbbase' got '{pdbbase}'."));
    }
    Ok(())
}

/// C `epicsStrPrintEscaped` (`libcom/src/misc/epicsString.c:230-262`) — the
/// escaping `dbWriteRecordFP` puts every field and `info` value through
/// (`dbStaticLib.c:845`, `:858`) so the dump reloads as `.db`.
///
/// C early-returns on an empty string and writes nothing, which is the same
/// bytes as escaping it, so that branch is not reproduced. NUL is not special
/// here: C's own `\0` case is missing from this function (it is present in
/// `dbTranslateEscape`), so a NUL byte falls through to the non-printable arm
/// and comes out `\x00`.
fn print_escaped(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            // C tests `isprint` on one BYTE. A multi-byte UTF-8 character
            // therefore comes out as one `\xNN` per byte in C, and does so
            // here too rather than being passed through whole.
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            c => {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("\\x{byte:02x}"));
                }
            }
        }
    }
    out
}

/// The record names grouped by record type, the walk C's `dbFirstRecordType` /
/// `dbFirstRecord` pair performs (`dbStaticLib.c:822-877`): one type at a time,
/// each type's records in load order.
///
/// [`super::commands`] holds the same walk for its own commands. It is six
/// lines and it is C's iteration order, not a shared policy, so it is stated
/// where it is used rather than exported.
fn record_names_by_type(ctx: &CommandContext) -> Vec<String> {
    let mut names = ctx.block_on(ctx.db().all_record_names());
    let rank = |name: &String| {
        let record_type = ctx
            .db()
            .get_record(name)
            .map(|rec| rec.read().record.record_type().to_string())
            .unwrap_or_default();
        match RECORD_TYPES.iter().position(|t| *t == record_type) {
            Some(i) => (0usize, i, String::new()),
            None => (1usize, 0, record_type),
        }
    };
    names.sort_by_cached_key(rank);
    names
}

/// C's `*`/empty sentinel for a record-type argument: both mean "every type"
/// (`dbWriteRecordFP`, `dbStaticLib.c:801-804`; `dbDumpDevice`,
/// `dbStaticLib.c:3434-3437`).
fn type_filter(arg: &ArgValue) -> Option<&str> {
    match arg {
        ArgValue::String(s) if !s.is_empty() && s != "*" => Some(s.as_str()),
        _ => None,
    }
}

/// `dbDumpRecord pdbbase [recordTypeName] [interest level]` — dump the loaded
/// record instances as `.db` text. C `dbDumpRecord` (`dbStaticLib.c:3285-3293`)
/// over `dbWriteRecordFP` (`dbStaticLib.c:792-881`), registered at
/// `dbStaticIocRegister.c:266`.
///
/// One documented deviation, in the level gate. C computes
/// `dctonly = level > 1 ? FALSE : TRUE` (`:800`) and then walks only the fields
/// carrying a `promptgroup` while `dctonly` holds. The generator parses
/// `promptgroup` and drops it, so the port has no DCT subset to restrict to and
/// levels 0 and 1 see the whole field table. What the level still decides is
/// C's other gate, which the port does carry: at level 0 only fields whose
/// value differs from the `.dbd` `initial` are printed, and from level 1 up
/// every field is (`:837-847` — C's `else if (level>0)` arm there is dead, the
/// first condition already covers it).
///
/// `dbIsVisibleRecord` is the second absence: C prints `grecord(` for a record
/// whose `visible` flag is set and `record(` otherwise (`:830-835`). The port
/// models no such flag, so every record is dumped as `record(`, which is what
/// C emits for every record a `.db` file loaded.
fn cmd_db_dump_record() -> CommandDef {
    CommandDef::new(
        "dbDumpRecord",
        vec![
            ArgDesc {
                name: "pdbbase",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "recordTypeName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "interest level",
                arg_type: ArgType::Int,
            },
        ],
        concat!(
            "Dump information about the recordTypeName with 'interest level' details.\n",
            "Example: dbDumpRecord ai 2\n",
            "If last argument(s) are missing, dump all recordType information in the database.",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            let wanted = type_filter(&args[1]);
            let level = match args[2] {
                ArgValue::Int(n) => n,
                _ => 0,
            };

            let names = record_names_by_type(ctx);
            let record_type_of = |name: &String| {
                ctx.db()
                    .get_record(name)
                    .map(|rec| rec.read().record.record_type().to_string())
            };

            // C resolves the named type through `dbFindRecordType` BEFORE
            // listing anything and reports an unknown one on stderr
            // (`dbStaticLib.c:816-822`). The port's record-type universe is the
            // generated table plus whatever a runtime registration added, which
            // is exactly the set a loaded record can carry.
            if let Some(wanted) = wanted
                && !RECORD_TYPES.contains(&wanted)
                && !names
                    .iter()
                    .any(|name| record_type_of(name).as_deref() == Some(wanted))
            {
                return Err(format!(
                    "dbWriteRecordFP: No record description for {wanted}"
                ));
            }

            for name in &names {
                let Some(record_type) = record_type_of(name) else {
                    continue;
                };
                if let Some(wanted) = wanted
                    && record_type != wanted
                {
                    continue;
                }
                for line in dump_one_record(ctx, name, &record_type, level) {
                    ctx.println(&line);
                }
            }

            // C walks each type's record list a SECOND time and emits the alias
            // nodes after the records (`dbStaticLib.c:868-877`), so the dump
            // reloads in an order where every alias target already exists.
            for name in &names {
                if let Some(wanted) = wanted
                    && record_type_of(name).as_deref() != Some(wanted)
                {
                    continue;
                }
                for alias in ctx.db().aliases_for_record(name) {
                    ctx.println(&format!("alias(\"{name}\",\"{alias}\")"));
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C's numbering for a `.dbd` link type — `link.h:28-43`. `DbLinkType` is a
/// Rust enum with no discriminants, so the wire number `dbDumpDevice` prints
/// (`dbStaticLib.c:3448`) is stated here rather than derived from the variant
/// order, which is a different order and would silently answer 3 for `VmeIo`.
fn c_link_type_number(t: crate::server::record::DbLinkType) -> i32 {
    use crate::server::record::DbLinkType as L;
    match t {
        L::Constant => 0,
        L::PvLink => 1,
        L::VmeIo => 2,
        L::CamacIo => 3,
        L::AbIo => 4,
        L::GpibIo => 5,
        L::BitbusIo => 6,
        L::JsonLink => 8,
        L::DbLink => 10,
        L::CaLink => 11,
        L::InstIo => 12,
        L::BbgpibIo => 13,
        L::RfIo => 14,
        L::VxiIo => 15,
    }
}

/// C `pamapspcType` (`special.h:42-61`) — the `SPC_*` name `dbDumpField` looks
/// up for a non-zero `special`.
///
/// Nine entries for ten codes: `SPC_ATTRIBUTE` (7) is defined and never added
/// to the table, so C's linear search falls off the end and prints the number
/// with no name after it. Reproduced rather than completed, because the
/// alternative is a line no C IOC prints.
fn c_special_name(code: i16) -> Option<&'static str> {
    Some(match code {
        1 => "SPC_NOMOD",
        2 => "SPC_DBADDR",
        3 => "SPC_SCAN",
        5 => "SPC_ALARMACK",
        6 => "SPC_AS",
        100 => "SPC_MOD",
        101 => "SPC_RESET",
        102 => "SPC_LINCONV",
        103 => "SPC_CALC",
        _ => return None,
    })
}

/// C's `pdbFldDes->special`, the `%hd` `dbDumpField` and `dbWriteRecordTypeFP`
/// both print.
fn c_special_code(special: Special) -> i16 {
    special as i16
}

/// The lines C's `dbDumpField` prints for one field descriptor
/// (`dbStaticLib.c:3374-3416`), less the ones the port has no datum behind.
/// See [`cmd_db_dump_field`] for the omissions and why each is omitted.
///
/// `ind_record_type` is C's `pdbFldDes->indRecordType`, the field's index into
/// `papFldDes`. It cannot be the caller's iteration position: the port drops
/// C's `DBF_NOACCESS` internals, which are interleaved with the rest from
/// `MLOK` at 13 onwards, so the index must come from
/// [`record_declaration_order`](crate::server::record::dbd_generated::record_declaration_order),
/// and is `None` for a record type registered at runtime, whose declaration
/// order the generator never saw.
fn dump_field_lines(desc: &FieldDesc, ind_record_type: Option<usize>) -> Vec<String> {
    let special = c_special_code(desc.special);
    let mut lines = vec![format!("    {}", desc.name)];
    if let Some(ind) = ind_record_type {
        // C's own indentation, six spaces where its neighbours use eight.
        lines.push(format!("\t      indRecordType: {ind}"));
    }
    lines.extend([
        // C's trailing space is unconditional and the name follows it only
        // when `special` is non-zero, so a plain field ends the line on that
        // space (`dbStaticLib.c:3380-3389`).
        match c_special_name(special) {
            Some(name) if special != 0 => format!("\t        special: {special} {name}"),
            _ => format!("\t        special: {special} "),
        },
        format!("\t     field_type: DBF_{}", desc.declared_dbf.name()),
        format!("\tprocess_passive: {}", u8::from(desc.pp)),
        format!("\t       property: {}", u8::from(desc.prop)),
        format!("\t       interest: {}", desc.interest),
        format!(
            "\t       as_level: {}",
            match desc.asl {
                Asl::Asl0 => 0,
                Asl::Asl1 => 1,
            }
        ),
        format!("\t        initial: {}", desc.initial.unwrap_or("")),
    ]);
    lines
}

/// `dbDumpMenu pdbbase [menuName]` — every `menu()` and its choices. C
/// `dbDumpMenu` (`dbStaticLib.c:3295-3302`) forwards to `dbWriteMenuFP`
/// (`:897-930`), registered at `dbStaticIocRegister.c:267`.
///
/// Unlike `dbDumpField`'s plain `strcmp`, this one takes the `NULL`/`""`/`"*"`
/// sentinel (`:907-910`): all three dump every menu.
///
/// The report is complete for the menus `epics-base-rs` declares. A downstream
/// crate's `.dbd` menus are generated into that crate's own `MENUS` and are not
/// visible from here, because a record type registers its FIELDS with the
/// process (`register_declared_fields`) and its menus have no such funnel;
/// C has one `pdbbase->menuList` for every loaded `.dbd`.
fn cmd_db_dump_menu() -> CommandDef {
    CommandDef::new(
        "dbDumpMenu",
        vec![
            ArgDesc {
                name: "pdbbase",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "menuName",
                arg_type: ArgType::String,
            },
        ],
        concat!(
            "Dump information about the available menuNames and choices defined within each menuName.\n",
            "Example: dbDumpMenu pdbbase menuAlarmStat \n",
            "If last argument(s) are missing, dump all menuNames information in the database.",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            // `dbWriteMenuFP:907-910` — NULL, "" and "*" all mean every menu.
            let wanted = match &args[1] {
                ArgValue::String(s) if !s.is_empty() && s != "*" => Some(s.clone()),
                _ => None,
            };
            for (name, idents, values) in crate::server::record::dbd_generated::MENUS {
                if let Some(w) = wanted.as_deref()
                    && *name != w
                {
                    continue;
                }
                ctx.println(&format!("menu({name}) {{"));
                for (ident, value) in idents.iter().zip(values.iter()) {
                    ctx.println(&format!("\tchoice({ident},\"{value}\")"));
                }
                ctx.println("}");
                // C `if (menuName) break;`.
                if wanted.is_some() {
                    break;
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbDumpRecordType pdbbase [recordTypeName]` — the record type's field table
/// in C's `papFldDes` order, alongside the same table sorted by field name. C
/// `dbDumpRecordType` (`dbStaticLib.c:3304-3343`), registered at
/// `dbStaticIocRegister.c:268`.
///
/// Like `dbDumpField` and unlike `dbDumpMenu`, the type argument is a plain
/// `strcmp` (`:3320-3325`): only a genuinely absent argument means "every
/// type", so `dbDumpRecordType pdbbase ""` prints nothing.
///
/// # The two columns and one line this port omits
///
/// C prints `offset` and `size` per field and a closing `rset * %p rec_size
/// %d`. All four are C struct layout, and none of them is the `.dbd` datum
/// their names suggest: `registryCommon.c:47` runs the generated
/// `<rec>RecordSizeOffset` at `registerRecordDeviceDriver` time, which
/// overwrites every `size` with `sizeof(prec->member)` and every `offset` with
/// `offsetof` (`dbdToRecordtypeH.pl:146-151`), and sets `rec_size =
/// sizeof(*prec)`. So on a booted IOC these are the widths and offsets of a C
/// record struct this port does not have; `rset` is its function-table address.
/// The remaining columns are `.dbd` facts and print unchanged.
///
/// Everything else C prints comes out of
/// [`record_declaration_order`](crate::server::record::dbd_generated::record_declaration_order),
/// which is C's `papFldDes` order — `.dbd` declaration order with dbCommon
/// spliced in at the record's `include` and the `DBF_NOACCESS` internals kept —
/// so `no_fields`, every `index`, the `sortind` permutation, `link_ind` and
/// `indvalFlddes` are C's own numbers rather than an index into one of the two
/// halves the port stores separately.
fn cmd_db_dump_record_type() -> CommandDef {
    CommandDef::new(
        "dbDumpRecordType",
        vec![
            ArgDesc {
                name: "pdbbase",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "recordTypeName",
                arg_type: ArgType::String,
            },
        ],
        concat!(
            "Dump information about available fields in the recortTypeName sorted by index and name.\n",
            "Example: dbDumpRecordType pdbbase calcout\n",
            "If last argument(s) are missing, dump fields information for all records in the database.",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            // C `strcmp`, not the `*`/empty sentinel `dbDumpMenu` takes.
            let wanted = match &args[1] {
                ArgValue::String(s) => Some(s.clone()),
                _ => None,
            };
            for record_type in RECORD_TYPES {
                if let Some(w) = wanted.as_deref()
                    && *record_type != w
                {
                    continue;
                }
                for line in dump_record_type_lines(record_type) {
                    ctx.println(&line);
                }
                // C `if (recordTypeName) break;` — the named type ends the walk.
                if wanted.is_some() {
                    break;
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// One record type's `dbDumpRecordType` report.
///
/// The counts C computes once at `.dbd` parse time (`dbLexRoutines.c:745-798`)
/// are derived here instead of stored, because the port's `.dbd` load is the
/// generator: `no_links` and `link_ind` are the `DBF_INLINK..DBF_FWDLINK`
/// declarations in `papFldDes` order, and the sort is C's own field-name
/// `strcmp` order (`:784-797`). `no_prompt` is the one count that cannot be
/// derived — `emit.rs` drops `promptgroup` — so the generator carries it.
fn dump_record_type_lines(record_type: &str) -> Vec<String> {
    use crate::server::record::dbd_generated as dbd;

    let Some(decl) = dbd::record_declaration_order(record_type) else {
        return Vec::new();
    };
    // The port keeps dbCommon separate from a record's own fields and drops the
    // `DBF_NOACCESS` internals from both, so a name in `decl` may have no
    // descriptor. Those are exactly C's internals, which are never links.
    let own = dbd::record_fields(record_type).unwrap_or(&[]);
    let declared_dbf = |name: &str| {
        dbd::DB_COMMON_FIELDS
            .iter()
            .chain(own.iter())
            .find(|d| d.name == name)
            .map(|d| d.declared_dbf)
    };
    let link_ind: Vec<usize> = decl
        .iter()
        .enumerate()
        .filter(|(_, name)| {
            matches!(
                declared_dbf(name),
                Some(DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink)
            )
        })
        .map(|(i, _)| i)
        .collect();

    let mut lines = vec![
        format!(
            "name({record_type}) no_fields({}) no_prompt({}) no_links({})",
            decl.len(),
            dbd::record_no_prompt(record_type).unwrap_or(0),
            link_ind.len()
        ),
        "index name\tsortind sortname".to_string(),
    ];

    // C's `sortFldInd[i]` is the declaration index of the i-th field name in
    // ascending `strcmp` order and `papsortFldName[i]` is that name. Field
    // names are unique and ASCII, so Rust's byte order is the same order.
    let mut sort_ind: Vec<usize> = (0..decl.len()).collect();
    sort_ind.sort_by_key(|&i| decl[i]);
    for (i, name) in decl.iter().enumerate() {
        lines.push(format!(
            "{:5} {}\t{:7} {}",
            i, name, sort_ind[i], decl[sort_ind[i]]
        ));
    }

    let mut link_line = String::from("link_ind ");
    for i in &link_ind {
        link_line.push_str(&format!(" {i}"));
    }
    lines.push(link_line);

    // C dereferences `pvalFldDes` unconditionally, so a record type with no
    // `VAL` field crashes it rather than printing a sentinel; every `.dbd`
    // declares one. Printing nothing is the only honest answer if one does not.
    if let Some(indval) = decl.iter().position(|name| *name == "VAL") {
        lines.push(format!("indvalFlddes {indval} name VAL"));
    }
    lines
}

/// `dbDumpField pdbbase [recordTypeName] [fieldName]` — dump the `.dbd` field
/// descriptors of a record type. C `dbDumpField` (`dbStaticLib.c:3346-3427`),
/// registered at `dbStaticIocRegister.c:270`.
///
/// Unlike its neighbours this command does NOT take `*` or the empty string as
/// "every type": C compares the argument with `strcmp` and only a genuinely
/// absent (NULL) argument means all (`:3361-3367`, `:3373`), so
/// `dbDumpField pdbbase ""` matches nothing and prints nothing. Reproduced.
///
/// # The six lines this port omits, and the one change that would restore four
///
/// C prints fifteen lines per field plus two conditionals and a per-type
/// attribute block. Nine of them are printed here; the rest have no datum
/// behind them and are left out rather than filled with a plausible value:
///
/// * `prompt`, `extra`, `base` and `promptgroup` are all parsed by the
///   generator — `tools/dbd-codegen/src/parse.rs` carries `prompt`,
///   `promptgroup`, `base` and `extra` as fields of its own — and then dropped
///   by `emit.rs`, which writes thirteen of them into [`FieldDesc`] and not
///   these four. They are one emitter change away, not a structural gap, and
///   the same change would give `dbDumpRecord` the DCT subset its own
///   documentation records as missing.
/// * `offset` and `size` are the offset and the width of the member in the
///   record's C struct, and the `ftPvt` line on a `DBF_DEVICE` field is a heap
///   pointer. None of the three exists here. `size` reads like the `.dbd`
///   `size(N)` attribute and is not: `registryCommon.c:47` runs the generated
///   `<rec>RecordSizeOffset` at `registerRecordDeviceDriver` time, which
///   overwrites every `size` with `sizeof(prec->member)`
///   (`dbdToRecordtypeH.pl:146-151`), so a booted C IOC prints 8 for
///   `ai.VAL` and 80 for `ai.INP` where the `.dbd` says nothing at all.
/// * The `menu:` line on a `DBF_MENU` field names the menu.
///   [`FieldDesc::menu`] holds the choice values, and a `&'static [&'static
///   str]` carries no name back.
///
/// The `Attribute:` block is NOT among them. It walks C's per-record-type
/// attribute list (`:3416-3423`), one `Attribute: name <n> value <v>` line per
/// entry after the field loop and printed even when `fname` narrowed the dump
/// to a single field, and the port has that list as
/// [`PvDatabase::record_type_attributes`](crate::server::database::PvDatabase::record_type_attributes)
/// — a `BTreeMap`, so C's `strcmp` walk order comes free — seeded with `RTYP`
/// and `VERS` per `dbReadCOM`'s tail (`dbLexRoutines.c:311-331`) and written by
/// the `dbPutAttribute` `commands.rs` registers. It is printed.
fn cmd_db_dump_field() -> CommandDef {
    CommandDef::new(
        "dbDumpField",
        vec![
            ArgDesc {
                name: "pdbbase",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "recordTypeName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "fieldName",
                arg_type: ArgType::String,
            },
        ],
        concat!(
            "Dump information about the fieldName in the recordTypeName.\n",
            "Example: dbDumpField  pdbbase calcout A\n",
            "If last argument(s) are missing, dump information\n",
            "about all fieldName in all recordTypeName in the database.",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            // C `strcmp`, not the `*`/empty sentinel its neighbours use.
            let name_arg = |arg: &ArgValue| match arg {
                ArgValue::String(s) => Some(s.clone()),
                _ => None,
            };
            let wanted_type = name_arg(&args[1]);
            let wanted_field = name_arg(&args[2]);

            for record_type in RECORD_TYPES {
                if let Some(wanted) = wanted_type.as_deref()
                    && *record_type != wanted
                {
                    continue;
                }
                // C's trailing space before the newline.
                ctx.println(&format!("recordtype({record_type}) "));
                let common = crate::server::record::dbd_generated::DB_COMMON_FIELDS;
                let own =
                    crate::server::record::dbd_generated::record_fields(record_type).unwrap_or(&[]);
                let decl =
                    crate::server::record::dbd_generated::record_declaration_order(record_type)
                        .unwrap_or(&[]);
                for desc in common.iter().chain(own.iter()) {
                    if let Some(wanted) = wanted_field.as_deref()
                        && desc.name != wanted
                    {
                        continue;
                    }
                    let ind = decl.iter().position(|name| *name == desc.name);
                    for line in dump_field_lines(desc, ind) {
                        ctx.println(&line);
                    }
                }
                // C walks `attributeList` after the field loop and OUTSIDE
                // it, so the block appears once per record type whether or not
                // `fname` narrowed the dump to a single field — and appears
                // even when `fname` matched nothing at all.
                for (name, value) in ctx.db().record_type_attributes(record_type) {
                    ctx.println(&format!("Attribute: name {name} value {value}"));
                }
                // C `if (recordTypeName) break;` — the named type ends the
                // walk even though the loop already filtered to it.
                if wanted_type.is_some() {
                    break;
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbDumpDevice pdbbase [recordTypeName]` — the device support declared for
/// each record type. C `dbDumpDevice` (`dbStaticLib.c:3429-3486`), registered
/// at `dbStaticIocRegister.c:270`.
///
/// C prints five values per device. Two are `.dbd` declarations the port
/// carries — `choice` (the `DTYP` string) and `link_type` — and are printed on
/// C's lines. The other three are C addresses: `pdset` and `pdsxt` are the
/// device support function tables, and `device name` is the C symbol naming
/// `pdset`, which the generator never sees because a port device support is a
/// Rust type rather than a linker symbol.
///
/// A record type whose device menu was contributed at runtime
/// ([`register_device_menu`](crate::server::record::register_device_menu),
/// how `asyn-rs` adds its `DTYP`s) has choices but no declared link type: the
/// registry carries names only. Those print the `choice` line alone rather than
/// a guessed `link_type`.
fn cmd_db_dump_device() -> CommandDef {
    CommandDef::new(
        "dbDumpDevice",
        vec![
            ArgDesc {
                name: "pdbbase",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "recordTypeName",
                arg_type: ArgType::String,
            },
        ],
        concat!(
            "Dump device support information for the recordTypeName.\n",
            "Example: dbDumpDevice pdbbase ai\n",
            "If last argument(s) are missing, dump device support\n",
            "information for all records in the database.",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            let wanted = type_filter(&args[1]);
            for record_type in RECORD_TYPES {
                if let Some(wanted) = wanted
                    && *record_type != wanted
                {
                    continue;
                }
                let declared = crate::server::record::dbd_generated::device_menu(record_type)
                    .unwrap_or_default();
                let contributed = crate::server::record::contributed_device_menu(record_type);
                if declared.is_empty() && contributed.is_empty() {
                    continue;
                }
                ctx.println(&format!("recordtype({record_type})"));
                for choice in declared.iter().copied().chain(contributed) {
                    ctx.println(&format!("\tchoice:    {choice}"));
                    if let Some(link_type) =
                        crate::server::record::dbd_generated::device_link_type(record_type, choice)
                    {
                        ctx.println(&format!("\tlink_type: {}", c_link_type_number(link_type)));
                    }
                }
                if wanted.is_some() {
                    break;
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbDumpDriver pdbbase` — C `dbDumpDriver` (`dbStaticLib.c:3488-3495`) over
/// `dbWriteDriverFP` (`:1076-1089`), registered at `dbStaticIocRegister.c:271`.
///
/// One `driver(<name>)` line per driver this IOC can report on. In C that is
/// `pdbbase->drvList`, and `dbior` walks the SAME list (`dbTest.c:723-725`),
/// so the two commands cannot disagree about which drivers exist. The port
/// keeps one registry, [`driver_support`](crate::server::driver_support) —
/// `register_driver_support` is the only way in and `dbior` already reads it —
/// so this report reads it too and the same guarantee holds.
///
/// A stock IOC prints nothing on either side: base R7.0.10 declares no
/// `driver()` and nothing in this port registers one outside its own tests.
fn cmd_db_dump_driver() -> CommandDef {
    CommandDef::new(
        "dbDumpDriver",
        vec![ArgDesc {
            name: "pdbbase",
            arg_type: ArgType::String,
        }],
        concat!(
            "Dump device support information.\n",
            "Example: dbDumpDriver pdbbase\n",
            "If the last argument(s) are missing, dump all device support information.",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            // C's order is the loaded `.dbd`'s, which `dbExpand` sorts; the
            // registry keeps registration order, so sort to the same shape.
            let mut names: Vec<String> = crate::server::driver_support::driver_supports()
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            names.sort();
            for name in names {
                ctx.println(&format!("driver({name})"));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbDumpLink pdbbase` — C `dbDumpLink` (`dbStaticLib.c:3497-3504`) over
/// `dbWriteLinkFP` (`:1091-1104`), registered at `dbStaticIocRegister.c:272`.
///
/// `link(<key>,<jlif>)` per declaration — BOTH columns, because both are what
/// the `.dbd` line said: `dbLinkType` stores `name` and `jlif_name` verbatim
/// (`dbLexRoutines.c:883-900`) and never resolves the symbol, exactly as it
/// never resolves a `registrar()` name. The port's build-time declarations
/// come from `PORTED_JSON_LINK_TYPES`, which carries the pair.
///
/// C's list is operative rather than descriptive: `dbFindLinkSup`
/// (`dbJLink.c:262`) looks a `{…}` link's leading map key up in it, so a type
/// absent from this report cannot be written in a link field. A measured
/// softIoc R7.0.10 prints five — `link(calc,lnkCalcIf)`,
/// `link(const,lnkConstIf)`, `link(debug,lnkDebugIf)`, `link(state,lnkStateIf)`,
/// `link(trace,lnkTraceIf)`, all from `links.dbd.pod:39,79,171,208,224`.
///
/// Three of those five are absent here, each for its own reason, and each is
/// refused by name rather than silently:
///
/// * `state` (`lnkStateIf`, `lnkState.c:226`) — unwritten, and only that. The
///   `dbState` registry it reads through exists in this port already
///   ([`crate::server::database::filters`]'s `sync` state table and
///   `builtin_devices::dbstate`), so nothing structural is in the way.
/// * `debug` (`lnkDebugIf`, `lnkDebug.c:1030`) — C's is a WRAPPER: it
///   substitutes its own `lset` and forwards every entry point to a child
///   link, logging each call. This port dispatches link operations by matching
///   [`ParsedLink`], with no `lset` vtable to substitute, so the technique has
///   no seat; a port of it would be a new variant threaded through every
///   match arm rather than a new jlif.
/// * `trace` (`lnkTraceIf`, `lnkDebug.c:1039`) — the same wrapper, differing
///   from `debug` only in its `alloc`; absent for the same reason.
///
/// `pva` comes from pvxs (`ioc/pvxs7x.dbd:2`, `link("pva", "lsetPVX")`), so its
/// second column is quoted from there. `ca` is this port's own extension with
/// no upstream `link()` line at all, and `lnkCaIf` is this port's own symbol
/// name for it.
fn cmd_db_dump_link() -> CommandDef {
    CommandDef::new(
        "dbDumpLink",
        vec![ArgDesc {
            name: "pdbbase",
            arg_type: ArgType::String,
        }],
        concat!(
            "Dump list of registered links\n",
            "Example: dbDumpLink pdbbase",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            for sup in link_sup_list().lock().unwrap().entries() {
                ctx.println(&format!("link({},{})", sup.key, sup.jlif_name));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// The port's `pdbbase->registrarList` — every `registrar()` declaration this
/// process has seen, in the order C would report them.
///
/// C fills the list in `dbRegistrar` (`dbLexRoutines.c:903-924`): a name
/// already in the `.dbd` gpHash is dropped and the survivor is `ellAdd`ed, so
/// a name declared twice appears once, at the position it was first seen, and
/// `dbWriteRegistrarFP` walks the list without sorting (`dbStaticLib.c:1106-1119`).
/// The list is declaration residue and nothing more — whether the symbol
/// exists is settled later by `registerRecordDeviceDriver`, and
/// `dbDumpRegistrar` never asks. That is why this port can answer the command
/// at all despite having no registrar mechanism of its own: the question is
/// about the text that was loaded, not about a function anyone will call.
///
/// # Why two vectors and not one
///
/// C's names arrive from two places and the difference is visible in the
/// output. Everything an IOC was BUILT with reaches C in a single expanded
/// `.dbd`, and `dbdExpand.pl` writes that file's `registrar()` lines
/// `foreach sort keys %{$registrars}` (`DBD/Output.pm:100-103`) — so the
/// build's declarations are ASCII-sorted no matter what order the source
/// `.dbd` fragments declared them in (`filters.dbd` declares `ts`, `dbnd`,
/// `arr`, `sync`, `dec`, `utag` in that order and `softIoc.dbd` prints them
/// sorted). A `.dbd` LOADED later at runtime is parsed as it stands and
/// appends in its own file order.
///
/// This port resolves its `.dbd` at build time, so [`BUILT_IN_REGISTRARS`]
/// and every cross-crate contribution stand for that expanded file and must
/// be rendered by its rule. Keeping one append-only vector made the slot a
/// registrar lands in depend on which crate happened to run first —
/// `rsrvRegistrar`, C's seventh, came out ninth. Sorting the whole list
/// instead would have been just as wrong in the other direction, because it
/// would re-order a runtime load's residue, which C leaves alone.
///
/// Process-global rather than a field of `PvDatabase`, for the reason
/// `dbDumpPath`'s loaded path is (`db_loader::set_loaded_path`): the datum
/// belongs to the loads a process performed, and a process has one `pdbbase`.
/// What a `.dbd` declaration must expose for [`DeclarationList`] to order and
/// dedup it: C keys both on the declared NAME (`dbLexRoutines.c:895`,
/// `:912-915`) whatever else the node carries.
trait DbdDeclaration {
    fn key(&self) -> &str;
}

impl DbdDeclaration for String {
    fn key(&self) -> &str {
        self
    }
}

struct DeclarationList<T> {
    /// The build's declarations — kept ASCII-sorted at every insertion, which
    /// is `dbdExpand.pl`'s `sort` held as an invariant instead of re-applied
    /// at print time.
    built_in: Vec<T>,
    /// What a runtime `dbLoadDatabase` read, in first-declaration order.
    loaded: Vec<T>,
}

/// Hand-written rather than derived: an empty list needs nothing of `T`, and
/// `#[derive(Default)]` would demand `T: Default` from every declaration kind.
impl<T> Default for DeclarationList<T> {
    fn default() -> Self {
        Self {
            built_in: Vec::new(),
            loaded: Vec::new(),
        }
    }
}

impl<T: DbdDeclaration> DeclarationList<T> {
    /// C's gpHash test, which spans the whole list: a name already declared
    /// is dropped wherever it was first seen (`dbLexRoutines.c:912-915`).
    fn contains(&self, key: &str) -> bool {
        self.built_in
            .iter()
            .chain(&self.loaded)
            .any(|d| d.key() == key)
    }

    fn declare_built_in(&mut self, decl: T) {
        if self.contains(decl.key()) {
            return;
        }
        let at = self.built_in.partition_point(|d| d.key() < decl.key());
        self.built_in.insert(at, decl);
    }

    fn declare_loaded(&mut self, decl: T) {
        if self.contains(decl.key()) {
            return;
        }
        self.loaded.push(decl);
    }

    fn entries(&self) -> impl Iterator<Item = &T> {
        self.built_in.iter().chain(&self.loaded)
    }
}

fn registrar_list() -> &'static std::sync::Mutex<DeclarationList<String>> {
    static LIST: std::sync::OnceLock<std::sync::Mutex<DeclarationList<String>>> =
        std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let mut list = DeclarationList::default();
        for name in BUILT_IN_REGISTRARS {
            list.declare_built_in((*name).to_string());
        }
        std::sync::Mutex::new(list)
    })
}

/// One `link(<key>, <jlif>)` declaration as this process holds it — the port's
/// `pdbbase->linkList` node. Owns its strings because a runtime
/// `dbLoadDatabase` contributes them.
#[derive(Clone, PartialEq, Eq)]
struct LinkDeclaration {
    key: String,
    jlif_name: String,
}

impl DbdDeclaration for LinkDeclaration {
    fn key(&self) -> &str {
        &self.key
    }
}

/// The port's `pdbbase->linkList`, under the same two-block rule as
/// [`registrar_list`]: `dbdExpand.pl` sorts the build's `link()` lines
/// (`DBD/Output.pm:92-97`) and a runtime `.dbd` appends in its own order.
fn link_sup_list() -> &'static std::sync::Mutex<DeclarationList<LinkDeclaration>> {
    static LIST: std::sync::OnceLock<std::sync::Mutex<DeclarationList<LinkDeclaration>>> =
        std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let mut list = DeclarationList::default();
        for sup in crate::server::record::PORTED_JSON_LINK_TYPES {
            list.declare_built_in(LinkDeclaration {
                key: sup.key.to_string(),
                jlif_name: sup.jlif_name.to_string(),
            });
        }
        std::sync::Mutex::new(list)
    })
}

/// Record the `link()` declarations one `dbLoadDatabase` read, as
/// [`add_loaded_registrars`] does for `registrar()`. A key the build already
/// declares is dropped, which is C's gpHash test.
pub(super) fn add_loaded_link_types(types: &[crate::server::db_loader::DbdLinkType]) {
    let mut list = link_sup_list().lock().unwrap();
    for t in types {
        if !t.key.is_empty() {
            list.declare_loaded(LinkDeclaration {
                key: t.key.clone(),
                jlif_name: t.lset.clone(),
            });
        }
    }
}

/// The `registrar()` lines this crate's own compiled-in feature set declares —
/// the port's share of what `softIoc.dbd` contributes to C's list before any
/// `dbLoadDatabase` runs.
///
/// C reaches a registrar by loading a `.dbd` whose `registrar(name)` line
/// `registerRecordDeviceDriver.pl` resolves to a linked symbol; this port
/// resolves its `.dbd` at build time and wires each feature in directly, so
/// the declaration has nowhere else to live. Keeping it here rather than
/// beside each feature is the same shape C has: a `registrar()` line lives in
/// a `.dbd` file, never in the C source of the thing it names.
///
/// The order written here does not matter — `DeclarationList::declare_built_in`
/// sorts, as `dbdExpand.pl` does — but a name belongs here only while the
/// behaviour behind it is compiled into this crate, which
/// `built_in_registrars_name_only_features_this_crate_implements` checks
/// through the public parse/registry surfaces rather than by repeating the
/// list:
///
/// * `arrInitialize`, `dbndInitialize`, `decInitialize`, `syncInitialize`,
///   `tsInitialize`, `utagInitialize` — C `filters.dbd`, the six server-side
///   channel filters, here as [`crate::server::database::filters`]'s `arr`,
///   `dbnd`, `dec`, `sync`, `ts` and `utag` plugins.
/// * `dlloadRegistar` — C `dlload.dbd`; registers the `dlload` command, which
///   [`super::misc_commands`] registers.
/// * `iocshSystemCommand` — C `system.dbd`; likewise for `system`.
///
/// Two of C's ten softIoc lines are deliberately absent. `asSub`
/// (`asSubRecordFunctions.c:87-91`) adds `asSubInit`/`asSubProcess` to the
/// function registry and neither subroutine exists in this workspace, so the
/// line would name a `SNAM` no record can resolve. `rsrvRegistrar`
/// (`rsrvIocRegister.c:34-39`) is `rsrv_register_server()` plus
/// `iocshRegister(casr)`; both halves exist, but in `epics-ca-rs`
/// (`register_ca_db_server`, and its `casr`), so the declaration is that
/// crate's to contribute and arrives through [`add_registrars`].
static BUILT_IN_REGISTRARS: &[&str] = &[
    "arrInitialize",
    "dbndInitialize",
    "decInitialize",
    "dlloadRegistar",
    "iocshSystemCommand",
    "syncInitialize",
    "tsInitialize",
    "utagInitialize",
];

/// Declare a registrar this build COMPILED IN, from a crate that
/// `epics-base-rs` cannot name — the cross-crate half of
/// [`BUILT_IN_REGISTRARS`], reached through [`super::add_registrars`].
///
/// It lands in sort position because that is the file it stands for: C gets
/// `rsrvRegistrar` from the same expanded `softIoc.dbd` as the other nine, and
/// the expansion sorted them. Which crate announces first therefore cannot
/// move it.
pub(super) fn add_registrars(names: &[String]) {
    let mut list = registrar_list().lock().unwrap();
    for name in names {
        if !name.is_empty() {
            list.declare_built_in(name.clone());
        }
    }
}

/// Record the `registrar()` names one `dbLoadDatabase` read. Called by
/// [`super::commands`]'s `db_read_database`, which is the port's `dbReadCOM`.
///
/// Appends, because C parses a runtime `.dbd` as it stands: only the build's
/// declarations went through `dbdExpand.pl`'s sort.
///
/// An empty name is dropped rather than listed. C never reaches the list with
/// one — `dbRegistrar` calls `yyerrorAbort` and the whole read fails
/// (`dbLexRoutines.c:908-911`) — but this port's loader accepts `registrar("")`,
/// and inventing a `registrar()` line for it would report a declaration C
/// would have refused to make. Restoring the abort belongs to the loader.
pub(super) fn add_loaded_registrars(names: &[String]) {
    let mut list = registrar_list().lock().unwrap();
    for name in names {
        if !name.is_empty() {
            list.declare_loaded(name.clone());
        }
    }
}

/// `dbDumpRegistrar pdbbase` — C `dbDumpRegistrar` (`dbStaticLib.c:3506-3513`)
/// over `dbWriteRegistrarFP` (`:1106-1119`), registered at
/// `dbStaticIocRegister.c:273`.
///
/// One `registrar(<name>)` line per `registrar()` declaration, in
/// `BUILT_IN_REGISTRARS` order: the build's declarations sorted, standing for the
/// expanded `softIoc.dbd` C reads at startup, then whatever each
/// `dbLoadDatabase` declared, in the order it declared it.
fn cmd_db_dump_registrar() -> CommandDef {
    CommandDef::new(
        "dbDumpRegistrar",
        vec![ArgDesc {
            name: "pdbbase",
            arg_type: ArgType::String,
        }],
        concat!(
            "Dump list of registered functions including ones for subroutine records,\n",
            "and ones that can be invoked from iocsh.\n",
            "Example: dbDumpRegistrar pdbbase\n",
            "If last argument(s) are missing, dump all registered functions",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            for name in registrar_list().lock().unwrap().entries() {
                ctx.println(&format!("registrar({name})"));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbDumpFunction pdbbase` — C `dbDumpFunction` (`dbStaticLib.c:3515-3522`)
/// over `dbWriteFunctionFP` (`:1121-1134`), registered at
/// `dbStaticIocRegister.c:274`.
///
/// One `function(<name>)` line per subroutine a `sub`/`aSub` record's `SNAM`
/// can resolve to. In C those names reach `pdbbase->functionList` from
/// `function()` lines the generated `registerRecordDeviceDriver.cpp` emits; in
/// the port they are the by-name registry `IocBuilder` installs and
/// `find_subroutine_named` reads, which is the same set answering the same
/// question. C's `functionList` and the registry gpHash `registryDump` walks
/// are two structures; the port has one map behind both, so this command and
/// `registryDump` share its single accessor.
fn cmd_db_dump_function() -> CommandDef {
    CommandDef::new(
        "dbDumpFunction",
        vec![ArgDesc {
            name: "pdbbase",
            arg_type: ArgType::String,
        }],
        concat!(
            "Dump list of registered subroutine functions.\n",
            "Example: dbDumpFunction pddbase\n",
            "If last argument(s) are missing, dump all registered subroutine functions",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            // `subroutine_entries` is the one accessor over the registry;
            // `registryDump` reads the same call for the address half. C has
            // two structures here — `pdbbase->functionList` in `.dbd`
            // declaration order and the registry gpHash — and the port
            // collapses both onto one `HashMap`, which has no declaration
            // order to report. Sorting is this report's answer to that, so it
            // belongs here and not in a second accessor.
            let mut names: Vec<String> = ctx
                .db()
                .subroutine_entries()
                .into_iter()
                .map(|(name, _addr)| name)
                .collect();
            names.sort();
            for name in names {
                ctx.println(&format!("function({name})"));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbDumpVariable pdbbase` — C `dbDumpVariable` (`dbStaticLib.c:3524-3531`)
/// over `dbWriteVariableFP` (`:1136-1149`), registered at
/// `dbStaticIocRegister.c:275`.
///
/// One `variable(<name>,<type>)` line per `variable()` declaration in the
/// loaded `.dbd`. C keeps TWO variable tables and this is the `.dbd` one, not
/// the iocsh table `var` reads: `registerRecordDeviceDriver.pl` copies this one
/// INTO the iocsh one, which also holds entries no `.dbd` declared — measured
/// on softIoc R7.0.10, `asCheckClientIP` is in the iocsh table and absent from
/// this report. So the source here is the vendored `.dbd`, through the
/// generator, and NOT [`super::vars`].
///
/// The port vendors the record `.dbd` files and not `dbCore.dbd`, `links.dbd`
/// or `rsrv.dbd`, so the seven record knobs are reported and C's fifteen
/// IOC-core knobs are not. That is a knob gap rather than a reporting one:
/// twelve of the fifteen (`atExitDebug`, `dbConvertStrict`, `logClientDebug`,
/// ...) name nothing this port has.
fn cmd_db_dump_variable() -> CommandDef {
    CommandDef::new(
        "dbDumpVariable",
        vec![ArgDesc {
            name: "pdbbase",
            arg_type: ArgType::String,
        }],
        concat!(
            "Dump list of variables used in the database.\n",
            "Example: dbDumpVariable pddbase\n",
            "If last argument(s) are missing, dump all variables.",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            for (name, dtype) in crate::server::record::dbd_generated::VARIABLES {
                ctx.println(&format!("variable({name},{dtype})"));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbPvdDump pdbbase [verbose]` — C `dbPvdDump` (`dbPvdLib.c:193-234`),
/// registered at `dbStaticIocRegister.c:277`.
///
/// C's body is a census of the open hash carrying the process-variable
/// directory: the bucket count, every non-empty bucket's occupancy, the
/// empty-bucket total, and above verbose 0 the names living in each bucket.
/// The port's directory is three `HashMap`s — records, aliases and simple PVs,
/// held disjoint by `check_name_free` — and `std::collections::HashMap`
/// publishes no bucket array, so every bucket number printed here would be an
/// invention rather than a measurement. What C measures that the port can
/// measure too is the directory's size and its contents, so those are the
/// lines: C's opening sentence with `entries` where C says `buckets`, then the
/// names at C's continuation indent, four to a row.
///
/// This is not `dbl` under a second name. C's directory carries the alias
/// names as well as the record names — `dbCreateAlias` reaches `dbPvdAdd` at
/// `dbStaticLib.c:1696`, `dbCreateRecord` at `:1448`, and nothing else does —
/// while `dbl` walks record types. On C R7.0.10 over a database of two records
/// and one alias, `dbPvdDump pdbbase 1` lists the alias and `dbl` does not; the
/// port's report is the same union, so the alias shows here and not in `dbl`
/// either.
///
/// The names are sorted. C emits them in bucket order, which is its hash
/// function's order over a table whose size the startup script may have
/// changed; the port has no such order to reproduce, and a `HashMap` walk is
/// seeded per process, so sorted is the only order in which two dumps of one
/// IOC can be compared.
fn cmd_db_pvd_dump() -> CommandDef {
    CommandDef::new(
        "dbPvdDump",
        vec![
            ArgDesc {
                name: "pdbbase",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "verbose",
                arg_type: ArgType::Int,
            },
        ],
        // The one usage text in this file that is NOT C's verbatim: C's
        // promises per-bucket output, which this port has no bucket array
        // to give (see the module doc).
        "Dump the process variable directory.\n\
         If verbose is greater than 0, also print the process variables in it.\n\
         Example: dbPvdDump pdbbase 1\n\
         If the last argument(s) are missing, dump the directory as though verbose is 0.",
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            // C gates the name walk on the raw `int` (`dbPvdLib.c:222`), so a
            // negative verbose prints them too and a missing argument does not.
            let verbose = match args[1] {
                ArgValue::Int(n) => n != 0,
                _ => false,
            };

            let mut names = ctx.block_on(ctx.db().all_record_names());
            names.extend(ctx.db().all_alias_names());
            names.extend(ctx.block_on(ctx.db().all_simple_pv_names()));
            names.sort();

            ctx.println(&format!(
                "Process Variable Directory has {} entries",
                names.len()
            ));
            if verbose {
                // C's own wrap: four names to a row, each `"  %s"`, rows after
                // the first indented `"\n         "` (`dbPvdLib.c:223-227`).
                for row in names.chunks(4) {
                    let mut line = String::from("         ");
                    for name in row {
                        line.push_str("  ");
                        line.push_str(name);
                    }
                    ctx.println(&line);
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbPvdTableSize <size>` — C `dbPvdTableSize` (`dbPvdLib.c:45-60`),
/// registered at `dbStaticIocRegister.c:278`.
///
/// The one command in this file with no `pdbbase` argument, and the one C
/// wraps in `iocshSetError` directly (`dbStaticIocRegister.c:210`), so a
/// rejected size fails the startup line.
///
/// What is reproduced is C's whole observable contract: the power-of-two test,
/// its diagnostic, and the non-zero status. What is NOT reproduced is the
/// assignment behind it — C stores the clamped size in `dbPvdHashTableSize`
/// for `dbPvdInitPvt` to size the open hash with (`dbPvdLib.c:62-78`), and the
/// port's process-variable directory is a `HashMap` that neither takes a
/// bucket count nor exposes one. Storing a value nothing can read, and
/// clamping it to bounds nothing enforces, would be state pretending to be a
/// knob, so the accepted size is validated and discarded.
fn cmd_db_pvd_table_size() -> CommandDef {
    CommandDef::new(
        "dbPvdTableSize",
        vec![ArgDesc {
            name: "size",
            arg_type: ArgType::Int,
        }],
        concat!(
            "Change the number of buckets in the process variable directory.\n",
            "\n",
            "The process variable directory size should be set before loading the database.\n",
            "The size of the process variable directory can automatically grow.\n",
            "The size must be a power of 2.\n",
            "\n",
            "Example: dbPvdTableSize 1024",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            // C reads `args[0].ival`, an `int`; a missing argument converts to
            // 0, which passes the power-of-two test and is then clamped up to
            // `MIN_SIZE`.
            let size = match args[0] {
                ArgValue::Int(n) => n as i32,
                _ => 0,
            };
            // C's `size & (size - 1)` (`dbPvdLib.c:47`). Signed, so a negative
            // size is rejected here rather than clamped: `-4 & -5` is `-8`.
            if size & size.wrapping_sub(1) != 0 {
                ctx.println(&format!("dbPvdTableSize: {size} is not a power of 2"));
                return Ok(CommandOutcome::Failed);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// C's `bus[LINK_NTYPES]` table (`dbStaticLib.c:3558-3574`) indexed by a
/// link's type, which is where C gets it too: `dbReportDeviceConfig` hands
/// unparsed link text to `dbParseLink` (`:3611`) and keys the report's first
/// column on the `ltype` that comes back.
///
/// The bus itself is read off the parse rather than re-derived from the text,
/// so C's identifier-letter table has exactly one implementation
/// ([`HwLinkKind`]) and this report cannot drift from what `dbpr` prints.
///
/// **Upstream defect, not reproduced.** C's `bus[]` is declared with
/// `LINK_NTYPES` (16, `link.h:44`) slots but written with only 15
/// initializers, so every entry from `RF_IO` on is shifted: `bus[RF_IO]`
/// (14) holds `"VXI"` and `bus[VXI_IO]` (15) is the zero fill. A C IOC
/// therefore labels an `#Rn Mn Dn En` RF link `VXI` and drops real
/// `#Vn Cn Sn` VXI links from the report entirely. The eight bus names the
/// table means to carry are unambiguous, so `VXI_IO` is labelled `VXI` here
/// and `RF_IO` — which the table never names — is skipped.
fn c_hw_bus(hw: &HwLink) -> Option<&'static str> {
    Some(match hw.kind() {
        HwLinkKind::InstIo => "INST",
        HwLinkKind::VmeIo => "VME",
        HwLinkKind::CamacIo => "CAMAC",
        HwLinkKind::AbIo => "AB",
        HwLinkKind::GpibIo => "GPIB",
        HwLinkKind::BitbusIo => "BITBUS",
        HwLinkKind::BbgpibIo => "BBGPIB",
        HwLinkKind::VxiIo => "VXI",
        // `RF_IO` is the one bus C's `bus[]` never names — see the defect
        // note above.
        HwLinkKind::RfIo => return None,
    })
}

/// The `%-8s %-20s %-20s %-20s %-s` report line C prints for every record
/// bound to a hardware link (`dbStaticLib.c:3648-3650`), returned as lines so
/// the walk is testable without an output capture.
///
/// The link column is `dbGetString` (`:3621`), i.e. the link re-rendered from
/// its parsed numbers. C has a second arm for a link whose text has not been
/// parsed yet — `plink->text` verbatim (`:3608-3616`) — which is reachable
/// only from a startup script running before `iocInit`; this port parses at
/// load, so there is no unparsed state to print and the rendered form is the
/// only answer.
///
/// C walks each record's link fields by declaration index
/// (`dbGetLinkField(pdbentry, ilink)`) and stops at the FIRST one carrying a
/// bus (`:3651` `break`). The port's equivalent enumeration is
/// [`PvDatabase::record_link_fields`](crate::server::database::PvDatabase::record_link_fields),
/// which yields `INP` and `OUT` first — the only two fields a hardware link
/// can legally reach, because every other link field's declared type is
/// `CONSTANT` (`dbLexRoutines.c:571`, enforced here by `check_link_assignment`).
fn device_config_lines(ctx: &CommandContext) -> Vec<String> {
    let mut lines = Vec::new();
    for name in record_names_by_type(ctx) {
        let Some((bus, link_value)) =
            ctx.db()
                .record_link_fields(&name)
                .into_iter()
                .find_map(|(_field, _text, parsed)| match parsed {
                    ParsedLink::Hw(hw) => c_hw_bus(&hw).map(|bus| (bus, hw.render())),
                    _ => None,
                })
        else {
            continue;
        };

        let Some(rec) = ctx.db().get_record(&name) else {
            continue;
        };
        let inst = rec.read();
        // C reads every one of these four through `dbGetString`
        // (`dbStaticLib.c:3629`, `:3637`, `:3641`, `:3645`) — the dbStatic
        // renderer, which is not the `DBR_STRING` conversion `dbgf` and a link
        // read go through. Both answer `Soft Timestamp` for `DTYP` and
        // `LINEAR` for `LINR`, but only this one renders `EGUF` the way C's
        // `cvt(` column does.
        let field_string =
            |field: &str| super::commands::db_get_string_field(&inst, field).unwrap_or_default();

        // C `cvt(<EGUL>,<EGUF>)`, and only when `LINR` reads exactly `LINEAR`
        // (`dbStaticLib.c:3632-3647`). A record type with no `LINR` field gets
        // the empty column, which is C's `dbFindField` failure arm.
        let cvt = if field_string("LINR") == "LINEAR" {
            format!("cvt({},{})", field_string("EGUL"), field_string("EGUF"))
        } else {
            String::new()
        };

        lines.push(format!(
            "{bus:<8} {link_value:<20} {:<20} {name:<20} {cvt}",
            field_string("DTYP")
        ));
    }
    lines
}

/// `dbReportDeviceConfig pdbbase` — C `dbReportDeviceConfig`
/// (`dbStaticLib.c:3575-3666`), registered at `dbStaticIocRegister.c:279`.
fn cmd_db_report_device_config() -> CommandDef {
    CommandDef::new(
        "dbReportDeviceConfig",
        vec![ArgDesc {
            name: "pdbbase",
            arg_type: ArgType::String,
        }],
        concat!(
            "Print the link type, link value, device type, record name,\n",
            "and linearisation info (if applicable),\n",
            "for every record using a specific device type.\n",
            "\n",
            "Example: dbReportDeviceConfig pdbbase",
        ),
        |args: &[ArgValue], ctx: &CommandContext| {
            check_pdbbase(&args[0])?;
            for line in device_config_lines(ctx) {
                ctx.println(&line);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// `dbhcr` — C `dbhcr` (`dbTest.c:774-783`), registered at
/// `dbIocRegister.c:617`.
///
/// `dbReportDeviceConfig(pdbbase, stdout)` behind a NULL-`pdbbase` guard, with
/// no arguments of its own. The guard's `No database loaded` branch is
/// unreachable here: `pdbbase` is C's parsed-`.dbd` root, and the port's DBD is
/// the compiled-in `dbd_generated` table, which is never absent. It shares
/// [`device_config_lines`] with `dbReportDeviceConfig` rather than restating
/// the report, which is the same sharing C does by calling the function.
fn cmd_dbhcr() -> CommandDef {
    CommandDef::new(
        "dbhcr",
        vec![],
        "dbhcr — Report hardware configuration for every record using a device type.",
        |_args: &[ArgValue], ctx: &CommandContext| {
            for line in device_config_lines(ctx) {
                ctx.println(&line);
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// One `record(...) { ... }` block — the body of C's inner `dbFirstRecord`
/// loop (`dbStaticLib.c:826-866`), returned as lines so the walk is testable
/// without an output capture.
fn dump_one_record(ctx: &CommandContext, name: &str, record_type: &str, level: i64) -> Vec<String> {
    let Some(rec) = ctx.db().get_record(name) else {
        return Vec::new();
    };
    let mut lines = vec![format!("record({record_type},\"{name}\") {{")];
    let inst = rec.read();

    let mut descs: Vec<&crate::server::record::FieldDesc> =
        crate::server::record::dbd_generated::DB_COMMON_FIELDS
            .iter()
            .chain(inst.record.field_list())
            .collect();
    // C walks `papFldDes` in declaration index order, which puts `dbCommon`
    // first and the record's own fields after it — the order this chain
    // already produces. The dedup is the port's: a record type that also
    // declares a `dbCommon` name would otherwise emit the field twice.
    let mut seen = std::collections::HashSet::new();
    descs.retain(|d| seen.insert(d.name));

    for desc in descs {
        // C skips `NAME` and the `DBF_NOACCESS` internals because neither is
        // reachable through `dbFirstField`; the port drops the internals at the
        // generator and holds `NAME` outside the field table.
        let Some(value) = inst.resolve_field(desc.name) else {
            continue;
        };
        if level == 0 && is_default_value(desc, &value) {
            continue;
        }
        lines.push(format!(
            "\tfield({},\"{}\")",
            desc.name,
            print_escaped(&value.to_string())
        ));
    }

    // C's `infoList` is an ELLLIST in declaration order
    // (`dbStaticLib.c:855-864`); the port's `info` is a HashMap with no order
    // to report, so one record's tags come out sorted by tag name.
    let mut tags: Vec<(&String, &String)> = inst.info.iter().collect();
    tags.sort();
    for (key, value) in tags {
        lines.push(format!("\tinfo(\"{key}\",\"{}\")", print_escaped(value)));
    }
    lines.push("}".to_string());
    lines
}

/// C `dbIsDefaultValue` (`dbStaticRun.c:230-367`) — is the field still holding
/// what the `.dbd` `initial()` seeded it with?
///
/// C switches on the declared `DBF_*` and compares the field's memory with
/// `initial` parsed at that width; the port compares the stored
/// [`EpicsValue`](crate::types::EpicsValue) with `initial` parsed at the
/// value's own width, which is the same comparison without the pointer cast.
/// A missing `initial` is C's zero/empty default in both.
///
/// The `DBF_DEVICE` arm is C's one non-comparison: it answers "no device
/// support is declared for this record type" rather than looking at the field
/// (`dbStaticRun.c:346-355`). Here `DTYP` is a menu-backed field like any
/// other and takes the index-0 default, so a record whose `.db` never spelled
/// `DTYP` is omitted at level 0 exactly as C omits it.
fn is_default_value(
    desc: &crate::server::record::FieldDesc,
    value: &crate::types::EpicsValue,
) -> bool {
    use crate::types::EpicsValue as V;

    let initial = desc.initial;
    match value {
        V::String(s) => match initial {
            Some(init) => s.as_str_lossy() == init,
            None => s.as_str_lossy().is_empty(),
        },
        V::Enum(v) | V::EnumWithChoices { index: v, .. } => {
            let Some(init) = initial else {
                return *v == 0;
            };
            // C resolves `initial` against the field's menu first and falls
            // back to parsing it as an unsigned integer
            // (`dbStaticRun.c:330-344`).
            match desc.menu.and_then(|m| m.iter().position(|c| *c == init)) {
                Some(index) => u64::from(*v) == index as u64,
                None => init.parse::<u16>().map(|def| *v == def).unwrap_or(false),
            }
        }
        V::Char(v) => int_is_default(u64::from(*v), initial),
        V::UChar(v) => int_is_default(u64::from(*v), initial),
        V::UShort(v) => int_is_default(u64::from(*v), initial),
        V::ULong(v) => int_is_default(u64::from(*v), initial),
        V::UInt64(v) => int_is_default(*v, initial),
        V::Short(v) => signed_is_default(i64::from(*v), initial),
        V::Long(v) => signed_is_default(i64::from(*v), initial),
        V::Int64(v) => signed_is_default(*v, initial),
        V::Float(v) => float_is_default(f64::from(*v), initial),
        V::Double(v) => float_is_default(*v, initial),
        // C's `default:` arm answers TRUE — an array or any type it has no
        // comparison for is treated as still holding its default
        // (`dbStaticRun.c:365-366`).
        _ => true,
    }
}

fn int_is_default(value: u64, initial: Option<&str>) -> bool {
    match initial {
        // C parses with base 0, so `0x10` and `020` are accepted
        // (`epicsParseUInt32(pflddes->initial, &def, 0, NULL)`).
        Some(init) => parse_c_unsigned(init)
            .map(|def| value == def)
            .unwrap_or(false),
        None => value == 0,
    }
}

fn signed_is_default(value: i64, initial: Option<&str>) -> bool {
    match initial {
        Some(init) => parse_c_signed(init)
            .map(|def| value == def)
            .unwrap_or(false),
        None => value == 0,
    }
}

fn float_is_default(value: f64, initial: Option<&str>) -> bool {
    match initial {
        Some(init) => init
            .trim()
            .parse::<f64>()
            .map(|def| value == def)
            .unwrap_or(false),
        None => value == 0.0,
    }
}

/// C's `epicsParseUInt*(s, &v, 0, NULL)` — base 0, so a `0x`/`0X` prefix is
/// hex, a leading `0` is octal, anything else decimal.
fn parse_c_unsigned(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok();
    }
    if s.len() > 1 && s.starts_with('0') {
        return u64::from_str_radix(&s[1..], 8).ok();
    }
    s.parse::<u64>().ok()
}

fn parse_c_signed(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('-') {
        return parse_c_unsigned(rest)
            .and_then(|v| i64::try_from(v).ok())
            .map(|v| -v);
    }
    parse_c_unsigned(s.strip_prefix('+').unwrap_or(s)).and_then(|v| i64::try_from(v).ok())
}

#[cfg(test)]
mod tests {
    // RTEMS-EXEC-MODEL-ALLOW(1): sync tests that hand-build their own tokio
    // runtime, exactly as the sibling `commands.rs` suite does.
    use super::*;
    use crate::server::database::PvDatabase;
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

    fn registry() -> CommandRegistry {
        let mut reg = CommandRegistry::new();
        super::super::commands::register_builtins(&mut reg);
        reg
    }

    fn capture(ctx: &CommandContext, f: impl FnOnce()) -> String {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        ctx.with_output(std::fs::File::create(&path).unwrap(), f);
        std::fs::read_to_string(&path).unwrap()
    }

    /// The `variable()` lines a C softIoc R7.0.10 `dbDumpVariable` prints that
    /// come from a record `.dbd` — the seven this port vendors. C's other
    /// fifteen are `dbCore.dbd` and `links.dbd` knobs the port has no
    /// counterpart for, so it vendors neither file.
    const C_RECORD_VARIABLES: &str = "\
variable(boHIGHlimit,double)
variable(boHIGHprecision,int)
variable(calcoutODLYlimit,double)
variable(calcoutODLYprecision,int)
variable(histogramSDELprecision,int)
variable(seqDLYlimit,double)
variable(seqDLYprecision,int)
";

    /// `dbDumpVariable` reads the `.dbd` declarations, not the iocsh `var`
    /// table, and the two really are different sets: `asCheckClientIP` is
    /// registered straight into the iocsh table by C's `asDbLib` and by this
    /// port's `access_commands`, and a measured C softIoc leaves it out of
    /// this report.
    #[test]
    fn db_dump_variable_reports_dbd_declarations_and_not_the_iocsh_var_table() {
        let (_db, ctx) = make_ctx();
        let printed = run(&ctx, "dbDumpVariable", &["pdbbase"]).unwrap();
        assert_eq!(printed, C_RECORD_VARIABLES);
        assert!(
            super::super::vars::variable_names().contains(&"asCheckClientIP"),
            "the iocsh variable table holds the knob this report must not"
        );
        assert!(!printed.contains("asCheckClientIP"));
    }

    /// C's `linkList` is operative — `dbFindLinkSup` resolves a link field's
    /// leading key in the very list `dbDumpLink` prints — so the report and
    /// the parser cannot be allowed to drift here either. Every reported type
    /// must load, and the two C types this port refuses must not be reported
    /// as available.
    #[test]
    fn db_dump_link_lists_exactly_the_types_a_link_field_accepts() {
        use crate::server::record::check_json_link_text;
        let (_db, ctx) = make_ctx();
        let printed = run(&ctx, "dbDumpLink", &["pdbbase"]).unwrap();
        assert_eq!(
            printed,
            "link(ca,lnkCaIf)\n\
             link(calc,lnkCalcIf)\n\
             link(const,lnkConstIf)\n\
             link(pva,lsetPVX)\n\
             link(state,lnkStateIf)\n"
        );
        for text in [
            "{const:1}",
            "{calc:{expr:\"A\"}}",
            "{pva:\"X\"}",
            "{ca:\"X\"}",
            "{state:\"x\"}",
        ] {
            assert!(
                check_json_link_text(text).is_ok(),
                "{text} is reported by dbDumpLink and must load"
            );
        }
        for (key, text) in [
            ("debug", "{debug:{const:1}}"),
            ("trace", "{trace:{const:1}}"),
        ] {
            assert!(
                check_json_link_text(text).is_err(),
                "{text} is refused, so it must stay out of the report"
            );
            assert!(!printed.contains(&format!("link({key},")));
        }
    }

    /// A `link()` line read at run time appends after the build's block and
    /// keeps the `jlif` name it declared — `dbLinkType` stores both strings
    /// verbatim (`dbLexRoutines.c:893-894`) and resolves neither.
    #[test]
    fn a_runtime_link_declaration_appends_with_its_jlif_name() {
        use std::io::Write;
        let (_db, ctx) = make_ctx();
        let mut tmp = tempfile::Builder::new().suffix(".dbd").tempfile().unwrap();
        // Sorts ahead of every built-in key, so only the two-block rule can
        // put it last.
        writeln!(tmp, "link(aaa, jlifAaa)").unwrap();
        run(&ctx, "dbLoadDatabase", &[tmp.path().to_str().unwrap()]).unwrap();

        assert_eq!(
            run(&ctx, "dbDumpLink", &["pdbbase"]).unwrap(),
            "link(ca,lnkCaIf)\n\
             link(calc,lnkCalcIf)\n\
             link(const,lnkConstIf)\n\
             link(pva,lsetPVX)\n\
             link(state,lnkStateIf)\n\
             link(aaa,jlifAaa)\n"
        );
    }

    /// C's `dbior` and `dbDumpDriver` walk one list (`dbTest.c:723-725`,
    /// `dbStaticLib.c:1076-1089`), so a driver one of them can see is a driver
    /// the other reports. The port has one registry behind both; this is the
    /// case that stops them drifting apart again.
    #[test]
    fn a_registered_driver_is_visible_to_both_db_dump_driver_and_dbior() {
        use crate::server::driver_support::{DriverSupport, register_driver_support};

        struct Probe;
        impl DriverSupport for Probe {
            fn report(&self, _level: i32) -> Option<String> {
                Some("probe".into())
            }
        }

        let (_db, ctx) = make_ctx();
        assert!(register_driver_support(
            "drvDumpDriverProbe",
            Arc::new(Probe)
        ));
        let dumped = run(&ctx, "dbDumpDriver", &["pdbbase"]).unwrap();
        assert!(dumped.contains("driver(drvDumpDriverProbe)\n"), "{dumped}");
        let reported = run(&ctx, "dbior", &["drvDumpDriverProbe", "0"]).unwrap();
        assert!(reported.contains("drvDumpDriverProbe"), "{reported}");
    }

    fn run(ctx: &CommandContext, name: &str, tokens: &[&str]) -> Result<String, String> {
        let reg = registry();
        let cmd = reg.get(name).unwrap();
        let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
        let args = super::super::registry::parse_args(&tokens, &cmd.args).unwrap();
        let mut outcome = Ok(());
        let printed = capture(ctx, || {
            outcome = cmd.handler.call(&args, ctx).map(|_| ());
        });
        outcome.map(|()| printed)
    }

    /// Measured against C `softIoc` R7.0.10, `dbDumpRecordType pdbbase ai`:
    /// the port's lines are C's with the `offset` and `size` columns and the
    /// closing `rset * (nil) rec_size 1200` removed, and nothing else.
    #[test]
    fn dump_record_type_reproduces_c_s_report_for_ai() {
        let (_db, ctx) = make_ctx();
        let out = run(&ctx, "dbDumpRecordType", &["pdbbase", "ai"]).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "name(ai) no_fields(93) no_prompt(49) no_links(6)");
        assert_eq!(lines[1], "index name\tsortind sortname");
        // C: `    0      0   61 NAME\t     24 ACKS`
        assert_eq!(lines[2], "    0 NAME\t     24 ACKS");
        // C: `   92   1192    8 SIMPVT\t     48 VAL`
        assert_eq!(lines[94], "   92 SIMPVT\t     48 VAL");
        // TSEL, SDIS, FLNK, INP, SIOL, SIML in `papFldDes` order.
        assert_eq!(lines[95], "link_ind  8 12 47 49 84 86");
        assert_eq!(lines[96], "indvalFlddes 48 name VAL");
        assert_eq!(
            lines.len(),
            97,
            "no line beyond C's, and none of C's dropped"
        );
    }

    /// The three counts on C's first line are derived, not stored, so a record
    /// type with many links is the boundary that separates a working
    /// derivation from one that happens to agree on `ai`. C `calcout`.
    #[test]
    fn dump_record_type_counts_calcout_s_links_like_c() {
        let (_db, ctx) = make_ctx();
        let out = run(&ctx, "dbDumpRecordType", &["pdbbase", "calcout"]).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "name(calcout) no_fields(171) no_prompt(64) no_links(25)"
        );
        assert_eq!(
            lines[173],
            "link_ind  8 12 47 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74"
        );
        assert_eq!(lines[174], "indvalFlddes 49 name VAL");
    }

    /// C compares the type argument with `strcmp` (`dbStaticLib.c:3320-3325`),
    /// so unlike `dbDumpMenu` the empty string is a type name that matches
    /// nothing rather than a wildcard.
    #[test]
    fn dump_record_type_takes_no_wildcard() {
        let (_db, ctx) = make_ctx();
        assert_eq!(run(&ctx, "dbDumpRecordType", &["pdbbase", ""]).unwrap(), "");
        assert_eq!(
            run(&ctx, "dbDumpRecordType", &["pdbbase", "*"]).unwrap(),
            ""
        );
        // An absent argument is the only "every type".
        let all = run(&ctx, "dbDumpRecordType", &["pdbbase"]).unwrap();
        assert_eq!(
            all.lines().filter(|l| l.starts_with("name(")).count(),
            RECORD_TYPES.len()
        );
    }

    /// `sortFldInd` is a permutation of the declaration indices whose names
    /// come out in ascending `strcmp` order, and every index C lists in
    /// `link_ind` names a link field. Checked on every record type rather than
    /// on the two whose C output is pinned above.
    #[test]
    fn dump_record_type_sort_and_links_hold_for_every_type() {
        use crate::server::record::dbd_generated as dbd;
        for record_type in RECORD_TYPES {
            let lines = dump_record_type_lines(record_type);
            let decl = dbd::record_declaration_order(record_type).unwrap();
            let rows = &lines[2..2 + decl.len()];

            let mut seen: Vec<usize> = rows
                .iter()
                .map(|row| {
                    let (_, tail) = row.split_once('\t').unwrap();
                    tail.split_whitespace().next().unwrap().parse().unwrap()
                })
                .collect();
            let sorted_names: Vec<&str> = seen.iter().map(|&i| decl[i]).collect();
            let mut ascending = sorted_names.clone();
            ascending.sort_unstable();
            assert_eq!(sorted_names, ascending, "{record_type} sortname order");
            seen.sort_unstable();
            assert_eq!(
                seen,
                (0..decl.len()).collect::<Vec<_>>(),
                "{record_type} sortind permutation"
            );

            let link_line = lines.iter().find(|l| l.starts_with("link_ind")).unwrap();
            let own = dbd::record_fields(record_type).unwrap_or(&[]);
            for tok in link_line["link_ind".len()..].split_whitespace() {
                let i: usize = tok.parse().unwrap();
                let desc = dbd::DB_COMMON_FIELDS
                    .iter()
                    .chain(own.iter())
                    .find(|d| d.name == decl[i])
                    .unwrap_or_else(|| panic!("{record_type} link_ind {i} has no descriptor"));
                assert!(
                    matches!(
                        desc.declared_dbf,
                        DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink
                    ),
                    "{record_type} link_ind {i} is {}",
                    desc.declared_dbf.name()
                );
            }
        }
    }

    #[test]
    fn a_named_menu_prints_c_s_three_columns() {
        let (_db, ctx) = make_ctx();
        let out = run(&ctx, "dbDumpMenu", &["pdbbase", "menuSimm"]).unwrap();
        // `menuSimm.dbd.pod` verbatim, in `dbWriteMenuFP`'s layout.
        assert_eq!(
            out,
            "menu(menuSimm) {\n\
             \tchoice(menuSimmNO,\"NO\")\n\
             \tchoice(menuSimmYES,\"YES\")\n\
             \tchoice(menuSimmRAW,\"RAW\")\n\
             }\n"
        );
    }

    #[test]
    fn the_choice_identifier_is_not_the_value() {
        // The whole reason the generator now carries a second column: the
        // identifier is the C enum name, and no rule derives it from the value
        // (`"NO CONVERSION"` -> `menuConvertNO_CONVERSION`? no).
        let (_db, ctx) = make_ctx();
        let out = run(&ctx, "dbDumpMenu", &["pdbbase", "menuYesNo"]).unwrap();
        assert!(
            out.contains("choice(menuYesNoNO,\"NO\")"),
            "identifier column missing: {out}"
        );
    }

    #[test]
    fn the_three_dump_all_spellings_agree() {
        let (_db, ctx) = make_ctx();
        let all = run(&ctx, "dbDumpMenu", &["pdbbase"]).unwrap();
        let empty = run(&ctx, "dbDumpMenu", &["pdbbase", ""]).unwrap();
        let star = run(&ctx, "dbDumpMenu", &["pdbbase", "*"]).unwrap();
        assert_eq!(all, empty);
        assert_eq!(all, star);
        assert_eq!(
            all.lines().filter(|l| l.starts_with("menu(")).count(),
            crate::server::record::dbd_generated::MENUS.len()
        );
    }

    #[test]
    fn an_unknown_menu_prints_nothing_and_succeeds() {
        let (_db, ctx) = make_ctx();
        // C's while-loop simply finds no match; it is not an error.
        assert_eq!(
            run(&ctx, "dbDumpMenu", &["pdbbase", "menuNoSuchThing"]).unwrap(),
            ""
        );
    }

    #[test]
    fn every_menu_row_pairs_an_identifier_with_each_choice() {
        for (name, idents, values) in crate::server::record::dbd_generated::MENUS {
            assert_eq!(
                idents.len(),
                values.len(),
                "menu({name}) has {} identifiers for {} choices",
                idents.len(),
                values.len()
            );
        }
    }

    fn load(ctx: &CommandContext, body: &str) {
        use std::io::Write;
        let tmp = tempfile::Builder::new().suffix(".db").tempfile().unwrap();
        write!(tmp.as_file(), "{body}").unwrap();
        let reg = registry();
        let cmd = reg.get("dbLoadRecords").unwrap();
        let args = super::super::registry::parse_args(
            &[tmp.path().to_string_lossy().to_string()],
            &cmd.args,
        )
        .unwrap();
        cmd.handler.call(&args, ctx).unwrap();
    }

    /// The `.db` frame `dbWriteRecordFP` emits around every record
    /// (`dbStaticLib.c:830-835`, `:865`).
    #[test]
    fn dump_record_emits_the_db_frame() {
        let (_db, ctx) = make_ctx();
        load(&ctx, "record(ai,\"AI:ONE\") { field(EGU,\"mm\") }\n");

        let out = run(&ctx, "dbDumpRecord", &["pdbbase"]).unwrap();
        assert!(
            out.contains("record(ai,\"AI:ONE\") {\n"),
            "C opens with `record(<type>,\"<name>\") {{` — got:\n{out}"
        );
        assert!(
            out.contains("\tfield(EGU,\"mm\")\n"),
            "a non-default field is printed at level 0 — got:\n{out}"
        );
        assert!(out.contains("\n}\n"), "C closes the block — got:\n{out}");
    }

    /// C's level gate: level 0 prints only fields differing from the `.dbd`
    /// `initial`, level 1 and up print every field (`dbStaticLib.c:837-847`).
    #[test]
    fn dump_record_level_selects_default_valued_fields() {
        let (_db, ctx) = make_ctx();
        load(&ctx, "record(ai,\"AI:LEVEL\") { field(EGU,\"mm\") }\n");

        let level0 = run(&ctx, "dbDumpRecord", &["pdbbase", "ai", "0"]).unwrap();
        let level1 = run(&ctx, "dbDumpRecord", &["pdbbase", "ai", "1"]).unwrap();
        assert!(
            level1.lines().count() > level0.lines().count(),
            "level 1 drops C's default-value filter — 0:\n{level0}\n1:\n{level1}"
        );
        // `PREC` is `initial("0")` on `ai`, so it is a default at level 0.
        assert!(
            !level0.contains("\tfield(PREC,"),
            "a field still holding its .dbd initial is omitted at level 0 — got:\n{level0}"
        );
        assert!(
            level1.contains("\tfield(PREC,"),
            "and printed at level 1 — got:\n{level1}"
        );
    }

    /// C reports an unknown record type through `dbWriteRecordFP` and prints
    /// nothing else (`dbStaticLib.c:816-822`).
    #[test]
    fn dump_record_reports_an_unknown_record_type() {
        let (_db, ctx) = make_ctx();
        load(&ctx, "record(ai,\"AI:KNOWN\") {}\n");

        let err = run(&ctx, "dbDumpRecord", &["pdbbase", "nosuchtype"]).unwrap_err();
        assert_eq!(err, "dbWriteRecordFP: No record description for nosuchtype");
    }

    /// `*` and an absent argument are one sentinel in C (`dbStaticLib.c:801-804`).
    #[test]
    fn dump_record_star_is_the_all_types_sentinel() {
        let (_db, ctx) = make_ctx();
        load(
            &ctx,
            "record(ai,\"AI:STAR\") {}\nrecord(bi,\"BI:STAR\") {}\n",
        );

        let star = run(&ctx, "dbDumpRecord", &["pdbbase", "*"]).unwrap();
        let bare = run(&ctx, "dbDumpRecord", &["pdbbase"]).unwrap();
        assert_eq!(star, bare);
        assert!(star.contains("AI:STAR") && star.contains("BI:STAR"));

        let only_ai = run(&ctx, "dbDumpRecord", &["pdbbase", "ai"]).unwrap();
        assert!(only_ai.contains("AI:STAR") && !only_ai.contains("BI:STAR"));
    }

    /// C emits the alias nodes after the records of their type
    /// (`dbStaticLib.c:868-877`), naming the TARGET first so the dump reloads.
    #[test]
    fn dump_record_emits_aliases_after_the_records() {
        let (_db, ctx) = make_ctx();
        load(
            &ctx,
            "record(ai,\"AI:TARGET\") {}\nalias(\"AI:TARGET\",\"AI:OTHERNAME\")\n",
        );

        let out = run(&ctx, "dbDumpRecord", &["pdbbase"]).unwrap();
        let alias_line = "alias(\"AI:TARGET\",\"AI:OTHERNAME\")";
        assert!(out.contains(alias_line), "got:\n{out}");
        assert!(
            out.find("record(ai,\"AI:TARGET\")").unwrap() < out.find(alias_line).unwrap(),
            "aliases follow the record blocks — got:\n{out}"
        );
    }

    /// `info()` tags round-trip through the dump (`dbStaticLib.c:855-864`).
    #[test]
    fn dump_record_emits_info_tags() {
        let (_db, ctx) = make_ctx();
        load(
            &ctx,
            "record(ai,\"AI:INFO\") { info(\"Q:group\",\"grp\") }\n",
        );

        let out = run(&ctx, "dbDumpRecord", &["pdbbase"]).unwrap();
        assert!(out.contains("\tinfo(\"Q:group\",\"grp\")\n"), "got:\n{out}");
    }

    /// `dbDumpDevice`'s two `.dbd` lines, byte for byte
    /// (`dbStaticLib.c:3447-3448`).
    #[test]
    fn dump_device_prints_the_choice_and_link_type() {
        let (_db, ctx) = make_ctx();
        let out = run(&ctx, "dbDumpDevice", &["pdbbase", "ai"]).unwrap();
        assert!(out.starts_with("recordtype(ai)\n"), "got:\n{out}");
        assert!(out.contains("\tchoice:    Soft Channel\n"), "got:\n{out}");
        assert!(out.contains("\tlink_type: 0\n"), "got:\n{out}");
        // C breaks out of the record-type walk once the named type is done
        // (`dbStaticLib.c:3484`).
        assert!(!out.contains("recordtype(bi)"), "got:\n{out}");
    }

    /// Without a type argument C walks every record type
    /// (`dbStaticLib.c:3438-3442`).
    #[test]
    fn dump_device_without_a_type_walks_them_all() {
        let (_db, ctx) = make_ctx();
        let out = run(&ctx, "dbDumpDevice", &["pdbbase"]).unwrap();
        assert!(out.contains("recordtype(ai)\n") && out.contains("recordtype(bi)\n"));
        // `*` is the same sentinel (`dbStaticLib.c:3434-3437`).
        assert_eq!(run(&ctx, "dbDumpDevice", &["pdbbase", "*"]).unwrap(), out);
    }

    /// The numbers `dbDumpDevice` prints are C's `link.h:28-43`, not the
    /// variant order of the Rust enum.
    #[test]
    fn link_type_numbers_are_c_s() {
        use crate::server::record::DbLinkType as L;
        assert_eq!(c_link_type_number(L::Constant), 0);
        assert_eq!(c_link_type_number(L::PvLink), 1);
        assert_eq!(c_link_type_number(L::VmeIo), 2);
        assert_eq!(c_link_type_number(L::CamacIo), 3);
        assert_eq!(c_link_type_number(L::AbIo), 4);
        assert_eq!(c_link_type_number(L::GpibIo), 5);
        assert_eq!(c_link_type_number(L::BitbusIo), 6);
        assert_eq!(c_link_type_number(L::JsonLink), 8);
        assert_eq!(c_link_type_number(L::InstIo), 12);
        assert_eq!(c_link_type_number(L::BbgpibIo), 13);
        assert_eq!(c_link_type_number(L::RfIo), 14);
        assert_eq!(c_link_type_number(L::VxiIo), 15);
    }

    /// `dbDumpFunction` prints C's `function(<name>)` per registered
    /// subroutine (`dbStaticLib.c:1130-1132`).
    #[test]
    fn dump_function_lists_the_registered_subroutines() {
        let (db, ctx) = make_ctx();
        assert_eq!(run(&ctx, "dbDumpFunction", &["pdbbase"]).unwrap(), "");

        let mut registry: std::collections::HashMap<
            String,
            Arc<crate::server::record::SubroutineFn>,
        > = std::collections::HashMap::new();
        for name in ["mySubZebra", "mySubAlpha"] {
            registry.insert(
                name.to_string(),
                Arc::new(
                    Box::new(|_r: &mut dyn crate::server::record::Record| Ok(0i64))
                        as crate::server::record::SubroutineFn,
                ),
            );
        }
        ctx.block_on(db.install_subroutine_registry(registry));

        // A HashMap has no declaration order to report, so the names sort.
        assert_eq!(
            run(&ctx, "dbDumpFunction", &["pdbbase"]).unwrap(),
            "function(mySubAlpha)\nfunction(mySubZebra)\n"
        );
    }

    /// An IOC that has loaded no `.dbd` still declares what it was built
    /// with. Measured on C `softIoc` R7.0.10-146, whose `softIoc.dbd` gives
    /// ten lines; these are the eight of them whose feature this crate
    /// carries, in C's own order.
    #[test]
    fn db_dump_registrar_reports_the_compiled_in_declarations() {
        let (_db, ctx) = make_ctx();
        assert_eq!(
            run(&ctx, "dbDumpRegistrar", &["pdbbase"]).unwrap(),
            "registrar(arrInitialize)\n\
             registrar(dbndInitialize)\n\
             registrar(decInitialize)\n\
             registrar(dlloadRegistar)\n\
             registrar(iocshSystemCommand)\n\
             registrar(syncInitialize)\n\
             registrar(tsInitialize)\n\
             registrar(utagInitialize)\n"
        );
    }

    /// A registrar this build compiled in but another crate owns takes the
    /// slot C gives it, not the slot its announcement order would.
    ///
    /// C's ten `softIoc.dbd` lines are ASCII-sorted because `dbdExpand.pl`
    /// writes them `foreach sort keys` (`DBD/Output.pm:100-103`), and
    /// `rsrvRegistrar` — whose body is `epics-ca-rs`'s — is the seventh of
    /// them. Announcing it through the cross-crate seam last must still put
    /// it there. Measured on C `softIoc` R7.0.10-146, `dbDumpRegistrar
    /// pdbbase`, minus the `asSub` line this crate does not carry.
    #[test]
    fn a_registrar_another_crate_owns_lands_in_cs_slot_not_last() {
        let (_db, ctx) = make_ctx();
        super::add_registrars(&["rsrvRegistrar".to_string()]);
        assert_eq!(
            run(&ctx, "dbDumpRegistrar", &["pdbbase"]).unwrap(),
            "registrar(arrInitialize)\n\
             registrar(dbndInitialize)\n\
             registrar(decInitialize)\n\
             registrar(dlloadRegistar)\n\
             registrar(iocshSystemCommand)\n\
             registrar(rsrvRegistrar)\n\
             registrar(syncInitialize)\n\
             registrar(tsInitialize)\n\
             registrar(utagInitialize)\n"
        );
    }

    /// The other side of that boundary: only the BUILD's declarations went
    /// through `dbdExpand.pl`'s sort. A `.dbd` read at run time is parsed as
    /// it stands (`dbLexRoutines.c:923`, a plain `ellAdd`), so its names
    /// append even when they sort ahead of every compiled-in one.
    #[test]
    fn a_runtime_declaration_appends_even_when_it_sorts_first() {
        use std::io::Write;
        let (_db, ctx) = make_ctx();
        let mut tmp = tempfile::Builder::new().suffix(".dbd").tempfile().unwrap();
        writeln!(tmp, "registrar(aaaRuntime)").unwrap();
        run(&ctx, "dbLoadDatabase", &[tmp.path().to_str().unwrap()]).unwrap();

        let out = run(&ctx, "dbDumpRegistrar", &["pdbbase"]).unwrap();
        assert!(
            out.ends_with("registrar(utagInitialize)\nregistrar(aaaRuntime)\n"),
            "runtime name sorted into the build's block: {out}"
        );
    }

    /// The list above is a claim about this crate, so prove each claim
    /// through the surface that would stop carrying it — a filter name the
    /// plugin table no longer knows parses as `UnknownFilter`, and a command
    /// no longer registered is absent from the registry.
    #[test]
    fn built_in_registrars_name_only_features_this_crate_implements() {
        use crate::server::database::filters::{FilterParseError, try_parse_filter_chain};

        let known = |name: &str| {
            !matches!(
                try_parse_filter_chain(&format!("{{\"{name}\":{{}}}}")),
                Err(FilterParseError::UnknownFilter { .. })
            )
        };
        for filter in ["arr", "dbnd", "dec", "sync", "ts", "utag"] {
            assert!(known(filter), "{filter}");
        }
        // The same probe on a name nothing implements, so the assertion above
        // is not passing on a parse that never rejects.
        assert!(!known("nosuchfilter"));

        let mut registry = CommandRegistry::new();
        super::super::commands::register_builtins(&mut registry);
        for command in ["dlload", "system"] {
            assert!(registry.get(command).is_some(), "{command}");
        }
    }

    /// Every `.dbd`-list dump takes C's `iocshArgPdbbase` and refuses anything
    /// that is not it (`iocsh.cpp:872-884`).
    #[test]
    fn the_dbd_declaration_lists_all_take_cs_pdbbase_argument() {
        let (_db, ctx) = make_ctx();
        for name in [
            "dbDumpDriver",
            "dbDumpLink",
            "dbDumpRegistrar",
            "dbDumpVariable",
        ] {
            assert_eq!(
                run(&ctx, name, &["nonsense"]).unwrap_err(),
                "Expecting 'pdbbase' got 'nonsense'.",
                "{name}"
            );
        }
    }

    /// C `dbPvdTableSize` (`dbPvdLib.c:45-60`) wrapped in `iocshSetError`
    /// (`dbStaticIocRegister.c:210`): the power-of-two test prints and fails
    /// the line, everything else is accepted silently.
    #[test]
    fn pvd_table_size_rejects_a_non_power_of_two() {
        let (_db, ctx) = make_ctx();
        let reg = registry();
        let cmd = reg.get("dbPvdTableSize").unwrap();

        let outcome = |tokens: &[&str]| {
            let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
            let args = super::super::registry::parse_args(&tokens, &cmd.args).unwrap();
            let mut failed = false;
            let printed = capture(&ctx, || {
                failed = matches!(cmd.handler.call(&args, &ctx), Ok(CommandOutcome::Failed));
            });
            (printed, failed)
        };

        assert_eq!(outcome(&["1024"]), (String::new(), false));
        assert_eq!(outcome(&["1"]), (String::new(), false));
        // C clamps rather than refuses below MIN_SIZE / above MAX_SIZE.
        assert_eq!(outcome(&["2"]), (String::new(), false));
        assert_eq!(outcome(&["131072"]), (String::new(), false));
        // 0 passes `size & (size - 1)` and is clamped up in C.
        assert_eq!(outcome(&["0"]), (String::new(), false));

        assert_eq!(
            outcome(&["1000"]),
            (
                "dbPvdTableSize: 1000 is not a power of 2\n".to_string(),
                true
            )
        );
        // Signed test, so a negative size is refused, not clamped.
        assert_eq!(
            outcome(&["-4"]),
            ("dbPvdTableSize: -4 is not a power of 2\n".to_string(), true)
        );
    }

    /// C `dbPvdDump` (`dbPvdLib.c:193-234`) is registered at
    /// `dbStaticIocRegister.c:277` with `argPdbbase` and one `iocshArgInt`
    /// named `verbose`, so a C `st.cmd` line parses here unchanged.
    #[test]
    fn db_pvd_dump_is_registered_with_cs_arity() {
        let reg = registry();
        let cmd = reg.get("dbPvdDump").expect("dbPvdDump must be registered");
        assert_eq!(cmd.args.len(), 2);
        assert_eq!(cmd.args[0].name, "pdbbase");
        assert_eq!(cmd.args[1].name, "verbose");
        assert!(matches!(cmd.args[1].arg_type, ArgType::Int));
    }

    /// The directory is the union of the port's three name spaces, which is
    /// what C's is: `dbCreateRecord` (`dbStaticLib.c:1448`) and
    /// `dbCreateAlias` (`:1696`) are `dbPvdAdd`'s only two callers. Measured
    /// against C `softIoc` R7.0.10 over two records and one alias — C's rows
    /// total three, and so does this count.
    #[test]
    fn the_directory_is_records_plus_aliases_plus_simple_pvs() {
        use crate::server::records::{ai::AiRecord, bo::BoRecord};
        use crate::types::EpicsValue;

        let (db, ctx) = make_ctx();
        ctx.block_on(db.add_record("AI:ONE", Box::new(AiRecord::new(1.0))))
            .unwrap();
        ctx.block_on(db.add_record("BO:TWO", Box::new(BoRecord::new(0))))
            .unwrap();
        ctx.block_on(db.add_alias("AI:ALIAS", "AI:ONE")).unwrap();
        assert_eq!(
            run(&ctx, "dbPvdDump", &["pdbbase"]).unwrap(),
            "Process Variable Directory has 3 entries\n"
        );

        ctx.block_on(db.add_pv("SIMPLE:PV", EpicsValue::Double(0.0)))
            .unwrap();
        assert_eq!(
            run(&ctx, "dbPvdDump", &["pdbbase"]).unwrap(),
            "Process Variable Directory has 4 entries\n"
        );
    }

    /// Why this is not `dbl` under a second name. C `softIoc` R7.0.10 on
    /// `AI:ONE`, `BO:TWO` and `alias("AI:ONE", "AI:ALIAS")`:
    ///
    /// ```text
    /// Process Variable Directory has 512 buckets
    ///  [  51]    1    AI:ONE
    ///  [ 385]    1    AI:ALIAS
    ///  [ 429]    1    BO:TWO
    /// 509 buckets empty.
    /// ```
    ///
    /// Both commands name the alias — `dbl` walks the record-type lists the
    /// alias node lives in too (measured: C `dbl` over a database with an
    /// alias prints the alias line). What separates them is the ORDER and the
    /// simple PVs: the directory is a name-sorted dump of every registered
    /// name, `dbl` is C's per-type load-order walk of the record nodes alone.
    #[test]
    fn verbose_dumps_the_directory_by_name_where_dbl_walks_by_type() {
        use crate::server::records::{ai::AiRecord, bo::BoRecord};
        use crate::types::EpicsValue;

        let (db, ctx) = make_ctx();
        ctx.block_on(db.add_record("AI:ONE", Box::new(AiRecord::new(1.0))))
            .unwrap();
        ctx.block_on(db.add_record("BO:TWO", Box::new(BoRecord::new(0))))
            .unwrap();
        ctx.block_on(db.add_alias("AI:ALIAS", "AI:ONE")).unwrap();
        ctx.block_on(db.add_pv("SIMPLE:PV", EpicsValue::Double(0.0)))
            .unwrap();

        let dumped = run(&ctx, "dbPvdDump", &["pdbbase", "1"]).unwrap();
        assert_eq!(
            dumped,
            format!(
                "Process Variable Directory has 4 entries\n{}  AI:ALIAS  AI:ONE  BO:TWO  SIMPLE:PV\n",
                " ".repeat(9)
            ),
            "got {dumped:?}"
        );

        let listed = run(&ctx, "dbl", &[]).unwrap();
        assert_eq!(
            listed, "AI:ONE\nAI:ALIAS\nBO:TWO\n",
            "dbl groups by record type in load order, and the simple PV is \
             not a record node"
        );
    }

    /// C wraps the names four to a row on the second row onwards
    /// (`dbPvdLib.c:223-227`: `i` starts at 1 and the row breaks whenever
    /// `++i % 4` is zero), each name written `"  %s"` and each new row
    /// indented `"\n         "`. With no bucket header to sit behind, every
    /// row here is a continuation row, so the wrap is uniform at four.
    #[test]
    fn the_name_rows_wrap_at_four_like_cs() {
        use crate::server::records::ai::AiRecord;

        let (db, ctx) = make_ctx();
        for i in 0..5 {
            ctx.block_on(db.add_record(&format!("PV:{i}"), Box::new(AiRecord::new(1.0))))
                .unwrap();
        }
        let dumped = run(&ctx, "dbPvdDump", &["pdbbase", "1"]).unwrap();
        let lines: Vec<&str> = dumped.lines().collect();
        assert_eq!(lines.len(), 3, "got {dumped:?}");
        assert_eq!(
            lines[1],
            format!("{}  PV:0  PV:1  PV:2  PV:3", " ".repeat(9))
        );
        assert_eq!(lines[2], format!("{}  PV:4", " ".repeat(9)));
    }

    /// C gates the name walk on the raw `int` (`dbPvdLib.c:222`), so 0 and a
    /// missing argument print no names and a negative verbose prints them.
    #[test]
    fn a_negative_verbose_prints_the_names_and_zero_does_not() {
        use crate::server::records::ai::AiRecord;

        let (db, ctx) = make_ctx();
        ctx.block_on(db.add_record("AI:ONE", Box::new(AiRecord::new(1.0))))
            .unwrap();

        let count = |tokens: &[&str]| run(&ctx, "dbPvdDump", tokens).unwrap().lines().count();
        assert_eq!(count(&["pdbbase"]), 1);
        assert_eq!(count(&["pdbbase", "0"]), 1);
        assert_eq!(count(&["pdbbase", "1"]), 2);
        assert_eq!(count(&["pdbbase", "-1"]), 2);
    }

    /// The case that proved the old shape wrong: both fields are SERVED as
    /// `DBF_STRING`, so reading `dbf_type` printed `DBF_STRING` for a forward
    /// link. `declared_dbf` is the `.dbd` token and separates them.
    #[test]
    fn link_fields_print_their_declared_dbf_not_their_served_type() {
        let (_db, ctx) = make_ctx();

        let flnk = run(&ctx, "dbDumpField", &["pdbbase", "ai", "FLNK"]).unwrap();
        assert!(
            flnk.contains("\t     field_type: DBF_FWDLINK\n"),
            "dbCommon.FLNK must be DBF_FWDLINK, got:\n{flnk}"
        );
        let inp = run(&ctx, "dbDumpField", &["pdbbase", "ai", "INP"]).unwrap();
        assert!(
            inp.contains("\t     field_type: DBF_INLINK\n"),
            "ai.INP must be DBF_INLINK, got:\n{inp}"
        );
        // And the six declaration-only codes really are distinct from the two
        // variants the emit used to collapse them into.
        let out = run(&ctx, "dbDumpField", &["pdbbase", "ao", "OUT"]).unwrap();
        assert!(out.contains("\t     field_type: DBF_OUTLINK\n"), "{out}");
        let dtyp = run(&ctx, "dbDumpField", &["pdbbase", "ai", "DTYP"]).unwrap();
        assert!(dtyp.contains("\t     field_type: DBF_DEVICE\n"), "{dtyp}");
        let scan = run(&ctx, "dbDumpField", &["pdbbase", "ai", "SCAN"]).unwrap();
        assert!(scan.contains("\t     field_type: DBF_MENU\n"), "{scan}");
        let a = run(&ctx, "dbDumpField", &["pdbbase", "aSub", "A"]).unwrap();
        assert!(a.contains("\t     field_type: DBF_NOACCESS\n"), "{a}");
    }

    /// The whole per-field block, in C's order and with C's whitespace —
    /// including the trailing space `special` leaves before its newline when
    /// the field declares none (`dbStaticLib.c:3380-3389`).
    #[test]
    fn one_field_block_matches_c_line_for_line() {
        let (_db, ctx) = make_ctx();
        let out = run(&ctx, "dbDumpField", &["pdbbase", "ai", "PREC"]).unwrap();
        assert_eq!(
            out,
            "recordtype(ai) \n\
             \x20   PREC\n\
             \t      indRecordType: 50\n\
             \t        special: 0 \n\
             \t     field_type: DBF_SHORT\n\
             \tprocess_passive: 0\n\
             \t       property: 1\n\
             \t       interest: 1\n\
             \t       as_level: 1\n\
             \t        initial: \n\
             Attribute: name RTYP value ai\n\
             Attribute: name VERS value none specified\n",
            "got:\n{out:?}"
        );
    }

    /// `indRecordType` is C's `papFldDes` index, which is NOT the position of
    /// the descriptor in the port's own tables: the three `DBF_NOACCESS`
    /// internals `MLOK`, `MLIS` and `BKLNK` sit at 13-15 in C and are dropped
    /// here, so every later field's two numbers differ. Measured on C `softIoc`
    /// R7.0.10.
    #[test]
    fn ind_record_type_is_c_s_papflddes_index() {
        let (_db, ctx) = make_ctx();
        let ind = |field: &str| {
            let out = run(&ctx, "dbDumpField", &["pdbbase", "ai", field]).unwrap();
            out.lines()
                .find_map(|l| l.strip_prefix("\t      indRecordType: "))
                .unwrap_or_else(|| panic!("no indRecordType for {field}: {out}"))
                .to_string()
        };
        assert_eq!(ind("NAME"), "0");
        // C 16; the port's fourteenth dbCommon descriptor.
        assert_eq!(ind("DISP"), "16");
        assert_eq!(ind("VAL"), "48");
        assert_eq!(ind("INP"), "49");
        assert_eq!(ind("PREC"), "50");
    }

    /// A `special()` field carries C's `SPC_*` name after the number, and the
    /// number is dbStatic's, not the enum's declaration index — `SPC_ALARMACK`
    /// is 5 with 4 unused.
    #[test]
    fn special_prints_c_s_number_and_name() {
        assert_eq!(c_special_code(Special::None), 0);
        assert_eq!(c_special_code(Special::NoMod), 1);
        assert_eq!(c_special_code(Special::DbAddr), 2);
        assert_eq!(c_special_code(Special::Scan), 3);
        assert_eq!(c_special_code(Special::AlarmAck), 5);
        assert_eq!(c_special_code(Special::As), 6);
        assert_eq!(c_special_code(Special::Attribute), 7);
        assert_eq!(c_special_code(Special::Mod), 100);
        assert_eq!(c_special_code(Special::Reset), 101);
        assert_eq!(c_special_code(Special::LinConv), 102);
        assert_eq!(c_special_code(Special::Calc), 103);

        // C's table is nine entries for ten codes: SPC_ATTRIBUTE has no name.
        assert_eq!(c_special_name(7), None);
        assert_eq!(c_special_name(5), Some("SPC_ALARMACK"));
        assert_eq!(c_special_name(102), Some("SPC_LINCONV"));

        let (_db, ctx) = make_ctx();
        let out = run(&ctx, "dbDumpField", &["pdbbase", "ai", "SCAN"]).unwrap();
        assert!(out.contains("\t        special: 3 SPC_SCAN\n"), "{out}");
        // `SPC_ALARMACK` appears in no base `.dbd` — C's record support uses
        // it through `special()` calls, never through a field declaration — so
        // the numeric assertions above are the whole regression guard for the
        // off-by-one that `Special as i32` used to give.
        let acks = run(&ctx, "dbDumpField", &["pdbbase", "ai", "ACKS"]).unwrap();
        assert!(acks.contains("\t        special: 1 SPC_NOMOD\n"), "{acks}");
    }

    /// `dbDumpRegistrar` answers the `.dbd` text a load read, so the case has
    /// to run the load. Two facts from C `dbRegistrar` (`dbLexRoutines.c:903-924`):
    /// a repeated name is dropped by the `.dbd` gpHash, and the survivor is
    /// `ellAdd`ed, so the list is first-declaration order and not sorted — C's
    /// output looks alphabetical only because `softIoc.dbd` is `dbExpand`
    /// output and that tool sorts each declaration family.
    ///
    /// Names are unique to this case: the list is process-global, exactly as
    /// `dbDumpPath`'s loaded path is.
    #[test]
    fn dbdumpregistrar_lists_the_dbd_declarations_deduped_in_load_order() {
        use std::io::Write;
        let (_db, ctx) = make_ctx();
        let mut tmp = tempfile::Builder::new().suffix(".dbd").tempfile().unwrap();
        writeln!(
            tmp,
            "registrar(zzRegB)\nregistrar(zzRegA)\nregistrar(zzRegB)"
        )
        .unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        // C refuses a read once the IOC has left `iocVoid` (`dbReadCOM` status
        // -2, measured: `dbLoadDatabase` after `iocInit` adds nothing to C's
        // list either), so the load has to happen here, before `ioc_init`.
        run(&ctx, "dbLoadDatabase", &[&path]).unwrap();

        let out = run(&ctx, "dbDumpRegistrar", &["pdbbase"]).unwrap();
        assert_eq!(out.matches("registrar(zzRegB)").count(), 1, "{out}");
        assert_eq!(out.matches("registrar(zzRegA)").count(), 1, "{out}");
        assert!(
            out.contains("registrar(zzRegB)\nregistrar(zzRegA)\n"),
            "first-declaration order, not sorted: {out}"
        );
    }

    /// `dbPutAttribute` writes the list this command walks, so the two commands
    /// have to agree. Measured on C `softIoc` R7.0.10-146: after
    /// `dbPutAttribute("ai","MYATTR","hello")` the block reads MYATTR, RTYP,
    /// VERS — `strcmp` order, which puts a new name ahead of the two the
    /// `.dbd` read seeded rather than appending it.
    #[test]
    fn dbputattribute_lands_in_the_block_in_strcmp_order() {
        let (_db, ctx) = make_ctx();
        let reg = registry();
        let cmd = reg.get("dbPutAttribute").unwrap();
        let tokens: Vec<String> = ["ai", "MYATTR", "hello"]
            .iter()
            .map(|t| t.to_string())
            .collect();
        let args = super::super::registry::parse_args(&tokens, &cmd.args).unwrap();
        capture(&ctx, || {
            cmd.handler.call(&args, &ctx).unwrap();
        });

        let out = run(&ctx, "dbDumpField", &["pdbbase", "ai", "NOSUCH"]).unwrap();
        assert_eq!(
            out,
            "recordtype(ai) \n\
             Attribute: name MYATTR value hello\n\
             Attribute: name RTYP value ai\n\
             Attribute: name VERS value none specified\n"
        );
        // The block belongs to the record type, not to the dump: another type
        // keeps its own two.
        let other = run(&ctx, "dbDumpField", &["pdbbase", "bo", "NOSUCH"]).unwrap();
        assert_eq!(
            other,
            "recordtype(bo) \n\
             Attribute: name RTYP value bo\n\
             Attribute: name VERS value none specified\n"
        );
    }

    /// C matches the record type with `strcmp`, so unlike `dbDumpDevice` the
    /// empty string is a type name that matches nothing rather than a
    /// wildcard, and an absent argument walks them all
    /// (`dbStaticLib.c:3361-3373`).
    #[test]
    fn the_type_argument_is_strcmp_not_a_wildcard() {
        let (_db, ctx) = make_ctx();
        assert_eq!(run(&ctx, "dbDumpField", &["pdbbase", ""]).unwrap(), "");
        assert_eq!(run(&ctx, "dbDumpField", &["pdbbase", "*"]).unwrap(), "");

        let all = run(&ctx, "dbDumpField", &["pdbbase"]).unwrap();
        assert!(all.contains("recordtype(ai) \n"), "every type is walked");
        assert!(all.contains("recordtype(bo) \n"), "every type is walked");

        // A named type stops the walk at that one.
        let one = run(&ctx, "dbDumpField", &["pdbbase", "ai"]).unwrap();
        assert_eq!(one.matches("recordtype(").count(), 1, "{one}");

        // A field name that no type declares still emits every header AND the
        // attribute block, which is what C does: the header precedes the field
        // filter and the block follows the loop from outside it. Measured on C
        // `softIoc` R7.0.10-146.
        let none = run(&ctx, "dbDumpField", &["pdbbase", "ai", "NOSUCH"]).unwrap();
        assert_eq!(
            none,
            "recordtype(ai) \n\
             Attribute: name RTYP value ai\n\
             Attribute: name VERS value none specified\n"
        );
    }

    /// C's `size` is `sizeof(prec->member)` after `registerRecordDeviceDriver`,
    /// not the `.dbd` `size(N)`: measured on C `softIoc` R7.0.10, `ai.DESC` is
    /// 41, `ai.VAL` 8 and `ai.INP` 80. Only the first is a `.dbd` fact, so the
    /// line is omitted rather than answered with a zero C never prints.
    #[test]
    fn size_is_c_struct_layout_and_is_not_printed() {
        let (_db, ctx) = make_ctx();
        for field in ["DESC", "VAL", "INP"] {
            let out = run(&ctx, "dbDumpField", &["pdbbase", "ai", field]).unwrap();
            assert!(!out.contains("size:"), "{field}: {out}");
        }
    }

    /// C's `%-8s %-20s %-20s %-20s %-s` report line, with the `cvt(...)`
    /// column `LINR = LINEAR` turns on (`dbStaticLib.c:3632-3650`).
    #[test]
    fn device_config_reports_the_hardware_link() {
        let (_db, ctx) = make_ctx();
        load(
            &ctx,
            "record(ai,\"AI:HW\") { field(DTYP,\"Soft Timestamp\") field(INP,\"@a b\") \
             field(LINR,\"LINEAR\") field(EGUL,\"1\") field(EGUF,\"10\") }\n",
        );
        let out = run(&ctx, "dbReportDeviceConfig", &["pdbbase"]).unwrap();
        assert_eq!(
            out,
            format!(
                "{:<8} {:<20} {:<20} {:<20} {}\n",
                "INST", "@a b", "Soft Timestamp", "AI:HW", "cvt(1,10)"
            ),
            "got:\n{out}"
        );
        // `dbhcr` is the same report with no arguments (`dbTest.c:774-783`).
        assert_eq!(run(&ctx, "dbhcr", &[]).unwrap(), out);
    }

    /// The `cvt(` column is `dbGetString`, not the `DBR_STRING` conversion.
    ///
    /// `1` and `10` above render the same either way; these two do not.
    /// Measured on `softIoc` @`R7.0.10`, where the same database reports
    /// `cvt(10000000,1.0e-03)` — `realToString` prints the integer when the
    /// value is within a `delta` of one and goes exponential below `1e-2`,
    /// while `cvtDoubleToString` with the record's `PREC` answered `0.001`.
    #[test]
    fn device_config_cvt_column_is_dbgetstring_not_dbr_string() {
        let (_db, ctx) = make_ctx();
        load(
            &ctx,
            "record(ai,\"AI:CVT\") { field(DTYP,\"Soft Timestamp\") field(INP,\"@\") \
             field(LINR,\"LINEAR\") field(EGUL,\"1e7\") field(EGUF,\"1e-3\") field(PREC,\"3\") }\n",
        );
        let out = run(&ctx, "dbReportDeviceConfig", &["pdbbase"]).unwrap();
        assert!(out.ends_with("cvt(10000000,1.0e-03)\n"), "got:\n{out}");
    }

    /// A record with no hardware link contributes no line: C skips every link
    /// whose type has no `bus[]` entry, `CONSTANT` and `*_LINK` included
    /// (`dbStaticLib.c:3624-3625`).
    #[test]
    fn device_config_skips_soft_records() {
        let (_db, ctx) = make_ctx();
        load(
            &ctx,
            "record(ai,\"AI:SOFT\") { field(INP,\"OTHER.VAL\") }\n\
             record(bi,\"BI:CONST\") { field(INP,\"1\") }\n",
        );
        assert_eq!(run(&ctx, "dbReportDeviceConfig", &["pdbbase"]).unwrap(), "");
    }

    /// A record type with no `LINR` field, and one whose `LINR` is not
    /// `LINEAR`, both get C's empty conversion column
    /// (`dbStaticLib.c:3634-3640`).
    #[test]
    fn device_config_leaves_the_conversion_column_empty() {
        let (_db, ctx) = make_ctx();
        load(
            &ctx,
            "record(ai,\"AI:NOLIN\") { field(DTYP,\"Soft Timestamp\") field(INP,\"@x\") }\n",
        );
        let out = run(&ctx, "dbReportDeviceConfig", &["pdbbase"]).unwrap();
        assert!(out.ends_with("AI:NOLIN             \n"), "got:\n{out:?}");
    }

    /// C's `dbParseLink` discriminators (`dbStaticLib.c:2296-2331`) mapped onto
    /// the eight bus names `bus[]` carries (`:3558-3574`).
    ///
    /// The bus arrives on the parse, so this exercises the identifier table
    /// itself; `c_hw_bus` is only the `bus[]` lookup on top of it.
    #[test]
    fn hardware_bus_names_follow_db_parse_link() {
        let bus = |text: &str| match crate::server::record::parse_link_field(
            text,
            crate::server::record::LinkFieldType::In,
        ) {
            ParsedLink::Hw(hw) => c_hw_bus(&hw),
            _ => None,
        };

        assert_eq!(bus("@dev parm"), Some("INST"));
        assert_eq!(bus("  @dev parm  "), Some("INST"));
        assert_eq!(bus("#C1 S2 @parm"), Some("VME"));
        assert_eq!(bus("#B1 C2 N3"), Some("CAMAC"));
        assert_eq!(bus("#B1 C2 N3 A4"), Some("CAMAC"));
        assert_eq!(bus("#B1 C2 N3 A4 F5"), Some("CAMAC"));
        assert_eq!(bus("#L1 A2 C3 S4 @p"), Some("AB"));
        assert_eq!(bus("#L1 A2 @p"), Some("GPIB"));
        assert_eq!(bus("#L1 N2 P3 S4 @p"), Some("BITBUS"));
        assert_eq!(bus("#L1 B2 G3 @p"), Some("BBGPIB"));
        assert_eq!(bus("#V1 C2 S3 @p"), Some("VXI"));
        assert_eq!(bus("#V1 S2 @p"), Some("VXI"));
        // C's `%i` is base-prefixed and signed.
        assert_eq!(bus("#C0x10 S-2"), Some("VME"));

        // `RF_IO` is the one `#` form `bus[]` never names — it IS parsed, and
        // the report is what skips it.
        assert_eq!(bus("#R1 M2 D3 E4"), None);
        // Not a hardware link at all.
        assert_eq!(bus("OTHER.VAL"), None);
        assert_eq!(bus("1.5"), None);
        assert_eq!(bus("{const:1}"), None);
        assert_eq!(bus(""), None);
        // Malformed `#` forms are C's `goto fail`, i.e. no line.
        assert_eq!(bus("#C S2"), None);
        assert_eq!(bus("#nonsense"), None);
        assert_eq!(bus("#A1 B2 C3 D4 E5 F6"), None);
    }

    /// `dbpr`'s type word for a hardware link is the bus the identifier
    /// letters named, so a CAMAC link stops reading `VME_IO`.
    ///
    /// The eight `link.h` bus types a `#` form can reach plus `INST_IO`,
    /// against `link.h:33-47`.
    #[test]
    fn a_hardware_links_type_word_is_its_bus() {
        let word = |text: &str| {
            crate::server::record::parse_link_field(text, crate::server::record::LinkFieldType::In)
                .link_type_after_init(text)
                .c_name()
        };

        assert_eq!(word("@dev p1 p2"), "INST_IO");
        assert_eq!(word("#C1 S2 @parm"), "VME_IO");
        assert_eq!(word("#B1 C2 N3 A4 F5"), "CAMAC_IO");
        assert_eq!(word("#R1 M2 D3 E4"), "RF_IO");
        assert_eq!(word("#L1 A2 C3 S4 @p"), "AB_IO");
        assert_eq!(word("#L1 A2 @p"), "GPIB_IO");
        assert_eq!(word("#L1 N2 P3 S4 @p"), "BITBUS_IO");
        assert_eq!(word("#L1 B2 G3 @p"), "BBGPIB_IO");
        assert_eq!(word("#V1 C2 S3 @p"), "VXI_IO");
        assert_eq!(word("#V1 S2 @p"), "VXI_IO");

        // C's `goto fail` leaves the field's link untouched, so what `dbpr`
        // prints is the unset CONSTANT it was born with — never a PV link
        // whose name starts with `#`.
        assert_eq!(word("#nonsense"), "CONSTANT");
        assert_eq!(word("#C S2"), "CONSTANT");
        assert_eq!(word("#A1 B2 C3 D4 E5 F6"), "CONSTANT");
        // `RF_IO` is the one bus that must carry no parm at all
        // (`dbStaticLib.c:2333-2343`).
        assert_eq!(word("#R1 M2 D3 E4 @p"), "CONSTANT");
    }

    /// C `cvtArg` for `iocshArgPdbbase` (`iocsh.cpp:872-884`).
    #[test]
    fn pdbbase_argument_refuses_anything_else() {
        assert!(check_pdbbase(&ArgValue::String("pdbbase".into())).is_ok());
        assert!(check_pdbbase(&ArgValue::String(String::new())).is_ok());
        assert!(check_pdbbase(&ArgValue::String("0".into())).is_ok());
        assert!(check_pdbbase(&ArgValue::Missing).is_ok());
        assert_eq!(
            check_pdbbase(&ArgValue::String("pdbase".into())).unwrap_err(),
            "Expecting 'pdbbase' got 'pdbase'."
        );
    }

    /// C `epicsStrPrintEscaped` (`epicsString.c:230-262`), including the NUL
    /// that its `switch` does not name and so escapes as a non-printable.
    #[test]
    fn escaping_matches_epics_str_print_escaped() {
        assert_eq!(print_escaped("plain"), "plain");
        assert_eq!(print_escaped("a\tb\nc"), "a\\tb\\nc");
        assert_eq!(print_escaped("q\"uote"), "q\\\"uote");
        assert_eq!(print_escaped("back\\slash"), "back\\\\slash");
        assert_eq!(print_escaped("tick'"), "tick\\'");
        assert_eq!(print_escaped("\x07\x08\x0c\x0b\r"), "\\a\\b\\f\\v\\r");
        assert_eq!(print_escaped("\u{0}"), "\\x00");
        assert_eq!(print_escaped("\u{1}"), "\\x01");
        // C escapes byte by byte, so a multi-byte character comes out as one
        // `\xNN` per byte rather than as the character.
        assert_eq!(print_escaped("é"), "\\xc3\\xa9");
        assert_eq!(print_escaped("with space"), "with space");
    }

    /// C `dbIsDefaultValue` (`dbStaticRun.c:230-367`): a missing `initial()` is
    /// the zero/empty default, and a present one is parsed at the field's width
    /// with base 0.
    #[test]
    fn default_value_follows_the_dbd_initial() {
        use crate::server::record::{Asl, Base, FieldDesc, Special};
        use crate::types::{DbFieldType, DbfCode, EpicsValue};

        let desc = |initial, menu| FieldDesc {
            name: "F",
            dbf_type: DbFieldType::Long,
            declared_dbf: DbfCode::Long,
            runtime_typed: false,
            read_only: false,
            special: Special::None,
            declared_special: Special::None,
            pp: false,
            asl: Asl::Asl1,
            size: 0,
            extra: None,
            menu,
            initial,
            interest: 0,
            prop: false,
            prompt: None,
            promptgroup: None,
            base: Base::Decimal,
        };

        // No `initial()` — zero and empty are the defaults.
        assert!(is_default_value(&desc(None, None), &EpicsValue::Long(0)));
        assert!(!is_default_value(&desc(None, None), &EpicsValue::Long(1)));
        assert!(is_default_value(
            &desc(None, None),
            &EpicsValue::String("".into())
        ));
        assert!(is_default_value(
            &desc(None, None),
            &EpicsValue::Double(0.0)
        ));

        // `initial("5")` makes 5 the default and 0 a written value.
        assert!(is_default_value(
            &desc(Some("5"), None),
            &EpicsValue::Long(5)
        ));
        assert!(!is_default_value(
            &desc(Some("5"), None),
            &EpicsValue::Long(0)
        ));

        // Base 0: `0x10` is 16 and `020` is 16, as `epicsParseInt32` reads them.
        assert!(is_default_value(
            &desc(Some("0x10"), None),
            &EpicsValue::Long(16)
        ));
        assert!(is_default_value(
            &desc(Some("020"), None),
            &EpicsValue::Long(16)
        ));

        // A menu `initial` resolves against the choice list first
        // (`dbStaticRun.c:330-344`), and falls back to a plain integer.
        static CHOICES: &[&str] = &["NO", "YES"];
        assert!(is_default_value(
            &desc(Some("YES"), Some(CHOICES)),
            &EpicsValue::Enum(1)
        ));
        assert!(!is_default_value(
            &desc(Some("YES"), Some(CHOICES)),
            &EpicsValue::Enum(0)
        ));
        assert!(is_default_value(
            &desc(Some("1"), Some(CHOICES)),
            &EpicsValue::Enum(1)
        ));
    }
}
