//! Emit the parsed `.dbd` declarations as a checked-in Rust source file.
//!
//! The one place the `DBF_*` -> `DbFieldType` mapping is decided is
//! [`dbf_to_rust`]. It emits the *field* type the `.dbd` declares — the
//! promotion a CA client observes (`DBF_ULONG` -> `DBR_DOUBLE`,
//! `DBF_USHORT` -> `DBR_LONG`, ...) is not folded in here, because
//! `DbFieldType::ca_wire_type` already owns it. Folding it in twice would
//! double-promote and lose the native width PVA serves.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::parse::{Device, Field, Menu, RecordType, Variable};

/// The `DBF_*` -> `DbFieldType` map, mirroring C `dbStaticLib`'s `dbfType`
/// enum and `dbAccess.c`'s `mapDBFToDBR`:
///
/// * `DBF_MENU`, `DBF_DEVICE` are served to clients as `DBR_ENUM` with the
///   menu's choice strings (`dbAccess.c:1074` `mapDBFToDBR`), so they carry
///   `DbFieldType::Enum` — not the `Short` the port used to declare.
/// * `DBF_INLINK`/`DBF_OUTLINK`/`DBF_FWDLINK` are served as `DBR_STRING`
///   (the link's `dbGetLinkStr` rendering).
/// * `DBF_NOACCESS` is a C-internal pointer (`RSET`, `DPVT`, `MLOK`, `BPTR`,
///   ...). It is not CA-visible and has no port representation; the caller
///   drops it.
pub fn dbf_to_rust(dbf: &str) -> Option<&'static str> {
    Some(match dbf {
        "DBF_STRING" => "String",
        "DBF_CHAR" => "Char",
        "DBF_UCHAR" => "UChar",
        "DBF_SHORT" => "Short",
        "DBF_USHORT" => "UShort",
        "DBF_LONG" => "Long",
        "DBF_ULONG" => "ULong",
        "DBF_INT64" => "Int64",
        "DBF_UINT64" => "UInt64",
        "DBF_FLOAT" => "Float",
        "DBF_DOUBLE" => "Double",
        "DBF_ENUM" | "DBF_MENU" | "DBF_DEVICE" => "Enum",
        "DBF_INLINK" | "DBF_OUTLINK" | "DBF_FWDLINK" => "String",
        "DBF_NOACCESS" => return None,
        _ => return None,
    })
}

/// The `DbfCode` variant for a `DBF_*` token — C's `dbfType` code
/// (`dbFldTypes.h:24-43` @R7.0.10), carried onto `FieldDesc::declared_dbf`.
///
/// Total where [`dbf_to_rust`] is lossy, and that is the whole point: this is
/// the DECLARATION, so it keeps the six tokens `dbf_to_rust` collapses onto
/// two served types apart. Unknown tokens are an error rather than a drop —
/// a `.dbd` gaining a nineteenth type must fail the generator, not lose its
/// declaration silently.
pub fn dbf_to_code(dbf: &str) -> Result<&'static str, String> {
    Ok(match dbf {
        "DBF_STRING" => "String",
        "DBF_CHAR" => "Char",
        "DBF_UCHAR" => "UChar",
        "DBF_SHORT" => "Short",
        "DBF_USHORT" => "UShort",
        "DBF_LONG" => "Long",
        "DBF_ULONG" => "ULong",
        "DBF_INT64" => "Int64",
        "DBF_UINT64" => "UInt64",
        "DBF_FLOAT" => "Float",
        "DBF_DOUBLE" => "Double",
        "DBF_ENUM" => "Enum",
        "DBF_MENU" => "Menu",
        "DBF_DEVICE" => "Device",
        "DBF_INLINK" => "Inlink",
        "DBF_OUTLINK" => "Outlink",
        "DBF_FWDLINK" => "Fwdlink",
        "DBF_NOACCESS" => "NoAccess",
        other => return Err(format!("{other}: not one of C's 18 DBF_* types")),
    })
}

/// True for a field that has no CA-visible representation at all: a
/// `DBF_NOACCESS` C-internal pointer (`RSET`, `DPVT`, `MLOK`, `BPTR`, ...).
///
/// `DBF_NOACCESS` alone is NOT the test. A `special(SPC_DBADDR)` field is
/// *also* declared `DBF_NOACCESS` in the `.dbd` — because its C struct member
/// is a bare pointer — and yet it is the most visible field the record has:
/// `waveform.VAL`, `lsi.VAL`, `aSub.A`. C types those at name-resolution time
/// from the record's `cvt_dbaddr`, so they are resolved from
/// `dbd/cvt_dbaddr.types` instead of dropped.
pub fn is_internal(f: &Field) -> bool {
    f.dbf == "DBF_NOACCESS" && f.special.as_deref() != Some("SPC_DBADDR")
}

/// The byte width of the C struct member an `extra("...")` declaration names.
///
/// No `.dbd` spells this. A C IOC's generated `<rec>RecordSizeOffset` writes
/// `sizeof(prec->member)` into `pdbFldDes->size`, so the width comes from the
/// C type — which is why the declaration text has to be carried at all, and
/// why `dbpr`'s `DBF_NOACCESS` arm reads both (`dbTest.c:1235-1262`).
///
/// `None` for a member this port cannot state a width for, and the test is on
/// the DECLARATOR, not just the type name: only a plain scalar member has a
/// width the type alone gives. A pointer (`*`) is refused because C prints its
/// ADDRESS (`PTR %p`), which no two IOCs agree on. An array (`[`) is refused
/// because its width is the bound, not the element — `calc.RPCL` is
/// `extra("char rpcl[INFIX_TO_POSTFIX_SIZE(160)]")`, whose size is that C
/// macro expanded, and reading the leading `char` would carry the row at one
/// byte and print a one-byte `dbpr` line where C prints the whole buffer.
fn extra_width(extra: &str) -> Option<u16> {
    if extra.contains('*') || extra.contains('[') {
        return None;
    }
    match extra.split_whitespace().next()? {
        "epicsInt8" | "epicsUInt8" | "char" => Some(1),
        "epicsInt16" | "epicsUInt16" | "short" => Some(2),
        "epicsInt32" | "epicsUInt32" | "epicsFloat32" => Some(4),
        "epicsInt64" | "epicsUInt64" | "epicsFloat64" => Some(8),
        // `epicsTimeStamp` is two `epicsUInt32` (`epicsTime.h`), which is
        // what `dbCommon.TIME` stands for.
        "epicsTimeStamp" => Some(8),
        _ => None,
    }
}

