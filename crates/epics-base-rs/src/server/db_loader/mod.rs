use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use crate::error::{CaError, CaResult};
use crate::runtime::log::ERL_ERROR;
use crate::server::record::{FieldDesc, Record, check_link_text};
use crate::types::{DbfCode, EpicsValue, PvString};

mod include;
mod source;
mod substitution;
#[cfg(test)]
pub(crate) use include::parse_include_directive;
pub use include::{
    DbLoadConfig, DbOpenedFile, PATH_LIST_SEPARATOR, db_add_path, db_open_file,
    db_open_file_located, db_path, expand_includes, loaded_path, parse_db_file,
    parse_db_file_with_breaktables, parse_db_opened_with_breaktables, set_loaded_path,
};
pub use source::{DbIncludeFrame, DbSource};
pub(crate) use substitution::resolve_template;
pub use substitution::{TemplateLoad, parse_substitutions, substitution_rows};

/// Factory function that creates a record instance.
pub type RecordFactory = Box<dyn Fn() -> Box<dyn Record> + Send + Sync>;

/// Global registry of external record type factories.
/// External crates (e.g., asyn-rs) can register their own record types
/// to override built-in stubs.
static RECORD_FACTORY_REGISTRY: OnceLock<Mutex<HashMap<String, RecordFactory>>> = OnceLock::new();

fn get_registry() -> &'static Mutex<HashMap<String, RecordFactory>> {
    RECORD_FACTORY_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register an external record type factory.
///
/// Two jobs. It lets an external crate override a built-in — the factory is
/// checked FIRST in [`create_record`], so `asyn-rs`'s full `AsynRecord` wins
/// over the CNCT-only stub here. And it is how an application obtains a record
/// type this crate implements but does not register: `dbd/stdRecords.dbd` is
/// what an EPICS Base IOC links, and `aCalcout`, `sCalcout`, `sseq`, `swait`,
/// `transform`, `asyn` and `busy` are not in it. A `.db` naming one of those
/// fails to load until the application asks for it, exactly as a C IOC needs
/// the module's `.dbd` before `dbLoadRecords` will take the record.
pub fn register_record_type(name: &str, factory: RecordFactory) {
    snapshot_declared_fields(name, &factory);
    let mut reg = get_registry()
        .lock()
        .expect("record factory registry mutex poisoned");
    reg.insert(name.to_string(), factory);
}

/// Publish `name`'s `.dbd` field declarations to the by-name registry
/// [`crate::types::dbf_link_class`] reads.
///
/// A downstream record type's declarations live in that crate's own
/// `dbd_generated` and reach `epics-base-rs` only through the record it
/// builds, so the factory is asked for one throwaway instance here — every
/// factory in the workspace is a bare `Default::default()` constructor. A type
/// `dbd_generated` already covers reports the same table it would be looked up
/// in, so registering it changes nothing.
pub(crate) fn snapshot_declared_fields(name: &str, factory: &RecordFactory) {
    use crate::server::record::FieldDeclaration;
    let fields = factory().field_list();
    if !fields.is_empty() {
        declared_types()
            .lock()
            .expect("declared record type set mutex poisoned")
            .insert(name.to_string());
    }
    crate::server::record::register_declared_fields(name, fields);
}

/// Every record type whose field declarations this process can reach BY NAME
/// through the by-name registry, as opposed to through `dbd_generated`.
///
/// [`record_type_field_exists`] needs to tell "this type declares no such
/// field" apart from "this process cannot see this type's declarations at
/// all", and the by-name registry cannot: `declared_field` answers a `dbCommon`
/// match for a type nobody ever registered. This set is that missing bit, and
/// [`snapshot_declared_fields`] is the one funnel every registration path —
/// [`register_record_type`], `IocBuilder` and `IocApplication` — already goes
/// through, so recording it there keeps a single owner.
static DECLARED_TYPES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn declared_types() -> &'static Mutex<HashSet<String>> {
    DECLARED_TYPES.get_or_init(|| Mutex::new(HashSet::new()))
}

/// C `dbFindField` (`dbStaticLib.c:1847-1859` @`R7.0.10`), asked with a record
/// type NAME because the parser holds a type string and no instance.
///
/// `Some(true)`/`Some(false)` is C's answer. `None` means this process cannot
/// see the type's declarations, and then it must not answer at all: C reaches
/// `dbRecordField` only for a type `dbFindRecordType` already resolved —
/// `dbRecordHead` calls `yyerrorAbort` for anything else
/// (`dbLexRoutines.c:1159-1165`) — so a type the port cannot resolve has no C
/// behaviour to match here, and [`create_record`] is what rejects it later.
///
/// Three populations answer `true`, and they are C's three:
///
/// - the field table, which for a vendored type already folds in `dbCommon`;
/// - the `DBF_NOACCESS` names the generator drops from that table but keeps in
///   `record_noaccess_fields` / `DB_COMMON_NOACCESS` — C keeps them in
///   `papFldDes`, so `dbFindField` FINDS them and it is `dbPutString` that
///   then refuses the write ([`is_declared_no_access`] is that half);
/// - `RTYP` and `VERS`, which are not fields at all. `dbFindField` falls back
///   to `dbGetRecordAttribute` (`dbStaticLib.c:1852-1853`) and `dbReadCOM`
///   installs exactly those two on every record type
///   (`dbLexRoutines.c:311-331`), so `field(RTYP,"zz")` loads in C. Measured.
///
/// The comparison is case-SENSITIVE: `dbFindFieldPart` binary-searches
/// `papsortFldName` with `strncmp` (`dbStaticLib.c:1822-1824`), so
/// `field(calc,"1")` on a calc record is NOT the `CALC` field. Asking
/// `declared_field` (which matches case-insensitively) and then comparing the
/// declaration's own spelling is what makes that exact.
fn record_type_field_exists(record_type: &str, field: &str) -> Option<bool> {
    use crate::server::record::dbd_generated;

    let visible = dbd_generated::record_fields(record_type).is_some()
        || declared_types()
            .lock()
            .expect("declared record type set mutex poisoned")
            .contains(record_type);
    if !visible {
        return None;
    }
    if crate::server::record::declared_field(record_type, field).is_some_and(|d| d.name == field) {
        return Some(true);
    }
    let listed = |names: &[&str]| names.contains(&field);
    if dbd_generated::record_noaccess_fields(record_type).is_some_and(listed)
        || listed(dbd_generated::DB_COMMON_NOACCESS)
    {
        return Some(true);
    }
    Some(matches!(field, "RTYP" | "VERS"))
}

/// C's `dbFieldConfusionMap` (`dbLexRoutines.c:1220-1228` @`R7.0.10`): pairs of
/// field names that name the same idea on different record types, so a `.db`
/// written against one type is answered with the other type's spelling instead
/// of a lexical near-miss. The partner of entry `i` is entry `i ^ 1`, which is
/// why one flat table reads in both directions.
///
/// A trailing `A-J` is a RANGE rather than a literal name: the character in the
/// last position carries its offset across to the partner's range, which is
/// what turns `DOLA` into `INPK` and `INP0` into `INPA`.
const FIELD_CONFUSION_MAP: [&str; 14] = [
    "INP", "OUT", "DOL", "INP", "ZNAM", "ZRST", "ONAM", "ONST", "INPA-J", "DOL0-9", "INPK-P",
    "DOLA-F", "INP0-9", "INPA-J",
];

/// The field name entry `i` of [`FIELD_CONFUSION_MAP`] proposes for `name`, or
/// `None` when the entry does not apply. Whether the record type DECLARES the
/// proposal is the caller's test, exactly as it is C's.
fn confusion_guess(name: &str, i: usize) -> Option<String> {
    let fieldname = FIELD_CONFUSION_MAP[i].as_bytes();
    let replacement = FIELD_CONFUSION_MAP[i ^ 1].as_bytes();
    let n = name.as_bytes();
    let l = fieldname.len();

    if l >= 3 && fieldname[l - 2] == b'-' {
        // Range entry. C compares `l-3` bytes with `strncmp`, which a shorter
        // name loses on its terminator, and then reads `name[l-3]` — the
        // terminator again for a name that stops at the prefix, and no range
        // starts at NUL. Note what C does NOT test: the tail beyond `l-3` is
        // ignored, so `INPAX` proposes `DOL0` and is rejected only because no
        // record declares it.
        let k = l - 3;
        if n.len() < k || n[..k] != fieldname[..k] {
            return None;
        }
        let c = n.get(k).copied().unwrap_or(0);
        if c < fieldname[k] || c > fieldname[l - 1] {
            return None;
        }
        let l2 = replacement.len();
        let mut buf = replacement[..l2 - 2].to_vec();
        buf[l2 - 3] += c - fieldname[k];
        return String::from_utf8(buf).ok();
    }

    (n == fieldname).then(|| String::from_utf8_lossy(replacement).into_owned())
}

/// C `epicsStrSimilarity` (`epicsString.c:398-456` @`R7.0.10`) — a Levenshtein
/// distance whose substitution is weighted, normalised into `0.0..=1.0`.
///
/// The weights are the whole point and a plain edit distance is not a
/// substitute: a delete or an insert costs 2, a substitution between different
/// letters costs 2, and one between the same letter in different case costs 1.
/// So `"YES"` scores 0.5 against `"yes"` where a plain distance would score it
/// the same as `"NO"`. The normaliser is `2 * max(len)`, the cost of rewriting
/// every character, which makes a pair sharing no usable character score
/// exactly 0 — the value the caller reads as "not a candidate at all".
fn str_similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let norm = 2 * a.len().max(b.len());
    let mut dist0: Vec<usize> = (0..=b.len()).map(|i| 2 * i).collect();
    let mut dist1 = vec![0usize; b.len() + 1];

    for (ai, &ca) in a.iter().enumerate() {
        dist1[0] = 2 * (ai + 1);
        for (bi, &cb) in b.iter().enumerate() {
            let delcost = dist0[bi + 1] + 2;
            let inscost = dist1[bi] + 2;
            let mut subcost = dist0[bi];
            if ca != cb {
                subcost += 1;
            }
            if !ca.eq_ignore_ascii_case(&cb) {
                subcost += 1;
            }
            dist1[bi + 1] = delcost.min(inscost).min(subcost);
        }
        std::mem::swap(&mut dist0, &mut dist1);
    }

    if norm == 0 {
        1.0
    } else {
        (norm - dist0[b.len()]) as f64 / norm as f64
    }
}

/// The factor C applies to a candidate's similarity once it has parsed the
/// assigned VALUE against the candidate's declared type
/// (`dbLexRoutines.c:1295-1368`): 1.0 when the value fits, 0.1 when it does
/// not, and for a menu or device field 0.5 or 1.5 for the two ways it can fit.
fn value_type_weight(record_type: &str, desc: &FieldDesc, value: &str) -> f64 {
    use crate::types::c_parse::{self, NumericField};

    let numeric = match desc.declared_dbf {
        DbfCode::Char => NumericField::Char,
        DbfCode::UChar => NumericField::UChar,
        DbfCode::Short => NumericField::Short,
        // C runs `DBF_ENUM` through `epicsParseUInt16` alongside `DBF_USHORT`
        // (`dbLexRoutines.c:1311-1314`) — an enum's `.db` spelling is its index.
        DbfCode::UShort | DbfCode::Enum => NumericField::UShort,
        DbfCode::Long => NumericField::Long,
        DbfCode::ULong => NumericField::ULong,
        DbfCode::Int64 => NumericField::Int64,
        DbfCode::UInt64 => NumericField::UInt64,
        DbfCode::Float => NumericField::Float,
        DbfCode::Double => NumericField::Double,
        DbfCode::Menu | DbfCode::Device => return menu_type_weight(record_type, desc, value),
        // C's `default:` arm leaves the status zero and `end` pointing AT the
        // quote it compares against, so a string or link candidate never takes
        // the mismatch penalty however the value is spelled.
        DbfCode::String
        | DbfCode::Inlink
        | DbfCode::Outlink
        | DbfCode::Fwdlink
        | DbfCode::NoAccess => return 1.0,
    };
    if c_parse::parse_auto_base_units_null(numeric, value).is_some() {
        1.0
    } else {
        0.1
    }
}

/// [`value_type_weight`]'s `DBF_MENU`/`DBF_DEVICE` arm: the value may name the
/// choice or index it, and C scores the two differently — a numeric device type
/// index halves the score because almost nobody writes one, a spelled-out
/// choice multiplies it by 1.5.
fn menu_type_weight(record_type: &str, desc: &FieldDesc, value: &str) -> f64 {
    use crate::types::c_parse::{self, NumericField};

    let device = matches!(desc.declared_dbf, DbfCode::Device);
    // C dereferences `pflddes->ftPvt` here without a NULL test and SEGFAULTS
    // when the loaded `.dbd` set declares no device support for the record
    // type: `record(calc,"C") { field(DOL, "1") }` kills softIoc @`R7.0.10`
    // outright, after the ERROR line and before any suggestion. An absent menu
    // is read here as an EMPTY one, which is the path C's own code takes for
    // `nChoice == 0` — neither the index nor the string can match, so the
    // candidate takes the mismatch penalty and the loader carries on.
    let contributed;
    let choices: &[&str] = if device {
        contributed = crate::server::record::merged_device_menu(record_type);
        &contributed
    } else {
        // `menu(menuScan)` is the one menu the generated table deliberately
        // leaves empty — its choices are the site's `menuScan.dbd`, resolved at
        // run time — so `SCAN` and `SSCN` come from the shared registry that
        // every other reader of a menu field uses. C has no such split: it
        // reads `pflddes->ftPvt`, which IS the loaded menu whatever the site
        // declared, so taking base's compile-time table here would answer for
        // the wrong menu on a site that ships its own rates.
        desc.menu
            .or_else(|| crate::server::record::shared_menu_choices(desc.name))
            .unwrap_or(&[])
    };

    if let Some(EpicsValue::UShort(index)) =
        c_parse::parse_auto_base_units_null(NumericField::UShort, value)
    {
        if usize::from(index) < choices.len() {
            return if device { 0.5 } else { 1.0 };
        }
    }
    if choices.contains(&value) { 1.5 } else { 0.1 }
}

/// C's suggestion line for `desc` (`dbLexRoutines.c:1379-1384` @`R7.0.10`),
/// without its newline.
///
/// Byte-exact against C: four leading spaces, the proposal in double quotes,
/// the question mark, then — only for a field whose `.dbd` declares a
/// `prompt` — TWO spaces and the prompt in parentheses.
fn suggestion_line(desc: &FieldDesc) -> String {
    let mut line = did_you_mean(desc.name);
    if let Some(prompt) = desc.prompt {
        line.push_str(&format!("  ({prompt})"));
    }
    line
}

/// The proposal both of C's `.db` suggestions are built from — four leading
/// spaces, the name in double quotes, a question mark
/// (`dbLexRoutines.c:1381`, `dbStaticLib.c:2702`), without its newline.
fn did_you_mean(name: &str) -> String {
    format!("    Did you mean \"{name}\"?")
}

/// The field C would propose after refusing `name` on `record_type`, or `None`
/// when C proposes nothing (`dbLexRoutines.c:1241-1378` @`R7.0.10`).
///
/// `None` is a real answer, not a fallback: every candidate whose similarity
/// weighs out to exactly zero is skipped, so a name sharing no character with
/// anything the record type declares — `field(ZZZZ, "1")` on an `ai` — leaves
/// the ERROR line standing alone rather than reaching for the least-bad name.
///
/// The confusion map is consulted first and wins outright when it hits, which
/// is why `bo.INP` proposes `OUT` rather than the `INP`-shaped near-misses that
/// score higher lexically.
fn suggest_field(record_type: &str, name: &str, value: &str) -> Option<&'static FieldDesc> {
    use crate::server::record::{Special, declared_field, declared_fields};

    for i in 0..FIELD_CONFUSION_MAP.len() {
        let Some(guess) = confusion_guess(name, i) else {
            continue;
        };
        // C resolves the proposal through `dbFindFieldPart`, an exact search of
        // the record type's sorted field names, so a proposal the type does not
        // declare is dropped and the next entry tried.
        if let Some(desc) = declared_field(record_type, &guess).filter(|d| d.name == guess) {
            return Some(desc);
        }
    }

    let mut best: Option<&'static FieldDesc> = None;
    let mut best_sim = -1.0_f64;
    for desc in declared_fields(record_type) {
        // A field the record type will not let a `.db` set is not a candidate
        // for having been misspelled INTO. The test is on the DECLARATION: C
        // reads `pdbFldDes->special`, which an SPC_DBADDR field's `cvt_dbaddr`
        // never rewrites, so `lsi.VAL` stays skipped even though the address it
        // resolves to carries `SPC_MOD`.
        if matches!(desc.declared_special, Special::NoMod | Special::DbAddr) {
            continue;
        }
        let mut sim = str_similarity(name, desc.name);
        if desc.promptgroup.is_none() {
            sim *= 0.5;
        }
        if desc.interest != 0 {
            sim *= 1.0 - 0.1 * f64::from(desc.interest);
        }
        if sim == 0.0 {
            continue;
        }
        if !value.is_empty() {
            sim *= value_type_weight(record_type, desc, value);
        }
        // `>`, not `>=`: a tie goes to the field declared first, and `dbCommon`
        // is included ahead of the record's own fields.
        if sim > best_sim {
            best_sim = sim;
            best = Some(desc);
        }
    }
    best
}

/// The `.dbd` name and choice table of the menu `field` resolves its `.db`
/// value against, or `None` when the field is not a menu.
///
/// A menu field is one declared `DBF_MENU` — or one that carries its own
/// choice table, however its `DbfCode` reads. An externally-registered
/// record type builds its `FieldDesc`s in its own crate, so its tables are
/// not the generated `MENUS` statics and its `declared_dbf` is whatever
/// [`FieldDesc::new`] derived from the served type (`Enum`, mostly); C could
/// only have written such a field as `DBF_MENU` with a named `menu()`, so a
/// carried table is treated as that declaration. The name half of C's pair
/// is then unavailable — the `MENUS` lookup is by table identity and the
/// external static is not in it — which is why the name is an `Option`:
/// validation does not need it, only the refusal wording does.
///
/// `DBF_DEVICE` is deliberately not answered here. C's device menu is the
/// record type's `device()` lines, so every DTYP a `.db` may name is in it; the
/// port has a second channel — `IocBuilder::register_device_support` and
/// `register_dynamic_device_support` — whose factories never reach
/// `merged_device_menu`, so a valid `field(DTYP,"asynInt32")` is absent from
/// the menu that would judge it.
///
/// `menuScan` is why the menu's NAME is looked up rather than carried on the
/// declaration: its choices are the site's, resolved at run time, so the
/// generated `FieldDesc` holds `None` and the table is in no generated row.
fn load_menu_of(desc: &FieldDesc) -> Option<(Option<&'static str>, &'static [&'static str])> {
    use crate::server::record::shared_menu_choices;

    // `DBF_DEVICE` stays out even when it carries a table: its channel is
    // the device menu, and its C refusal is worded differently (see above).
    if desc.declared_dbf != DbfCode::Menu
        && (desc.menu.is_none() || desc.declared_dbf == DbfCode::Device)
    {
        return None;
    }
    let choices = desc
        .menu
        .or_else(|| shared_menu_choices(desc.name))
        .unwrap_or(&[]);
    if choices.is_empty() {
        return None;
    }
    Some((menu_name_of(choices), choices))
}

/// The `.dbd` name of the menu whose choice table is `choices`.
///
/// C reads it off the menu itself (`pdbMenu->name`, `dbStaticRun.c:497`). The
/// generated `MENUS` table is name -> choices and a menu field's `menu` member
/// is that same static, so identity — not equality — is the relation: two menus
/// may list the same strings, and only identity tells them apart.
fn menu_name_of(choices: &'static [&'static str]) -> Option<&'static str> {
    use crate::server::record::dbd_generated::MENUS;

    if std::ptr::eq(choices, crate::server::record::menu_scan().choices()) {
        return Some("menuScan");
    }
    MENUS
        .iter()
        .find(|(_, _, table)| std::ptr::eq(*table, choices))
        .map(|(name, _, _)| *name)
}

/// C's refusal for a `.db` field value `dbPutString` would not take
/// (`dbLexRoutines.c:1406-1413` @`R7.0.10`), without its newline.
///
/// `detail` is `pdbentry->message`, which C prints unconditionally between the
/// value and the status text — an arm that set no message still leaves the two
/// spaces that produces.
fn cant_set_line(record: &str, field: &str, value: &str, detail: &str, status: &str) -> String {
    format!("{ERL_ERROR}: Can't set '{record}.{field}' to '{value}' {detail} : {status}")
}

/// The refusal C's `.db` loader would print for `value` on `field`, or `None`
/// when C takes the value.
///
/// This is `dbPutString` (`dbStaticLib.c:2570`) read as a predicate: the field
/// type it switches on is the DECLARED `DBF_*` token, and each arm either
/// stores or returns a status `dbRecordField` then prints. The name says menu
/// because the menu arm was ported first; the numeric arms are
/// [`numeric_value_refusal`] and reach the same two load paths through the same
/// three call sites, which is the point of deciding it here rather than at
/// whichever site happens to apply the field.
///
/// The menu arm of `dbPutStringNum` (`dbStaticRun.c:484-512`) has TWO refusals,
/// and they read differently. A value that is neither an exact choice nor a
/// number is `S_db_badChoice`, and the arm names the menu first
/// (`dbMsgPrint(pdbentry, "using menu %s", pdbMenu->name)`). A value that IS a
/// number but fails C's out-of-menu bound is `S_dbLib_badField` with no message
/// at all, which is why that line carries two spaces where the other carries
/// the menu.
///
/// The bound itself is not restated here: it already has an owner — the loader
/// arm of the single menu converter, which lets one past the last choice and
/// `65535` through — so this asks it, then asks only whether the value parsed,
/// to pick which of C's two wordings applies.
///
/// `None` also covers a menu that resolves no choice table at all; such a
/// field falls to the numeric arms. A menu whose NAME did not resolve — an
/// externally-registered record type's own table — still validates against
/// its choices; only the refusal wording loses the name C would print.
/// DTYP is deliberately NOT judged here, and the omission is a decision
/// rather than an oversight. C refuses a bad `field(DTYP,…)` during the
/// `.db` parse, through `dbPutStringNum`'s own `DBF_DEVICE` branch
/// (`dbStaticRun.c:499-501`), and words it its own way — `no such device
/// support for '<record type>' record type`, NOT the menu wording this
/// function builds. The port instead warns at `iocInit` from
/// `wire_device_support` and lets the load succeed. Closing that gap needs a
/// decision about when the device menu is considered CLOSED: C loads every
/// `.dbd` before any `.db`, so `dbDeviceMenu` is complete at put time,
/// whereas the port's `contributed_device_menu` is filled by downstream
/// crates (asyn) AFTER the load — refusing here would fail a
/// `field(DTYP,"asynInt32")` that works today.
pub(crate) fn menu_value_refusal(
    record_type: &str,
    record_name: &str,
    field: &str,
    value: &str,
) -> Option<MenuRefusal> {
    use crate::server::record::declared_field;

    let desc = declared_field(record_type, field)?;
    // C `dbPutString` refuses ANY field value that still carries a macro,
    // before the field-type switch it would otherwise reach
    // (`dbStaticLib.c:2579-2585` @`R7.0.10`). The `macroIsOk` half of that test
    // is `dbIsMacroOk(pdbentry)`, whose only definition in base is
    // `return(FALSE)` (`dbStaticRun.c:228`), so it exempts nothing in an IOC
    // and `stringHasMacro` alone decides; `dbLoadTemplate` defers by expanding
    // its substitutions before the text reaches the loader, not through this
    // hook.
    if value.contains("$(") || value.contains("${") {
        return Some(MenuRefusal {
            notice: Some(format!("{record_name}.{field} Has unexpanded macro")),
            // `S_dbLib_badField` with no `pdbentry->message`, which is what
            // leaves two spaces before the colon.
            line: cant_set_line(record_name, field, value, "", "Bad Field value"),
            // `dbRecordField` runs `dbPutStringSuggest` after every refusal
            // (`dbLexRoutines.c:1414`) and only its menu and device arms
            // propose anything, so a menu field still gets a proposal off the
            // unexpanded text -- measured on `compressTest.db`,
            // `field(ALG,"$(ALG,undefined)")` gets `Did you mean "Average"?`
            // while `field(NSAM,"$(NSAM,undefined)")` gets nothing.
            suggestion: load_menu_of(desc).and_then(|(_, choices)| suggest_choice(choices, value)),
        });
    }
    let probe = db_put_string_probe(value);
    let (detail, status, suggestion) = match load_menu_of(desc) {
        Some((menu_name, choices)) => menu_arm(desc, field, menu_name, choices, probe, value)?,
        // Not a menu — or one that resolves no choice table. Either way the
        // numeric arms of the same C function decide it.
        None => (
            String::new(),
            numeric_value_refusal(desc.declared_dbf, field, probe)?,
            None,
        ),
    };
    Some(MenuRefusal {
        notice: None,
        line: cant_set_line(record_name, field, value, &detail, status),
        suggestion,
    })
}

/// The `DBF_MENU` arm of `dbPutStringNum` (`dbStaticRun.c:484-512`), as the
/// three pieces its refusal is printed from.
fn menu_arm(
    desc: &FieldDesc,
    field: &str,
    menu_name: Option<&str>,
    choices: &[&'static str],
    probe: &str,
    value: &str,
) -> Option<(String, &'static str, Option<String>)> {
    use crate::server::record::resolve_menu_field_string_db_load;
    use crate::types::c_parse::{self, NumericField};

    if resolve_menu_field_string_db_load(field, choices, desc.dbf_type, probe).is_ok() {
        return None;
    }
    let (detail, status) = match c_parse::parse_auto_base_units_null(NumericField::UShort, probe) {
        // `epicsParseUInt16` succeeded, so the bound is what refused it.
        Some(_) => (String::new(), "Bad Field value"),
        // C words this with `pdbMenu->name`; an externally-registered menu
        // has no `.dbd` name, and the field's own name is the nearest thing
        // the wording can carry.
        None => (
            format!("using menu {}", menu_name.unwrap_or(field)),
            "Illegal choice",
        ),
    };
    // C suggests off the value it was GIVEN, not the "0" the numeric arm
    // substituted for an empty one — `dbRecordField` hands
    // `dbPutStringSuggest` its own `value` (`dbLexRoutines.c:1414`). Only the
    // menu and device arms of that function propose anything at all
    // (`dbStaticLib.c:2669-2708`), which is why the numeric refusals below
    // carry no suggestion.
    Some((detail, status, suggest_choice(choices, value)))
}

/// The `errSymLookup` text of the `S_stdlib_*` statuses (`epicsStdlib.h:43-51`)
/// `dbRecordField` prints after the value it refused.
const S_STDLIB_NO_CONVERSION: &str = "No digits to convert";
const S_STDLIB_EXTRANEOUS: &str = "Extraneous characters";
const S_STDLIB_UNDERFLOW: &str = "Too small to represent";
const S_STDLIB_OVERFLOW: &str = "Too large to represent";

/// C `dbPutStringNum`'s "empty string is the same as writing numeric zero"
/// (`dbStaticRun.c:405-407`).
///
/// C substitutes once, at the top of the function, so the choice test and the
/// numeric arms both see the `"0"` — `field(PINI,"")` is index 0 and
/// `field(PREC,"")` is the number 0, from one rule rather than two.
fn db_put_string_probe(value: &str) -> &str {
    if value.is_empty() { "0" } else { value }
}

/// The numeric arms of C `dbPutStringNum` (`dbStaticRun.c:409-481` @`R7.0.10`)
/// as the status text of the refusal they return, or `None` when C stores the
/// value.
///
/// `dbConvertStrict` is zero unless an IOC sets it (`dbStaticRun.c:35`), so the
/// integer widths do NOT range-check against the field: every signed one parses
/// through `epicsParseInt64` and every unsigned one through `epicsParseUInt64`,
/// and the 64-bit result is then CAST to the field. `field(PHAS,"70000")`
/// therefore loads and stores 4464 and `field(TPRO,"-1")` stores 255; only what
/// `strtol`/`strtoul` itself refuses — no digits, a 64-bit overflow, or a
/// trailing non-space tail — fails the load, which is why `field(VAL,"1.0")`
/// on a `longout` is "Extraneous characters". `DBF_ENUM` shares the unsigned
/// arm rather than the menu one, so `field(VAL,"ONE")` on an `mbbo` is "No
/// digits to convert" and never a choice lookup.
fn numeric_value_refusal(declared: DbfCode, field: &str, s: &str) -> Option<&'static str> {
    db_load_numeric_value(declared, field, s).err()
}

/// The numeric arms of C `dbPutStringNum` in full: `Ok(Some(v))` is the value C
/// STORES, `Err(status)` the refusal it returns, and `Ok(None)` a declaration
/// these arms do not decide.
///
/// The stored value is what makes this the whole arm rather than half of it.
/// `dbConvertStrict` is zero unless an IOC sets it (`dbStaticRun.c:35`), so the
/// integer widths do not range-check against the field: every signed one parses
/// through `epicsParseInt64` and every unsigned one through `epicsParseUInt64`,
/// and the 64-bit result is then CAST. Reading only the refusal out of that
/// leaves the truncation behind, and the port then refused `field(PHAS,"70000")`
/// where C stores 4464 — the mirror of the values it used to accept.
fn db_load_numeric_value(
    declared: DbfCode,
    field: &str,
    s: &str,
) -> Result<Option<EpicsValue>, &'static str> {
    use crate::runtime::stdlib::{ParseDoubleError, epics_parse_double};
    use crate::types::c_parse::{self, NumericField};

    let s = db_put_string_probe(s);
    // The 64-bit parse C runs and the cast it then makes, as one row per
    // declared width (`dbStaticRun.c:433-438`, `:466-471`). Both casts go
    // through `u64` because two's-complement truncation is the same operation
    // either way: `-40000` reaches a `DBF_SHORT` as 25536 and `-1` reaches a
    // `DBF_UCHAR` as 255, which is what C's `(epicsInt16)` / `(epicsUInt8)`
    // does to `strtol`'s and `strtoul`'s answers.
    let (width, cast): (NumericField, fn(u64) -> EpicsValue) = match declared {
        DbfCode::Char => (NumericField::Int64, |v| EpicsValue::Char(v as u8)),
        DbfCode::Short => (NumericField::Int64, |v| EpicsValue::Short(v as i16)),
        DbfCode::Long => (NumericField::Int64, |v| EpicsValue::Long(v as i32)),
        DbfCode::Int64 => (NumericField::Int64, |v| EpicsValue::Int64(v as i64)),
        DbfCode::UChar => (NumericField::UInt64, |v| EpicsValue::UChar(v as u8)),
        // C gives `DBF_ENUM` and `DBF_USHORT` one case, so a label never
        // resolves on an `mbbo` VAL — `field(VAL,"ONE")` is "No digits to
        // convert" and not a choice lookup.
        DbfCode::Enum | DbfCode::UShort => (NumericField::UInt64, |v| EpicsValue::UShort(v as u16)),
        DbfCode::ULong => (NumericField::UInt64, |v| EpicsValue::ULong(v as u32)),
        DbfCode::UInt64 => (NumericField::UInt64, |v| EpicsValue::UInt64(v)),
        DbfCode::Float | DbfCode::Double => {
            let v = epics_parse_double(s).map_err(|e| match e {
                ParseDoubleError::NoConversion => S_STDLIB_NO_CONVERSION,
                ParseDoubleError::Extraneous => S_STDLIB_EXTRANEOUS,
                ParseDoubleError::Underflow => S_STDLIB_UNDERFLOW,
                ParseDoubleError::Overflow => S_STDLIB_OVERFLOW,
            })?;
            if declared == DbfCode::Double {
                return Ok(Some(EpicsValue::Double(v)));
            }
            // `epicsParseFloat32` is `epicsParseDouble` plus two more gates
            // (`epicsStdlib.c:325-329`), whose owner is `narrow_to_f32`. They
            // are disjoint by magnitude, so the value itself names the side it
            // fell off.
            return match c_parse::narrow_to_f32(v) {
                Some(f) => Ok(Some(EpicsValue::Float(f))),
                None if v.abs() <= f64::from(f32::MIN_POSITIVE) => Err(S_STDLIB_UNDERFLOW),
                None => Err(S_STDLIB_OVERFLOW),
            };
        }
        // C's `dbPutString` sends the rest somewhere that is not a numeric
        // parse: a link to `dbParseLink`, a `DBF_STRING` to a length check
        // against the field size, `DBF_DEVICE` to the device menu, and
        // `DBF_NOACCESS` to "Can't set array field before iocInit()"
        // (`dbStaticLib.c:2586-2650`). None of them is decided here.
        DbfCode::String
        | DbfCode::Menu
        | DbfCode::Device
        | DbfCode::Inlink
        | DbfCode::Outlink
        | DbfCode::Fwdlink
        | DbfCode::NoAccess => return Ok(None),
    };
    let bits = match c_parse::parse_auto_base_units_null(width, s) {
        // `parse_auto_base_units_null` answers in the variant its width names,
        // and `width` is only ever one of these two.
        Some(EpicsValue::Int64(v)) => v as u64,
        Some(EpicsValue::UInt64(v)) => v,
        _ => return Err(integer_refusal(width, field, s)),
    };
    Ok(Some(cast(bits)))
}

