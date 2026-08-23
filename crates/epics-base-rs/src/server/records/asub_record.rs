use crate::error::{CaError, CaResult};
use crate::server::record::{
    FieldMetadataOverride, Ftype, InputFetchPolicy, LinkReadAs, OutTarget, ProcessOutcome, Record,
};
use crate::types::{EpicsValue, PvString};

/// Number of aSub channels. C `aSubRecord.c`: `NUM_ARGS == 21`
/// (inputs `A..U`, outputs `VALA..VALU`).
const NUM_ARGS: usize = 21;

/// `menuFtype` index for `DOUBLE`. C `aSubRecord` initialises every
/// `FTx`/`FTVx` to `DOUBLE`.
const FTYPE_DOUBLE: i16 = 10;

/// The per-channel suffix letters `A..U`.
const SUFFIX: [char; NUM_ARGS] = [
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S',
    'T', 'U',
];

/// `EFLG` (`menu(aSubEFLG)`) — when the record posts output-array DB events.
/// C `aSubRecord.c::monitor` (the `switch (prec->eflg)`).
const EFLG_NEVER: i16 = 0;
const EFLG_ON_CHANGE: i16 = 1;
const EFLG_ALWAYS: i16 = 2;

/// `menu(aSubEFLG)` choice labels (`aSubRecord.dbd.pod`).
const ASUB_EFLG_CHOICES: &[&str] = &["NEVER", "ON CHANGE", "ALWAYS"];

/// `menu(aSubLFLG)` choice labels (`aSubRecord.dbd.pod`): IGNORE=0 (resolve
/// SNAM once at init), READ=1 (re-read the name from the SUBL link every
/// process). The READ behaviour is owned by the processing path
/// (`fetch_values`), which holds the link + subroutine registry.
const ASUB_LFLG_CHOICES: &[&str] = &["IGNORE", "READ"];

/// `LFLG == IGNORE` (`menu(aSubLFLG)` index 0): SNAM is resolved at init and on
/// each SNAM put via `special()`. Only in this mode does C `aSubRecord.c::
/// special` run `registryFunctionFind(prec->snam)` and refuse an unregistered
/// name; in READ mode a SNAM put is not validated.
const LFLG_IGNORE: i16 = 0;

/// The output-array fields whose monitor posting `EFLG` gates: `VALA..VALU`
/// (the output values) and `NEVA..NEVU` (their element counts). C
/// `aSubRecord.c::monitor` posts `vala[i]` and `neva[i]` together inside the
/// `EFLG` switch. Leaked once for the `'static` lifetime the posting hooks
/// require (mirrors `multi_input_links`).
fn asub_output_event_fields() -> &'static [&'static str] {
    use std::sync::OnceLock;
    static FIELDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    FIELDS.get_or_init(|| {
        SUFFIX
            .iter()
            .flat_map(|&c| {
                [
                    Box::leak(format!("VAL{c}").into_boxed_str()) as &'static str,
                    Box::leak(format!("NEV{c}").into_boxed_str()) as &'static str,
                ]
            })
            .collect()
    })
}

/// The CLOSED set of fields a process cycle of an aSub may post, under
/// `EFLG != NEVER`: `VAL` plus [`asub_output_event_fields`]. `VAL` belongs in
/// it because the deadband post is a post and the framework routes it through
/// the same gate; the `EFLG == NEVER` arm is `VAL` alone.
fn asub_posted_fields() -> &'static [&'static str] {
    use std::sync::OnceLock;
    static FIELDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    FIELDS.get_or_init(|| {
        std::iter::once("VAL")
            .chain(asub_output_event_fields().iter().copied())
            .collect()
    })
}

/// aSub (array subroutine) record.
///
/// C `aSubRecord.c` exposes 21 input channels `A..U` (fed from
/// `INPA..INPU`) and 21 output channels `VALA..VALU` (driven to
/// `OUTA..OUTU`). Each channel has:
///   * `FTx` / `FTVx` — `menuFtype` element type;
///   * `NOx` / `NOVx` — maximum element count;
///   * `NEx` / `NEVx` — elements actually used;
///
/// so a channel can carry CHAR/SHORT/LONG/FLOAT/DOUBLE/STRING data,
/// not only `DOUBLE`. The per-channel value is stored as an
/// [`EpicsValue`], which represents any scalar or array type — the
/// `FTx` field selects the interpretation. The subroutine itself is
/// invoked by the framework (`RecordInstance::subroutine`).
pub struct ASubRecord {
    /// The subroutine return status, published as VAL (`aSubRecord.c:224`
    /// `prec->val = status`). VAL is `DBF_LONG` (`aSubRecord.dbd.pod:126`), i.e.
    /// `epicsInt32`, so a string caput parses via `epicsParseLong` and an
    /// out-of-i32-range or fractional value is REFUSED — modeling it as i32
    /// keys the put on the integer row instead of accepting any double.
    pub val: i32,
    pub snam: PvString,
    pub inam: PvString,
    /// Input links `INPA..INPU`.
    pub inp: [String; NUM_ARGS],
    /// Input values `A..U` — native typed via `fta`.
    pub a: [EpicsValue; NUM_ARGS],
    /// Output values `VALA..VALU` — native typed via `ftva`.
    pub vala: [EpicsValue; NUM_ARGS],
    /// Output links `OUTA..OUTU`.
    pub out: [String; NUM_ARGS],
    /// Input element-type menu `FTA..FTU` (`menuFtype`).
    pub fta: [i16; NUM_ARGS],
    /// Output element-type menu `FTVA..FTVU` (`menuFtype`).
    pub ftva: [i16; NUM_ARGS],
    /// Input max-element counts `NOA..NOU`.
    pub noa: [i32; NUM_ARGS],
    /// Output max-element counts `NOVA..NOVU`.
    pub nova: [i32; NUM_ARGS],
    /// Input elements-used `NEA..NEU`.
    pub nea: [i32; NUM_ARGS],
    /// Output elements-used `NEVA..NEVU`.
    pub neva: [i32; NUM_ARGS],
    /// Bad-return severity (`menuAlarmSevr`, default NO_ALARM). C
    /// `aSubRecord.c::do_sub` raises `SOFT_ALARM` at this severity when the
    /// subroutine returns a negative status (applied by the framework's
    /// `run_registered_subroutine`, which also publishes the status as VAL).
    pub brsv: i16,
    /// Display precision `PREC` for VAL. C `aSubRecord.c::get_precision`
    /// returns `prec->prec` as the precision of VAL (and any non-link field);
    /// the per-channel value fields use their link's precision, which the port
    /// does not track.
    pub prec: i16,
    /// Output event flag `EFLG` (`menu(aSubEFLG)`, default ON CHANGE). C
    /// `aSubRecord.c::monitor` gates `VALA..VALU`/`NEVA..NEVU` event posting:
    /// NEVER (never post), ON CHANGE (post on change — the framework default),
    /// ALWAYS (post every process).
    pub eflg: i16,
    /// Subroutine-name input mode `LFLG` (`menu(aSubLFLG)`, default IGNORE). C
    /// `aSubRecord.c::fetch_values`: READ re-reads SNAM from the SUBL link
    /// each process and re-resolves the subroutine when it changes (the
    /// framework's processing path performs the read + re-resolution, as it
    /// owns the link and subroutine registry).
    pub lflg: i16,
    /// Subroutine-name input link `SUBL` (`DBF_INLINK`). C `init_record`
    /// seeds SNAM from a constant SUBL; with LFLG=READ, `fetch_values` reads
    /// the current name from this link every process.
    pub subl: String,
    /// Old subroutine name `ONAM`. C `fetch_values` re-resolves the
    /// subroutine only when SNAM differs from ONAM; ONAM is set to SNAM on a
    /// successful swap.
    pub onam: PvString,
    /// This cycle's C `process` status, delivered by the framework's single
    /// `do_sub` owner through [`Record::set_subroutine_status`]. C pushes every
    /// OUT link only under `if (!status)` (aSubRecord.c:232-239), so this is the
    /// whole gate: `0` drives OUTA..OUTU, anything else drives nothing. A record
    /// that has never processed starts non-zero — no outputs before the first
    /// successful `do_sub`.
    sub_status: i64,
}