/// An internal the port carries as a full `FieldDesc` rather than as a bare
/// name in `record_noaccess_fields` / `DB_COMMON_NOACCESS`.
///
/// A `DBF_NOACCESS` row is never SERVED — `mapDBFToDBR` refuses its channel —
/// but C still resolves the name and still prints the field in `dbpr`, and
/// both need the declaration, not just proof that the name exists. So the row
/// is kept exactly when the port can say how wide it is; everything else stays
/// a name.
pub fn internal_width(f: &Field) -> Option<u16> {
    if !is_internal(f) {
        return None;
    }
    extra_width(f.extra.as_deref()?)
}

/// True when the row is dropped to a bare name.
pub fn is_dropped_internal(f: &Field) -> bool {
    is_internal(f) && internal_width(f).is_none()
}

/// What C's `cvt_dbaddr` makes of one `special(SPC_DBADDR)` field —
/// `dbd/cvt_dbaddr.types`.
pub struct CvtDbAddr {
    pub dbf: String,
    /// The `special` C leaves on the DBADDR. Usually still `SPC_DBADDR`, but
    /// `lsi`/`lso`/`asyn` raise `SPC_NOMOD` (making the field unwritable over
    /// CA — `rsrv/camessage.c:2545-2547`) or `SPC_MOD` inside `cvt_dbaddr` itself.
    pub special: String,
    /// The field whose value selects the type at runtime (`FTVL`, `SDEF`,
    /// `FTA`), or `None` when C's `cvt_dbaddr` always yields the same type.
    pub selector: Option<String>,
}

/// Parse `dbd/cvt_dbaddr.types`:
/// `record.FIELD  DBF_TYPE  SPC_xxx  selector|-  cite`.
pub fn parse_cvt_dbaddr(src: &str) -> Result<BTreeMap<(String, String), CvtDbAddr>, String> {
    let mut out = BTreeMap::new();
    for (n, line) in src.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(key), Some(dbf), Some(spc), Some(sel)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(format!(
                "cvt_dbaddr.types:{}: expected \
                 `record.FIELD DBF_TYPE SPC_xxx selector`, got `{line}`",
                n + 1
            ));
        };
        let (rec, field) = key
            .split_once('.')
            .ok_or_else(|| format!("cvt_dbaddr.types:{}: `{key}` is not `record.FIELD`", n + 1))?;
        if dbf_to_rust(dbf).is_none() {
            return Err(format!("cvt_dbaddr.types:{}: unknown type `{dbf}`", n + 1));
        }
        special_variant(spc).map_err(|e| format!("cvt_dbaddr.types:{}: {e}", n + 1))?;
        let prev = out.insert(
            (rec.to_string(), field.to_string()),
            CvtDbAddr {
                dbf: dbf.to_string(),
                special: spc.to_string(),
                selector: (sel != "-").then(|| sel.to_string()),
            },
        );
        if prev.is_some() {
            return Err(format!("cvt_dbaddr.types:{}: `{key}` listed twice", n + 1));
        }
    }
    Ok(out)
}

/// The `DbLinkType` variant for a `device()` line's `link_type` argument.
///
/// The names are C's, from `link.h`; an unknown one is a generator error rather
/// than a silent catch-all, because the whole point of carrying the link type is
/// that `dbCanSetLink` compares it for equality.
fn link_type_variant(link_type: &str) -> Result<&'static str, String> {
    Ok(match link_type {
        "CONSTANT" => "Constant",
        "PV_LINK" => "PvLink",
        "VME_IO" => "VmeIo",
        "CAMAC_IO" => "CamacIo",
        "AB_IO" => "AbIo",
        "GPIB_IO" => "GpibIo",
        "BITBUS_IO" => "BitbusIo",
        "INST_IO" => "InstIo",
        "BBGPIB_IO" => "BbgpibIo",
        "RF_IO" => "RfIo",
        "VXI_IO" => "VxiIo",
        other => return Err(format!("device(...): unknown link type `{other}`")),
    })
}

/// The `SPC_*` token -> the `Special` variant emitted. `special.h` numbers
/// them; the port names them.
fn special_variant(spc: &str) -> Result<&'static str, String> {
    Ok(match spc {
        "SPC_NOMOD" => "NoMod",
        "SPC_DBADDR" => "DbAddr",
        "SPC_SCAN" => "Scan",
        "SPC_ALARMACK" => "AlarmAck",
        "SPC_AS" => "As",
        "SPC_ATTRIBUTE" => "Attribute",
        "SPC_MOD" => "Mod",
        "SPC_RESET" => "Reset",
        "SPC_LINCONV" => "LinConv",
        "SPC_CALC" => "Calc",
        other => return Err(format!("unknown special({other}) — extend special_variant")),
    })
}

/// A menu's Rust const name: `menuSimm` -> `MENU_SIMM`, `calcoutOOPT` ->
/// `MENU_CALCOUT_OOPT`, `aaiPOST` -> `MENU_AAI_POST`.
pub fn menu_const(name: &str) -> String {
    let mut out = String::from("MENU_");
    let mut prev_lower = false;
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() && prev_lower && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    // `menuSimm` lexes to `MENU_MENU_SIMM` under the rule above; strip the
    // redundant `MENU_` the EPICS naming convention already carries.
    out.replace("MENU_MENU_", "MENU_")
}

/// A record type's field-table const name: `mbbiDirect` -> `MBBI_DIRECT_FIELDS`.
pub fn fields_const(record: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for (i, c) in record.chars().enumerate() {
        if c.is_ascii_uppercase() && prev_lower && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    out.push_str("_FIELDS");
    out
}

fn rust_str(s: &str) -> String {
    format!("{s:?}")
}

/// An optional `.dbd` string attribute as a Rust `Option<&'static str>`.
fn opt_str(v: &Option<String>) -> String {
    match v {
        Some(t) => format!("Some({})", rust_str(t)),
        None => "None".to_string(),
    }
}

pub struct Input<'a> {
    pub menus: &'a BTreeMap<String, Menu>,
    pub devices: &'a [Device],
    pub records: &'a [RecordType],
    /// `dbCommon`'s fields, for the one target that emits them (base). Empty for
    /// a downstream target: `dbCommon` is declared once and the declaration is
    /// base's.
    pub common: &'a [Field],
    pub cvt: &'a BTreeMap<(String, String), CvtDbAddr>,
    /// The vendored `.dbd` directory this module is generated from, for the
    /// header.
    pub dbd_dir: &'a str,
    /// How this module names `epics-base-rs` — `crate` when it is emitted into
    /// base, `epics_base_rs` when into a downstream crate.
    pub base_path: &'a str,
    /// Menus a field may reference that this target's `.dbd`s do not declare:
    /// the EPICS Base menus (`menuYesNo`, `menuOmsl`, `menuAlarmSevr`, ...),
    /// mapped to base's generated const path. Empty for the base target, which
    /// declares every menu it references.
    pub external_menus: &'a BTreeMap<String, String>,
    /// Record type names in DBD load order. Emitted only by the base target;
    /// empty everywhere else.
    pub order: &'a [String],
    /// Every `variable()` this target's `.dbd`s declare.
    pub variables: &'a [Variable],
}

