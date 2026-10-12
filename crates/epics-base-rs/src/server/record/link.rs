use std::borrow::Cow;

use crate::error::{CaError, CaResult, CalcSaid, LinkRefusal, LinkWhy};
use crate::runtime::json_string::{decode_json_string, decode_json_string_token};
use crate::server::recgbl::ReportString;
use crate::types::{DbFieldType, DbfLinkClass, EpicsValue, PvString};

/// Link processing policy for input/output links.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkProcessPolicy {
    NoProcess,
    #[default]
    ProcessPassive,
    /// CP (`pvlOptCP`): subscribe to source; when source changes, process
    /// this record unconditionally (`dbCa.c:958-962` adds `CA_DBPROCESS`
    /// regardless of `precord->scan`).
    ChannelProcess,
    /// CPP (`pvlOptCPP`): like `ChannelProcess`, but on a source change
    /// process this record only when its `SCAN` is `Passive` — C gates the
    /// `CA_DBPROCESS` action on `precord->scan == 0` (`dbCa.c:825`, `:959`, `:1034`).
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

/// Which bus a hardware link names — C's `dbLinkInfo::ltype` for the `@` and
/// `#` forms, decided by `dbParseLink` (`dbStaticLib.c:2296-2331`) and by
/// nothing downstream.
///
/// One variant per bus C's identifier-letter table can produce, because the
/// bus is what the letters MEAN. Collapsing every `#` form into one kind left
/// each consumer to either re-derive the bus from the raw text or print a
/// default, and both happened: `dbpr` printed `VME_IO` for a CAMAC link while
/// `dbReportDeviceConfig` carried a second copy of the whole table.
///
/// There is deliberately no "unrecognised bus" variant. C answers an
/// unmatched identifier with `goto fail` (`:2331`), so such text is a parse
/// failure and never becomes a link at all — see `try_parse_hw_link`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwLinkKind {
    /// `@dev arg1 arg2 ...` — `INST_IO`. The most common form, used by
    /// asyn-based device support.
    InstIo,
    /// `#Cn Sn [@parm]` — `VME_IO`; C's `"CS"`. C/S = card/signal.
    VmeIo,
    /// `#Bn Cn Nn [An [Fn]] [@parm]` — `CAMAC_IO`; C's `"BCN"`, `"BCNA"`,
    /// `"BCNF"` and `"BCNAF"`.
    CamacIo,
    /// `#Rn Mn Dn En` — `RF_IO`; C's `"RMDE"`, and the one `#` form that must
    /// carry no `@parm` at all (`dbStaticLib.c:2333-2343`).
    RfIo,
    /// `#Ln An Cn Sn [@parm]` — `AB_IO`; C's `"LACS"`.
    AbIo,
    /// `#Ln An [@parm]` — `GPIB_IO`; C's `"LA"`.
    GpibIo,
    /// `#Ln Nn Pn Sn [@parm]` — `BITBUS_IO`; C's `"LNPS"`.
    BitbusIo,
    /// `#Ln Bn Gn [@parm]` — `BBGPIB_IO`; C's `"LBG"`.
    BbgpibIo,
    /// `#Vn Cn Sn [@parm]` or `#Vn Sn [@parm]` — `VXI_IO`; C's `"VCS"` and
    /// `"VS"`.
    VxiIo,
}

impl HwLinkKind {
    /// The `link.h` type `dbParseLink` sets for this bus — the single place
    /// the mapping lives, so `dbpr`'s type word and `dbCanSetLink`'s
    /// compatibility test cannot disagree.
    #[must_use]
    pub fn db_link_type(self) -> DbLinkType {
        match self {
            HwLinkKind::InstIo => DbLinkType::InstIo,
            HwLinkKind::VmeIo => DbLinkType::VmeIo,
            HwLinkKind::CamacIo => DbLinkType::CamacIo,
            HwLinkKind::RfIo => DbLinkType::RfIo,
            HwLinkKind::AbIo => DbLinkType::AbIo,
            HwLinkKind::GpibIo => DbLinkType::GpibIo,
            HwLinkKind::BitbusIo => DbLinkType::BitbusIo,
            HwLinkKind::BbgpibIo => DbLinkType::BbgpibIo,
            HwLinkKind::VxiIo => DbLinkType::VxiIo,
        }
    }
}

/// Where a `VXI_IO` link's address comes from — C's `vxiio.flag`
/// (`link.h:166-173`, `VXIDYNAMIC`/`VXISTATIC` at `:63-64`) made explicit.
///
/// C keeps `frame`, `slot` and `la` side by side and lets `flag` say which
/// two of the three are meaningful, so `dbGetString` has to branch on the
/// flag before it can read the numbers (`dbStaticLib.c:1997-2005`). Here the
/// address IS the choice, so the unread member cannot be observed and the
/// renderer has nothing to test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VxiAddr {
    /// C `VXIDYNAMIC` — the `#Vn Cn Sn` form (`"VCS"`), addressed by slot.
    Dynamic { frame: i16, slot: i16 },
    /// C `VXISTATIC` — the `#Vn Sn` form (`"VS"`), addressed by logical
    /// address.
    Static { la: i16 },
}

/// Hardware link as parsed from a record's INP/OUT field — C's hardware
/// members of `union value` (`link.h:100-173`), one variant per bus.
///
/// **The numbers are the link.** C never keeps the text a hardware link was
/// written as: `dbParseLink` scans it into `hwnums[5]`, `dbSetLinkHW`
/// distributes those into the per-bus struct (`dbStaticLib.c:2453-2529`) and
/// every reader afterwards goes through `dbGetString`, which re-renders from
/// the struct with that bus's format (`:1953-2006`). So `#C0x10 S-2` reads
/// back as `#C16 S-2 @` — base normalised, `@parm` present though empty —
/// and no reader can see the original spelling. Storing the text beside the
/// parse would give the field two sources of truth and let them disagree;
/// here [`HwLink::render`] is the only way to get text out.
///
/// The integer widths are C's, not `i64`: `vmeio`/`camacio`/`rfio`/`abio`/
/// `gpibio`/`vxiio` hold `short` and `bitbusio`/`bbgpibio` hold
/// `unsigned char`, while `hwnums` is `int`, so the assignment truncates and
/// the renderer prints the truncated value — `#L300 N2 P3 S4` is bitbus link
/// 44. Carrying the wide value and narrowing at each printer would put that
/// rule in every reader instead of in the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HwLink {
    /// `@…` — `INST_IO`. C stores everything after the `@` verbatim
    /// (`dbStaticLib.c:2266-2274`), leading blanks included, so `@ dev`
    /// round-trips as `@ dev`.
    InstIo { string: String },
    /// `#Cn Sn [@parm]` — `VME_IO`.
    VmeIo {
        card: i16,
        signal: i16,
        parm: String,
    },
    /// `#Bn Cn Nn [An [Fn]] [@parm]` — `CAMAC_IO`.
    CamacIo {
        b: i16,
        c: i16,
        n: i16,
        a: i16,
        f: i16,
        parm: String,
    },
    /// `#Rn Mn Dn En` — `RF_IO`, the one bus with no parm at all.
    RfIo {
        cryo: i16,
        micro: i16,
        dataset: i16,
        element: i16,
    },
    /// `#Ln An Cn Sn [@parm]` — `AB_IO`.
    AbIo {
        link: i16,
        adapter: i16,
        card: i16,
        signal: i16,
        parm: String,
    },
    /// `#Ln An [@parm]` — `GPIB_IO`.
    GpibIo { link: i16, addr: i16, parm: String },
    /// `#Ln Nn Pn Sn [@parm]` — `BITBUS_IO`.
    BitbusIo {
        link: u8,
        node: u8,
        port: u8,
        signal: u8,
        parm: String,
    },
    /// `#Ln Bn Gn [@parm]` — `BBGPIB_IO`.
    BbgpibIo {
        link: u8,
        bbaddr: u8,
        gpibaddr: u8,
        parm: String,
    },
    /// `#Vn Cn Sn [@parm]` or `#Vn Sn [@parm]` — `VXI_IO`.
    VxiIo {
        addr: VxiAddr,
        signal: i16,
        parm: String,
    },
}

impl HwLink {
    /// Which bus this link names.
    #[must_use]
    pub fn kind(&self) -> HwLinkKind {
        match self {
            HwLink::InstIo { .. } => HwLinkKind::InstIo,
            HwLink::VmeIo { .. } => HwLinkKind::VmeIo,
            HwLink::CamacIo { .. } => HwLinkKind::CamacIo,
            HwLink::RfIo { .. } => HwLinkKind::RfIo,
            HwLink::AbIo { .. } => HwLinkKind::AbIo,
            HwLink::GpibIo { .. } => HwLinkKind::GpibIo,
            HwLink::BitbusIo { .. } => HwLinkKind::BitbusIo,
            HwLink::BbgpibIo { .. } => HwLinkKind::BbgpibIo,
            HwLink::VxiIo { .. } => HwLinkKind::VxiIo,
        }
    }

    /// The text C prints for this link — the hardware arm of `dbGetString`
    /// (`dbStaticLib.c:1953-2006`), format for format.
    ///
    /// The single owner of "hardware link → text": `dbpr`, `dbgf`, the CA
    /// server's string read-back and `dbReportDeviceConfig` all reach it,
    /// because in C they all reach `dbGetString`.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            HwLink::InstIo { string } => format!("@{string}"),
            HwLink::VmeIo { card, signal, parm } => format!("#C{card} S{signal} @{parm}"),
            HwLink::CamacIo {
                b,
                c,
                n,
                a,
                f,
                parm,
            } => format!("#B{b} C{c} N{n} A{a} F{f} @{parm}"),
            HwLink::RfIo {
                cryo,
                micro,
                dataset,
                element,
            } => format!("#R{cryo} M{micro} D{dataset} E{element}"),
            HwLink::AbIo {
                link,
                adapter,
                card,
                signal,
                parm,
            } => format!("#L{link} A{adapter} C{card} S{signal} @{parm}"),
            HwLink::GpibIo { link, addr, parm } => format!("#L{link} A{addr} @{parm}"),
            HwLink::BitbusIo {
                link,
                node,
                port,
                signal,
                parm,
            } => format!("#L{link} N{node} P{port} S{signal} @{parm}"),
            HwLink::BbgpibIo {
                link,
                bbaddr,
                gpibaddr,
                parm,
            } => format!("#L{link} B{bbaddr} G{gpibaddr} @{parm}"),
            HwLink::VxiIo {
                addr: VxiAddr::Dynamic { frame, slot },
                signal,
                parm,
            } => format!("#V{frame} C{slot} S{signal} @{parm}"),
            HwLink::VxiIo {
                addr: VxiAddr::Static { la },
                signal,
                parm,
            } => format!("#V{la} S{signal} @{parm}"),
        }
    }
}

/// A CONSTANT link: its text and the value it decodes to, decoded once when
/// the link is parsed — C decodes at `dbInitLink` and never again, so every
/// read of a constant is a copy, not a re-parse of the text.
#[derive(Debug)]
pub struct ConstantLink {
    text: String,
    value: EpicsValue,
    /// `Some` for a `{const: …}` link: the value `lnkConst` parsed, which its
    /// `loadArray` converts by its own type instead of handing the text to
    /// `dbPutConvertJSON` as `dbConstLink` does.
    json: Option<LnkConst>,
}

/// `const_link.type` and its value (`lnkConst.c:26-38`), one variant per
/// state the yajl callbacks can leave it in; `Empty` is `a0`, `[]`.
#[derive(Debug, Clone, PartialEq)]
enum LnkConst {
    Integer(i64),
    Double(f64),
    String(String),
    Integers(Vec<i64>),
    Doubles(Vec<f64>),
    Strings(Vec<String>),
    Empty,
}

/// Why a constant link's `loadArray` stored nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstLoadError {
    /// `S_db_badField`: an empty constant, `{const:[]}`, or text
    /// `dbPutConvertJSON` refuses. `devSASoft.c:106` treats it as "no INP".
    BadField,
    /// A scalar string's `cvt_st_*` status (`lnkConst.c:490-492`).
    Conversion,
}

/// `dbValueSize(DBR_STRING) - 1`, the `cvt_st_st` cut under a NULL `paddr`.
const DBR_STRING_CAPACITY: usize = 39;

impl LnkConst {
    /// The yajl callbacks `lnkConst_integer` / `_double` / `_string` /
    /// `_boolean` over the link's value. `None` is a value they refuse: null,
    /// a map, a nested array, strings mixed with numbers, or an integer yajl
    /// overflows on.
    fn of(v: &serde_json::Value) -> Option<Self> {
        use serde_json::Value as J;
        let int = |v: &J| match v {
            J::Bool(b) => Some(i64::from(*b)),
            J::Number(n) => n.as_i64(),
            _ => None,
        };
        let real = |v: &J| match v {
            J::Number(n) if n.is_f64() => n.as_f64(),
            v => int(v).map(|i| i as f64),
        };
        Some(match v {
            J::String(s) => Self::String(s.clone()),
            J::Array(items) if items.is_empty() => Self::Empty,
            J::Array(items) => {
                if let Some(v) = items.iter().map(int).collect::<Option<Vec<_>>>() {
                    Self::Integers(v)
                } else if let Some(v) = items.iter().map(real).collect::<Option<Vec<_>>>() {
                    // `lnkConst_double` promotes an `ai64` array to `af64`.
                    Self::Doubles(v)
                } else {
                    Self::Strings(
                        items
                            .iter()
                            .map(|v| v.as_str().map(str::to_owned))
                            .collect::<Option<_>>()?,
                    )
                }
            }
            v => match (int(v), real(v)) {
                (Some(i), _) => Self::Integer(i),
                (None, Some(d)) => Self::Double(d),
                (None, None) => return None,
            },
        })
    }

    /// C `lnkConst_loadArray` (`lnkConst.c:451-541`): every element through
    /// `dbFastPutConvertRoutine[its own type][dbrType]` with a NULL `paddr`.
    fn load_array(&self, dbr_type: DbFieldType, nreq: usize) -> Result<EpicsValue, ConstLoadError> {
        // `cvt_st_e` dereferences the NULL `paddr`; a choice loads as the
        // index it is.
        let row = match dbr_type {
            DbFieldType::Enum => DbFieldType::UShort,
            t => t,
        };
        let head = |n: usize| n.min(nreq);
        let mut loaded = match self {
            Self::Integer(i) => integers_into(row, std::slice::from_ref(i)),
            Self::Double(d) => doubles_into(row, std::slice::from_ref(d)),
            // The long-string branch: a `strncpy` of `nReq` bytes, NUL
            // forced into the last, NORD counting the NUL.
            Self::String(s) if row == DbFieldType::Char => {
                let bytes = s.as_bytes();
                let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
                let mut chars = bytes[..end.min(nreq.saturating_sub(1))].to_vec();
                if nreq > 0 {
                    chars.push(0);
                }
                return Ok(EpicsValue::CharArray(chars.into()));
            }
            Self::String(s) => match strings_into(row, std::slice::from_ref(s)) {
                (loaded, true) => loaded,
                (_, false) => return Err(ConstLoadError::Conversion),
            },
            Self::Integers(v) => integers_into(row, &v[..head(v.len())]),
            Self::Doubles(v) => doubles_into(row, &v[..head(v.len())]),
            // The array arm ignores each `conv` status: a refused element
            // keeps the calloc'd buffer's zero.
            Self::Strings(v) => strings_into(row, &v[..head(v.len())]).0,
            Self::Empty => return Err(ConstLoadError::BadField),
        };
        loaded.truncate(nreq);
        Ok(loaded.into_type(dbr_type))
    }
}

impl LnkConst {
    /// C `lnkConst_loadScalar` (`lnkConst.c:358-417`): the value, or an
    /// array's element 0, through `dbFastPutConvertRoutine[its own
    /// type][dbrType]`, so a JSON double takes the C cast (`{const:5e9}` into
    /// `DBF_LONG` is `INT_MIN`) and only a string can refuse.
    fn load_scalar(&self, dbr_type: DbFieldType) -> Result<EpicsValue, ConstLoadError> {
        let row = match dbr_type {
            DbFieldType::Enum => DbFieldType::UShort,
            t => t,
        };
        let strings = |v: &[String]| match strings_into(row, v) {
            (loaded, true) => Ok(loaded),
            (_, false) => Err(ConstLoadError::Conversion),
        };
        let loaded = match self {
            Self::Integer(i) => integers_into(row, std::slice::from_ref(i)),
            Self::Integers(v) => integers_into(row, &v[..1]),
            Self::Double(d) => doubles_into(row, std::slice::from_ref(d)),
            Self::Doubles(v) => doubles_into(row, &v[..1]),
            Self::String(s) => strings(std::slice::from_ref(s))?,
            Self::Strings(v) => strings(&v[..1])?,
            Self::Empty => return Err(ConstLoadError::BadField),
        };
        loaded
            .into_type(dbr_type)
            .first_element()
            .ok_or(ConstLoadError::BadField)
    }
}

/// `cvtInt64ToString` / `cvt_q_*`.
fn integers_into(row: DbFieldType, v: &[i64]) -> EpicsValue {
    if row == DbFieldType::String {
        return EpicsValue::StringArray(v.iter().map(|i| PvString::from(i.to_string())).collect());
    }
    EpicsValue::Int64Array(v.to_vec().into()).into_type(row)
}

/// `cvt_d_*`; `cvt_d_st` finds no rset under the NULL `paddr` and keeps its
/// precision of 6.
fn doubles_into(row: DbFieldType, v: &[f64]) -> EpicsValue {
    if row == DbFieldType::String {
        return EpicsValue::StringArray(
            v.iter()
                .map(|d| dbr_string(crate::types::codec::cvt_double_to_string(*d, 6).as_bytes()))
                .collect(),
        );
    }
    EpicsValue::DoubleArray(v.to_vec().into()).into_type(row)
}

