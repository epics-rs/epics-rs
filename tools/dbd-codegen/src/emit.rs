//! Emit the parsed `.dbd` declarations as a checked-in Rust source file.
//!
//! The one place the `DBF_*` -> [`DbFieldType`] mapping is decided is
//! [`dbf_to_rust`]. It emits the *field* type the `.dbd` declares — the
//! promotion a CA client observes (`DBF_ULONG` -> `DBR_DOUBLE`,
//! `DBF_USHORT` -> `DBR_LONG`, ...) is not folded in here, because
//! `DbFieldType::ca_wire_type` already owns it. Folding it in twice would
//! double-promote and lose the native width PVA serves.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::parse::{Device, Field, Menu, RecordType};

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

/// What C's `cvt_dbaddr` makes of one `special(SPC_DBADDR)` field —
/// `dbd/cvt_dbaddr.types`.
pub struct CvtDbAddr {
    pub dbf: String,
    /// The `special` C leaves on the DBADDR. Usually still `SPC_DBADDR`, but
    /// `lsi`/`lso`/`asyn` raise `SPC_NOMOD` (making the field unwritable over
    /// CA — `rsrv/camessage.c:2545`) or `SPC_MOD` inside `cvt_dbaddr` itself.
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

pub struct Input<'a> {
    pub menus: &'a BTreeMap<String, Menu>,
    pub devices: &'a [Device],
    pub records: &'a [RecordType],
    pub common: &'a [Field],
    pub cvt: &'a BTreeMap<(String, String), CvtDbAddr>,
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
                 crates/epics-base-rs/dbd/cvt_dbaddr.types citing the C source.",
                f.name
            )
        })
}