/// The type to emit for one field: the `.dbd`'s `DBF_*`, except for a
/// `special(SPC_DBADDR)` field, whose type C takes from the record's
/// `cvt_dbaddr` and this generator takes from `dbd/cvt_dbaddr.types`.
///
/// A missing entry is an error, not a fallback. That is what makes SPC_DBADDR a
/// *closed* exception: a `.dbd` cannot introduce a dynamically-typed field that
/// silently lands with the wrong width, because the generator will not build.
/// Resolves BOTH, because C's `cvt_dbaddr` overwrites both: `lsi.OVAL` is
/// `SPC_DBADDR` in the `.dbd` but `SPC_NOMOD` by the time a client sees it, and
/// SPC_NOMOD is what makes a field unwritable over CA.
fn field_dbf<'a>(
    record: &str,
    f: &'a Field,
    cvt: &'a BTreeMap<(String, String), CvtDbAddr>,
    dbd_dir: &str,
) -> Result<(&'a str, &'a str), String> {
    let declared = f.special.as_deref().unwrap_or("");
    if declared != "SPC_DBADDR" {
        return Ok((&f.dbf, declared));
    }
    cvt.get(&(record.to_string(), f.name.clone()))
        .map(|c| (c.dbf.as_str(), c.special.as_str()))
        .ok_or_else(|| {
            format!(
                "{record}.{}: special(SPC_DBADDR) — its type and special come from the \
                 record's cvt_dbaddr, which the .dbd does not carry. Add a line to \
                 {dbd_dir}/cvt_dbaddr.types citing the C source.",
                f.name
            )
        })
}