/// `OUTA..OUTU` -> `VALA..VALU`, the pairs C's push loop walks
/// (`dbPutLink(&(&prec->outa)[i], ..., (&prec->vala)[i], (&prec->neva)[i])`).
fn asub_output_links() -> &'static [(&'static str, &'static str)] {
    use std::sync::OnceLock;
    static PAIRS: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    PAIRS.get_or_init(|| {
        SUFFIX
            .iter()
            .map(|&c| {
                let link: &'static str = Box::leak(format!("OUT{c}").into_boxed_str());
                let val: &'static str = Box::leak(format!("VAL{c}").into_boxed_str());
                (link, val)
            })
            .collect()
    })
}

impl Default for ASubRecord {
    fn default() -> Self {
        Self {
            val: 0,
            snam: PvString::new(),
            inam: PvString::new(),
            inp: std::array::from_fn(|_| String::new()),
            a: std::array::from_fn(|_| channel_default(FTYPE_DOUBLE, 1)),
            vala: std::array::from_fn(|_| channel_default(FTYPE_DOUBLE, 1)),
            out: std::array::from_fn(|_| String::new()),
            fta: [FTYPE_DOUBLE; NUM_ARGS],
            ftva: [FTYPE_DOUBLE; NUM_ARGS],
            noa: [1; NUM_ARGS],
            nova: [1; NUM_ARGS],
            nea: [1; NUM_ARGS],
            neva: [1; NUM_ARGS],
            brsv: 0,
            prec: 0,
            eflg: EFLG_ON_CHANGE,
            lflg: 0,
            subl: String::new(),
            onam: PvString::new(),
            // Never processed: C's `status` is only 0 after a `do_sub` that
            // returned 0, so nothing is driven out before then.
            sub_status: SUBROUTINE_NOT_RUN,
        }
    }
}

/// The `sub_status` of a record that has not yet completed a `do_sub` — any
/// non-zero value; C's `status` is a `long` that only a successful `do_sub`
/// leaves at 0.
const SUBROUTINE_NOT_RUN: i64 = -1;

/// Parse a per-channel field name into `(prefix, channel index)`.
/// E.g. `"INPC"` -> `("INP", 2)`, `"VALU"` -> `("VAL", 20)`,
/// `"M"` -> `("", 12)`.
fn parse_channel(name: &str) -> Option<(&'static str, usize)> {
    const PREFIXES: [&str; 7] = ["INP", "OUT", "VAL", "FTV", "FT", "NOV", "NO"];
    // Single-letter input value field A..U.
    if name.len() == 1 {
        let c = name.chars().next().unwrap();
        return SUFFIX.iter().position(|&s| s == c).map(|i| ("", i));
    }
    // NEA..NEU and NEVA..NEVU.
    if let Some(rest) = name.strip_prefix("NEV") {
        if rest.len() == 1 {
            let c = rest.chars().next().unwrap();
            return SUFFIX.iter().position(|&s| s == c).map(|i| ("NEV", i));
        }
        return None;
    }
    if let Some(rest) = name.strip_prefix("NE") {
        if rest.len() == 1 {
            let c = rest.chars().next().unwrap();
            return SUFFIX.iter().position(|&s| s == c).map(|i| ("NE", i));
        }
        return None;
    }
    for prefix in PREFIXES {
        if let Some(rest) = name.strip_prefix(prefix) {
            if rest.len() == 1 {
                let c = rest.chars().next().unwrap();
                if let Some(i) = SUFFIX.iter().position(|&s| s == c) {
                    return Some((prefix, i));
                }
            }
        }
    }
    None
}

/// The `menuFtype` of an `FTx` index, with C `initFields`'s clamp: an index
/// past the menu is treated as CHAR (`if (*pft > DBF_ENUM) *pft = DBF_CHAR;`,
/// aSubRecord.c:186-187).
fn channel_ftype(ft: i16) -> Ftype {
    Ftype::from_index(ft).unwrap_or(Ftype::Char)
}

/// A channel cell as C `initFields` allocates it (aSubRecord.c:176-198,
/// run for the A..U/FTx/NOx triple AND the VALA..VALU/FTVx/NOVx one):
/// `callocMustSucceed(NOx, dbValueSize(FTx))`, a zero-filled FTx-typed
/// buffer of NOx elements (`*pno == 0` is clamped to 1). The port stores
/// the one-element case as the FTx-typed zero scalar.
fn channel_default(ft: i16, no: i32) -> EpicsValue {
    let ftype = channel_ftype(ft);
    if no > 1 {
        ftype.zeroed(no as usize)
    } else {
        ftype
            .zeroed(1)
            .first_element()
            .expect("zeroed(1) has element 0")
    }
}

