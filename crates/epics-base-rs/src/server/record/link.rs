use std::borrow::Cow;

use crate::error::{CaError, CaResult};
use crate::runtime::json_string::{decode_json_string, decode_json_string_token};
use crate::types::{DbfLinkClass, EpicsValue};

/// Link processing policy for input/output links.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkProcessPolicy {
    NoProcess,
    #[default]
    ProcessPassive,
    /// CP (`pvlOptCP`): subscribe to source; when source changes, process
    /// this record unconditionally (`dbCa.c:993` adds `CA_DBPROCESS`
    /// regardless of `precord->scan`).
    ChannelProcess,
    /// CPP (`pvlOptCPP`): like `ChannelProcess`, but on a source change
    /// process this record only when its `SCAN` is `Passive` — C gates the
    /// `CA_DBPROCESS` action on `precord->scan == 0` (`dbCa.c:854,994,1072`).
    ChannelProcessPassive,
}

impl LinkProcessPolicy {
    /// For a CP (`ChannelProcess`) or CPP (`ChannelProcessPassive`) link,
    /// returns `Some(passive_only)`: `false` for CP (always process the
    /// link-holder when the source changes), `true` for CPP (process it
    /// only when it is Passive). Returns `None` for every other policy, so
    /// CP-link registration can filter in one match.
    pub fn cp_passive_only(self) -> Option<bool> {
        match self {
            LinkProcessPolicy::ChannelProcess => Some(false),
            LinkProcessPolicy::ChannelProcessPassive => Some(true),
            _ => None,
        }
    }
}

/// Parsed link address pointing to another record's field.
#[derive(Clone, Debug)]
pub struct LinkAddress {
    pub record: String,
    pub field: String,
    pub policy: LinkProcessPolicy,
}

/// Hardware-link bus kind. Mirrors epics-base `link.h` bus enum.
/// We only carry kinds we can identify from the leading character or
/// a leading `@` token; the actual driver dispatch is by raw arg
/// string so unknown buses still land somewhere useful.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HwLinkKind {
    /// `@dev arg1 arg2 ...` — INST_IO. The most common form, used by
    /// asyn-based device support.
    InstIo,
    /// `#Cn Sn @parm` — VME_IO. C/S = card/signal, parm = optional.
    VmeIo,
    /// Other / unrecognized — payload kept verbatim.
    Other,
}

/// Hardware link as parsed from a record's INP/OUT field. Mirrors
/// epics-base PR #213 — accepts the `@dev arg1 ...` and `#C S` forms
/// directly so device-support adapters get a structured handle
/// instead of having to re-parse the raw string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HwLink {
    pub kind: HwLinkKind,
    /// Whitespace-tokenized argument list (after the leading `@` or
    /// `#…` discriminator). Empty when the link is just `@`.
    pub args: Vec<String>,
    /// Original verbatim payload (for drivers that prefer to do
    /// their own parsing — `dev arg1 0x1A` etc.).
    pub raw: String,
}

/// Parsed link — distinguishes constants, DB links, CA/PVA links, and empty.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedLink {
    None,
    Constant(String),
    Db(DbLink),
    Ca(CaLink),
    /// PVA (`pvalink`) link whose payload is a verbatim channel name —
    /// the string shorthand `{pva:"name"}` or the `pva://name` scheme
    /// form. Per pvxs `pva_parse_string` (pvalink_jlif.cpp:143-149) a
    /// string pvalink IS the channel name: any `?`/`&` in it is link
    /// DATA, not option syntax. The structured JSON longhand with
    /// options is [`ParsedLink::PvaJson`] instead, so this variant's
    /// `String` has exactly one meaning (a channel name) on every path.
    Pva(String),
    /// PVA (`pvalink`) link parsed from the structured JSON longhand
    /// `{pva:{pv:"name", field:"f", proc:"CP", …}}`. Carries the options
    /// as structured JLink members so the pvalink consumer reconstructs
    /// the `PvaLinkConfig` from map keys (pvalink_jlif.cpp:69-196), not
    /// from a `?key=value` URI query that pvxs never parses. See
    /// [`PvaJsonLink`].
    PvaJson(PvaJsonLink),
    /// `@dev arg1 …` or `#Cn Sn` hardware link (epics-base PR #213).
    Hw(HwLink),
    /// epics-base PR `e3c9d590` / `20404003`: a `lnkCalc` JSON link
    /// computes a result from one or more input PV values + a calc
    /// expression, optionally pulling its timestamp from one of the
    /// inputs. JSON form:
    /// `{calc:{expr:"A+B*2", args:["pv1","pv2.VAL"], time:"A"}}`
    /// — `time` is the input letter (A-L) whose timestamp the result
    /// should carry. `time` may be omitted (no timestamp passthrough).
    Calc(CalcLink),
}

/// A single JLink option value, preserving the JSON value KIND parsed
/// from a `{pva:{...}}` longhand option.
///
/// pvxs's pvalink JLink callback table dispatches strictly by JSON type:
/// `pva_parse_null`/`pva_parse_bool`/`pva_parse_integer`/`pva_parse_string`
/// are wired to distinct slots and the real/double slot is `NULL`
/// (pvxs `ioc/pvalink_jlif.cpp:286-300`), so JSON reals are unsupported.
/// Critically, a JSON boolean (`pipeline:true`) and a JSON string
/// (`pipeline:"yes"`) reach DIFFERENT callbacks and are NOT
/// interchangeable: a string value on a boolean-only key falls through
/// `pva_parse_string`'s unknown-key branch and is ignored
/// (`pvalink_jlif.cpp:189-191`), and a string `proc:"true"` is likewise
/// ignored because only `CP`/`CPP`/`PP`/`NPP`/empty are recognized
/// strings (`:156-170`). Collapsing every option to its text form erases
/// that distinction, so this enum carries the kind through to the pvalink
/// consumer, which reproduces pvxs's per-type dispatch exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JlinkValue {
    /// JSON `null` — `pva_parse_null` (`proc`→Default, `sevr`→NMS,
    /// `local`→false; pvalink_jlif.cpp:69-88).
    Null,
    /// JSON boolean — `pva_parse_bool` (pvalink_jlif.cpp:90-122).
    Bool(bool),
    /// JSON integer — `pva_parse_integer` (`Q`, `monorder`;
    /// pvalink_jlif.cpp:124-141).
    Int(i64),
    /// JSON string — `pva_parse_string` (`pv`, `field`, and the `proc`/
    /// `sevr` enum strings; pvalink_jlif.cpp:143-197).
    Str(String),
}

/// A PVA (`pvalink`) external link parsed from the structured JSON
/// longhand `{pva:{pv:"name", field:"f", proc:"CP", …}}`.
///
/// pvxs parses pvalink options only as JLink map keys / typed values
/// (pvalink_jlif.cpp:69-196): booleans (`pipeline`/`time`/`retry`/
/// `local`/`atomic`), integers (`Q`/`monorder`), strings (`field`/
/// `proc`/`sevr`). There is no `?key=value` URI query parser in the
/// JLink callback table (pvalink_jlif.cpp:286-300). Preserving the
/// options as structured pairs here keeps that provenance: the consumer
/// reads JLink members directly instead of re-parsing a synthetic query
/// string (which is exactly the non-pvxs syntax this representation
/// avoids).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PvaJsonLink {
    /// Channel name from the `pv` member.
    pub pv: String,
    /// Non-`pv` JLink options in source order with original key case
    /// (`field`, `proc`, `sevr`, `Q`, `pipeline`, `time`, `retry`,
    /// `local`, `atomic`, `monorder`, …) and original JSON value KIND
    /// ([`JlinkValue`]). Empty when the map carried only `pv` — in that
    /// case [`parse_link_v2`] yields a plain [`ParsedLink::Pva`] rather
    /// than this variant.
    pub options: Vec<(String, JlinkValue)>,
}

impl PvaJsonLink {
    /// Stable per-link identity key for the base→bridge link-set
    /// boundary — `pv` plus a canonical encoding of this link's options.
    ///
    /// Two structured `{pva:{pv:"SRC",…}}` links to the SAME PV that
    /// differ only by options (`field`, `Q`, `pipeline`, …) are distinct
    /// links and must keep distinct `pvaLinkConfig`s (pvxs
    /// `ioc/pvalink.h:65` — config is per-`pvaLink`; the shared channel
    /// is keyed separately by `(channelName, pvRequest)`,
    /// `ioc/pvalink.h:116`). The pvalink resolver caches each link's
    /// config under the string it is handed at resolve time; handing it
    /// the bare `pv` collapses every same-PV link onto one cache slot
    /// (last-writer-wins). This key restores per-link identity while the
    /// resolver still shares channels by bare PV + `(pipeline, Q)`.
    ///
    /// Delegates to [`pvajson_identity_key`] so the bridge can compute
    /// the identical key from the same `(pv, options)` at registration.
    pub fn link_identity_key(&self) -> String {
        pvajson_identity_key(&self.pv, &self.options)
    }
}

/// Separator byte between the bare `pv` and the encoded options in a
/// [`pvajson_identity_key`]. ASCII Unit Separator (`\u{1f}`) — a control
/// byte that cannot occur in a PV name or a `?key=value` user URI, so the
/// bridge can recover the bare PV by splitting on it and never confuses an
/// identity key for a convenience-URI query. Shared so base (the key
/// producer) and the bridge resolver (the key consumer) cannot drift.
pub const PVAJSON_IDENTITY_SEP: char = '\u{1f}';

/// Build the stable link-identity key for a structured pvalink from its
/// `pv` and parsed JLink options — see [`PvaJsonLink::link_identity_key`].
///
/// The encoding is INTERNAL to the base↔bridge link-set boundary and is
/// deliberately NOT pvxs link syntax and NOT a `?key=value` user URI:
/// the bare `pv` is the prefix up to the first [`PVAJSON_IDENTITY_SEP`],
/// followed by the options encoded `key=kind:value` and joined by the same
/// separator. Sorting makes the key canonical regardless of option
/// order. A separator the resolver never lenient-parses as a query is
/// what keeps this distinct from the convenience-URI path (so a
/// structured option is never re-read through the URI applier).
pub fn pvajson_identity_key(pv: &str, options: &[(String, JlinkValue)]) -> String {
    if options.is_empty() {
        return pv.to_string();
    }
    let mut pairs: Vec<String> = options
        .iter()
        .map(|(k, v)| match v {
            JlinkValue::Null => format!("{k}=n"),
            JlinkValue::Bool(b) => format!("{k}=b:{b}"),
            JlinkValue::Int(n) => format!("{k}=i:{n}"),
            JlinkValue::Str(s) => format!("{k}=s:{s}"),
        })
        .collect();
    pairs.sort();
    let mut key =
        String::with_capacity(pv.len() + 1 + pairs.iter().map(|p| p.len() + 1).sum::<usize>());
    key.push_str(pv);
    for p in &pairs {
        key.push(PVAJSON_IDENTITY_SEP);
        key.push_str(p);
    }
    key
}

/// Configuration for a `lnkCalc` link.
#[derive(Clone, Debug, PartialEq)]
pub struct CalcLink {
    /// Calc expression in epics-base postfix syntax — e.g. `"A+B*2"`,
    /// `"MAX(A,B,C)"`. Variables A..L bind to `args[0..12]`.
    pub expr: String,
    /// Input PV names. Each `args[i]` is fetched at link-read time and
    /// bound to the calc engine's variable slot at index `i` (0→A,
    /// 1→B, …). PV names may include a field suffix (`.VAL`, `.NORD`).
    /// Up to 12 inputs (calc engine A-L slots).
    pub args: Vec<String>,
    /// Input letter ('A'..='L') whose timestamp should be used for
    /// the result. `None` skips timestamp passthrough — the consumer
    /// uses its own `apply_timestamp` time.
    pub time_source: Option<char>,
}

/// Monitor propagation policy for links.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MonitorSwitch {
    /// NMS: Do not propagate alarm severity from link source.
    #[default]
    NoMaximize,
    /// MS: Maximize alarm severity from link source into this record.
    Maximize,
    /// MSS: Maximize severity, set status from source.
    MaximizeStatus,
    /// MSI: Maximize severity if source is invalid.
    MaximizeIfInvalid,
}

/// A database link to another record's field.
#[derive(Clone, Debug, PartialEq)]
pub struct DbLink {
    pub record: String,
    pub field: String,
    pub policy: LinkProcessPolicy,
    pub monitor_switch: MonitorSwitch,
}

impl DbLink {
    /// The channel name this link addresses: `record` for a default-`VAL`
    /// link, `record.FIELD` otherwise.
    ///
    /// This is the string C keeps verbatim in `plink->value.pv_link.pvname`
    /// and hands to `dbChannelCreate` (`dbDbLink.c:94`) and, when that fails,
    /// to `dbCaAddLink` (`dbLink.c:129`) — so the DB-link target name and the
    /// CA channel name a non-local target falls through to are the same
    /// string by construction, not by two sites agreeing to build it alike.
    pub fn channel_name(&self) -> String {
        if self.field == "VAL" {
            self.record.clone()
        } else {
            format!("{}.{}", self.record, self.field)
        }
    }
}