/// Which of `epicsParseLong`'s three statuses (`epicsStdlib.c:38-47`) refused
/// `s`, once the strict parse has said that one of them did.
fn integer_refusal(
    width: crate::types::c_parse::NumericField,
    field: &str,
    s: &str,
) -> &'static str {
    // `strtol` with base 0 either converts a digit or converts nothing, since
    // both prefixes it detects begin with the digit `0`, so the first character
    // after the leading whitespace and sign decides `endp == str`.
    let scanned = s.trim_start_matches(crate::runtime::stdlib::c_isspace);
    let scanned = scanned.strip_prefix(['-', '+']).unwrap_or(scanned);
    if !scanned.starts_with(|c: char| c.is_ascii_digit()) {
        return S_STDLIB_NO_CONVERSION;
    }
    // The same scan with the `units == NULL` tail test dropped, which is the
    // only other thing the strict form refuses for: digits that still do not
    // convert are `strtol`'s own ERANGE.
    if crate::types::c_parse::put_string_element(field, width, s).is_err() {
        S_STDLIB_OVERFLOW
    } else {
        S_STDLIB_EXTRANEOUS
    }
}

/// A refused `.db` menu value, with every line C prints for it.
pub(crate) struct MenuRefusal {
    /// What `dbPutString` itself printed before returning the status
    /// (`dbStaticLib.c:2582-2584`), which therefore precedes the `ERROR:` line
    /// its caller prints. `None` for the arms that return a status silently.
    pub notice: Option<String>,
    /// The `ERROR:` line.
    pub line: String,
    /// C `dbPutStringSuggest`'s proposal, or `None` when it proposes nothing.
    pub suggestion: Option<String>,
}

/// The choice C would propose after refusing `value` (`dbPutStringSuggest`,
/// `dbStaticLib.c:2665-2708` @`R7.0.10`), or `None` when it proposes none.
///
/// `maxdist` starts at 0.0 and the test is strict `>`, so a choice that scores
/// exactly zero is never offered and the earliest of equal scorers wins — the
/// same "no similarity, no suggestion" rule the field-name half follows, and
/// the reason `field(PINI,"7")` gets a bare refusal.
///
/// The metric is C's own `epicsStrSimilarity`, unweighted here: unlike the
/// field-name suggestion this one has no promptgroup, interest or type to
/// weigh a candidate by — a menu choice is just a string.
fn suggest_choice(choices: &[&'static str], value: &str) -> Option<String> {
    let mut best: Option<&str> = None;
    let mut max_dist = 0.0_f64;
    for choice in choices {
        let dist = str_similarity(value, choice);
        if dist > max_dist {
            max_dist = dist;
            best = Some(choice);
        }
    }
    best.map(did_you_mean)
}

/// Every externally registered record type, as `(name, entry address)`.
///
/// The address is the registered factory's own, which is what C's
/// `registryRecordTypeFind` prints: `registryAdd` stores the
/// `recordTypeLocation` pointer and the `%p` the command prints
/// (`registryIocRegister.c:31`) is that stored value. Read by the
/// `registry*` iocsh family; the built-in half of the type universe is
/// [`crate::server::record::dbd_generated::RECORD_TYPES`], which this map
/// overrides (see [`create_record`]).
pub(crate) fn registered_record_type_entries() -> Vec<(String, usize)> {
    let reg = get_registry()
        .lock()
        .expect("record factory registry mutex poisoned");
    reg.iter()
        .map(|(name, factory)| (name.clone(), (&**factory) as *const _ as *const () as usize))
        .collect()
}

/// A record definition parsed from a .db file.
#[derive(Debug, Clone)]
pub struct DbRecordDef {
    pub record_type: String,
    pub name: String,
    /// The record's fields, in file order. A `.db` value is a BYTE STRING, not
    /// text: C's escape translation writes one byte per `\xHH`
    /// (`epicsString.c:106`), and a DBF_STRING's budget is 40 BYTES. Modelling
    /// the value as a Rust `String` made `\xff` two bytes (R19-68).
    pub fields: Vec<DbFieldDef>,
    /// Aliases declared inside the record body (`alias("name")`).
    /// Mirrors epics-base PR #336 — alias names are validated with
    /// the same rules as record names.
    pub aliases: Vec<String>,
    /// `info(tag, value)` pairs declared inside the record body.
    /// Mirrors `info` directives in EPICS db files; common tags include
    /// `asyn:READBACK` (asyn upstream PRs #60 / #208), `Q:form`, etc.
    pub info_tags: Vec<(String, String)>,
}

/// One `field(NAME,"value")` element, with where it was read from.
///
/// The line is not decoration: C refuses a field value inside the parse,
/// so `yyerror` can name the line from `pinputFileNow` (`dbYacc.y:380`).
/// This port screens the same values in the install loop, after the text
/// is gone, so the position has to travel with the element — and it has
/// to travel ON it rather than in a parallel vector, because the install
/// loop `retain`s the list and a parallel vector would silently shift
/// every remaining field's line by one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbFieldDef {
    pub name: String,
    pub value: PvString,
    /// 1-based line of the flattened, macro-expanded text — the index
    /// [`DbSource::at`] takes. C's `pinputFileNow->line_num` when
    /// `dbRecordField` reduces, which is the line the element's closing
    /// `)` sits on.
    pub line: u32,
}

impl DbFieldDef {
    /// A field with no source behind it — a record built programmatically
    /// or by a test, where C has no line either.
    pub fn new(name: impl Into<String>, value: impl Into<PvString>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            line: 0,
        }
    }
}

/// Validate a record (or alias) name against epics-base PR #78 rules.
///
/// Returns `Err(CaError::DbParseError)` for the hard-error cases (empty
/// name, embedded space/tab/quote/dot/dollar). Logs `tracing::warn!`
/// for the soft-warning cases (leading `-`/`+`/`[`/`{`, embedded
/// non-printable characters); the parse continues so legacy databases
/// still load.
///
/// The check runs **after** macro substitution, mirroring base where
/// `dbRecordHead` is invoked from the lexer with the substituted name.
pub(crate) fn validate_record_name(name: &str, line: usize, col: usize) -> CaResult<()> {
    if name.is_empty() {
        return Err(CaError::DbParseError {
            line,
            column: col,
            message: "record/alias name can't be empty".into(),
        });
    }
    for (i, c) in name.chars().enumerate() {
        if i == 0 && matches!(c, '-' | '+' | '[' | '{') {
            tracing::warn!(name, "record/alias name should not begin with '{}'", c);
        }
        // Non-printable ASCII (< space) is a warning, not an error —
        // matches base PR #78's `errlogPrintf("Warning: ...")` branch.
        if (c as u32) < 0x20 {
            tracing::warn!(
                name,
                "record/alias name should not contain non-printable 0x{:02X}",
                c as u32
            );
            continue;
        }
        if matches!(c, ' ' | '\t' | '"' | '\'' | '.' | '$') {
            return Err(CaError::DbParseError {
                line,
                column: col,
                message: format!("bad character '{c}' in record/alias name \"{name}\""),
            });
        }
    }
    Ok(())
}

/// The single owner of C's `yyerror` / `yyerrorAbort` distinction.
///
/// C's `.db` reader has two error routines and uses both inside the same
/// file: `yyerror(NULL)` (`dbLexRoutines.c:453`, `:1178`) records the
/// failure, leaves everything already parsed in `pdbbase`, and lets
/// `pvt_yy_parse` return -1 once the whole text has been read
/// (`dbYacc.y:384-398`); `yyerrorAbort` (`dbLexRoutines.c:1133`, `:1166`)
/// stops the parse outright.
///
/// Invariant: a per-item failure records a diagnostic and a non-zero final
/// status but MUST NOT discard items already parsed; only an abort-class
/// failure — an `Err` return out of the parse — stops it. Every
/// recoverable site reports through [`DbFaults::recoverable`], which is
/// the one place that decides what a recoverable failure does, so no
/// caller has to re-derive the classification.
///
/// Emission invariant: [`DbFaults::recoverable`] is the SOLE owner of a
/// loader diagnostic's trip to the operator. Every other holder of a
/// `DbFaults` MUST treat it as a record that a diagnostic was emitted, and
/// MUST NOT emit one itself; a command that wants to say the load failed
/// says so in C's own words — `ERROR: Failed to load '<file>'` — and never
/// by repeating a line `recoverable` has already written. The `.db` loader
/// had exactly that double voice: `recoverable` printed AND stored, a
/// `status()` accessor handed the stored copy back, and `dbLoadRecords`
/// returned it as the shell's error text, so the port said each diagnostic
/// twice where C says it once. The rule is enforced by construction rather
/// than by convention: `messages` is private, no error value the loader
/// returns carries text, and the only accessor that yields the string is
/// `#[cfg(test)]`, so outside this crate's own unit tests a second print
/// cannot be written at all.
/// One recoverable `.db` diagnostic with every line C prints for it, in
/// C's order.
///
/// The order is the whole reason this is one value rather than three
/// calls. C writes a refusal as `dbPutString`'s own line, then the
/// `ERROR:` line `dbRecordField` builds from the status it returned,
/// then `dbPutStringSuggest`'s proposal, and only then `yyerror(NULL)`
/// (`dbLexRoutines.c:1379-1387`, `:1414`) — so the suggestion lands
/// ABOVE the position context, not below it. A `DbFaults` that also
/// offered a bare "print this line" method let a caller choose the side,
/// and both callers chose the wrong one: `Did you mean "Average"?`
/// printed after the ` 8 | …` echo instead of before the `ERROR: ` that
/// introduces it. There is no such method now; the order is a property
/// of the value and cannot be restated at a call site.
#[derive(Debug)]
pub struct DbDiagnostic {
    /// C `dbPutString`'s own `fprintf` before it returns the status
    /// (`dbStaticLib.c:2582-2584`) — printed ABOVE the `ERROR:` line.
    pub notice: Option<String>,
    /// The `ERROR:` line itself, and the only part recorded as a fault.
    pub message: String,
    /// C `dbPutStringSuggest`'s proposal — printed BELOW the `ERROR:`
    /// line and ABOVE `yyerror`'s position context.
    pub suggestion: Option<String>,
}

impl DbDiagnostic {
    /// The bare shape: an `ERROR:` line with nothing around it.
    pub fn new(message: String) -> Self {
        Self {
            notice: None,
            message,
            suggestion: None,
        }
    }

    /// The same line with C's `dbPutStringSuggest` proposal under it.
    pub fn suggesting(message: String, suggestion: Option<String>) -> Self {
        Self {
            notice: None,
            message,
            suggestion,
        }
    }
}

/// What the FIRST diagnostic of a text quotes to say WHERE in the line
/// it is — C's ` at or before '%s'` (`dbYacc.y:378`).
#[derive(Debug)]
enum Locator {
    /// C's `yytext`: the token the lexer had last matched. The port's
    /// recoverable diagnostics know it because they are raised from a
    /// grammar position C reduces with a known token — a field's
    /// closing `)`, say.
    Token(String),
    /// The column the parser had reached. C has no counterpart because
    /// its lexer always holds a `yytext`; this port's recursive-descent
    /// parser keeps no such seat, and naming the column it DOES hold is
    /// true where quoting a token it never recorded would be a guess.
    Column(usize),
}

#[derive(Debug, Default)]
pub struct DbFaults {
    messages: Vec<String>,
    /// C's lexer state that `yyerror` reads — `pinputFileNow` and
    /// `my_buffer` (`dbYacc.y:374-381`) — for the whole flattened text.
    source: Option<source::DbSource>,
    /// Where in [`Self::source`] a diagnostic is currently about, i.e.
    /// where C's `pinputFileNow` would be parked, together with the
    /// locator that names the spot inside the line.
    ///
    /// ONE field, not a line beside a token, because the two must agree:
    /// a line from one diagnostic wearing another's token is a position
    /// that points nowhere, and a caller that could set them apart would
    /// eventually set exactly that. `None` means the position is unknown
    /// and the context is omitted — this port emits some diagnostics
    /// after the parse has finished, and a stale line number is worse
    /// for the operator than no line number, so the position is CLEARED
    /// at the end of the parse rather than left to decay. See
    /// [`Self::seek`].
    park: Option<(u32, Locator)>,
    /// C's `yyFailed` (`dbYacc.y:377-380`): ` at or before` and the
    /// include stack are printed for the FIRST diagnostic of a text
    /// only, and every one after it gets the line echo alone.
    yy_failed: bool,
}

impl DbFaults {
    /// Bind the text a diagnostic will be located in — the port's
    /// standing-in for C's lexer holding `inputFileList` and `my_buffer`
    /// as it reads.
    pub fn bind_source(&mut self, source: source::DbSource) {
        self.source = Some(source);
    }

    /// C's lexer seat: say where the parser now is, so a diagnostic
    /// raised from here names that line. `yytext` is the token just
    /// consumed, which is what C quotes in ` at or before '%s'`.
    pub fn seek(&mut self, line: u32, yytext: &str) {
        self.park = Some((line, Locator::Token(yytext.to_owned())));
    }

    /// Leave the lexer's seat. Every later diagnostic prints without
    /// position rather than with the last one the parser happened to
    /// hold — the port raises some of them long after the text was read.
    pub fn seek_end(&mut self) {
        self.park = None;
    }

    /// C `yyerror(str)` — the arm with a message of its own
    /// (`dbYacc.y:373-374`), which is how EVERY abort-class rejection of
    /// a `.db` reaches the operator: `yyparse` calls it with `"syntax
    /// error"`, `dbLex.l` calls it with `"Invalid character '%c'"`, and
    /// `yyerrorAbort` calls it before setting `yyAbort`. The position
    /// context that follows is the same `yyerror` body the recoverable
    /// arm prints, so an abort names its file, its line and its source
    /// line exactly as a recovered fault does.
    ///
    /// This port raises an abort as a `CaError` returned from the
    /// parser rather than as a call into the lexer, so the position
    /// travels on the error and is unpacked here. `line == 0` is the
    /// include layer's "this failure has no line in any file" — a
    /// circular `include`, a file that could not be read — and prints
    /// the message alone, which is also what C's `yyerror` degrades to
    /// when `dbIncludePrint` has no frame to walk.
    ///
    /// Reporting HERE is what closes the family: every one of the
    /// parser's abort sites returns through the single exit that calls
    /// this, so none of them can be given the context and none
    /// forgotten, and no caller outside this module is left printing a
    /// `.db` rejection as a bare error string.
    pub fn abort(&mut self, err: &CaError) {
        let message = match err {
            CaError::DbParseError {
                line,
                column,
                message,
            } => {
                // `line: 0` is the include layer saying this failure has
                // no line in any file — a circular `include`, a file
                // that could not be read. The message stands alone then,
                // which is also what C's `yyerror` degrades to when
                // `dbIncludePrint` has no frame to walk.
                self.park = (*line > 0).then_some((*line as u32, Locator::Column(*column)));
                // The `message` field, NOT the error's `Display`: that
                // spells the position a second time, in this port's own
                // words (`DB parse error at line 3, column 6: …`), and
                // the position is what the two lines below say properly.
                message.clone()
            }
            _ => {
                self.park = None;
                err.to_string()
            }
        };
        self.yyerror(Some(&message));
        self.messages.push(message);
    }

    /// C `yyerror(NULL)`: print the diagnostic, remember that the load
    /// failed, and let the caller carry on with the next item.
    ///
    /// The context that follows is `yyerror`'s own body and not an
    /// addition to it: `ERROR: `, then — once per text — ` at or before
    /// '<yytext>'` and `dbIncludePrint`, then always the blank line and
    /// the ` <line> | <source>` echo `"\n %d | %s\n"` produces
    /// (`dbYacc.y:374-381`). Owning it here is what closes the family:
    /// every `.db` diagnostic in this port already reports through this
    /// one function, so none of them can be given the context and none
    /// forgotten.
    pub fn recoverable(&mut self, message: String) {
        self.report(DbDiagnostic::new(message));
    }

    /// [`Self::recoverable`] with the lines C prints AROUND the `ERROR:`
    /// one, in C's own order — see [`DbDiagnostic`].
    pub fn report(&mut self, diagnostic: DbDiagnostic) {
        let DbDiagnostic {
            notice,
            message,
            suggestion,
        } = diagnostic;
        if let Some(notice) = notice {
            eprintln!("{notice}");
        }
        eprintln!("{message}");
        if let Some(suggestion) = suggestion {
            eprintln!("{suggestion}");
        }
        self.yyerror(None);
        self.messages.push(message);
    }

    /// C `yyerror` itself (`dbYacc.y:371-383`), both of its arms.
    ///
    /// `str` is the message it was called WITH, and the arm is the whole
    /// difference in shape: `Some` writes `ERROR: <str>` on a line of
    /// its own and the position below it starts bare, while `None`
    /// writes the bare `ERROR: ` that the position's own first clause
    /// continues — which is where the two spaces in `ERROR:  at or
    /// before` come from. Modelled as C's argument rather than as a flag
    /// of this port's invention, so the two arms cannot drift apart.
    fn yyerror(&mut self, str: Option<&str>) {
        if let Some(str) = str {
            eprintln!("{ERL_ERROR}: {str}");
        }
        // C always has a position here — its lexer is still parked on
        // the line that failed. This port sometimes does not, and then
        // the bare `ERROR: ` is withheld WITH the position it exists to
        // introduce, rather than left dangling at the end of the stream.
        if let Some(context) = self.position_context() {
            if str.is_none() {
                eprint!("{ERL_ERROR}: ");
            }
            eprint!("{context}");
        }
    }

    /// The position half of `yyerror` (`dbYacc.y:377-381`) — everything
    /// after the message slot — or `None` when this port cannot say
    /// where it was.
    fn position_context(&mut self) -> Option<String> {
        let (line, locator) = self.park.as_ref()?;
        let line = *line;
        let clause = match locator {
            Locator::Token(token) => format!(" at or before '{token}'"),
            Locator::Column(column) => format!(" at or before column {column}"),
        };
        let (frames, text) = self.source.as_ref()?.at(line)?;
        let mut out = String::new();
        if !self.yy_failed {
            out.push_str(&clause);
            out.push_str(&source::include_print(frames));
            self.yy_failed = true;
        }
        // C's `my_buffer` still carries the newline `fgets` left on it, so
        // this format's own trailing `\n` is what puts a blank line under
        // the echo. A last line with no newline of its own loses it, in C
        // too.
        out.push_str(&format!("\n {line} | {text}\n"));
        Some(out)
    }

    /// Take over the faults a nested parse layer collected — an
    /// `include`, a `.substitutions` row — so the outermost load still
    /// reports them exactly once.
    ///
    /// The nested layer's source binding and `yyFailed` come with them:
    /// C has ONE lexer for the whole `dbReadCOM`, so a text that already
    /// printed ` at or before` must not print it again from an outer
    /// frame, and an outer frame with no text of its own must be able to
    /// locate a diagnostic in the inner one.
    pub fn absorb(&mut self, other: DbFaults) {
        self.messages.extend(other.messages);
        self.yy_failed |= other.yy_failed;
        if self.source.is_none() {
            self.source = other.source;
        }
    }

    /// Whether anything recoverable was reported.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The first diagnostic this load reported, for a test asserting the
    /// wording.
    ///
    /// `cfg(test)` is the whole point, not a convenience. Every one of these
    /// lines is ALREADY on the operator's stream — [`Self::recoverable`] put
    /// it there — so a production caller that could read the text back would
    /// be one `eprintln!` away from saying it twice, which is exactly what
    /// this type used to let `dbLoadRecords` do. Outside the crate's own
    /// tests there is now no way to obtain the string at all, so the second
    /// print is unwritable rather than merely discouraged. What production
    /// is allowed to learn is [`Self::is_empty`]: whether the load failed.
    #[cfg(test)]
    pub fn first_diagnostic(&self) -> Option<String> {
        match self.messages.split_first() {
            None => None,
            Some((first, [])) => Some(first.clone()),
            Some((first, rest)) => Some(format!("{first} (+{} more)", rest.len())),
        }
    }
}

/// A `menu(name) { choice(id, "string") ... }` declaration
/// (`dbYacc.y:82-101` @`R7.0.10`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DbdMenu {
    pub name: String,
    /// `(identifier, display string)` in declaration order — C keeps the
    /// order because a menu field's wire value is the INDEX into it.
    pub choices: Vec<(String, String)>,
}

/// One `field(NAME, DBF_TYPE) { item(value) ... }` inside a `recordtype`
/// (`dbYacc.y:120-153`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DbdField {
    pub name: String,
    pub dbf_type: String,
    /// Every `item(value)` pair in order. `menu(...)` arrives here under
    /// the literal key `"menu"`, which is what C's own action does
    /// (`dbRecordtypeFieldItem("menu", $3)`, `dbYacc.y:150`).
    pub items: Vec<(String, String)>,
}

/// A `recordtype(name) { ... }` declaration (`dbYacc.y:103-118`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DbdRecordType {
    pub name: String,
    pub fields: Vec<DbdField>,
    /// `%`-prefixed C definition lines, each without its `%`
    /// (`dbLex.l:94-97`: the pattern is `%.*`, so a cdef is a whole line).
    pub cdefs: Vec<String>,
}

/// `device(recordType, linkType, dsetName, "choiceString")`
/// (`dbYacc.y:155-163`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DbdDevice {
    pub record_type: String,
    pub link_type: String,
    pub dset: String,
    pub choice: String,
}

/// `link(key, lsetName)` (`dbYacc.y:172-178`) — the jlink registration.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DbdLinkType {
    pub key: String,
    pub lset: String,
}

/// `variable(name)` or `variable(name, type)` (`dbYacc.y:192-202`). C
/// defaults the type to `int` in the one-argument form.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DbdVariable {
    pub name: String,
    pub dtype: String,
}

/// The `.dbd` declarations a text carries.
///
/// C has ONE grammar: `database_item` (`dbYacc.y:48-63` @`R7.0.10`) lists
/// `record`/`grecord` alongside `menu`, `recordtype`, `device`, `driver`,
/// `link`, `registrar`, `function` and `variable`, and `dbLoadRecords` and
/// `dbLoadDatabase` differ only in which file they hand to the same
/// `yyparse`. A loader that accepts `record` and nothing else therefore
/// rejects text a C IOC loads. Each construct is parsed into its own shape
/// here rather than skipped, so a caller that needs the declaration — the
/// menu choice list behind a `.SCAN` enum, say — has it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DbdDefs {
    pub menus: Vec<DbdMenu>,
    pub record_types: Vec<DbdRecordType>,
    pub devices: Vec<DbdDevice>,
    pub drivers: Vec<String>,
    pub link_types: Vec<DbdLinkType>,
    pub registrars: Vec<String>,
    pub functions: Vec<String>,
    pub variables: Vec<DbdVariable>,
}

impl DbdDefs {
    /// True when the text declared nothing at all — the ordinary `.db`
    /// case.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.menus.is_empty()
            && self.record_types.is_empty()
            && self.devices.is_empty()
            && self.drivers.is_empty()
            && self.link_types.is_empty()
            && self.registrars.is_empty()
            && self.functions.is_empty()
            && self.variables.is_empty()
    }

    /// The menu declared under `name`, if this text declared it.
    #[must_use]
    pub fn menu(&self, name: &str) -> Option<&DbdMenu> {
        self.menus.iter().find(|m| m.name == name)
    }
}

/// Everything one `.db` text yields.
pub struct ParsedDb {
    pub records: Vec<DbRecordDef>,
    /// `breaktable(...)` definitions found in the text (C `dbBreakBody`,
    /// `dbLexRoutines.c`). The IOC builder feeds these into the
    /// breakpoint-table registry so `ai`/`ao` records with `LINR >= 3`
    /// can resolve their linearisation table.
    pub breaktables: Vec<crate::server::cvt_bpt::BrkTable>,
    /// File-scope `alias("record","new")` directives whose target is not
    /// declared in this text. C `dbAlias` (`dbLexRoutines.c:1508`) looks
    /// the target up in `savedPdbbase` — the whole accumulated database
    /// — so only a caller that owns that database can resolve them, and
    /// the parser must not decide their fate on its own.
    pub unresolved_aliases: Vec<(String, String)>,
    /// Recoverable failures this text produced (C `yyerror(NULL)`). The
    /// records above are everything that parsed in spite of them; the
    /// caller turns a non-empty set into the load's non-zero status.
    pub faults: DbFaults,
    /// The `.dbd` declarations the text carried. Empty for an ordinary
    /// `.db`; C accepts both through the same grammar.
    pub dbd: DbdDefs,
}

impl ParsedDb {
    /// Settle the load's status the way C `dbLoadRecords` does
    /// (`dbAccess.c:795-813`): anything `yyerror(NULL)` recovered from
    /// makes the whole load fail, even though the records that parsed
    /// are still carried here. Only a caller that can report a partial
    /// success — iocsh `dbLoadRecords`, which prints the status and
    /// keeps the shell alive — may read [`ParsedDb::faults`] directly;
    /// every caller that returns a plain `CaResult` goes through here so
    /// a diagnostic can never leave stderr with an `Ok` beside it.
    ///
    /// `source` is what C names in `ERROR: Failed to load '%s'`.
    pub fn load_status(self, source: &str) -> CaResult<Self> {
        if self.faults.is_empty() {
            Ok(self)
        } else {
            Err(CaError::DbLoadFailed(source.to_string()))
        }
    }
}

/// What [`ParsedDb::load_status`] names as the source when the text came
/// from memory rather than a file: C only ever loads a named file, so
/// there is no filename to quote.
pub const DB_STRING_SOURCE: &str = "<db string>";

/// C `dbAlias`'s diagnostic for a target no database knows
/// (`dbLexRoutines.c:1509-1513`). C follows it with `yyerror(NULL)`,
/// which sets `yyFailed` and returns 0: the load's status goes
/// non-zero, the parse continues, and everything already read stays.
pub fn unknown_alias_message(alias: &str, target: &str) -> String {
    format!("{ERL_ERROR}: Alias '{alias}' names an unknown record '{target}'")
}

/// Parse an EPICS .db file with macro substitution.
///
/// The records-only entry point: it has no database to resolve a
/// file-scope alias against, so an unmatched one is reported to stderr
/// and dropped. C keeps the records it already read too — callers that
/// own a database ([`crate::server::ioc_builder`], `dbLoadRecords`)
/// take [`ParsedDb::unresolved_aliases`] instead and resolve them.
pub fn parse_db(input: &str, macros: &HashMap<String, String>) -> CaResult<Vec<DbRecordDef>> {
    let parsed = parse_db_with_breaktables(input, macros)?.load_status(DB_STRING_SOURCE)?;
    for (target, alias) in &parsed.unresolved_aliases {
        eprintln!("{}", unknown_alias_message(alias, target));
    }
    Ok(parsed.records)
}

