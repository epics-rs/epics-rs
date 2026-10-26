use crate::error::{CaError, CaResult};
use crate::server::record::{
    CycleMarks, CyclePostMask, FieldMetadataOverride, Ftype, InputFetchPolicy, InputLinkRequest,
    LinkReadAs, MonitorValueClasses, ProcessOutcome, Record,
};
use crate::types::{EpicsValue, PvString};

/// Number of aSub channels. C `aSubRecord.c`: `NUM_ARGS == 21`
/// (inputs `A..U`, outputs `VALA..VALU`).
const NUM_ARGS: usize = 21;

/// `menuFtype` index for `DOUBLE`. C `aSubRecord` initialises every
/// `FTx`/`FTVx` to `DOUBLE`.
const FTYPE_DOUBLE: i16 = 10;

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
/// `EFLG` switch. Channel `i` is entries `2i` (`VALx`) and `2i + 1` (`NEVx`).
const OUTPUT_EVENT_FIELDS: [&str; 2 * NUM_ARGS] = [
    "VALA", "NEVA", "VALB", "NEVB", "VALC", "NEVC", "VALD", "NEVD", "VALE", "NEVE", "VALF", "NEVF",
    "VALG", "NEVG", "VALH", "NEVH", "VALI", "NEVI", "VALJ", "NEVJ", "VALK", "NEVK", "VALL", "NEVL",
    "VALM", "NEVM", "VALN", "NEVN", "VALO", "NEVO", "VALP", "NEVP", "VALQ", "NEVQ", "VALR", "NEVR",
    "VALS", "NEVS", "VALT", "NEVT", "VALU", "NEVU",
];

/// `monitor()`'s output posts, `monitor_mask|DBE_VALUE|DBE_LOG` each.
static OUTPUT_EVENT_POSTS: [(crate::server::record::FieldIx, CyclePostMask); 2 * NUM_ARGS] =
    crate::server::record::uniform_post_table(
        crate::server::record::dbd_generated::A_SUB_FIELDS,
        &OUTPUT_EVENT_FIELDS,
        CyclePostMask::MonitorValueLog,
    );

fn asub_output_event_fields() -> &'static [&'static str] {
    &OUTPUT_EVENT_FIELDS
}

/// The CLOSED set of fields a process cycle of an aSub may post, under
/// `EFLG != NEVER`: `VAL` plus [`asub_output_event_fields`]. `VAL` belongs in
/// it because the deadband post is a post and the framework routes it through
/// the same gate; the `EFLG == NEVER` arm is `VAL` alone.
fn asub_posted_fields() -> crate::server::record::DeclaredFields {
    static POSTED: [crate::server::record::FieldIx; 1 + 2 * NUM_ARGS] =
        crate::server::record::declared_set(
            crate::server::record::dbd_generated::A_SUB_FIELDS,
            POSTED_FIELDS,
        );
    &POSTED
}

const POSTED_FIELDS: [&str; 1 + 2 * NUM_ARGS] = {
    let mut t = ["VAL"; 1 + 2 * NUM_ARGS];
    let mut i = 0;
    while i < 2 * NUM_ARGS {
        t[i + 1] = OUTPUT_EVENT_FIELDS[i];
        i += 1;
    }
    t
};

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
    /// The VAL `monitor()` last posted (`aSubRecord.c:414-416`), seeded from
    /// VAL by the init tail.
    pub oval: i32,
    pub snam: PvString,
    pub inam: PvString,
    /// Input links `INPA..INPU`. Private with `fta`: [`Self::put_field`] is
    /// their one writer, and it moves `links_generation`.
    inp: [PvString; NUM_ARGS],
    /// Input values `A..U` — native typed via `fta`.
    pub a: [EpicsValue; NUM_ARGS],
    /// Output values `VALA..VALU` — native typed via `ftva`. Private with
    /// `neva`: [`Self::put_field`] is their one writer, and it sets `out_dirty`.
    vala: [EpicsValue; NUM_ARGS],
    /// `OVLA..OVLU` / `ONVA..ONVU`: the `vala`/`neva` that `monitor()` last
    /// posted (`aSubRecord.c:422-438`), seeded from them by the init tail.
    ovla: [EpicsValue; NUM_ARGS],
    onva: [i32; NUM_ARGS],
    /// Bit `i`: `monitor()` posts `VALx` (`val_marks`) / `NEVx` (`nev_marks`)
    /// this cycle, drained by [`Record::take_cycle_posted_fields`].
    val_marks: u32,
    nev_marks: u32,
    /// Bit `i`: `vala[i]`/`neva[i]` written since the last ON CHANGE
    /// `monitor()`, which left every clean channel equal to its `OVLx`.
    out_dirty: u32,
    /// Output links `OUTA..OUTU`. Private with `wired_outs`: [`Self::put_field`]
    /// is their one writer.
    out: [PvString; NUM_ARGS],
    /// The `(OUTx, VALx)` pairs whose `OUTx` is set, in channel order: what
    /// C's push loop does work for, `dbPutLink` on an unset link being a no-op.
    wired_outs: Vec<(&'static str, &'static str)>,
    /// Input element-type menu `FTA..FTU` (`menuFtype`).
    fta: [i16; NUM_ARGS],
    /// Output element-type menu `FTVA..FTVU` (`menuFtype`).
    pub ftva: [i16; NUM_ARGS],
    /// Input max-element counts `NOA..NOU`.
    pub noa: [i32; NUM_ARGS],
    /// Output max-element counts `NOVA..NOVU`.
    pub nova: [i32; NUM_ARGS],
    /// Input elements-used `NEA..NEU`.
    pub nea: [i32; NUM_ARGS],
    /// Output elements-used `NEVA..NEVU`.
    neva: [i32; NUM_ARGS],
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
    pub subl: PvString,
    /// Old subroutine name `ONAM`. C `fetch_values` re-resolves the
    /// subroutine only when SNAM differs from ONAM; ONAM is set to SNAM on a
    /// successful swap.
    pub onam: PvString,
    /// This cycle's C `process` status, delivered by the framework's single
    /// `do_sub` owner through [`Record::set_subroutine_status`]. C pushes every
    /// OUT link only under `if (!status)` (aSubRecord.c:234-240), so this is the
    /// whole gate: `0` drives OUTA..OUTU, anything else drives nothing. A record
    /// that has never processed starts non-zero — no outputs before the first
    /// successful `do_sub`.
    sub_status: i64,
    /// [`Record::input_links_generation`]: moved on by every write of an
    /// `INPx` text or of the `FTx`/`NOx` that [`Record::input_link_request`] reads.
    links_generation: u64,
}