/// A Channel Access / PV Access external link to a remote PV.
///
/// carries the parsed `MS`/`NMS`/`MSI`/`MSS` maximize-
/// severity policy alongside the PV name, so the alarm gate is applied
/// at the record-processing boundary (uniform with [`DbLink`]) rather
/// than discarded as syntax. Mirrors the C link option parsed by
/// `dbStaticLib.c:2375` and applied by `recGbl.c:264`. The PV name never
/// carries trailing modifier tokens (they are stripped during parse);
/// it may retain a `ca://` scheme prefix, which the resolver strips.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaLink {
    pub pv: String,
    pub monitor_switch: MonitorSwitch,
    /// Link-processing policy carried by this link's parsed modifier,
    /// exactly as [`DbLink::policy`] does. When it is `CP`/`CPP`, the dbCa
    /// equivalent (`calink`) must subscribe a monitor and process the
    /// link-holder on every remote change (C `dbCa.c:993-994`
    /// `CA_DBPROCESS`); `cp_passive_only()` reads it identically to the local
    /// DB path.
    ///
    /// A `CP`/`CPP` `CaLink` arises from the `ca://` scheme
    /// (`"ca://OTHER:PV CPP"`) or from `classify_cp_link` re-routing a
    /// `CP`/`CPP` link whose target is not a local record. It does **not**
    /// arise from a plaintext ` CA` modifier: `CA` is a process *class* in C's
    /// single `else if` chain (`dbStaticLib.c:2369-2373`), mutually exclusive
    /// with `CP`/`CPP` and matched *before* `CP`, so `"OTHER:PV CP CA"` is
    /// `pvlOptCA` alone — a plain CA link whose holder never processes.
    pub policy: LinkProcessPolicy,
}

impl CaLink {
    /// CA link with the default (`NoMaximize`) alarm policy and no
    /// CP/CPP processing — the shape used by callers that have only a
    /// bare PV name (JSON `{ca:…}` links carry no plaintext modifier).
    pub fn new(pv: impl Into<String>) -> Self {
        Self {
            pv: pv.into(),
            monitor_switch: MonitorSwitch::NoMaximize,
            policy: LinkProcessPolicy::NoProcess,
        }
    }
}

/// Discriminated link *type* — the Rust analogue of the C
/// `link.h` `pv_link` / `constantStr` discrimination
/// (`modules/database/src/ioc/dbStatic/link.h:28-39`):
///
/// ```text
/// #define CONSTANT  0   -> LinkType::Constant
/// #define PV_LINK   1   -> (unresolved; resolves to Db or Ca)
/// #define DB_LINK   10  -> LinkType::Db
/// #define CA_LINK   11  -> LinkType::Ca
/// ```
///
/// C device support inspects `prec->inp.type` to decide behaviour —
/// e.g. `devEpidSoft.c:110` (`if (pepid->inp.type == CONSTANT)`),
/// `devEpidSoftCallback.c:116` (`if (ptriglink->type != CA_LINK)`).
/// This enum gives a record's `process()` / its device support the
/// same discrimination on the framework's string link fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkType {
    /// Empty / unset link — C has no value and no target.
    Empty,
    /// `CONSTANT` — the link is a literal numeric/string value, not a
    /// reference to another PV. C `link.h` `#define CONSTANT 0`.
    Constant,
    /// `DB_LINK` — a reference to a record.field in *this* IOC's
    /// database. C `link.h` `#define DB_LINK 10`.
    Db,
    /// `CA_LINK` — a reference to a PV reached over Channel Access /
    /// PV Access (a remote PV). C `link.h` `#define CA_LINK 11`.
    Ca,
    /// A hardware (`@dev …` / `#Cn Sn`) or `lnkCalc` JSON link — not
    /// one of the C `link.h` value-bearing scalar discriminants the
    /// records in this task care about. Kept distinct so a caller is
    /// never forced to mis-classify it.
    Other,
}

/// The link type a `device()` declaration demands, and the type a link's *text*
/// parses to — C's `link.h` discriminants, which are the two sides
/// `dbCanSetLink` compares.
///
/// This is deliberately NOT [`LinkType`]. `LinkType` is the *runtime* kind of an
/// established link (a `PV_LINK` becomes `DB_LINK` or `CA_LINK` once the target
/// is looked up at `iocInit`); `DbLinkType` is the *static* kind the `.dbd` and
/// the `.db` text agree on at load time, before any lookup has happened. Fusing
/// them is what makes a link field mean two things at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbLinkType {
    /// `CONSTANT` — a literal, or an unset link. Also what C expects of every
    /// link field that is *not* the device link (`INP`/`OUT`), because those
    /// have no `devSup` and `dbCanSetLink` defaults `expected_type` to
    /// `CONSTANT` (`dbStaticLib.c:2403`).
    Constant,
    /// `PV_LINK` — `record.FLD` / `pv MS PP`. Not yet resolved to DB vs CA.
    PvLink,
    /// `JSON_LINK` — `{const:1}`, `{calc:{...}}`, `{pva:{...}}`.
    JsonLink,
    /// `VME_IO` — `#Cn Sn @parm`.
    VmeIo,
    /// `CAMAC_IO`, `AB_IO`, … — declared by some device supports, never parsed
    /// into a distinct [`ParsedLink`] variant by this port.
    CamacIo,
    AbIo,
    GpibIo,
    BitbusIo,
    /// `INST_IO` — `@dev arg …`. The form every asyn-based device support uses.
    InstIo,
    BbgpibIo,
    RfIo,
    VxiIo,
}

impl DbLinkType {
    /// The C name, for the error message C prints.
    pub fn c_name(self) -> &'static str {
        match self {
            DbLinkType::Constant => "CONSTANT",
            DbLinkType::PvLink => "PV_LINK",
            DbLinkType::JsonLink => "JSON_LINK",
            DbLinkType::VmeIo => "VME_IO",
            DbLinkType::CamacIo => "CAMAC_IO",
            DbLinkType::AbIo => "AB_IO",
            DbLinkType::GpibIo => "GPIB_IO",
            DbLinkType::BitbusIo => "BITBUS_IO",
            DbLinkType::InstIo => "INST_IO",
            DbLinkType::BbgpibIo => "BBGPIB_IO",
            DbLinkType::RfIo => "RF_IO",
            DbLinkType::VxiIo => "VXI_IO",
        }
    }

    /// `dbCanSetLink` (`dbStaticLib.c:2400-2419`), verbatim: the parsed type may
    /// be installed on a field whose device declares `self` iff they are equal,
    /// or both are drawn from the interchangeable
    /// `{CONSTANT, JSON_LINK, PV_LINK}` set — those three are what
    /// `dbPutString` will happily re-parse into one another at runtime, so C
    /// lets the `.db` pick any of them.
    ///
    /// Every hardware type is therefore exact-match only: an `INST_IO` device
    /// support gets `@…` or nothing.
    pub fn accepts(self, parsed: DbLinkType) -> bool {
        if self == parsed {
            return true;
        }
        let soft = |t: DbLinkType| {
            matches!(
                t,
                DbLinkType::Constant | DbLinkType::JsonLink | DbLinkType::PvLink
            )
        };
        soft(self) && soft(parsed)
    }
}

/// The link type the `.dbd` declares for one link field of one record — C's
/// `dbCanSetLink` `expected_type` (`dbStaticLib.c:2403`).
///
/// `None` means *no declaration exists*, not "no constraint": the record type
/// brings no vendored `.dbd` (`motor`, `table`, `scaler`, … — their device
/// support registers at runtime), or its `DTYP` names a device no vendored
/// `.dbd` declares (`asynInt32` on an `ai`). C has a `devSup` there and this
/// port does not, so there is nothing to compare and nothing is invented.
///
/// `dtyp` is the record's DTYP text, or `None` when the `.db` never spelled it —
/// which is NOT "no device": `DTYP` is a `DBF_DEVICE` menu index and its default
/// is 0, so the record is bound to the FIRST device its `.dbd` declares. C
/// rejects `record(ai,"X") { field(INP,"@instio parm") }` on exactly that
/// ground (measured on the built 7.0.10.1-DEV `softIoc`).
pub fn declared_link_type(
    record_type: &str,
    dtyp: Option<&str>,
    upper_field: &str,
) -> Option<DbLinkType> {
    // C `dbLexRoutines.c:571`: only INP/OUT are device links. Every other link
    // field has no `devSup`, so `dbCanSetLink` expects CONSTANT — which is why
    // the C IOC refuses `field(SDIS,"@instio parm")` on any record type.
    if !matches!(upper_field, "INP" | "OUT") {
        return Some(DbLinkType::Constant);
    }
    let menu = super::dbd_generated::device_menu(record_type)?;
    let dtyp = dtyp
        .filter(|d| !d.is_empty())
        .or_else(|| menu.first().copied())?;
    super::dbd_generated::device_link_type(record_type, dtyp)
}

/// C `dbCanSetLink` (`dbStaticLib.c:2400-2419`) as the single gate every link
/// assignment passes: the db-load one (`dbStaticLib.c:2222`) and the runtime
/// `dbPutField` one (`dbAccess.c:1133`), which in C are two call sites of this
/// one function and here are two call sites of this one function.
///
/// `Ok(())` when there is no declaration to enforce (see [`declared_link_type`])
/// or when the link's parsed type is one the bound device support takes.
pub fn check_link_assignment(
    record_type: &str,
    dtyp: Option<&str>,
    upper_field: &str,
    text: &str,
) -> CaResult<()> {
    let Some(class) = crate::types::dbf_link_class(record_type, upper_field) else {
        return Ok(());
    };
    let Some(expected) = declared_link_type(record_type, dtyp, upper_field) else {
        return Ok(());
    };
    let ftype = match class {
        DbfLinkClass::InLink => LinkFieldType::In,
        DbfLinkClass::OutLink => LinkFieldType::Out,
        DbfLinkClass::FwdLink => LinkFieldType::Fwd,
    };
    let Some(parsed) = parse_link_field(text, ftype).db_link_type() else {
        return Ok(());
    };
    if expected.accepts(parsed) {
        return Ok(());
    }
    Err(CaError::InvalidValue(format!(
        "{record_type}.{upper_field}: can't initialize link type {} with \"{text}\" (type {})",
        expected.c_name(),
        parsed.c_name(),
    )))
}

impl ParsedLink {
    /// The static [`DbLinkType`] this link's text parses to — C's
    /// `dbLinkInfo::ltype` as `dbParseLink` sets it.
    ///
    /// `ParsedLink::None` is `CONSTANT` because that is what C's parser makes of
    /// the empty string, and C rejects `field(INP,"")` on an `INST_IO` device
    /// for exactly that reason (measured on the built 7.0.10.1-DEV `softIoc`).
    /// A link the `.db` never mentions at all is a different thing — C never
    /// reaches `dbCanSetLink` for it (`if (!plink->text) continue;`,
    /// `dbStaticLib.c:2213`) — and so it must not be handed to this function.
    ///
    /// `None` for [`HwLinkKind::Other`]: this port keeps the payload of the
    /// remaining C hardware forms (`#Bn Cn Nn …`) verbatim rather than
    /// discriminating them, so there is no type to compare and the honest answer
    /// is "cannot say" — not a guess that would reject a `.db` C accepts.
    pub fn db_link_type(&self) -> Option<DbLinkType> {
        Some(match self {
            ParsedLink::None | ParsedLink::Constant(_) => DbLinkType::Constant,
            ParsedLink::Db(_) | ParsedLink::Ca(_) | ParsedLink::Pva(_) => DbLinkType::PvLink,
            ParsedLink::PvaJson(_) | ParsedLink::Calc(_) => DbLinkType::JsonLink,
            ParsedLink::Hw(hw) => match hw.kind {
                HwLinkKind::InstIo => DbLinkType::InstIo,
                HwLinkKind::VmeIo => DbLinkType::VmeIo,
                HwLinkKind::Other => return None,
            },
        })
    }

    /// The discriminated [`LinkType`] of this link — the C
    /// `prec->xxx.type` analogue. See [`LinkType`] for the C mapping.
    pub fn link_type(&self) -> LinkType {
        match self {
            ParsedLink::None => LinkType::Empty,
            ParsedLink::Constant(_) => LinkType::Constant,
            ParsedLink::Db(_) => LinkType::Db,
            ParsedLink::Ca(_) | ParsedLink::Pva(_) | ParsedLink::PvaJson(_) => LinkType::Ca,
            ParsedLink::Hw(_) | ParsedLink::Calc(_) => LinkType::Other,
        }
    }