pub fn emit(input: &Input<'_>) -> Result<String, String> {
    let mut out = String::new();
    let base = input.base_path;

    writeln!(
        out,
        "//! Record field tables generated from the vendored EPICS `.dbd` record\n\
         //! declarations — the machine-readable spec every `FieldDesc` used to be\n\
         //! hand-copied from.\n\
         //!\n\
         //! @generated by `tools/dbd-codegen` from `{}/*.dbd`.\n\
         //! DO NOT EDIT. Re-run `cargo run -p dbd-codegen -- --write` after changing a\n\
         //! vendored `.dbd`. Drift is caught by `dbd_codegen::tests::generated_files_are_not_stale`,\n\
         //! which `cargo nextest run` executes — locally and in CI — so a stale table\n\
         //! is a failing test, not a comment asking you to remember.\n\
         //!\n\
         //! What is *not* here, and why:\n\
         //!\n\
         //! * **`dbCommon` fields.** The port models them once in\n\
         //!   [`CommonFields`]({base}::server::record::CommonFields), so splicing them\n\
         //!   into every record table would double-declare them. Base's table emits\n\
         //!   them separately as `DB_COMMON_FIELDS` so the two models can be reconciled\n\
         //!   (and so a `dbCommon` addition is a diff, not a silent gap).\n\
         //! * **`DBF_NOACCESS` rows whose `extra(...)` names a pointer or an\n\
         //!   array** (`RSET`, `DPVT`, `MLOK`, `BPTR`, `PPN`, ...). C keeps\n\
         //!   `sizeof` the struct member in `pdbFldDes->size`, and neither a\n\
         //!   pointer (whose C value is an address no two IOCs agree on) nor an\n\
         //!   array (whose width is its bound, not its element) has one this port\n\
         //!   can state, so the *descriptor* is dropped rather than invented —\n\
         //!   but the NAME is kept (`record_noaccess_fields`): C's `dbNameToAddr`\n\
         //!   resolves them, so a SEARCH for `REC.BPTR` is answered and the\n\
         //!   refusal lands at channel creation, and the search gate needs the\n\
         //!   names to do the same.\n\
         //!\n\
         //!   A row whose `extra(...)` names a plain scalar — `dbCommon`'s `BKPT`\n\
         //!   and `TIME` — is CARRIED, descriptor and width both, because `dbpr`\n\
         //!   prints its bytes and `dba` reports its `Field Size` and both read\n\
         //!   the declaration. Carried is not servable: `FieldDesc::unreadable`\n\
         //!   is what refuses the read, so the SEARCH is answered off the\n\
         //!   descriptor instead of off the name list.\n",
        input.dbd_dir
    )
    .unwrap();
    out.push_str("#![allow(clippy::all)]\n\n");
    if !input.devices.is_empty() {
        writeln!(out, "use {base}::server::record::DbLinkType;").unwrap();
    }
    // `Asl`, `Special`, `DbFieldType` and `DbfCode` appear ONLY in an emitted `FieldDesc`
    // literal, so a target with no record fields (asyn declares only `device()`
    // lines, no `recordtype`) would import them unused — a hard error under
    // `clippy -D warnings`, which promotes rustc's `unused_imports`. `FieldDesc`
    // itself is still named by the `record_fields` return type even for such a
    // target, so it is imported unconditionally.
    let emits_fields = !input.records.is_empty() || !input.common.is_empty();
    if emits_fields {
        writeln!(
            out,
            "use {base}::server::record::{{Asl, Base, FieldDesc, Special}};"
        )
        .unwrap();
        writeln!(out, "use {base}::types::{{DbFieldType, DbfCode}};\n").unwrap();
    } else {
        writeln!(out, "use {base}::server::record::FieldDesc;\n").unwrap();
    }

    // --- menus ---------------------------------------------------------
    out.push_str(
        "// ---------------------------------------------------------------------\n\
         // `menu(...)` choice tables. The index is the stored `epicsEnum16` and is\n\
         // wire-visible (a `DBF_MENU` field is served as `DBR_ENUM`), so the order\n\
         // here is the `.dbd` declaration order, verbatim.\n\
         // ---------------------------------------------------------------------\n\n",
    );
    let mut menu_names: BTreeMap<String, String> = BTreeMap::new();
    for (name, menu) in input.menus {
        let konst = menu_const(name);
        if let Some(prev) = menu_names.insert(name.clone(), konst.clone()) {
            return Err(format!("menu const collision: {name} -> {prev}/{konst}"));
        }
        writeln!(out, "/// `menu({name})`").unwrap();
        writeln!(out, "pub static {konst}: &[&str] = &[").unwrap();
        for c in &menu.choices {
            writeln!(out, "    {},", rust_str(c)).unwrap();
        }
        out.push_str("];\n\n");
    }

    // Reverse map so two menus with the same const name are caught.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for k in menu_names.values() {
        if !seen.insert(k.as_str()) {
            return Err(format!("duplicate menu const {k}"));
        }
    }

    // The menu a field can name: one this target declares, or — for a downstream
    // target — one of base's, named by its path there. A target NEVER re-declares
    // a menu it did not write: the choice index is wire-visible, so a second copy
    // of `menuAlarmSevr` is exactly the second declaration this generator exists
    // to remove.
    let mut menu_paths = menu_names.clone();
    for (name, path) in input.external_menus {
        if !menu_paths.contains_key(name) {
            menu_paths.insert(name.clone(), path.clone());
        } else {
            return Err(format!(
                "menu({name}) is declared by {} AND by epics-base-rs — the wire-visible \
                 choice indices would be free to drift apart. Delete the local copy and \
                 let the field reference base's.",
                input.dbd_dir
            ));
        }
    }

    out.push_str(
        "/// Every `menu()` declared by a vendored `.dbd`, keyed by its EPICS name.\n\
         ///\n\
         /// This is the table `get_enum_strs` answers a `DBF_MENU` field from: the\n\
         /// field's [`FieldDesc::menu`] already points at its choices, and this map\n\
         /// exists for the by-name lookups (`.db` loading, `dbpr`).\n\
         pub fn menu_choices(menu: &str) -> Option<&'static [&'static str]> {\n",
    );
    emit_str_lookup(
        &mut out,
        "menu",
        menu_names.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    );
    out.push_str("}\n\n");

    // The `choice()` identifier is the C enum name, so nothing on the wire ever
    // sees it and `menu_choices` has no column for it — but `dbWriteMenuFP`
    // (`dbStaticLib.c:919-924`) prints `choice(<ident>,"<value>")`, so
    // `dbDumpMenu` cannot reproduce its report from the values alone.
    out.push_str(
        "/// Every `menu()` this target declares: EPICS name, `choice()` identifiers,\n\
         /// and choice values — the three columns `dbWriteMenuFP` prints\n\
         /// (`dbStaticLib.c:919-924`), which is what `dbDumpMenu` reports.\n\
         ///\n\
         /// C walks `pdbbase->menuList` in `.dbd` LOAD order. A compiled-in table has\n\
         /// no load order, so the rows are in name order; the per-menu choice order is\n\
         /// still the `.dbd` order, because that one is the wire-visible index.\n\
         pub static MENUS: &[(&str, &[&str], &[&str])] = &[\n",
    );
    for (name, menu) in input.menus {
        let konst = menu_const(name);
        write!(out, "    ({}, &[", rust_str(name)).unwrap();
        for (i, id) in menu.identifiers.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&rust_str(id));
        }
        writeln!(out, "], {konst}),").unwrap();
    }
    out.push_str("];\n\n");

    // `variable()` is the second half of C's `dbCore.dbd`-style declaration:
    // `dbVariable` appends it to `pdbbase->variableList`, which is what
    // `dbDumpVariable` prints. It is NOT the iocsh `var` table — C fills that
    // one from these declarations through `registerRecordDeviceDriver.pl`, and
    // a variable registered directly from C code (`asCheckClientIP`) is in the
    // iocsh table and absent from this one.
    out.push_str(
        "/// Every `variable()` this target's `.dbd`s declare, as EPICS name and\n\
         /// declared type — the two columns `dbWriteVariableFP` prints\n\
         /// (`dbStaticLib.c:1136-1149`), which is what `dbDumpVariable` reports.\n\
         ///\n\
         /// Name order, which is also C's: `dbVariable` appends in load order, but\n\
         /// the loaded `.dbd` is `dbExpand` output and that tool emits each\n\
         /// declaration family sorted (measured in an installed `softIoc.dbd`).\n\
         pub static VARIABLES: &[(&str, &str)] = &[\n",
    );
    let mut variables: Vec<&Variable> = input.variables.iter().collect();
    variables.sort_by(|a, b| a.name.cmp(&b.name));
    for var in variables {
        writeln!(
            out,
            "    ({}, {}),",
            rust_str(&var.name),
            rust_str(&var.dtype)
        )
        .unwrap();
    }
    out.push_str("];\n\n");

    // --- field tables ---------------------------------------------------
    out.push_str(
        "// ---------------------------------------------------------------------\n\
         // Per-record-type field tables. Record-*own* fields only: `dbCommon` is\n\
         // modelled separately (see the module docs).\n\
         // ---------------------------------------------------------------------\n\n",
    );

    let mut record_consts: Vec<(String, String)> = Vec::new();
    let mut noaccess_consts: Vec<(String, String)> = Vec::new();
    let mut decl_consts: Vec<(String, String)> = Vec::new();
    let mut no_prompt: Vec<(String, usize)> = Vec::new();
    for rec in input.records {
        let konst = fields_const(&rec.name);
        let own: Vec<&Field> = rec
            .fields
            .iter()
            .filter(|f| !f.from_common && !is_dropped_internal(f))
            .collect();
        let internal: Vec<&str> = rec
            .fields
            .iter()
            .filter(|f| !f.from_common && is_dropped_internal(f))
            .map(|f| f.name.as_str())
            .collect();

        writeln!(
            out,
            "/// `recordtype({})` — {} CA-visible own fields from `dbd/{}` \
             ({} `DBF_NOACCESS` internals dropped).",
            rec.name,
            own.len(),
            rec.source,
            internal.len()
        )
        .unwrap();
        writeln!(out, "pub static {konst}: &[FieldDesc] = &[").unwrap();
        for f in &own {
            out.push_str(&emit_field(
                &rec.name,
                f,
                &menu_paths,
                input.cvt,
                input.dbd_dir,
            )?);
        }
        out.push_str("];\n\n");
        record_consts.push((rec.name.clone(), konst.clone()));

        // The dropped internals keep their NAMES: C's `dbNameToAddr`
        // resolves a `DBF_NOACCESS` field, so a SEARCH for it is answered
        // and the refusal lands at channel creation. The search gate
        // consults this list to answer the same way.
        // Emitted for every record type, empty included: whether a type has a
        // dropped internal depends on what its `extra()` declarations name, so
        // a const that comes and goes with that breaks downstream code the day
        // one of those declarations becomes carryable.
        let na_konst = format!("{}_NOACCESS", konst.trim_end_matches("_FIELDS"));
        writeln!(
            out,
            "/// `recordtype({})` — the `DBF_NOACCESS` internal names dropped from\n\
             /// [`{konst}`]: resolvable (a SEARCH is answered), never servable.",
            rec.name
        )
        .unwrap();
        writeln!(out, "pub static {na_konst}: &[&str] = &[").unwrap();
        for name in &internal {
            writeln!(out, "    {},", rust_str(name)).unwrap();
        }
        out.push_str("];\n\n");
        noaccess_consts.push((rec.name.clone(), na_konst));

        // C's `papFldDes` — EVERY declaration in `.dbd` order with dbCommon
        // spliced at the `include`, internals included. It is a different list
        // from `{konst}` in a different order, and it is the one C indexes:
        // `dbRecordtypeBody` (`dbLexRoutines.c:745-750`) numbers `indRecordType`
        // over it, and `link_ind`, `sortFldInd` and `indvalFlddes` are all
        // indices into it. Names only — `offset` and the non-`DBF_STRING`
        // `size` are `offsetof`/`sizeof` of the generated C struct, which this
        // port has no counterpart for — except on a carried `DBF_NOACCESS`
        // row, whose width comes from the C type its `extra()` names.
        let decl_konst = format!("{}_DECL", konst.trim_end_matches("_FIELDS"));
        writeln!(
            out,
            "/// `recordtype({})` — C's `papFldDes` order, all {} declarations\n\
             /// including the internals [`{konst}`] drops. The index into this is\n\
             /// C's `indRecordType`.",
            rec.name,
            rec.fields.len()
        )
        .unwrap();
        writeln!(out, "pub static {decl_konst}: &[&str] = &[").unwrap();
        for f in &rec.fields {
            writeln!(out, "    {},", rust_str(&f.name)).unwrap();
        }
        out.push_str("];\n\n");
        decl_consts.push((rec.name.clone(), decl_konst));
        // `dbRecordtypeBody:752` counts `promptgroup`, not `prompt`.
        no_prompt.push((
            rec.name.clone(),
            rec.fields
                .iter()
                .filter(|f| f.promptgroup.is_some())
                .count(),
        ));
    }

    // Every `cvt_dbaddr.types` row must name a field that really is
    // `special(SPC_DBADDR)`, so the file cannot rot into a list of stale
    // overrides silently shadowing the `.dbd`.
    let declared: BTreeSet<(String, String)> = input
        .records
        .iter()
        .flat_map(|r| {
            r.fields
                .iter()
                .filter(|f| f.special.as_deref() == Some("SPC_DBADDR"))
                .map(move |f| (r.name.clone(), f.name.clone()))
        })
        .collect();
    for key in input.cvt.keys() {
        if !declared.contains(key) {
            return Err(format!(
                "cvt_dbaddr.types lists {}.{}, which no vendored .dbd declares as \
                 special(SPC_DBADDR). Remove the stale row.",
                key.0, key.1
            ));
        }
    }

    // --- dbCommon -------------------------------------------------------
    //
    // Emitted by the base target only: `dbCommon` has ONE declaration in the
    // port and it is base's `dbd/dbCommon.dbd`. A downstream `.dbd` that says
    // `include "dbCommon.dbd"` is served that one — its fields are marked
    // `from_common` and dropped from its record tables, exactly as base's are.
    let common_own: Vec<&Field> = input
        .common
        .iter()
        .filter(|f| !is_dropped_internal(f))
        .collect();
    if !common_own.is_empty() {
        writeln!(
            out,
            "/// `dbCommon.dbd` — the {} CA-visible fields every record type carries.\n\
             ///\n\
             /// The port serves these from\n\
             /// [`CommonFields`](crate::server::record::CommonFields), not from a\n\
             /// record's [`FieldDesc`] table; this table is the spec side of that\n\
             /// reconciliation (`db_common_matches_common_fields`).",
            common_own.len()
        )
        .unwrap();
        out.push_str("pub static DB_COMMON_FIELDS: &[FieldDesc] = &[\n");
        for f in &common_own {
            out.push_str(&emit_field(
                "dbCommon",
                f,
                &menu_paths,
                input.cvt,
                input.dbd_dir,
            )?);
        }
        out.push_str("];\n\n");

        let common_internal: Vec<&str> = input
            .common
            .iter()
            .filter(|f| is_dropped_internal(f))
            .map(|f| f.name.as_str())
            .collect();
        if !common_internal.is_empty() {
            out.push_str(
                "/// The `DBF_NOACCESS` internals of `dbCommon.dbd`, dropped from\n\
                 /// [`DB_COMMON_FIELDS`]: resolvable (a SEARCH is answered), never\n\
                 /// servable — see `record_noaccess_fields` for the record-own twins.\n",
            );
            out.push_str("pub static DB_COMMON_NOACCESS: &[&str] = &[\n");
            for name in &common_internal {
                writeln!(out, "    {},", rust_str(name)).unwrap();
            }
            out.push_str("];\n\n");
        }
    }

    if !input.devices.is_empty() {
        emit_devices(&mut out, input)?;
    }

    if !input.order.is_empty() {
        out.push_str(
            "/// Every record type this port declares, in DBD **load** order — the order\n\
             /// C's `pdbbase->recordTypeList` holds.\n\
             ///\n\
             /// `buildScanLists` (`dbScan.c:1054-1076`) feeds `scanAdd` record-type-major:\n\
             /// the outer loop walks `recordTypeList`, the inner one walks that type's\n\
             /// instances in `.db` order. So this table, not `.db` declaration order alone,\n\
             /// is what decides which of two same-`PHAS` records scans first.\n\
             ///\n\
             /// Base's own types come from `stdRecords.dbd`, vendored beside the `.dbd`\n\
             /// files it names; every other type this port vendors sorts after all of them,\n\
             /// where a C IOC also puts a module `.dbd` included after `base.dbd`.\n\
             pub static RECORD_TYPE_ORDER: &[&str] = &[\n",
        );
        for name in input.order {
            let _ = writeln!(out, "    {:?},", name);
        }
        out.push_str("];\n\n");
    }

    // --- registry -------------------------------------------------------
    out.push_str(
        "/// The record-own field table for a record type name, or `None` for a type\n\
         /// no vendored `.dbd` declares.\n\
         pub fn record_fields(record_type: &str) -> Option<&'static [FieldDesc]> {\n",
    );
    emit_str_lookup(
        &mut out,
        "record_type",
        record_consts.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    );
    out.push_str("}\n\n");

    out.push_str(
        "/// The record-own `DBF_NOACCESS` internal names for a record type — fields\n\
         /// C's `dbNameToAddr` resolves (a SEARCH is answered) but whose channel is\n\
         /// refused at creation (`mapDBFToDBR` -> `DBR_NOACCESS`). Empty for a type\n\
         /// declaring none, `None` for a type no vendored `.dbd` declares.\n\
         pub fn record_noaccess_fields(record_type: &str) -> Option<&'static [&'static str]> {\n",
    );
    emit_str_lookup(
        &mut out,
        "record_type",
        noaccess_consts
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    out.push_str("}\n\n");

    out.push_str(
        "/// Every field a record type declares, in C's `papFldDes` order — the order\n\
         /// `dbDumpRecordType` indexes and `link_ind`/`sortFldInd`/`indvalFlddes`\n\
         /// point into. Unlike `record_fields` this keeps the `DBF_NOACCESS`\n\
         /// internals and splices dbCommon where the `.dbd` puts its `include`, so\n\
         /// the index of a name here is C's own `indRecordType` for it.\n\
         pub fn record_declaration_order(record_type: &str) -> Option<&'static [&'static str]> {\n",
    );
    emit_str_lookup(
        &mut out,
        "record_type",
        decl_consts.iter().map(|(k, v)| (k.as_str(), v.as_str())),
    );
    out.push_str("}\n\n");

    out.push_str(
        "/// C's `pdbRecordType->no_prompt`: how many of `record_declaration_order`'s\n\
         /// declarations carry a `promptgroup(...)` (`dbLexRoutines.c:752`). Counted\n\
         /// here because the port drops `promptgroup` at emit, so it cannot be\n\
         /// recovered from the field tables.\n\
         pub fn record_no_prompt(record_type: &str) -> Option<u16> {\n",
    );
    if no_prompt.is_empty() {
        out.push_str("    let _ = record_type;\n    None\n");
    } else {
        out.push_str("    Some(match record_type {\n");
        for (name, n) in &no_prompt {
            writeln!(out, "        {} => {n},", rust_str(name)).unwrap();
        }
        out.push_str("        _ => return None,\n    })\n");
    }
    out.push_str("}\n\n");

    writeln!(
        out,
        "/// Every record type a vendored `.dbd` declares, in `.dbd` name order.\n\
         pub static RECORD_TYPES: &[&str] = &["
    )
    .unwrap();
    for (name, _) in &record_consts {
        writeln!(out, "    {},", rust_str(name)).unwrap();
    }
    out.push_str("];\n");

    Ok(out)
}