/// `OUTA..OUTU` -> `VALA..VALU`, the pairs C's push loop walks
/// (`dbPutLink(&(&prec->outa)[i], ..., (&prec->vala)[i], (&prec->neva)[i])`).
fn asub_output_links() -> &'static [(&'static str, &'static str)] {
    &OUTPUT_LINKS
}

static OUTPUT_LINKS: [(&str, &str); NUM_ARGS] = [
    ("OUTA", "VALA"),
    ("OUTB", "VALB"),
    ("OUTC", "VALC"),
    ("OUTD", "VALD"),
    ("OUTE", "VALE"),
    ("OUTF", "VALF"),
    ("OUTG", "VALG"),
    ("OUTH", "VALH"),
    ("OUTI", "VALI"),
    ("OUTJ", "VALJ"),
    ("OUTK", "VALK"),
    ("OUTL", "VALL"),
    ("OUTM", "VALM"),
    ("OUTN", "VALN"),
    ("OUTO", "VALO"),
    ("OUTP", "VALP"),
    ("OUTQ", "VALQ"),
    ("OUTR", "VALR"),
    ("OUTS", "VALS"),
    ("OUTT", "VALT"),
    ("OUTU", "VALU"),
];

/// `INPA..INPU` -> `A..U`, [`Record::multi_input_links`].
static INPUT_LINKS: [(&str, &str); NUM_ARGS] = [
    ("INPA", "A"),
    ("INPB", "B"),
    ("INPC", "C"),
    ("INPD", "D"),
    ("INPE", "E"),
    ("INPF", "F"),
    ("INPG", "G"),
    ("INPH", "H"),
    ("INPI", "I"),
    ("INPJ", "J"),
    ("INPK", "K"),
    ("INPL", "L"),
    ("INPM", "M"),
    ("INPN", "N"),
    ("INPO", "O"),
    ("INPP", "P"),
    ("INPQ", "Q"),
    ("INPR", "R"),
    ("INPS", "S"),
    ("INPT", "T"),
    ("INPU", "U"),
];

impl Default for ASubRecord {
    fn default() -> Self {
        Self {
            val: 0,
            oval: 0,
            snam: PvString::new(),
            inam: PvString::new(),
            inp: std::array::from_fn(|_| PvString::new()),
            a: std::array::from_fn(|_| channel_default(FTYPE_DOUBLE, 1)),
            vala: std::array::from_fn(|_| channel_default(FTYPE_DOUBLE, 1)),
            ovla: std::array::from_fn(|_| channel_default(FTYPE_DOUBLE, 1)),
            onva: [1; NUM_ARGS],
            val_marks: 0,
            nev_marks: 0,
            out_dirty: 0,
            out: std::array::from_fn(|_| PvString::new()),
            wired_outs: Vec::new(),
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
            subl: PvString::new(),
            onam: PvString::new(),
            // Never processed: C's `status` is only 0 after a `do_sub` that
            // returned 0, so nothing is driven out before then.
            sub_status: SUBROUTINE_NOT_RUN,
            links_generation: 0,
        }
    }
}

/// The `sub_status` of a record that has not yet completed a `do_sub` — any
/// non-zero value; C's `status` is a `long` that only a successful `do_sub`
/// leaves at 0.
const SUBROUTINE_NOT_RUN: i64 = -1;

/// The per-channel field family a name such as `INPC` belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chan {
    /// Input value `A..U`.
    Value,
    Inp,
    Out,
    Val,
    Ft,
    Ftv,
    No,
    Nov,
    Ne,
    Nev,
    Onv,
}

/// Parse a per-channel field name into `(family, channel index)`.
/// E.g. `"INPC"` -> `(Inp, 2)`, `"VALU"` -> `(Val, 20)`, `"M"` -> `(Value, 12)`.
fn parse_channel(name: &str) -> Option<(Chan, usize)> {
    let (&last, head) = name.as_bytes().split_last()?;
    let idx = usize::from(last.wrapping_sub(b'A'));
    if idx >= NUM_ARGS {
        return None;
    }
    let chan = match head {
        b"" => Chan::Value,
        b"INP" => Chan::Inp,
        b"OUT" => Chan::Out,
        b"VAL" => Chan::Val,
        b"FT" => Chan::Ft,
        b"FTV" => Chan::Ftv,
        b"NO" => Chan::No,
        b"NOV" => Chan::Nov,
        b"NE" => Chan::Ne,
        b"NEV" => Chan::Nev,
        b"ONV" => Chan::Onv,
        _ => return None,
    };
    Some((chan, idx))
}

/// The `menuFtype` of an `FTx` index, with C `initFields`'s clamp: an index
/// past the menu is treated as CHAR (`if (*pft > DBF_ENUM) *pft = DBF_CHAR;`,
/// aSubRecord.c:186-187).
fn channel_ftype(ft: i16) -> Ftype {
    Ftype::from_index(ft).unwrap_or(Ftype::Char)
}