    /// Extract the constant as an EpicsValue — the C `dbConstLoadScalar` /
    /// `dbConstLoadArray` pair (`dbConstLink.c:152-199`), which is what a
    /// record's `init_record` gets out of a constant link through `dbLoadLink`
    /// / `dbLoadLinkArray`.
    ///
    /// A bracketed constant (`[1, 2, 3]`) is an ARRAY: C hands the text to
    /// `dbPutConvertJSON`, which yields `nRequest` elements. Yielding the
    /// bracket text as a String instead left every array-valued constant link
    /// (`field(INP, "[1,2,3]")` on an aai/waveform, a constant closed-loop aao
    /// DOL) delivering a nonsense string to a typed array buffer.
    pub fn constant_value(&self) -> Option<EpicsValue> {
        if let ParsedLink::Constant(s) = self {
            if let Some(inner) = s
                .trim()
                .strip_prefix('[')
                .and_then(|body| body.strip_suffix(']'))
            {
                return Some(constant_array_value(inner));
            }
            if let Some(v) = parse_c_double(s) {
                Some(EpicsValue::Double(v))
            } else {
                Some(EpicsValue::String(s.clone().into()))
            }
        } else {
            None
        }
    }

    pub fn is_db(&self) -> bool {
        matches!(self, ParsedLink::Db(_))
    }

    /// True iff this link is a hardware (`@dev …` / `#Cn Sn`) link.
    pub fn is_hw(&self) -> bool {
        matches!(self, ParsedLink::Hw(_))
    }

    /// True iff this link is a writable OUT-link target — a local
    /// `Db` link or an external `Ca`/`Pva` link.
    ///
    /// The OUT-link write stage in `processing.rs` uses this to decide
    /// whether a record's OUT link has a target the value should be
    /// driven into. `Constant`/`Hw`/`Calc`/`None` are not writable
    /// targets (C `dbPutLink` returns `S_db_noLSET` for a link with no
    /// lset). Mirrors C `dbLink.c::dbPutLink` (dbLink.c:434-448), which
    /// dispatches DB *and* CA link writes uniformly through the link
    /// set's `putValue`.
    pub fn is_writable_out_link(&self) -> bool {
        matches!(
            self,
            ParsedLink::Db(_) | ParsedLink::Ca(_) | ParsedLink::Pva(_) | ParsedLink::PvaJson(_)
        )
    }

    /// Boundary identity string for an external (`Ca`/`Pva`/`PvaJson`)
    /// link, else `None` — the key the database hands the link-set when
    /// resolving / writing / forwarding this link.
    ///
    /// For `Ca` and `Pva` this is the channel-name / link string verbatim
    /// (borrowed). For `PvaJson` it is the per-link identity key
    /// ([`PvaJsonLink::link_identity_key`], owned), NOT the bare `pv`:
    /// two structured links to the same PV that differ by options must
    /// resolve to their own per-link config, so the boundary key must
    /// carry that identity (the resolver still shares the channel by bare
    /// PV — pvxs `ioc/pvalink.h:65,116`). Callers feed the result to the
    /// link-set, which is the only consumer; nothing relies on this
    /// returning the bare PV name.
    pub fn external_pv_name(&self) -> Option<Cow<'_, str>> {
        match self {
            ParsedLink::Ca(ca) => Some(Cow::Borrowed(ca.pv.as_str())),
            ParsedLink::Pva(name) => Some(Cow::Borrowed(name.as_str())),
            ParsedLink::PvaJson(j) => Some(Cow::Owned(j.link_identity_key())),
            _ => None,
        }
    }

    /// Maximize-severity policy carried by this link's parsed modifier.
    /// `Db`/`Ca` links carry an explicit [`MonitorSwitch`]; PVA links
    /// keep their `sevr` as link data (a `Pva` channel-name string or a
    /// `PvaJson` `sevr` option) for the pvalink lset to apply, so they
    /// report `None` here (the lset gate stands in). Used by record
    /// processing to apply the MS/NMS/MSI/MSS gate at the fold boundary.
    pub fn monitor_switch(&self) -> Option<MonitorSwitch> {
        match self {
            ParsedLink::Db(db) => Some(db.monitor_switch),
            ParsedLink::Ca(ca) => Some(ca.monitor_switch),
            _ => None,
        }
    }
}

/// The two link-value shapes the pvxs JLink root callbacks accept at parse
/// depth 0: a JSON string (channel-name shorthand) or a JSON object/map
/// (longhand options). Every other root token — null, bool, integer, real,
/// array — installs no channel name in pvxs, so [`classify_pva_root_value`]
/// returns `None` for them rather than coercing the raw token into a PV
/// name.
enum PvaRootValue<'a> {
    /// `{ ... }` longhand options map (handed to the sub-object parser).
    Object(&'a str),
    /// `"name"` / `'name'` string shorthand — exactly one matching quote
    /// pair stripped, contents kept verbatim (no semantic-character trim).
    StringName(&'a str),
}

/// Classify a root `pva`/`ca` link value by JSON shape. Accepts only a JSON
/// object or a (single- or double-) quoted string; rejects bare
/// `null`/`true`/`false`/number/array tokens the way the pvxs root JLink
/// callbacks do — `pva_parse_string` assigns `channelName` only at depth 0
/// while `pva_parse_null`/`bool`/`integer` ignore root-depth values
/// (pvalink_jlif.cpp:74-100,143-154).
fn classify_pva_root_value(value: &str) -> Option<PvaRootValue<'_>> {
    let v = value.trim();
    if v.starts_with('{') {
        return Some(PvaRootValue::Object(v));
    }
    for quote in ['"', '\''] {
        if let Some(rest) = v.strip_prefix(quote) {
            if let Some(inner) = rest.strip_suffix(quote) {
                return Some(PvaRootValue::StringName(inner));
            }
        }
    }
    None
}

/// Try to recognize a JSON-style link option (epics-base PR #86).
///
/// epics-base accepts inline JSON link options like `{ca: {pv: "foo"}}`,
/// `{pva: {pv: "foo"}}`, `{const: 1.5}`. The parser is JSON5-leaning
/// (unquoted keys, single quotes) — we accept that subset here using a
/// lightweight prepass that lowercases the leading key, then hands the
/// inner body to `serde_json::Value`.
///
/// Returns `Some(parsed)` when the string is a recognized JSON link;
/// `None` lets the caller fall through to legacy plain-text parsing.
/// The elements of a bracketed constant link, C `dbPutConvertJSON` on the
/// text `dbConstLoadArray` hands it (`dbConstLink.c:177-199`).
///
/// Numeric elements produce a `DoubleArray` — the record lands it in its own
/// typed buffer afterwards (C converts INTO `bptr` with `dbFastConvert`, so
/// the element type is the record's FTVL, never the link's). Any non-numeric
/// element makes the whole literal a string array (a `CHAR` waveform seeded
/// with names, `["one","two"]`). An empty `[]` is zero elements — C yields
/// `nRequest = 0`, i.e. NORD stays 0.
fn constant_array_value(inner: &str) -> EpicsValue {
    let body = inner.trim();
    if body.is_empty() {
        return EpicsValue::DoubleArray(Vec::new());
    }
    // A quoted element is a yajl string: decoded through the one owner. A bare
    // element (a number) carries no escapes.
    let elements: Vec<String> = body
        .split(',')
        .map(|e| {
            let e = e.trim();
            decode_json_string_token(e).unwrap_or_else(|| e.to_string())
        })
        .collect();
    let numbers: Option<Vec<f64>> = elements.iter().map(|e| e.parse::<f64>().ok()).collect();
    match numbers {
        Some(nums) => EpicsValue::DoubleArray(nums),
        None => EpicsValue::StringArray(elements.into_iter().map(|e| e.into()).collect()),
    }
}

/// C `epicsParseDouble(pstr, &value, NULL)` — the ONE test that decides whether
/// a link string is a number, and the ONE conversion that turns it into a value.
/// `dbParseLink` uses it to classify (`dbStaticLib.c:2346-2349`) and
/// `dbConstLink.c`'s `cvt_st_*` family uses the same C library parse to load, so
/// the two must agree: whatever classifies as CONSTANT must also load.
///
/// It is `strtod` under the hood, which accepts a HEX literal — `dbConstLink.c:34`:
/// *"constants may contain hex numbers, whereas database conversions can't"*.
/// softIoc: `field(DOL,"0x1f")` reports `DOL : CONSTANT 0x1f` and comes up at
/// VAL=31; `field(INP,"0x10")` on an ai loads VAL=16. Trailing text is rejected
/// (C passes a NULL units pointer → `S_stdlib_extraneous`), which is what keeps
/// `"5 PP"` a PV link rather than a constant.
///
/// So this is [`crate::runtime::stdlib::epics_parse_double`] and nothing else — C's ONE
/// `epicsParseDouble`, shared with the CA env-knob parser. A hand-rolled
/// re-implementation drifted from it on three families, all probed on softIoc
/// 7.0.10:
///
/// ```text
/// field(INP,"0x1p4")                -> CONSTANT, VAL=16               (hex float)
/// field(INP,"0xffffffffffffffffff") -> CONSTANT, VAL=4.72236648e+21   (wide hex)
/// field(INP,"1e400")                -> CA_LINK, UDF=1                 (ERANGE)
/// field(INP,"1e-320")               -> CA_LINK, UDF=1                 (ERANGE)
/// ```
///
/// The ERANGE half is what makes the difference structural rather than
/// cosmetic: `epicsParseDouble` returns a NON-ZERO status for `1e400`, so C's
/// `dbParseLink` (`dbStaticLib.c:2346-2349`) never reaches CONSTANT — the
/// literal becomes a PV link, and the record stays UDF. The pre-fix port made
/// it a defined record holding `inf`.
pub fn parse_c_double(s: &str) -> Option<f64> {
    crate::runtime::stdlib::epics_parse_double(s).ok()
}

/// What C's `dbLoadLinkLS` (dbLink.c:244) delivers into a long-string VAL.
///
/// `loadLS` is a lset entry of its own — NOT `recGblInitConstantLink` — and only
/// two lsets implement it, with different results:
///
/// * a plain CONSTANT link (`dbConstLink.c:178` `dbConstLoadLS`) runs the link
///   text through `dbLSConvertJSON`, and
/// * a JSON `{const:…}` link (`lnkConst.c:419` `lnkConst_loadLS`) copies its
///   string value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LsLoad {
    /// The link delivered a string: copy it into VAL (truncated at `SIZV-1`),
    /// `LEN = strlen + 1`.
    Text(String),
    /// C's number case. `dbLSConvertJSON` (dbConvertJSON.c:191-236) never calls
    /// `yajl_complete_parse`, so a bare number is still a pending token when the
    /// parse ends: NO callback fires, the destination buffer is untouched, and
    /// `*plen = pdest - pdest + 1` makes `LEN = 1`. That is what defines an
    /// `lso` with `field(DOL,"5")` while leaving `VAL` empty (softIoc: VAL "",
    /// LEN 1, UDF 0).
    LenOnly,
}

/// C `dbLoadLinkLS` over a link field's `.db` text — the long-string half of the
/// init-time constant load. `None` when the link loads nothing: a PV link (no
/// `loadLS` at init), an empty link, or a JSON `{const:…}` whose value is not a
/// string (`lnkConst_loadLS` returns `S_db_badField` for the numeric types).
///
/// softIoc (EPICS 7.0.10, linux-x86_64), `dbgf` after `iocInit`:
///
/// ```text
/// record(lso,"L1"){field(DOL,"5")}              VAL ""         LEN 1  UDF 0
/// record(lso,"L2"){field(DOL,{const:"hello"})}  VAL "hello"    LEN 6  UDF 0
/// record(lsi,"L3"){field(INP,{const:"hi there"})} VAL "hi there" LEN 9 UDF 0
/// record(lsi,"L4"){field(INP,"5")}              VAL ""         LEN 1  UDF 0
/// ```
pub fn load_link_ls(text: &str) -> Option<LsLoad> {
    let s = text.trim();
    if s.is_empty() {
        return None;
    }
    // JSON link: only `{const:…}` has a `loadLS`, and only its string forms
    // (`sc40`, and `ac40`'s first element) deliver text.
    if s.starts_with('{') && s.ends_with('}') {
        let value = json_const_value(s)?;
        return json_string_value(value).map(LsLoad::Text);
    }
    // Plain link: `loadLS` exists only on the CONSTANT lset, and a plain link is
    // CONSTANT iff `epicsParseDouble` accepts it — i.e. iff it is a number, the
    // one case `dbLSConvertJSON` leaves the buffer untouched.
    parse_c_double(s).map(|_| LsLoad::LenOnly)
}

/// The raw (un-dequoted) value text of a `{const: …}` JSON link, or `None` when
/// `s` is some other JSON link.
fn json_const_value(s: &str) -> Option<&str> {
    let inner = s[1..s.len() - 1].trim_start();
    let (key, rest) = inner.split_once(':')?;
    let key = key.trim().trim_matches('"').trim_matches('\'');
    if !key.eq_ignore_ascii_case("const") {
        return None;
    }
    Some(rest.trim().trim_end_matches(',').trim())
}

/// The JSON string a `{const:…}` link carries, DECODED — `"hi"` and the
/// string-array shorthand `["hi", …]` (C `ac40`, whose `loadLS` takes element 0).
/// A numeric value is not a string: `lnkConst_loadLS`'s `default` arm returns
/// `S_db_badField`, so nothing is loaded and LEN stays 0.
///
/// The decode is yajl's, not this function's: a `{…}` value's escapes are
/// translated by `dbJLinkParse` → yajl, never by the `.db` lexer
/// ([`decode_json_string_token`]).
fn json_string_value(value: &str) -> Option<String> {
    let v = value.trim();
    let first = if let Some(rest) = v.strip_prefix('[') {
        rest.trim_start().split(',').next()?.trim()
    } else {
        v
    };
    decode_json_string_token(first)
}

