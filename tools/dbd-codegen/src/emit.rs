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
///   ...). It is not CA-visible and has no served type; the caller carries it
///   as raw bytes ([`internal_width`]) or drops it to a name.
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

/// The width of the C struct member an `extra("...")` declaration names.
#[derive(Clone, Copy)]
pub enum Width {
    Bytes(u16),
    /// `sizeof(void *)` of the target, which no `.dbd` states and which a
    /// 32-bit RTEMS build does not share with a 64-bit host.
    Pointer,
}

impl Width {
    /// The `FieldDesc::size` initializer.
    fn tokens(self) -> String {
        match self {
            Width::Bytes(n) => n.to_string(),
            Width::Pointer => "core::mem::size_of::<*const ()>() as u16".to_string(),
        }
    }
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
/// width the type alone gives. A `*` declarator without a `[` is a pointer —
/// including a function pointer, `long (*cadr)(...)` — and its width is
/// `sizeof(void *)`, which is what `dbpr`'s `PTR %p` arm tests
/// (`dbTest.c:1239`). An array (`[`) is refused because its width is the
/// bound, not the element — `calc.RPCL` is
/// `extra("char rpcl[INFIX_TO_POSTFIX_SIZE(160)]")`, whose size is that C
/// macro expanded, and reading the leading `char` would carry the row at one
/// byte and print a one-byte `dbpr` line where C prints the whole buffer.
fn extra_width(extra: &str) -> Option<Width> {
    if extra.contains('[') {
        return None;
    }
    if extra.contains('*') {
        return Some(Width::Pointer);
    }
    match extra.split_whitespace().next()? {
        "epicsInt8" | "epicsUInt8" | "char" => Some(Width::Bytes(1)),
        "epicsInt16" | "epicsUInt16" | "short" => Some(Width::Bytes(2)),
        "epicsInt32" | "epicsUInt32" | "epicsFloat32" => Some(Width::Bytes(4)),
        "epicsInt64" | "epicsUInt64" | "epicsFloat64" => Some(Width::Bytes(8)),
        // `epicsTimeStamp` is two `epicsUInt32` (`epicsTime.h`), which is
        // what `dbCommon.TIME` stands for.
        "epicsTimeStamp" => Some(Width::Bytes(8)),
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
pub fn internal_width(f: &Field) -> Option<Width> {
    if !is_internal(f) {
        return None;
    }
    extra_width(f.extra.as_deref()?)
}

/// The width of any `DBF_NOACCESS` declaration, `SPC_DBADDR` rows included.
///
/// `FieldDesc::size` is `sizeof(prec->member)` whatever the row's `special`:
/// `dbpr` tests it (`dbTest.c:1239`) for the rows `cvt_dbaddr` re-types, such
/// as `waveform.VAL` — `void *val` — whose descriptor must keep the pointer
/// width or the row prints as nothing.
fn declared_width(f: &Field) -> Option<Width> {
    if f.dbf != "DBF_NOACCESS" {
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
    /// The field that sets C's `field_size` (`SIZV`): a long string.
    pub size: Option<String>,
}

/// Parse `dbd/cvt_dbaddr.types`:
/// `record.FIELD  DBF_TYPE  SPC_xxx  selector|-  size|-  cite`.
pub fn parse_cvt_dbaddr(src: &str) -> Result<BTreeMap<(String, String), CvtDbAddr>, String> {
    let mut out = BTreeMap::new();
    for (n, line) in src.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(key), Some(dbf), Some(spc), Some(sel), Some(size)) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            return Err(format!(
                "cvt_dbaddr.types:{}: expected \
                 `record.FIELD DBF_TYPE SPC_xxx selector size`, got `{line}`",
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
        if size != "-" && dbf != "DBF_STRING" {
            return Err(format!("cvt_dbaddr.types:{}: a size on a {dbf} row", n + 1));
        }
        let prev = out.insert(
            (rec.to_string(), field.to_string()),
            CvtDbAddr {
                dbf: dbf.to_string(),
                special: spc.to_string(),
                selector: (sel != "-").then(|| sel.to_string()),
                size: (size != "-").then(|| size.to_string()),
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
    /// The file this module is written to, for the header.
    pub out_file: &'a str,
    /// How this module names `epics-base-rs` — `crate` when it is emitted into
    /// base, `epics_base_rs` when into a downstream crate.
    pub base_path: &'a str,
    /// Menus a field may reference that this target's `.dbd`s do not declare:
    /// the EPICS Base menus (`menuYesNo`, `menuOmsl`, `menuAlarmSevr`, ...),
    /// mapped to base's generated const path. Empty for the base target, which
    /// declares every menu it references.
    pub external_menus: &'a BTreeMap<String, String>,
    /// Record type names in `recordTypeList` order. Emitted only by the base
    /// target; empty everywhere else.
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

/// The header's "how to regenerate" lines. An out-of-workspace table is not
/// walked by this workspace's drift test, so it names its own `--check`.
fn regen_note(input: &Input<'_>) -> String {
    if input.base_path == crate::targets::EXTERNAL_BASE_PATH {
        let out = input.out_file;
        format!(
            "//! DO NOT EDIT. Re-run `dbd-codegen --write --external {dbd} {out}` from the\n\
             //! repository root after changing a vendored `.dbd`; `--check` in place of\n\
             //! `--write` reports drift.\n",
            dbd = input.dbd_dir
        )
    } else {
        "//! DO NOT EDIT. Re-run `cargo run -p dbd-codegen -- --write` after changing a\n\
         //! vendored `.dbd`. Drift is caught by `dbd_codegen::tests::generated_files_are_not_stale`,\n\
         //! which `cargo nextest run` executes — locally and in CI — so a stale table\n\
         //! is a failing test, not a comment asking you to remember.\n"
            .to_string()
    }
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
         {}\
         //!\n\
         //! What is *not* here, and why:\n\
         //!\n\
         //! * **`dbCommon` fields.** The port models them once in\n\
         //!   [`CommonFields`]({base}::server::record::CommonFields), so splicing them\n\
         //!   into every record table would double-declare them. Base's table emits\n\
         //!   them separately as `DB_COMMON_FIELDS` so the two models can be reconciled\n\
         //!   (and so a `dbCommon` addition is a diff, not a silent gap).\n\
         //! * **`DBF_NOACCESS` rows whose `extra(...)` names an array or a type\n\
         //!   this generator has no width for** (`MLOK`, `MLIS`, `BKLNK`, `RPCL`,\n\
         //!   ...). C keeps `sizeof` the struct member in `pdbFldDes->size`, and\n\
         //!   an array's width is its bound, not its element, so the *descriptor*\n\
         //!   is dropped rather than invented — but the NAME is kept\n\
         //!   (`record_noaccess_fields`): C's `dbNameToAddr` resolves them, so a\n\
         //!   SEARCH for `REC.MLOK` is answered and the refusal lands at channel\n\
         //!   creation, and the search gate needs the names to do the same.\n\
         //!\n\
         //!   A row whose `extra(...)` names a plain scalar — `dbCommon`'s `BKPT`\n\
         //!   and `TIME` — or a pointer — `RSET`, `DPVT`, `BPTR`, `PPN`, ... — is\n\
         //!   CARRIED, descriptor and width both, because `dbpr` prints it and\n\
         //!   `dba` reports its `Field Size` and both read the declaration. A\n\
         //!   pointer's width is `sizeof(void *)` of the build, as C's is.\n\
         //!   Carried is not servable: `FieldDesc::unreadable` is what refuses\n\
         //!   the read, so the SEARCH is answered off the descriptor instead of\n\
         //!   off the name list.\n\
         //!\n\
         //! `slots::<rtype>::<FIELD>` is `FIELD`'s index in `<rtype>`'s field\n\
         //! table here, the one numbering a `FieldSlot` uses. Names are as\n\
         //! declared; a name that is not a Rust identifier has each other\n\
         //! character replaced by `_`, a leading digit prefixed by `_`, and a\n\
         //! keyword suffixed by `_`.\n",
        input.dbd_dir,
        regen_note(input)
    )
    .unwrap();
    out.push_str("#![allow(clippy::all)]\n\n");
    if !input.devices.is_empty() {
        writeln!(out, "use super::dsets;").unwrap();
        writeln!(out, "use {base}::server::record::{{DbLinkType, DevSup}};").unwrap();
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
    let mut dbaddr_consts: Vec<(String, String)> = Vec::new();
    let mut no_prompt: Vec<(String, usize)> = Vec::new();
    let mut slot_mods: Vec<(String, Vec<String>)> = Vec::new();
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
        slot_mods.push((
            rec.name.clone(),
            own.iter().map(|f| f.name.clone()).collect(),
        ));

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

        // The `.dbd`'s own `special(SPC_DBADDR)` rows — `pdbFldDes->special`,
        // which `cvt_dbaddr` cannot overwrite — so a put asks a list of a few
        // names rather than every row of `{konst}`.
        let da_konst = format!("{}_DBADDR", konst.trim_end_matches("_FIELDS"));
        writeln!(
            out,
            "/// `recordtype({})` — the fields its `.dbd` declares `special(SPC_DBADDR)`.",
            rec.name
        )
        .unwrap();
        writeln!(out, "pub static {da_konst}: &[&str] = &[").unwrap();
        for f in own
            .iter()
            .filter(|f| f.special.as_deref() == Some("SPC_DBADDR"))
        {
            writeln!(out, "    {},", rust_str(&f.name)).unwrap();
        }
        out.push_str("];\n\n");
        dbaddr_consts.push((rec.name.clone(), da_konst));

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
            "/// Every record type this port declares, in the order C's\n\
             /// `pdbbase->recordTypeList` holds: name order, because an IOC loads its\n\
             /// expanded `.dbd` and `dbdExpand.pl` writes record types sorted.\n\
             ///\n\
             /// `buildScanLists` (`dbScan.c:1054-1076`) feeds `scanAdd` record-type-major:\n\
             /// the outer loop walks `recordTypeList`, the inner one walks that type's\n\
             /// instances in `.db` order. So this table, not `.db` declaration order alone,\n\
             /// is what decides which of two same-`PHAS` records scans first.\n\
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
        "/// The fields a record type's `.dbd` declares `special(SPC_DBADDR)`, or\n\
         /// `None` for a type no vendored `.dbd` declares.\n\
         pub fn record_dbaddr_fields(record_type: &str) -> Option<&'static [&'static str]> {\n",
    );
    emit_str_lookup(
        &mut out,
        "record_type",
        dbaddr_consts.iter().map(|(k, v)| (k.as_str(), v.as_str())),
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

    emit_slots(&mut out, base, &slot_mods)?;

    // Base's tables are base's own lookups; a downstream crate hands its set
    // to base through `register_contributed_dbd`, its `dbLoadDatabase`.
    if base != "crate" {
        writeln!(
            out,
            "\n/// This crate's tables, for `{base}::server::record::register_contributed_dbd`.\n\
             pub static DBD: {base}::server::record::ContributedDbd =\n\
             \x20   {base}::server::record::ContributedDbd {{\n\
             \x20       record_types: RECORD_TYPES,\n\
             \x20       record_declaration_order,\n\
             \x20       record_fields,\n\
             \x20       record_no_prompt,\n\
             \x20       menus: MENUS,\n\
             \x20   }};"
        )
        .unwrap();
    }

    Ok(out)
}

/// `pub mod slots { pub mod <rtype> { pub const <FIELD>: FieldSlot = ...; } }`,
/// each value the field's index in its type's field table. Nothing is emitted
/// for a target with no record types.
fn emit_slots(out: &mut String, base: &str, types: &[(String, Vec<String>)]) -> Result<(), String> {
    if types.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "\n/// Each field's index in its record type's field table: the slot\n\
         /// `RecordCell::new` caches it at and `FieldSlot` names.\n\
         // Module names are the record type names as declared (`aSub`).\n\
         #[allow(non_snake_case)]\n\
         pub mod slots {{\n\
         \x20   use {base}::server::record::FieldSlot;"
    )
    .unwrap();
    let mut mods = BTreeSet::new();
    for (rtype, fields) in types {
        let module = rust_ident(rtype);
        if !mods.insert(module.clone()) {
            return Err(format!("slots module collision: {rtype} -> {module}"));
        }
        writeln!(
            out,
            "\n    pub mod {module} {{\n        use super::FieldSlot;"
        )
        .unwrap();
        let mut consts = BTreeSet::new();
        for (i, name) in fields.iter().enumerate() {
            let konst = rust_ident(name);
            if !consts.insert(konst.clone()) {
                return Err(format!("slots::{module} collision: {name} -> {konst}"));
            }
            let i = u16::try_from(i)
                .map_err(|_| format!("{rtype}.{name}: index {i} exceeds FieldSlot"))?;
            writeln!(
                out,
                "        pub const {konst}: FieldSlot = FieldSlot({i});"
            )
            .unwrap();
        }
        out.push_str("    }\n");
    }
    out.push_str("}\n");
    Ok(())
}

/// `name` as a Rust identifier, by the escape the module header documents.
fn rust_ident(name: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod",
        "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
        "trait", "true", "type", "unsafe", "use", "where", "while", "abstract", "become", "box",
        "do", "final", "macro", "override", "priv", "try", "typeof", "unsized", "virtual", "yield",
    ];
    let mut out: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    if out == "_" || KEYWORDS.contains(&out.as_str()) {
        out.push('_');
    }
    out
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

    // --- device rows ------------------------------------------------------
    //
    // Each `device()` line as C's `devSup`. Its `link_type` is what
    // `dbCanSetLink` (`dbStaticLib.c:2400-2419`) holds a link to, which is how
    // C rejects a `.db` that hands an `INST_IO` device a constant `INP`. The
    // dset shape comes from the target's hand-written `dsets` module, one
    // const per dset symbol, so a `device()` line without one does not build.
    out.push_str(
        "// ---------------------------------------------------------------------\n\
         // Per-record-type device rows (C `devSup`), in `.dbd` declaration order.\n\
         // ---------------------------------------------------------------------\n\n",
    );
    let mut row_consts: Vec<(&str, String)> = Vec::new();
    let mut by_record: BTreeMap<&str, Vec<&Device>> = BTreeMap::new();
    for d in input.devices {
        by_record.entry(d.record.as_str()).or_default().push(d);
    }
    for (record, devs) in &by_record {
        let konst = format!("DEVICES_{}", record.to_uppercase().replace('-', "_"));
        writeln!(out, "/// The `device({record}, ...)` lines.").unwrap();
        writeln!(out, "pub static {konst}: &[DevSup] = &[").unwrap();
        for d in devs {
            let variant = link_type_variant(&d.link_type)?;
            writeln!(
                out,
                "    DevSup {{\n        name: {},\n        choice: {},\n        link_type: DbLinkType::{variant},\n        dset: dsets::{},\n    }},",
                rust_str(&d.dset),
                rust_str(&d.name),
                d.dset.to_uppercase()
            )
            .unwrap();
        }
        out.push_str("];\n\n");
        row_consts.push((record, konst));
    }
    out.push_str(
        "/// Every `device()` line for a record type, in declaration order:\n\
         /// [`device_menu`]'s choices, each as its `devSup`.\n\
         pub fn devices(record_type: &str) -> Option<&'static [DevSup]> {\n",
    );
    emit_str_lookup(
        out,
        "record_type",
        row_consts
            .iter()
            .map(|(record, konst)| (*record, konst.as_str())),
    );
    out.push_str("}\n\n");
    out.push_str(
        "/// The link type the named `DTYP` of `record_type` demands of its `INP`/`OUT`.\n\
         ///\n\
         /// `None` when no vendored `.dbd` declares that `device()` — a downstream\n\
         /// crate registering its device support at runtime (asyn, motor) brings no\n\
         /// `.dbd`, so there is no declaration to enforce and none is invented.\n\
         pub fn device_link_type(record_type: &str, dtyp: &str) -> Option<DbLinkType> {\n\
         \x20   devices(record_type)?\n\
         \x20       .iter()\n\
         \x20       .find(|row| row.choice.eq_ignore_ascii_case(dtyp))\n\
         \x20       .map(|row| row.link_type)\n\
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
        } else if let Ok(code) = token.parse::<i16>() {
            // `special(111)`: C reads the number with `%hd`
            // (`dbLexRoutines.c:631`), so a named number is that name.
            match code {
                0 => "Special::None".to_string(),
                1 => "Special::NoMod".to_string(),
                2 => "Special::DbAddr".to_string(),
                3 => "Special::Scan".to_string(),
                5 => "Special::AlarmAck".to_string(),
                6 => "Special::As".to_string(),
                7 => "Special::Attribute".to_string(),
                100 => "Special::Mod".to_string(),
                101 => "Special::Reset".to_string(),
                102 => "Special::LinConv".to_string(),
                103 => "Special::Calc".to_string(),
                104.. => format!("Special::Record({code})"),
                _ => {
                    return Err(format!(
                        "{record}.{}: special({code}) names no code",
                        f.name
                    ));
                }
            }
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
    let long_string = entry.is_some_and(|c| c.size.is_some());
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
    writeln!(s, "        long_string: {long_string},").unwrap();
    writeln!(s, "        read_only: {read_only},").unwrap();
    writeln!(s, "        special: {special},").unwrap();
    writeln!(s, "        declared_special: {declared_special},").unwrap();
    writeln!(s, "        pp: {},", f.pp).unwrap();
    writeln!(s, "        asl: {asl},").unwrap();
    writeln!(
        s,
        "        size: {},",
        declared_width(f).map_or_else(|| f.size.unwrap_or(0).to_string(), Width::tokens)
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

    #[test]
    fn slot_identifiers_escape_deterministically() {
        assert_eq!(rust_ident("VAL"), "VAL");
        assert_eq!(rust_ident("aSub"), "aSub");
        assert_eq!(rust_ident("A.B"), "A_B");
        assert_eq!(rust_ident("1X"), "_1X");
        assert_eq!(rust_ident("type"), "type_");
        assert_eq!(rust_ident(""), "__");
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