/// The `dbGetLink` request naming a channel's FTx.
fn ftype_read(ftype: Ftype) -> LinkReadAs {
    match ftype {
        Ftype::String => LinkReadAs::String,
        Ftype::Char => LinkReadAs::Char,
        Ftype::UChar => LinkReadAs::UChar,
        Ftype::Short => LinkReadAs::Short,
        Ftype::UShort => LinkReadAs::UShort,
        Ftype::Long => LinkReadAs::Long,
        Ftype::ULong => LinkReadAs::ULong,
        Ftype::Int64 => LinkReadAs::Int64,
        Ftype::UInt64 => LinkReadAs::UInt64,
        Ftype::Float => LinkReadAs::Float,
        Ftype::Double => LinkReadAs::Double,
        Ftype::Enum => LinkReadAs::Enum,
    }
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
        (Ftype::String, EpicsValue::String(v)) => EpicsValue::StringArray(vec![v].into()),
        (Ftype::Char, EpicsValue::Char(v)) => EpicsValue::CharArray(vec![v].into()),
        (Ftype::UChar, EpicsValue::UChar(v)) => EpicsValue::UCharArray(vec![v].into()),
        (Ftype::Short, EpicsValue::Short(v)) => EpicsValue::ShortArray(vec![v].into()),
        (Ftype::UShort, EpicsValue::UShort(v)) => EpicsValue::UShortArray(vec![v].into()),
        (Ftype::Long, EpicsValue::Long(v)) => EpicsValue::LongArray(vec![v].into()),
        (Ftype::ULong, EpicsValue::ULong(v)) => EpicsValue::ULongArray(vec![v].into()),
        (Ftype::Int64, EpicsValue::Int64(v)) => EpicsValue::Int64Array(vec![v].into()),
        (Ftype::UInt64, EpicsValue::UInt64(v)) => EpicsValue::UInt64Array(vec![v].into()),
        (Ftype::Float, EpicsValue::Float(v)) => EpicsValue::FloatArray(vec![v].into()),
        (Ftype::Double, EpicsValue::Double(v)) => EpicsValue::DoubleArray(vec![v].into()),
        (Ftype::Enum, EpicsValue::Enum(v)) => EpicsValue::EnumArray(vec![v].into()),
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
        let converted = value.into_type(elem);
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
        let converted = scalar.into_type(elem);
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

/// Whether [`channel_get`] serves `v` as a scalar.
fn channel_is_scalar(v: &EpicsValue) -> bool {
    match v {
        EpicsValue::DoubleArray(a) => a.len() <= 1,
        other => !other.is_array(),
    }
}

/// C's `memcpy` into `OVLx` (`aSubRecord.c:434`): a string lands in the
/// buffer `dst` already owns; an array clone only shares the buffer.
fn copy_output(dst: &mut EpicsValue, src: &EpicsValue) {
    match (dst, src) {
        (EpicsValue::String(d), EpicsValue::String(s)) => d.clone_from(s),
        (d, s) => *d = s.clone(),
    }
}

impl ASubRecord {
    /// [`Record::monitor_value_classes`] on a cycle with a `VALx`/`NEVx`
    /// written since the last one: out of line, so a clean cycle takes no
    /// frame.
    #[inline(never)]
    fn monitor_outputs(&mut self, changed: bool) -> MonitorValueClasses {
        let marked = self.eflg == EFLG_ON_CHANGE && self.mark_changed_outputs();
        monitor_classes(changed, marked)
    }

    /// The ON CHANGE arm of C `monitor()` (`aSubRecord.c:422-438`) over the
    /// channels written since it last ran; true when it marked any.
    fn mark_changed_outputs(&mut self) -> bool {
        let mut dirty = std::mem::take(&mut self.out_dirty);
        while dirty != 0 {
            let i = dirty.trailing_zeros() as usize;
            dirty &= dirty - 1;
            let nev = self.neva[i];
            if nev != self.onva[i] || !same_prefix(&self.ovla[i], &self.vala[i], nev) {
                copy_output(&mut self.ovla[i], &self.vala[i]);
                self.val_marks |= 1 << i;
                if nev != self.onva[i] {
                    self.onva[i] = nev;
                    self.nev_marks |= 1 << i;
                }
            }
        }
        self.val_marks != 0
    }

    #[inline(never)]
    fn take_output_marks(&mut self) -> CycleMarks {
        let val_marks = std::mem::take(&mut self.val_marks);
        let nev_marks = std::mem::take(&mut self.nev_marks);
        let mut bits = 0u64;
        for i in 0..NUM_ARGS {
            bits |= u64::from(val_marks >> i & 1) << (2 * i);
            bits |= u64::from(nev_marks >> i & 1) << (2 * i + 1);
        }
        CycleMarks::of(&OUTPUT_EVENT_POSTS, u128::from(bits))
    }
}

fn monitor_classes(changed: bool, marked: bool) -> MonitorValueClasses {
    MonitorValueClasses {
        value: changed,
        archive: changed,
        hash_changed: false,
        marked,
    }
}

/// C's `memcmp(povl, pval, dbValueSize(ftv) * nev)` (`aSubRecord.c:432`): the
/// first `nev` elements compared as bytes, so a NaN equals its own bits.
fn same_prefix(old: &EpicsValue, new: &EpicsValue, nev: i32) -> bool {
    fn head<T>(v: &[T], n: usize) -> &[T] {
        &v[..n.min(v.len())]
    }
    fn bits<T: Copy, K: PartialEq>(a: &[T], b: &[T], n: usize, key: fn(T) -> K) -> bool {
        head(a, n)
            .iter()
            .map(|v| key(*v))
            .eq(head(b, n).iter().map(|v| key(*v)))
    }
    let n = nev.max(0) as usize;
    if n == 0 {
        return true;
    }
    use EpicsValue as V;
    match (old, new) {
        (V::Double(a), V::Double(b)) => a.to_bits() == b.to_bits(),
        (V::Float(a), V::Float(b)) => a.to_bits() == b.to_bits(),
        (V::DoubleArray(a), V::DoubleArray(b)) => bits(a, b, n, f64::to_bits),
        (V::FloatArray(a), V::FloatArray(b)) => bits(a, b, n, f32::to_bits),
        (V::CharArray(a), V::CharArray(b)) => head(a, n) == head(b, n),
        (V::UCharArray(a), V::UCharArray(b)) => head(a, n) == head(b, n),
        (V::ShortArray(a), V::ShortArray(b)) => head(a, n) == head(b, n),
        (V::UShortArray(a), V::UShortArray(b)) => head(a, n) == head(b, n),
        (V::LongArray(a), V::LongArray(b)) => head(a, n) == head(b, n),
        (V::ULongArray(a), V::ULongArray(b)) => head(a, n) == head(b, n),
        (V::Int64Array(a), V::Int64Array(b)) => head(a, n) == head(b, n),
        (V::UInt64Array(a), V::UInt64Array(b)) => head(a, n) == head(b, n),
        (V::StringArray(a), V::StringArray(b)) => head(a, n) == head(b, n),
        (a, b) => a == b,
    }
}

impl Record for ASubRecord {
    crate::slot_map!(crate::server::record::dbd_generated::slots::aSub;
        fields:
        VAL => val,
        OVAL => oval,
        SNAM => snam,
        INAM => inam,
        ONAM => onam,
        PREC => prec;
        custom: LFLG, SUBL, BRSV, EFLG, INPA, INPB, INPC, INPD, INPE, INPF, INPG, INPH, INPI,
            INPJ, INPK, INPL, INPM, INPN, INPO, INPP, INPQ, INPR, INPS, INPT, INPU, A, B, C, D,
            E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, FTA, FTB, FTC, FTD, FTE, FTF,
            FTG, FTH, FTI, FTJ, FTK, FTL, FTM, FTN, FTO, FTP, FTQ, FTR, FTS, FTT, FTU, NOA, NOB,
            NOC, NOD, NOE, NOF, NOG, NOH, NOI, NOJ, NOK, NOL, NOM, NON, NOO, NOP, NOQ, NOR, NOS,
            NOT, NOU, NEA, NEB, NEC, NED, NEE, NEF, NEG, NEH, NEI, NEJ, NEK, NEL, NEM, NEN, NEO,
            NEP, NEQ, NER, NES, NET, NEU, ONVA, ONVB, ONVC, ONVD, ONVE, ONVF, ONVG, ONVH, ONVI,
            ONVJ, ONVK, ONVL, ONVM, ONVN, ONVO, ONVP, ONVQ, ONVR, ONVS, ONVT, ONVU, OUTA, OUTB, OUTC, OUTD, OUTE, OUTF, OUTG, OUTH, OUTI,
            OUTJ, OUTK, OUTL, OUTM, OUTN, OUTO, OUTP, OUTQ, OUTR, OUTS, OUTT, OUTU, VALA, VALB,
            VALC, VALD, VALE, VALF, VALG, VALH, VALI, VALJ, VALK, VALL, VALM, VALN, VALO, VALP,
            VALQ, VALR, VALS, VALT, VALU, FTVA, FTVB, FTVC, FTVD, FTVE, FTVF, FTVG, FTVH, FTVI,
            FTVJ, FTVK, FTVL, FTVM, FTVN, FTVO, FTVP, FTVQ, FTVR, FTVS, FTVT, FTVU, NOVA, NOVB,
            NOVC, NOVD, NOVE, NOVF, NOVG, NOVH, NOVI, NOVJ, NOVK, NOVL, NOVM, NOVN, NOVO, NOVP,
            NOVQ, NOVR, NOVS, NOVT, NOVU, NEVA, NEVB, NEVC, NEVD, NEVE, NEVF, NEVG, NEVH, NEVI,
            NEVJ, NEVK, NEVL, NEVM, NEVN, NEVO, NEVP, NEVQ, NEVR, NEVS, NEVT, NEVU;
        spill: SADR, CADR, OVLA, OVLB, OVLC, OVLD, OVLE, OVLF, OVLG, OVLH, OVLI, OVLJ, OVLK,
            OVLL, OVLM, OVLN, OVLO, OVLP, OVLQ, OVLR, OVLS, OVLT, OVLU);

    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        crate::server::record::CycleHooks::MONITOR_SETS
    }

    fn has_array_monitor_post(&self) -> bool {
        false
    }

    /// C `monitor()` (`aSubRecord.c:405-451`), entered once per cycle: VAL
    /// against OVAL (`:414-417`), then under `EFLG=ON CHANGE` each `VALx`
    /// against the `OVLx` it last posted, not against the cycle's start
    /// (`:422-438`): a client put to `VALx` moves neither copy, so the next
    /// cycle posts it again. The marks are made here and reported as
    /// `marked`, so the cycle that made them takes them.
    fn monitor_value_classes(
        &mut self,
        _gate: crate::server::record::MonitorValueGate,
        _last_posted: &mut [f64; 2],
    ) -> MonitorValueClasses {
        let changed = self.monitor_value_changed() == Some(true);
        if self.out_dirty == 0 {
            return monitor_classes(changed, false);
        }
        self.monitor_outputs(changed)
    }

    /// `aSubRecord.c:414-417`.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let changed = self.val != self.oval;
        if changed {
            self.oval = self.val;
        }
        Some(changed)
    }

    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        if self.val_marks == 0 {
            return CycleMarks::NONE;
        }
        self.take_output_marks()
    }

    /// `VALx`/`NEVx` post from `monitor()`'s own `OVLx` test (or EFLG=ALWAYS),
    /// never from the framework's change detection.
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        asub_output_event_fields()
    }

    /// C `initFields` (`aSubRecord.c:118-121`, `:176-206`): pass 0 runs before
    /// any SNAM early return, so `ONVx` is `NOVx` even when init fails.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.nea = self.noa;
            self.neva = self.nova;
            self.onva = self.neva;
        }
        Ok(())
    }

    fn init_record_tail(&mut self, _: &mut crate::server::record::CommonFields) {
        // C `init_record`'s tail (`aSubRecord.c:164-171`).
        self.ovla = self.vala.clone();
        self.onva = self.neva;
        self.out_dirty = 0;
    }

    fn alarm_alone_posts_value(&self) -> bool {
        false
    }

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
        let (chan, idx) = parse_channel(field)?;
        match chan {
            Chan::Value => Some(self.noa[idx].max(0) as u32),
            Chan::Val => Some(self.nova[idx].max(0) as u32),
            _ => None,
        }
    }

    fn dbaddr_stored_as_scalar(&self, field: &str) -> bool {
        match parse_channel(field) {
            Some((Chan::Value, idx)) => !self.a[idx].is_array(),
            Some((Chan::Val, idx)) => channel_is_scalar(&self.vala[idx]),
            _ => false,
        }
    }

    /// C reads `prec->inpa..inpu` off the record and copies nothing; the generic
    /// `get_field` path hands back an owned `EpicsValue` per link, which is
    /// 21 clones on every cycle of a record that wires none of them.
    fn link_text_ref(&self, link_field: &str) -> Option<&[u8]> {
        let [b'I', b'N', b'P', slot] = *link_field.as_bytes() else {
            return None;
        };
        let slot = usize::from(slot.checked_sub(b'A')?);
        self.inp.get(slot).map(PvString::as_bytes)
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        // Channels first: no scalar name below parses as one, and the OUT
        // multi-output dispatch asks for all 21 `OUTx` every cycle.
        if let Some((chan, idx)) = parse_channel(name) {
            return Some(match chan {
                Chan::Value => self.a[idx].clone(),
                Chan::Inp => EpicsValue::String(self.inp[idx].clone().into()),
                Chan::Val => channel_get(&self.vala[idx]),
                Chan::Out => EpicsValue::String(self.out[idx].clone().into()),
                Chan::Ft => EpicsValue::Short(self.fta[idx]),
                Chan::Ftv => EpicsValue::Short(self.ftva[idx]),
                // DBF_ULONG: the bits C's epicsUInt32 holds.
                Chan::No => EpicsValue::ULong(self.noa[idx] as u32),
                Chan::Nov => EpicsValue::ULong(self.nova[idx] as u32),
                Chan::Ne => EpicsValue::ULong(self.nea[idx] as u32),
                Chan::Nev => EpicsValue::ULong(self.neva[idx] as u32),
                Chan::Onv => EpicsValue::ULong(self.onva[idx] as u32),
            });
        }
        match name {
            "VAL" => Some(EpicsValue::Long(self.val)),
            "OVAL" => Some(EpicsValue::Long(self.oval)),
            "SNAM" => Some(EpicsValue::String(self.snam.clone())),
            "INAM" => Some(EpicsValue::String(self.inam.clone())),
            "BRSV" => Some(EpicsValue::Short(self.brsv)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "EFLG" => Some(EpicsValue::Short(self.eflg)),
            "LFLG" => Some(EpicsValue::Short(self.lflg)),
            "SUBL" => Some(EpicsValue::String(self.subl.clone().into())),
            "ONAM" => Some(EpicsValue::String(self.onam.clone())),
            _ => None,
        }
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
            // SPC_NOMOD: only the init tail writes it.
            "OVAL" => {
                self.oval = value
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
                        self.subl = s;
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
        let (chan, idx) =
            parse_channel(name).ok_or_else(|| CaError::FieldNotFound(name.to_string()))?;
        if matches!(chan, Chan::Val | Chan::Ftv | Chan::Nov | Chan::Nev) {
            self.out_dirty |= 1 << idx;
        }
        match chan {
            // Input value A..U — shaped to the channel's declared FTx/NOx.
            // C `fetch_values` lands every link value in the FTx-typed
            // NOx-element buffer (aSubRecord.c:278-288); a direct put takes
            // the same conversion through `dbPut` into that buffer. NEx
            // tracks the elements actually delivered.
            Chan::Value => match shape_channel_value(value, self.fta[idx], self.noa[idx]) {
                ChannelDelivery::Delivered { value, count } => {
                    self.nea[idx] = count;
                    self.a[idx] = value;
                }
                ChannelDelivery::Empty => self.nea[idx] = 0,
            },
            Chan::Inp | Chan::Out => {
                let EpicsValue::String(s) = value else {
                    return Err(CaError::TypeMismatch(name.into()));
                };
                if chan == Chan::Inp {
                    self.inp[idx] = s;
                    self.links_generation += 1;
                } else {
                    self.out[idx] = s;
                    self.wired_outs = asub_output_links()
                        .iter()
                        .zip(&self.out)
                        .filter(|(_, out)| !out.is_empty())
                        .map(|(pair, _)| *pair)
                        .collect();
                }
            }
            // Output value VALA..VALU — shaped to the channel's declared
            // FTVx/NOVx, the same rule as the input side: a C subroutine
            // writes INTO the FTVx-typed NOVx-element buffer, so no other
            // shape can leave `do_sub`, and the OUT push reads that buffer
            // (`dbPutLink(&(&prec->outa)[i], (&prec->ftva)[i], ...)`,
            // aSubRecord.c:237-239). NEVx tracks the elements written.
            Chan::Val => match shape_channel_value(value, self.ftva[idx], self.nova[idx]) {
                ChannelDelivery::Delivered { value, count } => {
                    self.neva[idx] = count;
                    self.vala[idx] = value;
                }
                ChannelDelivery::Empty => self.neva[idx] = 0,
            },
            Chan::Ft | Chan::Ftv | Chan::No | Chan::Nov | Chan::Ne | Chan::Nev | Chan::Onv => {
                let v = match value {
                    EpicsValue::Short(v) => v as i32,
                    EpicsValue::Long(v) => v,
                    other => other
                        .to_f64()
                        .map(|f| f as i32)
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?,
                };
                match chan {
                    // FTx/NOx and FTVx/NOVx declare their cell's element type
                    // and capacity: C `initFields` clamps them (out-of-menu
                    // FTx -> CHAR at aSubRecord.c:186-187, NOx 0 -> 1 at
                    // :189-190) and allocates the cell as a zero-filled
                    // FTx-typed buffer with NEx = NOx (:192-196) — run for
                    // both the input and the output triple. All four are
                    // SPC_NOMOD, so these puts are the load path —
                    // re-deriving the cell here is that allocation.
                    Chan::Ft => {
                        self.fta[idx] = channel_ftype(v as i16).index();
                        self.links_generation += 1;
                        self.a[idx] = channel_default(self.fta[idx], self.noa[idx]);
                        self.nea[idx] = self.noa[idx].max(1);
                    }
                    Chan::No => {
                        self.noa[idx] = v.max(1);
                        self.links_generation += 1;
                        self.a[idx] = channel_default(self.fta[idx], self.noa[idx]);
                        self.nea[idx] = self.noa[idx];
                    }
                    Chan::Ftv => {
                        self.ftva[idx] = channel_ftype(v as i16).index();
                        self.vala[idx] = channel_default(self.ftva[idx], self.nova[idx]);
                        self.neva[idx] = self.nova[idx].max(1);
                    }
                    Chan::Nov => {
                        self.nova[idx] = v.max(1);
                        self.vala[idx] = channel_default(self.ftva[idx], self.nova[idx]);
                        self.neva[idx] = self.nova[idx];
                    }
                    Chan::Ne => self.nea[idx] = v,
                    Chan::Nev => self.neva[idx] = v,
                    // SPC_NOMOD: the load path only.
                    Chan::Onv => self.onva[idx] = v,
                    _ => unreachable!(),
                }
            }
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

    fn force_posted_fields(&self) -> crate::server::record::DeclaredFields {
        // EFLG=ALWAYS: C `monitor()` posts every `vala[i]`/`neva[i]` each
        // process regardless of change — the framework's force-post path.
        static OUTPUT_EVENTS: [crate::server::record::FieldIx; 2 * NUM_ARGS] =
            crate::server::record::declared_set(
                crate::server::record::dbd_generated::A_SUB_FIELDS,
                OUTPUT_EVENT_FIELDS,
            );
        if self.eflg == EFLG_ALWAYS {
            &OUTPUT_EVENTS
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
    fn process_posted_fields(&self) -> Option<crate::server::record::DeclaredFields> {
        Some(if self.eflg == EFLG_NEVER {
            &const {
                crate::server::record::declared_set(
                    crate::server::record::dbd_generated::A_SUB_FIELDS,
                    ["VAL"],
                )
            }
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
    /// INPA..INPU with the FTx type and `n = NOx`. Both are init-only: at
    /// process `dbGetLink` on a constant delivers nothing, so SNAM and A..U
    /// keep what was seeded (or what a client later put).
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        use crate::server::record::ConstantInitLink;
        let mut seeds = vec![ConstantInitLink::new("SUBL", "SNAM")];
        seeds.extend(
            self.multi_input_links()
                .iter()
                .enumerate()
                .map(|(i, (link, value))| {
                    ConstantInitLink::load_array(
                        link,
                        value,
                        channel_ftype(self.fta[i]).element_type(),
                        self.noa[i].max(0) as usize,
                    )
                }),
        );
        seeds
    }

    /// The `INPA..INPU` texts read straight off the record's own array, not
    /// through the default body's one name match per link.
    /// See [`Record::set_input_link_slots`].
    fn set_input_link_slots(&self) -> Option<(u64, u64)> {
        crate::server::record::input_link_slots_of(&self.inp)
    }

    fn multi_input_links(&self) -> &'static [(&'static str, &'static str)] {
        &INPUT_LINKS
    }

    /// C `aSubRecord.c::fetch_values` (277-289) returns on the first failed
    /// `dbGetLink` and `process` (216-218) then skips `do_sub`.
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::AbortOnFirstFailure
    }

    /// C `fetch_values` asks `dbGetLink` for the channel's declared FTx with
    /// `&nRequest = NOx` (`aSubRecord.c:283-284`): a counted read at every
    /// width, so an empty source delivers nothing rather than failing, and
    /// no read resolves the source first. `SUBL` is
    /// `dbGetLink(&subl, DBR_STRING, ...)` (`aSubRecord.c:256`).
    fn input_link_request(&self, link_field: &str) -> InputLinkRequest {
        if link_field == "SUBL" {
            return InputLinkRequest::As(LinkReadAs::String);
        }
        let Some(idx) = parse_channel(link_field)
            .filter(|&(chan, _)| chan == Chan::Inp)
            .map(|(_, idx)| idx)
        else {
            return InputLinkRequest::As(LinkReadAs::Native);
        };
        let read = ftype_read(channel_ftype(self.fta[idx]));
        InputLinkRequest::As(match read.dbr_type() {
            Some(dbr) => LinkReadAs::Array {
                dbr,
                n_request: self.noa[idx].max(1) as usize,
            },
            None => read,
        })
    }

    /// `INPx` lands in its value field.
    fn array_read_destination(&self, link_field: &str) -> Option<EpicsValue> {
        let (chan, idx) = parse_channel(link_field)?;
        (chan == Chan::Inp).then(|| self.a[idx].clone())
    }

    /// NEA moves only on success (`aSubRecord.c:283-287`).
    fn land_failed_array_read(&mut self, link_field: &str, left: EpicsValue) {
        if let Some((Chan::Inp, idx)) = parse_channel(link_field) {
            self.a[idx].write_head(left);
        }
    }

    /// `FTA..FTU` are fields, so the request above is the instance's.
    fn input_link_answers_fixed_at_type(&self) -> bool {
        false
    }

    fn input_links_generation(&self) -> Option<u64> {
        Some(self.links_generation)
    }

    /// The cycle status from the framework's single `do_sub` owner. Stored
    /// verbatim; [`Self::multi_output_links`] is its only reader.
    fn set_subroutine_status(&mut self, status: i64) {
        self.sub_status = status;
    }

    /// C `aSubRecord.c::process` (234-240) pushes EVERY output link once the
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
    /// the status itself, not a per-link condition. Only the wired pairs are
    /// lent: C's `dbPutLink` on an unset link is a no-op.
    fn declares_multi_output_links(&self) -> bool {
        true
    }

    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        if self.sub_status == 0 {
            &self.wired_outs
        } else {
            &[]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only wired OUTx pairs are driven, rewired by a put to OUTx and
    /// withheld while the cycle status is non-zero.
    #[test]
    fn multi_output_links_lends_the_wired_pairs() {
        let mut rec = ASubRecord::default();
        let out = |rec: &mut ASubRecord, link: &str, text: &str| {
            rec.put_field(link, EpicsValue::String(text.into()))
                .unwrap();
        };
        out(&mut rec, "OUTC", "X");
        out(&mut rec, "OUTA", "Y");
        assert!(rec.multi_output_links().is_empty(), "never ran");
        rec.set_subroutine_status(0);
        assert_eq!(
            rec.multi_output_links(),
            [("OUTA", "VALA"), ("OUTC", "VALC")]
        );
        out(&mut rec, "OUTA", "");
        out(&mut rec, "OUTU", "Z");
        assert_eq!(
            rec.multi_output_links(),
            [("OUTC", "VALC"), ("OUTU", "VALU")]
        );
        rec.set_subroutine_status(-1);
        assert!(rec.multi_output_links().is_empty());
    }

    /// An ON CHANGE string output lands in the `OVLx` buffer it already
    /// holds, as C's `memcpy` into `povl` does: no allocation per cycle.
    #[test]
    fn changed_string_output_reuses_the_ovl_buffer() {
        let mut rec = ASubRecord::default();
        rec.put_field("FTVA", EpicsValue::Short(0)).unwrap(); // STRING
        let ovl_ptr = |rec: &ASubRecord| match &rec.ovla[0] {
            EpicsValue::String(s) => s.as_bytes().as_ptr(),
            other => panic!("OVLA is {other:?}"),
        };
        rec.put_field("VALA", EpicsValue::String("first".into()))
            .unwrap();
        assert!(rec.mark_changed_outputs());
        let first = ovl_ptr(&rec);
        rec.put_field("VALA", EpicsValue::String("other".into()))
            .unwrap();
        assert!(rec.mark_changed_outputs());
        assert_eq!(ovl_ptr(&rec), first);
        assert_eq!(rec.ovla[0], EpicsValue::String("other".into()));
    }

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
            rec.put_field(name, EpicsValue::DoubleArray(vec![1.0, 2.0].into()))
                .unwrap();
            assert_eq!(
                rec.get_field(name),
                Some(EpicsValue::DoubleArray(vec![1.0, 2.0].into()))
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
        rec.put_field("C", EpicsValue::LongArray(vec![10, 20, 30].into()))
            .unwrap();
        assert_eq!(
            rec.get_field("C"),
            Some(EpicsValue::LongArray(vec![10, 20, 30].into()))
        );
        // NEC tracks elements used.
        assert_eq!(rec.get_field("NEC"), Some(EpicsValue::ULong(3)));
    }

    /// The contract of [`Record::input_links_generation`]: while it stands
    /// still, no `INPx` text and no `input_link_request` answer has moved.
    #[test]
    fn the_links_generation_moves_with_every_text_and_request() {
        let answers = |rec: &ASubRecord| {
            rec.multi_input_links()
                .iter()
                .map(|&(link, _)| {
                    (
                        rec.link_text_ref(link).map(<[u8]>::to_vec),
                        rec.input_link_request(link),
                    )
                })
                .collect::<Vec<_>>()
        };
        let mut rec = ASubRecord::default();
        let steps: [(&str, EpicsValue); 8] = [
            ("INPA", EpicsValue::String("SRC".into())),
            ("FTA", EpicsValue::Short(0)),
            ("NOA", EpicsValue::Long(4)),
            ("A", EpicsValue::String("x".into())),
            ("FTVB", EpicsValue::Short(0)),
            ("OUTB", EpicsValue::String("DST".into())),
            ("FTA", EpicsValue::Short(10)),
            ("INPU", EpicsValue::String("".into())),
        ];
        let mut moved = 0;
        for (field, value) in steps {
            let (before, generation) = (answers(&rec), rec.input_links_generation());
            rec.put_field(field, value).unwrap();
            if rec.input_links_generation() == generation {
                assert_eq!(answers(&rec), before, "{field} changed an answer in place");
            } else {
                moved += 1;
            }
        }
        assert_eq!(moved, 5, "every INPx, FTx and NOx put moves the generation");
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
            Some(EpicsValue::DoubleArray(vec![0.0; 4].into()))
        );
        assert_eq!(
            rec.get_field("NEA"),
            Some(EpicsValue::ULong(4)),
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
        assert_eq!(
            rec.get_field("C"),
            Some(EpicsValue::CharArray(vec![0, 0].into()))
        );
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
        rec.put_field(
            "A",
            EpicsValue::DoubleArray(vec![1.9, 2.2, 3.7, 4.4].into()),
        )
        .unwrap();
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::LongArray(vec![1, 2, 3].into()))
        );
        assert_eq!(rec.get_field("NEA"), Some(EpicsValue::ULong(3)));

        // A scalar source fills one element of an array cell (nRequest = 1).
        rec.put_field("A", EpicsValue::Double(7.0)).unwrap();
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::LongArray(vec![7].into()))
        );
        assert_eq!(rec.get_field("NEA"), Some(EpicsValue::ULong(1)));

        // An array into a one-element cell keeps element 0 (C's one-element
        // destination, same rule as the store's scalar reduction).
        rec.put_field("B", EpicsValue::DoubleArray(vec![5.5, 6.6].into()))
            .unwrap();
        assert_eq!(rec.get_field("B"), Some(EpicsValue::Double(5.5)));
        assert_eq!(rec.get_field("NEB"), Some(EpicsValue::ULong(1)));

        // An EMPTY array source delivers zero elements — dbGet clamps
        // nRequest to the source count (dbAccess.c:999-1000) and NEx records
        // it (aSubRecord.c:287) — but the clamped count then runs NO
        // converter (dbAccess.c:1016-1017), so the cell keeps what the last
        // real delivery left there. Both cell shapes.
        rec.put_field("B", EpicsValue::DoubleArray(vec![].into()))
            .unwrap();
        assert_eq!(
            rec.get_field("B"),
            Some(EpicsValue::Double(5.5)),
            "one-element cell retains the previous delivery"
        );
        assert_eq!(rec.get_field("NEB"), Some(EpicsValue::ULong(0)));
        rec.put_field("A", EpicsValue::DoubleArray(vec![].into()))
            .unwrap();
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::LongArray(vec![7].into())),
            "NOx > 1 cell retains the previous delivery"
        );
        assert_eq!(rec.get_field("NEA"), Some(EpicsValue::ULong(0)));

        // A non-empty delivery after the empty one overwrites normally.
        rec.put_field("A", EpicsValue::DoubleArray(vec![8.0, 9.0].into()))
            .unwrap();
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::LongArray(vec![8, 9].into()))
        );
        assert_eq!(rec.get_field("NEA"), Some(EpicsValue::ULong(2)));
    }

    /// An output cell follows the same `dbGet` rule: a subroutine that writes
    /// nothing into `vala[i]` leaves the buffer alone and only moves NEVx.
    #[test]
    fn empty_output_write_retains_the_output_cell() {
        let mut rec = ASubRecord::default();
        rec.put_field("FTVA", EpicsValue::Short(10)).unwrap(); // DOUBLE
        rec.put_field("NOVA", EpicsValue::Long(3)).unwrap();
        rec.put_field("VALA", EpicsValue::DoubleArray(vec![1.0, 2.0].into()))
            .unwrap();
        assert_eq!(rec.get_field("NEVA"), Some(EpicsValue::ULong(2)));

        rec.put_field("VALA", EpicsValue::DoubleArray(vec![].into()))
            .unwrap();
        assert_eq!(
            rec.get_field("VALA"),
            Some(EpicsValue::DoubleArray(vec![1.0, 2.0].into()))
        );
        assert_eq!(rec.get_field("NEVA"), Some(EpicsValue::ULong(0)));
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
            Some(EpicsValue::DoubleArray(vec![0.0; 3].into()))
        );
        assert_eq!(rec.get_field("NEVA"), Some(EpicsValue::ULong(3)));

        rec.put_field("VALA", EpicsValue::LongArray(vec![1, 2, 3, 4].into()))
            .unwrap();
        assert_eq!(
            rec.get_field("VALA"),
            Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into())),
            "a subroutine write converts element-wise and clamps to NOVx"
        );
        assert_eq!(rec.get_field("NEVA"), Some(EpicsValue::ULong(3)));

        rec.put_field("FTVB", EpicsValue::Short(0)).unwrap(); // STRING
        rec.put_field("VALB", EpicsValue::String("done".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("VALB"),
            Some(EpicsValue::String("done".into())),
            "a declared STRING output keeps its string"
        );
    }

    /// C `fetch_values` requests the channel's FTx from `dbGetLink` with
    /// `nRequest = NOx` (aSubRecord.c:283-284): every channel settles from its
    /// own FTx/NOx, whatever its source is.
    #[test]
    fn every_channel_reads_at_its_ftx_and_nox_source_unasked() {
        let mut rec = ASubRecord::default();
        rec.put_field("FTC", EpicsValue::Short(0)).unwrap(); // STRING
        assert_eq!(
            rec.input_link_request("INPC"),
            InputLinkRequest::As(LinkReadAs::Array {
                dbr: crate::types::DbFieldType::String,
                n_request: 1
            })
        );
        rec.put_field("NOC", EpicsValue::Long(16)).unwrap();
        assert_eq!(
            rec.input_link_request("INPC"),
            InputLinkRequest::As(LinkReadAs::Array {
                dbr: crate::types::DbFieldType::String,
                n_request: 16
            })
        );
        // A one-element numeric channel settles WITHOUT the source, so the
        // framework never resolves the target for it.
        assert_eq!(
            rec.input_link_request("INPA"),
            InputLinkRequest::As(LinkReadAs::Array {
                dbr: crate::types::DbFieldType::Double,
                n_request: 1
            }),
            "a DOUBLE channel reads one DBR_DOUBLE, source unasked"
        );
        assert_eq!(
            rec.input_link_request("SUBL"),
            InputLinkRequest::As(LinkReadAs::String),
            "SUBL is read as DBR_STRING"
        );
        assert_eq!(
            rec.input_link_request("SIML"),
            InputLinkRequest::As(LinkReadAs::Native),
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
        let names = |set: &[crate::server::record::FieldIx]| {
            set.iter()
                .map(|&ix| {
                    crate::server::record::field_at(
                        crate::server::record::dbd_generated::A_SUB_FIELDS,
                        ix,
                    )
                    .expect("declared")
                    .name
                })
                .collect::<Vec<_>>()
        };
        let posted = names(rec.process_posted_fields().expect("aSub closes the set"));
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
        assert!(names(rec.force_posted_fields()).contains(&"VALA"));
        assert!(names(rec.force_posted_fields()).contains(&"NEVU"));
        assert_eq!(
            rec.process_posted_fields().map(<[_]>::len),
            Some(NUM_ARGS * 2 + 1)
        );

        // NEVER: the set narrows to VAL, which C posts outside the switch.
        rec.put_field("EFLG", EpicsValue::Short(EFLG_NEVER))
            .unwrap();
        assert!(rec.force_posted_fields().is_empty());
        assert_eq!(rec.process_posted_fields().map(names), Some(vec!["VAL"]));
    }

    /// EFLG loads from a `.db` menu label (C `field(EFLG,"ALWAYS")`).
    #[test]
    fn eflg_loads_from_menu_label() {
        use crate::server::db_loader::{DbFieldDef, apply_fields, create_record};
        let mut rec = create_record("aSub").unwrap();
        let mut common = Vec::new();
        apply_fields(&mut rec, &[DbFieldDef::new("EFLG", "ALWAYS")], &mut common)
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
        use crate::server::db_loader::{DbFieldDef, apply_fields, create_record};
        let mut rec = create_record("aSub").unwrap();
        let mut common = Vec::new();
        apply_fields(
            &mut rec,
            &[
                DbFieldDef::new("LFLG", "READ"),
                DbFieldDef::new("SUBL", "NAME_HOLDER"),
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