fn try_parse_json_link(s: &str) -> Option<ParsedLink> {
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return None;
    }
    // First key: scan until ':' or end. Trim outer braces, accept
    // optional whitespace + optional quote around the key.
    let inner = &s[1..s.len() - 1];
    let inner_trim = inner.trim_start();
    let (key_raw, rest) = match inner_trim.split_once(':') {
        Some((k, r)) => (k.trim(), r.trim()),
        None => return None,
    };
    let key = key_raw
        .trim_matches('"')
        .trim_matches('\'')
        .to_ascii_lowercase();
    match key.as_str() {
        "const" => {
            // Constant: bare numeric, quoted string, or array. A quoted string
            // is a yajl string — its escapes are decoded here, by the one owner
            // (`lnkConst.c:199` receives the DECODED bytes from
            // `yajl_parser.c:273-281`); a bare token carries no escapes and is
            // taken verbatim, as is an array (its elements are decoded by
            // `constant_array_value`).
            let v = rest.trim_end_matches(',').trim();
            let decoded = match decode_json_string_token(v) {
                Some(s) => s,
                None => v.to_string(),
            };
            if decoded.is_empty() {
                Some(ParsedLink::None)
            } else {
                Some(ParsedLink::Constant(decoded))
            }
        }
        "ca" | "pva" => {
            // pvxs accepts the link value in two forms (pvalink_jlif.cpp:24-31
            // documents the string shorthand; pva_parse_string at :143-149
            // takes a string value at depth 0 as the channel name and a `pv`
            // string inside a map):
            //   shorthand  { pva: "name" }             — string IS the channel
            //              name verbatim; any `?`/`&` in it is link DATA, not
            //              option syntax (pvalink_jlif.cpp:143-149).
            //   longhand   { pva: { pv: "name", ... } } — map with a `pv`
            //              member; the other keys are STRUCTURED JLink options
            //              (pvalink_jlif.cpp:69-196), preserved as such so the
            //              pvalink bridge reconstructs PvaLinkConfig from map
            //              keys rather than from a synthetic `?key=value` query
            //              (which pvxs has no parser for — :286-300).
            // For CA only the PV name matters (CA links bypass pvalink).
            //
            // Branch on the value's JSON shape so the recognized string
            // shorthand is routed to the PVA/CA resolver instead of falling
            // through to legacy DB parsing (which would treat the raw JSON
            // text as a record name).
            let value = rest.trim_end_matches(',').trim();
            // Classify the root value by JSON shape — only a string
            // (channel-name shorthand) or an object/map (longhand options)
            // is a valid pvalink root; a bare `true`/`5`/`null`/`[..]` token
            // installs no channel name in pvxs and must NOT be coerced into a
            // literal PV name. `?` returns `None` here so a non-string root
            // falls through to legacy parsing instead of dialing a remote PV.
            match classify_pva_root_value(value)? {
                PvaRootValue::Object(obj) => {
                    if key == "ca" {
                        // JSON CA links carry no plain-text MS modifier and
                        // ignore pvalink options; take only the PV name.
                        // Alarm policy defaults to NoMaximize.
                        Some(ParsedLink::Ca(CaLink::new(
                            extract_pv_and_opts_from_subobject(obj)?.0,
                        )))
                    } else {
                        // PVA longhand: keep the options as structured JLink
                        // members.
                        let (pv, options) = extract_pv_and_opts_from_subobject(obj)?;
                        if options.is_empty() {
                            Some(ParsedLink::Pva(pv))
                        } else {
                            Some(ParsedLink::PvaJson(PvaJsonLink { pv, options }))
                        }
                    }
                }
                PvaRootValue::StringName(name) => {
                    // String shorthand: the contents are the verbatim channel
                    // name. Do NOT split `?` — it is link data (pvxs treats a
                    // string pvalink as the channel name in full). It is still a
                    // yajl string, so its escapes are decoded by the one owner.
                    let name = decode_json_string(name.trim());
                    if name.is_empty() {
                        return None;
                    }
                    if key == "ca" {
                        Some(ParsedLink::Ca(CaLink::new(name)))
                    } else {
                        Some(ParsedLink::Pva(name))
                    }
                }
            }
        }
        "calc" => {
            // Form: { calc: { expr: "...", args: ["pv1","pv2"], time: "A" } }
            //   - expr (required, string)
            //   - args (optional, JSON string array)
            //   - time (optional, single uppercase letter A..L)
            // We use serde_json for proper parsing — the previous
            // permissive substring approach can't handle nested
            // arrays / quoted commas reliably.
            let body = rest.trim();
            // Trim trailing brace-of-outer-object swallowed during the
            // initial split. The body always starts with `{` and the
            // outer brace was already stripped above.
            let body_obj = if body.ends_with('}') {
                body
            } else {
                return None;
            };
            let val: serde_json::Value = serde_json::from_str(body_obj).ok()?;
            let obj = val.as_object()?;
            let expr = obj.get("expr").and_then(|v| v.as_str())?.to_string();
            let args: Vec<String> = obj
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            // 12 input cap — calc engine A..L map.
            if args.len() > 12 {
                return None;
            }
            let time_source = obj
                .get("time")
                .and_then(|v| v.as_str())
                .and_then(|s| s.chars().next())
                .filter(|c| ('A'..='L').contains(c));
            Some(ParsedLink::Calc(CalcLink {
                expr,
                args,
                time_source,
            }))
        }
        _ => None,
    }
}

/// Extract the `pv` name and all other key-value options from a
/// JSON-ish sub-object body. Returns `(pv_name, options)` where
/// `options` is every non-`pv` key as a structured `(key, value)` pair
/// in source order (empty when there are no extra options). Accepts
/// unquoted keys, single or double quotes around values.
///
/// The options are kept STRUCTURED — not flattened into a `?k=v&…`
/// query string — so the pvalink consumer reconstructs `PvaLinkConfig`
/// from JLink map members (pvalink_jlif.cpp:69-196), matching pvxs,
/// which has no URI-query parser in its JLink callback table
/// (pvalink_jlif.cpp:286-300). Key case is preserved so a case-sensitive
/// key like `Q` survives, AND the JSON value KIND is preserved as a
/// [`JlinkValue`]: a quoted token is a string, a bare `true`/`false`/
/// `null` is the JSON keyword, a bare integer is an integer. pvxs
/// dispatches its pvalink callbacks strictly by this kind
/// (pvalink_jlif.cpp:286-300), so flattening a bare `pipeline:true` and
/// a quoted `pipeline:"yes"` to the same text would let Rust honor an
/// option pvxs ignores.
fn extract_pv_and_opts_from_subobject(body: &str) -> Option<(String, Vec<(String, JlinkValue)>)> {
    let body = body.trim_start_matches('{').trim_end_matches('}').trim();
    let mut pv: Option<String> = None;
    let mut opts: Vec<(String, JlinkValue)> = Vec::new();
    for entry in body.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // split_once splits at the FIRST ':' — PV names like "REC:AI"
        // have ':' inside the quoted value, but since we split on the
        // key separator `:` first (before the opening `"`) the colon
        // inside the quoted value survives (it's the second or later
        // colon in the entry string).
        let (k, v) = entry.split_once(':')?;
        let k_raw = k.trim().trim_matches('"').trim_matches('\'');
        // Trim only the trailing entry comma + whitespace. DO NOT strip
        // the value's quotes here — quote presence is what distinguishes
        // a JSON string from a bare bool/integer/null, the distinction
        // pvxs dispatches on (pvalink_jlif.cpp:286-300).
        let v_trimmed = v.trim().trim_matches(',').trim();
        if v_trimmed.is_empty() {
            continue;
        }
        if k_raw.eq_ignore_ascii_case("pv") {
            // `pv` is always the channel-name string: dequoted AND decoded by
            // the one JSON-string owner (yajl hands the jlif a decoded string).
            let name = decode_json_string_token(v_trimmed).unwrap_or_else(|| v_trimmed.to_string());
            if !name.is_empty() {
                pv = Some(name);
            }
        } else if let Some(val) = classify_jlink_value(v_trimmed) {
            // Preserve original key case (case-sensitive for keys like
            // `Q`) AND the JSON value KIND.
            opts.push((k_raw.to_string(), val));
        }
    }
    Some((pv?, opts))
}

/// Classify a raw JLink option value token into its JSON value KIND,
/// preserving the bool/integer/string distinction pvxs dispatches on
/// (pvxs `ioc/pvalink_jlif.cpp:286-300`). A quoted token (single or
/// double quote, the EPICS-relaxed JSON dialect) is a string; the bare
/// literals `true`/`false`/`null` are the JSON keywords (yajl is
/// case-sensitive, so only lowercase); a bare integer is an integer.
/// Anything else bare — a float (which pvxs's `NULL` real callback slot
/// rejects) or an unquoted word — is kept as a string so the consumer's
/// per-option validation decides, matching the previous lenient
/// tokenizer that treated every bare value as text.
fn classify_jlink_value(raw: &str) -> Option<JlinkValue> {
    let t = raw.trim();
    if let Some(decoded) = decode_json_string_token(t) {
        return Some(JlinkValue::Str(decoded));
    }
    match t {
        "" => None,
        "true" => Some(JlinkValue::Bool(true)),
        "false" => Some(JlinkValue::Bool(false)),
        "null" => Some(JlinkValue::Null),
        _ => match t.parse::<i64>() {
            Ok(n) => Some(JlinkValue::Int(n)),
            Err(_) => Some(JlinkValue::Str(t.to_string())),
        },
    }
}

/// Recognize a hardware (`@dev …` / `#Cn Sn`) link. Mirrors epics-base
/// PR #213. Hex literals in args are kept as-is — `@dev 0x1A` survives
/// tokenization with `0x1A` as a single arg, since base's #213 was
/// specifically about preserving such literals through the args list.
fn try_parse_hw_link(s: &str) -> Option<ParsedLink> {
    if s.is_empty() {
        return None;
    }
    let first = s.as_bytes()[0];
    if first == b'@' {
        let raw = s[1..].trim().to_string();
        let args: Vec<String> = raw.split_whitespace().map(|t| t.to_string()).collect();
        return Some(ParsedLink::Hw(HwLink {
            kind: HwLinkKind::InstIo,
            args,
            raw,
        }));
    }
    if first == b'#' {
        let raw = s[1..].trim().to_string();
        let args: Vec<String> = raw.split_whitespace().map(|t| t.to_string()).collect();
        return Some(ParsedLink::Hw(HwLink {
            kind: HwLinkKind::VmeIo,
            args,
            raw,
        }));
    }
    None
}

/// The **one** process-class modifier a link carries.
///
/// C `dbParseLink` (`dbStaticLib.c:2369-2373`) resolves the process class with
/// a single `else if` chain that **assigns** (never OR-s) exactly one bit:
///
/// ```c
/// if      (strstr(pstr, "NPP")) pinfo->modifiers = 0;
/// else if (strstr(pstr, "CPP")) pinfo->modifiers = pvlOptCPP;
/// else if (strstr(pstr, "PP"))  pinfo->modifiers = pvlOptPP;
/// else if (strstr(pstr, "CA"))  pinfo->modifiers = pvlOptCA;
/// else if (strstr(pstr, "CP"))  pinfo->modifiers = pvlOptCP;
/// ```
///
/// So the classes are mutually exclusive and the *chain order* — not the token
/// order in the string — decides which one wins. `"REC CP NPP"` is NPP,
/// `"OTHER:PV CP CA"` is CA (a plain CA link that never processes its holder),
/// and `"REC PP CA"` is PP (a **local DB** link, because `dbAccess.c:1104`
/// routes to `dbDbInitLink` when none of `CA|CP|CPP` is set).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum ProcessClass {
    /// `NPP`, and the modifier-less default: C `memset`s the modifier set to
    /// zero (`dbStaticLib.c:2252`) and only an explicit token sets a bit.
    #[default]
    Npp,
    Cpp,
    Pp,
    Ca,
    Cp,
}

impl ProcessClass {
    /// `(policy, force_ca)` — how this class lands in the port's link model.
    /// Only `pvlOptCA` makes the link an *external* CA channel; `CP`/`CPP`
    /// keep the local-DB shape and are routed by `classify_cp_link`.
    fn resolve(self) -> (LinkProcessPolicy, bool) {
        match self {
            Self::Npp => (LinkProcessPolicy::NoProcess, false),
            Self::Cpp => (LinkProcessPolicy::ChannelProcessPassive, false),
            Self::Pp => (LinkProcessPolicy::ProcessPassive, false),
            Self::Ca => (LinkProcessPolicy::NoProcess, true),
            Self::Cp => (LinkProcessPolicy::ChannelProcess, false),
        }
    }

    fn is_cp_or_cpp(self) -> bool {
        matches!(self, Self::Cp | Self::Cpp)
    }