/// A scalar as the 1-element FTx-typed array an `NOx > 1` cell stores — C's
/// buffer is an array whatever the source's shape; a scalar source just
/// delivers `nRequest = 1`.
fn wrap_one_element(scalar: EpicsValue, ftype: Ftype) -> EpicsValue {
    match (ftype, scalar) {
        (Ftype::String, EpicsValue::String(v)) => EpicsValue::StringArray(vec![v]),
        (Ftype::Char, EpicsValue::Char(v)) => EpicsValue::CharArray(vec![v]),
        (Ftype::UChar, EpicsValue::UChar(v)) => EpicsValue::UCharArray(vec![v]),
        (Ftype::Short, EpicsValue::Short(v)) => EpicsValue::ShortArray(vec![v]),
        (Ftype::UShort, EpicsValue::UShort(v)) => EpicsValue::UShortArray(vec![v]),
        (Ftype::Long, EpicsValue::Long(v)) => EpicsValue::LongArray(vec![v]),
        (Ftype::ULong, EpicsValue::ULong(v)) => EpicsValue::ULongArray(vec![v]),
        (Ftype::Int64, EpicsValue::Int64(v)) => EpicsValue::Int64Array(vec![v]),
        (Ftype::UInt64, EpicsValue::UInt64(v)) => EpicsValue::UInt64Array(vec![v]),
        (Ftype::Float, EpicsValue::Float(v)) => EpicsValue::FloatArray(vec![v]),
        (Ftype::Double, EpicsValue::Double(v)) => EpicsValue::DoubleArray(vec![v]),
        (Ftype::Enum, EpicsValue::Enum(v)) => EpicsValue::EnumArray(vec![v]),
        // The scalar came out of `convert_to(ftype.element_type())`, so the
        // pair always matches; a framework-internal transient lands as the
        // typed zero element rather than a panic.
        (ftype, _) => ftype.zeroed(1),
    }
}

/// What a delivery leaves in a channel cell.
///
/// C `dbGet` clamps `nRequest` to the source's element count and then runs no
/// converter at all when the clamped count is not positive
/// (dbAccess.c:1016-1017, `if (n <= 0) { ; /*do nothing*/ }`), so a
/// zero-element source leaves `(&prec->a)[i]` holding exactly what it held
/// before while `fetch_values` still records `NEx = 0` (aSubRecord.c:287). A
/// "new buffer plus a count" cannot express that — every value it can carry
/// is a buffer the caller would write — so the no-delivery case is its own
/// variant and the cell write is unreachable from it.
enum ChannelDelivery {
    /// `n > 0` elements were converted into the cell.
    Delivered { value: EpicsValue, count: i32 },
    /// `n <= 0`: the cell keeps its contents, only NEx/NEVx moves.
    Empty,
}

/// Shape a value entering a channel cell — a framework link delivery, a
/// direct put, or a subroutine's output write — to the cell's declared
/// type/capacity. On the input side this is C `fetch_values`'s
/// `dbGetLink(plink, (&prec->fta)[i], (&prec->a)[i], 0, &nRequest)` with
/// `nRequest = NOx` (aSubRecord.c:278-288): the link layer converts the
/// source element-wise into the FTx-typed buffer and clamps the delivered
/// count to NOx. On the output side it is the buffer itself: a C
/// subroutine writes *into* the FTVx-typed NOVx-element `vala[i]`, so no
/// other shape can exist there.
fn shape_channel_value(value: EpicsValue, ft: i16, no: i32) -> ChannelDelivery {
    let ftype = channel_ftype(ft);
    let elem = ftype.element_type();
    if no > 1 {
        let converted = value.convert_to(elem);
        let mut arr = if converted.is_array() {
            converted
        } else {
            wrap_one_element(converted, ftype)
        };
        arr.truncate(no as usize);
        let count = arr.count() as i32;
        if count <= 0 {
            return ChannelDelivery::Empty;
        }
        ChannelDelivery::Delivered { value: arr, count }
    } else {
        // A one-element destination takes element 0 (C `dbGet` converts the
        // source field at offset 0).
        let scalar = match value.first_element() {
            Some(first) => first,
            None if value.is_array() => return ChannelDelivery::Empty,
            None => value,
        };
        let converted = scalar.convert_to(elem);
        // `convert_to` has one array-producing scalar arm (String into a
        // CHAR target yields the byte buffer); a one-element cell keeps
        // element 0 of it. The source still delivered its one element, so
        // this is a delivery of the typed zero, not a retain.
        let v = match converted.first_element() {
            Some(first) => first,
            None if converted.is_array() => channel_default(ft, no),
            None => converted,
        };
        ChannelDelivery::Delivered { value: v, count: 1 }
    }
}

/// Surface an output array channel: empty -> scalar 0.0, single
/// element -> scalar, otherwise the array (matching the legacy
/// access shape clients expect).
fn channel_get(v: &EpicsValue) -> EpicsValue {
    match v {
        EpicsValue::DoubleArray(a) if a.is_empty() => EpicsValue::Double(0.0),
        EpicsValue::DoubleArray(a) if a.len() == 1 => EpicsValue::Double(a[0]),
        other => other.clone(),
    }
}