/// `cvt_st_*`, and whether every element converted; a refused element is 0.
fn strings_into(row: DbFieldType, v: &[String]) -> (EpicsValue, bool) {
    use crate::types::c_parse::{Converted, NumericField, put_string};
    let Some(numeric) = NumericField::of(row) else {
        return (
            EpicsValue::StringArray(v.iter().map(|s| dbr_string(s.as_bytes())).collect()),
            true,
        );
    };
    let mut converted = true;
    let elems: Vec<Option<EpicsValue>> = v
        .iter()
        .map(|s| match put_string("INP", numeric, s) {
            Ok(Converted::Stored(x)) => Some(x),
            Ok(Converted::Unchanged) => None,
            Err(_) => {
                converted = false;
                None
            }
        })
        .collect();
    macro_rules! pack {
        ($array:ident, $scalar:ident) => {
            EpicsValue::$array(
                elems
                    .into_iter()
                    .map(|e| match e {
                        Some(EpicsValue::$scalar(x)) => x,
                        _ => Default::default(),
                    })
                    .collect(),
            )
        };
    }
    let packed = match numeric {
        NumericField::Char => pack!(CharArray, Char),
        NumericField::UChar => pack!(UCharArray, UChar),
        NumericField::Short => pack!(ShortArray, Short),
        NumericField::UShort => pack!(UShortArray, UShort),
        NumericField::Long => pack!(LongArray, Long),
        NumericField::ULong => pack!(ULongArray, ULong),
        NumericField::Int64 => pack!(Int64Array, Int64),
        NumericField::UInt64 => pack!(UInt64Array, UInt64),
        NumericField::Float => pack!(FloatArray, Float),
        NumericField::Double => pack!(DoubleArray, Double),
    };
    (packed, converted)
}

fn dbr_string(bytes: &[u8]) -> PvString {
    PvString::from_bytes(&bytes[..bytes.len().min(DBR_STRING_CAPACITY)])
}

impl ConstantLink {
    /// The C `dbConstLoadScalar` / `dbConstLoadArray` pair
    /// (`dbConstLink.c:152-199`), which is what a record's `init_record` gets
    /// out of a constant link through `dbLoadLink` / `dbLoadLinkArray`.
    ///
    /// A bracketed constant (`[1, 2, 3]`) is an ARRAY: C hands the text to
    /// `dbPutConvertJSON`, which yields `nRequest` elements. Yielding the
    /// bracket text as a String instead left every array-valued constant link
    /// (`field(INP, "[1,2,3]")` on an aai/waveform, a constant closed-loop aao
    /// DOL) delivering a nonsense string to a typed array buffer.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let value = if let Some(inner) = text
            .trim()
            .strip_prefix('[')
            .and_then(|body| body.strip_suffix(']'))
        {
            constant_array_value(inner)
        } else if let Some(v) = parse_c_double(&text) {
            EpicsValue::Double(v)
        } else {
            EpicsValue::String(text.clone().into())
        };
        Self {
            text,
            value,
            json: None,
        }
    }

    /// C `dbLoadLinkArray` on a constant link: `lnkConst_loadArray` for a
    /// `{const: …}` link, else `dbConstLoadArray` (`dbConstLink.c:194-212`),
    /// the text through `dbPutConvertJSON`. The value is `dbr_type`-typed and
    /// its length is the `*pnReq` C returns.
    pub fn load_array(
        &self,
        dbr_type: DbFieldType,
        nreq: usize,
    ) -> Result<EpicsValue, ConstLoadError> {
        if let Some(json) = &self.json {
            return json.load_array(dbr_type, nreq);
        }
        if self.text.is_empty() {
            return Err(ConstLoadError::BadField);
        }
        let row = match dbr_type {
            DbFieldType::Enum => DbFieldType::UShort,
            t => t,
        };
        crate::server::db_convert_json::db_put_convert_json(&self.text, row, nreq)
            .map_err(|_| ConstLoadError::BadField)
    }

    /// `lnkConst_loadScalar` for a `{const: …}` link; `None` for a
    /// `dbConstLink` one, whose load is its text.
    pub(crate) fn load_json_scalar(
        &self,
        dbr_type: DbFieldType,
    ) -> Option<Result<EpicsValue, ConstLoadError>> {
        self.json.as_ref().map(|json| json.load_scalar(dbr_type))
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// C `dbLoadLinkLS` over this constant: `lnkConst_loadLS` for a
    /// `{const: …}` link, which loads only its string forms
    /// (`lnkConst.c:419-449`), else `dbConstLoadLS` ([`load_link_ls`]).
    pub(crate) fn load_ls(&self) -> Option<LsLoad> {
        match &self.json {
            Some(LnkConst::String(s)) => Some(LsLoad::Text(s.clone())),
            Some(LnkConst::Strings(v)) => v.first().cloned().map(LsLoad::Text),
            Some(_) => None,
            None => load_link_ls(&self.text),
        }
    }

    /// Whether this is a `{const: …}` link, loaded by `lnkConst` rather than
    /// `dbConstLink`.
    pub fn is_json(&self) -> bool {
        self.json.is_some()
    }

    pub fn value(&self) -> &EpicsValue {
        &self.value
    }
}

/// Equal by text and source: the value is a function of them, and comparing
/// the value would make a `nan` constant unequal to itself.
impl PartialEq for ConstantLink {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.json == other.json
    }
}

/// Parsed link — distinguishes constants, DB links, CA/PVA links, and empty.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedLink {
    None,
    /// Shared for the same reason as `Db`: the link is immutable once parsed.
    Constant(std::sync::Arc<ConstantLink>),
    /// C `dbRemoveLink` left the field with no `lset` and nothing added it
    /// back (`dbAccess.c:1167-1188`): `dbLinkIsDefined` is false and
    /// `dbLinkIsConstant` true (`dbLink.c:215-225`), a read is `dbGetLink`'s
    /// silent -1 and a put `S_db_noLSET` (`dbLink.c:336-337`, `:440-441`).
    /// The record keeps the text, and `plink->type` is what the remove left:
    /// `PV_LINK` for a DB or CA link (`dbDbLink.c:132`, `dbCa.c:441`), the
    /// parsed type for any other. No parse makes this. Declared beside
    /// `None` and `Constant`, so the three `dbLinkIsConstant` variants take
    /// adjacent niche values and [`is_constant`] stays one range compare.
    ///
    /// [`is_constant`]: crate::server::recgbl::simm::is_constant
    Removed(DbLinkType),
    /// Shared, not owned: a parsed DB link is immutable after `parse_link_v2`
    /// and the cycle clones the record's `parsed_inp` every process, so the
    /// clone is one atomic increment instead of the link's three heap copies,
    /// and the enum moves as 16 bytes instead of `DbLink`'s 160.
    Db(std::sync::Arc<DbLink>),
    Ca(CaLink),
    /// PVA (`pvalink`) link whose payload is a verbatim channel name —
    /// the string shorthand `{pva:"name"}` or the `pva://name` scheme
    /// form. Per pvxs `pva_parse_string` (pvxs/ioc/pvalink_jlif.cpp:143-149) a
    /// string pvalink IS the channel name: any `?`/`&` in it is link
    /// DATA, not option syntax. The structured JSON longhand with
    /// options is [`ParsedLink::PvaJson`] instead, so this variant's
    /// `String` has exactly one meaning (a channel name) on every path.
    Pva(String),
    /// PVA (`pvalink`) link parsed from the structured JSON longhand
    /// `{pva:{pv:"name", field:"f", proc:"CP", …}}`. Carries the options
    /// as structured JLink members so the pvalink consumer reconstructs
    /// the `PvaLinkConfig` from map keys (pvxs/ioc/pvalink_jlif.cpp:69-196), not
    /// from a `?key=value` URI query that pvxs never parses. See
    /// [`PvaJsonLink`].
    PvaJson(PvaJsonLink),
    /// `@dev arg1 …` or `#Cn Sn` hardware link (epics-base PR #213).
    Hw(HwLink),
    /// epics-base PR `e3c9d590` / `20404003`: a `lnkCalc` JSON link
    /// computes a result from one or more input PV values + a calc
    /// expression, optionally pulling its timestamp from one of the
    /// inputs. JSON form:
    /// `{calc: {expr:"A*B", args:[{pva:"record"}, 1.5], time:"a"}}`
    /// — `time` is the input letter whose timestamp the result should
    /// carry, upper or lower case, `A` through the [`CALC_NARGS`](crate::calc::CALC_NARGS)th
    /// letter (`lnkCalc.c:180-186`). `time` may be omitted (no timestamp
    /// passthrough).
    Calc(CalcLink),
    /// `{state:"NAME"}` / `{state:"!NAME"}` — a one-element `DBF_SHORT`
    /// view of a process-global named boolean (C `lnkState.c`). See
    /// [`StateLink`].
    State(StateLink),
}

/// A `{state:…}` JSON link (C `lnkState.c`, `link(state, lnkStateIf)` at
/// `links.dbd.pod:171`).
///
/// The link reads and writes one named boolean in the `dbState` registry —
/// the same registry the `sync` channel filter
/// ([`db_state_registry`](crate::server::database::filters::sync::db_state_registry))
/// and the `Db State` device support already share, so all three observe
/// the same bit.
///
/// `invert` is one flag on the LINK, not a property of a direction: C xors
/// `slink->invert` into the value it hands a reader (`lnkState.c:148`) and
/// into the bit a writer sets (`:199`), so a `!` link reads back what it
/// wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateLink {
    /// The state name, with any leading `!` already removed.
    pub name: String,
    /// C `slink->invert`.
    pub invert: bool,
}

impl StateLink {
    /// C `lnkState_string` (`lnkState.c:79-89`): a leading `!` inverts, but
    /// only `if (len > 1 && val[0] == '!')` — the one-character value `"!"`
    /// names a state called `!` and is not an inversion of the empty name.
    #[must_use]
    pub fn from_link_value(value: &str) -> Self {
        match value.strip_prefix('!') {
            Some(name) if !name.is_empty() => Self {
                name: name.to_string(),
                invert: true,
            },
            _ => Self {
                name: value.to_string(),
                invert: false,
            },
        }
    }

    /// The bit a reader sees: C `lnkState_getValue` (`lnkState.c:148`)
    /// computes `slink->invert ^ dbStateGet(slink->state)`.
    #[must_use]
    pub fn read(&self, raw: bool) -> bool {
        self.invert ^ raw
    }

    /// The bit a writer stores: C `lnkState_putValue` (`lnkState.c:196-200`)
    /// keeps the caller's truth value in `slink->val` and xors `invert` into
    /// what it hands `dbStateSet`/`dbStateClear`.
    #[must_use]
    pub fn write(&self, value: bool) -> bool {
        self.invert ^ value
    }

    /// What C `lnkState_putValue` (`lnkState.c:153-203`) does with `value`.
    ///
    /// An array is its element 0 alone: past the `nRequest == 0` return the
    /// arms read `*pbuffer`. `DBR_STRING` is textual — "Only \"\" and \"0\"
    /// are FALSE" — so `"0.0"` is TRUE. A scalar `Enum` is TRUE when nonzero:
    /// it is what bo and mbbo hand over, and C passes those as `DBR_USHORT`.
    /// Only an `FTVL=ENUM` array reaches C as `DBR_ENUM`, which has no arm.
    #[must_use]
    pub fn put_of(value: &crate::types::EpicsValue) -> StatePut {
        use crate::types::EpicsValue as V;
        if value.is_empty_array() {
            return StatePut::Nothing;
        }
        if matches!(value, V::EnumArray(_)) {
            return StatePut::BadDbrType;
        }
        // Borrowed: the put runs under the writer's hold.
        let text = match value {
            V::String(s) => Some(s),
            V::StringArray(a) => a.first(),
            _ => None,
        };
        if let Some(s) = text {
            let b = s.as_bytes();
            let (b0, b1) = (b.first().copied(), b.get(1).copied());
            return StatePut::Truth(
                b0.is_some_and(|c| c != 0) && (b0 != Some(b'0') || b1.is_some_and(|c| c != 0)),
            );
        }
        let element = value.first_element();
        element
            .as_ref()
            .unwrap_or(value)
            .to_f64()
            .map_or(StatePut::BadDbrType, |v| StatePut::Truth(v != 0.0))
    }
}

/// [`StateLink::put_of`]: the three ways C `lnkState_putValue` leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatePut {
    /// Drive the bit from this truth value, before `invert`.
    Truth(bool),
    /// `nRequest == 0` (`lnkState.c:154-155`): status 0, nothing written.
    Nothing,
    /// No arm for the DBR class: `S_db_badDbrtype`.
    BadDbrType,
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
/// (`pvxs/ioc/pvalink_jlif.cpp:189-191`), and a string `proc:"true"` is likewise
/// ignored because only `CP`/`CPP`/`PP`/`NPP`/empty are recognized
/// strings (`:156-170`). Collapsing every option to its text form erases
/// that distinction, so this enum carries the kind through to the pvalink
/// consumer, which reproduces pvxs's per-type dispatch exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JlinkValue {
    /// JSON `null` — `pva_parse_null` (`proc`→Default, `sevr`→NMS,
    /// `local`→false; pvxs/ioc/pvalink_jlif.cpp:69-88).
    Null,
    /// JSON boolean — `pva_parse_bool` (pvxs/ioc/pvalink_jlif.cpp:90-122).
    Bool(bool),
    /// JSON integer — `pva_parse_integer` (`Q`, `monorder`;
    /// pvxs/ioc/pvalink_jlif.cpp:124-141).
    Int(i64),
    /// JSON string — `pva_parse_string` (`pv`, `field`, and the `proc`/
    /// `sevr` enum strings; pvxs/ioc/pvalink_jlif.cpp:143-197).
    Str(String),
}

/// A PVA (`pvalink`) external link parsed from the structured JSON
/// longhand `{pva:{pv:"name", field:"f", proc:"CP", …}}`.
///
/// pvxs parses pvalink options only as JLink map keys / typed values
/// (pvxs/ioc/pvalink_jlif.cpp:69-196): booleans (`pipeline`/`time`/`retry`/
/// `local`/`atomic`), integers (`Q`/`monorder`), strings (`field`/
/// `proc`/`sevr`). There is no `?key=value` URI query parser in the
/// JLink callback table (pvxs/ioc/pvalink_jlif.cpp:286-300). Preserving the
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

/// One element of a `lnkCalc` `args` array.
///
/// C gives each slot two producers and one index: `lnkCalc_integer` and
/// `lnkCalc_double` write a bare number straight into `arg[nArgs++]`
/// (`lnkCalc.c:141`, `:161`), while an embedded JSON link takes
/// `inp[nArgs++]` (`lnkCalc.c:353`) and overwrites `arg[i]` from
/// `dbGetLink` at read time (`lnkCalc.c:585`). A slot is a value or a
/// link and never both, which is why this is a sum type rather than a
/// name string that some parse steps happen to fill in.
#[derive(Clone, Debug, PartialEq)]
pub enum CalcArg {
    /// A numeric literal, held for the life of the link. C leaves the
    /// matching `inp[i]` zeroed, i.e. a CONSTANT link, and `dbGetLink` on
    /// a constant delivers nothing (`dbConstLink.c:219-225`
    /// `*pnRequest = 0`), so nothing ever overwrites `arg[i]`.
    Literal(f64),
    /// An embedded JSON link — `{pva:"record"}`, `{const:2}`, a nested
    /// `{calc:…}`. Read once per evaluation, no processing.
    Link(Box<ParsedLink>),
}