    /// C `dbStaticLib.c:2369-2373` — a single `else if` chain over
    /// `strstr(pstr, …)` in the order `NPP, CPP, PP, CA, CP` that *assigns*
    /// (never OR-s) exactly one class. The order is load-bearing: `CPP` must be
    /// tested before `PP`, since `"CPP"` contains `"PP"`.
    fn from_modifier_text(mods: &str) -> Self {
        if mods.contains("NPP") {
            Self::Npp
        } else if mods.contains("CPP") {
            Self::Cpp
        } else if mods.contains("PP") {
            Self::Pp
        } else if mods.contains("CA") {
            Self::Ca
        } else if mods.contains("CP") {
            Self::Cp
        } else {
            Self::Npp
        }
    }
}

/// Which link *field* a link string is being parsed for — C's `ftype` argument
/// to `dbParseLink`, i.e. the `pfldDes->field_type` of the field that holds the
/// link (`dbAccess.c:1094` `dbPutFieldLink`).
///
/// The field type filters the parsed modifiers (`dbStaticLib.c:2380-2391`),
/// which is why the same text means different things in `INP` and `OUT`:
///
/// ```c
/// switch(ftype) {
/// case DBF_INLINK:  /* accept all */ break;
/// case DBF_OUTLINK:
///     if (modifiers & (pvlOptCPP|pvlOptCP))
///         errlogPrintf(ERL_WARNING ": Discarding CP/CPP modifier in CA output link …");
///     modifiers &= ~(pvlOptCPP|pvlOptCP);
///     break;
/// case DBF_FWDLINK: modifiers &= pvlOptCA; break;
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkFieldType {
    /// `DBF_INLINK` — `INP`, `INPA`…, `DOL`, `SDIS`, `TSEL`, `SIML`, … Every
    /// modifier is accepted.
    In,
    /// `DBF_OUTLINK` — `OUT`, `dfanout.OUTA`…, `sseq.LNK1`…, `SIOL`. `CP`/`CPP`
    /// is meaningless (it asks for input-side "process on change") and is
    /// discarded with a warning.
    Out,
    /// `DBF_FWDLINK` — `FLNK` (`dbCommon.dbd.pod:575`), `fanout.LNK1`…
    /// (`fanoutRecord.dbd.pod:144`). Only `CA` survives; every process and
    /// maximize-severity modifier is masked off.
    Fwd,
}

impl LinkFieldType {
    /// Apply C's per-field-type modifier mask (`dbStaticLib.c:2380-2391`).
    ///
    /// Silent by design: some OUT links (`dfanout.OUTn`, `sseq.LNKn`) are
    /// re-parsed from their raw text on every process cycle, so C's one-shot
    /// load-time `errlogPrintf` cannot live here. The diagnostic is emitted at
    /// the one-shot put/load boundary via [`out_link_discards_cp`], which also
    /// has the holder record's name — the `%s.%s` half of C's message.
    fn mask(self, class: ProcessClass, ms: MonitorSwitch) -> (ProcessClass, MonitorSwitch) {
        match self {
            Self::In => (class, ms),
            // `modifiers &= ~(pvlOptCPP|pvlOptCP)`.
            Self::Out => {
                let class = if class.is_cp_or_cpp() {
                    ProcessClass::Npp
                } else {
                    class
                };
                (class, ms)
            }
            // `modifiers &= pvlOptCA` — everything except the CA bit is
            // cleared, including the maximize-severity switch.
            Self::Fwd => {
                let class = if class == ProcessClass::Ca {
                    ProcessClass::Ca
                } else {
                    ProcessClass::Npp
                };
                (class, MonitorSwitch::NoMaximize)
            }
        }
    }
}

/// Does `s`, read as a `DBF_OUTLINK`, carry a `CP`/`CPP` modifier that
/// [`parse_output_link_v2`] will discard?
///
/// The query behind C's load-time warning (`dbStaticLib.c:2382-2386`
/// `"Discarding CP/CPP modifier in CA output link from %s.%s to %s."`). It uses
/// the same modifier resolution as the parser, so it cannot drift from what is
/// actually discarded — unlike a trailing-`" CP"` text test, which misses
/// `"TARGET CP MS"` and mis-fires on a target whose *name* ends in `CP`.
pub fn out_link_discards_cp(s: &str) -> bool {
    let s = s.trim();
    // Only a plain PV link reaches the modifier scan in C; a JSON/hardware/
    // scheme link never does.
    if s.is_empty() || s.starts_with(['{', '@', '#', '"']) {
        return false;
    }
    let Some((_, mods)) = s.split_once(' ') else {
        return false;
    };
    // Resolve through the same chain the parser uses, so the warning cannot
    // drift from the mask: exactly the classes the OUT mask downgrades.
    ProcessClass::from_modifier_text(mods).is_cp_or_cpp()
}

/// Split a link string into its target and its modifiers, mirroring C
/// `dbParseLink` (`dbStaticLib.c:2359-2379`).
///
/// C isolates the target at the **first space** (`strchr(pstr, ' ')`; tabs do
/// not separate) and then searches the *whole* remainder with `strstr` — the
/// modifiers are not positional suffixes and need no space between them
/// (C's own comment: `"QQCPPXMSITT"` is `pvlOptCPP|pvlOptMSI`). The process
/// class is resolved by [`ProcessClass`]'s fixed precedence; the
/// maximize-severity switch is a second, independent `else if` chain
/// (`:2375-2378`) in the order `NMS, MSI, MSS, MS`, longest match first.
///
/// Returns `(target, policy, monitor_switch, force_ca)` with `ftype`'s modifier
/// mask ([`LinkFieldType::mask`]) already applied. Shared by the plain-text path
/// and the `ca://` scheme path so `ca://PV MS` parses its `MS` instead of
/// folding it into the PV name.
fn split_link_modifiers(
    s: &str,
    ftype: LinkFieldType,
) -> (&str, LinkProcessPolicy, MonitorSwitch, bool) {
    let Some((target, mods)) = s.split_once(' ') else {
        // No space ⇒ no modifier text at all ⇒ modifiers stay zeroed (NPP),
        // and every mask leaves zero at zero.
        let (policy, force_ca) = ProcessClass::default().resolve();
        return (s, policy, MonitorSwitch::NoMaximize, force_ca);
    };

    // Process class — one assignment, C's fixed chain order.
    let class = ProcessClass::from_modifier_text(mods);

    // Maximize-severity switch — independent chain, longest match first.
    let ms = if mods.contains("NMS") {
        MonitorSwitch::NoMaximize
    } else if mods.contains("MSI") {
        MonitorSwitch::MaximizeIfInvalid
    } else if mods.contains("MSS") {
        MonitorSwitch::MaximizeStatus
    } else if mods.contains("MS") {
        MonitorSwitch::Maximize
    } else {
        MonitorSwitch::NoMaximize
    };

    // C filters the parsed modifiers by the holding field's type before the
    // link is ever built (`dbStaticLib.c:2380-2391`), so the mask belongs here
    // — at the single parse owner — not at each consumer.
    let (class, ms) = ftype.mask(class, ms);
    let (policy, force_ca) = class.resolve();
    (target, policy, ms, force_ca)
}

/// Parse a `DBF_INLINK` (`INP`, `INPA`…, `DOL`, `SDIS`, `TSEL`, `SIML`, …).
/// Every modifier is accepted — see [`parse_link_field`].
pub fn parse_link_v2(s: &str) -> ParsedLink {
    parse_link_field(s, LinkFieldType::In)
}

/// Parse a link string held by a link field of type `ftype`, applying that
/// type's C modifier mask (`dbStaticLib.c:2380-2391`). The single owner of
/// "link text → [`ParsedLink`]"; [`parse_link_v2`],
/// [`parse_output_link_v2`] and [`parse_forward_link_v2`] are its three
/// field-type entry points.
pub fn parse_link_field(s: &str, ftype: LinkFieldType) -> ParsedLink {
    let s = s.trim();
    // JSON-style links (epics-base PR #86) — try first so a leading
    // `{` is not mistaken for a leading-special record-name warning.
    if let Some(parsed) = try_parse_json_link(s) {
        return parsed;
    }
    // Hardware link (epics-base PR #213). `@` starts INST_IO; `#`
    // starts VME_IO. Everything else falls through to legacy parsing.
    if let Some(parsed) = try_parse_hw_link(s) {
        return parsed;
    }
    if s.is_empty() {
        return ParsedLink::None;
    }

    // CA/PVA protocol links. a `ca://PV MS` link carries
    // the same trailing maximize-severity modifiers as the legacy form,
    // so strip them off the scheme body before storing the PV name —
    // otherwise `MS` would be folded into the PV name (`"PV MS"`). The
    // parsed `MS`/`NMS`/`MSI`/`MSS` switch rides in the `CaLink` so the
    // alarm gate is applied at the record-processing boundary.
    if let Some(rest) = s.strip_prefix("ca://") {
        let (pv, policy, ms, _force_ca) = split_link_modifiers(rest, ftype);
        return ParsedLink::Ca(CaLink {
            pv: pv.to_string(),
            monitor_switch: ms,
            // Carry the parsed CP/CPP policy so `ca://OTHER:PV CPP`
            // drives holder processing;
            // pre-fix it was discarded into a bare latest-value link.
            policy,
        });
    }
    if let Some(rest) = s.strip_prefix("pva://") {
        return ParsedLink::Pva(rest.to_string());
    }

    // There is NO quoted-string constant. C's `dbParseLink` has exactly three
    // tests — braces => JSON, `epicsParseDouble` => CONSTANT, otherwise PV link
    // (`dbStaticLib.c:2280-2356`) — and a quote is not a number, so link text
    // that still carries quotes after the .db lexer is a PV NAME with quotes in
    // it. softIoc (EPICS 7.0.10), `dbpr X 2` after `iocInit`:
    //
    // ```text
    // record(ai,"QA"){field(INP,"\"hello\"")}        INP: CA_LINK "hello" NPP NMS
    // record(stringin,"QS"){field(INP,"\"hello\"")}  INP: CA_LINK "hello" NPP NMS
    // record(lso,"LQ"){field(DOL,"\"hello\"")}       DOL: CA_LINK "hello" NPP NMS
    // record(ai,"QE"){field(INP,"\"\"")}             INP: CA_LINK "" NPP NMS
    // record(ai,"QN"){field(INP,"")}                 INP: CONSTANT
    // ```
    //
    // — a never-connected CA link, so the record stays UDF=1. Only the EMPTY
    // text is an unset CONSTANT link. The string constant a long-string record
    // takes is the JSON `{const:"hello"}` handled above ([`load_link_ls`]).

    // Scalar numeric constant. C `dbParseLink` (`dbStaticLib.c:2346-2349`):
    //
    //     /* Link is a constant if empty or it holds just a number */
    //     if (len == 0 || epicsParseDouble(pstr, &value, NULL) == 0) {
    //         pinfo->ltype = CONSTANT;
    //         return 0;
    //     }
    //
    // The test runs on the WHOLE stripped string, *before* the modifier scan,
    // and `epicsParseDouble` with a NULL `units` argument rejects any trailing
    // text (`S_stdlib_extraneous`, `epicsStdlib.c`). So `"5 PP"` is NOT a
    // constant in C — it is a PV_LINK to a record named `5` with the `PP`
    // modifier (verified on softIoc: `INPA: CA_LINK 5 PP NMS`). Splitting the
    // modifiers off first and then testing the head for a number turns every
    // `"<number> <modifier>"` link into a constant: `SDIS="3 NPP"` with
    // `DISV=3` would disable the record forever, and `OUT="5 PP"` would drop
    // the write instead of alarming. Keep this test above
    // `split_link_modifiers`.
    if parse_c_double(s).is_some() {
        return ParsedLink::Constant(s.to_string());
    }

    // Array constant. C `dbParseLink` (`dbStaticLib.c:2354-2357`), right after
    // the scalar-constant test:
    //
    //     /* Link may be an array constant */
    //     if (pstr[0] == '[' && pstr[len-1] == ']') {
    //         pinfo->ltype = CONSTANT;
    //         return 0;
    //     }
    //
    // The bracketed text is handed verbatim to `dbPutConvertJSON`
    // (`dbConstLink.c::dbConstLoadArray`) when the record loads the link at
    // init. Resolved BEFORE the modifier split for the same reason as the
    // quoted constant: `[1, 2, 3]` legitimately contains spaces, and C reaches
    // the modifier scan only after both constant tests. Note the one form that
    // is NOT a constant: `"1 2 3"` — `epicsParseDouble` rejects the trailing
    // garbage, so C parses it as a PV_LINK to a record named `1`.
    if s.starts_with('[') && s.ends_with(']') && s.len() >= 2 {
        return ParsedLink::Constant(s.to_string());
    }

    // Isolate the target from its modifiers — see [`split_link_modifiers`].
    let (link_part, policy, ms, force_ca) = split_link_modifiers(s, ftype);

    // A `CA` modifier forces the link to be a CA link (C
    // `dbStaticLib.c:2372` — `pinfo->modifiers = pvlOptCA`, and
    // `dbAccess.c:1104` routes a link with any of `CA|CP|CPP` away from
    // `dbDbInitLink`). `dbParseLink` reaches the modifier scan only after the
    // constant test (`:2347`) has failed, so a CA-forced link is always a
    // `PV_LINK` — never a Constant or local Db link.
    if force_ca {
        // `CA` is a *process class*, so it is mutually exclusive with
        // `CP`/`CPP` and the class chain has already resolved the policy to
        // `NoProcess`: `"OTHER:PV CP CA"` matches "CA" before "CP" in C and
        // yields `pvlOptCA` alone — a plain CA link that never processes its
        // holder. The maximize-severity switch is a *separate* chain and does
        // ride along (`REC.VAL CA MS` keeps its MS gate).
        return ParsedLink::Ca(CaLink {
            pv: link_part.to_string(),
            monitor_switch: ms,
            policy,
        });
    }

    // DB link: split record from field exactly as C `dbNameToAddr`
    // (`dbAccess.c:667-671`) does — see [`split_record_field`].
    if let Some((rec, field)) = split_record_field(link_part) {
        return ParsedLink::Db(DbLink {
            record: rec.to_string(),
            field,
            policy,
            monitor_switch: ms,
        });
    }

    // No dot, or a remainder that is not a field name → DB link with the
    // default `VAL` field (C `dbFindFieldPart`'s "absent field name" branch,
    // `dbStaticLib.c:1802-1811`, which resolves to `pvalFldDes`).
    ParsedLink::Db(DbLink {
        record: link_part.to_string(),
        field: "VAL".to_string(),
        policy,
        monitor_switch: ms,
    })
}