/// Emit the body of a `fn(&str) -> Option<&'static [T]>` string-keyed lookup:
/// `Some(match key { "name" => KONST, ..., _ => return None })`.
///
/// When there are NO arms (a target that declares no menus, or no record types —
/// asyn declares only `device()` lines), that shape is `Some(match { _ => return
/// None })`, whose `Some(...)` is unreachable because the match diverges — a hard
/// `unreachable_code` error under `-D warnings`, which `#![allow(clippy::all)]`
/// does not cover (it is a rustc lint, not clippy's). The empty case emits the
/// constant `None` answer instead. The non-empty shape is unchanged, so every
/// existing generated table is byte-identical after rustfmt.
fn emit_str_lookup<'a>(
    out: &mut String,
    key: &str,
    arms: impl IntoIterator<Item = (&'a str, &'a str)>,
) {
    let arms: Vec<(&str, &str)> = arms.into_iter().collect();
    if arms.is_empty() {
        writeln!(out, "    let _ = {key};").unwrap();
        out.push_str("    None\n");
        return;
    }
    writeln!(out, "    Some(match {key} {{").unwrap();
    for (name, konst) in arms {
        writeln!(out, "        {} => {konst},", rust_str(name)).unwrap();
    }
    out.push_str("        _ => return None,\n    })\n");
}