/// C `dbQuietMacroWarnings` (`dbLexRoutines.c:58`), the `int` the `.db`
/// loader hands `macSuppressWarning` (`:273`).
///
/// A process global because C's is one, and because the only thing that
/// writes it is iocsh `var` — a per-load argument would let two loads in
/// one `st.cmd` disagree with the single knob the operator set.
static DB_QUIET_MACRO_WARNINGS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Read the knob. C default is `0`, i.e. warnings ON.
pub fn db_quiet_macro_warnings() -> bool {
    DB_QUIET_MACRO_WARNINGS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Write the knob — iocsh `var dbQuietMacroWarnings <n>`.
pub fn set_db_quiet_macro_warnings(quiet: bool) {
    DB_QUIET_MACRO_WARNINGS.store(quiet, std::sync::atomic::Ordering::Relaxed);
}

/// C `db_yyinput` (`dbLexRoutines.c:368-410`), as much of it as a port
/// that flattens before it parses can have.
///
/// C reads one `fgets` line, hands it to `macExpandString`, and warns
/// when the expansion reported an error — so both macro diagnostics an
/// operator sees are per LINE and neither is raised by the parser. This
/// is that seat: it expands line by line (quote state cannot leak across
/// a newline), raises the loader's own warning for a line macLib could
/// not fully resolve, and returns the flattened text together with the
/// [`DbSource`] that maps a line of it back to the file and text C would
/// have named.
///
/// The warning fires on C's `entry->error`, not on an undefined name:
/// `macExpandString` returns the same negative length for a recursive
/// reference (`macCore.c:895-904`), and that arm IS reachable from a
/// `.db` load — `dbLoadTemplate` hands a row's `A="$(B)", B="$(A)"`
/// straight to `dbLoadRecords`, where the two raw values walk into each
/// other on the first expansion. Measured against `softIoc R7.0.10`:
/// three lines C wrote that this used to swallow, now
/// [`MacroExpansion::recursive`] and the notice in [`refer`].
///
/// The earlier reading that no such input existed came from testing
/// through the iocsh command line, where the shell's own expansion
/// consumes `$(…)` before `dbLoadRecords` is called — `A=$(B),B=$(A)`
/// there is two UNDEFINED references, not a cycle.
///
/// The unterminated arm (`macCore.c:865-875`) is a separate line C
/// prints, `macLib: unterminated macro reference in …`, and this
/// expander still has none: `refer` returns `None` and the caller copies
/// the `$` through. UNFIXED, and untested against C either way.
///
/// `filename` is `None` for text that never came from a file — C has no
/// such path (`dbReadDatabase` always names one), so the per-file warning
/// is simply not raised rather than invented with a placeholder name.
pub(crate) fn db_read_lines(
    text: &str,
    macros: &HashMap<String, String>,
    filename: Option<&str>,
) -> (String, DbSource) {
    let mut out = String::with_capacity(text.len());
    let mut lines = Vec::new();
    let mut frames = Vec::new();
    for (i, raw) in text.split_inclusive('\n').enumerate() {
        let line_num = i as u32 + 1;
        let expanded = db_expand_line(raw, macros, filename, line_num);
        out.push_str(&expanded);
        lines.push(expanded);
        frames.push(std::sync::Arc::from(vec![DbIncludeFrame {
            path: None,
            filename: filename.map(str::to_string),
            line: line_num,
        }]));
    }
    (out, DbSource::new(lines, frames))
}

/// One line through C's `macExpandString` call and the warning that
/// follows it (`dbLexRoutines.c:378-387`) — the single seat for both
/// macro diagnostics a `.db` load produces.
///
/// `raw` must carry its own trailing newline exactly as `fgets` leaves
/// it, because macLib quotes the string it was given: C's console shows
/// the closing `)` of `(expanding string …)` on the NEXT line for that
/// reason, and a caller that trimmed first would print a different shape.
pub(crate) fn db_expand_line(
    raw: &str,
    macros: &HashMap<String, String>,
    filename: Option<&str>,
    line_num: u32,
) -> String {
    let opts = MacroExpandOptions {
        suppress_warnings: db_quiet_macro_warnings(),
        ..MacroExpandOptions::default()
    };
    let expansion = expand_macros(raw, macros, opts);
    // C `entry->error`, not "an undefined name": `macExpandString`
    // returns the same negative length for a recursive reference, and
    // this warning is what tells the operator the line went out wrong.
    if expansion.errored()
        && let Some(name) = filename
    {
        // C `db_yyinput` (`dbLexRoutines.c:384-387`), a direct `fprintf`
        // — so its magenta reaches a pipe as well as a terminal, unlike
        // the `macLib:` line above it, which errlog strips. C passes
        // `line_num+1` because its own counter is bumped after the read;
        // the count here is the reader's own and already 1-based.
        //
        // NOT gated on `dbQuietMacroWarnings`: `trans` sets
        // `entry->error` before it consults `FLAG_SUPPRESS_WARNINGS`
        // (`macCore.c:912-919`), so `macExpandString` still returns a
        // negative length and this line still prints. Measured on
        // `compressTest.db` with `var dbQuietMacroWarnings 1`: C drops
        // the four `macLib:` notices and keeps all four of these.
        eprintln!(
            "{}: '{name}' line {line_num} has undefined macros",
            crate::runtime::log::ERL_WARNING
        );
    }
    expansion.text
}

/// Like [`parse_db`] but returns the whole [`ParsedDb`].
pub fn parse_db_with_breaktables(
    input: &str,
    macros: &HashMap<String, String>,
) -> CaResult<ParsedDb> {
    // Per LINE, not per file: C's loader expands each `fgets` line
    // separately (`dbLexRoutines.c:375-391`), so quote state cannot leak
    // across lines — see [`db_read_lines`].
    let (expanded, source) = db_read_lines(input, macros, None);
    parse_db_expanded(&expanded, source)
}

/// The parser proper: text that has ALREADY been through
/// [`db_read_lines`], plus the map that locates a line in it.
///
/// Split out so a `.db` text is macro-expanded exactly ONCE per load.
/// The file path expanded twice — once in `expand_includes`, which must,
/// because it has to resolve an `include` filename before it can open it,
/// and again here — which was invisible while the expander said nothing
/// and would have printed every `macLib:` line twice the moment it did.
pub(crate) fn parse_db_expanded(expanded: &str, source: DbSource) -> CaResult<ParsedDb> {
    // The ONE place a `.db` abort meets the text it came from. C reaches
    // the same point through `yyerror(str)` from inside the lexer; this
    // parser returns instead, so the return is where the report has to
    // happen — and it is a single return, so every abort site the
    // grammar has gets the position at once. See [`DbFaults::abort`].
    let mut faults = DbFaults::default();
    faults.bind_source(source);
    match parse_db_items(expanded, &mut faults) {
        Ok(items) => Ok(ParsedDb {
            records: items.records,
            breaktables: items.breaktables,
            unresolved_aliases: items.unresolved_aliases,
            faults,
            dbd: items.dbd,
        }),
        Err(e) => {
            faults.abort(&e);
            // C follows the abort with `WARNING: dbReadCOM: Parser stack
            // dirty w/o error. <n>` when its `tempList` still holds the
            // half-built objects `yyparse` abandoned
            // (`dbLexRoutines.c:303-305`). Deliberately absent: it counts
            // C's own allocations, this parser builds its records on the
            // stack and has no such list, and printing it would state a
            // fact about a structure that does not exist here.
            Err(e)
        }
    }
}

/// What one text yields before its faults are attached.
///
/// Separate from [`ParsedDb`] because the reporter cannot be a field of
/// the result while the parse is still running: it is the parse's own
/// output channel, and an abort has to report through it AFTER the body
/// has given up on producing a result at all.
struct DbItems {
    records: Vec<DbRecordDef>,
    breaktables: Vec<crate::server::cvt_bpt::BrkTable>,
    unresolved_aliases: Vec<(String, String)>,
    dbd: DbdDefs,
}

/// The grammar itself.
///
/// `dbYacc.y` declares no `error` productions, so a genuine syntax error
/// ends `yyparse` and the whole text is lost in C too: every `?` below
/// is abort-class by C's own choice, and all of them leave through
/// [`parse_db_expanded`], which reports them. Only the semantic actions
/// C guards with `yyerror(NULL)` are recoverable, and those live in the
/// layers that own a database — see [`DbFaults`].
fn parse_db_items(expanded: &str, faults: &mut DbFaults) -> CaResult<DbItems> {
    let mut records = Vec::new();
    let mut breaktables: Vec<crate::server::cvt_bpt::BrkTable> = Vec::new();
    // `.dbd` declarations: C parses these through the same `yyparse` as
    // `record`, so they are accepted here rather than rejected.
    let mut dbd = DbdDefs::default();
    // Standalone `alias("record","newname")` directives (dbYacc.y:275).
    // Resolved against the record list after the full file is parsed
    // so the alias target may appear before or after the directive.
    let mut global_aliases: Vec<(String, String)> = Vec::new();
    let chars: Vec<char> = expanded.chars().collect();
    let mut pos = 0;
    let mut line = 1;
    let mut col = 1;

    while pos < chars.len() {
        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        if pos >= chars.len() {
            break;
        }

        // Top-level keyword. C dbStatic (`dbYacc.y:48-62`) accepts
        // `record`/`grecord` plus the directives below at file scope.
        let word = read_word(&chars, &mut pos, &mut col);
        if word.is_empty() {
            pos += 1;
            col += 1;
            continue;
        }

        // `path "dir"` / `addpath "dir"` — search-path directives
        // (dbYacc.y:71-81). Include resolution is handled by the
        // file-expansion layer (`expand_includes`); by the time raw
        // text reaches `parse_db` the path is already fixed, so these
        // are accepted and skipped rather than erroring out.
        if word == "path" || word == "addpath" {
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let _dir = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
            continue;
        }

        // `include "file"` (dbYacc.y:65-69). Includes are normally
        // inlined by `expand_includes` before `parse_db` runs; a bare
        // `include` reaching here means the caller parsed un-expanded
        // text. Accept the directive so the grammar matches C, but the
        // file is NOT loaded at this layer — that is the expansion
        // layer's job.
        if word == "include" {
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let _file = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
            continue;
        }

        // Standalone 2-arg `alias("record","newname")` (dbYacc.y:275).
        // Distinct from the in-record-body `alias("name")` form. The
        // new name is attached to the named record's alias list once
        // all records are parsed (the target may appear later in the
        // file).
        if word == "alias" {
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, '(', line)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let target = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, ',', line)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let alias_name = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
            validate_record_name(&alias_name, line, col)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, ')', line)?;
            global_aliases.push((target, alias_name));
            continue;
        }

        // `breaktable(name) { raw eng  raw eng  ... }` (dbYacc.y / C
        // `dbBreakBody`, dbLexRoutines.c:1003-1080). The body is a flat list
        // of numbers in `(raw, eng)` pairs, whitespace- or comma-separated;
        // `BrkTable::build` computes the interval slopes and validates the
        // table (>= 2 points, monotonic, no zero slope).
        if word == "breaktable" {
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, '(', line)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let bt_name = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, ')', line)?;
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, '{', line)?;

            // Read numeric tokens until the closing brace. Numbers are
            // separated by whitespace and/or commas (C `tokenVALUE` lexing).
            let is_number_char =
                |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | ':' | '.');
            let mut nums: Vec<f64> = Vec::new();
            loop {
                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                if pos >= chars.len() {
                    return Err(CaError::DbParseError {
                        line,
                        column: col,
                        message: "unexpected end of file in breaktable body".into(),
                    });
                }
                if chars[pos] == '}' {
                    pos += 1;
                    col += 1;
                    break;
                }
                if chars[pos] == ',' {
                    pos += 1;
                    col += 1;
                    continue;
                }
                let start = col;
                let mut tok = String::new();
                while pos < chars.len() && is_number_char(chars[pos]) {
                    tok.push(chars[pos]);
                    pos += 1;
                    col += 1;
                }
                if tok.is_empty() {
                    return Err(CaError::DbParseError {
                        line,
                        column: col,
                        message: format!(
                            "breaktable {bt_name}: expected a number, got '{}'",
                            chars[pos]
                        ),
                    });
                }
                let num: f64 = tok.parse().map_err(|_| CaError::DbParseError {
                    line,
                    column: start,
                    message: format!("breaktable {bt_name}: non-numeric value '{tok}'"),
                })?;
                nums.push(num);
            }

            // C `dbBreakBody`: an odd count is a missing raw/eng partner.
            if nums.len() % 2 != 0 {
                return Err(CaError::DbParseError {
                    line,
                    column: col,
                    message: format!("breaktable {bt_name}: Raw value missing"),
                });
            }
            let pairs: Vec<(f64, f64)> = nums.chunks_exact(2).map(|c| (c[0], c[1])).collect();
            let table = crate::server::cvt_bpt::BrkTable::build(bt_name, &pairs).map_err(|e| {
                CaError::DbParseError {
                    line,
                    column: col,
                    message: e,
                }
            })?;
            breaktables.push(table);
            continue;
        }

        // The `.dbd` half of C's one grammar (`dbYacc.y:51-59, 155-202`).
        // `menu` and `recordtype` carry a brace body; the rest are a
        // parenthesised argument list.
        if word == "menu" {
            dbd.menus
                .push(parse_menu(&chars, &mut pos, &mut line, &mut col)?);
            continue;
        }
        if word == "recordtype" {
            dbd.record_types
                .push(parse_recordtype(&chars, &mut pos, &mut line, &mut col)?);
            continue;
        }
        if word == "device" {
            let a = parse_arg_list(&chars, &mut pos, &mut line, &mut col, 4, 4, "device")?;
            dbd.devices.push(DbdDevice {
                record_type: a[0].clone(),
                link_type: a[1].clone(),
                dset: a[2].clone(),
                choice: a[3].clone(),
            });
            continue;
        }
        if word == "driver" {
            let a = parse_arg_list(&chars, &mut pos, &mut line, &mut col, 1, 1, "driver")?;
            dbd.drivers.push(a[0].clone());
            continue;
        }
        if word == "link" {
            let a = parse_arg_list(&chars, &mut pos, &mut line, &mut col, 2, 2, "link")?;
            dbd.link_types.push(DbdLinkType {
                key: a[0].clone(),
                lset: a[1].clone(),
            });
            continue;
        }
        if word == "registrar" {
            let a = parse_arg_list(&chars, &mut pos, &mut line, &mut col, 1, 1, "registrar")?;
            dbd.registrars.push(a[0].clone());
            continue;
        }
        if word == "function" {
            let a = parse_arg_list(&chars, &mut pos, &mut line, &mut col, 1, 1, "function")?;
            dbd.functions.push(a[0].clone());
            continue;
        }
        if word == "variable" {
            // C `dbYacc.y:192-202`: the one-argument form is
            // `dbVariable($3, "int")`.
            let a = parse_arg_list(&chars, &mut pos, &mut line, &mut col, 1, 2, "variable")?;
            dbd.variables.push(DbdVariable {
                name: a[0].clone(),
                dtype: a.get(1).cloned().unwrap_or_else(|| "int".to_string()),
            });
            continue;
        }

        if word != "record" && word != "grecord" {
            return Err(CaError::DbParseError {
                line,
                column: col,
                message: format!("expected 'record', got '{word}'"),
            });
        }

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        expect_char(&chars, &mut pos, &mut col, '(', line)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        let rec_type = read_token_string(&chars, &mut pos, &mut line, &mut col)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        expect_char(&chars, &mut pos, &mut col, ',', line)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        let name = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
        validate_record_name(&name, line, col)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        expect_char(&chars, &mut pos, &mut col, ')', line)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        let mut fields = Vec::new();
        let mut aliases: Vec<String> = Vec::new();
        let mut info_tags: Vec<(String, String)> = Vec::new();
        // C `dbYacc.y:237-241`: `record_body: /* empty */` is the FIRST
        // alternative, ahead of `'{' '}'` and `'{' record_field_list '}'`,
        // and `tokenRECORD` and `tokenGRECORD` share it — so
        // `record(ai,"TEMP")` with no braces is an all-defaults record,
        // not a syntax error. `record`/`grecord` reach this through the
        // same path above, so both forms are covered here.
        if pos < chars.len() && chars[pos] == '{' {
            pos += 1;
            col += 1;
            loop {
                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                if pos >= chars.len() {
                    return Err(CaError::DbParseError {
                        line,
                        column: col,
                        message: "unexpected end of file in record body".into(),
                    });
                }
                if chars[pos] == '}' {
                    pos += 1;
                    col += 1;
                    break;
                }

                let kw = read_word(&chars, &mut pos, &mut col);
                // `record_field: ... | include` (`dbYacc.y:271`). As at
                // file scope, the expansion layer owns the file; the
                // directive is accepted so the grammar matches C.
                if kw == "include" {
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    let _file = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
                    continue;
                }
                if kw != "field" && kw != "info" && kw != "alias" {
                    return Err(CaError::DbParseError {
                        line,
                        column: col,
                        message: format!("expected 'field', got '{kw}'"),
                    });
                }

                if kw == "alias" {
                    // alias("name") — capture and validate per PR #336.
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    expect_char(&chars, &mut pos, &mut col, '(', line)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    let alias_name = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
                    validate_record_name(&alias_name, line, col)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    expect_char(&chars, &mut pos, &mut col, ')', line)?;
                    aliases.push(alias_name);
                    continue;
                }
                if kw == "info" {
                    // info(tag, value) — capture for downstream consumers
                    // (asyn:READBACK, Q:form, etc.). PR #60 / #208 needs
                    // the asyn:READBACK tag in particular.
                    //
                    // Both `tag` and `value` accept quoted *or* unquoted
                    // tokens. Base's dbStaticLib parser tolerates either
                    // form and ad-core templates rely on the unquoted
                    // shape (`info(asyn:READBACK, "1")`).
                    //
                    // C `info(tokenSTRING, json_value)` (dbYacc.y:262-267): the tag
                    // is a `tokenSTRING` — the same raw, untranslated token a record
                    // NAME is — and only the value is a `jsonSTRING` that
                    // `dbRecordInfo` runs `dbTranslateEscape` over
                    // (dbLexRoutines.c:1435-1440). Hence the two different readers.
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    expect_char(&chars, &mut pos, &mut col, '(', line)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    let tag = read_token_string(&chars, &mut pos, &mut line, &mut col)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    expect_char(&chars, &mut pos, &mut col, ',', line)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    let value = read_field_value(&chars, &mut pos, &mut line, &mut col)?;
                    skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                    expect_char(&chars, &mut pos, &mut col, ')', line)?;
                    info_tags.push((tag, value.as_str_lossy().into_owned()));
                    continue;
                }

                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                expect_char(&chars, &mut pos, &mut col, '(', line)?;

                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                let field_name = read_token_string(&chars, &mut pos, &mut line, &mut col)?;

                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                expect_char(&chars, &mut pos, &mut col, ',', line)?;

                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                let field_value = read_field_value(&chars, &mut pos, &mut line, &mut col)?;

                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                expect_char(&chars, &mut pos, &mut col, ')', line)?;
                // C reduces `dbRecordField` with the closing paren as the
                // last token the lexer matched, which is why every field
                // diagnostic in C's console quotes ` at or before ')'`.
                let field_line = line as u32;
                faults.seek(field_line, ")");

                // C `dbRecordField` (`dbLexRoutines.c:1231-1416` @`R7.0.10`)
                // asks the record type what it declares BEFORE it stores
                // anything, and both of its name-side refusals end in
                // `yyerror(NULL)`: the field is dropped, the record and its
                // other fields survive, and the load's status goes non-zero.
                // The port had neither refusal, so `field(OUT,...)` on a calc
                // record loaded and `dbgf C:TWO.OUT` answered the raw text —
                // an invalid database that reads back plausibly, which is
                // worse than the parse error it replaced. This is the one
                // place the check can live: `apply_fields` resolves every
                // record type but never learns the record's NAME, and the
                // message is built from it.
                match record_type_field_exists(&rec_type, &field_name) {
                    Some(false) => {
                        // C follows the refusal with a spelling suggestion when
                        // it finds one, and prints NOTHING when it does not
                        // (`dbLexRoutines.c:1241-1385`). The silence is the half
                        // that has to be reproduced exactly: a suggestion the
                        // operator did not mean is worse than none.
                        //
                        // C reads the value BEFORE `dbTranslateEscape`, so its
                        // type test sees `\t` as two characters where this sees
                        // the tab. Nothing in the map or in the numeric and
                        // choice tests can be spelled with an escape, so the
                        // two agree on every value that changes a suggestion.
                        faults.report(DbDiagnostic::suggesting(
                            format!(
                                "{ERL_ERROR}: {rec_type} record '{name}' doesn't have a field \
                                 '{field_name}'"
                            ),
                            suggest_field(&rec_type, &field_name, &field_value.as_str_lossy())
                                .map(suggestion_line),
                        ));
                        continue;
                    }
                    // `indfield == 0` is `NAME`, dbCommon's first field
                    // (`dbLexRoutines.c:1390-1396`). `dbFindField` finds it,
                    // so this is a second, separately-worded refusal rather
                    // than a missing declaration.
                    Some(true) if field_name == "NAME" => {
                        faults.recoverable(format!(
                            "{ERL_ERROR}: Can't set 'NAME' field of record '{name}'"
                        ));
                        continue;
                    }
                    _ => {}
                }
                fields.push(DbFieldDef {
                    name: field_name,
                    value: field_value,
                    line: field_line,
                });
            }
        }

        records.push(DbRecordDef {
            record_type: rec_type,
            name,
            fields,
            aliases,
            info_tags,
        });
    }

    // Attach standalone `alias("record","newname")` directives to the
    // matching record (C `dbAlias`). A target this text does not
    // declare is NOT an error here: C resolves it against
    // `savedPdbbase`, so an earlier `dbLoadRecords` may own it. Hand it
    // to the caller, which is the only party that can see the database.
    let mut unresolved_aliases = Vec::new();
    for (target, alias_name) in global_aliases {
        match records.iter_mut().find(|r| r.name == target) {
            Some(rec) => rec.aliases.push(alias_name),
            None => unresolved_aliases.push((target, alias_name)),
        }
    }

    // C's lexer is done with this text; a diagnostic raised after this
    // point (the install loop, an alias resolved against the database)
    // must seek for itself rather than inherit wherever the parse
    // stopped. See [`DbFaults::seek_end`].
    faults.seek_end();

    Ok(DbItems {
        records,
        breaktables,
        unresolved_aliases,
        dbd,
    })
}

/// Resolution options for the macLib expansion engine ([`expand_macros`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct MacroExpandOptions {
    /// Fall back to the process environment when a name is unset,
    /// matching C `macCreateHandle(&h, environ)`. The `.db` parser
    /// leaves this off (its substitutions come only from the
    /// `dbLoadRecords` / `.substitutions` macro map); `dbLoadGroup`
    /// and autosave turn it on.
    pub env_fallback: bool,
    /// Treat `$$` as a literal `$`. An autosave `.req` convenience, NOT
    /// a macLib behavior — C macLib leaves `$$` verbatim (`$` is only
    /// special before `(`/`{`), so the `.db` parser leaves it off.
    pub dollar_escape: bool,
    /// C `macSuppressWarning` / `FLAG_SUPPRESS_WARNINGS`
    /// (`macCore.c:155-168`). It silences the `macLib:` diagnostics AND
    /// changes the text: a suppressed undefined reference is written back
    /// as `$(name)` where a warned one is `$(name,undefined)`
    /// (`refer`, `macCore.c:920-928`). Off is C's default; the `.db`
    /// loader turns it on from `dbQuietMacroWarnings`
    /// (`dbLexRoutines.c:58`, `:273`) and `msi` turns it on outright
    /// (`msi.cpp:154`).
    pub suppress_warnings: bool,
}

/// Outcome of [`expand_macros`]: the expanded text, plus the fault that
/// made it wrong if one did.
///
/// Two faults reach here — a name with no definition and a name that
/// resolves into itself — and they are C's single `entry->error`
/// (`macCore.c:216-224`) split by cause. Both lists are therefore
/// private, and the only way to read them is [`Self::fault`], or
/// [`Self::errored`] for the same answer as a bool. A consumer able to
/// reach one list directly hard-fails on the fault it named and returns
/// the other one's broken text as success; that is not hypothetical, it
/// is what the autosave `.req` reader did for as long as `undefined`
/// was `pub`.
#[derive(Clone, Debug, Default)]
pub struct MacroExpansion {
    pub text: String,
    /// Every macro referenced with neither a definition (nor an env
    /// value, when `env_fallback`) nor a default. The text still carries
    /// C's `$(name,undefined)` placeholder for each
    /// (`refer:errval = ",undefined)"`).
    undefined: Vec<String>,
    /// Every macro this expansion refused to resolve because resolving
    /// it re-entered itself — C `refer` finding `refentry->visited`
    /// already set (`macCore.c:895-904`). Separate from
    /// [`Self::undefined`] because the two are different faults with
    /// different placeholders, and a caller that hard-fails on an
    /// undefined name must not be told a recursive one was undefined.
    recursive: Vec<String>,
}

/// Which fault an expansion hit, and the first macro name that hit it.
/// One variant per placeholder C writes, so a caller cannot report a
/// recursion as an undefined name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacroFault<'a> {
    /// No definition, no env value, no default — C's
    /// `$(name,undefined)` placeholder (`macCore.c:913-928`).
    Undefined(&'a str),
    /// Resolving the name re-entered itself — C's `refentry->visited`
    /// guard (`macCore.c:895-904`).
    Recursive(&'a str),
}

impl MacroExpansion {
    /// The single owner of "did this expansion come out wrong, and
    /// why". Both arms are read here so that no caller can read only
    /// one.
    ///
    /// A macro is never in both lists: `recursive` is recorded only
    /// where the name resolved to a table entry, `undefined` only where
    /// it resolved to nothing. So the order below decides nothing but
    /// which of two independent faults a string carrying both reports
    /// first.
    #[must_use]
    pub fn fault(&self) -> Option<MacroFault<'_>> {
        if let Some(name) = self.undefined.first() {
            return Some(MacroFault::Undefined(name.as_str()));
        }
        self.recursive
            .first()
            .map(|name| MacroFault::Recursive(name.as_str()))
    }

    /// C `MAC_ENTRY.error`: whether the expansion came out wrong, by any
    /// of the arms that set it. `macExpandString` returns a negative
    /// length for all of them alike (`macCore.c:216-224`), and that
    /// negative length is the whole of what the `.db` reader's per-line
    /// warning and `macDefExpand`'s `NULL` are looking at. Delegates to
    /// [`Self::fault`] so the bool and the name can never disagree.
    #[must_use]
    pub fn errored(&self) -> bool {
        self.fault().is_some()
    }
}

/// Engine state threaded through [`trans`] / [`refer`] / [`parse_scoped`]:
/// the base macro map, the resolution options, and the running list of
/// undefined-macro names.
struct ExpandCtx<'a> {
    macros: &'a HashMap<String, String>,
    opts: MacroExpandOptions,
    undefined: Vec<String>,
    recursive: Vec<String>,
    /// C's `MAC_ENTRY.name` for this expansion, which every `macLib:`
    /// warning quotes. `macExpandString` fills in the source string
    /// itself and the type `"string"` (`macCore.c:208-209`), and `trans`
    /// hands the SAME entry down every recursion (`:849`, `:889`,
    /// `:909`), so a warning raised while expanding a macro's value
    /// still names the line the caller passed in.
    expanding: &'a str,
}

/// Expand `$(...)` / `${...}` macro references, mirroring the C `macLib`
/// engine (`modules/libcom/src/macLib/macCore.c` `trans` / `refer`).
/// This is the single macLib implementation for the crate; the `.db`
/// parser, `dbLoadGroup`, and autosave all route through it (with
/// per-caller [`MacroExpandOptions`]) rather than re-implementing it.
/// Implemented behaviors:
///
///   - `\<char>` blocks macro detection; both bytes reach the output in
///     the caller's own string, and the backslash is dropped from
///     anything that arrived through a macro (`trans:701-703,739-744`;
///     `macLib.plt:52`).
///   - macros are NOT expanded inside single quotes (`trans:733-736`);
///     the quote characters themselves survive only at level 0.
///   - a reference name is itself macro-expanded before lookup
///     (`refer` runs `trans` on the name — `$($(WHICH))`).
///   - the name terminates at `=`, `,` or the closing bracket
///     (`macEnd = "=,)"`); `,name=val` introduces scoped macro
///     definitions visible only inside that reference's expansion.
///   - a resolved macro value is re-scanned for further `$(...)`
///     (chained expansion); a self-/mutually-referential macro stops
///     at the `visiting` guard (C per-entry `visited`).
///   - an undefined macro with no default emits the placeholder
///     `$(name,undefined)` (`refer:errval = ",undefined)"`) and comes
///     back as [`MacroFault::Undefined`].
///   - with [`MacroExpandOptions::env_fallback`], an otherwise-unset
///     name resolves from the process environment before the default
///     (C `macCreateHandle(&h, environ)`).
pub fn expand_macros(
    input: &str,
    macros: &HashMap<String, String>,
    opts: MacroExpandOptions,
) -> MacroExpansion {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut ctx = ExpandCtx {
        macros,
        opts,
        undefined: Vec::new(),
        recursive: Vec::new(),
        expanding: input,
    };
    trans(
        &chars,
        0,
        &mut ctx,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut out,
    );
    MacroExpansion {
        text: out,
        undefined: ctx.undefined,
        recursive: ctx.recursive,
    }
}

/// Expand `$(...)` / `${...}` macro references with the default
/// macLib options (no env fallback, no `$$` escape, undefined →
/// placeholder). Thin wrapper over [`expand_macros`]; `pub` so
/// `dbLoadGroup` and other consumers reuse the one engine instead of
/// re-implementing macLib.
pub fn substitute_macros(input: &str, macros: &HashMap<String, String>) -> String {
    expand_macros(input, macros, MacroExpandOptions::default()).text
}

/// [`substitute_macros`], one line at a time — how C's file readers feed
/// macLib.
///
/// `dbLoadRecords` hands `macExpandString` a single `fgets` line
/// (`dbLexRoutines.c:375-391`), and `asInitFile` does the same
/// (`asLibRoutines.c:202-219`), so the expander's quote tracking (`trans`'s
/// `quote`, C `macCore.c`) resets at every newline. Expanding a whole file
/// in one [`substitute_macros`] call let one line's quote state leak into
/// the next: an apostrophe in a `#` comment opened single-quote suppression
/// and silently disabled every `$(...)` on the lines after it, until the
/// parser failed on an unexpanded record name. Within a line the quote
/// rules are unchanged — `'$(X)'` still suppresses.
pub fn substitute_macros_per_line(input: &str, macros: &HashMap<String, String>) -> String {
    input
        .split_inclusive('\n')
        .map(|line| substitute_macros(line, macros))
        .collect()
}

/// Translate `chars` into `out`, expanding macro references.
///
/// `scopes` is the stack of scoped-macro frames pushed by enclosing
/// `$(name,key=val)` references; lookup walks it innermost-first then
/// falls back to `ctx.macros` (and, when enabled, the environment).
/// `visiting` is the stack of macro names currently being expanded — it
/// guards against a self-referential macro (`A=$(A)`) recursing forever,
/// mirroring C `macCore.c`'s per-entry `visited` flag.
fn trans(
    chars: &[char],
    level: usize,
    ctx: &mut ExpandCtx,
    scopes: &mut Vec<HashMap<String, String>>,
    visiting: &mut Vec<String>,
    out: &mut String,
) {
    // C `macCore.c:701-703`: "discard quotes and escapes if level is > 0
    // (i.e. if these aren't the user's quotes and escapes)". Level 0 is
    // the string the caller handed `macExpandString`; every recursion out
    // of `refer` — a macro's value, a reference name, a default, a scoped
    // definition — runs at `level + 1`, so a quote or a backslash that
    // arrived through a macro is syntax and never reaches the output.
    let discard = level > 0;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Track single/double quote state (C `trans` `quote` var).
        if let Some(q) = quote {
            if c == q {
                quote = None;
                if discard {
                    i += 1;
                    continue;
                }
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            if discard {
                i += 1;
                continue;
            }
        }

        // `$$` → literal `$` (opt-in; autosave `.req` convenience).
        if ctx.opts.dollar_escape && c == '$' && i + 1 < chars.len() && chars[i + 1] == '$' {
            out.push('$');
            i += 2;
            continue;
        }

        // `\<char>`: skip macro detection; the backslash itself is
        // emitted only at level 0 (C `if (v < valend && !discard)`).
        if c == '\\' && i + 1 < chars.len() {
            if !discard {
                out.push('\\');
            }
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }

        // Macro reference: `$` followed by `(` or `{`, and NOT inside
        // single quotes (C `macRef && quote != '\''`).
        let mac_ref =
            c == '$' && i + 1 < chars.len() && (chars[i + 1] == '(' || chars[i + 1] == '{');
        if mac_ref && quote != Some('\'') {
            if let Some(next) = refer(chars, i, level, ctx, scopes, visiting, out) {
                i = next;
                continue;
            }
        }

        out.push(c);
        i += 1;
    }
}