/// Split a link target into `(record, FIELD)`, mirroring C `dbNameToAddr`
/// (`dbAccess.c:667-671`).
///
/// C terminates the *record* name at the **first** `.`
/// (`dbStaticLib.c:1548-1571` `dbFindRecordPart`: `strchr(pname, '.')`), then
/// matches the remainder against the record type's field table
/// (`dbStaticLib.c:1781-1846` `dbFindFieldPart`). That lexer accepts a field
/// name that is a C identifier — `[A-Za-z_][A-Za-z0-9_]*` — and imposes no
/// character-class or length restriction beyond it.
///
/// The port has no field table at parse time, so the identifier rule is the
/// guard. It matters that digits and `_` are accepted and that the 4-character
/// cap is gone: `mbboDirect` has `B0`…`BF`, `sseq` has `DO1`…`LNKA`, and
/// dbCommon has `OLDSIMM`/`NAMSG`. Pre-fix `'0'.is_ascii_uppercase()` was
/// `false`, so `INP="DIRECT.B0"` fell through to a link to a *record* literally
/// named `"DIRECT.B0"` — which never resolves locally, and
/// `classify_cp_link`'s `convert_to_ca` then re-routed it as an external CA
/// channel, losing lock-set atomicity, `PP` target processing and `MS`
/// severity inheritance.
///
/// Returns `None` when there is no `.` or the remainder is not a field name,
/// which is C's "absent field name" case (the link addresses `VAL`). Keeping
/// the whole string as the record name in that case is also what preserves a
/// dotted *remote* PV name (`OTHER:PV.1:X`) for the CA-link fallback, which
/// reconstructs the channel name from `record`/`field`.
fn split_record_field(link_part: &str) -> Option<(&str, String)> {
    let (rec, field) = link_part.split_once('.')?;
    let mut chars = field.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((rec, field.to_ascii_uppercase()))
}

/// Parse a `DBF_OUTLINK` (record `OUT`, `dfanout.OUTA`…, `sseq.LNK1`…,
/// `SIOL`).
///
/// C `dbParseLink` **discards** `CP`/`CPP` on a `DBF_OUTLINK` with a startup
/// warning (`dbStaticLib.c:2382-2387`): those modifiers ask for input-side
/// "process the holder when the source changes", which is meaningless on an
/// output. Honouring them opens a monitor on the *target* and reprocesses the
/// *holder* on every target change — a processing loop (holder writes TARGET →
/// TARGET change fires CP → holder reprocesses → writes TARGET …) that cannot
/// happen on a C IOC.
///
/// A modifier-less link is NPP (`NoProcess`) in both directions — C zeroes the
/// modifier set first (`:2252`). The OUT-link target-processing decision —
/// process when the link is explicit ` PP` **or** the destination field is
/// `.PROC` — lives in the write path
/// (`PvDatabase::write_db_link_value`), matching C
/// `dbDbPutValue` (`dbDbLink.c:387-390`).
pub fn parse_output_link_v2(s: &str) -> ParsedLink {
    parse_link_field(s, LinkFieldType::Out)
}

/// Parse a `DBF_FWDLINK` (`FLNK` — `dbCommon.dbd.pod:575`; `fanout.LNK1`… —
/// `fanoutRecord.dbd.pod:144`).
///
/// C masks the modifier set to `pvlOptCA` alone (`dbStaticLib.c:2390`): a
/// forward link only triggers processing, so every process-class modifier
/// other than `CA` and every maximize-severity modifier is dropped. `CA` is
/// kept because it still decides whether the trigger crosses IOCs.
pub fn parse_forward_link_v2(s: &str) -> ParsedLink {
    parse_link_field(s, LinkFieldType::Fwd)
}

/// Determine the [`LinkType`] of a record's string link field directly
/// from its raw text — the convenience API a record's `process()` or
/// its device support uses to discriminate one of its `INP` / `OUTL` /
/// `TRIG` link fields without having to match the whole [`ParsedLink`]
/// enum.
///
/// This is the framework's answer to C device support reading
/// `prec->inp.type` (`devEpidSoft.c:110`,
/// `devEpidSoftCallback.c:116`): the existing string link fields are
/// kept as-is, and this query is layered on top.
pub fn link_field_type(s: &str) -> LinkType {
    parse_link_v2(s).link_type()
}