impl Record for ASubRecord {
    fn record_type(&self) -> &'static str {
        "aSub"
    }

    /// `aSubRecord.c:294-304` — the ONE record type that routes metadata
    /// through OUT links as well: `get_inlinkNumber` maps `A`..`U` onto
    /// `INPA`..`INPU`, `get_outlinkNumber` maps `VALA`..`VALU` onto
    /// `OUTA`..`OUTU`, and all four metadata slots consult both
    /// (`:306-403`).
    fn link_backed_metadata_field(&self, field: &str) -> Option<String> {
        use crate::server::record::{arg_letter_offset, arg_link_field};
        let n = NUM_ARGS as u8;
        arg_letter_offset(field, "", n)
            .map(|i| arg_link_field("INP", i))
            .or_else(|| arg_letter_offset(field, "VAL", n).map(|i| arg_link_field("OUT", i)))
    }

    /// `aSubRecord.c::process` has no unconditional UDF re-derive: UDF is
    /// cleared only inside `do_sub` when the subroutine actually ran and
    /// returned `>= 0` (`aSubRecord.c:469-470` `prec->udf = FALSE`). The
    /// framework performs that clear in `run_registered_subroutine` (C
    /// `do_sub`'s home); a process cycle that runs no subroutine — e.g. a
    /// `caput .UDF 1` on a Passive record with no SNAM — sources nothing and
    /// must keep the client's UDF put. Opt out of the per-cycle blanket
    /// re-derive.
    fn clears_udf(&self) -> bool {
        false
    }

    /// `aSubRecord.c` has NO `recGblCheckUdf` anywhere — an undefined aSub
    /// raises NO alarm from UDF (softIoc: a fresh `record(aSub,"X"){}` reports
    /// UDF 1 with STAT/SEVR = NO_ALARM, unlike `sub` which raises UDF/INVALID).
    /// With `clears_udf` false, UDF can now legitimately stay 1 on a cycle that
    /// sourced nothing (or on a negative `do_sub` status), so this MUST be false
    /// or the framework's `rec_gbl_check_udf` would invent an alarm C never
    /// raises. See [`Record::raises_udf_alarm`]; same shape as `event`/`swait`.
    fn raises_udf_alarm(&self) -> bool {
        false
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Subroutine is invoked externally via RecordInstance.subroutine.
        Ok(ProcessOutcome::complete())
    }

    /// C `cvt_dbaddr` (`aSubRecord.c:476-495`) reads the capacity out of the
    /// channel's own count field — `(&prec->noa)[offset]` for `A..U`,
    /// `(&prec->nova)[offset]` for `VALA..VALU` — while `get_array_info` serves
    /// `NEx`/`NEVx`, the elements the last link fetch or subroutine actually
    /// delivered. `initFields` allocates each cell at its full `NOx` and never
    /// resizes it, so the capacity belongs to the declaration and only the
    /// served count moves.
    fn dbaddr_capacity(&self, field: &str) -> Option<u32> {
        let (prefix, idx) = parse_channel(field)?;
        match prefix {
            "" => Some(self.noa[idx].max(0) as u32),
            "VAL" => Some(self.nova[idx].max(0) as u32),
            _ => None,
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => return Some(EpicsValue::Long(self.val)),
            "SNAM" => return Some(EpicsValue::String(self.snam.clone())),
            "INAM" => return Some(EpicsValue::String(self.inam.clone())),
            "BRSV" => return Some(EpicsValue::Short(self.brsv)),
            "PREC" => return Some(EpicsValue::Short(self.prec)),
            "EFLG" => return Some(EpicsValue::Short(self.eflg)),
            "LFLG" => return Some(EpicsValue::Short(self.lflg)),
            "SUBL" => return Some(EpicsValue::String(self.subl.clone().into())),
            "ONAM" => return Some(EpicsValue::String(self.onam.clone())),
            _ => {}
        }
        let (prefix, idx) = parse_channel(name)?;
        Some(match prefix {
            "" => self.a[idx].clone(), // input value A..U
            "INP" => EpicsValue::String(self.inp[idx].clone().into()),
            "VAL" => channel_get(&self.vala[idx]),
            "OUT" => EpicsValue::String(self.out[idx].clone().into()),
            "FT" => EpicsValue::Short(self.fta[idx]),
            "FTV" => EpicsValue::Short(self.ftva[idx]),
            "NO" => EpicsValue::Long(self.noa[idx]),
            "NOV" => EpicsValue::Long(self.nova[idx]),
            "NE" => EpicsValue::Long(self.nea[idx]),
            "NEV" => EpicsValue::Long(self.neva[idx]),
            _ => return None,
        })
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                // VAL is DBF_LONG; the string→native put parse already rejects a
                // fractional / out-of-i32 caput in `coerce_put_value` (target is
                // DBF_LONG from `get_field`). Here VAL is also the status field
                // that subroutine closures and the framework assign directly with
                // whatever numeric they hold — C `prec->val = status` truncates
                // any arithmetic value to epicsInt32, so accept any numeric and
                // project it onto DBF_LONG rather than rejecting a Double writer.
                self.val = value
                    .to_dbf_i32()
                    .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                return Ok(());
            }
            "SNAM" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.snam = s;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                };
            }
            "INAM" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.inam = s;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                };
            }
            "BRSV" => {
                self.brsv = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("BRSV".into()))?
                    as i16;
                return Ok(());
            }
            "PREC" => {
                self.prec = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("PREC".into()))?
                    as i16;
                return Ok(());
            }
            "EFLG" => {
                self.eflg = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("EFLG".into()))?
                    as i16;
                return Ok(());
            }
            "LFLG" => {
                self.lflg = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LFLG".into()))?
                    as i16;
                return Ok(());
            }
            "SUBL" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.subl = s.as_str_lossy().into_owned();
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                };
            }
            "ONAM" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.onam = s;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                };
            }
            _ => {}
        }
        let (prefix, idx) =
            parse_channel(name).ok_or_else(|| CaError::FieldNotFound(name.to_string()))?;
        match prefix {
            // Input value A..U — shaped to the channel's declared FTx/NOx.
            // C `fetch_values` lands every link value in the FTx-typed
            // NOx-element buffer (aSubRecord.c:278-288); a direct put takes
            // the same conversion through `dbPut` into that buffer. NEx
            // tracks the elements actually delivered.
            "" => match shape_channel_value(value, self.fta[idx], self.noa[idx]) {
                ChannelDelivery::Delivered { value, count } => {
                    self.nea[idx] = count;
                    self.a[idx] = value;
                }
                ChannelDelivery::Empty => self.nea[idx] = 0,
            },
            "INP" | "OUT" => {
                let s = match value {
                    EpicsValue::String(s) => s.as_str_lossy().into_owned(),
                    _ => return Err(CaError::TypeMismatch(name.into())),
                };
                if prefix == "INP" {
                    self.inp[idx] = s;
                } else {
                    self.out[idx] = s;
                }
            }
            // Output value VALA..VALU — shaped to the channel's declared
            // FTVx/NOVx, the same rule as the input side: a C subroutine
            // writes INTO the FTVx-typed NOVx-element buffer, so no other
            // shape can leave `do_sub`, and the OUT push reads that buffer
            // (`dbPutLink(&(&prec->outa)[i], (&prec->ftva)[i], ...)`,
            // aSubRecord.c:236-238). NEVx tracks the elements written.
            "VAL" => match shape_channel_value(value, self.ftva[idx], self.nova[idx]) {
                ChannelDelivery::Delivered { value, count } => {
                    self.neva[idx] = count;
                    self.vala[idx] = value;
                }
                ChannelDelivery::Empty => self.neva[idx] = 0,
            },
            "FT" | "FTV" | "NO" | "NOV" | "NE" | "NEV" => {
                let v = match value {
                    EpicsValue::Short(v) => v as i32,
                    EpicsValue::Long(v) => v,
                    other => other
                        .to_f64()
                        .map(|f| f as i32)
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?,
                };
                match prefix {
                    // FTx/NOx and FTVx/NOVx declare their cell's element type
                    // and capacity: C `initFields` clamps them (out-of-menu
                    // FTx -> CHAR at aSubRecord.c:186-187, NOx 0 -> 1 at
                    // :189-190) and allocates the cell as a zero-filled
                    // FTx-typed buffer with NEx = NOx (:192-196) — run for
                    // both the input and the output triple. All four are
                    // SPC_NOMOD, so these puts are the load path —
                    // re-deriving the cell here is that allocation.
                    "FT" => {
                        self.fta[idx] = channel_ftype(v as i16).index();
                        self.a[idx] = channel_default(self.fta[idx], self.noa[idx]);
                        self.nea[idx] = self.noa[idx].max(1);
                    }
                    "NO" => {
                        self.noa[idx] = v.max(1);
                        self.a[idx] = channel_default(self.fta[idx], self.noa[idx]);
                        self.nea[idx] = self.noa[idx];
                    }
                    "FTV" => {
                        self.ftva[idx] = channel_ftype(v as i16).index();
                        self.vala[idx] = channel_default(self.ftva[idx], self.nova[idx]);
                        self.neva[idx] = self.nova[idx].max(1);
                    }
                    "NOV" => {
                        self.nova[idx] = v.max(1);
                        self.vala[idx] = channel_default(self.ftva[idx], self.nova[idx]);
                        self.neva[idx] = self.nova[idx];
                    }
                    "NE" => self.nea[idx] = v,
                    "NEV" => self.neva[idx] = v,
                    _ => unreachable!(),
                }
            }
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        Ok(())
    }

    /// C `aSubRecord.c::special` (SPC_MOD on SNAM) resolves the name via
    /// `registryFunctionFind` and returns `S_db_BadSub` for a non-empty
    /// unregistered name — but ONLY when `LFLG == IGNORE` (`:557-558`). In
    /// READ mode the name is read from the SUBL link at process time
    /// (`fetch_values`) and a SNAM put is not validated. The registry lookup
    /// itself is performed by the put owner; see
    /// [`Record::is_subroutine_name_field`].
    fn is_subroutine_name_field(&self, field: &str) -> bool {
        field == "SNAM" && self.lflg == LFLG_IGNORE
    }

    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        // EFLG is `menu(aSubEFLG)`, LFLG is `menu(aSubLFLG)`; serve their
        // labels so a `.db` `field(EFLG,"ON CHANGE")` / `field(LFLG,"READ")`
        // resolves and a client reads them as DBR_ENUM.
        match field {
            "EFLG" => Some(ASUB_EFLG_CHOICES),
            "LFLG" => Some(ASUB_LFLG_CHOICES),
            _ => None,
        }
    }

    fn force_posted_fields(&self) -> &'static [&'static str] {
        // EFLG=ALWAYS: C `monitor()` posts every `vala[i]`/`neva[i]` each
        // process regardless of change — the framework's force-post path.
        if self.eflg == EFLG_ALWAYS {
            asub_output_event_fields()
        } else {
            &[]
        }
    }

    /// C `aSubRecord.c` holds exactly five `db_post_events` calls, all inside
    /// `monitor()` (`:405-451`): `VAL` on `val != oval`, and the
    /// `vala[i]`/`neva[i]` pairs inside `switch (prec->eflg)`. There is none
    /// for `a[i]`, `nea[i]`, `snam`/`onam`, `FTx`, `NOx` or `INPx` — yet
    /// `fetch_values` (`:250-289`) writes `a[i]` and `nea[i]` on every cycle,
    /// and under `LFLG=READ` rewrites `snam`/`onam` too. Naming the set C
    /// posts is what keeps those writes silent; the blacklist this replaces
    /// would have had to name every field the record may ever write, and the
    /// ones `fetch_values` touches were missing from it, so an `INPA` that
    /// moved emitted `.A` and `.NEA` events C cannot produce.
    ///
    /// The three-way `EFLG` switch is here, in one place: `NEVER` admits
    /// `VAL` alone, the other two arms also admit the output pairs. They
    /// differ only in whether an UNCHANGED pair still posts, which is
    /// [`Record::force_posted_fields`].
    fn process_posted_fields(&self) -> Option<&'static [&'static str]> {
        Some(if self.eflg == EFLG_NEVER {
            &["VAL"]
        } else {
            asub_posted_fields()
        })
    }

    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        // C `aSubRecord.c::get_precision` reports `prec->prec` as VAL's display
        // precision. (The per-channel value fields use their link's precision
        // in C; the port does not model per-link precision, so only VAL is
        // served here.)
        if field.eq_ignore_ascii_case("VAL") {
            return Some(FieldMetadataOverride {
                precision: Some(self.prec),
                ..Default::default()
            });
        }
        None
    }

    /// C `aSubRecord.c:126-136`: `recGblInitConstantLink(&prec->subl,
    /// DBF_STRING, prec->snam)` followed by the `dbLoadLinkArray` loop over
    /// INPA..INPU. Both are init-only: at process `dbGetLink` on a constant
    /// delivers nothing, so SNAM and A..U keep what was seeded (or what a
    /// client later put).
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        let mut seeds = vec![crate::server::record::ConstantInitLink::new("SUBL", "SNAM")];
        seeds.extend(crate::server::record::seed_input_links(
            self.multi_input_links(),
        ));
        seeds
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        use std::sync::OnceLock;
        static PAIRS: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
        PAIRS.get_or_init(|| {
            SUFFIX
                .iter()
                .map(|&c| {
                    let link: &'static str = Box::leak(format!("INP{c}").into_boxed_str());
                    let val: &'static str = Box::leak(c.to_string().into_boxed_str());
                    (link, val)
                })
                .collect()
        })
    }

    /// C `aSubRecord.c::fetch_values` (277-289) returns on the first failed
    /// `dbGetLink` and `process` (216-218) then skips `do_sub`.
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::AbortOnFirstFailure
    }

    /// C `fetch_values` asks `dbGetLink` for the channel's declared FTx
    /// (`dbGetLink(plink, (&prec->fta)[i], ...)`, aSubRecord.c:283-284), so a
    /// STRING channel reading a scalar source receives its DBR_STRING form —
    /// an ENUM/MENU source delivers its state label, a numeric source its
    /// text — which the store must not funnel through a numeric conversion.
    /// A STRING channel with an ARRAY source keeps the native read: the
    /// whole array reaches the FTx-typed cell, whose put boundary converts
    /// it element-wise to strings (C's per-element `DBR_STRING` conversion
    /// of `nRequest = NOx` elements). Every non-STRING FTx likewise reads
    /// native — the cell is FTx-typed, so the put boundary's element-wise
    /// coercion is C's `dbGet` conversion into the FTx buffer.
    fn input_link_read_as(&self, link_field: &str, source: &OutTarget) -> Option<LinkReadAs> {
        let string_channel = parse_channel(link_field)
            .filter(|(prefix, _)| *prefix == "INP")
            .is_some_and(|(_, idx)| channel_ftype(self.fta[idx]) == Ftype::String);
        if string_channel && source.element_count <= 1 {
            Some(LinkReadAs::String)
        } else {
            Some(LinkReadAs::Native)
        }
    }

    /// The cycle status from the framework's single `do_sub` owner. Stored
    /// verbatim; [`Self::multi_output_links`] is its only reader.
    fn set_subroutine_status(&mut self, status: i64) {
        self.sub_status = status;
    }

    /// C `aSubRecord.c::process` (232-239) pushes EVERY output link once the
    /// cycle's status is 0:
    ///
    /// ```c
    /// if (!status) {
    ///     for (i = 0; i < NUM_ARGS; i++)
    ///         dbPutLink(&(&prec->outa)[i], (&prec->ftva)[i], (&prec->vala)[i],
    ///             (&prec->neva)[i]);
    /// }
    /// ```
    ///
    /// The port stored OUTA..OUTU and never wrote them, so a subroutine's
    /// results reached VALA..VALU (and CA monitors on them) but no downstream
    /// record. `status` is 0 only when `fetch_values` succeeded AND `do_sub` ran
    /// and returned 0, so a failed input link, an unresolved SNAM and a
    /// subroutine that reported failure each suppress every push — the gate is
    /// the status itself, not a per-link condition. The framework's generic
    /// `multi_output_links` dispatch skips an empty link name, which is C's
    /// `dbPutLink` on an unset link (a no-op).
    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        if self.sub_status == 0 {
            asub_output_links()
        } else {
            &[]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// input/output channels M..U exist (C `NUM_ARGS == 21`).
    #[test]
    fn channels_m_through_u_present() {
        let mut rec = ASubRecord::default();
        for name in ["M", "Q", "U"] {
            rec.put_field(name, EpicsValue::Double(3.0)).unwrap();
            assert_eq!(rec.get_field(name), Some(EpicsValue::Double(3.0)));
        }
        for (name, nov) in [("VALM", "NOVM"), ("VALU", "NOVU")] {
            // C requires the output capacity declared up front, like the
            // input side: the subroutine writes into an NOVx-element buffer.
            rec.put_field(nov, EpicsValue::Long(2)).unwrap();
            rec.put_field(name, EpicsValue::DoubleArray(vec![1.0, 2.0]))
                .unwrap();
            assert_eq!(
                rec.get_field(name),
                Some(EpicsValue::DoubleArray(vec![1.0, 2.0]))
            );
        }
        for name in ["NOM", "NOVU", "INPR", "OUTT"] {
            assert!(rec.get_field(name).is_some(), "channel field must exist");
        }
    }

    /// a non-double channel keeps its native type. `FTVA=STRING`
    /// with a string output value is represented faithfully.
    #[test]
    fn non_double_channel_keeps_native_type() {
        let mut rec = ASubRecord::default();
        rec.put_field("FTA", EpicsValue::Short(0)).unwrap(); // STRING
        rec.put_field("A", EpicsValue::String("hello".into()))
            .unwrap();
        assert_eq!(rec.get_field("A"), Some(EpicsValue::String("hello".into())));
        assert_eq!(rec.fta[0], 0);

        rec.put_field("FTC", EpicsValue::Short(5)).unwrap(); // LONG
        // C requires the capacity declared up front: NOx elements are
        // allocated at init, and `dbGetLink` clamps `nRequest` to NOx.
        rec.put_field("NOC", EpicsValue::Long(3)).unwrap();
        rec.put_field("C", EpicsValue::LongArray(vec![10, 20, 30]))
            .unwrap();
        assert_eq!(
            rec.get_field("C"),
            Some(EpicsValue::LongArray(vec![10, 20, 30]))
        );
        // NEC tracks elements used.
        assert_eq!(rec.get_field("NEC"), Some(EpicsValue::Long(3)));
    }

    /// FTx/NOx declare the input cell — C `initFields` (aSubRecord.c:176-198)
    /// allocates a zero-filled FTx-typed buffer of NOx elements, NEx = NOx,
    /// clamping an out-of-menu FTx to CHAR (:186-187).
    #[test]
    fn ftx_nox_declare_the_input_cell() {
        let mut rec = ASubRecord::default();
        rec.put_field("FTA", EpicsValue::Short(10)).unwrap(); // DOUBLE
        rec.put_field("NOA", EpicsValue::Long(4)).unwrap();
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::DoubleArray(vec![0.0; 4]))
        );
        assert_eq!(
            rec.get_field("NEA"),
            Some(EpicsValue::Long(4)),
            "C initFields: NEx starts at NOx"
        );

        rec.put_field("FTB", EpicsValue::Short(0)).unwrap(); // STRING
        assert_eq!(rec.get_field("B"), Some(EpicsValue::String("".into())));

        rec.put_field("FTC", EpicsValue::Short(99)).unwrap(); // out of menu
        rec.put_field("NOC", EpicsValue::Long(2)).unwrap();
        assert_eq!(
            rec.get_field("FTC"),
            Some(EpicsValue::Short(1)),
            "aSubRecord.c:186-187: FTx past DBF_ENUM becomes CHAR"
        );
        assert_eq!(rec.get_field("C"), Some(EpicsValue::CharArray(vec![0, 0])));
    }

    /// Values entering an input cell convert into the declared buffer — C
    /// `dbGetLink(plink, FTx, a[i], 0, &nRequest)` with `nRequest` clamped
    /// to NOx (aSubRecord.c:278-288); `dbPut` converts the same way.
    #[test]
    fn input_puts_convert_and_clamp_to_the_declared_buffer() {
        let mut rec = ASubRecord::default();
        rec.put_field("FTA", EpicsValue::Short(5)).unwrap(); // LONG
        rec.put_field("NOA", EpicsValue::Long(3)).unwrap();
        // An array source converts element-wise and clamps to NOx.
        rec.put_field("A", EpicsValue::DoubleArray(vec![1.9, 2.2, 3.7, 4.4]))
            .unwrap();
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::LongArray(vec![1, 2, 3]))
        );
        assert_eq!(rec.get_field("NEA"), Some(EpicsValue::Long(3)));

        // A scalar source fills one element of an array cell (nRequest = 1).
        rec.put_field("A", EpicsValue::Double(7.0)).unwrap();
        assert_eq!(rec.get_field("A"), Some(EpicsValue::LongArray(vec![7])));
        assert_eq!(rec.get_field("NEA"), Some(EpicsValue::Long(1)));

        // An array into a one-element cell keeps element 0 (C's one-element
        // destination, same rule as the store's scalar reduction).
        rec.put_field("B", EpicsValue::DoubleArray(vec![5.5, 6.6]))
            .unwrap();
        assert_eq!(rec.get_field("B"), Some(EpicsValue::Double(5.5)));
        assert_eq!(rec.get_field("NEB"), Some(EpicsValue::Long(1)));

        // An EMPTY array source delivers zero elements — dbGet clamps
        // nRequest to the source count (dbAccess.c:999-1000) and NEx records
        // it (aSubRecord.c:287) — but the clamped count then runs NO
        // converter (dbAccess.c:1016-1017), so the cell keeps what the last
        // real delivery left there. Both cell shapes.
        rec.put_field("B", EpicsValue::DoubleArray(vec![])).unwrap();
        assert_eq!(
            rec.get_field("B"),
            Some(EpicsValue::Double(5.5)),
            "one-element cell retains the previous delivery"
        );
        assert_eq!(rec.get_field("NEB"), Some(EpicsValue::Long(0)));
        rec.put_field("A", EpicsValue::DoubleArray(vec![])).unwrap();
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::LongArray(vec![7])),
            "NOx > 1 cell retains the previous delivery"
        );
        assert_eq!(rec.get_field("NEA"), Some(EpicsValue::Long(0)));

        // A non-empty delivery after the empty one overwrites normally.
        rec.put_field("A", EpicsValue::DoubleArray(vec![8.0, 9.0]))
            .unwrap();
        assert_eq!(rec.get_field("A"), Some(EpicsValue::LongArray(vec![8, 9])));
        assert_eq!(rec.get_field("NEA"), Some(EpicsValue::Long(2)));
    }

    /// An output cell follows the same `dbGet` rule: a subroutine that writes
    /// nothing into `vala[i]` leaves the buffer alone and only moves NEVx.
    #[test]
    fn empty_output_write_retains_the_output_cell() {
        let mut rec = ASubRecord::default();
        rec.put_field("FTVA", EpicsValue::Short(10)).unwrap(); // DOUBLE
        rec.put_field("NOVA", EpicsValue::Long(3)).unwrap();
        rec.put_field("VALA", EpicsValue::DoubleArray(vec![1.0, 2.0]))
            .unwrap();
        assert_eq!(rec.get_field("NEVA"), Some(EpicsValue::Long(2)));

        rec.put_field("VALA", EpicsValue::DoubleArray(vec![]))
            .unwrap();
        assert_eq!(
            rec.get_field("VALA"),
            Some(EpicsValue::DoubleArray(vec![1.0, 2.0]))
        );
        assert_eq!(rec.get_field("NEVA"), Some(EpicsValue::Long(0)));
    }

    /// FTVx/NOVx declare the output cell the same way (C `initFields` runs
    /// for the output triple too): the cell allocates typed and zeroed, and
    /// a subroutine's write converts and clamps into it — C's subroutine
    /// writes INTO the FTVx-typed NOVx-element buffer, so no other shape
    /// can leave `do_sub`.
    #[test]
    fn ftvx_novx_declare_the_output_cell() {
        let mut rec = ASubRecord::default();
        rec.put_field("FTVA", EpicsValue::Short(10)).unwrap(); // DOUBLE
        rec.put_field("NOVA", EpicsValue::Long(3)).unwrap();
        assert_eq!(
            rec.get_field("VALA"),
            Some(EpicsValue::DoubleArray(vec![0.0; 3]))
        );
        assert_eq!(rec.get_field("NEVA"), Some(EpicsValue::Long(3)));

        rec.put_field("VALA", EpicsValue::LongArray(vec![1, 2, 3, 4]))
            .unwrap();
        assert_eq!(
            rec.get_field("VALA"),
            Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0])),
            "a subroutine write converts element-wise and clamps to NOVx"
        );
        assert_eq!(rec.get_field("NEVA"), Some(EpicsValue::Long(3)));

        rec.put_field("FTVB", EpicsValue::Short(0)).unwrap(); // STRING
        rec.put_field("VALB", EpicsValue::String("done".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("VALB"),
            Some(EpicsValue::String("done".into())),
            "a declared STRING output keeps its string"
        );
    }

    /// C `fetch_values` requests the channel's FTx from `dbGetLink`
    /// (aSubRecord.c:283-284): a STRING channel with a scalar source reads
    /// DBR_STRING; an array source and every non-STRING channel read native
    /// (the FTx-typed cell converts at its put boundary).
    #[test]
    fn string_channels_read_scalar_links_as_strings() {
        let mut rec = ASubRecord::default();
        rec.put_field("FTC", EpicsValue::Short(0)).unwrap(); // STRING
        let scalar = OutTarget {
            element_count: 1,
            ..OutTarget::UNRESOLVED
        };
        assert_eq!(
            rec.input_link_read_as("INPC", &scalar),
            Some(LinkReadAs::String)
        );
        let array = OutTarget {
            element_count: 8,
            ..OutTarget::UNRESOLVED
        };
        assert_eq!(
            rec.input_link_read_as("INPC", &array),
            Some(LinkReadAs::Native)
        );
        assert_eq!(
            rec.input_link_read_as("INPA", &scalar),
            Some(LinkReadAs::Native),
            "a DOUBLE channel reads native"
        );
        assert_eq!(
            rec.input_link_read_as("SUBL", &scalar),
            Some(LinkReadAs::Native),
            "only INPx links carry a channel FTx"
        );
    }

    /// All 21 input channels feed `multi_input_links`.
    #[test]
    fn twenty_one_multi_input_links() {
        let rec = ASubRecord::default();
        assert_eq!(rec.multi_input_links().len(), 21);
    }

    /// PREC round-trips and is served as VAL's display precision (C
    /// `aSubRecord.c::get_precision` returns `prec->prec` for VAL).
    #[test]
    fn prec_round_trips_and_serves_val_precision() {
        let mut rec = ASubRecord::default();
        assert_eq!(rec.get_field("PREC"), Some(EpicsValue::Short(0)));
        assert_eq!(
            rec.field_metadata_override("VAL").and_then(|o| o.precision),
            Some(0)
        );

        rec.put_field("PREC", EpicsValue::Short(4)).unwrap();
        assert_eq!(rec.get_field("PREC"), Some(EpicsValue::Short(4)));
        assert_eq!(
            rec.field_metadata_override("VAL").and_then(|o| o.precision),
            Some(4),
            "PREC must be served as VAL display precision"
        );
        // Non-VAL fields carry no precision override.
        assert!(rec.field_metadata_override("SNAM").is_none());
    }

    /// EFLG gates output-array posting. C `aSubRecord.c::monitor`:
    /// NEVER posts no `VALx`/`NEVx`; ON CHANGE (default) leaves them to the
    /// framework's change-detection; ALWAYS force-posts them every process.
    #[test]
    fn eflg_drives_output_event_posting_policy() {
        let mut rec = ASubRecord::default();
        // Default ON CHANGE: neither force-post nor suppress.
        assert_eq!(rec.eflg, EFLG_ON_CHANGE);
        assert!(rec.force_posted_fields().is_empty());
        let posted = rec.process_posted_fields().expect("aSub closes the set");
        assert_eq!(posted.len(), NUM_ARGS * 2 + 1);
        assert!(posted.contains(&"VAL"));
        assert!(posted.contains(&"VALA"));
        assert!(posted.contains(&"NEVU"));
        // The input side is outside the set in every arm.
        assert!(!posted.contains(&"A"));
        assert!(!posted.contains(&"NEA"));

        // ALWAYS: VALx/NEVx force-posted every cycle (21 + 21 = 42 fields).
        rec.put_field("EFLG", EpicsValue::Short(EFLG_ALWAYS))
            .unwrap();
        assert_eq!(rec.get_field("EFLG"), Some(EpicsValue::Short(2)));
        assert_eq!(rec.force_posted_fields().len(), NUM_ARGS * 2);
        assert!(rec.force_posted_fields().contains(&"VALA"));
        assert!(rec.force_posted_fields().contains(&"NEVU"));
        assert_eq!(
            rec.process_posted_fields().map(<[&str]>::len),
            Some(NUM_ARGS * 2 + 1)
        );

        // NEVER: the set narrows to VAL, which C posts outside the switch.
        rec.put_field("EFLG", EpicsValue::Short(EFLG_NEVER))
            .unwrap();
        assert!(rec.force_posted_fields().is_empty());
        assert_eq!(rec.process_posted_fields(), Some(&["VAL"][..]));
    }

    /// EFLG loads from a `.db` menu label (C `field(EFLG,"ALWAYS")`).
    #[test]
    fn eflg_loads_from_menu_label() {
        use crate::server::db_loader::{apply_fields, create_record};
        let mut rec = create_record("aSub").unwrap();
        let mut common = Vec::new();
        apply_fields(&mut rec, &[("EFLG".into(), "ALWAYS".into())], &mut common)
            .expect("EFLG menu label must load");
        assert_eq!(rec.get_field("EFLG"), Some(EpicsValue::Short(2)));
    }

    /// BRSV round-trips as a `menuAlarmSevr` index (C `aSubRecord.c` BRSV,
    /// the severity used when the subroutine returns a negative status).
    #[test]
    fn brsv_round_trips() {
        let mut rec = ASubRecord::default();
        assert_eq!(rec.get_field("BRSV"), Some(EpicsValue::Short(0)));
        rec.put_field("BRSV", EpicsValue::Short(3)).unwrap();
        assert_eq!(rec.get_field("BRSV"), Some(EpicsValue::Short(3)));
    }

    /// LFLG/SUBL/ONAM round-trip and default to IGNORE / empty / empty
    /// (C `aSubRecord.c` defaults; ONAM init-copies the empty SNAM).
    #[test]
    fn lflg_subl_onam_round_trip() {
        let mut rec = ASubRecord::default();
        assert_eq!(rec.get_field("LFLG"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("SUBL"), Some(EpicsValue::String("".into())));
        assert_eq!(rec.get_field("ONAM"), Some(EpicsValue::String("".into())));

        rec.put_field("LFLG", EpicsValue::Short(1)).unwrap();
        rec.put_field("SUBL", EpicsValue::String("HOLDER".into()))
            .unwrap();
        rec.put_field("ONAM", EpicsValue::String("prev".into()))
            .unwrap();
        assert_eq!(rec.get_field("LFLG"), Some(EpicsValue::Short(1)));
        assert_eq!(
            rec.get_field("SUBL"),
            Some(EpicsValue::String("HOLDER".into()))
        );
        assert_eq!(
            rec.get_field("ONAM"),
            Some(EpicsValue::String("prev".into()))
        );
    }

    /// C `aSubRecord.c::special` runs `registryFunctionFind(prec->snam)` ONLY
    /// when `LFLG == IGNORE`; a SNAM put in READ mode is not validated, and no
    /// other field is a subroutine name.
    #[test]
    fn snam_is_a_subroutine_name_field_only_in_ignore_mode() {
        let mut rec = ASubRecord::default();
        assert_eq!(rec.lflg, LFLG_IGNORE);
        assert!(rec.is_subroutine_name_field("SNAM"));
        assert!(!rec.is_subroutine_name_field("INAM"));
        assert!(!rec.is_subroutine_name_field("VAL"));

        rec.put_field("LFLG", EpicsValue::Short(1)).unwrap(); // READ
        assert!(
            !rec.is_subroutine_name_field("SNAM"),
            "READ mode reads the name from SUBL at process time; a SNAM put is not validated"
        );
    }

    /// `field(LFLG,"READ")` resolves via the `menu(aSubLFLG)` labels;
    /// `field(SUBL,"...")` loads even though SUBL is `special(SPC_NOMOD)`
    /// (the loader writes through `put_field`, bypassing the runtime gate).
    #[test]
    fn lflg_subl_load_from_db() {
        use crate::server::db_loader::{apply_fields, create_record};
        let mut rec = create_record("aSub").unwrap();
        let mut common = Vec::new();
        apply_fields(
            &mut rec,
            &[
                ("LFLG".into(), "READ".into()),
                ("SUBL".into(), "NAME_HOLDER".into()),
            ],
            &mut common,
        )
        .expect("LFLG menu label and SUBL link must load");
        assert_eq!(rec.get_field("LFLG"), Some(EpicsValue::Short(1)));
        assert_eq!(
            rec.get_field("SUBL"),
            Some(EpicsValue::String("NAME_HOLDER".into()))
        );
    }
}