/// Configuration for a `lnkCalc` link.
#[derive(Clone, Debug, PartialEq)]
pub struct CalcLink {
    /// Calc expression in epics-base postfix syntax — e.g. `"A+B*2"`,
    /// `"MAX(A,B,C)"`. Variables A.. bind to `args` by position, up to
    /// [`CALC_NARGS`](crate::calc::CALC_NARGS).
    pub expr: String,
    /// Input arguments. Each `args[i]` is resolved at link-read time and
    /// bound to the calc engine's variable slot at index `i` (0→A, 1→B,
    /// …). At most [`CALC_NARGS`](crate::calc::CALC_NARGS) of them — C stops the parse at the
    /// limit rather than truncating (`lnkCalc.c:135-139`).
    pub args: Vec<CalcArg>,
    /// C `clink->tinp` (`lnkCalc.c:184`) as its letter: the input whose
    /// timestamp and userTag the reading record adopts. Normalised to upper
    /// case and bounded by [`CALC_NARGS`](crate::calc::CALC_NARGS) (`lnkCalc.c:180-186`), so it
    /// indexes `args` directly. `None` is C's `tinp = -1` —
    /// `lnkCalc_getTimestampTag` returns -1 and the consumer keeps its own
    /// `apply_timestamp` time.
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
///
/// The name is PRIVATE, and deliberately: it has two readings that are not
/// the same string, and a caller that can reach the field can only pick one
/// of them by accident. [`Self::pvname`] is the unsplit name C keeps in
/// `plink->value.pv_link.pvname`; [`Self::target`] is that name resolved
/// into record, field and filter. For `src.[2]` the first is `src.[2]` and
/// the second is (`src`, `VAL`, `arr`) — reading the raw halves gets a
/// "record" of `src.[2]` and a "field" of `VAL`, which is what made the
/// local lookup miss, `dbInitLink` locality rewrite the link as CA and the
/// link never connect. Callers now have to say which meaning they want.
#[derive(Clone, Debug, PartialEq)]
pub struct DbLink {
    /// The record half of the modifier-stripped link text, as
    /// [`split_record_field`] divided it — NOT the addressed record: a name
    /// whose remainder is not a field identifier (`src.[2]`, `A.B.C`) keeps
    /// the whole string here, exactly as C leaves it until
    /// `dbChannelCreate` walks the filter.
    record: String,
    /// The field half of the same split, or `VAL` when the name carried
    /// none — again the split, not the addressed field: `src.NORD[2]` leaves
    /// `VAL` here while the channel it names addresses `NORD`.
    field: String,
    pub policy: LinkProcessPolicy,
    pub monitor_switch: MonitorSwitch,
    /// [`Self::pvname`] resolved, computed ONCE here because
    /// [`Self::new`] is the only way to make a `DbLink` — so the two cannot
    /// drift, and no reader pays for the resolution.
    ///
    /// It used to be re-derived on every [`Self::target`] call: the name
    /// reassembled out of the halves above and handed back to
    /// `parse_channel_name`, five string allocations to recover a split this
    /// struct had already made. A process cycle asks it several times per
    /// link — the value read, the self-link test, the out-target resolve —
    /// where C dereferences a `dbAddr` it filled in at `dbInitLink`.
    channel: crate::server::database::filters::ChannelName,
    /// Whether [`Self::target`] addresses `VAL`, whose local name is the
    /// record's own and so cannot be a simple PV shadowing the field.
    target_is_val: bool,
    /// C's `dbAddr` from `dbDbInitLink`: the local record and field this
    /// link resolved to, so a read does no name work. Not part of the link's
    /// identity — a clone starts empty and equality ignores it.
    addr: DbAddrCell,
}

/// What a [`DbLink`]'s target name answers at one name-map revision.
pub(crate) enum DbLinkAnswer {
    /// A local record — C's `dbAddr`.
    Local(super::record_instance::ResolvedTarget),
    /// A simple PV spelling the target `REC.FLD`, which shadows the field.
    Shadowed(std::sync::Arc<crate::server::pv::ProcessVariable>),
    /// Neither; the by-name path decides.
    Absent,
}

/// The settled [`DbLinkAnswer`] of a [`DbLink`], stamped with the name-map
/// revision it was resolved under and re-resolved once that moves (records,
/// aliases and simple PVs can come and go after `iocInit` here, unlike C).
///
/// Two caches, because a link target handle never outlives its target's map
/// entry and only the reader's `INP` is reached by the sweep that enforces
/// it. `owned` (any link, any caller) keeps a `Weak`, so no record is held
/// by a link no sweep reaches. `lent` ([`DbLink::lend_addr`], reachable only
/// through `RecordInstance::lend_inp_addr` and [`DbLink::lend_out_addr`])
/// keeps strong handles, which
/// [`DbLink::release_addr`] drops when the target is removed or the reader
/// destroyed; until then it keeps every target it has published — one per
/// retarget, not per read — since a lent one may still be borrowed.
struct DbAddrCell {
    owned: parking_lot::Mutex<Option<CachedDbAddr>>,
    /// The name-map revision `current` answers for; `u64::MAX` for none.
    revision: std::sync::atomic::AtomicU64,
    /// The newest entry of `lent`, or null when the answer at `revision` is
    /// not a local record.
    current: std::sync::atomic::AtomicPtr<super::record_instance::ResolvedTarget>,
    lent: parking_lot::Mutex<Vec<std::sync::Arc<super::record_instance::ResolvedTarget>>>,
}

impl Default for DbAddrCell {
    fn default() -> Self {
        Self {
            owned: parking_lot::Mutex::new(None),
            revision: std::sync::atomic::AtomicU64::new(NO_REVISION),
            current: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            lent: parking_lot::Mutex::new(Vec::new()),
        }
    }
}

const NO_REVISION: u64 = u64::MAX;

struct CachedDbAddr {
    answer: CachedAnswer,
    revision: u64,
}

enum CachedAnswer {
    Local {
        rec: std::sync::Weak<super::record_instance::RecordCell>,
        name: std::sync::Arc<str>,
        put: super::record_instance::PutAddr,
        f64_slot: Option<super::record_trait::FieldSlot>,
    },
    Shadowed(std::sync::Arc<crate::server::pv::ProcessVariable>),
    Absent,
}

impl Clone for DbAddrCell {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for DbAddrCell {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for DbAddrCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DbAddrCell")
    }
}

impl DbLink {
    /// The answer at name-map `revision`: the cached one while the maps are
    /// at the revision it was resolved under, else `resolve`'s, cached.
    pub(crate) fn answer(
        &self,
        revision: u64,
        resolve: impl FnOnce() -> DbLinkAnswer,
    ) -> DbLinkAnswer {
        if let Some(cached) = &*self.addr.owned.lock()
            && cached.revision == revision
        {
            match &cached.answer {
                CachedAnswer::Local {
                    rec,
                    name,
                    put,
                    f64_slot,
                } => {
                    if let Some(rec) = rec.upgrade() {
                        return DbLinkAnswer::Local(super::record_instance::ResolvedTarget {
                            rec,
                            name: name.clone(),
                            put: *put,
                            f64_slot: *f64_slot,
                        });
                    }
                }
                CachedAnswer::Shadowed(pv) => return DbLinkAnswer::Shadowed(pv.clone()),
                CachedAnswer::Absent => return DbLinkAnswer::Absent,
            }
        }
        let answer = resolve();
        let cached = match &answer {
            DbLinkAnswer::Local(target) => CachedAnswer::Local {
                rec: std::sync::Arc::downgrade(&target.rec),
                name: target.name.clone(),
                put: target.put,
                f64_slot: target.f64_slot,
            },
            DbLinkAnswer::Shadowed(pv) => CachedAnswer::Shadowed(pv.clone()),
            DbLinkAnswer::Absent => CachedAnswer::Absent,
        };
        *self.addr.owned.lock() = Some(CachedDbAddr {
            answer: cached,
            revision,
        });
        answer
    }

    /// The local target of [`Self::answer`], lent instead of cloned: on a
    /// hit two loads and a compare, no lock and no refcount — C's `dbAddr`
    /// dereference. A settled non-local answer is a hit too, so it is not
    /// re-resolved each read. The handle is strong (see [`DbAddrCell`]).
    ///
    /// # Safety
    ///
    /// [`Self::release_addr`] must not run while the returned borrow lives.
    /// Private, so the lenders are
    /// [`super::record_instance::RecordInstance::lend_inp_addr`], whose
    /// `&mut` reader excludes `release_addr`'s callers, and
    /// [`Self::lend_out_addr`], whose `&SetGuard` does.
    #[inline(always)]
    unsafe fn lend_addr(
        &self,
        revision: u64,
        resolve: impl FnOnce() -> DbLinkAnswer,
    ) -> Option<&super::record_instance::ResolvedTarget> {
        use std::sync::atomic::Ordering::Acquire;
        if self.addr.revision.load(Acquire) == revision {
            let at = self.addr.current.load(Acquire);
            // SAFETY: a non-null `current` points into `lent`, which only
            // `release_addr` empties, and the caller excludes that.
            return (!at.is_null()).then(|| unsafe { &*at });
        }
        // SAFETY: as above, for the entry the miss publishes.
        self.publish_lent(revision, resolve)
            .map(|at| unsafe { &*std::sync::Arc::as_ptr(&at) })
    }

    /// An output link's local target lent for as long as the writer holds
    /// `held`, the set of the record that owns this link: C's `dbAddr` of
    /// `dbDbPutValue`. `None` is no settled local answer; the caller takes
    /// [`Self::answer`]'s owned route for it.
    ///
    /// Sound because `release_addr` runs only from
    /// `RecordInstance::release_link_addrs`, with the link's owner taken by
    /// a full acquire: `held` keeps that from every other thread, and this
    /// thread cannot reach it from inside a frame.
    #[inline(always)]
    pub(crate) fn lend_out_addr<'g>(
        &'g self,
        _held: &'g crate::server::database::SetGuard,
        revision: u64,
        resolve: impl FnOnce() -> DbLinkAnswer,
    ) -> Option<&'g super::record_instance::ResolvedTarget> {
        // SAFETY: see above; the borrow is tied to `held`.
        unsafe { self.lend_addr(revision, resolve) }
    }

    /// Resolve (outside the cell lock: `resolve` takes record locks) and
    /// publish the answer to `lent`. The same record re-resolved under a moved
    /// revision keeps its entry, since its field address is a function of
    /// the record.
    #[cold]
    #[inline(never)]
    fn publish_lent(
        &self,
        revision: u64,
        resolve: impl FnOnce() -> DbLinkAnswer,
    ) -> Option<std::sync::Arc<super::record_instance::ResolvedTarget>> {
        use std::sync::atomic::Ordering::Release;
        let DbLinkAnswer::Local(target) = resolve() else {
            self.addr.current.store(std::ptr::null_mut(), Release);
            self.addr.revision.store(revision, Release);
            return None;
        };
        let mut lent = self.addr.lent.lock();
        let entry = match lent.last() {
            Some(last) if std::sync::Arc::ptr_eq(&last.rec, &target.rec) => last.clone(),
            _ => {
                let entry = std::sync::Arc::new(target);
                lent.push(entry.clone());
                entry
            }
        };
        self.addr
            .current
            .store(std::sync::Arc::as_ptr(&entry).cast_mut(), Release);
        self.addr.revision.store(revision, Release);
        Some(entry)
    }

    /// Drop every lent target the cell holds — the [`DbAddrCell`] half of the
    /// rule that a link target handle never outlives its target's map
    /// entry.
    ///
    /// # Safety
    ///
    /// No borrow from [`Self::lend_addr`] may be live: call it only with
    /// the reader that lends this link exclusively borrowed.
    pub(crate) unsafe fn release_addr(&self) {
        use std::sync::atomic::Ordering::Release;
        let mut lent = self.addr.lent.lock();
        self.addr.current.store(std::ptr::null_mut(), Release);
        // A null `current` at a live revision is a settled answer, so the
        // emptied cell must answer for none.
        self.addr.revision.store(NO_REVISION, Release);
        lent.clear();
    }

    /// Build a DB link from the modifier-stripped PV name, applying C
    /// `dbNameToAddr`'s record/field split (`split_record_field`) and its
    /// "absent field name" fallback to `VAL` (`dbFindFieldPart`,
    /// `dbStaticLib.c:1802-1811`).
    ///
    /// The only way to make a `DbLink`, so the split and the fallback have
    /// one implementation rather than one per construction site, and a
    /// `record`/`field` pair that the split could never have produced is
    /// not constructible.
    pub fn new(pvname: &str, policy: LinkProcessPolicy, monitor_switch: MonitorSwitch) -> Self {
        let (record, field) = match split_record_field(pvname) {
            Some((record, field)) => (record.to_string(), field),
            None => (pvname.to_string(), "VAL".to_string()),
        };
        // From the REASSEMBLED name, not the caller's: `src.VAL` and `src`
        // split alike but are not the same `record_path`, and `pvname` is
        // what every reader of this link addresses.
        let channel = crate::server::database::filters::parse_channel_name(&if field == "VAL" {
            record.clone()
        } else {
            format!("{record}.{field}")
        });
        let target_is_val = channel.field == "VAL";
        Self {
            record,
            field,
            policy,
            monitor_switch,
            channel,
            target_is_val,
            addr: DbAddrCell::default(),
        }
    }

    /// The UNSPLIT name, verbatim: `record` for a default-`VAL` link,
    /// `record.FIELD` otherwise.
    ///
    /// This is the string C keeps in `plink->value.pv_link.pvname` and hands
    /// to `dbChannelCreate` (`dbDbLink.c:94`) and, when that fails, to
    /// `dbCaAddLink` (`dbLink.c:128`) — so the DB-link target name and the
    /// CA channel name a non-local target falls through to are the same
    /// string by construction, not by two sites agreeing to build it alike.
    /// Ask for it when the CA/PVA boundary is what you are feeding, or when
    /// you want a per-link identity; ask [`Self::target`] for anything that
    /// resolves against this IOC's record map.
    pub fn pvname(&self) -> String {
        if self.field == "VAL" {
            self.record.clone()
        } else {
            format!("{}.{}", self.record, self.field)
        }
    }

    /// The record, field and channel filter [`Self::pvname`] addresses.
    ///
    /// The split itself is not made here.
    /// [`parse_channel_name`](crate::server::database::filters::parse_channel_name)
    /// owns it for CA CREATE_CHANNEL and both PVA sources, and owning it
    /// for links too is what keeps "where does the name stop and the
    /// filter begin" one answer instead of four.
    pub fn target(&self) -> &crate::server::database::filters::ChannelName {
        &self.channel
    }

    /// [`Self::target`]'s field as the link text spells it: `ChannelName`
    /// upper-cases its `field`, and `dbFindFieldPart` compares the name as
    /// written. The `$` modifier is not part of the name.
    pub(crate) fn field_as_written(&self) -> &str {
        let field = crate::server::database::parse_pv_name(&self.channel.record_path).1;
        field.strip_suffix('$').unwrap_or(field)
    }

    /// Whether [`Self::target`]'s field is `VAL` (see `target_is_val`).
    pub(crate) fn target_is_val(&self) -> bool {
        self.target_is_val
    }
}