/// Parse a link string into a LinkAddress (legacy wrapper around parse_link_v2).
/// Formats: "REC.FIELD", "REC", "REC.FIELD PP", "REC.FIELD NPP", "" → None
pub fn parse_link(s: &str) -> Option<LinkAddress> {
    match parse_link_v2(s) {
        ParsedLink::Db(db) => Some(LinkAddress {
            record: db.record,
            field: db.field,
            policy: db.policy,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod json_link_tests {
    //! epics-base PR #86 — JSON-style inline link options.
    use super::*;

    #[test]
    fn json_const_numeric() {
        assert_eq!(
            parse_link_v2("{const: 1.5}"),
            ParsedLink::Constant("1.5".to_string())
        );
    }

    #[test]
    fn json_const_quoted_string() {
        assert_eq!(
            parse_link_v2(r#"{const: "hello"}"#),
            ParsedLink::Constant("hello".to_string())
        );
    }

    #[test]
    fn json_const_empty_is_none() {
        // `{const: ""}` matches base's empty-link convention.
        assert_eq!(parse_link_v2(r#"{const: ""}"#), ParsedLink::None);
    }

    #[test]
    fn json_ca_link() {
        assert_eq!(
            parse_link_v2(r#"{ca: { pv: "FOO" }}"#),
            ParsedLink::Ca(CaLink::new("FOO"))
        );
    }

    #[test]
    fn json_pva_link() {
        assert_eq!(
            parse_link_v2(r#"{pva: { pv: "FOO:bar" }}"#),
            ParsedLink::Pva("FOO:bar".to_string())
        );
    }

    #[test]
    fn json_ca_link_unquoted_key() {
        assert_eq!(
            parse_link_v2(r#"{ca: { pv: 'BAR' }}"#),
            ParsedLink::Ca(CaLink::new("BAR"))
        );
    }

    // pvxs string shorthand `{ pva: "PV" }` / `{ ca: "PV" }`
    // (pvalink_jlif.cpp:24-31, :143-149). Before the fix these fell through
    // to legacy DB parsing because the value is a string, not a `{pv:...}`
    // sub-object — the link silently became a DB link to a record whose name
    // was the raw JSON text.

    #[test]
    fn json_pva_link_string_shorthand() {
        assert_eq!(
            parse_link_v2(r#"{pva: "TARGET:AI"}"#),
            ParsedLink::Pva("TARGET:AI".to_string())
        );
    }

    #[test]
    fn json_ca_link_string_shorthand() {
        assert_eq!(
            parse_link_v2(r#"{ca: "TARGET:AI"}"#),
            ParsedLink::Ca(CaLink::new("TARGET:AI"))
        );
    }

    #[test]
    fn json_pva_link_string_shorthand_single_quotes() {
        assert_eq!(
            parse_link_v2(r#"{pva: 'invalid:pv:name'}"#),
            ParsedLink::Pva("invalid:pv:name".to_string())
        );
    }

    #[test]
    fn json_pva_link_longhand_preserves_options_structurally() {
        // The longhand map form keeps a `pv` member plus its options as
        // STRUCTURED JLink pairs (not a `?Q=4` query) so the consumer
        // reconstructs PvaLinkConfig from map keys, matching pvxs.
        assert_eq!(
            parse_link_v2(r#"{pva: { pv: "FOO:bar", Q: "4" }}"#),
            ParsedLink::PvaJson(PvaJsonLink {
                pv: "FOO:bar".to_string(),
                // `Q: "4"` is QUOTED, so its kind is a JSON string, not an
                // integer — the kind survives parsing (pvxs would later
                // ignore a string `Q`, which only accepts an integer).
                options: vec![("Q".to_string(), JlinkValue::Str("4".to_string()))],
            })
        );
    }

    /// The longhand parser preserves each option's JSON value KIND, the
    /// distinction pvxs dispatches its pvalink callbacks on
    /// (pvalink_jlif.cpp:286-300): a bare `true`/`false` is a boolean, a
    /// bare integer is an integer, a quoted token is a string. A boolean
    /// and a string spelling of the "same" value are NOT collapsed.
    #[test]
    fn json_pva_link_longhand_preserves_value_kind() {
        let link = parse_link_v2(
            r#"{pva: {pv: "X:AI", pipeline: true, retry: false, proc: "CP", Q: 4, sevr: true}}"#,
        );
        let j = match link {
            ParsedLink::PvaJson(j) => j,
            other => panic!("expected PvaJson, got {other:?}"),
        };
        assert_eq!(
            j.options,
            vec![
                ("pipeline".to_string(), JlinkValue::Bool(true)),
                ("retry".to_string(), JlinkValue::Bool(false)),
                ("proc".to_string(), JlinkValue::Str("CP".to_string())),
                ("Q".to_string(), JlinkValue::Int(4)),
                // `sevr: true` is a BOOLEAN, not the string "true".
                ("sevr".to_string(), JlinkValue::Bool(true)),
            ]
        );
    }

    /// pvxs installs root JLink callbacks only for the JSON string (channel
    /// shorthand) and map (longhand) cases; `pva_parse_null`/`bool`/`integer`
    /// ignore root-depth values (pvalink_jlif.cpp:74-100,143-154). A bare
    /// `true`/`5`/`null`/`[..]` root therefore installs no channel name and
    /// must NOT become an external PVA/CA link to a literal token. The old
    /// `value.starts_with('{')` heuristic accepted every non-`{` token as a
    /// PV name after trimming quotes, dialing channels named `"true"`/`"5"`/
    /// `"null"`/`"[1,2]"`.
    #[test]
    fn json_pva_link_root_nonstring_rejected() {
        // Accepted roots: string shorthand and object longhand.
        assert_eq!(
            parse_link_v2(r#"{pva: "TARGET"}"#),
            ParsedLink::Pva("TARGET".to_string())
        );
        assert_eq!(
            parse_link_v2(r#"{pva: { pv: "TARGET" }}"#),
            ParsedLink::Pva("TARGET".to_string())
        );

        // Rejected roots: a non-string, non-object value must never produce
        // a PVA link (it falls through to legacy parsing instead).
        for src in [
            r#"{pva: true}"#,
            r#"{pva: false}"#,
            r#"{pva: 5}"#,
            r#"{pva: null}"#,
            r#"{pva: [1,2]}"#,
        ] {
            assert!(
                !matches!(
                    parse_link_v2(src),
                    ParsedLink::Pva(_) | ParsedLink::PvaJson(_)
                ),
                "non-string pva root must not become a PVA link: {src}"
            );
        }

        // Same defect family on the `ca` key: a bare token must not become a
        // CA link to a literal `"true"`/`"5"` channel name.
        for src in [
            r#"{ca: true}"#,
            r#"{ca: 5}"#,
            r#"{ca: null}"#,
            r#"{ca: [1,2]}"#,
        ] {
            assert!(
                !matches!(parse_link_v2(src), ParsedLink::Ca(_)),
                "non-string ca root must not become a CA link: {src}"
            );
        }
    }

    // epics-base PR #213 — hardware-link parsing.

    #[test]
    fn hw_link_inst_io() {
        let parsed = parse_link_v2("@simDriver 0 INPUT");
        match parsed {
            ParsedLink::Hw(hw) => {
                assert_eq!(hw.kind, HwLinkKind::InstIo);
                assert_eq!(hw.args, vec!["simDriver", "0", "INPUT"]);
                assert_eq!(hw.raw, "simDriver 0 INPUT");
            }
            other => panic!("expected Hw, got {other:?}"),
        }
    }

    #[test]
    fn hw_link_inst_io_with_hex() {
        // PR #213 specifically: hex literals in HW-link args must
        // survive tokenization intact.
        let parsed = parse_link_v2("@dev 0xFF mask=0x1A");
        match parsed {
            ParsedLink::Hw(hw) => {
                assert_eq!(hw.kind, HwLinkKind::InstIo);
                assert_eq!(hw.args, vec!["dev", "0xFF", "mask=0x1A"]);
            }
            other => panic!("expected Hw, got {other:?}"),
        }
    }

    #[test]
    fn hw_link_vme_io() {
        let parsed = parse_link_v2("#C0 S2");
        match parsed {
            ParsedLink::Hw(hw) => {
                assert_eq!(hw.kind, HwLinkKind::VmeIo);
                assert_eq!(hw.args, vec!["C0", "S2"]);
            }
            other => panic!("expected Hw, got {other:?}"),
        }
    }

    #[test]
    fn hw_link_inst_io_empty_args() {
        // `@` alone — kind set, args empty, raw empty.
        let parsed = parse_link_v2("@");
        match parsed {
            ParsedLink::Hw(hw) => {
                assert_eq!(hw.kind, HwLinkKind::InstIo);
                assert!(hw.args.is_empty());
                assert!(hw.raw.is_empty());
            }
            other => panic!("expected Hw, got {other:?}"),
        }
    }

    // Link-type discrimination — C `link.h` CONSTANT / DB_LINK /
    // CA_LINK (`dbStatic/link.h:28-39`).

    #[test]
    fn link_type_constant_numeric() {
        assert_eq!(link_field_type("3.14"), LinkType::Constant);
        assert_eq!(link_field_type("{const: 7}"), LinkType::Constant);
    }

    /// A quoted string is NOT a constant: C's `dbParseLink` tests braces then
    /// `epicsParseDouble`, and a quote is neither — softIoc makes
    /// `field(INP,"\"hello\"")` a `CA_LINK "hello" NPP NMS`, and even
    /// `field(INP,"\"\"")` a `CA_LINK ""`. Only the empty text is an unset
    /// CONSTANT link.
    #[test]
    fn link_type_quoted_string_is_a_pv_link() {
        assert_eq!(link_field_type(r#""hello""#), LinkType::Db);
        assert_eq!(link_field_type(r#""""#), LinkType::Db);
    }

    #[test]
    fn link_type_empty_is_empty() {
        assert_eq!(link_field_type(""), LinkType::Empty);
        assert_eq!(link_field_type("   "), LinkType::Empty);
    }

    #[test]
    fn link_type_db_link() {
        assert_eq!(link_field_type("REC.VAL"), LinkType::Db);
        assert_eq!(link_field_type("REC"), LinkType::Db);
        assert_eq!(link_field_type("REC.VAL PP"), LinkType::Db);
    }

    #[test]
    fn link_type_ca_link() {
        assert_eq!(link_field_type("ca://REMOTE:PV"), LinkType::Ca);
        assert_eq!(link_field_type("pva://REMOTE:PV"), LinkType::Ca);
        assert_eq!(link_field_type(r#"{ca: { pv: "REMOTE" }}"#), LinkType::Ca);
    }

    #[test]
    fn link_type_hw_and_calc_are_other() {
        assert_eq!(link_field_type("@dev 0 IN"), LinkType::Other);
        assert_eq!(link_field_type("#C0 S2"), LinkType::Other);
        // calc link uses strict JSON (quoted keys) — serde_json parse.
        assert_eq!(
            link_field_type(r#"{calc: {"expr": "A+1", "args": ["pv1"]}}"#),
            LinkType::Other
        );
    }

    // Bare ` CA` modifier — C `dbStaticLib.c:2372` forces a
    // `pv_link` to a CA link (`link.h` `CA_LINK` 11).

    #[test]
    fn ca_modifier_classifies_as_ca() {
        // `REC.FIELD CA` must parse as a CA link carrying the
        // `record.field` PV name — NOT a Db link to field "FIELD CA".
        // No `MS`-class modifier → default NoMaximize.
        assert_eq!(
            parse_link_v2("REC.FIELD CA"),
            ParsedLink::Ca(CaLink::new("REC.FIELD"))
        );
        assert_eq!(link_field_type("REC.FIELD CA"), LinkType::Ca);
    }

    #[test]
    fn ca_modifier_bare_pv_name() {
        // No field suffix — `localPv CA` is still a CA link.
        assert_eq!(
            parse_link_v2("localPv CA"),
            ParsedLink::Ca(CaLink::new("localPv"))
        );
        assert_eq!(link_field_type("localPv CA"), LinkType::Ca);
    }

    #[test]
    fn ca_modifier_combined_with_ms() {
        // The MS-class switch is an INDEPENDENT chain (`dbStaticLib.c:2375`),
        // so it rides along with the CA process class in any token order.
        assert_eq!(
            parse_link_v2("REC.VAL CA MS"),
            ParsedLink::Ca(CaLink {
                pv: "REC.VAL".to_string(),
                monitor_switch: MonitorSwitch::Maximize,
                policy: LinkProcessPolicy::NoProcess,
            })
        );
        // `CA NMS` carries the explicit NoMaximize switch.
        assert_eq!(
            parse_link_v2("REC.VAL CA NMS"),
            ParsedLink::Ca(CaLink {
                pv: "REC.VAL".to_string(),
                monitor_switch: MonitorSwitch::NoMaximize,
                policy: LinkProcessPolicy::NoProcess,
            })
        );
        assert_eq!(link_field_type("REC.VAL CA NMS"), LinkType::Ca);
    }

    // R6-3 — the process class is ONE `else if` chain over the whole modifier
    // string (`dbStaticLib.c:2369-2373`), in the order NPP, CPP, PP, CA, CP,
    // ASSIGNING exactly one bit. The chain order decides, not the token order:
    // the pre-fix parser stripped ordered suffixes (last one wins) and let a
    // ` CA` flag coexist with a CP/PP policy.
    #[test]
    fn process_class_precedence_is_chain_order_not_token_order() {
        // "REC CP NPP" → NPP wins (modifiers = 0): no CP subscription. The
        // pre-fix suffix loop stripped " NPP" then " CP" and ended at CP,
        // registering the holder in the CP trigger registry.
        assert_eq!(
            parse_link_v2("REC CP NPP"),
            ParsedLink::Db(DbLink {
                record: "REC".to_string(),
                field: "VAL".to_string(),
                policy: LinkProcessPolicy::NoProcess,
                monitor_switch: MonitorSwitch::NoMaximize,
            })
        );
        // "OTHER:PV CP CA" → CA is matched BEFORE CP, so it is pvlOptCA alone:
        // a plain CA link whose holder never processes. Pre-fix this was a Ca
        // link with policy=ChannelProcess, reprocessing the holder on every
        // remote monitor update.
        assert_eq!(
            parse_link_v2("OTHER:PV CP CA"),
            ParsedLink::Ca(CaLink {
                pv: "OTHER:PV".to_string(),
                monitor_switch: MonitorSwitch::NoMaximize,
                policy: LinkProcessPolicy::NoProcess,
            })
        );
        // "REC PP CA" → PP is matched BEFORE CA, so no CA|CP|CPP bit is set
        // and dbAccess.c:1104 routes to dbDbInitLink: a LOCAL DB link with PP.
        // Pre-fix this became an external CA channel.
        assert_eq!(
            parse_link_v2("REC PP CA"),
            ParsedLink::Db(DbLink {
                record: "REC".to_string(),
                field: "VAL".to_string(),
                policy: LinkProcessPolicy::ProcessPassive,
                monitor_switch: MonitorSwitch::NoMaximize,
            })
        );
        // CPP is matched before PP, and CP only after CA.
        assert!(matches!(
            parse_link_v2("REC CPP"),
            ParsedLink::Db(DbLink {
                policy: LinkProcessPolicy::ChannelProcessPassive,
                ..
            })
        ));
        assert!(matches!(
            parse_link_v2("REC CP"),
            ParsedLink::Db(DbLink {
                policy: LinkProcessPolicy::ChannelProcess,
                ..
            })
        ));
        // Exactly one class: a modifier-less link is NPP.
        assert!(matches!(
            parse_link_v2("REC"),
            ParsedLink::Db(DbLink {
                policy: LinkProcessPolicy::NoProcess,
                ..
            })
        ));
    }

    // C searches the modifier text with `strstr`, so the tokens need no space
    // between them — its own comment pins `"QQCPPXMSITT"` as CPP|MSI. The
    // pre-fix suffix loop required a leading space and an exact tail match, so
    // it saw no modifiers at all here.
    #[test]
    fn modifiers_are_substring_matched_not_space_delimited() {
        assert_eq!(
            parse_link_v2("REC QQCPPXMSITT"),
            ParsedLink::Db(DbLink {
                record: "REC".to_string(),
                field: "VAL".to_string(),
                policy: LinkProcessPolicy::ChannelProcessPassive,
                monitor_switch: MonitorSwitch::MaximizeIfInvalid,
            })
        );
    }

    // The MS chain is longest-match-first too: NMS, MSI, MSS, MS.
    #[test]
    fn maximize_switch_precedence() {
        for (link, expect) in [
            ("REC NMS", MonitorSwitch::NoMaximize),
            ("REC MSI", MonitorSwitch::MaximizeIfInvalid),
            ("REC MSS", MonitorSwitch::MaximizeStatus),
            ("REC MS", MonitorSwitch::Maximize),
            // NMS is matched first even though the string also contains "MS".
            ("REC MS NMS", MonitorSwitch::NoMaximize),
        ] {
            match parse_link_v2(link) {
                ParsedLink::Db(db) => assert_eq!(db.monitor_switch, expect, "link {link}"),
                other => panic!("link {link}: expected Db, got {other:?}"),
            }
        }
    }

    #[test]
    fn ca_scheme_link_parses_ms_modifier() {
        // `ca://PV MS` must strip the modifier off the
        // scheme body — pre-fix the PV name became `"PV MS"`.
        assert_eq!(
            parse_link_v2("ca://SR:DCCT MS"),
            ParsedLink::Ca(CaLink {
                pv: "SR:DCCT".to_string(),
                monitor_switch: MonitorSwitch::Maximize,
                policy: LinkProcessPolicy::NoProcess,
            })
        );
        assert_eq!(
            parse_link_v2("ca://SR:DCCT MSI"),
            ParsedLink::Ca(CaLink {
                pv: "SR:DCCT".to_string(),
                monitor_switch: MonitorSwitch::MaximizeIfInvalid,
                policy: LinkProcessPolicy::NoProcess,
            })
        );
        assert_eq!(
            parse_link_v2("ca://SR:DCCT MSS"),
            ParsedLink::Ca(CaLink {
                pv: "SR:DCCT".to_string(),
                monitor_switch: MonitorSwitch::MaximizeStatus,
                policy: LinkProcessPolicy::NoProcess,
            })
        );
        // Bare `ca://PV` → default NoMaximize, PV name intact.
        assert_eq!(
            parse_link_v2("ca://SR:DCCT"),
            ParsedLink::Ca(CaLink::new("SR:DCCT"))
        );
    }

    #[test]
    fn ca_modifier_does_not_affect_plain_db_link() {
        // A link with no ` CA` modifier stays a Db link — the fix
        // must not over-trigger on record names that merely contain
        // the letters "ca".
        assert_eq!(link_field_type("camera.VAL"), LinkType::Db);
        assert_eq!(link_field_type("REC.VAL PP"), LinkType::Db);
    }

    /// JSON pvalink options survive parse_link_v2 as STRUCTURED JLink
    /// members (pv + ordered (key,value) pairs), not as a `?key=value`
    /// URI query — pvxs has no query parser (pvalink_jlif.cpp:286-300);
    /// options are JLink map keys (:69-196).
    #[test]
    fn br_r10_json_pva_options_preserved_in_parsed_link() {
        // All options present: field, proc (CPP), sevr (MS), Q.
        let link = parse_link_v2(
            r#"{pva: {pv: "TARGET:AI", field: "display.precision", proc: "CPP", sevr: "MS", Q: 8}}"#,
        );
        let j = match link {
            ParsedLink::PvaJson(j) => j,
            other => panic!("expected PvaJson, got {other:?}"),
        };
        assert_eq!(j.pv, "TARGET:AI", "pv must be the bare channel name");
        // No re-encoded query syntax anywhere in the channel name.
        assert!(
            !j.pv.contains('?'),
            "pv must not carry a `?` query: {}",
            j.pv
        );
        // Options preserved in source order with original key case.
        assert_eq!(
            j.options,
            vec![
                (
                    "field".to_string(),
                    JlinkValue::Str("display.precision".to_string())
                ),
                ("proc".to_string(), JlinkValue::Str("CPP".to_string())),
                ("sevr".to_string(), JlinkValue::Str("MS".to_string())),
                // `Q: 8` is a BARE integer — its kind is preserved as Int.
                ("Q".to_string(), JlinkValue::Int(8)),
            ]
        );
    }

    /// A PVA string shorthand keeps the channel name verbatim — a `?` in
    /// it is link DATA, not option syntax (pvxs pva_parse_string,
    /// pvalink_jlif.cpp:143-149). It must NOT become a PvaJson with
    /// parsed options.
    #[test]
    fn json_pva_string_shorthand_keeps_query_chars_verbatim() {
        assert_eq!(
            parse_link_v2(r#"{pva: "TARGET:AI?field=x"}"#),
            ParsedLink::Pva("TARGET:AI?field=x".to_string())
        );
    }

    /// The `pva://` scheme form is likewise a verbatim channel name; a
    /// `?` is not split out as options.
    #[test]
    fn pva_scheme_keeps_query_chars_verbatim() {
        assert_eq!(
            parse_link_v2("pva://TARGET:AI?field=x"),
            ParsedLink::Pva("TARGET:AI?field=x".to_string())
        );
    }

    /// `external_pv_name` returns a PvaJson link's per-link identity key
    /// (pv + canonical options), not the bare PV — so two same-PV links
    /// that differ by options resolve to distinct configs.
    #[test]
    fn pva_json_external_pv_name() {
        let link = parse_link_v2(r#"{pva: {pv: "TARGET:AI", proc: "CP"}}"#);
        let key = link.external_pv_name().expect("PvaJson carries a key");
        assert_eq!(key.as_ref(), "TARGET:AI\u{1f}proc=s:CP");
        assert_eq!(link.link_type(), LinkType::Ca);
        assert!(link.is_writable_out_link());
    }

    /// Two structured links to the SAME PV differing only by options get
    /// DISTINCT identity keys (the per-link cache key); the bare PV is
    /// the prefix up to the first separator so the resolver can still
    /// recover the shared channel identity.
    #[test]
    fn pva_json_identity_key_separates_same_pv_links() {
        let a = match parse_link_v2(r#"{pva: {pv: "SRC", field: "value", Q: 64}}"#) {
            ParsedLink::PvaJson(j) => j,
            other => panic!("expected PvaJson, got {other:?}"),
        };
        let b = match parse_link_v2(r#"{pva: {pv: "SRC", field: "alarm.severity", Q: 1}}"#) {
            ParsedLink::PvaJson(j) => j,
            other => panic!("expected PvaJson, got {other:?}"),
        };
        assert_ne!(
            a.link_identity_key(),
            b.link_identity_key(),
            "same-PV links with different options must not collide"
        );
        // Both keys start with the bare PV up to the first separator.
        assert_eq!(a.link_identity_key().split('\u{1f}').next(), Some("SRC"));
        assert_eq!(b.link_identity_key().split('\u{1f}').next(), Some("SRC"));
        // Option order is canonicalized: the same options in a different
        // source order yield the same key.
        let c = match parse_link_v2(r#"{pva: {pv: "SRC", Q: 64, field: "value"}}"#) {
            ParsedLink::PvaJson(j) => j,
            other => panic!("expected PvaJson, got {other:?}"),
        };
        assert_eq!(a.link_identity_key(), c.link_identity_key());
    }

    /// Bare pvalink JSON with no extra options is unchanged.
    #[test]
    fn br_r10_json_pva_bare_pv_unchanged() {
        assert_eq!(
            parse_link_v2(r#"{pva: { pv: "FOO:bar" }}"#),
            ParsedLink::Pva("FOO:bar".to_string())
        );
    }

    #[test]
    fn json_unknown_key_falls_through_to_legacy() {
        // Unknown JSON top-level key must NOT be hijacked — leave it
        // for legacy parsing (which will likely produce None or a
        // weird Db link, but not crash).
        let result = parse_link_v2("{unknown: 42}");
        // Not Constant("42"), not Ca/Pva — must be one of the
        // legacy fall-through outcomes.
        assert!(matches!(
            result,
            ParsedLink::None | ParsedLink::Db(_) | ParsedLink::Constant(_)
        ));
    }

    /// BUG 2 — a bare OUT link defaults to NPP (`NoProcess`). C
    /// `dbDbPutValue` (dbDbLink.c:386-389) processes the target only
    /// on an explicit `PP` flag.
    #[test]
    fn parse_output_link_bare_is_noprocess() {
        match parse_output_link_v2("TARGET.VAL") {
            ParsedLink::Db(db) => {
                assert_eq!(db.policy, LinkProcessPolicy::NoProcess);
                assert_eq!(db.record, "TARGET");
                assert_eq!(db.field, "VAL");
            }
            other => panic!("expected Db link, got {other:?}"),
        }
    }

    /// BUG 2 — an explicit ` PP` on an OUT link keeps `ProcessPassive`.
    #[test]
    fn parse_output_link_explicit_pp_processes() {
        match parse_output_link_v2("TARGET.VAL PP") {
            ParsedLink::Db(db) => {
                assert_eq!(db.policy, LinkProcessPolicy::ProcessPassive);
            }
            other => panic!("expected Db link, got {other:?}"),
        }
    }

    /// A modifier-less OUT link to a `.PROC` field parses to the uniform
    /// `NoProcess` default like any other bare link; the target-process
    /// decision for `.PROC` lives in the write path
    /// (`write_db_link_value`, C `dbDbPutValue` dbDbLink.c:387-390), not
    /// in a parse-time policy. (Behaviour — that a `.PROC` write still
    /// processes the target — is covered by the database-level
    /// `*_proc_out_link_processes_target` test.)
    #[test]
    fn parse_output_link_proc_field_is_noprocess() {
        match parse_output_link_v2("TARGET.PROC") {
            ParsedLink::Db(db) => {
                assert_eq!(db.field, "PROC");
                assert_eq!(db.policy, LinkProcessPolicy::NoProcess);
            }
            other => panic!("expected Db link, got {other:?}"),
        }
    }

    /// A modifier-less **input** link defaults to NPP (`NoProcess`),
    /// matching C `dbParseLink` (`dbStaticLib.c:2252` memset→0;
    /// `pvlOptPP` set only on an explicit ` PP`). A bare INP must NOT
    /// cause `dbDbGetValue` (`dbDbLink.c:175`) to process the passive
    /// source on read.
    #[test]
    fn parse_input_link_bare_is_noprocess() {
        match parse_link_v2("SRC.VAL") {
            ParsedLink::Db(db) => {
                assert_eq!(db.policy, LinkProcessPolicy::NoProcess);
                assert_eq!(db.record, "SRC");
                assert_eq!(db.field, "VAL");
            }
            other => panic!("expected Db link, got {other:?}"),
        }
        // An explicit ` PP` input link still promotes to ProcessPassive.
        match parse_link_v2("SRC.VAL PP") {
            ParsedLink::Db(db) => assert_eq!(db.policy, LinkProcessPolicy::ProcessPassive),
            other => panic!("expected Db link, got {other:?}"),
        }
    }

    /// BUG 2 — an explicit ` NPP` OUT link is `NoProcess` (unchanged
    /// from `parse_link_v2`, but pinned here for completeness).
    #[test]
    fn parse_output_link_explicit_npp_is_noprocess() {
        match parse_output_link_v2("TARGET.VAL NPP") {
            ParsedLink::Db(db) => {
                assert_eq!(db.policy, LinkProcessPolicy::NoProcess);
            }
            other => panic!("expected Db link, got {other:?}"),
        }
    }

    /// CP and CPP must parse to distinct policies — collapsing CPP into
    /// `ChannelProcess` loses C's `precord->scan == 0` gate (`dbCa.c:994`).
    #[test]
    fn parse_cp_and_cpp_are_distinct_policies() {
        match parse_link_v2("SRC.VAL CP") {
            ParsedLink::Db(db) => assert_eq!(db.policy, LinkProcessPolicy::ChannelProcess),
            other => panic!("expected Db link for CP, got {other:?}"),
        }
        match parse_link_v2("SRC.VAL CPP") {
            ParsedLink::Db(db) => {
                assert_eq!(db.policy, LinkProcessPolicy::ChannelProcessPassive)
            }
            other => panic!("expected Db link for CPP, got {other:?}"),
        }
    }

    /// `cp_passive_only`: CP → `Some(false)`, CPP → `Some(true)`, others → None.
    #[test]
    fn cp_passive_only_maps_cp_and_cpp() {
        assert_eq!(
            LinkProcessPolicy::ChannelProcess.cp_passive_only(),
            Some(false)
        );
        assert_eq!(
            LinkProcessPolicy::ChannelProcessPassive.cp_passive_only(),
            Some(true)
        );
        assert_eq!(LinkProcessPolicy::ProcessPassive.cp_passive_only(), None);
        assert_eq!(LinkProcessPolicy::NoProcess.cp_passive_only(), None);
    }

    /// R6-4 — `DBF_OUTLINK` discards CP/CPP (`dbStaticLib.c:2382-2387`
    /// `modifiers &= ~(pvlOptCPP|pvlOptCP)`), so an OUT link can never report
    /// `cp_passive_only()` and can never be registered as a CP holder by
    /// `classify_cp_link`. The boundary that matters is the *combination* —
    /// C strips only the CP/CPP bit, leaving PP/CA/MS untouched.
    #[test]
    fn out_link_discards_cp_and_cpp_but_keeps_the_other_modifiers() {
        for text in ["TARGET.VAL CP", "TARGET.VAL CPP", "TARGET CP MS"] {
            match parse_output_link_v2(text) {
                ParsedLink::Db(db) => {
                    assert_eq!(
                        db.policy,
                        LinkProcessPolicy::NoProcess,
                        "{text} must lose its CP/CPP class"
                    );
                    assert_eq!(
                        db.policy.cp_passive_only(),
                        None,
                        "{text} must not be a CP holder"
                    );
                }
                other => panic!("expected Db link for {text}, got {other:?}"),
            }
        }
        // The MS bit survives the OUTLINK mask (only CP/CPP is cleared).
        match parse_output_link_v2("TARGET CP MS") {
            ParsedLink::Db(db) => assert_eq!(db.monitor_switch, MonitorSwitch::Maximize),
            other => panic!("expected Db link, got {other:?}"),
        }
        // `PP CP` — C's chain matches PP first, so there is no CP bit to strip
        // and the link stays a processing output link.
        match parse_output_link_v2("TARGET PP CP") {
            ParsedLink::Db(db) => assert_eq!(db.policy, LinkProcessPolicy::ProcessPassive),
            other => panic!("expected Db link, got {other:?}"),
        }
        // A ` CA` OUT link is still an external channel — CA is not masked.
        assert!(matches!(
            parse_output_link_v2("OTHER:PV CA"),
            ParsedLink::Ca(_)
        ));
    }

    /// The warning predicate must agree with what the parser actually discards:
    /// true exactly when the OUT mask downgrades a CP/CPP class.
    #[test]
    fn out_link_discards_cp_predicate_tracks_the_mask() {
        for text in ["TARGET CP", "TARGET CPP", "TARGET CP MS", "TARGET MSI CPP"] {
            assert!(out_link_discards_cp(text), "{text} loses its CP/CPP");
        }
        for text in [
            "TARGET",            // no modifiers at all
            "TARGET PP",         // PP wins the chain
            "TARGET CA",         // CA wins the chain
            "TARGET NPP",        // NPP wins the chain
            "TARGET PP CP",      // PP is matched before CP
            "TARGETCP",          // a name that merely ends in "CP" is not a modifier
            "\"CP\"",            // quoted constant, never modifier-scanned
            "{ca: {pv: \"X\"}}", // JSON link, never modifier-scanned
        ] {
            assert!(!out_link_discards_cp(text), "{text} discards nothing");
        }
    }

    /// R6-4 — `DBF_FWDLINK` masks the modifier set to `pvlOptCA` alone
    /// (`dbStaticLib.c:2390`): every process class other than CA, and every
    /// maximize-severity switch, is cleared.
    #[test]
    fn fwd_link_masks_everything_except_ca() {
        for text in ["NEXT PP", "NEXT CP", "NEXT CPP", "NEXT NPP"] {
            match parse_forward_link_v2(text) {
                ParsedLink::Db(db) => {
                    assert_eq!(db.policy, LinkProcessPolicy::NoProcess, "{text}");
                    assert_eq!(db.monitor_switch, MonitorSwitch::NoMaximize, "{text}");
                }
                other => panic!("expected Db link for {text}, got {other:?}"),
            }
        }
        // MS is masked off even without a process-class modifier.
        match parse_forward_link_v2("NEXT MS") {
            ParsedLink::Db(db) => assert_eq!(db.monitor_switch, MonitorSwitch::NoMaximize),
            other => panic!("expected Db link, got {other:?}"),
        }
        // CA is the one bit that survives — a forward link may still cross IOCs.
        match parse_forward_link_v2("OTHER:REC CA MS") {
            ParsedLink::Ca(ca) => {
                assert_eq!(ca.pv, "OTHER:REC");
                assert_eq!(ca.monitor_switch, MonitorSwitch::NoMaximize);
            }
            other => panic!("expected Ca link, got {other:?}"),
        }
    }
}