/// The `device(...)` half of a target's `.dbd` set: the DTYP choice menu and the
/// link type each choice demands. Emitted only for a target that declares device
/// support in a vendored `.dbd` — the downstream crates register theirs at
/// runtime and bring no `device()` line, so there is nothing to declare and none
/// is invented.
fn emit_devices(out: &mut String, input: &Input<'_>) -> Result<(), String> {
    // --- device menus ---------------------------------------------------
    //
    // C `dbDeviceMenu`: the ordered list of `device()` declarations for a record
    // type. `DTYP` is an INDEX into it — an unset DTYP is index 0, the first
    // device the loaded `.dbd` declares for that type, which is why a C IOC
    // serves `caget -t REC.DTYP` as "Soft Channel" for a bare `record(ai,"X"){}`
    // and as the empty string for a record type that declares no device at all
    // (`dbConvert.c::getDeviceString` fails the get, leaving the caller's buffer
    // zeroed).
    out.push_str(
        "// ---------------------------------------------------------------------\n\
         // Per-record-type device menus (C `dbDeviceMenu`), in `.dbd` declaration\n\
         // order. The index is wire-visible: it IS the value of the DTYP field.\n\
         // ---------------------------------------------------------------------\n\n",
    );
    let mut device_menus: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for d in input.devices {
        let choices = device_menus.entry(d.record.as_str()).or_default();
        // C `dbDeviceMenu` holds one entry per device() line; a duplicate choice
        // string for the same record type is a `.dbd` error there and would make
        // the index ambiguous here.
        if choices.contains(&d.name.as_str()) {
            return Err(format!(
                "device({}, ..., {:?}) is declared twice",
                d.record, d.name
            ));
        }
        choices.push(d.name.as_str());
    }
    let mut device_consts: Vec<(&str, String)> = Vec::new();
    for (record, choices) in &device_menus {
        let konst = format!("DEVICE_MENU_{}", record.to_uppercase().replace('-', "_"));
        writeln!(
            out,
            "/// `device({record}, ...)` choices, in declaration order."
        )
        .unwrap();
        writeln!(out, "pub static {konst}: &[&str] = &[").unwrap();
        for c in choices {
            writeln!(out, "    {},", rust_str(c)).unwrap();
        }
        out.push_str("];\n\n");
        device_consts.push((record, konst));
    }
    out.push_str(
        "/// The device menu for a record type — the choices its `DTYP` field selects\n\
         /// among, in `.dbd` declaration order. `None` for a record type that declares\n\
         /// no device support: C has no `dbDeviceMenu` for it, and its DTYP renders as\n\
         /// the empty string.\n\
         pub fn device_menu(record_type: &str) -> Option<&'static [&'static str]> {\n\
         \x20   Some(match record_type {\n",
    );
    for (record, konst) in &device_consts {
        writeln!(out, "        {} => {konst},", rust_str(record)).unwrap();
    }
    out.push_str("        _ => return None,\n    })\n}\n\n");

    // The record types [`device_menu`] answers `Some` for — its keys, as a
    // list. A downstream crate whose device support has no `device()` line in
    // base's `.dbd` (asyn) walks this to hand each menu to base's
    // `register_device_menu` at startup, so base can merge the asyn choices into
    // the record type's `DTYP` list without depending on the downstream crate.
    writeln!(
        out,
        "/// The record types [`device_menu`] returns `Some` for, in `.dbd` name order."
    )
    .unwrap();
    out.push_str("pub static DEVICE_MENU_RECORD_TYPES: &[&str] = &[\n");
    for (record, _) in &device_consts {
        writeln!(out, "    {},", rust_str(record)).unwrap();
    }
    out.push_str("];\n\n");

    // --- device link types ----------------------------------------------
    //
    // The `link_type` argument of each `device()` line — C `devSup::link_type`.
    // `dbCanSetLink` (`dbStaticLib.c:2400-2419`) refuses to install a link whose
    // parsed type is incompatible with the device's, which is how C rejects a
    // `.db` that hands an `INST_IO` device a constant `INP`. The port dropped
    // this argument at the parser and so accepted that `.db`.
    out.push_str(
        "// ---------------------------------------------------------------------\n\
         // Per-(record type, DTYP) device LINK TYPE (C `devSup::link_type`), the\n\
         // half of the `device()` declaration `dbCanSetLink` enforces.\n\
         // ---------------------------------------------------------------------\n\n",
    );
    let mut link_consts: Vec<(&str, String)> = Vec::new();
    let mut by_record: BTreeMap<&str, Vec<&Device>> = BTreeMap::new();
    for d in input.devices {
        by_record.entry(d.record.as_str()).or_default().push(d);
    }
    for (record, devs) in &by_record {
        let konst = format!(
            "DEVICE_LINK_TYPES_{}",
            record.to_uppercase().replace('-', "_")
        );
        writeln!(
            out,
            "/// `device({record}, LINK_TYPE, ...)` — the link type each DTYP choice demands."
        )
        .unwrap();
        writeln!(out, "pub static {konst}: &[(&str, DbLinkType)] = &[").unwrap();
        for d in devs {
            let variant = link_type_variant(&d.link_type)?;
            writeln!(out, "    ({}, DbLinkType::{variant}),", rust_str(&d.name)).unwrap();
        }
        out.push_str("];\n\n");
        link_consts.push((record, konst));
    }
    out.push_str(
        "/// The link type the named `DTYP` of `record_type` demands of its `INP`/`OUT`.\n\
         ///\n\
         /// `None` when no vendored `.dbd` declares that `device()` — a downstream\n\
         /// crate registering its device support at runtime (asyn, motor) brings no\n\
         /// `.dbd`, so there is no declaration to enforce and none is invented.\n\
         pub fn device_link_type(record_type: &str, dtyp: &str) -> Option<DbLinkType> {\n\
         \x20   let table = match record_type {\n",
    );
    for (record, konst) in &link_consts {
        writeln!(out, "        {} => {konst},", rust_str(record)).unwrap();
    }
    out.push_str(
        "        _ => return None,\n\
         \x20   };\n\
         \x20   table\n\
         \x20       .iter()\n\
         \x20       .find(|(name, _)| name.eq_ignore_ascii_case(dtyp))\n\
         \x20       .map(|(_, lt)| *lt)\n\
         }\n\n",
    );

    Ok(())
}