/// A Channel Access / PV Access external link to a remote PV.
///
/// carries the parsed `MS`/`NMS`/`MSI`/`MSS` maximize-
/// severity policy alongside the PV name, so the alarm gate is applied
/// at the record-processing boundary (uniform with [`DbLink`]) rather
/// than discarded as syntax. Mirrors the C link option parsed by
/// `dbStaticLib.c:2375` and applied by `recGbl.c:263-281`. The PV name never
/// carries trailing modifier tokens (they are stripped during parse);
/// it may retain a `ca://` scheme prefix, which the resolver strips.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaLink {
    pub pv: String,
    pub monitor_switch: MonitorSwitch,
    /// Link-processing policy carried by this link's parsed modifier,
    /// exactly as [`DbLink::policy`] does. When it is `CP`/`CPP`, the dbCa
    /// equivalent (`calink`) must subscribe a monitor and process the
    /// link-holder on every remote change (C `dbCa.c:958-962`
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
    /// `DB_LINK` — a `PV_LINK` that `dbInitLink` resolved to a record in THIS
    /// database (`dbLink.c:112-120`). Never produced by
    /// [`ParsedLink::db_link_type`]: `dbParseLink` cannot see locality, so it
    /// stops at `PV_LINK` and only [`ParsedLink::link_type_after_init`]
    /// reaches this.
    DbLink,
    /// `CA_LINK` — a `PV_LINK` that `dbInitLink` sent to Channel Access,
    /// either because the target is not local or because the link asked for
    /// `CA`/`CP`/`CPP` (`dbLink.c:112-125`). Same note as [`Self::DbLink`].
    CaLink,
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
            DbLinkType::DbLink => "DB_LINK",
            DbLinkType::CaLink => "CA_LINK",
        }
    }

    /// What C renders a link of this type with an EMPTY payload as — the state
    /// `dbInitRecordLinks` leaves every link in before it looks at the text
    /// (`dbStaticLib.c:2185-2212`), and the state a link it refuses is left in,
    /// because that pass frees `plink->text` on every arm (`:2229`).
    ///
    /// The strings are C's own `dbGetStringNum` formats (`:1953-2006`) with
    /// every numeric field zero: `#V0 C0 S0 @` because a zeroed `vxiio.flag`
    /// is `VXIDYNAMIC` (`link.h:52`). `DB_LINK`/`CA_LINK` are runtime
    /// identities of a `PV_LINK` and no `device()` line can declare them, so
    /// they answer for the empty `PV_LINK` they came from.
    #[must_use]
    pub fn empty_link_text(self) -> &'static str {
        match self {
            DbLinkType::Constant | DbLinkType::JsonLink => "",
            DbLinkType::PvLink | DbLinkType::DbLink | DbLinkType::CaLink => "",
            DbLinkType::VmeIo => "#C0 S0 @",
            DbLinkType::CamacIo => "#B0 C0 N0 A0 F0 @",
            DbLinkType::AbIo => "#L0 A0 C0 S0 @",
            DbLinkType::GpibIo => "#L0 A0 @",
            DbLinkType::BitbusIo => "#L0 N0 P0 S0 @",
            DbLinkType::InstIo => "@",
            DbLinkType::BbgpibIo => "#L0 B0 G0 @",
            DbLinkType::RfIo => "#R0 M0 D0 E0",
            DbLinkType::VxiIo => "#V0 C0 S0 @",
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

/// C `dbParseLink` (`dbStaticLib.c:2626`) — the only half of the link gate a
/// **db-load** runs. `dbPutString` on a link field parses the text and stores
/// it; whether the link FITS the record's device support is never asked there.
///
/// Two failure arms, both C's: a brace-delimited link naming no registered
/// jlink type, and a `#` form whose identifier letters name no bus.
pub fn check_link_text(record_type: &str, upper_field: &str, text: &str) -> CaResult<()> {
    let Some(class) = crate::types::dbf_link_class(record_type, upper_field) else {
        return Ok(());
    };
    check_json_link_text(text, LinkFieldType::for_class(class))?;
    check_hw_link_text(text)
}

/// C `dbCanSetLink` (`dbStaticLib.c:2400-2419`), the rule alone: may a link of
/// the type this text parses to be installed on a field whose device support
/// declares another?
///
/// `None` when it fits, when the field is not a link field, or when no
/// vendored `device()` line declares a type to enforce (see
/// [`declared_link_type`]). `Some((declared, parsed))` is the pair C names in
/// its diagnostic, in that order.
pub fn link_type_mismatch(
    record_type: &str,
    dtyp: Option<&str>,
    upper_field: &str,
    text: &str,
) -> Option<(DbLinkType, DbLinkType)> {
    let class = crate::types::dbf_link_class(record_type, upper_field)?;
    let expected = declared_link_type(record_type, dtyp, upper_field)?;
    let parsed = parse_link_field(text, LinkFieldType::for_class(class)).db_link_type();
    (!expected.accepts(parsed)).then_some((expected, parsed))
}

/// C's `dbInitRecordLinks` refusal (`dbStaticLib.c:2223-2224`) minus the
/// severity word, which the caller supplies. `None` when the link fits.
///
/// This is the `iocInit` half: C asks `dbCanSetLink` once, over the record as
/// loaded, which is why a `.db` may spell `INP` before `DTYP` and still bind.
pub fn link_type_refusal(
    record_name: &str,
    record_type: &str,
    dtyp: Option<&str>,
    upper_field: &str,
    text: &str,
) -> Option<String> {
    let mismatch = link_type_mismatch(record_type, dtyp, upper_field, text)?;
    Some(refusal_line(record_name, upper_field, text, mismatch))
}

/// The wording, owned once. `subject` is what C prints in front of the field
/// name: the record's NAME at `iocInit`, which is the only place C prints this
/// line at all. The runtime seam ([`check_link_assignment`]) passes the record
/// TYPE instead — `RecordInstance` stores no name, and C prints nothing there
/// in any case (`dbPutFieldLink` returns `S_dbLib_badField` silently,
/// `dbAccess.c:1132-1135`), so that text is the port's own diagnostic and not
/// a line any C IOC emits.
fn refusal_line(
    subject: &str,
    upper_field: &str,
    text: &str,
    (expected, parsed): (DbLinkType, DbLinkType),
) -> String {
    format!(
        "{subject}.{upper_field}: can't initialize link type {} with \"{text}\" (type {})",
        expected.c_name(),
        parsed.c_name(),
    )
}

/// C `dbPutFieldLink`'s gate (`dbAccess.c:1094-1135`): a link written at
/// RUNTIME is parsed and then held to `dbCanSetLink`, and a failure of either
/// refuses the put with the link left exactly as it was. Both halves in one
/// call because that seam wants both; the db-load path takes
/// [`check_link_text`] alone and the `iocInit` pass [`link_type_refusal`]
/// alone.
pub fn check_link_assignment(
    record_type: &str,
    dtyp: Option<&str>,
    upper_field: &str,
    text: &str,
) -> CaResult<()> {
    check_link_text(record_type, upper_field, text)?;
    match link_type_mismatch(record_type, dtyp, upper_field, text) {
        None => Ok(()),
        Some(mismatch) => Err(CaError::InvalidValue(refusal_line(
            record_type,
            upper_field,
            text,
            mismatch,
        ))),
    }
}

impl ParsedLink {
    /// A DB link, shared behind the `Arc` [`ParsedLink::Db`] carries.
    pub fn db(link: DbLink) -> Self {
        ParsedLink::Db(std::sync::Arc::new(link))
    }

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
    /// Total, because [`HwLinkKind`] now names every bus C's identifier table
    /// can produce: a `#` form outside that table is a parse failure, not a
    /// link of unknown type, so there is no case left that cannot answer.
    #[must_use]
    pub fn db_link_type(&self) -> DbLinkType {
        match self {
            ParsedLink::None | ParsedLink::Constant(_) => DbLinkType::Constant,
            ParsedLink::Removed(ty) => *ty,
            ParsedLink::Db(_) | ParsedLink::Ca(_) | ParsedLink::Pva(_) => DbLinkType::PvLink,
            ParsedLink::PvaJson(_) | ParsedLink::Calc(_) | ParsedLink::State(_) => {
                DbLinkType::JsonLink
            }
            ParsedLink::Hw(hw) => hw.kind().db_link_type(),
        }
    }

    /// C `plink->type` AFTER `dbInitLink` has run (`dbLink.c:92-130`) — the
    /// identity `dbpr` prints in front of a link field's text
    /// (`dbTest.c:1205-1224`), as opposed to the static parse
    /// [`Self::db_link_type`] gives.
    ///
    /// `self` must already have been through the database's locality
    /// fallthrough (`PvDatabase::db_init_link_locality`), which is where a
    /// `PV_LINK` naming no local record becomes a CA channel. What is left
    /// for this function is the other half of C's `dbInitLink` test: a link
    /// carrying `CA`, `CP` or `CPP` never reaches `dbDbInitLink`
    /// (`dbAccess.c:1104`), so C makes a `CP` link to a LOCAL record a
    /// `CA_LINK` too. This port keeps such a link on the DB link set on
    /// purpose (see `db_init_link_locality`), so the *identity* is restored
    /// here rather than by moving the link, and `dbpr` prints the word C
    /// prints without the behaviour changing.
    ///
    /// `raw` is the link's stored text, and the JSON test runs on it rather
    /// than on the variant because C's does: `dbParseLink` decides
    /// `JSON_LINK` from the braces alone and never looks at what the JSON
    /// evaluates to (`dbStaticLib.c:2280-2287`). This port instead resolves
    /// the JSON to the link it describes, so `{const:1}` arrives here as
    /// [`ParsedLink::Constant`] and `{pva:"X"}` as [`ParsedLink::Pva`] — the
    /// same two variants a bare `1` and this port's `pva://X` scheme
    /// extension produce. Nothing in the parse tree distinguishes them
    /// afterwards; the text does.
    pub fn link_type_after_init(&self, raw: &str, ftype: LinkFieldType) -> DbLinkType {
        if matches!(parse_json_link(raw.trim(), ftype), JsonLinkParse::Parsed(_)) {
            return DbLinkType::JsonLink;
        }
        match self {
            // C `dbParseLink` makes the empty string a CONSTANT
            // (`dbStaticLib.c:2346-2349`), and a link field the `.db` never
            // mentions is left CONSTANT by `dbInitRecordLinks` before the
            // parse loop is even entered (`dbStaticLib.c:2189-2214`).
            ParsedLink::None | ParsedLink::Constant(_) => DbLinkType::Constant,
            // Only reachable from the JSON longhand, which the brace test
            // above has already claimed; kept exhaustive rather than
            // unreachable so a future non-JSON spelling cannot fall into the
            // `Constant` arm unnoticed.
            ParsedLink::PvaJson(_) | ParsedLink::Calc(_) | ParsedLink::State(_) => {
                DbLinkType::JsonLink
            }
            // `pva://X` — this port's scheme extension, which C would parse
            // as a `PV_LINK` naming no local record.
            ParsedLink::Pva(_) | ParsedLink::Ca(_) => DbLinkType::CaLink,
            ParsedLink::Db(link) => {
                if link.policy.cp_passive_only().is_some() {
                    DbLinkType::CaLink
                } else {
                    DbLinkType::DbLink
                }
            }
            // `dbInitLink` leaves a hardware link's type exactly as the parse
            // set it (`dbLink.c:92-130` only re-types `PV_LINK`), so the bus
            // the identifier letters named is what `dbpr` prints.
            ParsedLink::Hw(hw) => hw.kind().db_link_type(),
            ParsedLink::Removed(ty) => *ty,
        }
    }

    /// The discriminated [`LinkType`] of this link — the C
    /// `prec->xxx.type` analogue. See [`LinkType`] for the C mapping.
    pub fn link_type(&self) -> LinkType {
        match self {
            // A removed link, like an empty one, has no value and no target.
            ParsedLink::None | ParsedLink::Removed(_) => LinkType::Empty,
            ParsedLink::Constant(_) => LinkType::Constant,
            ParsedLink::Db(_) => LinkType::Db,
            ParsedLink::Ca(_) | ParsedLink::Pva(_) | ParsedLink::PvaJson(_) => LinkType::Ca,
            ParsedLink::Hw(_) | ParsedLink::Calc(_) | ParsedLink::State(_) => LinkType::Other,
        }
    }

    /// The constant's value — see [`ConstantLink::new`].
    pub fn constant_value(&self) -> Option<EpicsValue> {
        if let ParsedLink::Constant(c) = self {
            Some(c.value().clone())
        } else {
            None
        }
    }

    /// A CONSTANT link holding `text`.
    pub fn constant(text: impl Into<String>) -> Self {
        ParsedLink::Constant(std::sync::Arc::new(ConstantLink::new(text)))
    }

    /// A `{const: <value>}` link, `value` being the JSON text. A quoted string
    /// is a yajl string — its escapes are decoded here (`lnkConst.c:199`
    /// receives the DECODED bytes from `yajl_parser.c:273-281`); a bare token
    /// is taken verbatim, as is an array. `{const:""}` is a constant too: its
    /// `sc40` loads like any string (`cvt_st_d` of "" is 0, a CHAR array gets
    /// the lone NUL), unlike an empty CONSTANT link `dbConstLoad*` refuses.
    pub fn json_constant(value: &str) -> Self {
        let decoded = decode_json_string_token(value).unwrap_or_else(|| value.to_string());
        let json = serde_json::from_str(value)
            .ok()
            .and_then(|v| LnkConst::of(&v));
        let mut constant = ConstantLink::new(decoded);
        constant.json = json;
        ParsedLink::Constant(std::sync::Arc::new(constant))
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
            ParsedLink::Db(_)
                | ParsedLink::Ca(_)
                | ParsedLink::Pva(_)
                | ParsedLink::PvaJson(_)
                // The set is "the link's lset implements `putValue`". C
                // `lnkStateIf` (`lnkState.c:226-232`) reaches `lnkState_lset`
                // (`:205-216`), whose `putValue` is `lnkState_putValue`
                // (`:153-203`); `lnkCalcIf`'s lset leaves the slot NULL, which
                // is why calc is a readable link and state is both.
                | ParsedLink::State(_)
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
    /// carry that identity (the resolver shares the channel by
    /// `(pv, pipeline, queue_size)`, not by bare PV, matching pvxs, whose
    /// channel cache is keyed by `(channelName, printed pvRequest)` —
    /// `ioc/pvalink.h:65,116`). Callers feed the result to the
    /// link-set, which is the only consumer; nothing relies on this
    /// returning the bare PV name.
    pub fn external_pv_name(&self) -> Option<Cow<'_, str>> {
        match self {
            ParsedLink::Ca(ca) => Some(Cow::Borrowed(ca.pv.as_str())),
            // Scheme-qualified, because these two variants KNOW their scheme
            // and the boundary must not lose it: `split_external_link_name`
            // reads the prefix back into `LinkTarget::Scheme("pva")`, and
            // without it a pva link addressed by its bare name resolves to
            // `LinkTarget::Any` — every registered lset in turn, first to
            // answer wins. Measured on a database holding only a `ca` link
            // set: `INPA="{pva:{pv:\"SRC\"}}"` was read from the CA link set.
            // The lset itself still sees the unprefixed body, which is what
            // `split_external_link_name` hands it.
            ParsedLink::Pva(name) => Some(Cow::Owned(format!("pva://{name}"))),
            ParsedLink::PvaJson(j) => Some(Cow::Owned(format!(
                "pva://{key}",
                key = j.link_identity_key()
            ))),
            _ => None,
        }
    }

    /// The link-set scheme that must be REGISTERED on the database before
    /// this link can be serviced at all, or `None` when nothing needs to be
    /// installed for it.
    ///
    /// Parsing a link and being able to service it are two different facts,
    /// and this is the second one. The first is `{pva:…}` loading at all,
    /// which needs the database to declare the type ([`LinkList`]) as C needs
    /// `pvxs7x.dbd`'s `link()` line. A declared type can still reach iocInit
    /// with no link set installed, and so can `pva://`, which C has no
    /// `link()` gate for; `PvDatabase::db_init_link_locality` asks this and
    /// refuses both rather than letting the lset dispatch hand the bare name
    /// to whatever OTHER lset is registered.
    ///
    /// `Ca` deliberately answers `None`. The parse erases the difference
    /// between `ca://X`, `{ca:{pv:"X"}}`, the `X CA` modifier and the
    /// `dbInitLink` non-local fallthrough — all four are `Ca(CaLink{pv:"X"})`
    /// — and the last two must keep working with no lset, through the legacy
    /// [`ExternalPvResolver`](crate::server::database::ExternalPvResolver).
    /// There is no C rule to match either: `ca` has no `link()` line in base
    /// or in pvxs, so it is this port's own extension.
    pub fn required_link_set_scheme(&self) -> Option<&'static str> {
        match self {
            // Produced only by `pva://…` and `{pva:…}` — see the `pva` row of
            // `IMPLEMENTED_JSON_LINK_TYPES`, whose `lsetPVX` is what
            // `epics-bridge-rs` installs as the `"pva"` link set.
            ParsedLink::Pva(_) | ParsedLink::PvaJson(_) => Some("pva"),
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
/// (pvxs/ioc/pvalink_jlif.cpp:74-100,143-154).
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
    // C hands the WHOLE bracketed text to `dbPutConvertJSON`
    // (`dbConstLink.c:207`), which is the same converter `dbpf` calls — so
    // the two paths accept and refuse the same literals, and a quoted
    // element's escapes are decoded by that one parser rather than by a
    // second comma-splitter here.
    let json = format!("[{inner}]");
    // C passes the record's `FTVL`; this port types the value afterwards
    // (see `ConstantLink::new`), so the two element kinds C's
    // callbacks admit are tried in turn — a string element is legal only
    // into `DBF_STRING` (`dbConvertJSON.c:78-82`), which is exactly the
    // "all numeric, else strings" split this used to reach by parsing twice.
    use crate::types::DbFieldType;
    for target in [DbFieldType::Double, DbFieldType::String] {
        if let Ok(v) =
            crate::server::db_convert_json::db_put_convert_json(&json, target, usize::MAX)
        {
            return v;
        }
    }
    // C errlogs and returns `S_db_badField`, leaving `dbLoadLinkArray` to
    // report the link uninitialized so the field keeps its default
    // (`dbConstLink.c:208-211`). Zero elements is that state; the diagnostic
    // line has no route out of this signature.
    EpicsValue::DoubleArray(Vec::new().into())
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
    /// LEN 1, UDF 0). An empty `[]` ends the same way.
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
    //
    // Through the dialect first, as `parse_json_link` does. C reaches this
    // text through `dbJLinkParse` → yajl (`dbJLink.c:402-406`), so the link
    // body is already comment-free and its keys already lexed by the time
    // `lnkConst`'s callbacks see it; reading the raw `.db` bytes here meant
    // `{const: /*why*/ "hi"}` loaded nothing at all.
    if s.starts_with('{') && s.ends_with('}') {
        let normalized = crate::json5::relaxed_to_strict(s).ok()?;
        let value = json_const_value(&normalized)?;
        return json_string_value(value).map(LsLoad::Text);
    }
    // Plain link: `loadLS` exists only on the CONSTANT lset (a number or a
    // `[…]`, `dbStaticLib.c:2346-2356`), which hands the text to
    // `dbLSConvertJSON` whatever it is.
    let bracketed = s.starts_with('[') && s.ends_with(']');
    if !bracketed && parse_c_double(s).is_none() {
        return None;
    }
    match crate::server::db_convert_json::db_ls_convert_json(s).ok()? {
        Some(text) => Some(LsLoad::Text(text.as_str_lossy().into_owned())),
        None => Some(LsLoad::LenOnly),
    }
}

/// The raw (un-dequoted) value text of a `{const: …}` JSON link, or `None` when
/// `s` is some other JSON link.
pub(crate) fn json_const_value(s: &str) -> Option<&str> {
    let inner = s[1..s.len() - 1].trim_start();
    let (key, rest) = inner.split_once(':')?;
    let key = key.trim().trim_matches('"');
    if key != "const" {
        return None;
    }
    Some(rest.trim().trim_end_matches(',').trim())
}

/// The JSON string a `{const:…}` link carries, DECODED — `"hi"` and the
/// string-array shorthand `["hi", …]` (C `ac40`, whose `loadLS` takes element 0).
/// A numeric value is not a string: `lnkConst_loadLS`'s `default` arm returns
/// `S_db_badField`, so nothing is loaded and LEN stays 0 — and so does an empty
/// `[]` (`a0`).
///
/// The decode is yajl's, not this function's: a `{…}` value's escapes are
/// translated by `dbJLinkParse` → yajl, never by the `.db` lexer
/// ([`decode_json_string_token`]). An array goes through the same yajl scan,
/// because a comma inside an element (`["a,b"]`) is not a separator.
fn json_string_value(value: &str) -> Option<String> {
    let v = value.trim();
    if v.starts_with('[') {
        let first = crate::server::db_convert_json::db_ls_convert_json(v).ok()??;
        return Some(first.as_str_lossy().into_owned());
    }
    decode_json_string_token(v)
}

/// The JSON link types this port has a jlif for — what a C binary has LINKED,
/// as `(key, jlif)`: the `jlif` objects `registerRecordDeviceDriver` can hand
/// `registryJLinkAdd`. Base's `const`/`calc`/`state` are
/// `links.dbd.pod:39,79,171`, and `pva` is pvxs's `link("pva", "lsetPVX")`
/// (`ioc/pvxs7x.dbd:2`).
///
/// `ca` has no `link()` line in base or pvxs — it is this port's own
/// extension, kept because `.db` files in this workspace already use it, and
/// `lnkCaIf` is therefore this port's own symbol name rather than one quoted
/// from upstream.
///
/// Whether a database then FINDS a type is not this table's question: that is
/// the database's [`LinkList`].
pub(crate) const IMPLEMENTED_JSON_LINK_TYPES: [(&str, &str); 5] = [
    ("const", "lnkConstIf"),
    ("calc", "lnkCalcIf"),
    ("state", "lnkStateIf"),
    ("pva", "lsetPVX"),
    ("ca", "lnkCaIf"),
];

/// The base types every database starts with: the build's `link()` lines
/// (`softIoc.dbd`), which `registerAllRecordDeviceDrivers` binds before any
/// `.db` loads. In `dbdExpand.pl`'s order — sorted by jlif
/// (`DBD/Output.pm:92-95`). `pva` is not one: a binary adds it.
const BASE_JSON_LINK_TYPES: [&str; 4] = ["ca", "calc", "const", "state"];

/// One `linkSup` node: C keeps `name` and `jlif_name` side by side
/// (`dbLexRoutines.c:883-900`) and `dbDumpLink` prints both
/// (`dbStaticLib.c:1099-1102`). Both strings are declaration TEXT, stored
/// verbatim; `bound` is `plinkSup->pjlif != NULL`.
#[derive(Clone)]
pub(crate) struct LinkSup {
    pub(crate) key: String,
    pub(crate) jlif_name: String,
    bound: bool,
}

/// A database's `pdbbase->linkList`, in insertion order — `dbLinkType`
/// `ellAdd`s and `dbWriteLinkFP` walks the list unsorted.
///
/// It belongs to the database, not the process: two databases in one process
/// (every test, every builder) must not see each other's declarations.
#[derive(Clone)]
pub(crate) struct LinkList {
    sups: Vec<LinkSup>,
}

impl LinkList {
    /// What a database holds before its first `dbLoadDatabase`.
    pub(crate) fn base() -> Self {
        let mut list = Self { sups: Vec::new() };
        for key in BASE_JSON_LINK_TYPES {
            let jlif = implemented_jlif(key).expect("a base type is implemented");
            list.declare(key, jlif);
            list.bind(key);
        }
        list
    }

    /// C `dbLinkType` (`dbLexRoutines.c:883-900`): a key already in the list
    /// is dropped, anything else appends.
    pub(crate) fn declare(&mut self, key: &str, jlif_name: &str) {
        if self.find(key).is_none() {
            self.sups.push(LinkSup {
                key: key.to_string(),
                jlif_name: jlif_name.to_string(),
                bound: false,
            });
        }
    }

    /// C `registryJLinkAdd` (`registryJLinks.c:16-23`): binds support only to
    /// a type the list already declares, and says whether it did.
    pub(crate) fn bind(&mut self, key: &str) -> bool {
        match self.sups.iter_mut().find(|s| s.key == key) {
            Some(sup) => {
                sup.bound = true;
                true
            }
            None => false,
        }
    }

    pub(crate) fn entries(&self) -> &[LinkSup] {
        &self.sups
    }

    fn find(&self, key: &str) -> Option<&LinkSup> {
        // `dbFindLinkSup` is a `gphFind`: the name matches case-sensitively.
        self.sups.iter().find(|s| s.key == key)
    }
}

/// The `jlif` this port carries for `key`, if it carries one.
pub(crate) fn implemented_jlif(key: &str) -> Option<&'static str> {
    IMPLEMENTED_JSON_LINK_TYPES
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, jlif)| *jlif)
}