pub fn emit(input: &Input<'_>) -> Result<String, String> {
    let mut out = String::new();

    out.push_str(
        "//! Record field tables generated from the vendored EPICS `.dbd` record\n\
         //! declarations — the machine-readable spec every `FieldDesc` used to be\n\
         //! hand-copied from.\n\
         //!\n\
         //! @generated by `tools/dbd-codegen` from `crates/epics-base-rs/dbd/*.dbd`.\n\
         //! DO NOT EDIT. Re-run `cargo run -p dbd-codegen -- --write` after changing a\n\
         //! vendored `.dbd`. Drift is caught by `dbd_codegen::tests::generated_file_is_not_stale`,\n\
         //! which `cargo nextest run` executes — locally and in CI — so a stale table\n\
         //! is a failing test, not a comment asking you to remember.\n\
         //!\n\
         //! What is *not* here, and why:\n\
         //!\n\
         //! * **`dbCommon` fields.** The port models them once in\n\
         //!   [`CommonFields`](crate::server::record::CommonFields), so splicing them\n\
         //!   into every record table would double-declare them. They are emitted\n\
         //!   separately as [`DB_COMMON_FIELDS`] so the two models can be reconciled\n\
         //!   (and so a `dbCommon` addition is a diff, not a silent gap).\n\
         //! * **`DBF_NOACCESS` fields** (`RSET`, `DPVT`, `MLOK`, `BPTR`, `PPN`, ...).\n\
         //!   These are C-internal pointers with no CA/PVA representation — C's\n\
         //!   `mapDBFToDBR` sends them to `DBR_NOACCESS` and `dbChannelCreate` refuses\n\
         //!   a channel on them. A Rust port has no pointer to expose, so they are\n\
         //!   dropped rather than invented; the count is reported by the generator.\n\
         \n",
    );
    out.push_str("#![allow(clippy::all)]\n\n");
    out.push_str("use super::link::DbLinkType;\n");
    out.push_str("use super::record_trait::{Asl, FieldDesc, Special};\n");
    out.push_str("use crate::types::DbFieldType;\n\n");

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

    writeln!(
        out,
        "/// Every `menu()` declared by a vendored `.dbd`, keyed by its EPICS name.\n\
         ///\n\
         /// This is the table `get_enum_strs` answers a `DBF_MENU` field from: the\n\
         /// field's [`FieldDesc::menu`] already points at its choices, and this map\n\
         /// exists for the by-name lookups (`.db` loading, `dbpr`).\n\
         pub fn menu_choices(menu: &str) -> Option<&'static [&'static str]> {{\n\
         \x20   Some(match menu {{"
    )
    .unwrap();
    for (name, konst) in &menu_names {
        writeln!(out, "        {} => {konst},", rust_str(name)).unwrap();
    }
    out.push_str("        _ => return None,\n    })\n}\n\n");

    // --- field tables ---------------------------------------------------
    out.push_str(
        "// ---------------------------------------------------------------------\n\
         // Per-record-type field tables. Record-*own* fields only: `dbCommon` is\n\
         // modelled separately (see the module docs).\n\
         // ---------------------------------------------------------------------\n\n",
    );

    let mut record_consts: Vec<(String, String)> = Vec::new();
    for rec in input.records {
        let konst = fields_const(&rec.name);
        let own: Vec<&Field> = rec
            .fields
            .iter()
            .filter(|f| !f.from_common && !is_internal(f))
            .collect();
        let dropped = rec
            .fields
            .iter()
            .filter(|f| !f.from_common && is_internal(f))
            .count();

        writeln!(
            out,
            "/// `recordtype({})` — {} CA-visible own fields from `dbd/{}` \
             ({dropped} `DBF_NOACCESS` internals dropped).",
            rec.name,
            own.len(),
            rec.source
        )
        .unwrap();
        writeln!(out, "pub static {konst}: &[FieldDesc] = &[").unwrap();
        for f in &own {
            out.push_str(&emit_field(&rec.name, f, &menu_names, input.cvt)?);
        }
        out.push_str("];\n\n");
        record_consts.push((rec.name.clone(), konst));
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
    let common_own: Vec<&Field> = input.common.iter().filter(|f| !is_internal(f)).collect();
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
        out.push_str(&emit_field("dbCommon", f, &menu_names, input.cvt)?);
    }
    out.push_str("];\n\n");

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

    // --- registry -------------------------------------------------------
    out.push_str(
        "/// The record-own field table for a record type name, or `None` for a type\n\
         /// no vendored `.dbd` declares.\n\
         pub fn record_fields(record_type: &str) -> Option<&'static [FieldDesc]> {\n\
         \x20   Some(match record_type {\n",
    );
    for (name, konst) in &record_consts {
        writeln!(out, "        {} => {konst},", rust_str(name)).unwrap();
    }
    out.push_str("        _ => return None,\n    })\n}\n\n");

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

fn emit_field(
    record: &str,
    f: &Field,
    menus: &BTreeMap<String, String>,
    cvt: &BTreeMap<(String, String), CvtDbAddr>,
) -> Result<String, String> {
    let (dbf, spc) = field_dbf(record, f, cvt)?;
    let ty = dbf_to_rust(dbf).ok_or_else(|| format!("{record}.{}: unknown {dbf}", f.name))?;
    let special = if spc.is_empty() {
        "Special::None".to_string()
    } else {
        format!("Special::{}", special_variant(spc)?)
    };
    // C's `.dbd` default is ASL1 (`dbLexRoutines.c:570`); `asl(ASL0)` lowers it.
    let asl = match f.asl.as_deref() {
        Some("ASL0") => "Asl::Asl0",
        Some("ASL1") | None => "Asl::Asl1",
        Some(other) => return Err(format!("{}: unknown asl({other})", f.name)),
    };
    let menu = match &f.menu {
        Some(m) => {
            let konst = menus.get(m).ok_or_else(|| {
                format!("{}: menu({m}) is not declared by any vendored .dbd", f.name)
            })?;
            format!("Some({konst})")
        }
        None => "None".to_string(),
    };
    let initial = match &f.initial {
        Some(i) => format!("Some({})", rust_str(i)),
        None => "None".to_string(),
    };

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
    // `read_only` is `special(SPC_NOMOD)` — its own bit because it is the one
    // attribute the runtime put gate reads on every write.
    let read_only = spc == "SPC_NOMOD";
    writeln!(s, "    FieldDesc {{").unwrap();
    writeln!(s, "        name: {},", rust_str(&f.name)).unwrap();
    writeln!(s, "        dbf_type: DbFieldType::{ty},").unwrap();
    writeln!(s, "        runtime_typed: {runtime_typed},").unwrap();
    writeln!(s, "        read_only: {read_only},").unwrap();
    writeln!(s, "        special: {special},").unwrap();
    writeln!(s, "        pp: {},", f.pp).unwrap();
    writeln!(s, "        asl: {asl},").unwrap();
    writeln!(s, "        size: {},", f.size.unwrap_or(0)).unwrap();
    writeln!(s, "        menu: {menu},").unwrap();
    writeln!(s, "        initial: {initial},").unwrap();
    writeln!(s, "        interest: {},", f.interest.unwrap_or(0)).unwrap();
    writeln!(s, "        prop: {},", f.prop).unwrap();
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