fn emit_field(
    record: &str,
    f: &Field,
    menus: &BTreeMap<String, String>,
    cvt: &BTreeMap<(String, String), CvtDbAddr>,
    dbd_dir: &str,
) -> Result<String, String> {
    let (dbf, spc) = field_dbf(record, f, cvt, dbd_dir)?;
    // A kept `DBF_NOACCESS` internal has no served type — its channel is
    // refused at creation — and every reader C allows treats it as `size`
    // raw bytes (`dbTest.c:1249-1262`), so that is the type it carries.
    let kept_internal = internal_width(f);
    let ty = match kept_internal {
        Some(_) => "UChar",
        None => dbf_to_rust(dbf).ok_or_else(|| format!("{record}.{}: unknown {dbf}", f.name))?,
    };
    let variant = |token: &str| -> Result<String, String> {
        Ok(if token.is_empty() {
            "Special::None".to_string()
        } else {
            format!("Special::{}", special_variant(token)?)
        })
    };
    let special = variant(spc)?;
    // `field_dbf` swapped in the `cvt_dbaddr` code for an SPC_DBADDR field, so
    // `spc` above is what C's resolved `DBADDR` carries. The `.dbd`'s own token
    // is a SECOND fact — it is what `pdbFldDes->special` keeps, and what every
    // reader of the declaration (the loader's field suggestion) must see.
    let declared_special = variant(f.special.as_deref().unwrap_or(""))?;
    // C's `.dbd` default is ASL1 (`dbLexRoutines.c:570`); `asl(ASL0)` lowers it.
    let asl = match f.asl.as_deref() {
        Some("ASL0") => "Asl::Asl0",
        Some("ASL1") | None => "Asl::Asl1",
        Some(other) => return Err(format!("{}: unknown asl({other})", f.name)),
    };
    let menu = match &f.menu {
        // `menuScan` is the one menu that is NOT a compile-time table. C reads
        // it out of `pdbbase` at `iocInit` (`initPeriodic`, `dbScan.c:858`)
        // because a site may ship its own `menuScan.dbd` with other rates, so
        // a field declared `menu(menuScan)` — `SSCN`, on the 21 records that
        // carry it — must resolve through the LOADED menu. Emitting `None`
        // sends it to `shared_menu_choices`, which is where that lives; a
        // `Some(MENU_SCAN)` here would pin the field to base's own rates and
        // serve a site IOC labels its menu does not have.
        Some(m) if m == "menuScan" => "None".to_string(),
        Some(m) => {
            let konst = menus.get(m).ok_or_else(|| {
                format!("{}: menu({m}) is not declared by any vendored .dbd", f.name)
            })?;
            format!("Some({konst})")
        }
        None => "None".to_string(),
    };
    let initial = opt_str(&f.initial);

    let mut s = String::new();
    // An SPC_DBADDR field's type is not in the `.dbd` — say where it came from,
    // and which field re-selects it at runtime, right at the entry.
    //
    // A row WITH a selector is the one kind of field whose declared type is not
    // what it is served as: C's `cvt_dbaddr` overwrites `paddr->field_type` from
    // the selector's live value, so `dbf_type` below is only the selector's
    // default. `runtime_typed` carries that to the runtime, which then serves
    // the record's stored variant instead of the declaration. A row WITHOUT a
    // selector has a fixed type that the declaration states correctly.
    let entry = cvt.get(&(record.to_string(), f.name.clone()));
    if let Some(c) = entry {
        match &c.selector {
            Some(sel) => writeln!(
                s,
                "    // {dbf} from cvt_dbaddr.types; re-typed at runtime by `{sel}`.",
            )
            .unwrap(),
            None => writeln!(s, "    // {dbf} from cvt_dbaddr.types.").unwrap(),
        }
    }
    let runtime_typed = entry.is_some_and(|c| c.selector.is_some());
    // The RAW declaration, before `field_dbf` swapped in the cvt_dbaddr type —
    // C's `pdbFldDes->field_type`, which `dbDumpField` prints and which
    // `dbPutString` refuses a `.db` assignment on (`dbStaticLib.c:2646-2650`).
    // It is a SECOND fact, not a spelling of `dbf_type`: `dbf_to_rust` above
    // collapses six tokens onto two served types and the swap loses the rest,
    // so after either one the row can no longer say what it was declared.
    let declared_dbf = dbf_to_code(&f.dbf).map_err(|e| format!("{}: {e}", f.name))?;
    // `read_only` is `special(SPC_NOMOD)` — its own bit because it is the one
    // attribute the runtime put gate reads on every write.
    let read_only = spc == "SPC_NOMOD";
    writeln!(s, "    FieldDesc {{").unwrap();
    writeln!(s, "        name: {},", rust_str(&f.name)).unwrap();
    writeln!(s, "        dbf_type: DbFieldType::{ty},").unwrap();
    writeln!(s, "        declared_dbf: DbfCode::{declared_dbf},").unwrap();
    writeln!(s, "        runtime_typed: {runtime_typed},").unwrap();
    writeln!(s, "        read_only: {read_only},").unwrap();
    writeln!(s, "        special: {special},").unwrap();
    writeln!(s, "        declared_special: {declared_special},").unwrap();
    writeln!(s, "        pp: {},", f.pp).unwrap();
    writeln!(s, "        asl: {asl},").unwrap();
    writeln!(
        s,
        "        size: {},",
        kept_internal.unwrap_or_else(|| f.size.unwrap_or(0) as u16)
    )
    .unwrap();
    // C's loader requires `extra` on every DBF_NOACCESS row and `size` on
    // every DBF_STRING one (`dbLexRoutines.c:755-762`); both are carried
    // because `dbpr`'s DBF_NOACCESS arm reads the declaration TEXT to choose
    // a renderer (`dbTest.c:1235-1247`).
    writeln!(s, "        extra: {},", opt_str(&f.extra)).unwrap();
    writeln!(s, "        menu: {menu},").unwrap();
    writeln!(s, "        initial: {initial},").unwrap();
    writeln!(s, "        interest: {},", f.interest.unwrap_or(0)).unwrap();
    writeln!(s, "        prop: {},", f.prop).unwrap();
    // `prompt` is the parenthesised half of the db loader's field suggestion
    // and `promptgroup` one of its weights (`dbLexRoutines.c:1288`, `:1382`),
    // so a table without them cannot reproduce that line. The parser has
    // always read both; only the emitter dropped them.
    writeln!(s, "        prompt: {},", opt_str(&f.prompt)).unwrap();
    writeln!(s, "        promptgroup: {},", opt_str(&f.promptgroup)).unwrap();
    // `base(HEX)` picks `ulongToHexString` over the decimal converters inside
    // `dbGetStringNum` (`dbStaticLib.c:2074-2124`), so a table without it
    // makes `dbpr` print `ZRVL: 10` where C prints `ZRVL: 0xa`.
    writeln!(
        s,
        "        base: Base::{},",
        if f.base_hex { "Hex" } else { "Decimal" }
    )
    .unwrap();
    writeln!(s, "    }},").unwrap();
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_const_names_follow_the_epics_convention() {
        assert_eq!(menu_const("menuSimm"), "MENU_SIMM");
        assert_eq!(menu_const("menuAlarmSevr"), "MENU_ALARM_SEVR");
        assert_eq!(menu_const("calcoutOOPT"), "MENU_CALCOUT_OOPT");
        assert_eq!(menu_const("aaiPOST"), "MENU_AAI_POST");
        assert_eq!(menu_const("bufferingALG"), "MENU_BUFFERING_ALG");
    }

    #[test]
    fn field_table_const_names_split_camel_case() {
        assert_eq!(fields_const("ai"), "AI_FIELDS");
        assert_eq!(fields_const("mbbiDirect"), "MBBI_DIRECT_FIELDS");
        assert_eq!(fields_const("aSub"), "A_SUB_FIELDS");
        assert_eq!(fields_const("int64in"), "INT64IN_FIELDS");
    }

    /// The generator emits the *declared* DBF type and leaves the CA promotion
    /// to `DbFieldType::ca_wire_type`. Folding the promotion in here would make
    /// `ROFF` a `Double` field — losing the `uint32` PVA serves natively and the
    /// unsigned range a `dbPut` must accept.
    #[test]
    fn dbf_map_is_the_declared_type_not_the_ca_promotion() {
        assert_eq!(dbf_to_rust("DBF_ULONG"), Some("ULong"));
        assert_eq!(dbf_to_rust("DBF_USHORT"), Some("UShort"));
        assert_eq!(dbf_to_rust("DBF_INT64"), Some("Int64"));
        // A menu IS served as DBR_ENUM, and that is a declaration-level fact
        // (`mapDBFToDBR`), not a wire promotion — so it is folded in.
        assert_eq!(dbf_to_rust("DBF_MENU"), Some("Enum"));
        assert_eq!(dbf_to_rust("DBF_DEVICE"), Some("Enum"));
        assert_eq!(dbf_to_rust("DBF_INLINK"), Some("String"));
        assert_eq!(dbf_to_rust("DBF_NOACCESS"), None);
    }
}