/// Who answers C's `dbFindLinkSup` for one parse.
#[derive(Clone, Copy)]
pub(crate) enum LinkLookup<'a> {
    /// A database's `linkList`: the load and put gates of that database.
    Database(&'a LinkList),
    /// No database — the text is re-read after its database admitted it, or
    /// built by a caller that has none. Only whether this port implements
    /// the type is asked.
    Implemented,
}

/// JSON link types epics-base registers that this port has not implemented.
/// They are rejected like an unknown type — the point of rejecting at all is
/// that a missing implementation must be visible rather than silently
/// degrading into a PV-name link — but the diagnostic says which it is.
/// `debug` and `trace` are lset WRAPPERS: their jlif takes a child link and
/// their lset forwards every call to the child's after logging
/// (`lnkDebug.c` / `lnkTrace.c`). This port dispatches a link by its
/// [`ParsedLink`] variant rather than through a per-link lset vtable, so
/// there is nothing for a wrapper to substitute itself into — porting them
/// needs the vtable first, not a new variant.
const UNPORTED_C_JSON_LINK_TYPES: &[&str] = &["debug", "trace"];

/// What a link field's text is, read as a JSON link — the port's
/// `dbJLinkParse` (`dbJLink.c:379-445`).
///
/// The three states exist because two of them used to be one. A single
/// `Option` conflated "this text is not brace-delimited, try the other link
/// syntaxes" with "this IS a JSON link and it is bad", so the second fell
/// through `parse_link_field`'s remaining tests and came out as a PV link
/// whose name was the JSON text — a channel that can never connect.
pub enum JsonLinkParse {
    /// Not brace-delimited: C's `dbParseLink` never calls `dbJLinkParse`
    /// (`dbStaticLib.c:2280`), so the other link syntaxes still apply.
    NotJson,
    /// A JSON link this port serves.
    Parsed(ParsedLink),
    /// Brace-delimited and unusable. C stops the parse (`jlif_stop`), which
    /// `dbJLinkParse` turns into `S_db_badField` and `dbParseLink` into
    /// `S_dbLib_badField` — the field never loads. The refusal carries the
    /// line C's `dbJLinkInit` prints, when C prints one.
    Rejected(LinkRefusal),
}

/// What the first map key of a brace-delimited link body is — C's `link_name`
/// in `dbjl_map_key` (`dbJLink.c:260`), plus the two ways there isn't one.
enum JsonLinkKey<'a> {
    /// The link type name, unquoted and trimmed.
    Named(&'a str),
    /// An empty map. yajl parses `{}` without error, so `dbJLinkParse` returns
    /// 0 with a NULL `product` and `dbParseLink` stores `JSON_LINK` with no
    /// jlink (`dbStaticLib.c:2281-2285`) — a link that does nothing.
    Empty,
    /// Not JSON yajl would accept, so `dbJLinkParse` comes back
    /// `yajl_status_error` → `S_db_badField` (`dbJLink.c:426-438`).
    Malformed,
}

fn json_link_key(s: &str) -> JsonLinkKey<'_> {
    let inner = s[1..s.len() - 1].trim();
    if inner.is_empty() {
        return JsonLinkKey::Empty;
    }
    let Some((key_raw, _)) = inner.split_once(':') else {
        return JsonLinkKey::Malformed;
    };
    let key = key_raw.trim().trim_matches('"');
    if key.is_empty() {
        JsonLinkKey::Malformed
    } else {
        JsonLinkKey::Named(key)
    }
}

/// The first map key of `prefix`, the text of a link up to where the parse
/// stopped, if a whole key and its `:` precede that point.
fn key_before(prefix: &str) -> Option<String> {
    let normalized = crate::json5::relaxed_to_strict(prefix).ok()?;
    let (key_raw, _) = normalized.strip_prefix('{')?.split_once(':')?;
    let key = key_raw.trim().trim_matches('"');
    (!key.is_empty()).then(|| key.to_string())
}

/// The refusal for a link type key `lookup` does not serve — C `dbjl_map_key`
/// (`dbJLink.c:262-275`): a key the list lacks, then a key with no support.
fn unknown_link_type(key: impl AsRef<str>, lookup: LinkLookup<'_>) -> Option<LinkRefusal> {
    let key = key.as_ref();
    if UNPORTED_C_JSON_LINK_TYPES.contains(&key) {
        return Some(LinkRefusal::new(LinkWhy::Unported(
            crate::server::recgbl::ReportString::new(key),
        )));
    }
    let bound = match lookup {
        LinkLookup::Database(list) => list.find(key).map(|sup| sup.bound),
        LinkLookup::Implemented => implemented_jlif(key).map(|_| true),
    };
    let key = crate::server::recgbl::ReportString::new(key);
    match bound {
        None => Some(LinkRefusal::new(LinkWhy::NotFound(key))),
        Some(false) => Some(LinkRefusal::new(LinkWhy::NotLoaded(key))),
        Some(true) => None,
    }
}

/// Read a link field's text as a JSON link — the single owner of "is this a
/// JSON link, and is it usable".
///
/// C `dbParseLink` (`dbStaticLib.c:2280-2286`) treats ANY brace-delimited
/// link text as JSON with no escape hatch, and hands it to `dbJLinkParse`;
/// an unregistered first key is `errlogPrintf("dbJLinkInit: Link type '%s'
/// not found")` plus `jlif_stop` (`dbJLink.c:262-268`), and a registered
/// type whose own callbacks refuse the value stops the parse the same way.
/// Both come back here as [`JsonLinkParse::Rejected`], so brace-delimited
/// text never reaches PV-name parsing on any path.
pub fn parse_json_link(s: &str, ftype: LinkFieldType) -> JsonLinkParse {
    parse_json_link_in(s, ftype, LinkLookup::Implemented)
}

/// [`parse_json_link`] with `dbFindLinkSup` answered by `lookup`, for every
/// key the text names — the first and each embedded one.
pub(crate) fn parse_json_link_in(
    s: &str,
    ftype: LinkFieldType,
    lookup: LinkLookup<'_>,
) -> JsonLinkParse {
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') || s.len() < 2 {
        return JsonLinkParse::NotJson;
    }
    if let Some(refusal) = jlink_syntax_refusal(s, ftype, lookup) {
        return JsonLinkParse::Rejected(refusal);
    }
    // Read the link type from the NORMALIZED text. A comment is whitespace in
    // this dialect, so `{/*which*/calc:{…}}` names the type `calc`, but
    // `json_link_key` splits on the first `:` and would take the comment as
    // part of the key.
    let normalized = match crate::json5::relaxed_to_strict(s) {
        Ok(normalized) => normalized,
        Err(e) => {
            // yajl hands `dbjl_map_key` the key before it reaches the
            // unterminated comment, so an unknown type is reported first.
            let crate::json5::Json5Error::UnterminatedBlockComment { offset } = e;
            if let Some(refusal) =
                key_before(&s[..offset]).and_then(|k| unknown_link_type(k, lookup))
            {
                return JsonLinkParse::Rejected(refusal);
            }
            return JsonLinkParse::Rejected(LinkRefusal::new(LinkWhy::UnterminatedComment(offset)));
        }
    };
    let key = match json_link_key(&normalized) {
        JsonLinkKey::Named(key) => key,
        JsonLinkKey::Empty => return JsonLinkParse::Parsed(ParsedLink::None),
        JsonLinkKey::Malformed => {
            return JsonLinkParse::Rejected(LinkRefusal::new(LinkWhy::NoTypeKey));
        }
    };
    if let Some(refusal) = unknown_link_type(key, lookup) {
        return JsonLinkParse::Rejected(refusal);
    }
    // The body reader splits on `:` and matches barewords too, so it needs the
    // same normalized text.
    match try_parse_json_link_body(&normalized, lookup) {
        Some(parsed) => JsonLinkParse::Parsed(parsed),
        // DEVIATION, deliberate. A registered type whose jlif merely IGNORES a
        // token it does not want (pvxs `pva_parse_integer` at
        // `pvxs/ioc/pvalink_jlif.cpp:123-141` logs and returns `jlif_continue`) loads in
        // C as a link with no channel name. The port cannot build the link the
        // text asked for, and the only other outcome reachable from here is the
        // PV-name link this whole function exists to prevent, so it refuses and
        // says which type refused.
        None => JsonLinkParse::Rejected(LinkRefusal::new(LinkWhy::ValueRejected(
            crate::server::recgbl::ReportString::new(key),
        ))),
    }
}

/// The link field type `dbjl_start_map` hands the next key's `alloc_jlink`
/// (`parser->dbfType`, `dbJLink.c:197-209`).
type ChildSlot = Option<LinkFieldType>;

/// What a `parse_start_map` answers (`jlif_key_result`, `dbJLink.h`).
enum MapStart {
    Stop,
    Continue,
    Child(LinkFieldType),
}

/// The one refusal a callback records before it stops the parse.
type Said = Option<LinkRefusal>;

/// One open jlink on `dbjl`'s stack: its type's key, its parse callbacks and
/// its `parseDepth`.
struct Jlink {
    key: crate::server::recgbl::ReportString,
    jlif: Jlif,
    depth: u32,
}

/// The parse halves of the registered `jlif`s. A callback slot C leaves NULL
/// is `CALL_OR_STOP`, a stop with no word (`dbJLink.c:58`).
enum Jlif {
    Const(LnkConstParse),
    Calc(LnkCalcParse),
    /// `lnkStateIf` fills only `parse_string` (`lnkState.c:226-232`).
    State,
    /// A type whose callbacks the port does not model (pvxs `pva`, `ca`):
    /// its value is only checked as JSON.
    Opaque,
}

impl Jlif {
    /// `pjlif->alloc_jlink(parser->dbfType)`: the refusals that depend on the
    /// link field's type (`lnkConst.c:50-53`, `lnkCalc.c:81-84`,
    /// `lnkState.c:54-57`) say their own line.
    fn alloc(key: &str, ftype: LinkFieldType) -> Result<Self, LinkRefusal> {
        let failed = |key, said| Err(LinkRefusal::new(LinkWhy::AllocFailed { key, said }));
        match (key, ftype) {
            ("const", LinkFieldType::Out | LinkFieldType::Fwd) => {
                failed("const", "lnkConst: Only works with input links")
            }
            ("calc", LinkFieldType::Fwd) => failed("calc", "lnkCalc: No support for forward links"),
            ("state", LinkFieldType::Fwd) => failed("state", "lnkState: DBF_FWDLINK not supported"),
            ("const", _) => Ok(Self::Const(LnkConstParse {
                shape: ConstShape::S0,
            })),
            ("calc", _) => Ok(Self::Calc(LnkCalcParse::new(ftype))),
            ("state", _) => Ok(Self::State),
            _ => Ok(Self::Opaque),
        }
    }

    fn null(&mut self) -> bool {
        matches!(self, Self::Opaque)
    }

    fn boolean(&mut self, said: &mut Said) -> bool {
        match self {
            Self::Const(c) => c.number(said),
            Self::Opaque => true,
            Self::Calc(_) | Self::State => false,
        }
    }

    fn integer(&mut self, value: i64, said: &mut Said) -> bool {
        match self {
            Self::Const(c) => c.number(said),
            Self::Calc(c) => c.integer(value, said),
            Self::Opaque => true,
            Self::State => false,
        }
    }

    fn double(&mut self, value: f64, said: &mut Said) -> bool {
        match self {
            Self::Const(c) => c.number(said),
            Self::Calc(c) => c.double(value, said),
            Self::Opaque => true,
            Self::State => false,
        }
    }

    fn string(&mut self, bytes: &[u8], said: &mut Said) -> bool {
        match self {
            Self::Const(c) => c.string(said),
            Self::Calc(c) => c.string(bytes, said),
            Self::State | Self::Opaque => true,
        }
    }

    fn start_map(&mut self, said: &mut Said) -> MapStart {
        match self {
            Self::Calc(c) => c.start_map(said),
            Self::Opaque => MapStart::Continue,
            Self::Const(_) | Self::State => MapStart::Stop,
        }
    }

    fn map_key(&mut self, key: &[u8], said: &mut Said) -> bool {
        match self {
            Self::Calc(c) => c.map_key(key, said),
            Self::Opaque => true,
            Self::Const(_) | Self::State => false,
        }
    }

    fn end_map(&mut self, said: &mut Said) -> bool {
        match self {
            Self::Calc(c) => c.end_map(said),
            Self::Opaque => true,
            Self::Const(_) | Self::State => false,
        }
    }

    fn start_array(&mut self, said: &mut Said) -> bool {
        match self {
            Self::Const(c) => c.start_array(said),
            Self::Calc(c) => c.start_array(said),
            Self::Opaque => true,
            Self::State => false,
        }
    }

    fn end_array(&mut self) -> bool {
        match self {
            Self::Calc(c) => c.end_array(),
            Self::Const(_) | Self::Opaque => true,
            Self::State => false,
        }
    }

    /// `parent->pif->end_child`, which only lnkCalc fills.
    fn end_child(&mut self, said: &mut Said) {
        if let Self::Calc(c) = self {
            c.end_child(said);
        }
    }
}

/// C `dbjl_callbacks` (`dbJLink.c:56-376`): the stack of open jlinks, each
/// fed by its own type's callbacks, a child allocated with the link field
/// type its parent's `parse_start_map` asked for.
struct Dbjl<'a> {
    lookup: LinkLookup<'a>,
    stack: Vec<Jlink>,
    slot: ChildSlot,
    /// What stopped the parse, if a callback or `dbjl` itself said why.
    said: Said,
}

impl Dbjl<'_> {
    /// `dbjl_value`: a value that leaves its link at depth 0 completes it,
    /// and the parent's `end_child` takes it.
    fn value(&mut self, ok: bool) -> bool {
        if !ok {
            return false;
        }
        if self.stack.last().is_some_and(|top| top.depth == 0) {
            self.stack.pop();
            if let Some(parent) = self.stack.last_mut() {
                parent.jlif.end_child(&mut self.said);
            }
        }
        true
    }

    fn top(&mut self) -> Option<(&mut Jlink, &mut Said)> {
        self.stack.last_mut().map(|top| (top, &mut self.said))
    }
}

impl crate::server::db_convert_json::Callbacks for Dbjl<'_> {
    fn null(&mut self) -> bool {
        let ok = self.top().is_some_and(|(top, _)| top.jlif.null());
        self.value(ok)
    }
    fn boolean(&mut self, _value: bool) -> bool {
        let ok = self.top().is_some_and(|(top, said)| top.jlif.boolean(said));
        self.value(ok)
    }
    fn integer(&mut self, value: i64) -> bool {
        let ok = self
            .top()
            .is_some_and(|(top, said)| top.jlif.integer(value, said));
        self.value(ok)
    }
    fn double(&mut self, value: f64) -> bool {
        let ok = self
            .top()
            .is_some_and(|(top, said)| top.jlif.double(value, said));
        self.value(ok)
    }
    fn string(&mut self, bytes: &[u8]) -> bool {
        let ok = self
            .top()
            .is_some_and(|(top, said)| top.jlif.string(bytes, said));
        self.value(ok)
    }
    fn start_map(&mut self) -> bool {
        let Some((top, said)) = self.top() else {
            return true;
        };
        top.depth += 1;
        match top.jlif.start_map(said) {
            MapStart::Stop => false,
            MapStart::Continue => true,
            MapStart::Child(ftype) => {
                self.slot = Some(ftype);
                true
            }
        }
    }
    fn map_key(&mut self, key: &[u8]) -> bool {
        let Some(ftype) = self.slot.take() else {
            return match self.top() {
                Some((top, said)) => top.jlif.map_key(key, said),
                None => {
                    let key = String::from_utf8_lossy(key);
                    self.said = Some(LinkRefusal::new(LinkWhy::SecondKey(
                        crate::server::recgbl::ReportString::new(&key),
                    )));
                    false
                }
            };
        };
        let key = String::from_utf8_lossy(key);
        if let Some(refusal) = unknown_link_type(&*key, self.lookup) {
            self.said = Some(refusal);
            return false;
        }
        match Jlif::alloc(&key, ftype) {
            Ok(jlif) => {
                let key = crate::server::recgbl::ReportString::new(&key);
                self.stack.push(Jlink {
                    key,
                    jlif,
                    depth: 0,
                });
                true
            }
            Err(refusal) => {
                self.said = Some(refusal);
                false
            }
        }
    }
    fn end_map(&mut self) -> bool {
        let Some((top, said)) = self.top() else {
            return true;
        };
        top.depth -= 1;
        let ok = top.jlif.end_map(said);
        self.value(ok)
    }
    fn start_array(&mut self) -> bool {
        self.top().is_some_and(|(top, said)| {
            top.depth += 1;
            top.jlif.start_array(said)
        })
    }
    fn end_array(&mut self) -> bool {
        let ok = self.top().is_some_and(|(top, _)| {
            top.depth -= 1;
            top.jlif.end_array()
        });
        self.value(ok)
    }
}