/// Expand one macro reference starting at `chars[start]` (`$`). On
/// success returns the index just past the closing bracket; returns
/// `None` if the reference is unterminated (caller copies `$` raw).
fn refer(
    chars: &[char],
    start: usize,
    level: usize,
    ctx: &mut ExpandCtx,
    scopes: &mut Vec<HashMap<String, String>>,
    visiting: &mut Vec<String>,
    out: &mut String,
) -> Option<usize> {
    let close = if chars[start + 1] == '(' { ')' } else { '}' };
    // Find the matching close bracket, honoring nested `$(`/`${`.
    let body_start = start + 2;
    let mut depth = 1usize;
    let mut j = body_start;
    while j < chars.len() && depth > 0 {
        if j + 1 < chars.len() && chars[j] == '$' && (chars[j + 1] == '(' || chars[j + 1] == '{') {
            depth += 1;
            j += 2;
            continue;
        }
        if depth == 1 && chars[j] == close || depth > 1 && (chars[j] == ')' || chars[j] == '}') {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        j += 1;
    }
    if depth != 0 {
        return None; // unterminated — caller emits '$' literally
    }
    let body = &chars[body_start..j];
    let after = j + 1;

    // Split the body at the first top-level `=` or `,` (the C
    // `macEnd` terminator set). Nested `$(...)` brackets are skipped
    // so a `=`/`,` inside an inner reference does not terminate.
    let split = top_level_terminator(body);
    let (name_chars, rest) = match split {
        Some(k) => (&body[..k], &body[k..]),
        None => (body, &body[body.len()..]),
    };

    // the name itself may contain macro references — expand it.
    let mut name = String::new();
    trans(name_chars, level + 1, ctx, scopes, visiting, &mut name);

    // Default value (`=...`) and scoped definitions (`,k=v`).
    let mut default: Option<&[char]> = None;
    let mut scoped: Vec<(String, String)> = Vec::new();
    if let Some(first) = rest.first() {
        if *first == '=' {
            // Default runs until the first top-level `,` or end.
            let dflt = &rest[1..];
            let dsplit = top_level_comma(dflt);
            match dsplit {
                Some(k) => {
                    default = Some(&dflt[..k]);
                    parse_scoped(&dflt[k..], level, ctx, scopes, visiting, &mut scoped);
                }
                None => default = Some(dflt),
            }
        } else if *first == ',' {
            parse_scoped(rest, level, ctx, scopes, visiting, &mut scoped);
        }
    }

    // Push the scoped frame (visible only inside this expansion).
    let mut frame: HashMap<String, String> = HashMap::new();
    for (k, v) in scoped {
        frame.insert(k, v);
    }
    scopes.push(frame);

    // Look up: innermost scope first, then base macros, then (when
    // enabled) the process environment — C `macCreateHandle(&h, environ)`
    // installs env entries as macros, so env resolution sits at the same
    // level as a defined macro, before any default.
    let resolved = scopes
        .iter()
        .rev()
        .find_map(|s| s.get(&name).cloned())
        .or_else(|| ctx.macros.get(&name).cloned())
        .or_else(|| {
            if ctx.opts.env_fallback {
                crate::runtime::env::get(&name)
            } else {
                None
            }
        });

    match resolved {
        Some(val) => {
            if visiting.contains(&name) {
                // C `refer` finding `refentry->visited` already set
                // (`macCore.c:895-904`): the reference is refused, NOT
                // resolved. The port used to emit the value verbatim,
                // which broke the cycle but left the operator with a
                // silently half-expanded `.db` — measured against
                // `softIoc` on `A="$(B)", B="$(A)"` through a
                // `.substitutions`, C wrote three lines here and this
                // wrote none.
                //
                // `visiting.last()` is C's `entry` — the macro whose raw
                // value is being translated when the cycle closes — and
                // `name` is its `refentry`, which is why C's own output
                // for that pair reads `macro A is recursive (expanding
                // macro B)`.
                //
                // C detects this once per macro TABLE entry, in the
                // `expand()` pass it runs when the table is dirty
                // (`macCore.c:646-679`); this expander resolves lazily
                // and so reports once per reference. Same cycle, same
                // sentence, and the count follows the engine.
                ctx.recursive.push(name.clone());
                if !ctx.opts.suppress_warnings {
                    let recursive = if crate::runtime::log::errlog_console_paints() {
                        "\x1b[35;1mrecursive\x1b[0m"
                    } else {
                        "recursive"
                    };
                    let entry = visiting.last().map_or("", String::as_str);
                    crate::runtime::log::errlog_printf(&format!(
                        "macLib: macro {entry} is {recursive} (expanding macro {name})\n"
                    ));
                }
                out.push('$');
                out.push('(');
                out.push_str(&name);
                // Same knob, same two texts as the undefined arm
                // (`macCore.c:920-928`).
                if ctx.opts.suppress_warnings {
                    out.push(')');
                } else {
                    out.push_str(",recursive)");
                }
            } else {
                // re-scan the resolved value for further refs.
                visiting.push(name.clone());
                let val_chars: Vec<char> = val.chars().collect();
                trans(&val_chars, level + 1, ctx, scopes, visiting, out);
                visiting.pop();
            }
        }
        None => match default {
            Some(def_chars) => {
                // C `refer` translates the default at `level + 1`
                // (`macCore.c:909`), so every quote in it is discarded —
                // not just a surrounding pair.
                trans(def_chars, level + 1, ctx, scopes, visiting, out);
            }
            None => {
                // L-4: undefined macro placeholder. Record the name so
                // a hard-fail caller reaches it through
                // `MacroExpansion::fault`, instead of accepting the
                // placeholder as text.
                ctx.undefined.push(name.clone());
                if !ctx.opts.suppress_warnings {
                    // C `refer` (`macCore.c:913-917`), through
                    // `errlogPrintf` and not `fprintf` — which is why the
                    // magenta on `undefined` follows the console while the
                    // `ERROR`/`WARNING` words of the `.db` loader do not:
                    // errlog strips escapes when its console is not a
                    // terminal (`errlog.c:672-681`) and a direct `fprintf`
                    // never enters that pump.
                    let undefined = if crate::runtime::log::errlog_console_paints() {
                        "\x1b[35;1mundefined\x1b[0m"
                    } else {
                        "undefined"
                    };
                    crate::runtime::log::errlog_printf(&format!(
                        "macLib: macro {name} is {undefined} (expanding string {})\n",
                        ctx.expanding
                    ));
                }
                out.push('$');
                out.push('(');
                out.push_str(&name);
                // C writes the bare `)` under suppression and the
                // `,undefined)` tail otherwise (`macCore.c:920-928`), so
                // the knob changes the loader's own view of the value and
                // not just what the operator reads.
                if ctx.opts.suppress_warnings {
                    out.push(')');
                } else {
                    out.push_str(",undefined)");
                }
            }
        },
    }

    scopes.pop();
    Some(after)
}

/// Parse a `,key=val,key2=val2,...` scoped-definition tail. A bare
/// `,key` with no `=` defines nothing (C silently skips it).
fn parse_scoped(
    rest: &[char],
    level: usize,
    ctx: &mut ExpandCtx,
    scopes: &mut Vec<HashMap<String, String>>,
    visiting: &mut Vec<String>,
    out: &mut Vec<(String, String)>,
) {
    let mut k = 0;
    while k < rest.len() {
        if rest[k] != ',' {
            break;
        }
        k += 1; // step over ','
        // Scoped name: up to next top-level `=` or `,`.
        let seg = &rest[k..];
        let term = top_level_terminator(seg);
        let (name_part, tail) = match term {
            Some(t) => (&seg[..t], &seg[t..]),
            None => (seg, &seg[seg.len()..]),
        };
        let mut sname = String::new();
        trans(name_part, level + 1, ctx, scopes, visiting, &mut sname);
        k += name_part.len();
        if let Some('=') = tail.first() {
            let valseg = &tail[1..];
            let vterm = top_level_comma(valseg);
            let (val_part, _) = match vterm {
                Some(t) => (&valseg[..t], &valseg[t..]),
                None => (valseg, &valseg[valseg.len()..]),
            };
            let mut sval = String::new();
            trans(val_part, level + 1, ctx, scopes, visiting, &mut sval);
            out.push((sname, sval));
            k += 1 + val_part.len();
        }
        // else: bare `,name` — no value, defines nothing.
    }
}

/// Index of the first top-level `=` or `,` in `body`, skipping any
/// `$(...)` / `${...}` nested reference.
fn top_level_terminator(body: &[char]) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        if c == '$' && i + 1 < body.len() && (body[i + 1] == '(' || body[i + 1] == '{') {
            depth += 1;
            i += 2;
            continue;
        }
        if (c == ')' || c == '}') && depth > 0 {
            depth -= 1;
        } else if depth == 0 && (c == '=' || c == ',') {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Index of the first top-level `,` in `body` (used to split a
/// default value from trailing scoped definitions).
fn top_level_comma(body: &[char]) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = 0;
    while i < body.len() {
        let c = body[i];
        if c == '$' && i + 1 < body.len() && (body[i + 1] == '(' || body[i + 1] == '{') {
            depth += 1;
            i += 2;
            continue;
        }
        if (c == ')' || c == '}') && depth > 0 {
            depth -= 1;
        } else if depth == 0 && c == ',' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Read `( a , b , ... )` and return the strings. `min`/`max` bound the
/// count the way C's grammar does by having one production per arity.
fn parse_arg_list(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
    min: usize,
    max: usize,
    what: &str,
) -> CaResult<Vec<String>> {
    skip_whitespace_and_comments(chars, pos, line, col);
    let open_line = *line;
    expect_char(chars, pos, col, '(', open_line)?;
    let mut args = Vec::new();
    loop {
        skip_whitespace_and_comments(chars, pos, line, col);
        args.push(read_token_string(chars, pos, line, col)?);
        skip_whitespace_and_comments(chars, pos, line, col);
        match chars.get(*pos) {
            Some(',') => {
                *pos += 1;
                *col += 1;
            }
            _ => break,
        }
    }
    skip_whitespace_and_comments(chars, pos, line, col);
    expect_char(chars, pos, col, ')', *line)?;
    if args.len() < min || args.len() > max {
        let want = if min == max {
            format!("{min}")
        } else {
            format!("{min} or {max}")
        };
        return Err(CaError::DbParseError {
            line: open_line,
            column: *col,
            message: format!("{what} takes {want} arguments, got {}", args.len()),
        });
    }
    Ok(args)
}

/// `menu(name) { choice(id, "string") ... }` (`dbYacc.y:82-101`). The body
/// also admits `include` (`:101`).
fn parse_menu(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<DbdMenu> {
    let name = parse_arg_list(chars, pos, line, col, 1, 1, "menu")?.remove(0);
    skip_whitespace_and_comments(chars, pos, line, col);
    expect_char(chars, pos, col, '{', *line)?;
    let mut choices = Vec::new();
    loop {
        skip_whitespace_and_comments(chars, pos, line, col);
        match chars.get(*pos) {
            None => {
                return Err(CaError::DbParseError {
                    line: *line,
                    column: *col,
                    message: format!("unexpected end of file in menu({name})"),
                });
            }
            Some('}') => {
                *pos += 1;
                *col += 1;
                break;
            }
            _ => {}
        }
        let kw = read_word(chars, pos, col);
        match kw.as_str() {
            "choice" => {
                let mut a = parse_arg_list(chars, pos, line, col, 2, 2, "choice")?;
                let value = a.remove(1);
                choices.push((a.remove(0), value));
            }
            "include" => {
                skip_whitespace_and_comments(chars, pos, line, col);
                let _file = read_token_string(chars, pos, line, col)?;
            }
            _ => {
                return Err(CaError::DbParseError {
                    line: *line,
                    column: *col,
                    message: format!("expected 'choice' in menu({name}), got '{kw}'"),
                });
            }
        }
    }
    // `menuScan` is not an ordinary menu: it is the SCAN field's domain AND
    // the periodic scan rates, and C reads it out of `pdbbase` at `iocInit`
    // (`initPeriodic`, `dbScan.c:858`). Installing it here is C's own timing —
    // the yacc action fills `pdbbase` during the parse — and it is the single
    // place a site menu can enter the record system from, so both load paths
    // (`IocBuilder` and iocsh `dbLoadRecords`) get it without either knowing.
    if name == "menuScan" {
        let values: Vec<String> = choices.iter().map(|(_, value)| value.clone()).collect();
        crate::server::record::menu_scan::install(&values).map_err(|e| CaError::DbParseError {
            line: *line,
            column: *col,
            message: format!("menu({name}): {e}"),
        })?;
    }
    Ok(DbdMenu { name, choices })
}

/// `recordtype(name) { field(NAME, TYPE) { item(v) ... } | %cdef |
/// include ... }` (`dbYacc.y:103-153`). The empty body `{}` is its own C
/// production (`dbRecordtypeEmpty`) and is accepted here as an empty
/// field list.
fn parse_recordtype(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<DbdRecordType> {
    let name = parse_arg_list(chars, pos, line, col, 1, 1, "recordtype")?.remove(0);
    skip_whitespace_and_comments(chars, pos, line, col);
    expect_char(chars, pos, col, '{', *line)?;
    let mut fields = Vec::new();
    let mut cdefs = Vec::new();
    loop {
        skip_whitespace_and_comments(chars, pos, line, col);
        match chars.get(*pos) {
            None => {
                return Err(CaError::DbParseError {
                    line: *line,
                    column: *col,
                    message: format!("unexpected end of file in recordtype({name})"),
                });
            }
            Some('}') => {
                *pos += 1;
                *col += 1;
                break;
            }
            // `%.*` (`dbLex.l:94-97`): a C definition is the rest of the
            // line, minus the `%`. It is NOT a comment, so it cannot go
            // through `skip_whitespace_and_comments`.
            Some('%') => {
                *pos += 1;
                *col += 1;
                let mut text = String::new();
                while let Some(&c) = chars.get(*pos) {
                    if c == '\n' {
                        break;
                    }
                    text.push(c);
                    *pos += 1;
                    *col += 1;
                }
                cdefs.push(text);
                continue;
            }
            _ => {}
        }
        let kw = read_word(chars, pos, col);
        match kw.as_str() {
            "field" => fields.push(parse_recordtype_field(chars, pos, line, col, &name)?),
            "include" => {
                skip_whitespace_and_comments(chars, pos, line, col);
                let _file = read_token_string(chars, pos, line, col)?;
            }
            _ => {
                return Err(CaError::DbParseError {
                    line: *line,
                    column: *col,
                    message: format!("expected 'field' in recordtype({name}), got '{kw}'"),
                });
            }
        }
    }
    Ok(DbdRecordType {
        name,
        fields,
        cdefs,
    })
}

/// `field(NAME, DBF_TYPE) { item(value) ... }` (`dbYacc.y:132-153`).
fn parse_recordtype_field(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
    rtype: &str,
) -> CaResult<DbdField> {
    let mut head = parse_arg_list(chars, pos, line, col, 2, 2, "field")?;
    let dbf_type = head.remove(1);
    let name = head.remove(0);
    skip_whitespace_and_comments(chars, pos, line, col);
    expect_char(chars, pos, col, '{', *line)?;
    let mut items = Vec::new();
    loop {
        skip_whitespace_and_comments(chars, pos, line, col);
        match chars.get(*pos) {
            None => {
                return Err(CaError::DbParseError {
                    line: *line,
                    column: *col,
                    message: format!("unexpected end of file in {rtype}.{name}"),
                });
            }
            Some('}') => {
                *pos += 1;
                *col += 1;
                break;
            }
            _ => {}
        }
        // C has two productions here and the second exists only because
        // `menu` lexes as its own token: it stores the item under the
        // literal key "menu" (`dbYacc.y:144-152`), so both arms land in
        // the same list.
        let key = read_word(chars, pos, col);
        if key.is_empty() {
            return Err(CaError::DbParseError {
                line: *line,
                column: *col,
                message: format!("expected a field item in {rtype}.{name}"),
            });
        }
        let value = parse_arg_list(chars, pos, line, col, 1, 1, &key)?.remove(0);
        items.push((key, value));
    }
    Ok(DbdField {
        name,
        dbf_type,
        items,
    })
}

fn skip_whitespace_and_comments(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) {
    while *pos < chars.len() {
        match chars[*pos] {
            ' ' | '\t' | '\r' => {
                *pos += 1;
                *col += 1;
            }
            '\n' => {
                *pos += 1;
                *line += 1;
                *col = 1;
            }
            '#' => {
                // Line comment
                while *pos < chars.len() && chars[*pos] != '\n' {
                    *pos += 1;
                }
            }
            _ => break,
        }
    }
}

fn read_word(chars: &[char], pos: &mut usize, col: &mut usize) -> String {
    let mut word = String::new();
    while *pos < chars.len() && (chars[*pos].is_ascii_alphanumeric() || chars[*pos] == '_') {
        word.push(chars[*pos]);
        *pos += 1;
        *col += 1;
    }
    word
}

fn read_quoted_string(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<String> {
    if *pos >= chars.len() || chars[*pos] != '"' {
        return Err(CaError::DbParseError {
            line: *line,
            column: *col,
            message: "expected '\"'".into(),
        });
    }
    *pos += 1;
    *col += 1;

    // C dbStatic lexer parity (dbLex.l:88-92): a quoted `tokenSTRING` matches
    // `{doublequote}({dqschar}|{escape})*{doublequote}` where `escape =
    // {backslash}.`. The lexer keeps the **raw bytes** of the string body —
    // `dbmfStrdup(yytext+1)` then NUL-terminates one byte early, so only the
    // surrounding quotes are stripped; escape sequences are NOT translated.
    //
    // This rule reads NAMES ONLY (record/alias/breaktable names, `path` and
    // `include` arguments, the `info` tag) — every `tokenSTRING` in the C
    // grammar. Field and info VALUES are a different token from a different
    // start condition (`jsonSTRING`, dbLex.l:111-114) and DO get translated;
    // they go through [`read_json_string`]. Do not merge the two: unescaping
    // here would corrupt every record name (R18-91).
    //
    // The escape sequence still consumes its following char for delimiter
    // purposes — `\"` does not terminate the string — but both bytes are
    // emitted verbatim.
    let mut s = String::new();
    while *pos < chars.len() && chars[*pos] != '"' {
        if chars[*pos] == '\\' && *pos + 1 < chars.len() && chars[*pos + 1] != '\n' {
            // Emit both the backslash and the escaped char raw.
            //
            // The `!= '\n'` guard is required: C `{escape}` is
            // `{backslash}.` and flex `.` never matches a newline
            // (`{dqschar}` is `[^"\n\\]`), so a backslash immediately
            // before a newline is NOT an escape. Skipping this branch
            // lets the newline fall through to the `Newline in string`
            // error below (dbLex.l:131-133).
            s.push('\\');
            s.push(chars[*pos + 1]);
            *pos += 2;
            *col += 2;
        } else if chars[*pos] == '\n' {
            // dbLex.l:131-133: a newline inside an unterminated quoted
            // string is a hard parse error (`yyerrorAbort("Newline in
            // string, closing quote missing")`), not a literal char.
            return Err(CaError::DbParseError {
                line: *line,
                column: *col,
                message: "Newline in string, closing quote missing".into(),
            });
        } else {
            s.push(chars[*pos]);
            *pos += 1;
            *col += 1;
        }
    }

    if *pos >= chars.len() {
        return Err(CaError::DbParseError {
            line: *line,
            column: *col,
            message: "unterminated string".into(),
        });
    }
    *pos += 1; // skip closing "
    *col += 1;
    Ok(s)
}

/// A quoted field/info VALUE — C's `jsonSTRING` (dbLex.l:111-114, matched in
/// the `JSON` start condition the `field(`/`info(` rules switch into,
/// dbYacc.y:256-267), followed by the translation `dbLexRoutines.c:1398-1403`
/// and `:1435-1440` run on it.
///
/// C keeps the quotes on a `jsonSTRING` — they are precisely the marker that
/// the translation is still owed — and `dbGetFieldValue`/`dbRecordInfo` then
/// strip them and call `dbTranslateEscape(value, value)` before `dbPutString`.
/// So `field(DESC,"hex\x41end")` stores `hexAend`, and a link field's
/// `"@drvUser(\"chan1\")"` reaches device support with real quotes in it, not
/// backslashes. This is the half [`read_quoted_string`] (names, raw) must NOT
/// do, and the whole of R18-91.
///
/// The accepted grammar is C's (dbLex.l:23-33), which is STRICTER than a raw
/// read: a literal control character and the escapes `\1`..`\9` are syntax
/// errors, `\x` demands exactly two hex digits and `\u` exactly four. Single
/// quotes delimit a value string as well as double (`jsonsqstr`), and the other
/// quote character is then an ordinary character. C accepts `\uHHHH` in the
/// lexer but implements no `\u` in the translation, so it comes out as the
/// literal text `uHHHH` — reproduced, because a `.db` in the field may rely on
/// what C actually stores.
fn read_json_string(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<PvString> {
    let quote = match chars.get(*pos) {
        Some(&c @ ('"' | '\'')) => c,
        _ => {
            return Err(CaError::DbParseError {
                line: *line,
                column: *col,
                message: "expected a quoted value".into(),
            });
        }
    };
    *pos += 1;
    *col += 1;

    let err = |line: usize, col: usize, message: &str| CaError::DbParseError {
        line,
        column: col,
        message: message.into(),
    };

    // The escaped source text, quotes stripped — C's `yytext`, minus the two
    // quote characters `dbGetFieldValue` steps over.
    let mut escaped = String::new();
    loop {
        let Some(&c) = chars.get(*pos) else {
            return Err(err(*line, *col, "unterminated string"));
        };
        if c == quote {
            *pos += 1;
            *col += 1;
            break;
        }
        if c == '\n' {
            // dbLex.l:131-133 — the JSON string rule cannot match a newline
            // (`normalchar` excludes every control character), and the
            // catch-all reports the missing quote.
            return Err(err(*line, *col, "Newline in string, closing quote missing"));
        }
        if c == '\\' {
            let Some(&esc) = chars.get(*pos + 1) else {
                return Err(err(*line, *col, "unterminated string"));
            };
            // `escapedchar` is `{backslash}[^ux1-9]`; `x` and `u` introduce the
            // fixed-width `latinchar`/`unicodechar` forms, and `\1`..`\9` are
            // not escapes at all (C has no octal here).
            let hex = |n: usize| {
                chars
                    .get(*pos + 2..*pos + 2 + n)
                    .is_some_and(|ds| ds.iter().all(|d| d.is_ascii_hexdigit()))
            };
            let width = match esc {
                'x' if hex(2) => 4,
                'u' if hex(4) => 6,
                'x' | 'u' | '1'..='9' => {
                    return Err(err(
                        *line,
                        *col,
                        "invalid escape sequence (\\x needs 2 hex digits, \
                         \\u needs 4, and \\1..\\9 are not escapes)",
                    ));
                }
                _ => 2,
            };
            // `\<newline>` is a legal escape here (the class `[^ux1-9]` matches
            // a newline where the name rule's `.` does not), so the line
            // counter has to follow it.
            let consumed = &chars[*pos..*pos + width];
            escaped.extend(consumed);
            *pos += width;
            if consumed.contains(&'\n') {
                *line += 1;
                *col = 0;
            } else {
                *col += width;
            }
            continue;
        }
        if (c as u32) < 0x20 {
            return Err(err(
                *line,
                *col,
                "a control character must be escaped inside a quoted value",
            ));
        }
        escaped.push(c);
        *pos += 1;
        *col += 1;
    }

    Ok(PvString::from_bytes(
        crate::runtime::epics_string::raw_from_escaped(&escaped),
    ))
}

/// A brace-delimited JSON field value, returned verbatim (braces included).
/// Braces inside a JSON string do not nest the value, and a `\` escapes the
/// next character inside a string.
fn read_json_value(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<String> {
    let start_line = *line;
    let close = if chars.get(*pos) == Some(&'[') {
        ']'
    } else {
        '}'
    };
    let mut s = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while *pos < chars.len() {
        let c = chars[*pos];
        s.push(c);
        *pos += 1;
        if c == '\n' {
            *line += 1;
            *col = 0;
        } else {
            *col += 1;
        }

        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(s);
                }
            }
            _ => {}
        }
    }

    Err(CaError::DbParseError {
        line: start_line,
        column: *col,
        message: format!("unterminated JSON value (missing '{close}')"),
    })
}

/// A C `tokenSTRING` — the ONE reader for every grammar position `dbYacc.y`
/// spells that way (record type, record name, field name, info tag, alias
/// names, breaktable name, path/include).
///
/// `dbLex.l:88-97` gives `tokenSTRING` two lexical forms and the grammar never
/// distinguishes them:
///
/// * a bareword — `[a-zA-Z0-9_\-+:.\[\]<>;]+` (`dbLex.l:21`);
/// * a double-quoted string — taken RAW, the lexer only strips the quotes; no
///   escape translation happens in the INITIAL start condition (that is the
///   `JSON` condition's job, and it applies to VALUES only — see
///   [`read_field_value`]).
///
/// So `record("ai", "QT1") { field("VAL", "5") }` and `record(ai, QT2)` are the
/// same grammar to C, and softIoc loads both. The port used to read the type
/// and the field name with [`read_word`] (bareword only, and a narrower charset
/// at that) and the record/alias names with [`read_quoted_string`] (quoted
/// only) — so each position accepted one of C's two forms and refused the
/// other, turning a legal `.db` into a hard startup failure. Reading them all
/// through here is what makes the two forms indistinguishable to the caller,
/// as they are to C.
fn read_token_string(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<String> {
    if matches!(chars.get(*pos), Some('"')) {
        return read_quoted_string(chars, pos, line, col);
    }

    let mut s = String::new();
    while let Some(&c) = chars.get(*pos) {
        if c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '+' | ':' | '.' | '[' | ']' | '<' | '>' | ';')
        {
            s.push(c);
            *pos += 1;
            *col += 1;
        } else {
            break;
        }
    }
    if s.is_empty() {
        let got = chars.get(*pos).map_or("EOF".to_string(), |c| c.to_string());
        return Err(CaError::DbParseError {
            line: *line,
            column: *col,
            message: format!("expected a name (bareword or quoted string), got '{got}'"),
        });
    }
    Ok(s)
}

/// A field or info VALUE — C's `json_value` (dbYacc.y:256-267): the parser
/// switches into the `JSON` start condition for it, so a quoted value is a
/// `jsonSTRING` and its escapes are translated ([`read_json_string`]). The
/// unquoted forms (`jsonBARE`, `jsonNUMBER`) carry no escapes and are taken
/// verbatim, as C does.
fn read_field_value(
    chars: &[char],
    pos: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> CaResult<PvString> {
    if matches!(chars.get(*pos), Some('"' | '\'')) {
        return read_json_string(chars, pos, line, col);
    }

    // JSON value: C's dbLex tokenizes a brace- or bracket-delimited field
    // value as JSON (`field(DOL, {const:"hello"})`, `field(INP, {pva:"PV"})`,
    // `field(INP, [1, 2, 3])`) and hands the raw text to `dbParseLink`, which
    // calls `dbJLinkParse` for an object (dbStaticLib.c:2280-2285) and takes
    // a bracketed value as an array CONSTANT (`:2352-2356`). `json_array` is
    // a `json_value` alternative just like `json_object` (dbYacc.y:328-337,
    // `:356-363`), so both open the same way; the port had no `[` branch, so
    // `[1, 2, 3]` fell through to the bareword reader, stopped at the first
    // comma and failed the whole file. Keep the text verbatim — the link
    // parser is the one that interprets it.
    if matches!(chars.get(*pos), Some('{' | '[')) {
        // The brace text is JSON source — ASCII/UTF-8 by construction, and the
        // escapes inside it belong to yajl, not to the `.db` lexer (R19-67).
        return read_json_value(chars, pos, line, col).map(PvString::from);
    }

    // Unquoted value: a C `bareword` (dbLex.l:21) —
    // `[a-zA-Z0-9_\-+:.\[\]<>;]+`. Leading/trailing whitespace is
    // skipped; an embedded space or any character outside the
    // bareword set is a lexer error in C (the text would tokenize as
    // two tokens). L-5: the Rust parser previously accepted arbitrary
    // bytes up to the next `,`/`)`, which is strictly more permissive
    // than C.
    let is_bareword = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-' | '+' | ':' | '.' | '[' | ']' | '<' | '>' | ';')
    };

    let mut s = String::new();
    while *pos < chars.len() && is_bareword(chars[*pos]) {
        s.push(chars[*pos]);
        *pos += 1;
        *col += 1;
    }
    // Skip trailing whitespace before the delimiter.
    while *pos < chars.len() && matches!(chars[*pos], ' ' | '\t' | '\r' | '\n') {
        if chars[*pos] == '\n' {
            *line += 1;
            *col = 0;
        }
        *pos += 1;
        *col += 1;
    }
    // The only thing allowed after an unquoted value is the field
    // delimiter (`,` or `)`); anything else means the value contained
    // an illegal bareword character.
    if *pos < chars.len() && chars[*pos] != ')' && chars[*pos] != ',' {
        return Err(CaError::DbParseError {
            line: *line,
            column: *col,
            message: format!(
                "illegal character '{}' in unquoted value (expected a quoted string or bareword)",
                chars[*pos]
            ),
        });
    }
    Ok(PvString::from(s))
}

fn expect_char(
    chars: &[char],
    pos: &mut usize,
    col: &mut usize,
    expected: char,
    line: usize,
) -> CaResult<()> {
    if *pos >= chars.len() || chars[*pos] != expected {
        let got = if *pos < chars.len() {
            chars[*pos].to_string()
        } else {
            "EOF".to_string()
        };
        return Err(CaError::DbParseError {
            line,
            column: *col,
            message: format!("expected '{expected}', got '{got}'"),
        });
    }
    *pos += 1;
    *col += 1;
    Ok(())
}

/// C `dbStaticLib`'s record PROTOTYPE — the `initial("…")` directive.
///
/// `dbReadDatabase` builds one zeroed prototype per record type and writes each
/// field's `.dbd` `initial(...)` into it (`dbLexRoutines.c:1150-1163`,
/// `dbPutStringNum`); `dbCreateRecord` then copies that prototype, and the
/// `.db`'s own `field(...)` lines overwrite it. So a `.dbd` initial is the
/// record's default, and a `record(calc,"X") {}` with no CALC line still has
/// `CALC="0"` — not the empty string.
///
/// The port had no such step: every record hand-wrote a Rust `Default`, and
/// nothing tied it to the `.dbd`. calc, calcout and swait all forgot
/// `initial("0")` on their CALC/OCAL (`calcRecord.dbd:26`,
/// `calcoutRecord.dbd:62,382`, `swaitRecord.dbd:424`), so a default record
/// carried the EMPTY expression — which base's `postfix()` refuses
/// (`CALC_ERR_NULL_ARG`) and every evaluation then fails, putting a plain
/// `record(calc,"X") {}` into CALC/INVALID on its first process where C sits at
/// NO_ALARM.
///
/// This is the single owner of that step, so the `.dbd` is the only place a
/// default can be stated. A record's `Default` supplies the zeroed struct; the
/// `.dbd` supplies the initial value. The store is [`Record::put_field_internal`]
/// — dbStatic writes the prototype directly, with no `special()` and no
/// `SPC_NOMOD` gate, so a read-only field's initial lands too (its record's
/// `init_record` overwrites it afterwards, exactly as in C).
///
/// A field the record does not own carries its default in `CommonFields`
/// instead; there is no record storage here to write it into, and
/// `tests/dbd_initial_parity.rs` is what holds those to the `.dbd`.
fn apply_dbd_initials(record: &mut Box<dyn Record>) -> CaResult<()> {
    // The `.dbd` is the source, so the initials come from the GENERATED table —
    // `crate::server::record::dbd_generated`, which `tools/dbd-codegen` emits
    // from the vendored `.dbd` files. Asking the `.dbd` directly is what makes
    // the default impossible to get wrong by hand.
    //
    // This is BASE's generated table, so it seeds base's record types only. The
    // downstream record types (`motor`, `table`, `scaler`, `epid`, `throttle`,
    // `timestamp`) now carry a generated table too — in their own crate, which
    // base cannot name — and their `.dbd` `initial()` values are therefore
    // parsed but never applied: those records keep the defaults their `Default`
    // impl sets. Seeding them is not a table lookup but a semantic change (C
    // applies the `.dbd` initial BEFORE `init_record`, and each record's
    // `init_record` then overwrites some of them — `motor`'s VERS is stamped
    // 7.4 over an `initial("1")`), so it is left to a change that can validate
    // each record against its C `init_record` rather than folded in here.
    let Some(fields) = crate::server::record::dbd_generated::record_fields(record.record_type())
    else {
        return Ok(()); // a record type base's vendored `.dbd` set does not declare
    };
    let initials: Vec<(&'static str, &'static str)> = fields
        .iter()
        .filter(|f| record.implements_field(f.name))
        .filter_map(|f| f.initial.map(|v| (f.name, v)))
        .collect();

    for (name, initial) in initials {
        let spec = fields.iter().find(|f| f.name == name);
        // The `.db` load path's coercion, reused verbatim: a `menu()` field's
        // initial is a LABEL (`initial("Passive")`), everything else parses to
        // the field's stored type — which is what the record already holds, the
        // `.dbd` type being the fallback for a field it cannot yet produce.
        let dbf_type = match record
            .get_field(name)
            .map(|v| v.db_field_type())
            .or_else(|| spec.map(|f| f.dbf_type))
        {
            Some(t) => t,
            None => continue,
        };
        let parsed = if let Some(choices) = spec
            .and_then(|f| f.menu)
            .or_else(|| record.menu_field_choices(name))
            .or_else(|| crate::server::record::shared_menu_choices(name))
        {
            crate::server::record::resolve_menu_field_string_db_load(
                name, choices, dbf_type, initial,
            )?
        } else {
            EpicsValue::parse_bytes(dbf_type, initial.as_bytes()).map_err(|e| {
                CaError::InvalidValue(format!(
                    "{}.{name}: cannot parse .dbd initial(\"{initial}\") as {dbf_type:?}: {e}",
                    record.record_type()
                ))
            })?
        };
        // A field whose zeroed Rust default already IS the `.dbd` initial needs
        // no store — and must not get one: a `SPC_NOMOD` field the record serves
        // but has no store arm for (`sseq.IX1`, C's read-only step index, whose
        // initial is the 0 it already holds) would fail the write. The write is
        // therefore driven by the DIFFERENCE, which is the thing this step exists
        // to remove.
        if record.get_field(name).as_ref() == Some(&parsed) {
            continue;
        }
        record.put_field_internal(name, parsed)?;
    }
    Ok(())
}

/// Create a record from a type name.
/// Checks the external factory registry first, then falls back to built-in types.
///
/// The record comes back holding its `.dbd` initial values — see
/// `apply_dbd_initials`. Every load path (`IocBuilder`, `dbLoadRecords`,
/// `dbCreateRecord`) goes through here, so no record can reach the database
/// disagreeing with its `.dbd` about a default.
pub fn create_record(record_type: &str) -> CaResult<Box<dyn Record>> {
    let mut record = create_record_raw(record_type)?;
    apply_dbd_initials(&mut record)?;
    Ok(record)
}

fn create_record_raw(record_type: &str) -> CaResult<Box<dyn Record>> {
    // Check external registry first (allows overrides from e.g. asyn-rs)
    if let Ok(reg) = get_registry().lock() {
        if let Some(factory) = reg.get(record_type) {
            return Ok(factory());
        }
    }

    use crate::server::records::*;

    match record_type {
        "ai" => Ok(Box::new(ai::AiRecord::default())),
        "ao" => Ok(Box::new(ao::AoRecord::default())),
        "bi" => Ok(Box::new(bi::BiRecord::default())),
        "bo" => Ok(Box::new(bo::BoRecord::default())),
        "stringin" => Ok(Box::new(stringin::StringinRecord::default())),
        "stringout" => Ok(Box::new(stringout::StringoutRecord::default())),
        "longin" => Ok(Box::new(longin::LonginRecord::default())),
        "longout" => Ok(Box::new(longout::LongoutRecord::default())),
        "int64in" => Ok(Box::new(int64in::Int64inRecord::default())),
        "int64out" => Ok(Box::new(int64out::Int64outRecord::default())),
        "lsi" => Ok(Box::new(lsi::LsiRecord::default())),
        "lso" => Ok(Box::new(lso::LsoRecord::default())),
        "mbbi" => Ok(Box::new(mbbi::MbbiRecord::default())),
        "mbbo" => Ok(Box::new(mbbo::MbboRecord::default())),
        "mbbiDirect" => Ok(Box::new(mbbi_direct::MbbiDirectRecord::default())),
        "mbboDirect" => Ok(Box::new(mbbo_direct::MbboDirectRecord::default())),
        "event" => Ok(Box::new(event::EventRecord::default())),
        "printf" => Ok(Box::new(printf::PrintfRecord::default())),
        "waveform" => Ok(Box::new(waveform::WaveformRecord::with_kind(
            waveform::ArrayKind::Waveform,
        ))),
        "aai" => Ok(Box::new(waveform::WaveformRecord::with_kind(
            waveform::ArrayKind::Aai,
        ))),
        "aao" => Ok(Box::new(waveform::WaveformRecord::with_kind(
            waveform::ArrayKind::Aao,
        ))),
        "subArray" => Ok(Box::new(waveform::WaveformRecord::with_kind(
            waveform::ArrayKind::SubArray,
        ))),
        "calc" => Ok(Box::new(calc::CalcRecord::default())),
        "fanout" => Ok(Box::new(fanout::FanoutRecord::default())),
        "seq" => Ok(Box::new(seq::SeqRecord::default())),
        "calcout" => Ok(Box::new(calcout::CalcoutRecord::default())),
        "dfanout" => Ok(Box::new(dfanout::DfanoutRecord::default())),
        "compress" => Ok(Box::new(compress::CompressRecord::default())),
        "histogram" => Ok(Box::new(histogram::HistogramRecord::default())),
        "sel" => Ok(Box::new(sel::SelRecord::default())),
        "sub" => Ok(Box::new(sub_record::SubRecord::default())),
        "aSub" => Ok(Box::new(asub_record::ASubRecord::default())),
        "permissive" => Ok(Box::new(permissive::PermissiveRecord::default())),
        "state" => Ok(Box::new(state::StateRecord::default())),
        _ => Err(unregistered_record_type(record_type)),
    }
}

/// The error a record type reaches when no arm above claims it.
///
/// Two populations land here and they are not the same mistake. A type
/// `epics-base-rs` vendors a `.dbd` for — so `record_fields` knows its shape —
/// but does not construct is **module-owned**: `dbd/stdRecords.dbd`, vendored
/// byte-identical from C, is the manifest of what an EPICS Base IOC links, and
/// `aCalcout`/`asyn`/`busy`/`sCalcout`/`sseq`/`swait`/`transform` are not in
/// it. They belong to calc, asyn and busy, and base's default registry no
/// longer claims them; the application opts in with
/// [`register_record_type`] (or `IocBuilder`/`IocApplication`'s method of the
/// same name). Anything else is simply not a record type the port knows.
fn unregistered_record_type(record_type: &str) -> CaError {
    let message = if crate::server::record::dbd_generated::record_fields(record_type).is_some() {
        format!(
            "record type '{record_type}' is not an EPICS Base record type \
             (it is not in stdRecords.dbd) and is not registered; register it \
             with register_record_type(\"{record_type}\", ...) before loading \
             a .db that uses it"
        )
    } else {
        format!("unknown record type: '{record_type}'")
    };
    CaError::DbParseError {
        line: 0,
        column: 0,
        message,
    }
}

/// Create a record, checking extra factories first, then built-in types.
/// Preferred over `create_record()` — avoids the global registry.
pub fn create_record_with_factories(
    record_type: &str,
    extra_factories: &std::collections::HashMap<String, super::RecordFactory>,
) -> CaResult<Box<dyn Record>> {
    if let Some(factory) = extra_factories.get(record_type) {
        let mut record = factory();
        apply_dbd_initials(&mut record)?;
        return Ok(record);
    }
    create_record(record_type)
}

/// Apply fields from a DbRecordDef to a record.
/// Returns the record along with any common field values.
/// Rewrite a `LINR` field that names a loaded breakpoint table to the numeric
/// `menuConvert` index that selects it. C extends the `menuConvert` menu with
/// one name-sorted choice per loaded table, so the first table is index 3
/// ([`crate::server::cvt_bpt::BreakTableRegistry`]). Only `ai`/`ao` carry a
/// `LINR` field; fixed labels (`NO_CONVERSION`/`SLOPE`/`LINEAR`) and numeric
/// values match no table name and are left for [`apply_fields`]'s menu
/// resolution. A no-op when the registry is empty. Shared by the IocBuilder
/// and `dbLoadRecords` load paths so both resolve table names identically.
pub fn resolve_linr_breaktable_names(
    record_type: &str,
    fields: &mut [DbFieldDef],
    registry: &crate::server::cvt_bpt::BreakTableRegistry,
) {
    if registry.is_empty() || !matches!(record_type, "ai" | "ao") {
        return;
    }
    for DbFieldDef {
        name: fname,
        value: fvalue,
        ..
    } in fields.iter_mut()
    {
        if fname.eq_ignore_ascii_case("LINR") {
            // A breakpoint-table NAME is an identifier — ASCII.
            if let Some(idx) = registry.linr_index_of(&fvalue.as_str_lossy()) {
                *fvalue = PvString::from(idx.to_string());
            }
        }
    }
}

/// The link fields whose value must reach [`RecordCommon`] — and therefore
/// device-support init via `DeviceSupportContext` — for every record type,
/// even one that stores the field itself.
///
/// [`RecordCommon`]: crate::server::record::CommonFields
fn is_common_link_field(upper_name: &str) -> bool {
    matches!(upper_name, "INP" | "OUT")
}

/// C's db-load half of the link gate. `dbPutString` on a link field runs
/// `dbParseLink` and stores the text (`dbStaticLib.c:2626-2634`) — nothing
/// else is asked at load, so a `.db` fails here only on text that will not
/// PARSE: a brace-delimited link naming no registered jlink type, or a `#`
/// form whose identifier letters name no bus.
///
/// Whether the link FITS the record's bound device support is `dbCanSetLink`,
/// and C asks that exactly once, at `iocInit`, from `dbInitRecordLinks`
/// (`dbStaticLib.c:2222`) — which prints the refusal and keeps the record.
/// [`PvDatabase::db_init_record_links`] is that pass. Asking it here instead
/// cost a `.db` every record after the first bad link: measured against
/// `softIoc` R7.0.10 on a ten-record file, C kept ten and the port kept two.
///
/// [`PvDatabase::db_init_record_links`]: crate::server::database::PvDatabase
fn check_link_syntax(record_type: &str, fields: &[DbFieldDef]) -> CaResult<()> {
    for DbFieldDef { name, value, .. } in fields {
        check_link_text(record_type, &name.to_uppercase(), &value.as_str_lossy())?;
    }
    Ok(())
}

/// C's `pflddes->field_type == DBF_NOACCESS`, read from the two places the
/// generated tables keep the declaration.
///
/// A `DBF_NOACCESS` row survives into the field table two ways, and neither
/// leaves `dbf_type` saying `NOACCESS` — which is why [`FieldDesc::no_access`]
/// has to read the DECLARED type. `special(SPC_DBADDR)` rows are re-typed from
/// `cvt_dbaddr.types` so they can be SERVED; rows whose `extra(...)` names a
/// plain scalar (`BKPT`, `TIME`) are carried for their width and stay
/// unservable. Everything else is dropped from the table and survives only as a
/// name, in `record_noaccess_fields` / `DB_COMMON_NOACCESS`. The union of table
/// and names is the declaration, so both are asked.
fn is_declared_no_access(
    record_type: &str,
    field: &str,
    spec: Option<&'static crate::server::record::FieldDesc>,
) -> bool {
    if spec.is_some_and(|f| f.no_access()) {
        return true;
    }
    let named = |list: &[&str]| list.iter().any(|f| f.eq_ignore_ascii_case(field));
    crate::server::record::dbd_generated::record_noaccess_fields(record_type).is_some_and(named)
        || named(crate::server::record::dbd_generated::DB_COMMON_NOACCESS)
}

/// The value C's `.db` loader stores for `value` on a numeric `field`, or
/// `None` where `dbPutStringNum` has no numeric arm for the declaration.
///
/// A refusal is `None` too, and deliberately: the screen
/// ([`menu_value_refusal`]) has already removed every field C refuses before
/// the load reaches here, so a refusal at this point can only come from a
/// caller that skipped the screen — and those keep the conversion they had.
fn lax_numeric(declared: DbfCode, field: &str, value: &str) -> Option<EpicsValue> {
    db_load_numeric_value(declared, field, value).ok().flatten()
}

pub fn apply_fields(
    record: &mut Box<dyn Record>,
    fields: &[DbFieldDef],
    common_fields: &mut Vec<(String, EpicsValue)>,
) -> CaResult<()> {
    check_link_syntax(record.record_type(), fields)?;

    for DbFieldDef { name, value, .. } in fields {
        let upper_name = name.to_uppercase();
        // Menu labels, numbers and link text are ASCII by construction; only a
        // DBF_STRING can carry a byte outside it, and that one goes to
        // `EpicsValue::parse_bytes` below, which keeps the bytes.
        let value_str = &value.as_str_lossy();

        // Two separate questions, and they must stay separate: *who owns* this
        // field (the record, or the framework's dbCommon fallback), and *what
        // type* the `.db` string parses as. `field_list()` is the spec and now
        // carries every field the `.dbd` declares — including INP/OUT, which
        // most record types leave to the framework — so ownership is asked of
        // `implements_field()`, not of table membership.
        let owned = record.implements_field(&upper_name);
        // The DECLARATION, from the same owner every other consumer asks: the
        // `.dbd`-generated table first, the record's hand table only where the
        // `.dbd` does not reach. Reading it off `field_list()` alone left an
        // `aSub`'s `field(FTA,"LONG")` with no `menu()` to resolve against.
        let spec = crate::server::record::record_instance::field_desc_of(
            record.as_ref(),
            upper_name.as_str(),
        );

        // C `dbPutString` switches on the DECLARED field type, and a
        // `DBF_NOACCESS` field is refused outright — `dbStaticLib.c:2646-2650`:
        //
        // ```c
        //     case DBF_NOACCESS:
        //         dbMsgPrint(pdbentry, "Can't set array field before iocInit()");
        //         /* fall through */
        //     default:
        //         return S_dbLib_badField;
        // ```
        //
        // The port used to name the five fields it had been told about
        // (lsi/lso/printf VAL, lsi/lso OVAL) through `long_string_fields`, so the
        // other 172 `DBF_NOACCESS` rows the vendored `.dbd`s declare — every
        // `waveform.VAL`, `aSub.VALx`, `SIMPVT` — were accepted and quietly
        // stored where C fails the load. A by-name list cannot be complete;
        // the declaration can, so ask it.
        if is_declared_no_access(record.record_type(), &upper_name, spec) {
            return Err(CaError::InvalidValue(format!(
                "field {upper_name}: can't set array field before iocInit() \
                 (a DBF_NOACCESS field is not settable from a .db file, as in C)"
            )));
        }

        if owned {
            // Parse to the type the record STORES, exactly as
            // `put_field_internal_default` coerces to it: `put_field`'s arms
            // match on what is stored, and a `menu()` field is declared
            // `DBF_MENU` but stored as a `Short` choice index. The `.dbd` type
            // is the fallback for a field the record cannot yet produce a value
            // for.
            let dbf_type = record
                .get_field(&upper_name)
                .map(|v| v.db_field_type())
                .or_else(|| spec.map(|f| f.dbf_type))
                .ok_or_else(|| {
                    CaError::InvalidValue(format!("field {upper_name}: no type to parse it as"))
                })?;
            // A `DBF_MENU` field's `.db` value resolves against THAT field's
            // own menu (label-first, then a numeric index) — C dbStaticLib
            // `dbPutStringNum`. Using the field's choices, not a cross-menu
            // global table, is what keeps a menu-specific label from being
            // dropped or mis-mapped (e.g. `field(SELM,"Specified")`).
            let parsed = if let Some(choices) = spec
                .and_then(|f| f.menu)
                .or_else(|| record.menu_field_choices(&upper_name))
                .or_else(|| crate::server::record::shared_menu_choices(&upper_name))
            {
                crate::server::record::resolve_menu_field_string_db_load(
                    &upper_name,
                    choices,
                    dbf_type,
                    value_str,
                )?
            } else if let Some(v) =
                spec.and_then(|f| lax_numeric(f.declared_dbf, &upper_name, value_str))
            {
                // C's `dbPutStringNum` conversion, not `dbConvert`'s: the `.db`
                // load parses to 64 bits and casts, so it stores what a
                // `caput` of the same text would be refused for. Projected onto
                // the carrier the record actually holds, which for a numeric
                // field is the declared width itself.
                v.convert_to(dbf_type)
            } else {
                // BYTES, not text: a DBF_STRING is a byte string with a 40-byte
                // budget, and `field(VAL,"h\xffz")` is 3 bytes in C (R19-68).
                EpicsValue::parse_bytes(dbf_type, value.as_bytes()).map_err(|e| {
                    CaError::InvalidValue(format!(
                        "field {upper_name} (type {dbf_type:?}): cannot parse '{value_str}': {e}"
                    ))
                })?
            };
            record.put_field(&upper_name, parsed)?;
            // A record type may declare INP/OUT in its own `field_list`,
            // mirroring its C `.dbd` (`scalerRecord`, `motorRecord`,
            // `acalcout`, …). The value is then stored by the record — but in
            // C the link is *also* what device support reads at init:
            // `devXxx.c::init_record` dereferences `prec->out` / `prec->inp`
            // directly (e.g. `devScalerAsyn.c::scaler_init_record` parses OUT
            // to pick its board), and a record owning the field changes
            // nothing about that. Routing the value to the record ALONE left
            // `RecordCommon.inp`/`.out` — the single source of truth
            // `DeviceSupportContext` is built from — empty, so a dynamic
            // device-support factory saw `ctx.out == ""` and could not
            // disambiguate. Mirror link fields into the common fields as well
            // so the link text reaches device-support init for every record
            // type. This is text only: `RecordInstance::put_common_field`
            // arms the framework's own link dispatch (`parsed_inp` /
            // `parsed_out`) only for a record that does NOT declare the field,
            // so a record driving its own link is not driven twice.
            if is_common_link_field(&upper_name) {
                common_fields.push((upper_name.clone(), EpicsValue::String(value.clone())));
            }
        } else {
            // Store as common field for RecordInstance to handle. The BYTES: a
            // common DBF_STRING (DESC, EGU, …) has the same 40-byte budget.
            // A numeric one is converted HERE, by the same `dbPutStringNum` the
            // screen refuses with, so the sink is handed a value rather than
            // text it would convert by the runtime `dbPut` rules instead.
            let converted = spec.and_then(|f| lax_numeric(f.declared_dbf, &upper_name, value_str));
            common_fields.push((
                upper_name.clone(),
                converted.unwrap_or_else(|| EpicsValue::String(value.clone())),
            ));
        }

        // C `dbPutString` (`dbStaticLib.c:2653-2660`): writing VAL through the
        // STATIC library also writes UDF = 0 —
        //
        // ```c
        // if (!status && strcmp(pflddes->name, "VAL") == 0) {
        //     ...
        //     if (!dbFindField(&dbentry, "UDF")) dbPutString(&dbentry, "0");
        // }
        // ```
        //
        // — so `field(VAL,"1")` in a `.db` DEFINES the record at load, on every
        // record type, whatever its `process()` later does with UDF. The load
        // owner is the only place this can live: it is the one gate both
        // `dbLoadRecords` paths (IocBuilder and iocsh) cross. Pushed in file
        // order, so an explicit `field(UDF,...)` after `field(VAL,...)` still
        // wins, exactly as it does in C.
        if upper_name == "VAL" {
            common_fields.push(("UDF".to_string(), EpicsValue::Char(0)));
        }
    }
    Ok(())
}

/// Render a fixture path so it survives being used as a MACRO or
/// ENVIRONMENT value.
///
/// `trans` discards escapes in a substituted value (C
/// `macCore.c:701-703`), so every `\` in a Windows path is eaten the
/// moment the value is re-scanned and `$(TOP)/x.db` resolves against a
/// separator-less string. Win32 accepts `/` as a separator, which is
/// why EPICS build systems put `TOP` in forward-slash form there; a
/// test that feeds `tempdir()` into a macro has to do the same.
#[cfg(test)]
pub(crate) fn macro_safe_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_db() {
        let input = r#"
    record(ai, "TEMP") {
    field(DESC, "Temperature")
    field(SCAN, "1 second")
    field(HOPR, "100")
    field(LOPR, "0")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, "ai");
        assert_eq!(records[0].name, "TEMP");
        assert_eq!(records[0].fields.len(), 4);
        // Line 3, not 2: the raw string opens with a newline, so
        // `record(...)` is line 2 and this field is the line under it.
        // The number is the whole point of carrying it on the element —
        // it is what `yyerror` prints when the field is refused.
        assert_eq!(
            records[0].fields[0],
            DbFieldDef {
                name: "DESC".into(),
                value: "Temperature".into(),
                line: 3
            }
        );
    }

    #[test]
    fn test_macro_substitution() {
        let input = r#"
    record(ai, "$(P)TEMP") {
    field(DESC, "$(D=Default Desc)")
    }
    "#;
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "IOC:".to_string());

        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records[0].name, "IOC:TEMP");
        assert_eq!(records[0].fields[0].value, "Default Desc");
    }

    #[test]
    fn test_multiple_records() {
        let input = r#"
    record(ai, "TEMP1") {
    field(VAL, "25.0")
    }
    record(bo, "SWITCH") {
    field(VAL, "1")
    field(ZNAM, "Off")
    field(ONAM, "On")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].record_type, "ai");
        assert_eq!(records[1].record_type, "bo");
    }

    #[test]
    fn test_comments() {
        let input = r#"
    # This is a comment
    record(ai, "TEMP") {
    # Another comment
    field(VAL, "25.0")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn test_unknown_record_type() {
        let result = create_record("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_quoted_string_escape() {
        // R18-91: a quoted field VALUE is a `jsonSTRING`, and
        // `dbGetFieldValue` runs `dbTranslateEscape` over it — so
        // `"hello \"world\""` stores the 13 chars `hello "world"`, with real
        // quotes. (This test previously pinned the defect, asserting the raw
        // 15-char form.) The `\"` still does not terminate the string.
        //
        // softIoc: field(VAL,"a \"b\" c") -> dbgf prints "a \"b\" c", i.e. it
        // re-escapes on print (dbTest.c:1007), so the stored bytes are 9.
        let input = r#"
    record(stringin, "TEST") {
    field(VAL, "hello \"world\"")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].value, r#"hello "world""#);
    }

    /// C `dbRecordField`'s `dbFindField` arm (`dbLexRoutines.c:1240-1388`
    /// @`R7.0.10`) ends in `yyerror(NULL)`, not `yyerrorAbort`: the FIELD is
    /// dropped and nothing else is. Measured on softIoc @`R7.0.10` with this
    /// exact text — `dbl` lists C:ONE, C:TWO and C:THREE, `dbgf C:TWO.CALC`
    /// is "2", and `dbgf C:TWO.OUT` is `PV 'C:TWO.OUT' not found`.
    #[test]
    fn an_undeclared_field_is_dropped_and_the_rest_of_the_load_survives() {
        let parsed = parse_db_with_breaktables(
            r#"
record(calc, "C:ONE") { field(CALC, "1") }
record(calc, "C:TWO") {
    field(OUT, "C:ONE.VAL PP")
    field(CALC, "2")
}
record(calc, "C:THREE") { field(CALC, "3") }
"#,
            &HashMap::new(),
        )
        .expect("an undeclared field must not abort the parse");

        let names: Vec<&str> = parsed.records.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["C:ONE", "C:TWO", "C:THREE"],
            "every record survives"
        );
        let two = &parsed.records[1];
        assert_eq!(
            two.fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["CALC"],
            "OUT is dropped, CALC is kept"
        );
        assert_eq!(
            parsed.faults.first_diagnostic().unwrap(),
            format!("{ERL_ERROR}: calc record 'C:TWO' doesn't have a field 'OUT'"),
            "C's message, verbatim"
        );
    }

    /// The second name-side refusal in the same C function
    /// (`dbLexRoutines.c:1390-1396`): `dbFindField` FINDS `NAME` — it is
    /// dbCommon's field 0 — so `indfield == 0` refuses it separately, with its
    /// own wording. softIoc @`R7.0.10` on `field(NAME,"C:OTHER")` prints
    /// `ERROR: Can't set 'NAME' field of record 'C:N'` and still loads C:N
    /// with CALC applied.
    #[test]
    fn the_name_field_is_refused_with_its_own_message() {
        let parsed = parse_db_with_breaktables(
            r#"record(calc, "C:N") { field(NAME, "C:OTHER") field(CALC, "5") }"#,
            &HashMap::new(),
        )
        .expect("NAME must not abort the parse");
        assert_eq!(parsed.records[0].name, "C:N");
        assert_eq!(
            parsed.records[0]
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["CALC"]
        );
        assert_eq!(
            parsed.faults.first_diagnostic().unwrap(),
            format!("{ERL_ERROR}: Can't set 'NAME' field of record 'C:N'")
        );
    }

    /// `dbFindFieldPart` compares with `strncmp` (`dbStaticLib.c:1822-1824`),
    /// so the lookup is case-SENSITIVE and `calc` is not `CALC`. softIoc
    /// @`R7.0.10`: `ERROR: calc record 'C:CASE' doesn't have a field 'calc'`.
    #[test]
    fn a_field_name_in_the_wrong_case_is_not_the_field() {
        let parsed = parse_db_with_breaktables(
            r#"record(calc, "C:CASE") { field(calc, "9") }"#,
            &HashMap::new(),
        )
        .unwrap();
        assert!(parsed.records[0].fields.is_empty());
        assert_eq!(
            parsed.faults.first_diagnostic().unwrap(),
            format!("{ERL_ERROR}: calc record 'C:CASE' doesn't have a field 'calc'")
        );
    }

    /// `dbFindField` falls back to `dbGetRecordAttribute`
    /// (`dbStaticLib.c:1852-1853`), and `dbReadCOM` installs `RTYP` and `VERS`
    /// on every record type (`dbLexRoutines.c:311-331`) — so these two load in
    /// C where any other unknown name does not. Measured: softIoc @`R7.0.10`
    /// takes `field(RTYP,"zz")` and `field(VERS,"9")` on a calc record with no
    /// diagnostic at all.
    #[test]
    fn the_rtyp_and_vers_record_attributes_load() {
        let parsed = parse_db_with_breaktables(
            r#"record(calc, "C:A") { field(RTYP, "zz") field(VERS, "9") field(CALC, "1") }"#,
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(parsed.records[0].fields.len(), 3);
        assert!(parsed.faults.is_empty(), "C reports nothing here");
    }

    /// A `DBF_NOACCESS` field is IN `papFldDes`, so `dbFindField` finds it and
    /// it is `dbPutString` that refuses the write (`dbStaticLib.c:2646-2650`).
    /// The name-side gate must therefore let it through to
    /// [`is_declared_no_access`] rather than claim the field does not exist.
    #[test]
    fn a_noaccess_field_passes_the_name_gate() {
        let parsed = parse_db_with_breaktables(
            r#"record(waveform, "W:ONE") { field(VAL, "1") }"#,
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(parsed.records[0].fields.len(), 1);
        assert!(parsed.faults.is_empty());
        assert_eq!(
            record_type_field_exists("waveform", "VAL"),
            Some(true),
            "waveform.VAL is declared DBF_NOACCESS, not absent"
        );
    }

    /// C `dbPutString`'s `DBF_NOACCESS` arm refuses the write
    /// (`dbStaticLib.c:2646-2650`), and `is_declared_no_access` asks two
    /// sources because the declaration lives in two places. This pins that the
    /// answer does not depend on WHICH.
    ///
    /// `BKPT` and `TIME` are the reason: they used to be answered by the name
    /// list, and carrying their descriptors took them out of it, so the refusal
    /// now comes from the `spec` arm alone. That arm had no live caller before
    /// — every row reaching it was `special(SPC_DBADDR)`, which the name gate
    /// had already let through — so a silent acceptance here would have looked
    /// exactly like the absence it replaced.
    #[test]
    fn a_no_access_field_is_refused_from_a_db_file_by_either_source() {
        use crate::server::record::Record;

        let load = |rt: &str, field: &str| -> CaResult<Box<dyn Record>> {
            let mut rec = create_record(rt).unwrap();
            let mut common = Vec::new();
            apply_fields(&mut rec, &[DbFieldDef::new(field, "1")], &mut common)?;
            Ok(rec)
        };

        // The `spec` arm: a carried descriptor, no longer in any name list.
        for field in ["BKPT", "TIME"] {
            assert!(
                !crate::server::record::dbd_generated::DB_COMMON_NOACCESS.contains(&field),
                "{field} is carried as a descriptor, so the name list must not \
                 be what refuses it"
            );
            let Err(err) = load("ai", field) else {
                panic!("ai.{field} is DBF_NOACCESS and must not be settable from a .db");
            };
            assert!(err.to_string().contains("DBF_NOACCESS"), "{field}: {err}");
        }

        // The name-list arm, which those two left: still refusing.
        let Err(err) = load("ai", "MLOK") else {
            panic!("ai.MLOK is DBF_NOACCESS and must not be settable from a .db");
        };
        assert!(err.to_string().contains("DBF_NOACCESS"), "MLOK: {err}");

        // And a settable field of the same record still loads, so the gate did
        // not simply start refusing everything.
        assert!(load("ai", "PREC").is_ok());
    }

    /// C reaches `dbRecordField` only for a type `dbFindRecordType` resolved;
    /// anything else is `yyerrorAbort` in `dbRecordHead`
    /// (`dbLexRoutines.c:1159-1165`). The port cannot abort there — the type
    /// is resolved later, by `create_record` — so the name gate must decline
    /// to judge a type it cannot see instead of calling every field absent.
    #[test]
    fn a_record_type_with_no_visible_declarations_is_not_judged() {
        assert_eq!(record_type_field_exists("noSuchRecordType", "ANY"), None);
        let parsed = parse_db_with_breaktables(
            r#"record(noSuchRecordType, "X:ONE") { field(WHATEVER, "1") }"#,
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(parsed.records[0].fields.len(), 1);
        assert!(parsed.faults.is_empty());
    }

    #[test]
    fn test_quoted_value_translates_escapes() {
        // R18-91: `\n`, `\t`, `\\` and `\xHH` are all TRANSLATED in a field
        // value — softIoc `field(DESC,"hex\x41end")` -> `dbgf E2.DESC` =
        // "hexAend". (This test previously pinned the defect under the name
        // `test_quoted_string_keeps_escapes_raw`.)
        // Every field named here is one `stringin` actually declares:
        // `field(OUT,...)` used to stand in the middle of this list, which
        // `dbRecordField` refuses outright (`ERROR: stringin record 'TEST'
        // doesn't have a field 'OUT'`, measured on softIoc @`R7.0.10`).
        let input = r#"
    record(stringin, "TEST") {
    field(DESC, "line1\nline2")
    field(VAL, "a\\b\tc")
    field(SIOL, "hex\x41end")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].value, "line1\nline2");
        assert_eq!(records[0].fields[1].value, "a\\b\tc");
        assert_eq!(records[0].fields[2].value, "hexAend");
    }

    /// The other half of the split: a record/alias NAME is a `tokenSTRING`
    /// (dbLex.l:88-92) and keeps its escape bytes RAW. Unescaping the shared
    /// helper would have broken every name.
    #[test]
    fn test_record_name_keeps_escapes_raw() {
        let input = r#"
    record(stringin, "T\tEST") {
    field(VAL, "x")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].name, r"T\tEST");
    }

    /// An `info` VALUE is translated (`dbRecordInfo`, dbLexRoutines.c:1435-1440);
    /// the info TAG is a `tokenSTRING` and is not.
    #[test]
    fn test_info_value_translates_escapes() {
        let input = r#"
    record(stringin, "TEST") {
    info("Q:group", "tab\there")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].info_tags[0].0, "Q:group");
        assert_eq!(records[0].info_tags[0].1, "tab\there");
    }

    /// A single-quoted value is a `jsonsqstr` (dbLex.l:31-33) and is translated
    /// the same way — softIoc: `field(VAL,'sq:\tx')` stores `sq:<TAB>x`.
    #[test]
    fn test_single_quoted_value_translates_escapes() {
        let input = "record(stringin, \"TEST\") {\n field(VAL, 'sq:\\tx')\n}\n";
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].value, "sq:\tx");
    }

    /// C's value grammar is STRICTER than a raw read: `\1`..`\9` is not an
    /// escape (`escapedchar` = `{backslash}[^ux1-9]`, dbLex.l:25), so a `.db`
    /// carrying an octal escape is a syntax error — softIoc rejects
    /// `field(VAL,"octal\101x")` with `ERROR: syntax error`, and the port must
    /// not silently start up with different data.
    #[test]
    fn test_octal_escape_in_value_is_rejected() {
        let input = r#"
    record(stringin, "TEST") {
    field(VAL, "octal\101x")
    }
    "#;
        assert!(matches!(
            parse_db(input, &HashMap::new()),
            Err(CaError::DbParseError { .. })
        ));
    }

    /// A link field rides the same path: the escaped quotes reach device
    /// support as real quotes, not backslashes (R18-91's headline impact).
    #[test]
    fn test_link_field_value_is_translated() {
        let input = r#"
    record(stringin, "TEST") {
    field(INP, "@drvUser(\"chan1\")")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].value, r#"@drvUser("chan1")"#);
    }

    #[test]
    fn test_quoted_string_newline_aborts() {
        // a literal newline inside a quoted string (missing
        // closing quote) is a hard parse error in C (dbLex.l:131-133).
        let input = "record(stringin, \"TEST\") {\n    field(DESC, \"line1\nline2\")\n}\n";
        let res = parse_db(input, &HashMap::new());
        assert!(
            matches!(res, Err(CaError::DbParseError { ref message, .. })
                if message.contains("Newline in string")),
            "expected newline-in-string abort, got {res:?}"
        );
    }

    #[test]
    fn test_quoted_string_backslash_before_newline_aborts() {
        // The NAME rule (dbLex.l:88-92): `{escape}` is `{backslash}.` and flex
        // `.` never matches a newline (`{dqschar}` is `[^"\n\\]`), so a
        // backslash immediately before a newline is NOT an escape — the string
        // stays unterminated and aborts (dbLex.l:131-133).
        //
        // softIoc, `record(stringin,"BN2\<newline>X")`:
        //   ERROR: Invalid character '"' / Invalid character '\' / syntax error
        let input = "record(stringin, \"TE\\\nST\") {\n    field(DESC, \"x\")\n}\n";
        let res = parse_db(input, &HashMap::new());
        assert!(
            matches!(res, Err(CaError::DbParseError { ref message, .. })
                if message.contains("Newline in string")),
            "expected newline-in-string abort, got {res:?}"
        );
    }

    /// The VALUE rule is the other start condition and disagrees on exactly
    /// this character: `escapedchar` is `{backslash}[^ux1-9]` (dbLex.l:25), a
    /// character CLASS, which does match a newline. So a backslash before a
    /// newline is a valid escape in a value, and the translation's default arm
    /// yields the newline itself.
    ///
    /// softIoc, `field(DESC, "line1\<newline>line2")` → `dbgf BN.DESC` prints
    /// `"line1\nline2"` — i.e. a real newline, re-escaped for display.
    #[test]
    fn test_value_backslash_before_newline_is_a_newline() {
        let input = "record(stringin, \"TEST\") {\n    field(DESC, \"line1\\\nline2\")\n}\n";
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].value, "line1\nline2");
    }

    #[test]
    fn test_macro_with_quoted_default_in_string() {
        // C EPICS macLib treats quotes inside $(...) as literal characters.
        // e.g. $(XPOS="") means "default to empty-string pair".
        let input = r#"
    record(longout, "$(P)$(R)PositionXLink") {
    field(DOL, "$(XPOS="") CP MS")
    }
    "#;
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "SIM1:".to_string());
        macros.insert("R".to_string(), "Over1:1:".to_string());
        macros.insert("XPOS".to_string(), "SIM1:ROI1:MinX_RBV".to_string());
        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records[0].fields[0].value, "SIM1:ROI1:MinX_RBV CP MS");
    }

    #[test]
    fn test_macro_with_quoted_default_unset() {
        // When XPOS is not set, $(XPOS="") should expand to "" (literal quotes)
        let input = r#"
    record(longout, "TEST:Link") {
    field(DOL, "$(XPOS="") CP MS")
    }
    "#;
        let macros = HashMap::new();
        let records = parse_db(input, &macros).unwrap();
        // With undefined macro and default="", the field gets the raw default
        assert!(records[0].fields[0].value.as_str_lossy().contains("CP MS"));
    }

    #[test]
    fn test_recursive_macro_default() {
        // $(TS_PORT=$(PORT)_TS) with PORT=ATTR1 → ATTR1_TS
        let input = r#"
    record(stringin, "TEST") {
    field(VAL, "$(TS_PORT=$(PORT)_TS)")
    }
    "#;
        let mut macros = HashMap::new();
        macros.insert("PORT".to_string(), "ATTR1".to_string());
        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records[0].fields[0].value, "ATTR1_TS");
    }

    #[test]
    fn test_substitute_directive_in_expand() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create a simple child template
        let child = dir.path().join("child.db");
        let mut f = std::fs::File::create(&child).unwrap();
        writeln!(f, r#"record(ai, "$(P)$(R)Val") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "$(ADDR)")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        // Create parent with substitute + include
        let parent = dir.path().join("parent.db");
        let mut f = std::fs::File::create(&parent).unwrap();
        writeln!(f, r#"substitute "R=A:,ADDR=0""#).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();
        writeln!(f, r#"substitute "R=B:,ADDR=1""#).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "IOC:".to_string());
        // C `dbOpenFile` searches the path list, never the including
        // file's directory, so the tempdir has to be ON the list.
        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 10,
        };
        let records = parse_db_file(&parent, &macros, &config).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "IOC:A:Val");
        assert_eq!(records[0].fields[0].value, "0");
        assert_eq!(records[1].name, "IOC:B:Val");
        assert_eq!(records[1].fields[0].value, "1");
    }

    #[test]
    fn test_empty_string_numeric_parse() {
        // C EPICS treats empty VAL as 0 for numeric record types
        let input = r#"
    record(longin, "TEST:Int") {
    field(VAL, "")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        // Should parse without error — empty string → 0
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn test_calcout_process() {
        use crate::server::record::Record;
        use crate::server::records::calcout::CalcoutRecord;

        let mut rec = CalcoutRecord::default();
        rec.put_field("CALC", EpicsValue::String("A+B".into()))
            .unwrap();
        // C compiles CALC in special(SPC_CALC), never in the field write —
        // dbPut always runs both, so drive the same pair here.
        rec.special("CALC", true).unwrap();
        rec.put_field("A", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(4.0)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 7.0).abs() < 1e-10),
            other => panic!("expected Double(7.0), got {:?}", other),
        }
    }

    #[test]
    fn test_calcout_oopt() {
        use crate::server::record::Record;
        use crate::server::records::calcout::CalcoutRecord;

        let mut rec = CalcoutRecord::default();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OOPT", EpicsValue::Short(1)).unwrap(); // On Change
        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();

        // First process — value changes from 0 to 5
        rec.process().unwrap();
        assert!((rec.oval - 5.0).abs() < 1e-10);

        // Second process — same value, OVAL should not update (but val still computes)
        rec.process().unwrap();
        // OVAL is still 5.0 since val didn't change
    }

    #[test]
    fn test_calcout_dopt() {
        use crate::server::record::Record;
        use crate::server::records::calcout::CalcoutRecord;

        let mut rec = CalcoutRecord::default();
        rec.put_field("CALC", EpicsValue::String("A+B".into()))
            .unwrap();
        // C compiles CALC in special(SPC_CALC), never in the field write —
        // dbPut always runs both, so drive the same pair here.
        rec.special("CALC", true).unwrap();
        rec.put_field("OCAL", EpicsValue::String("A*B".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use OCAL
        rec.put_field("A", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(4.0)).unwrap();
        rec.process().unwrap();

        // VAL = A+B = 7, OVAL = A*B = 12
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 7.0).abs() < 1e-10),
            other => panic!("expected Double(7.0), got {:?}", other),
        }
        match rec.get_field("OVAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 12.0).abs() < 1e-10),
            other => panic!("expected Double(12.0), got {:?}", other),
        }
    }

    #[test]
    fn test_dfanout_basic() {
        use crate::server::record::Record;
        use crate::server::records::dfanout::DfanoutRecord;

        let mut rec = DfanoutRecord::default();
        rec.put_field("VAL", EpicsValue::Double(42.0)).unwrap();
        assert_eq!(rec.record_type(), "dfanout");
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
    }

    #[test]
    fn test_dfanout_output_links() {
        use crate::server::record::Record;
        use crate::server::records::dfanout::DfanoutRecord;

        let mut rec = DfanoutRecord::default();
        rec.put_field("OUTA", EpicsValue::String("REC_A".into()))
            .unwrap();
        rec.put_field("OUTB", EpicsValue::String("REC_B".into()))
            .unwrap();
        let links = rec.output_links();
        assert_eq!(links.len(), 2);
    }

    #[test]
    fn test_compress_circular_buffer() {
        use crate::server::record::Record;
        use crate::server::records::compress::CompressRecord;

        let mut rec = CompressRecord::new(5, 4); // nsam=5, alg=Circular Buffer
        for i in 0..7 {
            rec.push_value(i as f64);
        }
        // C `get_array_info` linearises FIFO oldest→newest. After
        // 7 pushes to nsam=5: nuse saturates at 5, the last 5 values
        // are [2, 3, 4, 5, 6].
        match rec.get_field("VAL") {
            Some(EpicsValue::DoubleArray(arr)) => {
                assert_eq!(arr, vec![2.0, 3.0, 4.0, 5.0, 6.0]);
            }
            other => panic!("expected DoubleArray, got {:?}", other),
        }
    }

    #[test]
    fn test_compress_n_to_1_mean() {
        use crate::server::record::Record;
        use crate::server::records::compress::CompressRecord;

        let mut rec = CompressRecord::new(10, 2); // alg=Mean
        rec.put_field("N", EpicsValue::Long(3)).unwrap();
        rec.push_value(3.0);
        rec.push_value(6.0);
        rec.push_value(9.0); // mean = 6.0
        match rec.get_field("VAL") {
            Some(EpicsValue::DoubleArray(arr)) => {
                assert!((arr[0] - 6.0).abs() < 1e-10);
            }
            other => panic!("expected DoubleArray, got {:?}", other),
        }
    }

    #[test]
    fn test_histogram_bucket_count() {
        use crate::server::records::histogram::HistogramRecord;

        let mut rec = HistogramRecord::new(10, 0.0, 10.0);
        rec.add_sample(2.5); // bucket 2
        rec.add_sample(2.7); // bucket 2
        // C `histogramRecord.c:340-345` selects the bucket with a
        // closed upper edge (`temp <= i*wdth`): a value exactly on a
        // boundary lands in the LOWER bucket. sgnl=7.0, wdth=1.0 ->
        // i=7 is the first `7.0 <= i*1.0`, dest = i-1 = bucket 6.
        rec.add_sample(7.0); // boundary value -> bucket 6 (C parity)
        assert_eq!(rec.val[2], 2);
        assert_eq!(rec.val[6], 1);
    }

    #[test]
    fn test_histogram_out_of_range() {
        use crate::server::records::histogram::HistogramRecord;

        let mut rec = HistogramRecord::new(10, 0.0, 10.0);
        rec.add_sample(-1.0); // below range
        rec.add_sample(10.0); // at upper limit (excluded)
        rec.add_sample(15.0); // above range
        let total: u32 = rec.val.iter().sum();
        assert_eq!(total, 0);
    }

    #[test]
    fn test_sel_specified() {
        use crate::server::record::Record;
        use crate::server::records::sel::SelRecord;

        let mut rec = SelRecord::default();
        rec.put_field("SELM", EpicsValue::Short(0)).unwrap(); // Specified
        rec.put_field("SELN", EpicsValue::Short(2)).unwrap(); // Select C
        rec.put_field("C", EpicsValue::Double(99.0)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 99.0).abs() < 1e-10),
            other => panic!("expected Double(99.0), got {:?}", other),
        }
    }

    #[test]
    fn test_sel_high_low_median() {
        use crate::server::record::Record;
        use crate::server::records::sel::SelRecord;

        let mut rec = SelRecord::default();
        rec.put_field("A", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(30.0)).unwrap();
        rec.put_field("C", EpicsValue::Double(20.0)).unwrap();

        // High
        rec.put_field("SELM", EpicsValue::Short(1)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 30.0).abs() < 1e-10),
            other => panic!("expected Double(30.0), got {:?}", other),
        }

        // Low
        rec.put_field("SELM", EpicsValue::Short(2)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 10.0).abs() < 1e-10), // min of finite values (A=10,B=30,C=20)
            other => panic!("expected near 0.0, got {:?}", other),
        }
    }

    #[test]
    fn db_load_menu_labels_resolve_against_field_menu() {
        // A DBF_MENU field's `.db` value resolves against THAT field's own
        // menu (C dbStaticLib dbPutStringNum: label-first, then a numeric
        // index), not a cross-menu global table. Before the fix, the loader
        // routed every menu label through one global table that mis-mapped
        // menu-specific labels and dropped labels it did not carry.
        use crate::server::record::Record;

        let apply = |rt: &str, field: &str, value: &str| -> CaResult<Box<dyn Record>> {
            let mut rec = create_record(rt).unwrap();
            let mut common = Vec::new();
            apply_fields(&mut rec, &[DbFieldDef::new(field, value)], &mut common)?;
            Ok(rec)
        };

        // sel.SELM (record-specific menu selSELM): "Specified" is index 0.
        // The old global table returned 1 (from menuFanout's "Specified").
        let rec = apply("sel", "SELM", "Specified").unwrap();
        assert_eq!(rec.get_field("SELM"), Some(EpicsValue::Enum(0)));

        // A later choice proves the whole menu, not just index 0.
        let rec = apply("sel", "SELM", "High Signal").unwrap();
        assert_eq!(rec.get_field("SELM"), Some(EpicsValue::Enum(1)));

        // A bare numeric index still resolves (C epicsParseUInt16 fallback).
        let rec = apply("sel", "SELM", "2").unwrap();
        assert_eq!(rec.get_field("SELM"), Some(EpicsValue::Enum(2)));

        // ai.LINR (shared menu menuConvert, SHORT-typed): "LINEAR" is index 2.
        // The old global table lacked menuConvert, so the load errored.
        let rec = apply("ai", "LINR", "LINEAR").unwrap();
        assert_eq!(rec.get_field("LINR"), Some(EpicsValue::Short(2)));

        // An unknown choice errors (C S_db_badChoice), never a silent mis-map.
        assert!(apply("sel", "SELM", "Bogus").is_err());
    }

    #[test]
    fn test_parse_breaktable_basic() {
        let input = r#"
breaktable(typeJdegC) {
    0    0
    365  67.0
    1000 178.0
}
record(ai, "T") { field(LINR, "typeJdegC") }
"#;
        let parsed = parse_db_with_breaktables(input, &HashMap::new()).unwrap();
        let breaktables = parsed.breaktables;
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(breaktables.len(), 1);
        let t = &breaktables[0];
        assert_eq!(t.name, "typeJdegC");
        assert_eq!(t.points.len(), 3);
        assert_eq!(t.points[0].raw, 0.0);
        assert_eq!(t.points[0].eng, 0.0);
        // slope[0] = (67-0)/(365-0).
        assert!((t.points[0].slope - (67.0 / 365.0)).abs() < 1e-12);
    }

    #[test]
    fn test_parse_breaktable_quoted_name_and_commas() {
        let input = r#"breaktable("tbl") { 0,0, 10,100 }"#;
        let breaktables = parse_db_with_breaktables(input, &HashMap::new())
            .unwrap()
            .breaktables;
        assert_eq!(breaktables.len(), 1);
        assert_eq!(breaktables[0].name, "tbl");
        assert_eq!(breaktables[0].points.len(), 2);
        assert!((breaktables[0].points[0].slope - 10.0).abs() < 1e-12);
    }

    #[test]
    fn test_parse_breaktable_odd_count_errors() {
        let input = r#"breaktable(bad) { 0 0 10 }"#;
        let Err(err) = parse_db_with_breaktables(input, &HashMap::new()) else {
            panic!("odd point count must be a parse error");
        };
        match err {
            CaError::DbParseError { message, .. } => {
                assert!(message.contains("Raw value missing"), "{message}")
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn test_resolve_linr_breaktable_names_rewrites_to_index() {
        use crate::server::cvt_bpt::{BreakTableRegistry, BrkTable};
        let mut reg = BreakTableRegistry::new();
        reg.insert(BrkTable::build("alpha", &[(0.0, 0.0), (1.0, 1.0)]).unwrap());

        // A registered non-standard table name on an ai/ao LINR is rewritten to
        // its index — the first user-table slot (15), since the standard
        // menuConvert names reserve 3..=14.
        let mut fields = vec![DbFieldDef::new("LINR", PvString::from("alpha"))];
        resolve_linr_breaktable_names("ai", &mut fields, &reg);
        assert_eq!(fields[0].value, "15");

        // A fixed menuConvert label matches no table name and is untouched.
        let mut fixed = vec![DbFieldDef::new("LINR", PvString::from("LINEAR"))];
        resolve_linr_breaktable_names("ai", &mut fixed, &reg);
        assert_eq!(fixed[0].value, "LINEAR");

        // A non-ai/ao record's field is never rewritten.
        let mut other = vec![DbFieldDef::new("LINR", PvString::from("alpha"))];
        resolve_linr_breaktable_names("bo", &mut other, &reg);
        assert_eq!(other[0].value, "alpha");
    }

    #[test]
    fn test_sub_record_register_and_call() {
        use crate::server::record::{Record, RecordInstance, SubroutineFn};
        use crate::server::records::sub_record::SubRecord;
        use std::sync::Arc;

        let mut rec = SubRecord::default();
        rec.put_field("SNAM", EpicsValue::String("double_val".into()))
            .unwrap();
        rec.put_field("VAL", EpicsValue::Double(5.0)).unwrap();

        let mut instance = RecordInstance::new("TEST_SUB".into(), rec);
        let sub_fn: SubroutineFn = Box::new(|record: &mut dyn Record| {
            if let Some(EpicsValue::Double(v)) = record.get_field("VAL") {
                record.put_field("VAL", EpicsValue::Double(v * 2.0))?;
            }
            Ok(0)
        });
        instance.subroutine = Some(Arc::new(sub_fn));

        instance.process_local().unwrap();

        match instance.record.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 10.0).abs() < 1e-10),
            other => panic!("expected Double(10.0), got {:?}", other),
        }
    }

    #[test]
    fn test_new_record_types_in_db() {
        let input = r#"
    record(calcout, "TEST_CO") {
    field(CALC, "A+1")
    }
    record(dfanout, "TEST_DF") {
    field(VAL, "5.0")
    }
    record(compress, "TEST_CMP") {
    field(DESC, "test compress")
    }
    record(histogram, "TEST_HIST") {
    field(DESC, "test hist")
    }
    record(sel, "TEST_SEL") {
    field(SELM, "0")
    }
    record(sub, "TEST_SUB") {
    field(SNAM, "my_sub")
    }
    "#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 6);
        // Verify they can all be created
        for def in &records {
            create_record(&def.record_type).unwrap();
        }
    }

    // ===== include / parse_db_file tests =====

    #[test]
    fn test_parse_include_directive() {
        // Normal include
        assert_eq!(
            parse_include_directive(r#"include "foo.template""#),
            Some("foo.template".to_string())
        );
        // With leading whitespace
        assert_eq!(
            parse_include_directive(r#"  include "bar.db""#),
            Some("bar.db".to_string())
        );
        // With trailing comment
        assert_eq!(
            parse_include_directive(r#"include "baz.template" # a comment"#),
            Some("baz.template".to_string())
        );
        // No quote — not an include
        assert_eq!(parse_include_directive("include something"), None);
        // Comment line
        assert_eq!(parse_include_directive(r#"# include "ignored.db""#), None);
        // Not an include keyword
        assert_eq!(parse_include_directive("record(ai, \"X\") {"), None);
        // "includes" is not "include"
        assert_eq!(parse_include_directive(r#"includes "nope.db""#), None);
    }

    #[test]
    fn test_commented_include_ignored() {
        assert_eq!(parse_include_directive(r#"# include "file.db""#), None);
        assert_eq!(parse_include_directive(r#"  # include "file.db""#), None);
    }

    #[test]
    fn test_expand_includes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create child.db
        let child_path = dir.path().join("child.db");
        let mut f = std::fs::File::create(&child_path).unwrap();
        writeln!(f, r#"record(ai, "CHILD") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "1.0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        // Create parent.db that includes child.db
        let parent_path = dir.path().join("parent.db");
        let mut f = std::fs::File::create(&parent_path).unwrap();
        writeln!(f, r#"record(ao, "PARENT") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "2.0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let result = expand_includes(
            &parent_path,
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        )
        .unwrap();
        assert!(result.contains(r#"record(ao, "PARENT")"#));
        assert!(result.contains(r#"record(ai, "CHILD")"#));

        // Verify it parses correctly
        let records = parse_db(&result, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn test_circular_include_error() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let a_path = dir.path().join("a.template");
        let b_path = dir.path().join("b.template");

        let mut fa = std::fs::File::create(&a_path).unwrap();
        writeln!(fa, r#"include "b.template""#).unwrap();

        let mut fb = std::fs::File::create(&b_path).unwrap();
        writeln!(fb, r#"include "a.template""#).unwrap();

        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let result = expand_includes(&a_path, &HashMap::new(), &config, &mut DbFaults::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("circular include"), "error was: {err}");
    }

    #[test]
    fn test_duplicate_include_allowed() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let shared_path = dir.path().join("shared.db");
        let mut f = std::fs::File::create(&shared_path).unwrap();
        writeln!(f, r#"record(ai, "SHARED") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        // main.db includes shared.db twice (not circular, just duplicate)
        let main_path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main_path).unwrap();
        writeln!(f, r#"include "shared.db""#).unwrap();
        writeln!(f, r#"include "shared.db""#).unwrap();

        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let result = expand_includes(
            &main_path,
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        )
        .unwrap();
        // shared.db content appears twice
        assert_eq!(result.matches(r#"record(ai, "SHARED")"#).count(), 2);
    }

    #[test]
    fn test_include_depth_limit() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create a chain: file0 -> file1 -> file2 -> ... -> file33
        for i in 0..34 {
            let path = dir.path().join(format!("file{i}.db"));
            let mut f = std::fs::File::create(&path).unwrap();
            if i < 33 {
                writeln!(f, r#"include "file{}.db""#, i + 1).unwrap();
            } else {
                writeln!(f, r#"record(ai, "DEEP") {{"#).unwrap();
                writeln!(f, r#"    field(VAL, "0")"#).unwrap();
                writeln!(f, r#"}}"#).unwrap();
            }
        }

        let config = DbLoadConfig {
            include_paths: vec![dir.path().to_path_buf()],
            max_include_depth: 32,
        };
        let result = expand_includes(
            &dir.path().join("file0.db"),
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("depth limit"), "error was: {err}");
    }

    /// R5-3: C `dbIncludeNew` (`dbLexRoutines.c:450-456`) reports an
    /// unopenable include with `yyerror(NULL)`, so the include is skipped
    /// and the rest of the file is still read. This test used to assert
    /// the opposite — that the whole expansion failed.
    #[test]
    fn test_include_not_found_is_recoverable() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"record(ai, "SIM:A") {{ field(VAL, "1") }}"#).unwrap();
        writeln!(f, r#"include "nonexistent.db""#).unwrap();
        writeln!(f, r#"record(ai, "SIM:B") {{ field(VAL, "2") }}"#).unwrap();

        let config = DbLoadConfig::default();
        let mut faults = DbFaults::default();
        let text = expand_includes(&path, &HashMap::new(), &config, &mut faults)
            .expect("a missing include must not fail the expansion");
        assert!(text.contains("SIM:A") && text.contains("SIM:B"));
        let err = faults
            .first_diagnostic()
            .expect("the load's status must go non-zero");
        assert_eq!(
            err,
            format!("{ERL_ERROR}: Can't open include file 'nonexistent.db'")
        );
    }

    #[test]
    fn test_include_with_macro_filename() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir).unwrap();

        let child_path = subdir.join("child.db");
        let mut f = std::fs::File::create(&child_path).unwrap();
        writeln!(f, r#"record(ai, "CHILD") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let main_path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main_path).unwrap();
        writeln!(f, r#"include "$(DIR)/child.db""#).unwrap();

        let mut macros = HashMap::new();
        macros.insert("DIR".to_string(), macro_safe_path(&subdir));

        let config = DbLoadConfig::default();
        let result =
            expand_includes(&main_path, &macros, &config, &mut DbFaults::default()).unwrap();
        assert!(result.contains(r#"record(ai, "CHILD")"#));
    }

    #[test]
    fn test_include_search_order() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let inc_dir = dir.path().join("inc");
        std::fs::create_dir(&inc_dir).unwrap();

        // Put file in include path only (not in current dir)
        let child_path = inc_dir.join("child.db");
        let mut f = std::fs::File::create(&child_path).unwrap();
        writeln!(f, r#"record(ai, "FROM_INC") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let main_path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main_path).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        let config = DbLoadConfig {
            include_paths: vec![inc_dir.clone()],
            max_include_depth: 32,
        };
        let result = expand_includes(
            &main_path,
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        )
        .unwrap();
        assert!(result.contains(r#"record(ai, "FROM_INC")"#));

        // I-R3-3: a same-named file next to the INCLUDING file must not
        // win. C `dbIncludeNew` (`dbLexRoutines.c:450`) goes through
        // `dbOpenFile`, which walks the path list and never looks at the
        // parent file's directory or the process CWD.
        let local_child = dir.path().join("child.db");
        let mut f = std::fs::File::create(&local_child).unwrap();
        writeln!(f, r#"record(ai, "FROM_LOCAL") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let result = expand_includes(
            &main_path,
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        )
        .unwrap();
        assert!(result.contains(r#"record(ai, "FROM_INC")"#));
        assert!(!result.contains(r#"record(ai, "FROM_LOCAL")"#));

        // …and a name that already carries a separator bypasses the list
        // entirely, exactly as C's `strchr(filename, '/')` gate does.
        let sep_main = dir.path().join("sep_main.db");
        let mut f = std::fs::File::create(&sep_main).unwrap();
        writeln!(f, r#"include "{}""#, local_child.display()).unwrap();
        let result = expand_includes(
            &sep_main,
            &HashMap::new(),
            &config,
            &mut DbFaults::default(),
        )
        .unwrap();
        assert!(result.contains(r#"record(ai, "FROM_LOCAL")"#));
    }

    #[test]
    fn test_addpath_directive_resolves_include() {
        // L-3: an `addpath` directive inside a .db file mutates the
        // include search path for subsequent `include` directives.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let inc_dir = dir.path().join("extra");
        std::fs::create_dir(&inc_dir).unwrap();

        // child.db lives ONLY in the extra/ dir.
        let child = inc_dir.join("child.db");
        let mut f = std::fs::File::create(&child).unwrap();
        writeln!(f, r#"record(ai, "FROM_ADDPATH") {{ field(VAL, "0") }}"#).unwrap();

        let main = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main).unwrap();
        writeln!(f, r#"addpath "{}""#, inc_dir.display()).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        // No include path in config — only the addpath directive can
        // make this resolve.
        let config = DbLoadConfig::default();
        let result =
            expand_includes(&main, &HashMap::new(), &config, &mut DbFaults::default()).unwrap();
        assert!(result.contains(r#"record(ai, "FROM_ADDPATH")"#));
    }

    #[test]
    fn test_path_directive_replaces_search_path() {
        // L-3: `path` replaces the search path entirely.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let inc_dir = dir.path().join("p");
        std::fs::create_dir(&inc_dir).unwrap();

        let child = inc_dir.join("c.db");
        let mut f = std::fs::File::create(&child).unwrap();
        writeln!(f, r#"record(ai, "VIA_PATH") {{ field(VAL, "0") }}"#).unwrap();

        let main = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main).unwrap();
        writeln!(f, r#"path "{}""#, inc_dir.display()).unwrap();
        writeln!(f, r#"include "c.db""#).unwrap();

        let config = DbLoadConfig::default();
        let result =
            expand_includes(&main, &HashMap::new(), &config, &mut DbFaults::default()).unwrap();
        assert!(result.contains(r#"record(ai, "VIA_PATH")"#));
    }

    /// A `DTYP=` macro is pure text substitution, exactly like every other
    /// macro: it reaches a record only where the `.db` wrote
    /// `field(DTYP,"$(DTYP)")`. It must NOT rewrite a record that spells its
    /// DTYP literally.
    ///
    /// This replaces `test_dtyp_override_existing_only`, which pinned the
    /// opposite (force-override) behaviour. C `dbLoadRecords` runs its macros
    /// through macLib during lexing (`dbLexRoutines.c`); there is no DTYP
    /// special case, and a force-override corrupts any multi-record file whose
    /// helper records carry a literal DTYP — e.g. the vendored
    /// `scaler-rs/db/scaler.db`, where two `bo` helpers are
    /// `field(DTYP,"Soft Channel")` and only the `scaler` record references
    /// `$(DTYP)`.
    #[test]
    fn dtyp_macro_substitutes_references_and_leaves_literals_alone() {
        let input = r#"
record(bo, "$(P)_calcEnable") {
    field(DTYP, "Soft Channel")
    field(ZNAM, "ENABLE")
}
record(scaler, "$(P)") {
    field(DTYP, "$(DTYP)")
    field(FREQ, "10000000")
}
record(ao, "$(P)_noDtyp") {
    field(VAL, "1")
}
"#;
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "SCALER1".to_string());
        macros.insert("DTYP".to_string(), "Scaler-rs".to_string());

        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records.len(), 3);

        let dtyp_of = |rec: &DbRecordDef| -> Option<String> {
            rec.fields
                .iter()
                .find(|f| f.name == "DTYP")
                .map(|f| f.value.as_str_lossy().into_owned())
        };

        // Literal DTYP: untouched by the macro. Force-override used to corrupt
        // this into "Scaler-rs", breaking the soft helper records.
        assert_eq!(dtyp_of(&records[0]).as_deref(), Some("Soft Channel"));
        // $(DTYP) reference: substituted.
        assert_eq!(dtyp_of(&records[1]).as_deref(), Some("Scaler-rs"));
        // No DTYP field: none added.
        assert_eq!(dtyp_of(&records[2]), None);
    }

    #[test]
    fn test_parse_db_file_no_includes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("simple.db");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"record(ai, "$(P)TEMP") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "25.0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "IOC:".to_string());

        let config = DbLoadConfig::default();
        let records = parse_db_file(&path, &macros, &config).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "IOC:TEMP");
    }

    // epics-base PR #78 — record-name validation regressions.

    #[test]
    fn name_validation_accepts_typical_names() {
        for n in [
            "IOC:TEMP",
            "MOTOR-1",
            "X[1]",
            "scan_5",
            "BL3:STD:01",
            "abc.xyz_record",
        ]
        .iter()
        .copied()
        // ".xyz" inside the bracket-allowed shape would actually fail
        // — the slash above is just to demonstrate the OK/FAIL split.
        {
            // Skip the deliberately-bad sample.
            if n.contains('.') {
                assert!(validate_record_name(n, 1, 1).is_err());
                continue;
            }
            validate_record_name(n, 1, 1).unwrap_or_else(|e| panic!("'{n}' should pass: {e:?}"));
        }
    }

    #[test]
    fn name_validation_rejects_empty() {
        assert!(validate_record_name("", 1, 1).is_err());
    }

    #[test]
    fn name_validation_rejects_bad_chars() {
        // Mirrors base: TAB (0x09) is non-printable so it falls into the
        // warn-and-continue branch, NOT the hard-error set. Only the
        // printable bad chars below produce a hard error.
        for bad in ["spa ce", "do.t", "qu\"ot", "ap'os", "do$llar"] {
            assert!(
                validate_record_name(bad, 1, 1).is_err(),
                "'{bad}' must be rejected"
            );
        }
    }

    #[test]
    fn name_validation_warns_but_passes_on_nonprintable() {
        // 0x09 (TAB) and 0x01 are < 0x20 so they only emit a warning.
        validate_record_name("ta\tb", 1, 1).expect("TAB is warn-only per base spec");
        validate_record_name("hello\x01world", 1, 1).expect("0x01 is warn-only");
    }

    #[test]
    fn name_validation_warns_on_leading_special_but_passes() {
        for warn in ["-x", "+y", "[arr", "{obj"] {
            validate_record_name(warn, 1, 1).expect("leading special is warn-only");
        }
    }

    /// R5-1: a bare `[...]` array constant is a `json_value`
    /// (`dbYacc.y:328-337`), so C loads this record. The port had no `[`
    /// branch in `read_field_value`, consumed `[1` as a bareword and
    /// stopped at the comma, so the whole file parsed to nothing. Both
    /// lines are transcribed from base's own test databases —
    /// `modules/database/test/std/rec/arrayOpTest.db:4` and
    /// `linkInitTest.db:15`.
    #[test]
    fn parse_db_accepts_a_bare_array_constant() {
        let src = r#"
record(waveform, "wfrec") {
    field(NELM, "10")
    field(FTVL, "LONG")
    field(INP, [1, 2, 3])
}
record(lsi, "longstr4") {
  field(SIZV, "100")
  field(INP, ["One","Two","Three","Four"])
}
"#;
        let recs = parse_db(src, &HashMap::new()).expect("base's own test database must load");
        assert_eq!(recs.len(), 2);
        let inp = |r: &DbRecordDef| {
            r.fields
                .iter()
                .find(|f| f.name == "INP")
                .map(|f| f.value.as_str_lossy().into_owned())
                .unwrap()
        };
        assert_eq!(inp(&recs[0]), "[1, 2, 3]");
        assert_eq!(inp(&recs[1]), r#"["One","Two","Three","Four"]"#);
    }

    #[test]
    fn parse_db_propagates_name_validation_error() {
        let bad = r#"record(ai, "BAD NAME") { }"#;
        let res = parse_db(bad, &HashMap::new());
        assert!(matches!(res, Err(CaError::DbParseError { .. })));
    }

    // epics-base PR #336 — alias parsing + name validation.

    #[test]
    fn parse_db_captures_aliases() {
        let src = r#"record(ai, "TARGET") {
            alias("ALIAS1")
            alias("ALIAS2")
            field(VAL, 42)
        }"#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "TARGET");
        assert_eq!(recs[0].aliases, vec!["ALIAS1", "ALIAS2"]);
        assert_eq!(recs[0].fields.len(), 1);
    }

    #[test]
    fn parse_db_rejects_alias_with_bad_name() {
        let src = r#"record(ai, "TARGET") {
            alias("BAD ALIAS")
        }"#;
        let res = parse_db(src, &HashMap::new());
        assert!(matches!(res, Err(CaError::DbParseError { .. })));
    }

    // L-2 — top-level directive grammar coverage.

    #[test]
    fn parse_db_accepts_path_and_addpath() {
        // `path`/`addpath` at file scope are accepted and skipped —
        // include resolution is the expansion layer's job.
        let src = r#"
            path "/opt/epics/db"
            addpath "/extra/db"
            record(ai, "REC") { field(VAL, "1") }
        "#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "REC");
    }

    #[test]
    fn parse_db_accepts_top_level_include() {
        // A bare `include` at file scope is accepted (grammar parity);
        // the file is not loaded at this layer.
        let src = r#"
            include "common.db"
            record(ai, "REC") { field(VAL, "1") }
        "#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
    }

    #[test]
    fn parse_db_global_alias_two_arg() {
        // Standalone `alias("record","newname")` attaches the new name
        // to the target record's alias list.
        let src = r#"
            record(ai, "TARGET") { field(VAL, "1") }
            alias("TARGET", "TARGET_ALIAS")
        "#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].aliases, vec!["TARGET_ALIAS"]);
    }

    #[test]
    fn parse_db_global_alias_forward_reference() {
        // The alias directive may precede its target record.
        let src = r#"
            alias("TARGET", "EARLY_ALIAS")
            record(ai, "TARGET") { field(VAL, "1") }
        "#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs[0].aliases, vec!["EARLY_ALIAS"]);
    }

    /// I-R3-5: C `dbYacc.y:237-241` puts `record_body: /* empty */`
    /// FIRST, ahead of `'{' '}'` and `'{' record_field_list '}'`, and
    /// `tokenRECORD` / `tokenGRECORD` (`:60-61`) share it — so a
    /// bodyless declaration is an all-defaults record. The port
    /// demanded `{` and the rejection discarded every record in the
    /// file.
    #[test]
    fn parse_db_record_without_a_body_is_valid() {
        let src = r#"
record(ai, "TEMP")
record(ai, "PRESS") { field(SCAN, "1 second") }
grecord(ai, "GTEMP")
record(ai, "EMPTY") { }
"#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        let names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["TEMP", "PRESS", "GTEMP", "EMPTY"]);
        assert!(recs[0].fields.is_empty(), "bodyless record takes defaults");
        assert_eq!(recs[1].fields.len(), 1);
        assert!(recs[2].fields.is_empty(), "grecord shares record_body");
        assert!(recs[3].fields.is_empty());

        // Boundary: the bodyless record is the last token in the file,
        // so there is nothing after the head at all.
        let recs = parse_db(r#"record(ai, "LAST")"#, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "LAST");
    }

    /// I-R3-4: C `dbAlias` resolves the target against `savedPdbbase`
    /// (`dbLexRoutines.c:1508`), the accumulated database, so a target
    /// this text does not declare is not the parser's to reject — an
    /// earlier `dbLoadRecords` may own it. The parser hands it out
    /// unresolved and keeps every record it read; C's own failure path
    /// (`yyerror(NULL)`, `dbYacc.y:370-383`) also keeps them.
    #[test]
    fn parse_db_global_alias_unknown_record_is_deferred_not_rejected() {
        let src = r#"
            alias("NOSUCH", "X")
            record(ai, "KEPT") { field(VAL, "1") }
        "#;
        let parsed = parse_db_with_breaktables(src, &HashMap::new()).unwrap();
        assert_eq!(
            parsed.unresolved_aliases,
            vec![("NOSUCH".to_string(), "X".to_string())]
        );
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].name, "KEPT");
        // The records-only wrapper still yields what parsed.
        assert_eq!(parse_db(src, &HashMap::new()).unwrap().len(), 1);
    }

    // L-5 — unquoted field values are restricted to the C bareword set.

    #[test]
    fn parse_db_unquoted_bareword_value_ok() {
        let src = r#"record(ai, "REC") { field(VAL, 42) field(EGU, deg-C) }"#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs[0].fields[0].value, "42");
        assert_eq!(recs[0].fields[1].value, "deg-C");
    }

    #[test]
    fn parse_db_unquoted_value_with_space_rejected() {
        // An unquoted multi-word value is two tokens in C — a parse
        // error. The author must quote it.
        let src = r#"record(ai, "REC") { field(DESC, hello world) }"#;
        let res = parse_db(src, &HashMap::new());
        assert!(
            matches!(res, Err(CaError::DbParseError { .. })),
            "unquoted value with space must be rejected, got {res:?}"
        );
    }

    #[test]
    fn parse_db_unquoted_value_with_illegal_char_rejected() {
        // `*` is outside the C bareword set.
        let src = r#"record(ai, "REC") { field(DESC, a*b) }"#;
        let res = parse_db(src, &HashMap::new());
        assert!(matches!(res, Err(CaError::DbParseError { .. })));
    }

    #[test]
    fn parse_db_record_without_alias_has_empty_aliases() {
        let src = r#"record(ai, "PLAIN") { field(VAL, 1) }"#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert!(recs[0].aliases.is_empty());
    }

    /// Regression: `info(asyn:READBACK, "1")` (unquoted tag,
    /// quoted value) is the form ad-core templates use. Base accepts
    /// it; an earlier parser fix tightened the grammar to require a
    /// quoted tag, which broke all NDOverlayN / NDFile / NDArrayBase
    /// templates and the mini-beamline/xrt-beamline IOCs that load
    /// commonPlugins.cmd.
    #[test]
    fn parse_db_info_accepts_unquoted_tag() {
        let src = r#"
record(ai, "REC") {
    field(VAL, "0")
    info(asyn:READBACK, "1")
    info("Q:group", "demo")
    info(autosaveFields, "VAL DESC")
}
"#;
        let recs = parse_db(src, &HashMap::new()).unwrap();
        assert_eq!(recs.len(), 1);
        let tags = &recs[0].info_tags;
        // Unquoted tag, quoted value.
        assert!(
            tags.iter().any(|(k, v)| k == "asyn:READBACK" && v == "1"),
            "unquoted tag must parse: {tags:?}"
        );
        // Quoted tag, quoted value (existing form).
        assert!(tags.iter().any(|(k, v)| k == "Q:group" && v == "demo"));
        // Unquoted tag, unquoted multi-word value.
        assert!(
            tags.iter()
                .any(|(k, v)| k == "autosaveFields" && v == "VAL DESC"),
            "unquoted multi-word value must parse: {tags:?}"
        );
    }

    /// C parity (modules/libcom/test/macLib.plt:52):
    ///   $(a)\$(b)  with a=foo  ->  foo\$(b)
    /// The `\` MUST block the macro-reference detection of the following
    /// `$` so `$(b)` survives as literal text. The `\` itself is
    /// preserved verbatim (macLib level 0 semantic — downstream
    /// caller-side escape passes may discard it).
    #[test]
    fn substitute_macros_backslash_escapes_dollar() {
        let mut macros = HashMap::new();
        macros.insert("a".to_string(), "foo".to_string());
        macros.insert("b".to_string(), "baz".to_string());

        // Anchor test: backslash before $ blocks expansion.
        assert_eq!(substitute_macros(r"$(a)\$(b)", &macros), r"foo\$(b)");

        // Backslash before brace form too.
        assert_eq!(substitute_macros(r"\${a}", &macros), r"\${a}");

        // Without backslash, both expand.
        assert_eq!(substitute_macros("$(a)$(b)", &macros), "foobaz");

        // Backslash escape consumes the IMMEDIATELY next char (one
        // step at a time, matching C macCore.c:741-744). So `\\$(a)`
        // emits the first `\` + second `\` literal (escape pair), then
        // resumes parsing at `$(a)` which expands. Result: `\\foo`.
        assert_eq!(substitute_macros(r"\\$(a)", &macros), r"\\foo");

        // Backslash NOT before $ / { passes through too (escape any next char).
        assert_eq!(
            substitute_macros(r"path\file $(a)", &macros),
            r"path\file foo"
        );
    }

    // Resolved macro values are re-expanded (chained).
    #[test]
    fn substitute_macros_chained_expansion() {
        // P=$(Q), Q=IOC:  →  $(P) expands to IOC:
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "$(Q)".to_string());
        macros.insert("Q".to_string(), "IOC:".to_string());
        assert_eq!(substitute_macros("$(P)TEMP", &macros), "IOC:TEMP");
    }

    // `$(name,key=val,...)` scoped macro definitions.
    #[test]
    fn substitute_macros_scoped_definitions() {
        // INNER references A and B which are only defined for the
        // duration of this reference.
        let mut macros = HashMap::new();
        macros.insert("INNER".to_string(), "$(A)-$(B)".to_string());
        assert_eq!(substitute_macros("$(INNER,A=1,B=2)", &macros), "1-2");
    }

    // `expand_macros` reports every undefined macro so a hard-fail
    // caller (autosave) can surface it; the text still carries the
    // C `$(name,undefined)` placeholder for the no-fail callers.
    #[test]
    fn expand_macros_reports_undefined_names() {
        let macros = HashMap::new();
        let r = expand_macros("$(A)$(B=def)$(C)", &macros, MacroExpandOptions::default());
        assert_eq!(r.text, "$(A,undefined)def$(C,undefined)");
        // B had a default → not undefined; A and C are, in scan order.
        assert_eq!(r.undefined, vec!["A".to_string(), "C".to_string()]);
    }

    /// C `macCore.c:701-703` — "discard quotes and escapes if level
    /// is > 0 (i.e. if these aren't the user's quotes and escapes)".
    /// One case per boundary of that rule, both sides of level 0,
    /// measured against `macExpandString` from libCom 3.25.1.
    #[test]
    fn quotes_and_escapes_survive_only_at_level_zero() {
        let mut macros = HashMap::new();
        macros.insert("X".to_string(), "1".to_string());
        let l0 = |src: &str| expand_macros(src, &macros, MacroExpandOptions::default()).text;
        let l1 = |raw: &str| {
            let mut m = macros.clone();
            m.insert("M".to_string(), raw.to_string());
            expand_macros("$(M)", &m, MacroExpandOptions::default()).text
        };

        for (src, at_l0, at_l1) in [
            (r#""abc""#, r#""abc""#, "abc"),
            ("'abc'", "'abc'", "abc"),
            (r"a\b", r"a\b", "ab"),
            (r"a\\b", r"a\\b", r"a\b"),
            (r#""$(X)""#, r#""1""#, "1"),
            ("'$(X)'", "'$(X)'", "$(X)"),
            ("Beam's energy", "Beam's energy", "Beams energy"),
            (r#"a"b"#, r#"a"b"#, "ab"),
            ("$(X)'s", "1's", "1s"),
        ] {
            assert_eq!(l0(src), at_l0, "level 0 keeps the user's own: {src}");
            assert_eq!(l1(src), at_l1, "level 1 discards: {src}");
        }

        // A default value is translated at `level + 1` too
        // (`macCore.c:909`), so every quote in it goes — not just a
        // surrounding pair, which is what the port used to strip.
        assert_eq!(l0("$(NOSUCH=Beam's energy)"), "Beams energy");
        assert_eq!(l0(r"$(NOSUCH=a\b)"), "ab");
        assert_eq!(l0(r#"$(NOSUCH="q")"#), "q");
    }

    /// The `.db` shape the rule changes: a field default carrying an
    /// apostrophe. C loads `Beams energy` because the default is
    /// translated below level 0; the port used to load the apostrophe.
    #[test]
    fn a_db_field_default_loses_its_apostrophe_like_c() {
        let recs = parse_db(
            "record(ai,\"X:B\") { field(DESC, \"$(BEAM=Beam's energy)\") }",
            &HashMap::new(),
        )
        .expect("record must parse");
        assert_eq!(recs.len(), 1);
        let desc = recs[0]
            .fields
            .iter()
            .find(|f| f.name == "DESC")
            .map(|f| f.value.to_string());
        assert_eq!(desc.as_deref(), Some("Beams energy"));
    }

    // The default options match `.db` parse semantics: no env fallback,
    // and `$$` is NOT an escape (macLib leaves it verbatim).
    #[test]
    fn expand_macros_default_opts_leave_dollar_dollar_verbatim() {
        let macros = HashMap::new();
        assert_eq!(substitute_macros("$$100", &macros), "$$100");
        let r = expand_macros("$$100", &macros, MacroExpandOptions::default());
        assert_eq!(r.text, "$$100");
        assert!(r.undefined.is_empty());
    }

    // `env_fallback` resolves an otherwise-unset name from the process
    // environment (C `macCreateHandle(&h, environ)`), at the same level
    // as a defined macro — before any default.
    #[test]
    fn expand_macros_env_fallback_opt_in() {
        let var = "_EPICS_BASE_RS_MACRO_ENV_TEST";
        // Off by default: unset macro stays undefined even with the env set.
        unsafe { std::env::set_var(var, "FROM_ENV") };
        let macros = HashMap::new();
        let off = expand_macros(&format!("$({var})"), &macros, MacroExpandOptions::default());
        assert_eq!(off.text, format!("$({var},undefined)"));
        // On: resolves from the environment.
        let on = expand_macros(
            &format!("$({var})"),
            &macros,
            MacroExpandOptions {
                env_fallback: true,
                ..MacroExpandOptions::default()
            },
        );
        assert_eq!(on.text, "FROM_ENV");
        assert!(on.undefined.is_empty());
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn substitute_macros_scoped_not_leaking() {
        // A scoped macro must not leak past its reference.
        let mut macros = HashMap::new();
        macros.insert("INNER".to_string(), "$(A)".to_string());
        // After the scoped $(INNER,A=9), a bare $(A) is undefined.
        let out = substitute_macros("$(INNER,A=9)|$(A)", &macros);
        assert_eq!(out, "9|$(A,undefined)");
    }

    // Quote state resets at every newline (C expands one fgets line per
    // `macExpandString` call, dbLexRoutines.c:375-391): a quote character on
    // one line must not suppress expansion on the next, while suppression
    // WITHIN a line keeps working.
    #[test]
    fn substitute_macros_per_line_resets_quote_state_at_newline() {
        let mut macros = HashMap::new();
        macros.insert("X".to_string(), "VAL".to_string());
        // Whole-file expansion leaks the apostrophe's quote state forward…
        assert_eq!(
            substitute_macros("don't\n$(X)", &macros),
            "don't\n$(X)",
            "precondition: the whole-file engine suppresses across the newline"
        );
        // …the per-line form does not.
        assert_eq!(
            substitute_macros_per_line("don't\n$(X)", &macros),
            "don't\nVAL"
        );
        // Same-line suppression is unchanged.
        assert_eq!(
            substitute_macros_per_line("'$(X)' $(X)\n$(X)", &macros),
            "'$(X)' VAL\nVAL"
        );
    }

    // The formerly-bypassing path end to end: an apostrophe in a `#` comment
    // must not stop `$(P)` expanding on the record line below it.
    #[test]
    fn parse_db_expands_macros_after_a_comment_apostrophe() {
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "T:".to_string());
        let db = "# this comment's apostrophe must stay harmless\n\
                  record(ai, \"$(P)X\") {\n}\n";
        let records = parse_db(db, &macros).expect("the comment must not break expansion");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "T:X");
    }

    // Macros are NOT expanded inside single quotes.
    #[test]
    fn substitute_macros_suppressed_in_single_quotes() {
        let mut macros = HashMap::new();
        macros.insert("X".to_string(), "VAL".to_string());
        // Single quotes suppress.
        assert_eq!(substitute_macros("'$(X)'", &macros), "'$(X)'");
        // Double quotes do NOT suppress.
        assert_eq!(substitute_macros("\"$(X)\"", &macros), "\"VAL\"");
    }

    // The reference name is macro-expanded before lookup.
    #[test]
    fn substitute_macros_indirect_name() {
        // $($(WHICH)) — WHICH selects which macro to read.
        let mut macros = HashMap::new();
        macros.insert("WHICH".to_string(), "SEL".to_string());
        macros.insert("SEL".to_string(), "chosen".to_string());
        assert_eq!(substitute_macros("$($(WHICH))", &macros), "chosen");
    }

    // L-4 — undefined macro with no default emits `$(name,undefined)`.
    #[test]
    fn substitute_macros_undefined_placeholder() {
        let macros = HashMap::new();
        assert_eq!(
            substitute_macros("$(MISSING)", &macros),
            "$(MISSING,undefined)"
        );
    }

    #[test]
    fn substitute_macros_default_with_comma_is_c_parity() {
        // C parity: `$(LIST=a,b,c)` — the name is LIST, the
        // default is `a` (terminates at the first top-level comma),
        // and `b`/`c` are bare scoped names that define nothing.
        let macros = HashMap::new();
        assert_eq!(substitute_macros("$(LIST=a,b,c)", &macros), "a");
    }

    #[test]
    fn substitute_macros_self_reference_terminates() {
        // Re-expansion must not recurse forever on `A=$(A)`.
        // The recursion guard emits the value once without re-scan.
        let mut macros = HashMap::new();
        macros.insert("A".to_string(), "$(A)".to_string());
        // Must terminate; the exact text is the cycle-broken value.
        let out = substitute_macros("$(A)", &macros);
        assert!(out.contains("A"), "self-ref expansion produced: {out}");
    }

    #[test]
    fn substitute_macros_mutual_reference_terminates() {
        // A=$(B), B=$(A) — mutual cycle must also terminate.
        let mut macros = HashMap::new();
        macros.insert("A".to_string(), "$(B)".to_string());
        macros.insert("B".to_string(), "$(A)".to_string());
        let _ = substitute_macros("$(A)", &macros); // must not hang
    }

    /// The real `menuScan` body at `R7.0.10`
    /// (`modules/database/src/ioc/db/menuScan.dbd.pod`, the text after
    /// `=cut`). The choice ORDER is the whole point: a menu field's wire
    /// value is the index into this list, so a loader that keeps the
    /// choices unordered or deduplicated would serve a different `.SCAN`
    /// than a C IOC for the same `.dbd`.
    #[test]
    fn a_menu_declaration_keeps_its_choices_in_declaration_order() {
        let src = r#"
menu(menuScan) {
	choice(menuScanPassive,"Passive")
	choice(menuScanEvent,"Event")
	choice(menuScanI_O_Intr,"I/O Intr")
	# Periodic scans follow, ordered from slowest to fastest
	choice(menuScan10_second,"10 second")
	choice(menuScan_1_second,".1 second")
}
"#;
        let parsed = parse_db_with_breaktables(src, &HashMap::new()).expect("menu must parse");
        assert!(parsed.records.is_empty());
        let menu = parsed.dbd.menu("menuScan").expect("menuScan declared");
        assert_eq!(
            menu.choices,
            vec![
                ("menuScanPassive".to_string(), "Passive".to_string()),
                ("menuScanEvent".to_string(), "Event".to_string()),
                ("menuScanI_O_Intr".to_string(), "I/O Intr".to_string()),
                ("menuScan10_second".to_string(), "10 second".to_string()),
                ("menuScan_1_second".to_string(), ".1 second".to_string()),
            ],
            "the `#` line inside the body is a comment, not a choice"
        );
    }

    /// Boundary: `recordtype` with a field body, a `menu(...)` item, and a
    /// `%` C definition. C stores the menu item under the literal key
    /// `"menu"` (`dbYacc.y:144-152`) and strips only the leading `%` from
    /// a cdef (`dbLex.l:94-97`).
    #[test]
    fn a_recordtype_keeps_its_field_items_its_menu_item_and_its_cdefs() {
        let src = r#"
recordtype(ai) {
    %#include "callback.h"
    field(SCAN,DBF_MENU) {
        prompt("Scan Mechanism")
        promptgroup("10 - Common")
        special(SPC_SCAN)
        menu(menuScan)
    }
    field(PREC,DBF_SHORT) {
        prompt("Display Precision")
    }
}
"#;
        let parsed =
            parse_db_with_breaktables(src, &HashMap::new()).expect("recordtype must parse");
        let rt = &parsed.dbd.record_types[0];
        assert_eq!(rt.name, "ai");
        assert_eq!(rt.cdefs, vec!["#include \"callback.h\"".to_string()]);
        assert_eq!(rt.fields.len(), 2);
        assert_eq!(rt.fields[0].name, "SCAN");
        assert_eq!(rt.fields[0].dbf_type, "DBF_MENU");
        assert_eq!(
            rt.fields[0].items,
            vec![
                ("prompt".to_string(), "Scan Mechanism".to_string()),
                ("promptgroup".to_string(), "10 - Common".to_string()),
                ("special".to_string(), "SPC_SCAN".to_string()),
                ("menu".to_string(), "menuScan".to_string()),
            ]
        );
        assert_eq!(rt.fields[1].name, "PREC");
    }

    /// Boundary: `recordtype_body: '{' '}'` is its OWN C production
    /// (`dbRecordtypeEmpty`, `dbYacc.y:110-113`), not a degenerate field
    /// list — a `.dbd` that declares a type with no fields must load.
    #[test]
    fn an_empty_recordtype_body_is_not_a_syntax_error() {
        let parsed = parse_db_with_breaktables("recordtype(stub) {}", &HashMap::new())
            .expect("the empty body is a production of its own");
        assert_eq!(parsed.dbd.record_types[0].name, "stub");
        assert!(parsed.dbd.record_types[0].fields.is_empty());
    }

    /// Boundary: the six single-line declarations, including `variable`'s
    /// two arities — C defaults the one-argument form to `int`
    /// (`dbYacc.y:192-202`).
    #[test]
    fn the_single_line_dbd_declarations_all_parse() {
        let src = r#"
device(ai,CONSTANT,devAiSoft,"Soft Channel")
driver(drvVxi)
link(const,lsetConst)
registrar(asSub)
function(myCalcFunc)
variable(dbRecordsOnceOnly)
variable(CASDEBUG,int)
"#;
        let d = parse_db_with_breaktables(src, &HashMap::new())
            .expect("every dbd declaration must parse")
            .dbd;
        assert_eq!(
            d.devices,
            vec![DbdDevice {
                record_type: "ai".into(),
                link_type: "CONSTANT".into(),
                dset: "devAiSoft".into(),
                choice: "Soft Channel".into(),
            }]
        );
        assert_eq!(d.drivers, vec!["drvVxi".to_string()]);
        assert_eq!(
            d.link_types,
            vec![DbdLinkType {
                key: "const".into(),
                lset: "lsetConst".into()
            }]
        );
        assert_eq!(d.registrars, vec!["asSub".to_string()]);
        assert_eq!(d.functions, vec!["myCalcFunc".to_string()]);
        assert_eq!(
            d.variables,
            vec![
                DbdVariable {
                    name: "dbRecordsOnceOnly".into(),
                    dtype: "int".into()
                },
                DbdVariable {
                    name: "CASDEBUG".into(),
                    dtype: "int".into()
                },
            ],
            "the one-argument form defaults to int, as C's action does"
        );
    }

    /// Boundary: C has one production per arity, so a wrong count is a
    /// parse error rather than a silently-dropped argument.
    #[test]
    fn a_declaration_with_the_wrong_argument_count_is_refused() {
        for src in [
            "driver(a,b)",
            "link(only)",
            "device(ai,CONSTANT,devAiSoft)",
            "variable(a,b,c)",
        ] {
            assert!(
                parse_db_with_breaktables(src, &HashMap::new()).is_err(),
                "{src} must not parse"
            );
        }
    }

    /// Boundary: the mixed file. `database_item` is ONE list in C, so
    /// records and declarations interleave freely and the records must
    /// still come out.
    #[test]
    fn records_and_dbd_declarations_interleave_in_one_file() {
        let src = r#"
menu(menuYesNo) { choice(menuYesNoNO,"NO") choice(menuYesNoYES,"YES") }
record(ai, "A") { field(VAL, "1") }
driver(drvSoft)
record(bo, "B") {}
"#;
        let parsed = parse_db_with_breaktables(src, &HashMap::new()).expect("mixed file");
        let names: Vec<&str> = parsed.records.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"]);
        assert_eq!(parsed.dbd.menus.len(), 1);
        assert_eq!(parsed.dbd.drivers, vec!["drvSoft".to_string()]);
    }

    /// Boundary: an ordinary `.db` declares nothing, so the new field must
    /// stay empty rather than pick anything up.
    #[test]
    fn an_ordinary_db_carries_no_dbd_declarations() {
        let parsed =
            parse_db_with_breaktables(r#"record(ai, "A") { field(VAL, "1") }"#, &HashMap::new())
                .unwrap();
        assert!(parsed.dbd.is_empty());
    }

    /// Boundary: `record_field: ... | include` (`dbYacc.y:271`) — an
    /// `include` inside a record body is grammar, not an error.
    #[test]
    fn an_include_inside_a_record_body_is_accepted() {
        let src = r#"record(ai, "A") { field(VAL, "1") include "more.db" field(PREC, "3") }"#;
        let recs = parse_db(src, &HashMap::new()).expect("include is a record_field alternative");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].fields.len(), 2);
    }
}

#[cfg(test)]
mod suggestion_tests {
    use super::*;

    /// The whole diagnostic the loader would print for a refused field, or
    /// `None` where C prints only the ERROR line.
    fn line(record_type: &str, field: &str, value: &str) -> Option<String> {
        assert_eq!(
            record_type_field_exists(record_type, field),
            Some(false),
            "{record_type}.{field} must be a REFUSED field for this to mean anything"
        );
        suggest_field(record_type, field, value).map(suggestion_line)
    }

    /// C's own `epicsStrSimilarity` expectations, ported from
    /// `epicsStringTest.c:118-141` @`R7.0.10` — the table that pins the
    /// case-insensitive half-weight, which a plain edit distance does not have.
    #[test]
    fn similarity_matches_the_c_test_table() {
        for (expect, a, b) in [
            (1.00, "", ""),
            (1.00, "A", "A"),
            (0.00, "A", "B"),
            (1.00, "exact", "exact"),
            (0.90, "10 second", "10 seconds"),
            (0.71, "Passive", "Pensive"),
            (0.11, "10 second", "Pensive"),
            (0.97, "Set output to IVOV", "Set output To IVOV"),
            // Unrelated except for `i` ~= `I`, which a case-blind metric would
            // score 0 and a case-strict one would score the same as a
            // substitution.
            (0.06, "Passive", "I/O Intr"),
            (0.06, "I/O Intr", "Pensive"),
            (0.50, "YES", "yes"),
            (0.00, "YES", "NO"),
            (0.67, "YES", "Yes"),
            (0.67, "Tes", "yes"),
        ] {
            let got = str_similarity(a, b);
            assert!(
                (got - expect).abs() < 0.01,
                "similarity({a:?}, {b:?}) = {got}, C says {expect}"
            );
        }
    }

    /// Measured against the reference `softIoc` @`R7.0.10`, one
    /// `record(<type>,"C") { field(<name>, "<value>") }` per row, stderr with
    /// the ANSI colouring stripped. Byte-exact, including the four-space
    /// indent, the two spaces before the prompt and the parentheses.
    #[test]
    fn the_suggestion_is_byte_exact_against_the_reference_ioc() {
        for (rt, field, value, expect) in [
            // Confusion map, simple form — it wins outright over anything
            // lexical, which is the point of having it.
            (
                "bo",
                "INP",
                "x",
                Some("    Did you mean \"OUT\"?  (Output Specification)"),
            ),
            (
                "ao",
                "INP",
                "1",
                Some("    Did you mean \"OUT\"?  (Output Specification)"),
            ),
            (
                "ai",
                "OUT",
                "1",
                Some("    Did you mean \"INP\"?  (Input Specification)"),
            ),
            (
                "ai",
                "DOL",
                "1",
                Some("    Did you mean \"INP\"?  (Input Specification)"),
            ),
            (
                "bi",
                "ZRST",
                "off",
                Some("    Did you mean \"ZNAM\"?  (Zero Name)"),
            ),
            (
                "mbbi",
                "ZNAM",
                "off",
                Some("    Did you mean \"ZRST\"?  (Zero String)"),
            ),
            (
                "mbbo",
                "ONAM",
                "on",
                Some("    Did you mean \"ONST\"?  (One String)"),
            ),
            // Confusion map, range form — the trailing character carries its
            // offset across to the partner range.
            (
                "calc",
                "DOLA",
                "1",
                Some("    Did you mean \"INPK\"?  (Input K)"),
            ),
            (
                "calc",
                "DOLF",
                "1",
                Some("    Did you mean \"INPP\"?  (Input P)"),
            ),
            (
                "calc",
                "INP0",
                "1",
                Some("    Did you mean \"INPA\"?  (Input A)"),
            ),
            (
                "calc",
                "INP9",
                "1",
                Some("    Did you mean \"INPJ\"?  (Input J)"),
            ),
            (
                "ao",
                "DOL0",
                "1",
                Some("    Did you mean \"DOL\"?  (Desired Output Link)"),
            ),
            (
                "seq",
                "INPA",
                "1",
                Some("    Did you mean \"DOL0\"?  (Input link 0)"),
            ),
            // Lexical similarity.
            (
                "ai",
                "ASL",
                "1",
                Some("    Did you mean \"ASLO\"?  (Adjustment Slope)"),
            ),
            (
                "ai",
                "PRE",
                "3",
                Some("    Did you mean \"PREC\"?  (Display Precision)"),
            ),
            (
                "ai",
                "DES",
                "hello",
                Some("    Did you mean \"DESC\"?  (Descriptor)"),
            ),
            (
                "ai",
                "FLNKK",
                "X",
                Some("    Did you mean \"FLNK\"?  (Forward Process Link)"),
            ),
            (
                "calc",
                "CALCC",
                "A+B",
                Some("    Did you mean \"CALC\"?  (Calculation)"),
            ),
            (
                "permissive",
                "LABE",
                "hi",
                Some("    Did you mean \"LABL\"?  (Button Label)"),
            ),
            (
                "stringin",
                "VALL",
                "hello",
                Some("    Did you mean \"VAL\"?  (Current Value)"),
            ),
            // The VALUE moves the answer: the same misspelling picks a
            // different field once the value stops parsing as the near-miss's
            // type.
            (
                "ai",
                "ASL",
                "abc",
                Some("    Did you mean \"ASG\"?  (Access Security Group)"),
            ),
            (
                "ai",
                "HIH",
                "1",
                Some("    Did you mean \"HIHI\"?  (Hihi Alarm Limit)"),
            ),
            (
                "ai",
                "HIH",
                "abc",
                Some("    Did you mean \"SDIS\"?  (Scanning Disable)"),
            ),
            // A device menu: the numeric index halves the score and loses,
            // the spelled-out choice multiplies it by 1.5 and wins.
            (
                "ai",
                "DTY",
                "1",
                Some("    Did you mean \"SDLY\"?  (Sim. Mode Async Delay)"),
            ),
            (
                "ai",
                "DTY",
                "Soft Channel",
                Some("    Did you mean \"DTYP\"?  (Device Type)"),
            ),
            (
                "ai",
                "DTYX",
                "1",
                Some("    Did you mean \"DTYP\"?  (Device Type)"),
            ),
            // `menu(menuScan)` resolves through the loaded table, not the
            // generated one, which is empty for it by construction.
            (
                "ai",
                "SCA",
                "Passive",
                Some("    Did you mean \"SCAN\"?  (Scan Mechanism)"),
            ),
            (
                "ai",
                "SCANN",
                "Passive",
                Some("    Did you mean \"SCAN\"?  (Scan Mechanism)"),
            ),
            // `lsi.VAL` is declared `special(SPC_DBADDR)` and so is never
            // proposed, even though its `cvt_dbaddr` resolves the address to
            // `SPC_MOD`.
            (
                "lsi",
                "oval",
                "2 second",
                Some("    Did you mean \"SCAN\"?  (Scan Mechanism)"),
            ),
            // No candidate shares a character with the name, so every score is
            // zero and C prints nothing at all.
            ("ai", "ZZZZ", "1", None),
            ("ai", "Q", "1", None),
        ] {
            assert_eq!(
                line(rt, field, value).as_deref(),
                expect,
                "{rt}.{field} = {value:?}"
            );
        }
    }

    /// A record type with no device support at all.
    ///
    /// C @`R7.0.10` does not answer this case: `dbRecordField` reaches
    /// `DTYP`'s `DBF_DEVICE` arm, dereferences a NULL `dbDeviceMenu` and
    /// SEGFAULTS — measured on the reference `softIoc` for all nine base
    /// record types that declare no `device()` line (`aSub`, `calc`,
    /// `compress`, `dfanout`, `fanout`, `permissive`, `sel`, `seq`, `sub`),
    /// for any bad field name with a non-empty value. The port reads the
    /// absent menu as an empty one — the branch C's own code takes for
    /// `nChoice == 0` — so `DTYP` simply takes the mismatch penalty and the
    /// answer is the one C gives when the value is empty and the crash is
    /// skipped.
    #[test]
    fn a_record_type_with_no_device_support_still_answers() {
        let expect = Some("    Did you mean \"LOLO\"?  (Lolo Alarm Limit)");
        // C's own answer, measured: `field(DOL, "")` takes no type test at all.
        assert_eq!(line("calc", "DOL", "").as_deref(), expect);
        // The value that kills C.
        assert_eq!(line("calc", "DOL", "1").as_deref(), expect);
    }

    /// The map proposes a name; the record type decides whether it exists.
    #[test]
    fn a_confusion_map_proposal_the_type_does_not_declare_is_dropped() {
        // `DOL` -> `INP`, which `calc` does not declare (it has `INPA`..`INPL`),
        // so the map yields nothing and the lexical pass answers instead.
        assert_eq!(confusion_guess("DOL", 2).as_deref(), Some("INP"));
        assert!(crate::server::record::declared_field("calc", "INP").is_none());
        assert_eq!(
            line("calc", "DOL", "").as_deref(),
            Some("    Did you mean \"LOLO\"?  (Lolo Alarm Limit)")
        );
    }

    /// C builds the range proposal from the first `l-3` characters and the one
    /// at `l-3`, and never looks at the tail — so a longer name still proposes,
    /// and is rejected only by not existing.
    #[test]
    fn the_range_map_ignores_the_tail_of_the_name() {
        assert_eq!(confusion_guess("INPAX", 8).as_deref(), Some("DOL0"));
        assert_eq!(confusion_guess("INPA", 8).as_deref(), Some("DOL0"));
        // The prefix must match and the character must be inside the range.
        assert_eq!(confusion_guess("INP", 8), None);
        assert_eq!(confusion_guess("INPZ", 8), None);
        assert_eq!(confusion_guess("XNPA", 8), None);
    }

    /// A field with no `prompt` in its `.dbd` gets the bare line — C only
    /// prints the parenthesised half `if (bestFld->prompt)`.
    #[test]
    fn a_field_without_a_prompt_gets_the_bare_line() {
        let desc = FieldDesc::new("ZZ", crate::types::DbFieldType::Long, false);
        assert_eq!(suggestion_line(&desc), "    Did you mean \"ZZ\"?");
    }
}

/// C's two `.db` menu-value refusals, measured on the reference `softIoc`
/// @`R7.0.10`.
#[cfg(test)]
mod menu_refusal_tests {
    use super::{ERL_ERROR, menu_value_refusal};

    fn line(rt: &str, rec: &str, field: &str, value: &str) -> Option<String> {
        menu_value_refusal(rt, rec, field, value).map(|r| r.line)
    }

    fn hint(rt: &str, field: &str, value: &str) -> Option<String> {
        menu_value_refusal(rt, "R", field, value).and_then(|r| r.suggestion)
    }

    /// C `dbPutString` refuses ANY value still carrying a macro before it
    /// reaches the field-type switch (`dbStaticLib.c:2579-2585`), and
    /// `dbIsMacroOk` is `return(FALSE)` in the only definition base has
    /// (`dbStaticRun.c:228`), so the exemption never fires in an IOC.
    ///
    /// Every row was read off the reference `softIoc` loading
    /// `modules/database/test/std/rec/compressTest.db` with no macros
    /// defined, which is the one file in B's 317-case corpus where C's guard
    /// fires at all.
    #[test]
    fn an_unexpanded_macro_is_refused_before_the_field_type_is_read() {
        for (field, value, suggestion) in [
            // A link field: no other arm of this funnel refuses one, which is
            // why the port used to install it and leave a live CA search for a
            // PV literally named `$(INP,undefined)`.
            ("INP", "$(INP,undefined) NPP", None),
            // Menu fields: `dbRecordField` runs `dbPutStringSuggest` after
            // this refusal exactly as it does after the menu arm's own
            // (`dbLexRoutines.c:1414`).
            (
                "ALG",
                "$(ALG,undefined)",
                Some("    Did you mean \"Average\"?"),
            ),
            (
                "BALG",
                "$(BALG,undefined)",
                Some("    Did you mean \"FIFO Buffer\"?"),
            ),
            // A numeric field, whose own arm answered "No digits to convert"
            // before the guard was put ahead of it.
            ("NSAM", "$(NSAM,undefined)", None),
        ] {
            let refusal = menu_value_refusal("compress", "comp", field, value)
                .unwrap_or_else(|| panic!("compress.{field} = {value:?} must be refused"));
            assert_eq!(
                refusal.notice,
                Some(format!("comp.{field} Has unexpanded macro"))
            );
            assert_eq!(
                refusal.line,
                format!("{ERL_ERROR}: Can't set 'comp.{field}' to '{value}'  : Bad Field value")
            );
            assert_eq!(refusal.suggestion.as_deref(), suggestion);
        }
    }

    /// The test C makes is `strstr(pstring,"$(") || strstr(pstring,"${")`, so
    /// both spellings refuse and a value carrying neither is left to the arms
    /// that were already deciding it.
    #[test]
    fn both_macro_spellings_refuse_and_a_plain_value_still_does_not() {
        assert!(menu_value_refusal("compress", "comp", "INP", "${INP} NPP").is_some());
        assert!(menu_value_refusal("compress", "comp", "INP", "some:pv NPP").is_none());
        assert_eq!(
            hint("ai", "SCAN", "Passiv"),
            Some(String::from("    Did you mean \"Passive\"?"))
        );
    }

    /// Every row was read off `softIoc -d <file>` and copied verbatim,
    /// including the two spaces the empty `pdbentry->message` leaves in the
    /// `Bad Field value` form.
    #[test]
    fn the_refusal_is_byte_exact_against_the_reference_ioc() {
        for (rt, rec, field, value, expect) in [
            (
                "ai",
                "M1",
                "SCAN",
                "Passiv",
                Some(format!(
                    "{ERL_ERROR}: Can't set 'M1.SCAN' to 'Passiv' using menu menuScan : Illegal choice"
                )),
            ),
            (
                "ai",
                "M3",
                "PINI",
                "YESS",
                Some(format!(
                    "{ERL_ERROR}: Can't set 'M3.PINI' to 'YESS' using menu menuPini : Illegal choice"
                )),
            ),
            (
                "ai",
                "N1",
                "PRIO",
                "HIGHH",
                Some(format!(
                    "{ERL_ERROR}: Can't set 'N1.PRIO' to 'HIGHH' using menu menuPriority : Illegal choice",
                )),
            ),
            (
                "ai",
                "L3",
                "LINR",
                "NoSuchTable",
                Some(format!(
                    "{ERL_ERROR}: Can't set 'L3.LINR' to 'NoSuchTable' using menu menuConvert : Illegal choice",
                )),
            ),
            (
                "sel",
                "Y1",
                "SELM",
                "Bogus",
                Some(format!(
                    "{ERL_ERROR}: Can't set 'Y1.SELM' to 'Bogus' using menu selSELM : Illegal choice"
                )),
            ),
            // A number that clears `epicsParseUInt16` but not C's bound takes
            // the OTHER arm: no `dbMsgPrint`, so no menu name and two spaces,
            // and `S_dbLib_badField` rather than `S_db_badChoice`.
            (
                "ai",
                "P2",
                "PINI",
                "7",
                Some(format!(
                    "{ERL_ERROR}: Can't set 'P2.PINI' to '7'  : Bad Field value"
                )),
            ),
            // C's bound is `value > nChoice`, so one past the last choice
            // loads — `menuPini` has six.
            ("ai", "P1", "PINI", "6", None),
            // ... and so does `USHRT_MAX`, which is what lets the `menuScan`
            // "use SCAN" sentinel into `SSCN`.
            ("ai", "P3", "PINI", "65535", None),
            // "empty string is the same as writing numeric zero".
            ("ai", "P4", "PINI", "", None),
            // `epicsParseUInt16(pstring, &value, 0, NULL)` — base 0, so `0x`
            // and octal are numbers.
            ("ai", "P5", "PINI", "0x2", None),
            // `strcmp`, so the choice test is case-SENSITIVE and this falls
            // through to the numeric one.
            (
                "ai",
                "P6",
                "PINI",
                "yes",
                Some(format!(
                    "{ERL_ERROR}: Can't set 'P6.PINI' to 'yes' using menu menuPini : Illegal choice"
                )),
            ),
            // An exact choice, and an in-menu index, both load.
            ("ai", "OK", "PINI", "RUNNING", None),
            ("ai", "OK", "PINI", "3", None),
            ("ai", "OK", "SCAN", "1 second", None),
        ] {
            assert_eq!(
                line(rt, rec, field, value).as_deref(),
                expect.as_deref(),
                "{rt} {rec}.{field} = {value:?}"
            );
        }
    }

    /// Not a menu field, so the menu wording is not what judges it — C reaches
    /// a different arm of `dbPutStringNum` for those.
    #[test]
    fn a_non_menu_field_is_not_judged_by_the_menu_arm() {
        // `DBF_SHORT` still fails, through the numeric arm and with the
        // `S_stdlib_*` wording rather than a menu name.
        assert_eq!(
            line("ai", "R", "PREC", "not a number"),
            Some(format!(
                "{ERL_ERROR}: Can't set 'R.PREC' to 'not a number'  : No digits to convert"
            ))
        );
        assert_eq!(hint("ai", "PREC", "not a number"), None);
        // `DBF_STRING` has a length check, not a parse.
        assert_eq!(line("ai", "R", "DESC", "anything at all"), None);
        // `DBF_DEVICE`: the port's builder-registered device support never
        // reaches the device menu, so the menu cannot judge a DTYP.
        assert_eq!(line("ai", "R", "DTYP", "Soft Chanel"), None);
    }

    /// C `dbPutStringSuggest` (`dbStaticLib.c:2665-2708`), measured on the
    /// reference `softIoc` @`R7.0.10` alongside each refusal above.
    #[test]
    fn the_choice_suggestion_is_byte_exact_against_the_reference_ioc() {
        for (rt, field, value, expect) in [
            (
                "ai",
                "SCAN",
                "Passiv",
                Some("    Did you mean \"Passive\"?"),
            ),
            ("ai", "PINI", "YESS", Some("    Did you mean \"YES\"?")),
            ("ai", "PRIO", "HIGHH", Some("    Did you mean \"HIGH\"?")),
            (
                "ai",
                "LINR",
                "NoSuchTable",
                Some("    Did you mean \"SLOPE\"?"),
            ),
            (
                "sel",
                "SELM",
                "Bogus",
                Some("    Did you mean \"Low Signal\"?"),
            ),
            ("ai", "PINI", "yes", Some("    Did you mean \"YES\"?")),
            // `maxdist` starts at 0.0 and the test is strict `>`: a value that
            // shares nothing with any choice gets the bare refusal, which is
            // what C prints for an out-of-menu index.
            ("ai", "PINI", "7", None),
        ] {
            assert_eq!(
                hint(rt, field, value).as_deref(),
                expect,
                "{rt}.{field} = {value:?}"
            );
        }
    }

    /// Every `dbCommon` menu field must resolve its menu's name, or the
    /// refusal would be printed without the half C words it with.
    #[test]
    fn every_common_menu_field_names_its_menu() {
        use crate::server::record::{dbd_generated::DB_COMMON_FIELDS, declared_field};
        use crate::types::DbfCode;

        for desc in DB_COMMON_FIELDS {
            if desc.declared_dbf != DbfCode::Menu {
                continue;
            }
            assert!(
                super::load_menu_of(desc).is_some_and(|(name, _)| name.is_some()),
                "{}: menu name did not resolve",
                desc.name
            );
        }
        // And a record-own menu field, resolved through the same table.
        let linr = declared_field("ai", "LINR").expect("ai declares LINR");
        assert_eq!(
            super::load_menu_of(linr).and_then(|(n, _)| n),
            Some("menuConvert")
        );
    }

    /// An externally-registered record type's menu field carries its own
    /// choice table — a static from its OWN crate, absent from the generated
    /// `MENUS` — and `FieldDesc::new` derives `declared_dbf` from the served
    /// type, not from the table. Both halves used to drop the field out of
    /// the menu path entirely, and its labels then refused through the
    /// `DBF_ENUM`/`DBF_USHORT` numeric arm as "No digits to convert".
    #[test]
    fn an_external_choice_table_is_a_menu_without_a_name() {
        use crate::server::record::FieldDesc;
        use crate::types::{DbFieldType, DbfCode};

        static EXT_CHOICES: &[&str] = &["Local", "Remote"];
        let mut desc = FieldDesc::new("MODE", DbFieldType::Enum, false);
        desc.menu = Some(EXT_CHOICES);
        assert_eq!(desc.declared_dbf, DbfCode::Enum);
        assert_eq!(super::load_menu_of(&desc), Some((None, EXT_CHOICES)));

        // A valid label is not refused; an invalid one gets C's menu
        // wording, with the field's name standing in for the `.dbd` menu
        // name the external table does not have.
        assert_eq!(
            super::menu_arm(&desc, "MODE", None, EXT_CHOICES, "Remote", "Remote"),
            None
        );
        let (detail, status, _) =
            super::menu_arm(&desc, "MODE", None, EXT_CHOICES, "Bogus", "Bogus").unwrap();
        assert_eq!(detail, "using menu MODE");
        assert_eq!(status, "Illegal choice");

        // `DBF_DEVICE` keeps its own channel even when it carries a table.
        let mut dtyp = FieldDesc::new("DTYP", DbFieldType::Enum, false);
        dtyp.declared_dbf = DbfCode::Device;
        dtyp.menu = Some(EXT_CHOICES);
        assert_eq!(super::load_menu_of(&dtyp), None);
    }
}

/// C's `.db` NUMERIC refusals, measured on the reference `softIoc` @`R7.0.10`.
///
/// The whole point of the round is the pair `1e-310` / `70000`: glibc raises
/// `ERANGE` for a subnormal, so C refuses a value the port stored, while
/// `dbConvertStrict` being zero makes C STORE a truncated `70000` the port
/// would refuse. One C function decides both, so one screen has to.
#[cfg(test)]
mod numeric_refusal_tests {
    use super::{
        DbfCode, ERL_ERROR, db_load_numeric_value, menu_value_refusal, numeric_value_refusal,
    };
    use crate::types::EpicsValue;

    fn status(rt: &str, field: &str, value: &str) -> Option<String> {
        let r = menu_value_refusal(rt, "R", field, value)?;
        // Every numeric refusal is worded the same way: no `dbMsgPrint`, so
        // two spaces, and no suggestion.
        assert_eq!(r.suggestion, None, "{rt}.{field} = {value:?}");
        let head = format!("{ERL_ERROR}: Can't set 'R.{field}' to '{value}'  : ");
        Some(
            r.line
                .strip_prefix(&head)
                .unwrap_or_else(|| panic!("{:?} does not open with {head:?}", r.line))
                .to_string(),
        )
    }

    #[test]
    fn the_refusals_are_byte_exact_against_the_reference_ioc() {
        for (rt, field, value, expect) in [
            // `DBF_DOUBLE`, `epicsParseFloat64` = `epicsParseDouble`. All three
            // ERANGE shapes report OVERFLOW unless the result reached exactly
            // zero, because `epicsStdlib.c:165` reads a non-zero result as
            // "too large" whichever end it came off.
            ("ao", "OROC", "5e-324", "Too large to represent"),
            ("ao", "HOPR", "1e-310", "Too large to represent"),
            ("ao", "HOPR", "-1e-310", "Too large to represent"),
            ("ao", "LOPR", "1e400", "Too large to represent"),
            ("ao", "EGUF", "1e-400", "Too small to represent"),
            ("ao", "EGUL", "abc", "No digits to convert"),
            ("ao", "AOFF", "1.5x", "Extraneous characters"),
            // `DBF_SHORT`/`DBF_UINT64`/`DBF_INT64`, the lax arm: only what
            // `strtol`/`strtoul` itself refuses.
            (
                "ai",
                "PHAS",
                "99999999999999999999",
                "Too large to represent",
            ),
            ("ai", "PHAS", "zz", "No digits to convert"),
            ("ai", "PHAS", "3 x", "Extraneous characters"),
            (
                "ai",
                "UTAG",
                "99999999999999999999999",
                "Too large to represent",
            ),
            (
                "int64out",
                "VAL",
                "9223372036854775808",
                "Too large to represent",
            ),
            // An integer field given a decimal: `strtol` stops at the `.` and
            // the tail is extraneous, so this common `.db` idiom FAILS the load.
            ("longout", "VAL", "1.0", "Extraneous characters"),
            ("ai", "PREC", "3.0", "Extraneous characters"),
            // `DBF_ENUM` takes the unsigned arm, not the menu one, so a label
            // never resolves on an `mbbo` VAL.
            ("mbbo", "VAL", "ONE", "No digits to convert"),
        ] {
            assert_eq!(
                status(rt, field, value).as_deref(),
                Some(expect),
                "{rt}.{field} = {value:?}"
            );
        }
    }

    #[test]
    fn the_values_c_stores_are_not_refused() {
        for (rt, field, value) in [
            // Whitespace either side is skipped at both ends of `epicsParse*`.
            ("ao", "ASLO", "  2.5  "),
            ("ai", "PREC", " 4"),
            ("ai", "PREC", "4 "),
            // Base 0: hex, binary and octal prefixes, on the integer arms and
            // through `strtod` on the double one.
            ("ao", "ROFF", "0x10"),
            ("ao", "HOPR", "0x10"),
            ("longout", "VAL", "0x10"),
            ("waveform", "NELM", "0b11"),
            ("ai", "PREC", "010"),
            // The lax arm casts instead of range-checking: 4464, 25536, 255,
            // 44, `ULLONG_MAX` and 705032704 respectively.
            ("ai", "PHAS", "70000"),
            ("ai", "PHAS", "-40000"),
            ("ai", "TPRO", "-1"),
            ("ai", "TPRO", "300"),
            ("ai", "UTAG", "-1"),
            ("longout", "VAL", "5000000000"),
            ("int64out", "VAL", "-9223372036854775808"),
            ("mbbo", "VAL", "5"),
            ("ai", "TSE", "-2"),
            // `epicsParseDouble` sets no ERANGE for the `inf`/`nan` WORDS.
            ("ao", "HOPR", "1e3"),
            ("ao", "HOPR", "nan"),
            ("ao", "HOPR", "inf"),
            ("ao", "HOPR", ".5"),
            // "empty string is the same as writing numeric zero".
            ("ai", "PREC", ""),
            ("ao", "HOPR", ""),
        ] {
            assert_eq!(
                menu_value_refusal(rt, "R", field, value).map(|r| r.line),
                None,
                "{rt}.{field} = {value:?}"
            );
        }
    }

    /// `DBF_FLOAT` and `DBF_CHAR` have no field in any `epics-base` record, so
    /// their arms are exercised at the switch instead of through a record.
    ///
    /// `epicsParseFloat`'s two extra gates (`epicsStdlib.c:325-329`) are
    /// deliberately asymmetric: the underflow test reads `value > 0`, so a
    /// negative subnormal-for-`float` is STORED, and the overflow test reads
    /// `finite(value)`, so an `inf` literal is stored while `1e39` is not.
    #[test]
    fn the_float_gate_boundaries() {
        let float = |s: &str| numeric_value_refusal(DbfCode::Float, "F", s);
        let flt_min = format!("{:e}", f64::from(f32::MIN_POSITIVE));
        let flt_max = format!("{:e}", f64::from(f32::MAX));

        assert_eq!(float("1e-40"), Some("Too small to represent"));
        assert_eq!(float(&flt_min), Some("Too small to represent"));
        assert_eq!(float("-1e-40"), None);
        assert_eq!(float("1e39"), Some("Too large to represent"));
        assert_eq!(float("-1e39"), Some("Too large to represent"));
        assert_eq!(float(&flt_max), Some("Too large to represent"));
        assert_eq!(float("inf"), None);
        assert_eq!(float("-inf"), None);
        assert_eq!(float("nan"), None);
        assert_eq!(float("1.5"), None);
        // The double parse runs first, so its own ERANGE wins.
        assert_eq!(float("1e-400"), Some("Too small to represent"));
        assert_eq!(float("1e400"), Some("Too large to represent"));
        assert_eq!(float("zz"), Some("No digits to convert"));

        let ch = |s: &str| numeric_value_refusal(DbfCode::Char, "C", s);
        assert_eq!(ch("300"), None); // cast to 44, not refused
        assert_eq!(ch("-1"), None);
        assert_eq!(ch("abc"), Some("No digits to convert"));
        assert_eq!(ch("1.0"), Some("Extraneous characters"));
    }

    /// The lax arms CAST rather than range-check, so the value C stores is not
    /// the value the text names. Every row was read back with `dbgf` on the
    /// reference `softIoc` after the load that produced it.
    #[test]
    fn the_stored_value_is_the_cast_c_makes() {
        for (declared, text, expect) in [
            (DbfCode::Short, "70000", EpicsValue::Short(4464)),
            (DbfCode::Short, "-40000", EpicsValue::Short(25536u16 as i16)),
            (DbfCode::Short, "010", EpicsValue::Short(8)),
            (DbfCode::Short, "-2", EpicsValue::Short(-2)),
            (DbfCode::UChar, "-1", EpicsValue::UChar(255)),
            (DbfCode::UChar, "300", EpicsValue::UChar(44)),
            (DbfCode::Long, "5000000000", EpicsValue::Long(705032704)),
            (DbfCode::Long, "0x10", EpicsValue::Long(16)),
            (DbfCode::ULong, "0b11", EpicsValue::ULong(3)),
            (DbfCode::UInt64, "-1", EpicsValue::UInt64(u64::MAX)),
            (
                DbfCode::Int64,
                "-9223372036854775808",
                EpicsValue::Int64(i64::MIN),
            ),
            (DbfCode::Enum, "5", EpicsValue::UShort(5)),
            (DbfCode::Char, "300", EpicsValue::Char(44)),
            (DbfCode::Char, "-1", EpicsValue::Char(255)),
            (DbfCode::Double, "0x10", EpicsValue::Double(16.0)),
            (DbfCode::Double, "  2.5  ", EpicsValue::Double(2.5)),
            (DbfCode::Float, "1.5", EpicsValue::Float(1.5)),
            // The empty value is the number zero on every arm.
            (DbfCode::Short, "", EpicsValue::Short(0)),
            (DbfCode::Double, "", EpicsValue::Double(0.0)),
        ] {
            assert_eq!(
                db_load_numeric_value(declared, "F", text),
                Ok(Some(expect)),
                "{declared:?} = {text:?}"
            );
        }
    }

    /// The three types whose C arm is not a numeric parse at all.
    #[test]
    fn the_non_numeric_declarations_decline() {
        for code in [
            DbfCode::String,
            DbfCode::Menu,
            DbfCode::Device,
            DbfCode::Inlink,
            DbfCode::Outlink,
            DbfCode::Fwdlink,
            DbfCode::NoAccess,
        ] {
            assert_eq!(numeric_value_refusal(code, "X", "not a number"), None);
        }
    }
}