/// C `dbJLinkParse` (`dbJLink.c:379-445`) as a gate: text yajl rejects is
/// reported with yajl's rendered error; a callback's stop prints only what
/// the callbacks said.
fn jlink_syntax_refusal(
    s: &str,
    ftype: LinkFieldType,
    lookup: LinkLookup<'_>,
) -> Option<LinkRefusal> {
    use crate::server::db_convert_json::{Parser, Status};

    let txt = s.as_bytes();
    let mut parser = Parser::new(Dbjl {
        lookup,
        stack: Vec::new(),
        slot: Some(ftype),
        said: None,
    });
    let mut status = parser.parse(txt);
    if status == Status::Ok {
        status = parser.finish();
    }
    match status {
        Status::Ok => None,
        // A stop with no word is still a refusal: the top link's type
        // refused its value, or there was no link to take it.
        Status::ClientCanceled => Some(parser.ctx.said.take().unwrap_or_else(|| {
            LinkRefusal::new(match parser.ctx.stack.last() {
                Some(top) => LinkWhy::ValueRejected(top.key),
                None => LinkWhy::NoTypeKey,
            })
        })),
        Status::Error => Some(LinkRefusal::new(LinkWhy::Yajl(parser.error(txt)))),
    }
}

/// The shape C `lnkConst`'s callbacks (`lnkConst.c:91-262`) have built so
/// far: its `type` with the element kind folded to number or string, which is
/// all that decides a refusal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConstShape {
    S0,
    Scalar,
    A0,
    Numbers,
    Strings,
}

/// `lnkConstIf`'s parse half. A boolean is `lnkConst_integer`; `parse_null`
/// and `parse_start_map` are NULL, so they stop without a word.
struct LnkConstParse {
    shape: ConstShape,
}

impl LnkConstParse {
    fn mixed(said: &mut Said) -> bool {
        *said = Some(LinkRefusal::new(LinkWhy::ConstStop(Some(
            "lnkConst: Mixed data types in array\n",
        ))));
        false
    }

    fn number(&mut self, said: &mut Said) -> bool {
        match self.shape {
            ConstShape::S0 => self.shape = ConstShape::Scalar,
            ConstShape::A0 | ConstShape::Numbers => self.shape = ConstShape::Numbers,
            ConstShape::Strings => return Self::mixed(said),
            ConstShape::Scalar => return false,
        }
        true
    }

    fn string(&mut self, said: &mut Said) -> bool {
        match self.shape {
            ConstShape::S0 => self.shape = ConstShape::Scalar,
            ConstShape::A0 | ConstShape::Strings => self.shape = ConstShape::Strings,
            ConstShape::Numbers => return Self::mixed(said),
            ConstShape::Scalar => return false,
        }
        true
    }

    fn start_array(&mut self, said: &mut Said) -> bool {
        if self.shape != ConstShape::S0 {
            *said = Some(LinkRefusal::new(LinkWhy::ConstStop(Some(
                "lnkConst: Embedded array value\n",
            ))));
            return false;
        }
        self.shape = ConstShape::A0;
        true
    }
}

/// `calc_link.pstate` (`lnkCalc.c:43-51`), numbered as C prints it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum CalcPs {
    Init = 0,
    Expr,
    Major,
    Minor,
    Args,
    Out,
    Prec,
    Units,
    Time,
    Error,
}

/// `lnkCalcIf`'s parse half (`lnkCalc.c:122-370`): the state it keeps that
/// decides a refusal.
struct LnkCalcParse {
    ftype: LinkFieldType,
    pstate: CalcPs,
    nargs: usize,
    expr: bool,
    major: bool,
    minor: bool,
    units: bool,
    out: bool,
}

impl LnkCalcParse {
    fn new(ftype: LinkFieldType) -> Self {
        Self {
            ftype,
            pstate: CalcPs::Init,
            nargs: 0,
            expr: false,
            major: false,
            minor: false,
            units: false,
            out: false,
        }
    }

    /// One more `arg[nArgs++]` / `inp[nArgs++]`, refused at the limit.
    fn take_arg(&mut self, said: &mut Said) -> bool {
        if self.nargs == crate::calc::CALC_NARGS {
            calc_stop(said, CalcSaid::TooManyArgs);
            return false;
        }
        self.nargs += 1;
        true
    }

    fn integer(&mut self, value: i64, said: &mut Said) -> bool {
        match self.pstate {
            CalcPs::Prec => true,
            CalcPs::Args => self.take_arg(said),
            _ => {
                calc_stop(said, CalcSaid::UnexpectedInteger(value));
                false
            }
        }
    }

    fn double(&mut self, value: f64, said: &mut Said) -> bool {
        if self.pstate != CalcPs::Args {
            let g = crate::calc::engine::cvt::fmt_g(value, 6, false, false);
            calc_stop(said, CalcSaid::UnexpectedDouble(ReportString::new(&g)));
            return false;
        }
        self.take_arg(said)
    }

    fn string(&mut self, bytes: &[u8], said: &mut Said) -> bool {
        let text = String::from_utf8_lossy(bytes);
        match self.pstate {
            CalcPs::Units => {
                self.units = true;
                true
            }
            CalcPs::Time => {
                let letter = text.bytes().next().map(|b| b.to_ascii_uppercase());
                match letter {
                    Some(l)
                        if text.len() == 1
                            && l.is_ascii_uppercase()
                            && usize::from(l - b'A') < crate::calc::CALC_NARGS =>
                    {
                        true
                    }
                    _ => {
                        calc_stop(said, CalcSaid::BadTime(ReportString::new(&text)));
                        false
                    }
                }
            }
            // DEVIATION, kept from `json_calc_arg`: C stops on a string here,
            // the port takes it as an embedded link's PV name.
            CalcPs::Args => self.take_arg(said),
            CalcPs::Expr | CalcPs::Major | CalcPs::Minor => {
                match self.pstate {
                    CalcPs::Expr => self.expr = true,
                    CalcPs::Major => self.major = true,
                    _ => self.minor = true,
                }
                match crate::calc::compile(&text) {
                    Ok(_) => true,
                    Err(e) => {
                        let why = crate::calc::calc_error_str(e.code()).unwrap_or("Unknown error");
                        calc_stop(said, CalcSaid::BadExpr(why));
                        false
                    }
                }
            }
            _ => {
                calc_stop(said, CalcSaid::UnexpectedString(ReportString::new(&text)));
                false
            }
        }
    }

    fn start_map(&mut self, said: &mut Said) -> MapStart {
        match self.pstate {
            CalcPs::Args => MapStart::Child(LinkFieldType::In),
            CalcPs::Out => MapStart::Child(LinkFieldType::Out),
            CalcPs::Init => MapStart::Continue,
            _ => {
                calc_stop(said, CalcSaid::UnexpectedMap);
                MapStart::Stop
            }
        }
    }

    fn map_key(&mut self, key: &[u8], said: &mut Said) -> bool {
        let next = match key {
            b"out" if self.ftype == LinkFieldType::Out && !self.out => Some(CalcPs::Out),
            b"expr" if !self.expr => Some(CalcPs::Expr),
            b"args" if self.nargs == 0 => Some(CalcPs::Args),
            b"prec" => Some(CalcPs::Prec),
            b"time" => Some(CalcPs::Time),
            b"major" if !self.major => Some(CalcPs::Major),
            b"minor" if !self.minor => Some(CalcPs::Minor),
            b"units" if !self.units => Some(CalcPs::Units),
            _ => None,
        };
        match next {
            Some(pstate) => {
                self.pstate = pstate;
                true
            }
            None => {
                let key = String::from_utf8_lossy(key);
                calc_stop(said, CalcSaid::UnknownKey(ReportString::new(&key)));
                false
            }
        }
    }

    fn end_map(&mut self, said: &mut Said) -> bool {
        if self.pstate == CalcPs::Error {
            return false;
        }
        if self.ftype == LinkFieldType::In && !self.expr {
            calc_stop(said, CalcSaid::NoExpr);
            return false;
        }
        if self.ftype == LinkFieldType::Out && !self.out {
            calc_stop(said, CalcSaid::NoOut);
            return false;
        }
        true
    }

    fn start_array(&mut self, said: &mut Said) -> bool {
        if self.pstate != CalcPs::Args {
            calc_stop(said, CalcSaid::UnexpectedArray);
            return false;
        }
        true
    }

    fn end_array(&mut self) -> bool {
        self.pstate != CalcPs::Error
    }

    fn end_child(&mut self, said: &mut Said) {
        match self.pstate {
            CalcPs::Args => {
                if !self.take_arg(said) {
                    self.pstate = CalcPs::Error;
                }
            }
            CalcPs::Out => self.out = true,
            ps => {
                calc_stop(said, CalcSaid::UnexpectedChild(ps as i32));
                self.pstate = CalcPs::Error;
            }
        }
    }
}

fn calc_stop(said: &mut Said, why: CalcSaid) {
    *said = Some(LinkRefusal::new(LinkWhy::CalcStop(why)));
}

/// C `dbParseLink`'s JSON arm as a validation gate: `Err` exactly when the
/// text is a brace-delimited link C would refuse to load
/// (`dbStaticLib.c:2281-2283` → `S_dbLib_badField`).
pub fn check_json_link_text(text: &str, ftype: LinkFieldType) -> CaResult<()> {
    check_json_link_text_in(text, ftype, LinkLookup::Implemented)
}

/// [`check_json_link_text`] against `lookup`.
pub(crate) fn check_json_link_text_in(
    text: &str,
    ftype: LinkFieldType,
    lookup: LinkLookup<'_>,
) -> CaResult<()> {
    match parse_json_link_in(text, ftype, lookup) {
        JsonLinkParse::Rejected(refusal) => Err(CaError::BadLink(refusal)),
        JsonLinkParse::NotJson | JsonLinkParse::Parsed(_) => Ok(()),
    }
}

/// C `dbParseLink`'s `#` arm as the same kind of gate: `Err` exactly when the
/// identifier letters name no bus, which is `goto fail` → `S_dbLib_badField`
/// (`dbStaticLib.c:2296-2331`).
///
/// Measured on softIoc R7.0.10, a `.db` carrying `field(INP,"#nonsense")` is
/// refused while the file loads — `Can't set 'H:BAD.INP' to '#nonsense'  :
/// Bad Field value` — and `dbgf H:BAD.INP` then reads back `""`. Without this
/// the port stored the text and served it, because the refused parse comes
/// back as an unset CONSTANT that the device's declared CONSTANT accepts.
fn check_hw_link_text(text: &str) -> CaResult<()> {
    let text = text.trim();
    if text.starts_with('#') && try_parse_hw_link(text).is_none() {
        return Err(CaError::BadLink(LinkRefusal::new(LinkWhy::NoBus)));
    }
    Ok(())
}

/// Read a link body that has ALREADY been through
/// [`crate::json5::relaxed_to_strict`]. Every key here is quoted and every
/// comment is gone, so the `:` split below is safe.
fn try_parse_json_link_body(s: &str, lookup: LinkLookup<'_>) -> Option<ParsedLink> {
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
    let key = key_raw.trim_matches('"');
    match key {
        "const" => Some(ParsedLink::json_constant(rest.trim_end_matches(',').trim())),
        "ca" | "pva" => {
            // pvxs accepts the link value in two forms (pvxs/ioc/pvalink_jlif.cpp:24-31
            // documents the string shorthand; pva_parse_string at :143-149
            // takes a string value at depth 0 as the channel name and a `pv`
            // string inside a map):
            //   shorthand  { pva: "name" }             — string IS the channel
            //              name verbatim; any `?`/`&` in it is link DATA, not
            //              option syntax (pvxs/ioc/pvalink_jlif.cpp:143-149).
            //   longhand   { pva: { pv: "name", ... } } — map with a `pv`
            //              member; the other keys are STRUCTURED JLink options
            //              (pvxs/ioc/pvalink_jlif.cpp:69-196), preserved as such so the
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
        "state" => {
            // `lnkStateIf` fills only the `parse_string` slot
            // (`lnkState.c:226-232`). Every other value kind lands on a NULL
            // callback, and a NULL callback "is equivalent to providing a
            // routine that always returns jlif_stop" (`dbJLink.h:3-5`, via
            // `CALL_OR_STOP`), so `{state:1}` and `{state:{…}}` are refused
            // where `{state:"X"}` loads.
            let value = rest.trim_end_matches(',').trim();
            let text = decode_json_string_token(value)?;
            Some(ParsedLink::State(StateLink::from_link_value(&text)))
        }
        "calc" => {
            // C `lnkCalc_map_key` (`lnkCalc.c:247-296`) knows expr, args,
            // prec, time, major, minor, units and out; this port reads expr,
            // args and time.
            let body = rest.trim();
            if !body.ends_with('}') {
                return None;
            }
            // Already strict: `parse_json_link` normalized the whole text
            // through `crate::json5` before dispatching here.
            let val: serde_json::Value = serde_json::from_str(body).ok()?;
            let obj = val.as_object()?;
            // C `lnkCalc_end_map` (`lnkCalc.c:305-308`): an input link with
            // no `expr` is `jlif_stop`.
            let expr = obj.get("expr").and_then(|v| v.as_str())?.to_string();
            let args = match obj.get("args") {
                None => Vec::new(),
                Some(v) => {
                    // C has no "args must be an array" rule. `lnkCalc_start_array`
                    // (`lnkCalc.c:319-329`) only asserts the parser is in
                    // `ps_args`, and every value handler appends to
                    // `clink->arg` on that state alone —
                    // `lnkCalc_integer:121-144`, `lnkCalc_double`,
                    // `lnkCalc_end_child:341-370`. So a bare `args:5` is one
                    // argument, which is how base's own
                    // `std/rec/linkRetargetLink.db:20-23` spells it; requiring
                    // the brackets refused a link C loads.
                    let elems: &[serde_json::Value] = v
                        .as_array()
                        .map_or(std::slice::from_ref(v), |a| a.as_slice());
                    // The limit is a refusal, not a truncation
                    // (`lnkCalc.c:135-139`, `:155-159`, `:346-350` all
                    // `jlif_stop`). C's own doc says "up to 24"
                    // (`links.dbd.pod:131`) but A..U is 21 letters and
                    // `postfix.h:29` defines 21, so the code wins.
                    if elems.len() > crate::calc::CALC_NARGS {
                        return None;
                    }
                    elems
                        .iter()
                        .map(|v| json_calc_arg(v, lookup))
                        .collect::<Option<Vec<_>>>()?
                }
            };
            // C `lnkCalc_string` at `ps_time` (`lnkCalc.c:180-186`): exactly
            // one character, `toupper`ed, in `'A' ..< 'A' + CALCPERFORM_NARGS`.
            // Anything else is `jlif_stop` — an out-of-range letter refuses
            // the link rather than silently dropping the timestamp source.
            let time_source = match obj.get("time") {
                None => None,
                Some(v) => {
                    let text = v.as_str()?;
                    let mut chars = text.chars();
                    let letter = chars.next()?.to_ascii_uppercase();
                    if chars.next().is_some()
                        || !letter.is_ascii_uppercase()
                        || (letter as u8 - b'A') as usize >= crate::calc::CALC_NARGS
                    {
                        return None;
                    }
                    Some(letter)
                }
            };
            Some(ParsedLink::Calc(CalcLink {
                expr,
                args,
                time_source,
            }))
        }
        _ => None,
    }
}

/// One `args` element: a numeric literal, or an embedded JSON link.
///
/// Embedded links go back through [`parse_json_link_in`], the single owner, so a
/// nested unknown type is refused exactly as a top-level one is.
fn json_calc_arg(v: &serde_json::Value, lookup: LinkLookup<'_>) -> Option<CalcArg> {
    match v {
        serde_json::Value::Number(n) => Some(CalcArg::Literal(n.as_f64()?)),
        // `lnkCalc_start_map` asks for `jlif_key_child_inlink` (`lnkCalc.c:234-235`).
        serde_json::Value::Object(_) => {
            match parse_json_link_in(&serde_json::to_string(v).ok()?, LinkFieldType::In, lookup) {
                JsonLinkParse::Parsed(parsed) => Some(CalcArg::Link(Box::new(parsed))),
                JsonLinkParse::NotJson | JsonLinkParse::Rejected(_) => None,
            }
        }
        // DEVIATION, deliberate and pre-existing. C refuses a bare string
        // here: `lnkCalc_string` reaches its `pstate < ps_expr || pstate >
        // ps_minor` guard with `pstate == ps_args` and returns `jlif_stop`
        // ("Unexpected string", `lnkCalc.c:187-190`), so `args:["pv1"]` does
        // not load in a C IOC at all. This port has always documented and
        // accepted it as shorthand for an embedded link, and in-tree
        // databases use it; it is kept as a link slot like any other rather
        // than as a third kind of argument.
        serde_json::Value::String(name) => Some(CalcArg::Link(Box::new(parse_link_field(
            name,
            LinkFieldType::In,
        )))),
        _ => None,
    }
}

/// Extract the `pv` name and all other key-value options from a
/// JSON-ish sub-object body. Returns `(pv_name, options)` where
/// `options` is every non-`pv` key as a structured `(key, value)` pair
/// in source order (empty when there are no extra options). Runs on text that
/// has already been through [`crate::json5::relaxed_to_strict`], so every key
/// is quoted and every string carries strict-JSON escapes.
///
/// The options are kept STRUCTURED — not flattened into a `?k=v&…`
/// query string — so the pvalink consumer reconstructs `PvaLinkConfig`
/// from JLink map members (pvxs/ioc/pvalink_jlif.cpp:69-196), matching pvxs,
/// which has no URI-query parser in its JLink callback table
/// (pvxs/ioc/pvalink_jlif.cpp:286-300). Key case is preserved so a case-sensitive
/// key like `Q` survives, AND the JSON value KIND is preserved as a
/// [`JlinkValue`]: a quoted token is a string, a bare `true`/`false`/
/// `null` is the JSON keyword, a bare integer is an integer. pvxs
/// dispatches its pvalink callbacks strictly by this kind
/// (pvxs/ioc/pvalink_jlif.cpp:286-300), so flattening a bare `pipeline:true` and
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
        let k_raw = k.trim().trim_matches('"');
        // Trim only the trailing entry comma + whitespace. DO NOT strip
        // the value's quotes here — quote presence is what distinguishes
        // a JSON string from a bare bool/integer/null, the distinction
        // pvxs dispatches on (pvxs/ioc/pvalink_jlif.cpp:286-300).
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
/// (pvxs `ioc/pvalink_jlif.cpp:286-300`). A quoted token is a string —
/// the dialect's single-quoted spelling is already a double-quoted one by
/// the time it gets here; the bare
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

/// Recognize a hardware (`@…` / `#Cn Sn …`) link — C `dbParseLink`'s two
/// hardware arms (`dbStaticLib.c:2266-2274` and `:2292-2344`).
///
/// `None` means C's `goto fail`: the identifier letters name no bus, a
/// letter has no number after it, there is trailing junk, or an `RF_IO`
/// link carries a parm. Such text is not a hardware link of unknown type,
/// it is not a link at all.
fn try_parse_hw_link(s: &str) -> Option<ParsedLink> {
    if s.is_empty() {
        return None;
    }
    match s.as_bytes()[0] {
        // C copies everything after the `@` with no further trimming — the
        // caller has already stripped the whole string's leading and
        // trailing blanks, so an inner leading blank survives.
        b'@' => Some(ParsedLink::Hw(HwLink::InstIo {
            string: s[1..].to_string(),
        })),
        b'#' => hw_scan(&s[1..]).map(ParsedLink::Hw),
        _ => None,
    }
}

/// Scan a `#` link into its bus payload — C's
/// `sscanf(target, "# %c%i %c%i %c%i %c%i %c%i %c", …)` and the identifier
/// table and distribution that follow it (`dbStaticLib.c:2296-2344`,
/// `:2453-2529`), with `text` everything after the `#`.
///
/// The scan and the distribution are one step here because in C they are one
/// decision: `hwid` picks both the link type and which slot of `hwnums` each
/// number lands in. Splitting them would put the positional rule in two
/// places, and it is the positional rule that produces C's odd corners — the
/// `"BCNF"` spelling stores its `F` number in `camacio.a` and leaves
/// `camacio.f` zero, because `dbSetLinkHW` reads `hwnums[3]` and `hwnums[4]`
/// whatever the letters were.
///
/// The numbers C does not scan are zero, not garbage: `dbParseLink` opens
/// with `memset(pinfo, 0, sizeof(*pinfo))` (`:2251`), so `#B1 C2 N3` reads
/// back as `#B1 C2 N3 A0 F0 @`.
fn hw_scan(text: &str) -> Option<HwLink> {
    // C isolates the parm at the first `@` before scanning (`:2292-2298`),
    // so `@` and everything after it is not part of the identifier. The parm
    // itself is kept verbatim to the end of the (already trailing-trimmed)
    // string.
    let (head, parm) = match text.split_once('@') {
        Some((head, parm)) => (head, Some(parm)),
        None => (text, None),
    };

    let mut hwid = String::new();
    let mut count = 0usize;
    let mut nums = [0i64; 5];
    let mut rest = head;
    loop {
        // The literal blank in C's format matches any run of whitespace,
        // including none — which is why `#C1S2` is a valid VME link.
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        // C's eleventh conversion, `junk`: a sixth identifier means
        // `ret == 11` and `ret > 10` fails.
        if count == 5 {
            return None;
        }
        let mut chars = rest.chars();
        // `%c` takes whatever byte is there; only the assembled `hwid`
        // decides, and no bus name contains a non-letter.
        let id = chars.next()?;
        // A letter with no number is C's odd `ret`.
        let (value, after) = scan_c_integer(chars.as_str())?;
        nums[count] = value;
        count += 1;
        hwid.push(id);
        rest = after;
    }

    // C narrows `int hwnums[]` to the width of the destination member, and
    // `dbGetString` prints what was stored, so the truncation is observable.
    let s = |i: usize| nums[i] as i16;
    let u = |i: usize| nums[i] as u8;
    let parm_string = || parm.unwrap_or_default().to_string();

    Some(match hwid.as_str() {
        "CS" => HwLink::VmeIo {
            card: s(0),
            signal: s(1),
            parm: parm_string(),
        },
        "BCN" | "BCNA" | "BCNF" | "BCNAF" => HwLink::CamacIo {
            b: s(0),
            c: s(1),
            n: s(2),
            a: s(3),
            f: s(4),
            parm: parm_string(),
        },
        // `RF_IO` is the only bus with no string member at all, so a parm on
        // one is C's last `goto fail` (`dbStaticLib.c:2333-2343`).
        "RMDE" if parm.is_none() => HwLink::RfIo {
            cryo: s(0),
            micro: s(1),
            dataset: s(2),
            element: s(3),
        },
        "LACS" => HwLink::AbIo {
            link: s(0),
            adapter: s(1),
            card: s(2),
            signal: s(3),
            parm: parm_string(),
        },
        "LA" => HwLink::GpibIo {
            link: s(0),
            addr: s(1),
            parm: parm_string(),
        },
        "LNPS" => HwLink::BitbusIo {
            link: u(0),
            node: u(1),
            port: u(2),
            signal: u(3),
            parm: parm_string(),
        },
        "LBG" => HwLink::BbgpibIo {
            link: u(0),
            bbaddr: u(1),
            gpibaddr: u(2),
            parm: parm_string(),
        },
        "VCS" => HwLink::VxiIo {
            addr: VxiAddr::Dynamic {
                frame: s(0),
                slot: s(1),
            },
            signal: s(2),
            parm: parm_string(),
        },
        "VS" => HwLink::VxiIo {
            addr: VxiAddr::Static { la: s(0) },
            signal: s(1),
            parm: parm_string(),
        },
        _ => return None,
    })
}

/// One `%i` conversion: leading whitespace, an optional sign, then the
/// longest `strtol`-base-0 prefix — `0x`/`0X` hex, a leading `0` octal,
/// otherwise decimal. Returns the value and the text after it, because `%i`
/// stops at the first character that cannot continue the number and C's
/// format resumes scanning right there.
///
/// A digit run too long for the destination is C's undefined behaviour
/// (`%i` overflowing an `int`); it saturates here so the syntax still
/// parses, and the narrowing to the bus member decides what is stored.
fn scan_c_integer(s: &str) -> Option<(i64, &str)> {
    let s = s.trim_start();
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (radix, digits) = if let Some(hex) = body
        .strip_prefix("0x")
        .or_else(|| body.strip_prefix("0X"))
        .filter(|hex| hex.starts_with(|c: char| c.is_ascii_hexdigit()))
    {
        (16, hex)
    } else if let Some(octal) = body.strip_prefix('0') {
        (8, octal)
    } else {
        (10, body)
    };
    let end = digits
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(digits.len());
    if end == 0 {
        // `strtol` has already consumed the leading `0` as the value, so
        // `#C0S1` is card 0 and `#C0x` is card 0 followed by junk.
        return if radix == 8 { Some((0, digits)) } else { None };
    }
    let magnitude = i64::from_str_radix(&digits[..end], radix).unwrap_or(i64::MAX);
    Some((
        if negative {
            magnitude.wrapping_neg()
        } else {
            magnitude
        },
        &digits[end..],
    ))
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
    /// The link-field type of a `DBF_*LINK` field class — the one mapping
    /// between the two spellings of the same thing, so a reader that has a
    /// [`DbfLinkClass`] from the `.dbd` can reach the parser.
    pub fn for_class(class: DbfLinkClass) -> Self {
        match class {
            DbfLinkClass::InLink => Self::In,
            DbfLinkClass::OutLink => Self::Out,
            DbfLinkClass::FwdLink => Self::Fwd,
        }
    }

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
    match parse_json_link(s, ftype) {
        JsonLinkParse::Parsed(parsed) => return parsed,
        // C never builds a link out of JSON it could not resolve: the put
        // fails and the field keeps its unset value. The load-time gate
        // ([`check_json_link_text`]) is what reports it; leaving an unset
        // link here is what keeps a dead PV-name link built from the raw JSON
        // text out of every path that reaches this parser without the gate.
        JsonLinkParse::Rejected(_) => return ParsedLink::None,
        JsonLinkParse::NotJson => {}
    }
    // Hardware link (epics-base PR #213). `@` starts INST_IO; `#` is
    // followed by identifier letters that name the bus. Everything else
    // falls through to legacy parsing.
    if let Some(parsed) = try_parse_hw_link(s) {
        return parsed;
    }
    if s.is_empty() {
        return ParsedLink::None;
    }
    // A `#` form the identifier table did not match is C's `goto fail`
    // (`dbStaticLib.c:2331`): `dbParseLink` returns `S_dbLib_badField`, the
    // put fails and the field keeps the unset CONSTANT link it was born
    // with. Falling through instead would build a PV link whose name starts
    // with `#`, a channel that can never connect.
    if s.starts_with('#') {
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
    // takes is `{const:"hello"}` or a bare `["hello"]` ([`load_link_ls`]).

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
        return ParsedLink::constant(s);
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
        return ParsedLink::constant(s);
    }

    // Isolate the target from its modifiers — see [`split_link_modifiers`].
    let (link_part, policy, ms, force_ca) = split_link_modifiers(s, ftype);

    // A `CA` modifier forces the link to be a CA link (C
    // `dbStaticLib.c:2372` — `pinfo->modifiers = pvlOptCA`, and
    // `dbAccess.c:1104` routes a link with any of `CA|CP|CPP` away from
    // `dbDbInitLink`). `dbParseLink` reaches the modifier scan only after the
    // constant test (`dbStaticLib.c:2347`) has failed, so a CA-forced link is
    // always a `PV_LINK` — never a Constant or local Db link.
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
    // (`dbAccess.c:667-671`) does, falling back to the default `VAL` field
    // when there is no dot or the remainder is not a field name (C
    // `dbFindFieldPart`'s "absent field name" branch,
    // `dbStaticLib.c:1802-1811`, which resolves to `pvalFldDes`). Both live
    // in `DbLink::new`.
    ParsedLink::db(DbLink::new(link_part, policy, ms))
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
/// The field keeps its case: `dbFindFieldPart` compares with `strncmp`
/// (`dbStaticLib.c:1822`), so `SRC.desc` names no field and the link falls
/// through to CA.
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
    Some((rec, field.to_string()))
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
/// (`PvDatabase::write_db_link_value_in`), matching C
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

#[cfg(test)]
mod json_link_tests {
    //! epics-base PR #86 — JSON-style inline link options.
    use super::*;

    #[test]
    fn json_const_numeric() {
        assert_eq!(
            parse_link_v2("{const: 1.5}"),
            ParsedLink::json_constant("1.5")
        );
    }

    #[test]
    fn json_const_quoted_string() {
        assert_eq!(
            parse_link_v2(r#"{const: "hello"}"#),
            ParsedLink::json_constant(r#""hello""#)
        );
    }

    #[test]
    fn json_const_empty_is_a_constant() {
        // An `sc40` "" (`lnkConst.c:199`), not the empty link: it loads.
        let ParsedLink::Constant(c) = parse_link_v2(r#"{const: ""}"#) else {
            panic!("not a constant");
        };
        assert!(c.is_json() && c.text().is_empty());
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
    // (pvxs/ioc/pvalink_jlif.cpp:24-31, :143-149). Before the fix these fell through
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
    /// (pvxs/ioc/pvalink_jlif.cpp:286-300): a bare `true`/`false` is a boolean, a
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
    /// ignore root-depth values (pvxs/ioc/pvalink_jlif.cpp:74-100,143-154). A bare
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

        // Rejected roots: a non-string, non-object value builds no link at
        // all. It used to fall through to legacy parsing, which is how it
        // came out as a PV-name link named after the JSON text; brace text
        // no longer reaches that arm (`parse_json_link`), so the field is
        // refused instead.
        for src in [
            r#"{pva: true}"#,
            r#"{pva: false}"#,
            r#"{pva: 5}"#,
            r#"{pva: null}"#,
            r#"{pva: [1,2]}"#,
        ] {
            assert_eq!(
                parse_link_v2(src),
                ParsedLink::None,
                "non-string pva root must not become any link: {src}"
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
            assert_eq!(
                parse_link_v2(src),
                ParsedLink::None,
                "non-string ca root must not become any link: {src}"
            );
        }
    }

    // Hardware links — C `dbParseLink`'s `@`/`#` arms (`dbStaticLib.c`
    // `:2266-2274`, `:2292-2344`), `dbSetLinkHW` (`:2453-2529`) and the
    // hardware arm of `dbGetString` (`:1953-2006`). One case per boundary of
    // the scan, not one per bus story.

    fn hw(text: &str) -> HwLink {
        match parse_link_v2(text) {
            ParsedLink::Hw(hw) => hw,
            other => panic!("expected Hw for {text:?}, got {other:?}"),
        }
    }

    #[test]
    fn an_inst_io_link_keeps_its_string_byte_for_byte() {
        assert_eq!(
            hw("@simDriver 0 INPUT"),
            HwLink::InstIo {
                string: "simDriver 0 INPUT".into()
            }
        );
        // PR #213: hex literals in an INST_IO payload are data, not numbers.
        assert_eq!(hw("@dev 0xFF mask=0x1A").render(), "@dev 0xFF mask=0x1A");
        // C copies from just after the `@` to the trailing-trimmed end, so a
        // blank there is part of the string.
        assert_eq!(hw("@ dev  two ").render(), "@ dev  two");
        assert_eq!(
            hw("@"),
            HwLink::InstIo {
                string: String::new()
            }
        );
    }

    #[test]
    fn a_vme_link_reads_back_from_its_numbers_not_its_text() {
        assert_eq!(
            hw("#C0x10 S-2"),
            HwLink::VmeIo {
                card: 16,
                signal: -2,
                parm: String::new(),
            }
        );
        // Base normalised and the `@parm` present though empty: this is the
        // whole point of rendering from the parse.
        assert_eq!(hw("#C0x10 S-2").render(), "#C16 S-2 @");
        assert_eq!(hw("#C010 S2").render(), "#C8 S2 @");
        assert_eq!(hw("#C1 S2 @sim").render(), "#C1 S2 @sim");
    }

    #[test]
    fn identifier_letters_need_no_blank_between_the_pairs() {
        // C's format is `"# %c%i %c%i …"`; the literal blank matches an empty
        // run, and `%i` stops at the first byte that cannot continue.
        assert_eq!(hw("#C1S2").render(), "#C1 S2 @");
        assert_eq!(hw("#  C 1   S 2").render(), "#C1 S2 @");
    }

    #[test]
    fn numbers_the_scan_never_reached_are_zero() {
        // `dbParseLink` memsets `pinfo`, so a short CAMAC spelling still has
        // all five members.
        assert_eq!(hw("#B1 C2 N3").render(), "#B1 C2 N3 A0 F0 @");
        assert_eq!(hw("#B1 C2 N3 A4").render(), "#B1 C2 N3 A4 F0 @");
        assert_eq!(hw("#B1 C2 N3 A4 F5").render(), "#B1 C2 N3 A4 F5 @");
    }

    #[test]
    fn the_camac_f_spelling_stores_its_number_in_a() {
        // Upstream, and deliberate here: `dbSetLinkHW` distributes by
        // POSITION (`hwnums[3]` → `camacio.a`), so the `"BCNF"` spelling's
        // fifth number is not the `f` member. Reproduced because the port
        // must read back what C reads back.
        assert_eq!(hw("#B1 C2 N3 F5").render(), "#B1 C2 N3 A5 F0 @");
    }

    #[test]
    fn an_rf_link_carries_no_parm_at_all() {
        assert_eq!(
            hw("#R1 M2 D3 E4"),
            HwLink::RfIo {
                cryo: 1,
                micro: 2,
                dataset: 3,
                element: 4,
            }
        );
        assert_eq!(hw("#R1 M2 D3 E4").render(), "#R1 M2 D3 E4");
        // C's last `goto fail`: RF_IO has no string member to put it in.
        assert_eq!(parse_link_v2("#R1 M2 D3 E4 @x"), ParsedLink::None);
    }

    #[test]
    fn every_remaining_bus_renders_its_own_format() {
        assert_eq!(hw("#L1 A2 C3 S4").render(), "#L1 A2 C3 S4 @");
        assert_eq!(hw("#L1 A2 @gpib").render(), "#L1 A2 @gpib");
        assert_eq!(hw("#L1 N2 P3 S4").render(), "#L1 N2 P3 S4 @");
        assert_eq!(hw("#L1 B2 G3").render(), "#L1 B2 G3 @");
    }

    #[test]
    fn a_vxi_link_is_addressed_by_slot_or_by_logical_address() {
        assert_eq!(
            hw("#V1 C2 S3"),
            HwLink::VxiIo {
                addr: VxiAddr::Dynamic { frame: 1, slot: 2 },
                signal: 3,
                parm: String::new(),
            }
        );
        assert_eq!(hw("#V1 C2 S3").render(), "#V1 C2 S3 @");
        assert_eq!(
            hw("#V7 S3 @p"),
            HwLink::VxiIo {
                addr: VxiAddr::Static { la: 7 },
                signal: 3,
                parm: "p".into(),
            }
        );
        assert_eq!(hw("#V7 S3 @p").render(), "#V7 S3 @p");
    }

    #[test]
    fn a_number_wider_than_its_bus_member_truncates_where_c_truncates() {
        // `short` members.
        assert_eq!(hw("#C70000 S1").render(), "#C4464 S1 @");
        // `unsigned char` members, printed with `%u`.
        assert_eq!(hw("#L300 N2 P3 S-1").render(), "#L44 N2 P3 S255 @");
        assert_eq!(hw("#L256 B1 G2").render(), "#L0 B1 G2 @");
    }

    #[test]
    fn the_parm_is_whatever_follows_the_first_at_sign() {
        assert_eq!(hw("#C1 S2 @ a b ").render(), "#C1 S2 @ a b");
        assert_eq!(hw("#C1 S2@x@y").render(), "#C1 S2 @x@y");
        assert_eq!(hw("#C1 S2 @").render(), "#C1 S2 @");
    }

    #[test]
    fn a_hash_form_the_identifier_table_rejects_is_not_a_link() {
        for src in [
            "#XY1 Z2",           // letters name no bus
            "#C1 S",             // odd conversion count: a letter with no number
            "#C1 S2 J3",         // odd, and the letters would not match anyway
            "#A1 B2 C3 D4 E5 F", // eleventh conversion: C's `ret > 10`
            "#C1 S2 junk",
            "#",
            "#C1 2",
        ] {
            assert_eq!(
                parse_link_v2(src),
                ParsedLink::None,
                "{src:?} must not become a link"
            );
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

    /// BOUNDARY: a comment inside a `.db` link body. `yajl_alloc` sets
    /// `yajl_allow_comments` (`yajl.c:77`) and `dbJLinkParse` never clears it,
    /// so a link body may carry one anywhere whitespace is legal. Measured
    /// against base's own yajl compiled standalone: `{expr:"A*B" /* c */}`
    /// parses.
    #[test]
    fn a_comment_inside_a_link_body_parses() {
        let JsonLinkParse::Parsed(ParsedLink::Calc(link)) = parse_json_link(
            r#"{calc: {expr:"A*B" /* the product */, args:[2, 3]}}"#,
            LinkFieldType::In,
        ) else {
            panic!("a comment in a link body must not stop the parse");
        };
        assert_eq!(link.expr, "A*B");
        assert_eq!(link.args.len(), 2);
    }

    /// Base's OWN shipped test database, verbatim —
    /// `modules/database/test/std/rec/linkRetargetLink.db:20-23`, exercised by
    /// `linkRetargetLinkTest.c`. It spells the expression single-quoted (yajl
    /// lexes `'…'` with the same routine as `"…"`, `yajl_lex.c:695-699`) and
    /// its `args` UNBRACKETED, so a reader that demands either spelling cannot
    /// load a database upstream ships and tests.
    #[test]
    fn base_s_own_shipped_calc_link_parses_verbatim() {
        let JsonLinkParse::Parsed(ParsedLink::Calc(link)) = parse_json_link(
            "{calc:{\n                expr:'A+5',\n                args:5\n             }}",
            LinkFieldType::In,
        ) else {
            panic!("base's own linkRetargetLink.db calc link must parse");
        };
        assert_eq!(link.expr, "A+5");
        assert_eq!(link.args.len(), 1, "`args:5` is ONE argument in C");
    }

    /// The scalar and the bracketed spelling are the same link. C reaches
    /// `clink->arg[clink->nArgs++] = num` from `ps_args` alone
    /// (`lnkCalc_integer`, `lnkCalc.c:120-143`); `lnkCalc_start_array`
    /// (`:319-329`) adds nothing but a state check, so the brackets are
    /// optional rather than required.
    #[test]
    fn a_scalar_args_value_is_one_argument() {
        for body in [
            "{calc:{expr:\"A+5\", args:5}}",
            "{calc:{expr:\"A+5\", args:[5]}}",
        ] {
            let JsonLinkParse::Parsed(ParsedLink::Calc(link)) =
                parse_json_link(body, LinkFieldType::In)
            else {
                panic!("{body} must parse");
            };
            assert!(
                matches!(link.args.as_slice(), [CalcArg::Literal(v)] if *v == 5.0),
                "{body} -> {:?}",
                link.args.len()
            );
        }
        // The same rule reaches an embedded link, C `lnkCalc_end_child`
        // (`lnkCalc.c:340-368`), which also appends on `ps_args` alone.
        let JsonLinkParse::Parsed(ParsedLink::Calc(link)) = parse_json_link(
            r#"{calc:{expr:"A", args:{pva:"REC:AI"}}}"#,
            LinkFieldType::In,
        ) else {
            panic!("a bare embedded link in `args` must parse");
        };
        assert!(matches!(link.args.as_slice(), [CalcArg::Link(_)]));
    }

    /// A single-quoted PV name reaches the link DEQUOTED, not with its quotes
    /// carried into the channel name.
    #[test]
    fn a_single_quoted_pv_name_is_dequoted() {
        let JsonLinkParse::Parsed(ParsedLink::Pva(pv)) =
            parse_json_link(r#"{pva: {pv: 'REC:AI.VAL'}}"#, LinkFieldType::In)
        else {
            panic!("a single-quoted pv name must parse");
        };
        assert_eq!(pv, "REC:AI.VAL");
        // The shorthand spelling too, where the whole value is the name.
        let JsonLinkParse::Parsed(ParsedLink::Pva(pv)) =
            parse_json_link(r"{pva: 'REC:AI.VAL'}", LinkFieldType::In)
        else {
            panic!("the single-quoted shorthand must parse");
        };
        assert_eq!(pv, "REC:AI.VAL");
    }

    /// BOUNDARY: a comment BEFORE the link-type key. The key reader splits on
    /// the first `:`, so an unstripped comment used to become part of the type
    /// name and the link was refused as an unknown type.
    #[test]
    fn a_comment_before_the_link_type_key_parses() {
        let JsonLinkParse::Parsed(ParsedLink::Calc(link)) =
            parse_json_link(r#"{/* which */ calc: {expr:"A+1"}}"#, LinkFieldType::In)
        else {
            panic!("a comment before the link type must not stop the parse");
        };
        assert_eq!(link.expr, "A+1");
    }

    /// BOUNDARY: an unterminated `/*` in a link body is refused, not silently
    /// stripped to EOF.
    #[test]
    fn an_unterminated_comment_in_a_link_body_is_rejected() {
        assert!(matches!(
            parse_json_link(
                r#"{calc: {expr:"A*B" /* never closed }}"#,
                LinkFieldType::In
            ),
            JsonLinkParse::Rejected(_)
        ));
    }

    #[test]
    fn link_type_hw_and_calc_are_other() {
        assert_eq!(link_field_type("@dev 0 IN"), LinkType::Other);
        assert_eq!(link_field_type("#C0 S2"), LinkType::Other);
        // Link bodies are JSON5; strict JSON is the subset that also parses.
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
            ParsedLink::db(DbLink::new(
                "REC",
                LinkProcessPolicy::NoProcess,
                MonitorSwitch::NoMaximize,
            ))
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
            ParsedLink::db(DbLink::new(
                "REC",
                LinkProcessPolicy::ProcessPassive,
                MonitorSwitch::NoMaximize,
            ))
        );
        // CPP is matched before PP, and CP only after CA.
        assert!(matches!(
            parse_link_v2("REC CPP"),
            ParsedLink::Db(l) if l.policy == LinkProcessPolicy::ChannelProcessPassive
        ));
        assert!(matches!(
            parse_link_v2("REC CP"),
            ParsedLink::Db(l) if l.policy == LinkProcessPolicy::ChannelProcess
        ));
        // Exactly one class: a modifier-less link is NPP.
        assert!(matches!(
            parse_link_v2("REC"),
            ParsedLink::Db(l) if l.policy == LinkProcessPolicy::NoProcess
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
            ParsedLink::db(DbLink::new(
                "REC",
                LinkProcessPolicy::ChannelProcessPassive,
                MonitorSwitch::MaximizeIfInvalid,
            ))
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
    /// URI query — pvxs has no query parser (pvxs/ioc/pvalink_jlif.cpp:286-300);
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
    /// pvxs/ioc/pvalink_jlif.cpp:143-149). It must NOT become a PvaJson with
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
    /// that differ by options resolve to distinct configs — under the
    /// `pva://` scheme prefix, which is what keeps the boundary dispatch on
    /// `LinkTarget::Scheme("pva")` instead of "try every registered lset".
    /// The lset itself is still handed the unprefixed key:
    /// `split_external_link_name` takes the prefix back off.
    #[test]
    fn pva_json_external_pv_name() {
        let link = parse_link_v2(r#"{pva: {pv: "TARGET:AI", proc: "CP"}}"#);
        let key = link.external_pv_name().expect("PvaJson carries a key");
        assert_eq!(key.as_ref(), "pva://TARGET:AI\u{1f}proc=s:CP");
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
    /// `dbDbPutValue` (dbDbLink.c:387-389) processes the target on either of
    /// two conditions, and a bare link meets neither: the link addresses the
    /// target's `PROC` field, or it carries `PP` **and** the target is Passive
    /// (`ppv_link->pvlMask & pvlOptPP && pdest->scan == 0`).
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
    /// (`write_db_link_value_in`, C `dbDbPutValue` dbDbLink.c:387-390), not
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
    /// `ChannelProcess` loses C's `precord->scan == 0` gate (`dbCa.c:825`, `:959`, `:1034`).
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

    /// C `lnkState_string` (`lnkState.c:79-89`) with the `!` present and
    /// absent. `{state:"X"}` and `{state:"!X"}` address the SAME named bit;
    /// only the sense differs.
    #[test]
    fn a_state_link_is_a_name_plus_a_sense() {
        let plain = parse_link_field(r#"{state:"GREEN"}"#, LinkFieldType::In);
        assert_eq!(
            plain,
            ParsedLink::State(StateLink {
                name: "GREEN".into(),
                invert: false
            })
        );
        let inverted = parse_link_field(r#"{state:"!GREEN"}"#, LinkFieldType::In);
        assert_eq!(
            inverted,
            ParsedLink::State(StateLink {
                name: "GREEN".into(),
                invert: true
            })
        );
    }

    /// The `len > 1` half of that test, which a naive `starts_with('!')`
    /// drops: softIoc R7.0.10 on `{state:"!"}` creates a state literally
    /// named `!` (measured — `dbStateShowAll 1` lists `'!' : FALSE`), and
    /// `{state:""}` creates one named `` .
    #[test]
    fn a_lone_bang_is_a_state_name_not_an_inversion() {
        assert_eq!(
            parse_link_field(r#"{state:"!"}"#, LinkFieldType::In),
            ParsedLink::State(StateLink {
                name: "!".into(),
                invert: false
            })
        );
        assert_eq!(
            parse_link_field(r#"{state:""}"#, LinkFieldType::In),
            ParsedLink::State(StateLink {
                name: String::new(),
                invert: false
            })
        );
    }

    /// `lnkStateIf` fills only `parse_string`; every other value kind hits a
    /// NULL callback, which `dbJLink.h` defines as `jlif_stop`.
    #[test]
    fn only_a_string_value_builds_a_state_link() {
        for body in [
            r#"{state:1}"#,
            r#"{state:true}"#,
            r#"{state:null}"#,
            r#"{state:1.5}"#,
            r#"{state:{name:"X"}}"#,
            r#"{state:["X"]}"#,
        ] {
            assert!(
                matches!(
                    parse_json_link(body, LinkFieldType::In),
                    JsonLinkParse::Rejected(_)
                ),
                "C stops the parse for {body}"
            );
        }
    }

    /// C `lnkState_alloc` (`lnkState.c:51-55`) refuses `DBF_FWDLINK`, so the
    /// field keeps the unset link it was born with. The other two link field
    /// classes take it.
    #[test]
    fn a_state_link_is_refused_on_a_forward_link() {
        assert_eq!(
            parse_link_field(r#"{state:"GREEN"}"#, LinkFieldType::Fwd),
            ParsedLink::None
        );
        for ftype in [LinkFieldType::In, LinkFieldType::Out] {
            assert!(matches!(
                parse_link_field(r#"{state:"GREEN"}"#, ftype),
                ParsedLink::State(_)
            ));
        }
    }

    /// One flag, both directions: C xors `slink->invert` into the value it
    /// reads (`lnkState.c:148`) and into the bit it writes (`:199`), so an
    /// inverted link reads back what it wrote.
    #[test]
    fn inversion_is_one_flag_on_the_link_not_one_per_direction() {
        let inverted = StateLink {
            name: "X".into(),
            invert: true,
        };
        for wanted in [false, true] {
            let stored = inverted.write(wanted);
            assert_eq!(inverted.read(stored), wanted);
        }
        let plain = StateLink {
            name: "X".into(),
            invert: false,
        };
        assert!(plain.write(true), "no inversion: the bit passes through");
        assert!(plain.read(true), "no inversion: the bit passes through");
    }

    /// C `lnkState_putValue`'s `DBR_STRING` arm is TEXTUAL — its own comment
    /// is `Only "" and "0" are FALSE` (`lnkState.c:181-185`) — so `"hello"`
    /// and `"0.0"` are TRUE where a numeric read of them is 0 or fails.
    #[test]
    fn a_string_is_false_only_when_it_is_empty_or_exactly_zero() {
        use crate::types::EpicsValue as V;
        let t = |v: V| match StateLink::put_of(&v) {
            StatePut::Truth(t) => t,
            other => panic!("{v:?} is a truth put, got {other:?}"),
        };
        let f = |s: &str| t(V::String(s.into()));
        assert!(!f(""));
        assert!(!f("0"));
        assert!(f("0.0"));
        assert!(f("00"));
        assert!(f("hello"));
        // C reads the C string, so a NUL ends it.
        assert!(!f("0\0x"));
        assert!(!f("\0x"));
        // The numeric arms are plain `!!value`.
        assert!(!t(V::Short(0)));
        assert!(t(V::Short(-1)));
        assert!(!t(V::Double(0.0)));
        assert!(!t(V::Double(-0.0)));
        assert!(t(V::Double(-0.5)));
        assert!(t(V::Double(f64::NAN)));
        assert!(!t(V::Char(0)));
        assert!(t(V::Enum(1)));
    }

    /// Element 0 decides an array, an empty one is `nRequest == 0`, and an
    /// `FTVL=ENUM` array is the one `DBR_ENUM` put: `lnkState.c:154-190`.
    #[test]
    fn an_array_put_is_element_zero_empty_is_nothing_enum_is_refused() {
        use crate::types::EpicsValue as V;
        let p = StateLink::put_of;
        assert_eq!(
            p(&V::DoubleArray(vec![0.0, 1.0].into())),
            StatePut::Truth(false)
        );
        assert_eq!(p(&V::LongArray(vec![2, 0].into())), StatePut::Truth(true));
        assert_eq!(
            p(&V::StringArray(vec!["0".into(), "x".into()].into())),
            StatePut::Truth(false)
        );
        assert_eq!(p(&V::DoubleArray(Vec::new().into())), StatePut::Nothing);
        assert_eq!(p(&V::EnumArray(Vec::new().into())), StatePut::Nothing);
        assert_eq!(p(&V::EnumArray(vec![1].into())), StatePut::BadDbrType);
    }

    /// `dbDumpLink` reads the registry, and `dbGetString` reads the stored
    /// text: a state link is a `JSON_LINK` whose text is what the `.db`
    /// spelled, verbatim (measured — softIoc `dbgf S:GET.INP` answers
    /// `"{state:\"GREEN\"}"`).
    #[test]
    fn a_state_link_is_a_json_link() {
        let link = parse_link_field(r#"{state:"GREEN"}"#, LinkFieldType::In);
        assert_eq!(link.db_link_type(), DbLinkType::JsonLink);
        assert!(
            LinkList::base()
                .entries()
                .iter()
                .any(|s| s.key == "state" && s.jlif_name == "lnkStateIf"),
            "`link(state, lnkStateIf)` is `links.dbd.pod:171`"
        );
    }
}

impl super::record_instance::RecordInstance {
    /// The reader's own `INP` with its target lent for as long as the reader
    /// is held — the only way to a lent target. `release_addr` runs only with
    /// the reader exclusively borrowed (`RecordInstance::destroy`,
    /// `release_link_targets_to`), and no other instance holds this
    /// `Arc<DbLink>`: every `parsed_inp` is its own parse.
    #[inline(always)]
    pub(crate) fn lend_inp_addr(
        &mut self,
        revision: u64,
        resolve: impl FnOnce(&DbLink) -> DbLinkAnswer,
    ) -> Option<(&ParsedLink, &super::record_instance::ResolvedTarget)> {
        let link = self.parsed_inp.link();
        let ParsedLink::Db(db) = link else {
            return None;
        };
        // SAFETY: `self` is borrowed `&mut` for the returned lifetime.
        let at = unsafe { db.lend_addr(revision, || resolve(db)) }?;
        Some((link, at))
    }
}
