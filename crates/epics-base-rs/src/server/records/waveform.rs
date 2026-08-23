use crate::error::{CaError, CaResult};
use crate::server::record::{
    FieldMetadataOverride, Ftype, MENU_FTYPE, MENU_YES_NO, ProcessAction, ProcessOutcome, Record,
    parse_link_v2,
};
use crate::server::records::count_put;
use crate::types::{DbFieldType, EpicsValue, PvString, c_parse};

/// Which EPICS record-type name a [`WaveformRecord`] reports. The four
/// upstream array record types (`waveform`, `aai`, `aao`, `subArray`)
/// share the same scalar fields and DBR encoding; differentiation is
/// only at the record-type string and (for `aao`) the output-record
/// flag. Keeping them as one storage type avoids 1500 LOC of
/// duplication while preserving each type's identity to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayKind {
    Waveform,
    Aai,
    Aao,
    SubArray,
}

impl ArrayKind {
    pub fn as_record_type(self) -> &'static str {
        match self {
            Self::Waveform => "waveform",
            Self::Aai => "aai",
            Self::Aao => "aao",
            Self::SubArray => "subArray",
        }
    }

    /// `aao` is an output record (the framework calls `device.write`);
    /// the rest are input. Drives [`Record::can_device_write`].
    pub fn is_output(self) -> bool {
        matches!(self, Self::Aao)
    }
}

/// Waveform record — manual Record impl (no macro). Also serves as the
/// storage for `aai`, `aao`, and `subArray` since the four share their
/// scalar surface. The [`Self::kind`] field selects the reported
/// `record_type()` and the output/input distinction.
pub struct WaveformRecord {
    pub kind: ArrayKind,
    pub val: EpicsValue,
    pub nelm: i32,
    pub nord: i32,
    /// `FTVL` — the element type of the VAL buffer, C's `prec->ftvl`. Held as the
    /// TYPE, not as the menu index: [`Ftype`] is the single owner of the
    /// index↔element-type mapping, so an FTVL the buffer has no storage for is
    /// unrepresentable.
    ///
    /// Private, and written only by [`Self::set_ftvl`], which reallocates VAL in
    /// the same breath (C allocates `bptr` from `dbValueSize(ftvl)` and never
    /// re-types it independently). A `pub` field let a caller retype FTVL while
    /// VAL kept its old element variant — the desync the `field_list`/`get_field`
    /// pair then reports two different native types from.
    ftvl: Ftype,
    pub mpst: i16, // Monitor Post Mode: 0=Always, 1=OnChange
    pub apst: i16, // Archive Post Mode: 0=Always, 1=OnChange
    pub hash: u32, // Hash of array for OnChange detection
    /// C `BUSY` (`DBF_SHORT`, `special(SPC_NOMOD)`): waveform acquisition-active
    /// flag, set by waveform device support (e.g. `devAsynXXXTimeSeries`) and
    /// read-only to CA clients. waveformRecord.dbd.pod:461. Waveform kind only.
    pub busy: bool,
    /// C `RARM` (`DBF_SHORT`, `pp(TRUE)`): re-arm acquisition control read by
    /// waveform device support: 1=start (clear, arm), 2=stop, 3=resume, 0=no-op.
    /// The device resets it to 0 each process. waveformRecord.dbd.pod:411.
    /// Waveform kind only (aai/aao/subArray do not declare it).
    pub rarm: i16,
    pub egu: PvString,
    pub hopr: f64,
    pub lopr: f64,
    pub prec: i16,
    /// subArray-only: starting offset into the source array. Out-of-
    /// range values clamp to the source length; NORD=0 in that case.
    /// Ignored when `kind != SubArray`.
    pub indx: i32,
    /// subArray-only: declared maximum length of the source array.
    /// Used as an additional upper bound when computing the slice end:
    /// `end = min(indx + nelm, min(source_len, malm))`. Defaults to 0
    /// for non-subArray kinds — those records ignore the field
    /// altogether.
    pub malm: i32,
    /// Simulation block (waveform/aai/aao only; subArray has no sim block).
    /// SIMM is DBF_MENU menu(menuYesNo), SIMS is menu(menuAlarmSevr);
    /// SIML/SIOL the sim in/out links. waveformRecord.dbd.pod:475-507,
    /// aaiRecord.dbd.pod:374-402, aaoRecord.dbd.pod:407-435. SSCN (menuScan)
    /// and OLDSIMM (menuSimm, SPC_NOMOD) are served by the common path
    /// (`CommonFields::sscn` / `CommonFields::oldsimm`) — framework state
    /// written only by the simulation-mode owner
    /// (`RecordInstance::rec_gbl_save_simm` / `rec_gbl_check_simm`).
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    /// SDLY — "Sim. Mode Async Delay" (`DBF_DOUBLE`, `initial("-1.0")`;
    /// aaiRecord.dbd.pod:409-415, aaoRecord.dbd.pod:442-448,
    /// waveformRecord.dbd.pod:510-516). A non-negative SDLY makes the
    /// simulated SIOL round-trip asynchronous: C's `readValue`/`writeValue`
    /// arms the `callbackRequestProcessCallbackDelayed(..., prec->sdly)`
    /// watchdog and holds PACT across the delay (aaiRecord.c:365-374). The
    /// framework reads it via `get_field("SDLY")`, so the field must exist —
    /// an absent SDLY defaults to -1.0 (synchronous) there.
    pub sdly: f64,
    /// aao-only: output mode select, `menu(menuOmsl)` (0=supervisory,
    /// 1=closed_loop). When `closed_loop`, aao sources VAL from `DOL`
    /// before each write (C `aaoRecord.c::fetchValue`, 357). waveform/aai/
    /// subArray declare no OMSL — `aaoRecord.dbd.pod:355` is the only one
    /// of the four that does — so the field is exposed only when
    /// `kind == Aao`.
    pub omsl: i16,
    /// aao-only: desired-output-location input link. Pulled into VAL each
    /// process cycle when `omsl == closed_loop` and the link is a real
    /// (non-constant) link (C `aaoRecord.c::fetchValue` `dbGetLink`, 366).
    pub dol: String,
    /// Did `init_record` pass 1 load a constant closed-loop DOL into the
    /// buffer? C `fetchValue(prec, 1)` sets `prec->udf = FALSE` on success
    /// (aaoRecord.c:371-374); UDF lives in the common fields, which
    /// `init_record` cannot reach, so the outcome is carried here and folded
    /// in by `post_init_finalize_undef`.
    constant_dol_loaded: bool,
    /// aao closed-loop: did this cycle ask the framework to fetch DOL, and did
    /// that fetch fail? C `fetchValue(prec, 0)`'s `dbGetLink` status
    /// (aaoRecord.c:366-374), which `process` returns on (167-168). The
    /// framework reports the outcome through `set_resolved_input_links`.
    dol_fetch_requested: bool,
    dol_read_failed: bool,
    /// The INP counterpart of `dol_read_failed`: did the last soft-channel INP
    /// read fail? C carries it as `read_sa`'s local `status`, because C
    /// evaluates `udf = !!status` in the same `process` that ran the read; the
    /// port splits the read (framework) from the UDF derivation (the record),
    /// so the outcome has to be remembered across the seam. Set by
    /// [`Record::soft_input_read_failed`] and cleared by the two things that
    /// clear C's `udf`: [`Self::sa_subset`], which runs on exactly C's
    /// `if (!status) subset(...)` condition (devSASoft.c:118-120), and a put to
    /// VAL (`dbAccess.c:1415`). Only subArray reads it; the other three kinds
    /// clear UDF unconditionally whatever `readValue` returned.
    inp_read_failed: bool,
    /// The element count the record ARRIVED at `init_record` with — elements an
    /// in-process `record()` build put into VAL before `add_record`. C has no
    /// such path (VAL is `DBF_NOACCESS` on all four `.dbd`s, so a `.db` cannot
    /// fill the buffer and C's dsets are calloc'd-empty by construction), which
    /// is why C's `init_record`s are free to overwrite NORD outright. The port's
    /// builder path is real data, so the init-time NORD rules
    /// ([`Record::init_record`], [`Record::soft_input_dset_init`]) may only
    /// decide the count for a record that arrived EMPTY — never discard this.
    /// Captured at pass 0, before the record's own seed runs.
    prebuilt_nord: i32,
}

/// Type aliases for documentation / pattern-match clarity. All point
/// at [`WaveformRecord`] — runtime type discrimination is the
/// [`ArrayKind`] field.
pub type AaiRecord = WaveformRecord;
pub type AaoRecord = WaveformRecord;
pub type SubArrayRecord = WaveformRecord;

/// `menu(waveformPOST)` choice labels for the `MPST`/`APST` fields, in
/// `.dbd` value order (`waveformRecord.dbd.pod:20-23`). The order is the
/// *reverse* of `menu(menuPost)` — "Always" is index 0 here — and is
/// wire-visible, so this record keeps its own table rather than the
/// shared `MENU_POST`. `aai`/`aao` use the identically-ordered
/// `menu(aaiPOST)`/`menu(aaoPOST)`, so the same table serves every
/// [`ArrayKind`].
const WAVEFORM_POST: &[&str] = &["Always", "On Change"];

/// `menu(waveformPOST)` indices: `Always` posts every cycle, `On Change`
/// posts only when the array-content hash differs from the stored `HASH`.
const WAVEFORM_POST_ALWAYS: i16 = 0;
const WAVEFORM_POST_ONCHANGE: i16 = 1;

/// `epicsOldString` width — a STRING-FTVL element occupies a fixed
/// `MAX_STRING_SIZE`-byte slot in `bptr`, so the hash sees that many bytes
/// per element (null-padded), matching C's raw buffer layout.
const MAX_STRING_SIZE: usize = 40;

/// Port of EPICS `epicsMemHash` (epicsString.c:378-388), the array-content
/// hash used by waveform/aai/aao `monitor()` for On Change detection. It is
/// a Jenkins one-at-a-time variant that consumes bytes in pairs, applying
/// formula A to even byte positions and formula B to odd ones. C
/// dereferences `char` — signed on the x86_64 / aarch64 reference builds —
/// so each byte is sign-extended to 32 bits before the XOR; `b as i8 as
/// u32` reproduces that exactly.
fn epics_mem_hash(bytes: &[u8], seed: u32) -> u32 {
    let mut hash = seed;
    for (i, &b) in bytes.iter().enumerate() {
        let c = b as i8 as u32;
        if i % 2 == 0 {
            hash ^= !((hash << 11) ^ c ^ (hash >> 5));
        } else {
            hash ^= (hash << 7) ^ c ^ (hash >> 3);
        }
    }
    hash
}

impl Default for WaveformRecord {
    fn default() -> Self {
        Self {
            kind: ArrayKind::Waveform,
            val: EpicsValue::StringArray(Vec::new()),
            nelm: 1,
            nord: 0,
            // `field(FTVL,DBF_MENU){ menu(menuFtype) }` carries no `initial(...)`
            // in any of the four `.dbd`s, so the calloc'd record starts at menu
            // index 0 = STRING and a `record(waveform,"X"){}` serves `FTVL=STRING`
            // on a C IOC (measured: `caget -t P:WAVEFORM.FTVL` -> `STRING`).
            ftvl: Ftype::String,
            mpst: 0,
            apst: 0,
            hash: 0,
            busy: false,
            rarm: 0,
            egu: PvString::new(),
            hopr: 0.0,
            lopr: 0.0,
            prec: 0,
            indx: 0,
            // C `subArrayRecord.dbd.pod` `field(MALM,DBF_ULONG){ initial("1") }`;
            // C `init_record` also floors MALM to 1 (subArrayRecord.c:96-97), so
            // MALM is never 0 — it is always a real source-view cap. (Ignored by
            // non-subArray kinds, which never read the field.)
            malm: 1,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            // C `field(SDLY,DBF_DOUBLE) { initial("-1.0") }` — negative means
            // "synchronous simulation", the default on all three sim-bearing
            // array kinds.
            sdly: -1.0,
            // C `aaoRecord.dbd.pod` declares OMSL `menu(menuOmsl)` and DOL
            // `DBF_INLINK` with no `initial(...)`, so both default to the
            // zero value: OMSL=supervisory (no DOL fetch), DOL=constant/empty.
            omsl: 0,
            dol: String::new(),
            constant_dol_loaded: false,
            dol_fetch_requested: false,
            dol_read_failed: false,
            inp_read_failed: false,
            prebuilt_nord: 0,
        }
    }
}

impl WaveformRecord {
    /// Construct an array record with an explicit [`ArrayKind`].
    /// Lets `db_loader::create_record` mint `aai`, `aao`, or `subArray`
    /// without needing distinct types per record-type name.
    pub fn with_kind(kind: ArrayKind) -> Self {
        Self {
            kind,
            ..Default::default()
        }
    }

    /// True for the kinds whose `.dbd` declares a simulation block
    /// (waveform/aai/aao). `subArray` is a pure array-slicing record with
    /// no SIMM/SIML/SIOL/SIMS/OLDSIMM fields (`subArrayRecord.dbd.pod`), so
    /// it must not answer those names.
    fn has_sim_block(&self) -> bool {
        !matches!(self.kind, ArrayKind::SubArray)
    }

    /// True for the kinds whose `.dbd` declares BUSY, the `special(SPC_NOMOD)`
    /// acquisition-active indicator: waveform (`waveformRecord.dbd.pod:461`)
    /// and subArray (`subArrayRecord.dbd.pod:390`). aai/aao declare none.
    /// This is the single owner of BUSY's kind membership: both the get arm and
    /// the put arm read it, so no kind can answer the field on one route and
    /// deny it on the other. `waveform_busy_field_set_matches_declares_busy`
    /// pins the static field sets to the same predicate.
    fn declares_busy(&self) -> bool {
        matches!(self.kind, ArrayKind::Waveform | ArrayKind::SubArray)
    }

    /// True for the kinds whose `.dbd` declares the monitor/archive posting-mode
    /// block MPST/APST + the HASH they drive (waveform/aai/aao). `subArray` has
    /// no On-Change posting mechanism at all (`subArrayRecord.dbd.pod` declares
    /// none of the three), so it must not answer those names.
    fn has_post_block(&self) -> bool {
        !matches!(self.kind, ArrayKind::SubArray)
    }
}

impl WaveformRecord {
    pub fn new(nelm: i32, element_type: DbFieldType) -> Self {
        let ftvl = Ftype::of_element_type(element_type);
        Self {
            val: ftvl.zeroed(nelm.max(0) as usize),
            nelm,
            nord: 0,
            ftvl,
            ..Default::default()
        }
    }

    /// The VAL buffer's element type, as a `menuFtype` choice.
    pub fn ftvl(&self) -> Ftype {
        self.ftvl
    }

    /// Retype the VAL buffer. The buffer is reallocated (zero-filled, `NORD = 0`)
    /// exactly as C's `init_record` `callocMustSucceed(nelm, dbValueSize(ftvl))`
    /// does, so FTVL and the VAL variant cannot disagree.
    pub fn set_ftvl(&mut self, ftvl: Ftype) {
        self.ftvl = ftvl;
        self.reallocate_val();
    }

    /// The element type of the VAL buffer for the current FTVL — the C
    /// `dbValueSize(prec->ftvl)` element `bptr` is typed with.
    fn ftvl_element_type(&self) -> DbFieldType {
        self.ftvl.element_type()
    }

    /// How many elements the VAL buffer physically holds — the single owner of
    /// the "how big is `bptr`" rule, so every allocation/resize/landing site
    /// agrees by construction.
    ///
    /// * waveform/aai/aao: `NELM` (`waveformRecord.c:104`
    ///   `callocMustSucceed(prec->nelm, ...)`).
    /// * subArray: `MALM` (`subArrayRecord.c:100` `callocMustSucceed(prec->malm,
    ///   ...)`). MALM is the declared length of the SOURCE array; NELM is only
    ///   the slice length. C's `cvt_dbaddr` (168) reports `no_elements = malm`
    ///   and `put_array_info` (190-202) caps NORD at MALM, so a client may write
    ///   up to MALM elements into VAL and the record slices NELM of them out at
    ///   process. Sizing the buffer to NELM instead truncated the client's array
    ///   before the slice could see it: with `NELM=3 INDX=1` a put of
    ///   `[10,20,30,40,50]` kept only `[10,20,30]`, so the INDX slice could only
    ///   ever yield `[20,30]` where C yields `[20,30,40]`.
    fn val_capacity(&self) -> usize {
        if matches!(self.kind, ArrayKind::SubArray) {
            self.malm.max(1) as usize
        } else {
            self.nelm.max(0) as usize
        }
    }

    /// Land a VAL source in the FTVL-typed buffer — the single owner of the
    /// "what does a VAL put do" rule, so VAL is an FTVL-typed ARRAY of
    /// [`Self::val_capacity`] elements on every path, by construction.
    ///
    /// C never replaces the buffer, whatever the source's shape: every write
    /// hands `dbGetLink`/`dbPutField` a pointer to the FTVL-typed `bptr` with
    /// `nRequest = NELM` (`aaoRecord.c:366`, `devWfSoft.c::readLocked`), the
    /// conversion runs INTO that buffer, and NORD becomes the element count
    /// actually written. So:
    ///
    /// * an ARRAY source fills the head of the buffer — `NORD = min(len, NELM)`;
    /// * a SCALAR source is ONE element (`nReq = 1`) landing in `bptr[0]`, with
    ///   `NORD = 1`.
    ///
    /// The pre-fix port's fallback arm (`other => { nord = 1; val = other }`)
    /// stored the SCALAR VARIANT in VAL instead, breaking that invariant on the
    /// everyday `DOL="SETPOINT"` closed-loop aao (a scalar ao feeding an array
    /// output): `array_content_bytes` has no scalar arm, so the On-Change hash
    /// went empty and MPST/APST posting silently died; `resize_val_preserving`
    /// found no array variant and reallocated, wiping the data; and the scalar
    /// propagated on to the OUT target as a scalar. The same arm swallowed the
    /// array variants the buffer does not use (USHORT/ULONG/ENUM/STRING
    /// element types), which likewise ended up stored verbatim with NORD=1.
    ///
    /// Conversion goes through [`EpicsValue::convert_to`], the single
    /// value-coercion owner, against the FTVL element type — C's
    /// `dbFastConvert` into `bptr`.
    fn land_val_in_buffer(&mut self, value: EpicsValue) -> CaResult<()> {
        let cap = self.val_capacity();
        let converted = value.convert_to(self.ftvl_element_type());
        // NORD is capped at the buffer capacity (C bounds every request to the
        // allocated element count, so NORD <= capacity by construction) and the
        // buffer is resized to that capacity to preserve the CA channel element
        // count.
        macro_rules! land {
            ($src:expr, $variant:ident, $zero:expr) => {{
                let mut arr = $src;
                self.nord = arr.len().min(cap) as i32;
                arr.resize(cap, $zero);
                self.val = EpicsValue::$variant(arr);
                Ok(())
            }};
        }
        match converted {
            EpicsValue::CharArray(a) => land!(a, CharArray, 0),
            EpicsValue::UCharArray(a) => land!(a, UCharArray, 0),
            EpicsValue::ShortArray(a) => land!(a, ShortArray, 0),
            EpicsValue::UShortArray(a) => land!(a, UShortArray, 0),
            EpicsValue::LongArray(a) => land!(a, LongArray, 0),
            EpicsValue::ULongArray(a) => land!(a, ULongArray, 0),
            EpicsValue::Int64Array(a) => land!(a, Int64Array, 0),
            EpicsValue::UInt64Array(a) => land!(a, UInt64Array, 0),
            EpicsValue::FloatArray(a) => land!(a, FloatArray, 0.0),
            EpicsValue::DoubleArray(a) => land!(a, DoubleArray, 0.0),
            EpicsValue::EnumArray(a) => land!(a, EnumArray, 0),
            EpicsValue::StringArray(a) => land!(a, StringArray, PvString::new()),
            // Scalar source: one element, into bptr[0].
            EpicsValue::Char(x) => land!(vec![x], CharArray, 0),
            EpicsValue::UChar(x) => land!(vec![x], UCharArray, 0),
            EpicsValue::Short(x) => land!(vec![x], ShortArray, 0),
            EpicsValue::UShort(x) => land!(vec![x], UShortArray, 0),
            EpicsValue::Long(x) => land!(vec![x], LongArray, 0),
            EpicsValue::ULong(x) => land!(vec![x], ULongArray, 0),
            EpicsValue::Int64(x) => land!(vec![x], Int64Array, 0),
            EpicsValue::UInt64(x) => land!(vec![x], UInt64Array, 0),
            EpicsValue::Float(x) => land!(vec![x], FloatArray, 0.0),
            EpicsValue::Double(x) => land!(vec![x], DoubleArray, 0.0),
            EpicsValue::Enum(x) => land!(vec![x], EnumArray, 0),
            EpicsValue::String(x) => land!(vec![x], StringArray, PvString::new()),
            other => Err(CaError::TypeMismatch(format!(
                "VAL: {other:?} does not convert to the FTVL element type"
            ))),
        }
    }

    /// C `aaoRecord.c::fetchValue(prec, init=1)` (351-377), reached from
    /// `init_record` pass 1 (aaoRecord.c:147):
    ///
    /// ```c
    ///     if (prec->omsl != menuOmslclosed_loop) return 0;
    ///     isConst = dbLinkIsConstant(&prec->dol);
    ///     if (init && isConst) {
    ///         status = dbLoadLinkArray(&prec->dol, prec->ftvl, prec->bptr, &nReq);
    ///         if (!status) { prec->nord = nReq; prec->udf = FALSE; }
    ///     }
    /// ```
    ///
    /// The `init && isConst` arm is the ONLY path by which a constant
    /// closed-loop DOL ever reaches an aao: the per-cycle arm is
    /// `!init && !isConst`, so the constant is never re-fetched (that gate is
    /// [`Self::pre_input_link_actions`]). Without this load a
    /// `field(DOL,"[1,2,3]")` / `field(DOL,"5")` closed-loop aao wrote its
    /// zero-filled buffer to OUT forever and stayed UDF — the constant was
    /// simply dropped.
    ///
    /// The other three kinds have no OMSL/DOL, and a supervisory-mode aao does
    /// not source VAL from DOL at all — both return before the load, as C's
    /// first line does.
    fn fetch_constant_dol(&mut self) {
        if !matches!(self.kind, ArrayKind::Aao) || self.omsl != MENU_OMSL_CLOSED_LOOP {
            return;
        }
        let dol = parse_link_v2(&self.dol);
        let Some(value) = crate::server::recgbl::simm::constant_load_value(&dol) else {
            // Not a constant (a real link — fetched per cycle instead), or an
            // unset one: C `dbConstLoadArray` rejects an empty constant with
            // `S_db_badField`, leaving NORD and UDF untouched.
            return;
        };
        // `land_val_in_buffer` IS `dbLoadLinkArray`'s landing rule: convert
        // into the FTVL-typed buffer, `NORD = nRequest`.
        if self.land_val_in_buffer(value).is_ok() {
            self.constant_dol_loaded = true;
        }
    }

    /// Reallocate VAL buffer to match current FTVL and [`Self::val_capacity`] —
    /// C `callocMustSucceed(nelm, dbValueSize(prec->ftvl))`.
    fn reallocate_val(&mut self) {
        self.val = self.ftvl.zeroed(self.val_capacity());
        self.nord = 0;
    }

    /// Resize the VAL buffer to the current [`Self::val_capacity`] **while
    /// preserving existing element data** — shrink truncates, grow zero-pads,
    /// and NORD is clamped to the new length.
    ///
    /// C parity: `waveformRecord` does not support a destructive
    /// run-time NELM change — `init_record` allocates `bptr` once and a
    /// freely-writable NELM that wiped VAL would lose the waveform
    /// contents a CA client just stored. Keeping the data on resize is
    /// the non-destructive equivalent.
    fn resize_val_preserving(&mut self) {
        let n = self.val_capacity();
        match &mut self.val {
            EpicsValue::CharArray(v) => v.resize(n, 0),
            EpicsValue::UCharArray(v) => v.resize(n, 0),
            EpicsValue::ShortArray(v) => v.resize(n, 0),
            EpicsValue::LongArray(v) => v.resize(n, 0),
            EpicsValue::Int64Array(v) => v.resize(n, 0),
            EpicsValue::UInt64Array(v) => v.resize(n, 0),
            EpicsValue::FloatArray(v) => v.resize(n, 0.0),
            EpicsValue::DoubleArray(v) => v.resize(n, 0.0),
            EpicsValue::EnumArray(v) => v.resize(n, 0),
            EpicsValue::StringArray(v) => v.resize(n, PvString::new()),
            // VAL is not currently an array variant — fall back to a
            // fresh allocation sized to the new NELM.
            _ => {
                self.reallocate_val();
                return;
            }
        }
        if (self.nord as usize) > n {
            self.nord = n as i32;
        }
    }

    /// Serialize the first `NORD` elements of `VAL` to their native
    /// (little-endian on the reference builds) byte layout — the bytes C
    /// `monitor()` feeds to `epicsMemHash` over `nord * dbValueSize(ftvl)`
    /// (waveformRecord.c:306-307). Each element contributes exactly its
    /// `dbValueSize` bytes; a STRING element occupies a fixed
    /// `MAX_STRING_SIZE` slot, null-padded.
    fn array_content_bytes(&self) -> Vec<u8> {
        let n = self.nord.max(0) as usize;
        let mut out = Vec::new();
        match &self.val {
            EpicsValue::CharArray(v) => out.extend(v.iter().take(n).copied()),
            EpicsValue::ShortArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::UShortArray(v) | EpicsValue::EnumArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::LongArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::ULongArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::FloatArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::DoubleArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::Int64Array(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::UInt64Array(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::StringArray(v) => {
                for s in v.iter().take(n) {
                    let mut slot = [0u8; MAX_STRING_SIZE];
                    let bytes = s.as_bytes();
                    let copy = bytes.len().min(MAX_STRING_SIZE - 1);
                    slot[..copy].copy_from_slice(&bytes[..copy]);
                    out.extend_from_slice(&slot);
                }
            }
            _ => {}
        }
        out
    }

    /// C `subArrayRecord.c::readValue` (310-314): NELM and INDX are clamped
    /// against MALM at every process — never at field put, so the `.db` load
    /// order of NELM/MALM/INDX cannot matter. MALM is always >= 1
    /// (`init_record` floors it, 96-97).
    fn sa_clamp_bounds(&mut self) {
        let malm = self.malm.max(1);
        // NELM/INDX/MALM are DBF_ULONG in C (subArrayRecord.dbd.pod), and C's
        // readValue clamp is on `epicsUInt32` (`if (nelm > malm) nelm = malm`,
        // subArrayRecord.c:310) — an UNSIGNED compare. The port carries them in
        // i32, so a full-range NELM put (4294967295) lands as -1; comparing that
        // signed leaves `-1 > malm` false and the huge value survives, where C
        // clamps it to MALM. Compare as u32 to reproduce C: 4294967295 > malm
        // clamps to malm, while a genuine small NELM is unaffected.
        if (self.nelm as u32) > (malm as u32) {
            self.nelm = malm;
        }
        // INDX is DBF_ULONG too (subArrayRecord.dbd.pod:385), and its clamp is
        // the same unsigned compare one line further down in the same C
        // function (`if (prec->indx >= prec->malm) prec->indx = prec->malm - 1;`,
        // subArrayRecord.c:313-314). Signed, `caput SA.INDX -1` — which
        // `epicsParseUInt32`'s `strtoul` legitimately turns into 0xFFFFFFFF —
        // read as -1 < malm and slipped through as "start at the beginning";
        // C clamps it to MALM-1 and the slice comes out empty.
        if (self.indx as u32) >= (malm as u32) {
            self.indx = malm - 1;
        }
        // Both bounds are now in [0, MALM] / [0, MALM-1] BY CONSTRUCTION, so
        // every consumer below reads them directly — no floors, no re-checks.
    }

    /// C `devSASoft.c::subset` (39-56) — the ONE subArray slicing primitive:
    ///
    /// ```c
    ///     long ecount = nRequest - prec->indx;
    ///     if (ecount > 0) {
    ///         if (ecount > prec->nelm) ecount = prec->nelm;
    ///         memmove(prec->bptr, (char *)prec->bptr + prec->indx * esize,
    ///                 ecount * esize);
    ///     } else ecount = 0;
    ///     prec->nord = ecount;
    ///     prec->udf = FALSE;
    /// ```
    ///
    /// `n_request` is the element count currently valid at the HEAD of the
    /// buffer — what `dbLoadLinkArray` / `dbGetLink` just landed there, or NORD
    /// itself when the INP is empty and there is nothing to load. The shift is
    /// in place and destructive: C `memmove`s `bptr` down by INDX and leaves the
    /// tail stale, and NORD is what bounds every read, so the port does the
    /// same. (That is what makes a repeatedly-processed empty-INP subArray eat
    /// its own array — `[10,20,30,40,50]` INDX=1 NELM=3 goes `[20,30,40]` →
    /// `[30,40]` → `[40]` → empty+UDF. Verified against a built softIoc.)
    ///
    /// UDF is not touched here: C's `process` overwrites it on the next line
    /// with `prec->udf = !!status` (subArrayRecord.c:148), and that status is
    /// `readValue`'s `nord <= 0` test — which is [`Record::value_is_undefined`].
    fn sa_subset(&mut self, n_request: i32) {
        // Reaching here IS C's `if (!status) subset(prec, ...)` — both callers
        // are a read that returned 0 — so the remembered failure is over.
        self.inp_read_failed = false;
        let len = self.val.count() as i32;
        let n_request = n_request.clamp(0, len);
        let indx = self.indx;
        let mut ecount = n_request - indx;
        if ecount > 0 {
            if ecount > self.nelm {
                ecount = self.nelm;
            }
            let start = indx as usize;
            let end = start + ecount as usize;
            macro_rules! shift {
                ($v:expr) => {
                    if start > 0 {
                        $v.copy_within(start..end, 0);
                    }
                };
            }
            match &mut self.val {
                EpicsValue::CharArray(v) => shift!(v),
                EpicsValue::UCharArray(v) => shift!(v),
                EpicsValue::ShortArray(v) => shift!(v),
                EpicsValue::UShortArray(v) => shift!(v),
                EpicsValue::LongArray(v) => shift!(v),
                EpicsValue::ULongArray(v) => shift!(v),
                EpicsValue::Int64Array(v) => shift!(v),
                EpicsValue::UInt64Array(v) => shift!(v),
                EpicsValue::FloatArray(v) => shift!(v),
                EpicsValue::DoubleArray(v) => shift!(v),
                EpicsValue::EnumArray(v) => shift!(v),
                // A STRING-FTVL subArray: `PvString` is not `Copy`, so the
                // element move is a rotate rather than a `copy_within` — same
                // result, C's `memmove` over `MAX_STRING_SIZE`-wide slots.
                EpicsValue::StringArray(v) => {
                    if start > 0 {
                        v[..end].rotate_left(start);
                    }
                }
                _ => {}
            }
        } else {
            ecount = 0;
        }
        self.nord = ecount;
    }

    /// C `devSASoft.c::read_sa`'s load half (97-119): ask the INP for
    /// `nRequest = min(INDX + NELM, MALM)` elements into `bptr`, then
    /// `subset(nRequest)` with the count the link actually landed.
    ///
    /// The DB-link arm (`dbGetLink` via `dbLinkDoLocked`) and the constant arm
    /// (`dbLoadLinkArray`) differ ONLY in where `source` came from, so both go
    /// through here — one slicing rule, no per-path special case.
    /// [`Self::land_val_in_buffer`] is `dbFastConvert`-into-`bptr`: it converts
    /// to the FTVL element type and lands at most `val_capacity()` (= MALM)
    /// elements, and a SCALAR source is one element at `bptr[0]` with
    /// `nRequest = 1`.
    fn sa_load_and_subset(&mut self, source: EpicsValue) -> CaResult<()> {
        self.sa_clamp_bounds();
        let n_request = self.indx.saturating_add(self.nelm).min(self.malm.max(1));
        let mut src = source.convert_to(self.ftvl_element_type());
        src.truncate(n_request as usize);
        let loaded = src.count() as i32;
        self.land_val_in_buffer(src)?;
        self.sa_subset(loaded);
        Ok(())
    }

    /// The port's `get_array_info`: the one answer to "how many elements of VAL
    /// does this record serve", shared by every reader so the count cannot
    /// disagree with [`Record::value_is_undefined`].
    ///
    /// C gives subArray its own body — `if (prec->udf) *no_elements = 0; else
    /// *no_elements = prec->nord;` (subArrayRecord.c:181-184) — where
    /// `waveformRecord.c:196` and `aaiRecord.c:226` return NORD flat. Deriving
    /// the zero from the same predicate that produces UDF is what keeps the two
    /// in step: there is no state in which a record calls itself undefined and
    /// still hands out its buffer.
    fn served_element_count(&self) -> usize {
        if self.value_is_undefined() {
            return 0;
        }
        self.nord.max(0) as usize
    }
}

/// `menu(menuOmsl)` index for `closed_loop` (`MENU_OMSL[1]`,
/// `menu_choices.rs:61`). When `aao.omsl == closed_loop` the record sources
/// VAL from DOL each cycle (C `aaoRecord.c::fetchValue`).
const MENU_OMSL_CLOSED_LOOP: i16 = 1;

/// C `dbLinkIsConstant(&prec->dol)`: is the aao DOL a constant rather than a
/// fetchable link? Used to gate the process-time fetch (`!isConst`), so a
/// constant is never re-applied per cycle over a client caput.
///
/// The classification is [`parse_link_v2`]'s alone — C's own
/// `dbParseLink` (`dbStaticLib.c:2346-2357`) is the single classifier, and it
/// calls a link constant iff the text is empty, parses whole as a double, or
/// is bracketed (`[1, 2, 3]`). This helper used to ALSO accept a
/// whitespace-separated numeric list (`"1 2 3"`) as a constant array; C does
/// not — `epicsParseDouble` rejects the trailing garbage, so C makes that a
/// PV_LINK to a record named `1`. An empty/unset link is constant
/// (`dbLink.c:220`, whose `lset` is `dbConst_lset`).
fn dol_is_constant(dol: &str) -> bool {
    crate::server::recgbl::simm::is_constant(&parse_link_v2(dol))
}

impl Record for WaveformRecord {
    fn record_type(&self) -> &'static str {
        self.kind.as_record_type()
    }

    /// The index fields four kinds answer from their own array bounds rather
    /// than from the field's type range — so they cannot take the routed
    /// `default:` arm, and cannot keep the VAL cache either.
    ///
    /// `subArrayRecord.c:262-291` `get_control_double` lists four fields beyond
    /// `VAL`, each bounded by the record's maximum array length `MALM`: `INDX`
    /// (a 0-based offset, so `MALM - 1` up), `NELM` (a length, so `MALM` up and
    /// **1** down — C's control lower differs from its graphic lower of 0,
    /// `:246` vs `:277`), `NORD` (`MALM` up, 0 down) and `BUSY` (a flag: 1/0).
    ///
    /// The other three kinds list fewer fields, and bound them by `NELM` (the
    /// record's own length) rather than by `MALM`:
    ///
    /// * `waveformRecord.c:268-289` — `BUSY` (a flag: 1/0) and `NORD` (`NELM`
    ///   up, 0 down); it does NOT list `NELM`, which is why `NELM` takes the
    ///   routed type range for a waveform and `MALM` for a subArray.
    /// * `aaiRecord.c:293-310` / `aaoRecord.c:296-313` — only `NORD`.
    ///
    /// `get_graphic_double` is a DIFFERENT list per kind, so the two slots are
    /// answered separately here:
    ///
    /// * `subArray` (`:231-260`) — the same four, but `NELM`'s lower is **0**.
    /// * `waveform` (`:251-266`) — `BUSY` and `NORD`, same values as control.
    /// * `aai` (`:276-291`) / `aao` (`:279-294`) — only `NORD`.
    ///
    /// `VAL` is absent from both lists here: it answers HOPR/LOPR, which is
    /// what the VAL metadata cache already holds.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        let malm = self.malm as f64;
        let nelm = self.nelm as f64;
        let f = field.to_ascii_uppercase();

        let ctrl_limits = match (self.kind, f.as_str()) {
            (ArrayKind::SubArray, "INDX") => Some((malm - 1.0, 0.0)),
            (ArrayKind::SubArray, "NELM") => Some((malm, 1.0)),
            (ArrayKind::SubArray, "NORD") => Some((malm, 0.0)),
            (ArrayKind::SubArray, "BUSY") => Some((1.0, 0.0)),
            (ArrayKind::Waveform, "BUSY") => Some((1.0, 0.0)),
            (ArrayKind::Waveform | ArrayKind::Aai | ArrayKind::Aao, "NORD") => Some((nelm, 0.0)),
            _ => None,
        };
        let disp_limits = match (self.kind, f.as_str()) {
            (ArrayKind::SubArray, "INDX") => Some((malm - 1.0, 0.0)),
            // The graphic lower is 0 where the control lower is 1.
            (ArrayKind::SubArray, "NELM") => Some((malm, 0.0)),
            (ArrayKind::SubArray, "NORD") => Some((malm, 0.0)),
            (ArrayKind::SubArray, "BUSY") => Some((1.0, 0.0)),
            (ArrayKind::Waveform, "BUSY") => Some((1.0, 0.0)),
            (ArrayKind::Waveform | ArrayKind::Aai | ArrayKind::Aao, "NORD") => Some((nelm, 0.0)),
            _ => None,
        };
        if ctrl_limits.is_none() && disp_limits.is_none() {
            return None;
        }
        Some(FieldMetadataOverride {
            ctrl_limits,
            disp_limits,
            ..Default::default()
        })
    }

    /// Expose the concrete record so device support can drive the
    /// device-only control fields that have no generic put path — `BUSY`
    /// is `special(SPC_NOMOD)`/read-only and `RARM` is reset by the
    /// device's own process. The time-series device support
    /// (`devAsynXXXTimeSeries`) downcasts here to apply its RARM state
    /// machine and reflect BUSY, mirroring the MotorRecord precedent.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    /// Pass-0 finalisation, mirroring each kind's C `init_record` pass 0. It
    /// runs after the loader has applied every field put, which is what NELM
    /// (`.db`-supplied) needs — a `Default`/`new()` seed would be computed
    /// before NELM exists.
    ///
    /// * waveform/aai: `prec->nord = (prec->nelm == 1)` (waveformRecord.c:100,
    ///   aaiRecord.c:113). A one-element array record is fully populated by
    ///   construction — its single element IS the value — so it serves NORD=1
    ///   from load. The port seeded NORD=0 always, and `get_field("VAL")`
    ///   truncates to NORD, so a NELM=1 record served a ZERO-length array to
    ///   every client until its first process. On a waveform the seed only
    ///   SURVIVES when device support is not the soft one — see
    ///   [`Record::soft_input_dset_init`], which is `devWfSoft`'s other arm.
    /// * aao: 0. C's record seeds `nord = (nelm == 1)` too (aaoRecord.c:116-120)
    ///   but calls its dset's `init_record` in the SAME pass (:127), and
    ///   `devAaoSoft.c:43-51` is `if (dbLinkIsConstant(&prec->out)) prec->nord =
    ///   0;`. Links are not resolved until AFTER pass 0
    ///   (`iocInit.c::initDatabase`: `doInitRecord0`, then `doResolveLinks`,
    ///   then `doInitRecord1`), so at that moment EVERY aao's OUT still reads as
    ///   a constant and the seed is wiped for all of them — measured on the
    ///   compiled softIoc: a NELM=1 aao with `field(OUT,"OTHER:PV")` serves
    ///   NORD=0. The only thing that puts elements in an aao at init is the
    ///   constant-DOL `fetchValue` at pass 1 below.
    /// * subArray: `prec->nord = 0` (subArrayRecord.c:101 — never the NELM=1
    ///   seed), MALM floored to 1, then NELM clamped down to MALM
    ///   (subArrayRecord.c:95-103). Process-time clamping in `set_val` (the
    ///   readValue equivalent) re-applies the same bounds each cycle, so the
    ///   .db order of NELM/MALM/INDX cannot matter.
    ///
    /// Every rule here decides the count for a record that arrived EMPTY. A
    /// record that arrived WITH elements — an in-process `record()` build that
    /// put VAL before `add_record`, a state C has no path to — keeps them:
    /// `Self::prebuilt_nord` is captured first, and no init rule may discard
    /// it.
    ///
    /// Pass 1 runs the aao's `fetchValue(prec, 1)` (aaoRecord.c:147) — see
    /// `Self::fetch_constant_dol`.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 {
            self.fetch_constant_dol();
            return Ok(());
        }
        if pass != 0 {
            return Ok(());
        }
        self.prebuilt_nord = self.nord;
        if matches!(self.kind, ArrayKind::SubArray) {
            if self.malm < 1 {
                self.malm = 1;
            }
            // Unsigned clamp, as in C `init_record` (subArrayRecord.c:103, an
            // `epicsUInt32` compare) and `sa_clamp_bounds` — a NELM at the top of
            // the unsigned range must clamp down to MALM, not survive as a
            // negative i32.
            if (self.nelm as u32) > (self.malm as u32) {
                self.nelm = self.malm;
            }
            // C `callocMustSucceed(prec->malm, dbValueSize(prec->ftvl))`
            // (subArrayRecord.c:100) — the buffer is MALM elements wide, sized
            // here (after the `.db` has applied FTVL/MALM) rather than at any
            // single field put, so the load order of FTVL/MALM/NELM cannot
            // matter. Preserving resize, not a wipe: a `record()`-builder
            // subArray may already hold element data (a `.db` cannot — VAL is
            // `DBF_NOACCESS`).
            self.resize_val_preserving();
            return Ok(());
        }
        if matches!(self.kind, ArrayKind::Aao) {
            return Ok(());
        }
        if self.prebuilt_nord == 0 && self.nelm == 1 {
            self.nord = 1;
        }
        Ok(())
    }

    /// waveform/aai/aao clear UDF in `process()` itself, on the line after
    /// `readValue` returns and whatever status it returned —
    /// `prec->pact = TRUE; prec->udf = FALSE;` (waveformRecord.c:143-144,
    /// aaiRecord.c:173-174) and `if (!pact) { prec->udf = FALSE; ... }`
    /// (aaoRecord.c:164-165). A failed SIOL read, or an illegal SIMM (whose
    /// `readValue` returns -1), still leaves the record DEFINED; the framework's
    /// simulation tail otherwise gates the clear on the read status.
    ///
    /// subArray does the opposite — `prec->udf = !!status`
    /// (subArrayRecord.c:148) — so it keeps the default.
    fn clears_udf_unconditionally(&self) -> bool {
        !matches!(self.kind, ArrayKind::SubArray)
    }

    /// All four kinds assign UDF on a failed read, for the two different
    /// reasons named just above: waveform/aai/aao clear it outright on the line
    /// after `readValue`, and subArray's `prec->udf = !!status`
    /// (subArrayRecord.c:148) is the status itself — which
    /// [`Self::value_is_undefined`] now carries. Either way the derive must run.
    fn derives_udf_on_read_failure(&self) -> bool {
        true
    }

    /// waveform/aai/aao have NO `checkAlarms` and never name UDF_ALARM: grep
    /// the three record sources and the only severities they raise are
    /// SIMM_ALARM and SOFT_ALARM (aaiRecord.c:364,381; aaoRecord.c:395,421;
    /// waveformRecord.c:349,376). So UDF, whatever it says, never becomes an
    /// alarm on these three — the framework's central `rec_gbl_check_udf` must
    /// not raise one for them.
    ///
    /// subArray is again the exception: `if (status) recGblSetSevr(prec,
    /// UDF_ALARM, prec->udfs);` (subArrayRecord.c:149-150).
    fn raises_udf_alarm(&self) -> bool {
        matches!(self.kind, ArrayKind::SubArray)
    }

    /// `devWfSoft.c::init_record` (39-51) — the arm the constant-INP loader
    /// above does NOT cover:
    ///
    /// ```c
    ///     long status = dbLoadLinkArray(&prec->inp, prec->ftvl, prec->bptr, &nelm);
    ///     if (!status) { prec->nord = nelm; prec->udf = FALSE; }
    ///     else          prec->nord = 0;
    /// ```
    ///
    /// The call is unconditional — no `dbLinkIsConstant` gate — and only the
    /// CONSTANT lset supplies a `loadArray` (`dbConstLink.c:239`; `dbLoadLinkArray`
    /// returns `S_db_noLSET` otherwise, `dbLink.c:255-264`), so a waveform whose
    /// INP is a real link, or unset, lands in the `else`: NORD = 0. That is why C
    /// serves NORD=0 on a bare `record(waveform,"X"){}` (NELM defaults to 1) while
    /// the identical `aai` serves 1 — `devAaiSoft.c:55` loads only
    /// `if (dbLinkIsConstant(plink))` and leaves the record's seed alone otherwise.
    ///
    /// So the three input kinds split here, and each is C's own dset:
    ///
    /// * waveform — the seed does not survive a failed load;
    /// * aai — it does;
    /// * subArray — `devSASoft.c:58-74` loads and subsets (NORD comes from the
    ///   slice, `subset()`), and leaves NORD alone when the load fails; its seed
    ///   is 0 to begin with.
    fn soft_input_dset_init(&mut self, loaded: bool) {
        // `prebuilt_nord`: the elements a caller loaded are not C's calloc'd
        // buffer, and the dset's zeroing arm exists because C's buffer IS empty.
        if matches!(self.kind, ArrayKind::Waveform) && !loaded && self.prebuilt_nord == 0 {
            self.nord = 0;
        }
    }

    /// C `fetchValue(prec, 1)`'s `if (!status) { prec->nord = nReq;
    /// prec->udf = FALSE; }` (aaoRecord.c:371-374) — the UDF half of the
    /// constant-DOL load, which `init_record` cannot apply itself (UDF is a
    /// common field). An aao whose value came from a constant DOL is DEFINED.
    fn post_init_finalize_undef(&mut self, udf: &mut bool) -> CaResult<()> {
        if self.constant_dol_loaded {
            *udf = false;
        }
        Ok(())
    }

    /// `VAL`: `get_field("VAL")` serves NORD valid elements, but the channel's
    /// native count is the buffer capacity so a client sizes its buffer right —
    /// C `cvt_dbaddr` `no_elements` vs `get_array_info` `*no_elements = nord`.
    /// The capacity is the same `Self::val_capacity` every array kind
    /// advertises: waveform/aai/aao report `nelm` (`waveformRecord.c:183`,
    /// `aaiRecord.c:215`, `aaoRecord.c:219`), subArray reports `malm`
    /// (`subArrayRecord.c:168`). Reporting it only for the waveform kind left
    /// aai/aao/subArray advertising a 0-length channel, so every array put/get
    /// was refused "Invalid element count requested".
    fn field_native_count(&self, field: &str) -> Option<u32> {
        (field == "VAL").then(|| self.val_capacity() as u32)
    }

    /// `aao` is an output record; the rest of the array family read
    /// from INP. Output records take the device-write path in
    /// processing.rs (or fall through to the soft-link write when the
    /// DTYP is empty / "Soft Channel").
    fn can_device_write(&self) -> bool {
        self.kind.is_output()
    }

    /// `MPST`/`APST` are `DBF_MENU menu(waveformPOST)`
    /// (`waveformRecord.dbd.pod:523-533`), served as `DBR_ENUM`. `FTVL`
    /// (`menu(menuFtype)`) is a shared menu resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "MPST" | "APST" if self.has_post_block() => Some(WAVEFORM_POST),
            // SIMM is menu(menuYesNo) (NO/YES) on the array records. SIMS
            // (menuAlarmSevr) and OLDSIMM (menuSimm) resolve via the shared
            // menu registry. Only the kinds that carry a sim block answer.
            "SIMM" if self.has_sim_block() => Some(MENU_YES_NO),
            _ => None,
        }
    }

    /// C waveform/aai/aao `monitor()` (waveformRecord.c:291-326): MPST/APST
    /// "Always vs On Change" posting. In Always mode the corresponding bit
    /// posts every cycle; in On Change mode the array content is hashed
    /// (`epicsMemHash` over `nord * dbValueSize(ftvl)` native bytes) and the
    /// bit posts — plus `HASH` is updated and reported changed — only when
    /// the hash differs from the stored `HASH`. `subArray` has no such
    /// mechanism, so it (and every non-array record) keeps the default
    /// `None` and the generic deadband decision.
    fn array_monitor_post(&mut self) -> Option<crate::server::record::ArrayMonitorPost> {
        if matches!(self.kind, ArrayKind::SubArray) {
            return None;
        }
        let mut post_value = self.mpst == WAVEFORM_POST_ALWAYS;
        let mut post_archive = self.apst == WAVEFORM_POST_ALWAYS;
        let mut hash_changed = false;
        if self.mpst == WAVEFORM_POST_ONCHANGE || self.apst == WAVEFORM_POST_ONCHANGE {
            let h = epics_mem_hash(&self.array_content_bytes(), 0);
            if h != self.hash {
                self.hash = h;
                hash_changed = true;
                if self.mpst == WAVEFORM_POST_ONCHANGE {
                    post_value = true;
                }
                if self.apst == WAVEFORM_POST_ONCHANGE {
                    post_archive = true;
                }
            }
        }
        Some(crate::server::record::ArrayMonitorPost {
            post_value,
            post_archive,
            hash_changed,
        })
    }

    /// `HASH` is posted by C `monitor()` with a literal `DBE_VALUE` only on
    /// a hash change (waveformRecord.c:317-319), never through VAL's change
    /// detection — exclude it from the generic change-detection loop so it
    /// is neither double-posted nor spuriously posted in Always mode.
    fn event_posted_fields(&self) -> &'static [&'static str] {
        if matches!(self.kind, ArrayKind::SubArray) {
            &[]
        } else {
            &["HASH"]
        }
    }

    // EGU/HOPR/LOPR/PREC are backed by typed storage and exposed through both
    // get_field/put_field and field_list, so populate_display_info reads the
    // loaded values for the DBR_GR display limits (waveformRecord.c:251-252).
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => {
                // Return only NORD valid elements, not the full NELM buffer.
                // CA clients use the returned element count to interpret the
                // data (e.g. PyDMImageView computes height = count / width).
                let mut val = self.val.clone();
                val.truncate(self.served_element_count());
                Some(val)
            }
            // The DBF types are the `.dbd.pod`'s (see `array_field_list!`): NELM
            // is DBF_ULONG on all four kinds, NORD is DBF_ULONG on
            // waveform/aai/aao and DBF_LONG on subArray. The value variant is
            // what CA and PVA project the native type from (CA promotes
            // DBF_ULONG to DBR_DOUBLE per db_convert.h; PVA serves uint32), so
            // it has to agree with the `FieldDesc` — as it does for VAL.
            "NELM" => Some(EpicsValue::ULong(self.nelm as u32)),
            "NORD" => Some(match self.kind {
                ArrayKind::SubArray => EpicsValue::Long(self.nord),
                _ => EpicsValue::ULong(self.nord as u32),
            }),
            "FTVL" => Some(EpicsValue::Short(self.ftvl.index())),
            "MPST" if self.has_post_block() => Some(EpicsValue::Short(self.mpst)),
            "APST" if self.has_post_block() => Some(EpicsValue::Short(self.apst)),
            // HASH (DBF_ULONG) — the On Change content hash. Only the
            // waveform/aai/aao kinds declare it; subArray has no such field.
            "HASH" if self.has_post_block() => Some(EpicsValue::ULong(self.hash)),
            // subArray-specific INDX/MALM fields. Other array record
            // kinds expose them as zero (matches C dbpr output for a
            // record type that doesn't declare the field).
            "INDX" if matches!(self.kind, ArrayKind::SubArray) => {
                Some(EpicsValue::ULong(self.indx as u32))
            }
            "MALM" if matches!(self.kind, ArrayKind::SubArray) => {
                Some(EpicsValue::ULong(self.malm as u32))
            }
            // RARM (re-arm control) is waveform-only, used by waveform device
            // support (devAsynXXXTimeSeries); aai/aao/subArray do not declare it.
            // BUSY (acquisition-active) is declared by waveform AND subArray —
            // [`Self::declares_busy`] owns that membership.
            "RARM" if matches!(self.kind, ArrayKind::Waveform) => {
                Some(EpicsValue::Short(self.rarm))
            }
            "BUSY" if self.declares_busy() => Some(EpicsValue::Short(self.busy as i16)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            // Simulation block — waveform/aai/aao only (not subArray).
            "SIMM" if self.has_sim_block() => Some(EpicsValue::Short(self.simm)),
            "SIML" if self.has_sim_block() => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" if self.has_sim_block() => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" if self.has_sim_block() => Some(EpicsValue::Short(self.sims)),
            "SDLY" if self.has_sim_block() => Some(EpicsValue::Double(self.sdly)),
            // aao-only output-mode / desired-output link (aaoRecord.dbd.pod).
            "OMSL" if matches!(self.kind, ArrayKind::Aao) => Some(EpicsValue::Short(self.omsl)),
            "DOL" if matches!(self.kind, ArrayKind::Aao) => {
                Some(EpicsValue::String(self.dol.clone().into()))
            }
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                // A DBR_STRING source is C's `putString<FTVL>` row
                // (`dbConvert.c:941-1147`): each of the `nRequest` elements is
                // PARSED with `epicsParse*` at `dbConvertBase` 0, and a
                // non-zero status aborts the whole `dbPut` so the field keeps
                // its old value. A scalar string is `nRequest == 1`, so it
                // lands as ONE element.
                //
                // Measured on the compiled softIoc, `caput` then `caget`:
                // `FTVL=CHAR` refuses "hi" and stores 65 for "65";
                // `FTVL=DOUBLE`/`LONG` refuse "hi", take `0x10` as 16, and
                // `LONG` truncates 1.7 to 1. The rule does not vary with FTVL,
                // so neither does this branch. Treating a string as the
                // buffer's BYTES for CHAR/UCHAR — and silently coercing it to
                // 0 for every other FTVL — was a special case at one boundary
                // that C does not have anywhere: the byte carry is what a
                // client asking DBR_CHAR gets (`caput -S`), which arrives here
                // already as a `CharArray` and is untouched by this branch.
                let value = match (&value, c_parse::NumericField::of(self.ftvl_element_type())) {
                    (EpicsValue::String(s), Some(numeric)) => {
                        c_parse::put_string(name, numeric, &s.as_str_lossy())?
                    }
                    // FTVL=STRING is `putStringString`, a text copy, not a
                    // parse — `NumericField::of` reports None for it.
                    _ => value,
                };
                // C `dbAccess.c:1415` `if (isValueField) precord->udf = FALSE;`
                // — a put to VAL re-establishes the buffer, so the remembered
                // read failure no longer describes it. (Reachable without a
                // process only through an NPP link put; subArray's VAL is
                // `pp(TRUE)`, so a CA put re-derives UDF a moment later.)
                self.inp_read_failed = false;
                self.land_val_in_buffer(value)
            }
            "NELM" => {
                let Some(n) = count_put(&value) else {
                    return Err(CaError::InvalidValue(format!(
                        "NELM requires Long, got {value:?}"
                    )));
                };
                if matches!(self.kind, ArrayKind::SubArray) {
                    // subArray: NELM (DBF_ULONG, pp(TRUE), NO special —
                    // subArrayRecord.dbd.pod:379) is the SLICE LENGTH, not the
                    // buffer size — the buffer is MALM wide (`val_capacity`) and a
                    // NELM put does not touch it. C's NELM is a plain
                    // `epicsUInt32`: `dbPutField` stores ANY value verbatim (0 and
                    // the full unsigned range included — C rejects none of them),
                    // the `pp(TRUE)` process re-slices, and `bptr` (allocated once
                    // at `init_record`) keeps its contents throughout. Reallocating
                    // a zeroed buffer here EMPTIED VAL on every NELM put: C `caput
                    // SA.NELM 2` on an `INP="[1,2,3,4]"` subArray reads back
                    // `VAL=[1,2] NORD=2`; the port read back an empty VAL, NORD=0.
                    //
                    // Store as the DBF_ULONG bit pattern (`count_put` reinterprets,
                    // so 4294967295 lands as -1 in the i32 carrier). NO rejection
                    // of zero/negative and NO MALM clamp here — C clamps NELM->MALM
                    // at init_record and at process (subArrayRecord.c:103-104,
                    // 310-311) with an UNSIGNED compare, never at field put, so the
                    // .db load order of NELM vs MALM cannot matter. `init_record`
                    // and `sa_clamp_bounds` (the readValue equivalent) apply it.
                    self.nelm = n;
                    Ok(())
                } else {
                    // waveform/aai/aao: NELM is `special(SPC_NOMOD)`, so a CA put
                    // never reaches here — this arm is the `.db`-load / builder
                    // sizing path, where NELM must be a positive element count.
                    // Preserve the existing element data instead of wiping VAL.
                    if n <= 0 {
                        return Err(CaError::InvalidValue(format!(
                            "NELM must be positive, got {n}"
                        )));
                    }
                    self.nelm = n;
                    self.resize_val_preserving();
                    Ok(())
                }
            }
            "FTVL" => {
                let EpicsValue::Short(v) = value else {
                    return Err(CaError::InvalidValue(format!(
                        "FTVL requires Short, got {value:?}"
                    )));
                };
                // An index past the menu is not a choice: C `dbPutStringNum`
                // rejects it with `S_db_badChoice` and leaves the field alone,
                // rather than storing an index whose `dbValueSize` is undefined.
                let Some(ftvl) = Ftype::from_index(v) else {
                    return Err(CaError::BadChoice(format!(
                        "FTVL: {v} is not one of {MENU_FTYPE:?}"
                    )));
                };
                self.set_ftvl(ftvl);
                Ok(())
            }
            "MPST" if self.has_post_block() => {
                if let EpicsValue::Short(v) = value {
                    self.mpst = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch("MPST".into()))
                }
            }
            "APST" if self.has_post_block() => {
                if let EpicsValue::Short(v) = value {
                    self.apst = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch("APST".into()))
                }
            }
            // HASH (DBF_ULONG, waveform/aai/aao — waveformRecord.dbd.pod:535) is
            // the On-Change content hash. Its .dbd carries no special()/pp(), so
            // C dbPutField stores a client caput verbatim, exactly like any plain
            // DBF_ULONG field. The store is TRANSIENT: `array_monitor_post`
            // recomputes HASH from the array bytes on the next On-Change process
            // (waveformRecord.c:311-319), overwriting it — but the put itself
            // succeeds and reads back until then, matching C. subArray declares no
            // HASH (`has_post_block` is false there), so it falls through. Accept
            // the native ULong carrier and the legacy signed Long (bit-preserving
            // reinterpret), the same rule the mbbi/mbbo DBF_ULONG fields use.
            "HASH" if self.has_post_block() => {
                self.hash = match value {
                    EpicsValue::ULong(v) => v,
                    EpicsValue::Long(v) => v as u32,
                    _ => return Err(CaError::TypeMismatch("HASH".into())),
                };
                Ok(())
            }
            "NORD" => Err(CaError::ReadOnlyField(name.to_string())),
            "INDX" if matches!(self.kind, ArrayKind::SubArray) => {
                let Some(v) = count_put(&value) else {
                    return Err(CaError::TypeMismatch("INDX".into()));
                };
                // Store the DBF_ULONG bit pattern verbatim, exactly as the NELM
                // arm does — `count_put` has already reinterpreted, so a
                // `caput SA.INDX -1` lands here as -1 carrying C's 0xFFFFFFFF.
                // Flooring it at 0 threw that away and turned an out-of-range
                // index into "start at the beginning". NO MALM clamp here
                // either: C clamps INDX->MALM-1 at process
                // (subArrayRecord.c:313-314), never at field put, so the .db
                // load order of INDX vs MALM cannot matter. `sa_clamp_bounds`
                // (the readValue equivalent) applies it.
                self.indx = v;
                Ok(())
            }
            "MALM" if matches!(self.kind, ArrayKind::SubArray) => {
                let Some(v) = count_put(&value) else {
                    return Err(CaError::TypeMismatch("MALM".into()));
                };
                // C floors MALM to 1 (subArrayRecord.c:96-97); it is never 0.
                // No NELM/INDX re-clamp here — both are clamped against MALM at
                // `init_record` (post-load) and at process (310-314), so a MALM
                // put in any .db load order is reconciled there, not here.
                self.malm = v.max(1);
                Ok(())
            }
            // RARM (re-arm control, pp(TRUE) — client-settable) is waveform-only;
            // the device support reads it and resets it to 0. BUSY
            // (acquisition-active, special(SPC_NOMOD) — device-set, not
            // client-writable) is waveform + subArray ([`Self::declares_busy`]).
            "RARM" if matches!(self.kind, ArrayKind::Waveform) => {
                let v = match value {
                    EpicsValue::Short(v) => v,
                    EpicsValue::Long(v) => v as i16,
                    _ => return Err(CaError::TypeMismatch("RARM".into())),
                };
                self.rarm = v;
                Ok(())
            }
            "BUSY" if self.declares_busy() => Err(CaError::ReadOnlyField(name.to_string())),
            "EGU" => {
                if let EpicsValue::String(s) = value {
                    self.egu = s;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch("EGU".into()))
                }
            }
            "HOPR" => {
                self.hopr = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("HOPR".into()))?;
                Ok(())
            }
            "LOPR" => {
                self.lopr = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LOPR".into()))?;
                Ok(())
            }
            "PREC" => {
                self.prec = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("PREC".into()))?
                    as i16;
                Ok(())
            }
            // Simulation block — waveform/aai/aao only (not subArray).
            "SIMM" if self.has_sim_block() => match value {
                EpicsValue::Short(v) => {
                    self.simm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIMM".into())),
            },
            "SIML" if self.has_sim_block() => match value {
                EpicsValue::String(s) => {
                    self.siml = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIML".into())),
            },
            "SIOL" if self.has_sim_block() => match value {
                EpicsValue::String(s) => {
                    self.siol = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIOL".into())),
            },
            "SIMS" if self.has_sim_block() => match value {
                EpicsValue::Short(v) => {
                    self.sims = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIMS".into())),
            },
            "SDLY" if self.has_sim_block() => match value {
                EpicsValue::Double(v) => {
                    self.sdly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SDLY".into())),
            },
            // aao-only OMSL (menu, resolved to a Short index by the central
            // shared_menu_choices("OMSL") path) and DOL link string. The
            // field_list AAO set carries these so apply_fields routes
            // field(OMSL/DOL,...) here rather than to common fields.
            "OMSL" if matches!(self.kind, ArrayKind::Aao) => match value {
                EpicsValue::Short(v) => {
                    self.omsl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OMSL".into())),
            },
            "DOL" if matches!(self.kind, ArrayKind::Aao) => match value {
                EpicsValue::String(s) => {
                    self.dol = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("DOL".into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    /// aao `OMSL=closed_loop` desired-output pull. C `aaoRecord.c::fetchValue`
    /// (357-377): an aao whose `omsl == closed_loop` sources its array from
    /// `DOL` before writing. At PROCESS time C fetches only when DOL is a
    /// *non-constant* link (`!init && !isConst` → `dbGetLink`); a constant DOL
    /// is loaded once at init via `dbLoadLinkArray` and is a per-cycle no-op.
    /// This pre-input hook mirrors the process-time `!isConst` arm: it emits a
    /// `ReadDbLink { DOL -> VAL }` only for a real link — the framework reads
    /// the link's native array and applies it via `put_field("VAL", ...)`,
    /// which sets `NORD = element count` exactly as C's `nord = nReq`. A
    /// constant or empty DOL emits nothing, so a constant is never re-applied
    /// over a client caput to VAL (C's `!dbLinkIsConstant` gate). Supervisory
    /// mode and the other three array kinds (no OMSL/DOL) return no actions.
    ///
    /// Residual: the init-time constant-array load (`dbLoadLinkArray`, C's
    /// `init && isConst` arm) is not applied here — it belongs to
    /// `init_record`, not to the per-cycle hook.
    fn pre_input_link_actions(&mut self) -> Vec<ProcessAction> {
        if !matches!(self.kind, ArrayKind::Aao) || self.omsl != MENU_OMSL_CLOSED_LOOP {
            return Vec::new();
        }
        // C `!dbLinkIsConstant(&prec->dol)`: only a real (DB/CA/PVA) link is
        // fetched at process time; a constant (scalar, array literal, or
        // empty) is not re-applied each cycle.
        if dol_is_constant(&self.dol) {
            return Vec::new();
        }
        // The cycle now depends on this fetch — `set_resolved_input_links`
        // reports whether it landed.
        self.dol_fetch_requested = true;
        vec![ProcessAction::ReadDbLink {
            link_field: "DOL",
            target_field: "VAL",
        }]
    }

    /// The framework's per-cycle report of which input-link fetches produced a
    /// value — C `RTN_SUCCESS(dbGetLink(...))`. For the closed-loop aao that
    /// is `fetchValue`'s status: a requested DOL fetch missing from the report
    /// is C's non-zero `dbGetLink` status, which aborts the cycle in
    /// [`Self::process`].
    fn set_resolved_input_links(&mut self, resolved: &[&'static str]) {
        self.dol_read_failed = self.dol_fetch_requested && !resolved.contains(&"DOL");
        self.dol_fetch_requested = false;
    }

    /// C `aaoRecord.c::process` (164-174):
    ///
    /// ```c
    ///     if (!pact) {
    ///         prec->udf = FALSE;
    ///         if (!!(status = fetchValue(prec, 0)))
    ///             return status;
    ///         recGblGetTimeStampSimm(...);
    ///     }
    ///     status = writeValue(prec);
    ///     ...
    ///     monitor(prec);
    ///     recGblFwdLink(prec);
    /// ```
    ///
    /// A failed closed-loop DOL fetch RETURNS — before `writeValue`, before the
    /// timestamp, before `monitor` and before `recGblFwdLink`, with PACT still
    /// clear. Nothing is written out and nothing is posted: the record must not
    /// push a stale VAL to its device/OUT target as if it were the new desired
    /// output. (`fetchValue`'s `dbGetLink` has already raised LINK/INVALID via
    /// `setLinkAlarm`; C leaves it PENDING in nsta/nsev, because the abort also
    /// skips the `recGblResetAlarms` inside `monitor`.) `CompleteNoEmit` is that
    /// return exactly — no device write, no OUT, no monitor, no FLNK, no alarm
    /// commit, PACT clear.
    ///
    /// The port ran the whole cycle regardless: the framework's pre-input stage
    /// discarded the read failure, so a dead DOL meant OUT kept receiving the
    /// last good value and downstream records kept being triggered by it.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if self.dol_read_failed {
            self.dol_read_failed = false;
            return Ok(ProcessOutcome::complete_no_emit());
        }
        Ok(ProcessOutcome::complete())
    }

    /// Delivery of an INP-link value into the record's value buffer.
    ///
    /// waveform/aai/aao land the array whole (`devWfSoft.c::readLocked`:
    /// `dbGetLink(pinp, ftvl, bptr, 0, &nRequest)` with `nRequest = NELM`).
    /// subArray is the slicing record: its device support asks the link for
    /// `INDX + NELM` elements and subsets them (`devSASoft.c::read_sa`), which
    /// is `Self::sa_load_and_subset` — the same primitive the constant-INP
    /// process reload uses, so link delivery and constant reload cannot drift
    /// apart.
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        if matches!(self.kind, ArrayKind::SubArray) {
            return self.sa_load_and_subset(value);
        }
        match self.put_field("VAL", value.clone()) {
            Ok(()) => Ok(()),
            Err(CaError::TypeMismatch(_)) => {
                let target = self
                    .get_field("VAL")
                    .map(|v| v.db_field_type())
                    .unwrap_or(DbFieldType::Double);
                let coerced = value.convert_to(target);
                self.put_field("VAL", coerced)
            }
            Err(e) => Err(e),
        }
    }

    /// C `devSASoft.c::read_sa` (92-123), the CONSTANT-INP arm — subArray is
    /// the documented exception to the load-once rule every other soft input
    /// device support follows:
    ///
    /// ```c
    ///     rt.nRequest = prec->indx + prec->nelm;
    ///     if (rt.nRequest > prec->malm) rt.nRequest = prec->malm;
    ///     if (dbLinkIsConstant(&prec->inp)) {
    ///         status = dbLoadLinkArray(&prec->inp, prec->ftvl, prec->bptr, &rt.nRequest);
    ///         if (status == S_db_badField) {   /* INP was empty */
    ///             rt.nRequest = prec->nord;
    ///             status = 0;
    ///         }
    ///     }
    ///     ...
    ///     if (!status) subset(prec, rt.nRequest);
    /// ```
    ///
    /// So on EVERY process:
    ///
    /// * a non-empty constant INP (`field(INP,"[1,2,3,4]")`) is re-loaded and
    ///   re-sliced — a client caput to VAL is restored to the INDX window of the
    ///   constant on the next cycle;
    /// * an EMPTY/unset INP still subsets, with `nRequest = NORD` — the
    ///   "client writes VAL, the record slices it by INDX" pattern, which is the
    ///   standard way a subArray is driven with no input link at all.
    ///
    /// Both arms were dead in the port: `set_val` (the only slicing site) runs
    /// on link delivery, and after R15-78 a constant INP delivers nothing at
    /// process — so INDX was inert on exactly the two configurations C
    /// documents.
    ///
    /// The other three array kinds keep the default (`false`): `devWfSoft.c`
    /// and `devAaiSoft.c` both open with `if (dbLinkIsConstant(pinp)) return 0;`.
    fn read_constant_inp(&mut self, value: Option<EpicsValue>) -> bool {
        if !matches!(self.kind, ArrayKind::SubArray) {
            return false;
        }
        match value {
            Some(v) => {
                let _ = self.sa_load_and_subset(v);
            }
            None => {
                // `S_db_badField` — the INP is empty: nRequest = NORD, subset
                // the buffer the client wrote.
                self.sa_clamp_bounds();
                let n_request = self.nord;
                self.sa_subset(n_request);
            }
        }
        true
    }

    /// The record's half of C `subArrayRecord.c:148` `prec->udf = !!status`.
    /// That status has BOTH halves of `readValue` (313-319) in it:
    ///
    /// ```c
    /// status = (*pdset->read_sa)(prec);   /* the INP read */
    /// if (prec->nord <= 0)                /* the empty-slice test */
    ///     status = -1;
    /// ```
    ///
    /// so a subArray is undefined when its INP read failed (`inp_read_failed`,
    /// reported through [`Self::soft_input_read_failed`]) OR when the slice came
    /// out empty — INDX walked past the data, which an empty-INP subArray
    /// re-subsetting its own buffer or a client caput to INDX can both produce.
    /// The framework owns `common.udf`; [`Self::served_element_count`] reads the
    /// same predicate so the served count follows without a second rule.
    ///
    /// waveform/aai/aao keep the default answer for an array VAL (defined) —
    /// they clear UDF unconditionally in `process()` anyway
    /// ([`Self::clears_udf_unconditionally`]).
    fn value_is_undefined(&self) -> bool {
        if matches!(self.kind, ArrayKind::SubArray) {
            return self.inp_read_failed || self.nord <= 0;
        }
        false
    }

    /// C `devSASoft.c::read_sa` (118-120) runs `subset()` only when the
    /// `dbLinkDoLocked(readLocked)` status was 0, so a failed INP leaves NORD
    /// and `bptr` at their pre-failure contents and `readValue` hands the
    /// non-zero status to `process`. Remember it: without this the record
    /// cannot tell "the link delivered nothing because there is no link" from
    /// "the link is broken", and calls its stale slice valid data.
    fn soft_input_read_failed(&mut self) {
        self.inp_read_failed = true;
    }
}

#[cfg(test)]
mod array_kind_tests {
    use super::*;
    use crate::server::record::FieldDeclaration;

    #[test]
    fn epics_mem_hash_matches_c_reference_vectors() {
        // Reference values produced by the verbatim C `epicsMemHash`
        // (epicsString.c:378-388) compiled on this machine (signed char,
        // little-endian). The CharArray vector includes high-bit bytes
        // (0x80/0xFF) to pin the signed-char sign extension.
        let mut da = Vec::new();
        da.extend_from_slice(&1.0f64.to_le_bytes());
        da.extend_from_slice(&2.0f64.to_le_bytes());
        assert_eq!(epics_mem_hash(&da, 0), 0xa23a_aba6);

        let mut la = Vec::new();
        for x in [1i32, 2, 3] {
            la.extend_from_slice(&x.to_le_bytes());
        }
        assert_eq!(epics_mem_hash(&la, 0), 0x3429_76d1);

        assert_eq!(epics_mem_hash(&[0x00, 0x80, 0xFF, 0x7F], 0), 0x7be0_007f);
        // Odd length exercises the mid-pair break in the C loop.
        assert_eq!(epics_mem_hash(&[0xAA, 0xBB, 0xCC], 0), 0x06ab_0bfc);
        assert_eq!(epics_mem_hash(&[], 0), 0);
    }

    /// HASH (DBF_ULONG, waveform/aai/aao) accepts a client put and stores the
    /// full unsigned range verbatim — u32::MAX and the negative-into-unsigned
    /// wrap both round-trip. subArray declares no HASH and rejects the name.
    #[test]
    fn hash_put_stores_ulong_for_post_kinds() {
        for kind in [ArrayKind::Waveform, ArrayKind::Aai, ArrayKind::Aao] {
            let mut r = WaveformRecord::with_kind(kind);
            r.put_field("HASH", EpicsValue::ULong(1)).unwrap();
            assert_eq!(r.get_field("HASH"), Some(EpicsValue::ULong(1)));
            r.put_field("HASH", EpicsValue::ULong(u32::MAX)).unwrap();
            assert_eq!(r.get_field("HASH"), Some(EpicsValue::ULong(u32::MAX)));
            // Legacy signed carrier: -1 reinterprets to u32::MAX bit pattern.
            r.put_field("HASH", EpicsValue::Long(-1)).unwrap();
            assert_eq!(r.get_field("HASH"), Some(EpicsValue::ULong(u32::MAX)));
        }
        let mut sa = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert!(matches!(
            sa.put_field("HASH", EpicsValue::ULong(1)),
            Err(CaError::FieldNotFound(_))
        ));
    }

    /// subArray NELM (DBF_ULONG, pp(TRUE), no special) accepts any client put:
    /// zero stores and survives the readValue clamp (0 <= MALM), while a
    /// full-range put (u32::MAX, or the -1 legacy wrap) is stored and then
    /// clamped DOWN to MALM by the unsigned compare — matching C
    /// (subArrayRecord.c:310). The port previously rejected NELM <= 0 outright
    /// and, if stored, would have kept the huge value under a signed compare.
    #[test]
    fn subarray_nelm_put_accepts_zero_and_full_range() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert_eq!(r.malm, 1);
        // Zero: stored, and 0 <= MALM so the clamp leaves it.
        r.put_field("NELM", EpicsValue::ULong(0)).unwrap();
        assert_eq!(r.get_field("NELM"), Some(EpicsValue::ULong(0)));
        r.sa_clamp_bounds();
        assert_eq!(r.get_field("NELM"), Some(EpicsValue::ULong(0)));
        // Full unsigned range: stored, then clamped down to MALM.
        r.put_field("NELM", EpicsValue::ULong(u32::MAX)).unwrap();
        r.sa_clamp_bounds();
        assert_eq!(r.get_field("NELM"), Some(EpicsValue::ULong(1)));
        // Negative-into-unsigned via the legacy Long carrier: same wrap.
        r.put_field("NELM", EpicsValue::Long(-1)).unwrap();
        r.sa_clamp_bounds();
        assert_eq!(r.get_field("NELM"), Some(EpicsValue::ULong(1)));
    }

    #[test]
    fn waveform_default_kind() {
        let r = WaveformRecord::default();
        assert_eq!(r.record_type(), "waveform");
        assert!(!r.can_device_write(), "waveform is input-only");
    }

    #[test]
    fn aai_record_type_and_input() {
        let r = WaveformRecord::with_kind(ArrayKind::Aai);
        assert_eq!(r.record_type(), "aai");
        assert!(!r.can_device_write(), "aai is input");
    }

    #[test]
    fn aao_is_output() {
        let r = WaveformRecord::with_kind(ArrayKind::Aao);
        assert_eq!(r.record_type(), "aao");
        assert!(r.can_device_write(), "aao must take the device-write path");
    }

    #[test]
    fn sub_array_record_type() {
        let r = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert_eq!(r.record_type(), "subArray");
        assert!(!r.can_device_write(), "subArray is input");
    }

    /// C's `cvt_dbaddr` fixes every array record's VAL channel `no_elements` at
    /// the buffer capacity — `nelm` for waveform/aai/aao (`waveformRecord.c:183`,
    /// `aaiRecord.c:215`, `aaoRecord.c:219`), `malm` for subArray
    /// (`subArrayRecord.c:168`). A kind that reported `None` here advertised a
    /// 0-length channel, so every array put/get was refused "Invalid element
    /// count requested". Boundary: each kind's capacity source, plus a non-VAL
    /// field that must stay `None`.
    #[test]
    fn every_array_kind_advertises_val_capacity_as_native_count() {
        for kind in [ArrayKind::Waveform, ArrayKind::Aai, ArrayKind::Aao] {
            let mut r = WaveformRecord::with_kind(kind);
            r.nelm = 16;
            assert_eq!(
                r.field_native_count("VAL"),
                Some(16),
                "{} VAL native count must be NELM (its cvt_dbaddr no_elements)",
                r.record_type()
            );
            assert_eq!(
                r.field_native_count("NORD"),
                None,
                "{} native count only applies to VAL",
                r.record_type()
            );
        }
        let mut sa = WaveformRecord::with_kind(ArrayKind::SubArray);
        sa.malm = 12;
        assert_eq!(
            sa.field_native_count("VAL"),
            Some(12),
            "subArray VAL native count must be MALM, not NELM"
        );
        assert_eq!(sa.field_native_count("VAL"), Some(sa.val_capacity() as u32));
    }

    #[test]
    fn aliases_resolve_to_waveform_record() {
        // The type aliases are documentation-only; constructing
        // through them must yield the same concrete struct.
        let a: AaiRecord = WaveformRecord::with_kind(ArrayKind::Aai);
        let b: AaoRecord = WaveformRecord::with_kind(ArrayKind::Aao);
        let c: SubArrayRecord = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert_eq!(a.record_type(), "aai");
        assert_eq!(b.record_type(), "aao");
        assert_eq!(c.record_type(), "subArray");
    }

    /// aao alone declares OMSL/DOL (`aaoRecord.dbd.pod`). `apply_fields` gates
    /// `field(OMSL/DOL,...)` on `field_list` membership, so the aao set must
    /// carry both names and the waveform/aai set must not — otherwise the
    /// loader would misroute them to common fields and the fetch would never
    /// arm.
    #[test]
    fn aao_field_list_includes_omsl_dol_other_kinds_do_not() {
        let aao = WaveformRecord::with_kind(ArrayKind::Aao);
        let names: Vec<&str> = aao.field_list().iter().map(|f| f.name).collect();
        assert!(names.contains(&"OMSL"), "aao field_list must carry OMSL");
        assert!(names.contains(&"DOL"), "aao field_list must carry DOL");

        for kind in [ArrayKind::Waveform, ArrayKind::Aai, ArrayKind::SubArray] {
            let r = WaveformRecord::with_kind(kind);
            let names: Vec<&str> = r.field_list().iter().map(|f| f.name).collect();
            assert!(
                !names.contains(&"OMSL") && !names.contains(&"DOL"),
                "{kind:?} must not declare OMSL/DOL"
            );
        }
    }

    /// RARM (re-arm, `pp(TRUE)` settable) is waveform-only. BUSY
    /// (acquisition-active, `special(SPC_NOMOD)` read-only) is declared by
    /// waveform AND subArray (`subArrayRecord.dbd.pod:390-393`); softIoc on a
    /// loaded `subArray` record:
    ///
    /// ```text
    /// dbgf SUB.BUSY   -> DBF_SHORT: 0
    /// dbpf SUB.BUSY 1 -> dbPut Attempt to modify noMod field PV: SUB.BUSY
    /// ```
    ///
    /// aai/aao declare neither field.
    #[test]
    fn waveform_rarm_busy_fields_waveform_only() {
        let wf = WaveformRecord::with_kind(ArrayKind::Waveform);
        let rarm = wf
            .field_list()
            .iter()
            .find(|f| f.name == "RARM")
            .expect("waveform field_list must carry RARM");
        let busy = wf
            .field_list()
            .iter()
            .find(|f| f.name == "BUSY")
            .expect("waveform field_list must carry BUSY");
        assert!(!rarm.read_only, "RARM is pp(TRUE) — client-settable");
        assert!(busy.read_only, "BUSY is special(SPC_NOMOD) — read-only");

        // RARM round-trips through put/get; BUSY is read-only (device-set) and
        // reflects the struct flag.
        let mut wf = WaveformRecord::with_kind(ArrayKind::Waveform);
        wf.put_field("RARM", EpicsValue::Short(1)).unwrap();
        assert_eq!(wf.get_field("RARM"), Some(EpicsValue::Short(1)));
        assert_eq!(wf.get_field("BUSY"), Some(EpicsValue::Short(0)));
        assert!(
            wf.put_field("BUSY", EpicsValue::Short(1)).is_err(),
            "BUSY must reject CA puts (SPC_NOMOD)"
        );
        wf.busy = true;
        assert_eq!(wf.get_field("BUSY"), Some(EpicsValue::Short(1)));

        // aai/aao declare neither field: not in field_list, get None, put errors.
        for kind in [ArrayKind::Aai, ArrayKind::Aao] {
            let mut r = WaveformRecord::with_kind(kind);
            let names: Vec<&str> = r.field_list().iter().map(|f| f.name).collect();
            assert!(
                !names.contains(&"RARM") && !names.contains(&"BUSY"),
                "{kind:?} must not declare RARM/BUSY"
            );
            assert_eq!(r.get_field("RARM"), None, "{kind:?} RARM get must be None");
            assert_eq!(r.get_field("BUSY"), None, "{kind:?} BUSY get must be None");
            assert!(r.put_field("RARM", EpicsValue::Short(1)).is_err());
        }

        // subArray declares BUSY (read-only) but no RARM.
        let mut sub = WaveformRecord::with_kind(ArrayKind::SubArray);
        let names: Vec<&str> = sub.field_list().iter().map(|f| f.name).collect();
        assert!(
            !names.contains(&"RARM"),
            "subArray must not declare RARM (waveform-only)"
        );
        assert_eq!(sub.get_field("RARM"), None);
        let busy = sub
            .field_list()
            .iter()
            .find(|f| f.name == "BUSY")
            .expect("subArray field_list must carry BUSY");
        assert!(busy.read_only, "subArray BUSY is special(SPC_NOMOD)");
        assert_eq!(sub.get_field("BUSY"), Some(EpicsValue::Short(0)));
        assert!(
            sub.put_field("BUSY", EpicsValue::Short(1)).is_err(),
            "subArray BUSY must reject CA puts (SPC_NOMOD)"
        );
        sub.busy = true;
        assert_eq!(sub.get_field("BUSY"), Some(EpicsValue::Short(1)));
    }

    /// The static field sets and the runtime get/put arms must agree on which
    /// kinds declare BUSY — [`WaveformRecord::declares_busy`] is the one owner,
    /// so no kind may carry the FieldDesc while the arms deny the name (or the
    /// reverse: a name the arms answer but `field_list` never advertises).
    #[test]
    fn waveform_busy_field_set_matches_declares_busy() {
        for kind in [
            ArrayKind::Waveform,
            ArrayKind::Aai,
            ArrayKind::Aao,
            ArrayKind::SubArray,
        ] {
            let r = WaveformRecord::with_kind(kind);
            let in_set = r.field_list().iter().any(|f| f.name == "BUSY");
            assert_eq!(
                in_set,
                r.declares_busy(),
                "{kind:?}: field_list BUSY membership must match declares_busy()"
            );
            assert_eq!(
                r.get_field("BUSY").is_some(),
                r.declares_busy(),
                "{kind:?}: BUSY get arm must match declares_busy()"
            );
        }
    }

    /// `as_any_mut` exposes the concrete record so device support can drive the
    /// fields with no generic put path — read-only BUSY and the device-reset
    /// RARM (the TimeSeries device support uses exactly this downcast).
    #[test]
    fn waveform_as_any_mut_downcasts_to_concrete_record() {
        let mut r: Box<dyn Record> = Box::new(WaveformRecord::with_kind(ArrayKind::Waveform));
        let wf = r
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<WaveformRecord>())
            .expect("waveform must expose itself via as_any_mut");
        // Device-only writes that the generic put path cannot do: BUSY is
        // read-only, RARM is reset by the device after applying it.
        wf.busy = true;
        wf.rarm = 0;
        assert_eq!(r.get_field("BUSY"), Some(EpicsValue::Short(1)));
        assert_eq!(r.get_field("RARM"), Some(EpicsValue::Short(0)));
    }

    /// OMSL (resolved to a Short index by the central menu path) and DOL
    /// round-trip through aao's get/put; non-aao kinds expose neither.
    #[test]
    fn aao_omsl_dol_round_trip_and_kind_gated() {
        let mut aao = WaveformRecord::with_kind(ArrayKind::Aao);
        aao.put_field("OMSL", EpicsValue::Short(MENU_OMSL_CLOSED_LOOP))
            .unwrap();
        aao.put_field("DOL", EpicsValue::String("src.VAL".into()))
            .unwrap();
        assert_eq!(
            aao.get_field("OMSL"),
            Some(EpicsValue::Short(MENU_OMSL_CLOSED_LOOP))
        );
        assert_eq!(
            aao.get_field("DOL"),
            Some(EpicsValue::String("src.VAL".into()))
        );

        // waveform has no OMSL/DOL: get returns None, put is FieldNotFound.
        let mut wf = WaveformRecord::with_kind(ArrayKind::Waveform);
        assert_eq!(wf.get_field("OMSL"), None);
        assert_eq!(wf.get_field("DOL"), None);
        assert!(wf.put_field("OMSL", EpicsValue::Short(1)).is_err());
        assert!(
            wf.put_field("DOL", EpicsValue::String("src.VAL".into()))
                .is_err()
        );
    }

    /// C `aaoRecord.c::fetchValue` process arm (`!init && !isConst`): an aao
    /// with `omsl == closed_loop` and a real (non-constant) DOL link emits a
    /// `ReadDbLink { DOL -> VAL }` pre-input action so the framework pulls the
    /// source array into VAL (which sets NORD) before the write.
    #[test]
    fn aao_closed_loop_real_link_emits_read_db_link() {
        let mut aao = WaveformRecord::with_kind(ArrayKind::Aao);
        aao.omsl = MENU_OMSL_CLOSED_LOOP;
        aao.dol = "srcWaveform.VAL".to_string();
        assert_eq!(
            aao.pre_input_link_actions(),
            vec![ProcessAction::ReadDbLink {
                link_field: "DOL",
                target_field: "VAL",
            }]
        );

        // A bare record name parses to a DB link too (parse_link_v2), so it
        // also arms the fetch.
        aao.dol = "srcWaveform".to_string();
        assert_eq!(aao.pre_input_link_actions().len(), 1);
    }

    /// C gates the process-time fetch on `!dbLinkIsConstant`: a constant DOL
    /// (numeric/array literal) is loaded once at init, never re-applied per
    /// cycle. An empty DOL has no source at all. Both emit no action, so a
    /// client caput to VAL is not clobbered.
    #[test]
    fn aao_closed_loop_constant_or_empty_dol_emits_nothing() {
        let mut aao = WaveformRecord::with_kind(ArrayKind::Aao);
        aao.omsl = MENU_OMSL_CLOSED_LOOP;

        // Constant array literal. NOT `1 2 3`: `epicsParseDouble` rejects the
        // trailing text, so C parses that as a PV_LINK to a record named `1`
        // (`dbStaticLib.c:2346-2357`).
        aao.dol = "[1,2,3]".to_string();
        assert!(aao.pre_input_link_actions().is_empty());

        aao.dol = "42".to_string(); // constant scalar literal
        assert!(aao.pre_input_link_actions().is_empty());

        aao.dol = String::new(); // unset
        assert!(aao.pre_input_link_actions().is_empty());

        aao.dol = "   ".to_string(); // whitespace-only
        assert!(aao.pre_input_link_actions().is_empty());
    }

    /// Supervisory mode (the default) never fetches DOL, even with a real
    /// link configured (C `if(prec->omsl != menuOmslclosed_loop) return 0`).
    /// And the other three array kinds have no OMSL/DOL, so they never fetch
    /// regardless of the struct's `omsl` value.
    #[test]
    fn supervisory_and_non_aao_kinds_emit_nothing() {
        let mut aao = WaveformRecord::with_kind(ArrayKind::Aao);
        aao.omsl = 0; // supervisory
        aao.dol = "srcWaveform.VAL".to_string();
        assert!(aao.pre_input_link_actions().is_empty());

        for kind in [ArrayKind::Waveform, ArrayKind::Aai, ArrayKind::SubArray] {
            let mut r = WaveformRecord::with_kind(kind);
            // Force the would-be fetch state; the kind gate must still win.
            r.omsl = MENU_OMSL_CLOSED_LOOP;
            r.dol = "srcWaveform.VAL".to_string();
            assert!(
                r.pre_input_link_actions().is_empty(),
                "{kind:?} has no OMSL/DOL and must not fetch"
            );
        }
    }

    /// The framework applies the fetched array through `put_field("VAL", ...)`,
    /// which sets `NORD = element count` — the contract C `fetchValue` relies
    /// on (`prec->nord = nReq`). Pin it for the aao DOL-pull path.
    #[test]
    fn aao_val_put_sets_nord_for_dol_pull() {
        let mut aao = WaveformRecord::with_kind(ArrayKind::Aao);
        aao.nelm = 8;
        aao.put_field("VAL", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
            .unwrap();
        assert_eq!(aao.nord, 3, "NORD must equal the pulled element count");
    }

    /// NORD is capped at NELM by construction at every VAL array write
    /// (C dbPutField / dbGetLink bound the request to NELM). Boundary
    /// cases: source < NELM (NORD = source), == NELM, and > NELM
    /// (NORD = NELM, the previously over-reported case). Exercised here
    /// on a plain waveform — the cap lives in the shared put_field VAL
    /// arm, so it covers aao DOL pulls and every other internal delivery.
    #[test]
    fn put_val_caps_nord_at_nelm() {
        // FTVL=LONG: the buffer's element type is what the source converts INTO
        // (C `bptr` is `dbValueSize(ftvl)`-typed), so a LONG-element record is
        // what keeps a LongArray source a LongArray.
        let mut wf = WaveformRecord::new(4, DbFieldType::Long);
        wf.kind = ArrayKind::Waveform;

        // source < NELM
        wf.put_field("VAL", EpicsValue::LongArray(vec![1, 2]))
            .unwrap();
        assert_eq!(wf.nord, 2, "source < NELM: NORD == source length");

        // source == NELM
        wf.put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3, 4]))
            .unwrap();
        assert_eq!(wf.nord, 4, "source == NELM: NORD == NELM");

        // source > NELM: NORD must clamp to NELM, not the source length
        wf.put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3, 4, 5, 6, 7]))
            .unwrap();
        assert_eq!(wf.nord, 4, "source > NELM: NORD must clamp to NELM");
        // and the served VAL holds exactly NORD (== NELM) valid elements
        let val = wf.get_field("VAL").unwrap();
        if let EpicsValue::LongArray(v) = val {
            assert_eq!(v, vec![1, 2, 3, 4], "VAL serves exactly NELM elements");
        } else {
            panic!("VAL should be LongArray, got {val:?}");
        }
    }

    /// PR #a02c310 follow-up: subArray slices source[INDX..INDX+NELM]
    /// into VAL with NORD set to the actual copied length. Source
    /// shorter than INDX → NORD=0. INDX+NELM > source.len → only
    /// available tail is copied, rest zero-padded to NELM.
    #[test]
    fn subarray_slices_input_at_indx_with_nelm_take() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        // FTVL defaults to the declared STRING (menuFtype index 0); this case is
        // about the SLICE, so give it the DOUBLE buffer a real `.db` would.
        r.set_ftvl(Ftype::Double);
        // 4-element double buffer; consume up to 4 from offset 2.
        // MALM is the source-view cap (C floors it to >= 1), so it must be at
        // least the source length to expose the whole source — a real subArray
        // .db sets MALM to the upstream waveform's NELM.
        r.put_field("MALM", EpicsValue::Long(6)).unwrap();
        r.put_field("NELM", EpicsValue::Long(4)).unwrap();
        r.put_field("INDX", EpicsValue::Long(2)).unwrap();
        let source = EpicsValue::DoubleArray(vec![10.0, 11.0, 12.0, 13.0, 14.0, 15.0]);
        r.set_val(source).unwrap();
        assert_eq!(r.nord, 4, "should copy 4 elements from offset 2");
        let val = r.get_field("VAL").unwrap();
        if let EpicsValue::DoubleArray(v) = val {
            assert_eq!(v, vec![12.0, 13.0, 14.0, 15.0]);
        } else {
            panic!("VAL should be DoubleArray, got {val:?}");
        }
    }

    #[test]
    fn subarray_indx_out_of_range_yields_nord_zero() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        // MALM=20 leaves INDX=10 un-clamped (10 < MALM), so the slice starts
        // past the 3-element source and yields NORD=0. With MALM at/under the
        // source length C would instead clamp INDX to MALM-1 (313-314) and read
        // a tail; this case isolates the genuine "INDX beyond source" path.
        r.put_field("MALM", EpicsValue::Long(20)).unwrap();
        r.put_field("NELM", EpicsValue::Long(3)).unwrap();
        r.put_field("INDX", EpicsValue::Long(10)).unwrap();
        let source = EpicsValue::LongArray(vec![1, 2, 3]);
        r.put_field("FTVL", EpicsValue::Short(5)).unwrap(); // LONG
        r.set_val(source).unwrap();
        assert_eq!(r.nord, 0, "INDX past source.len must zero NORD");
    }

    #[test]
    fn subarray_partial_tail_zero_pads_to_nelm() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        r.set_ftvl(Ftype::Double);
        // MALM=5 == source length: the whole source is visible, the slice from
        // offset 3 has only 2 valid elements and zero-pads the rest to NELM.
        r.put_field("MALM", EpicsValue::Long(5)).unwrap();
        r.put_field("NELM", EpicsValue::Long(5)).unwrap();
        r.put_field("INDX", EpicsValue::Long(3)).unwrap();
        let source = EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0]);
        r.set_val(source).unwrap();
        assert_eq!(r.nord, 2, "only 2 elements available from offset 3");
        // get_field("VAL") truncates to NORD — caller-visible slice
        // is only the 2 valid elements.
        if let Some(EpicsValue::DoubleArray(v)) = r.get_field("VAL") {
            assert_eq!(v, vec![4.0, 5.0]);
        } else {
            panic!("VAL must be DoubleArray of valid tail");
        }
    }

    #[test]
    fn subarray_malm_caps_visible_source_length() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        r.set_ftvl(Ftype::Double);
        r.put_field("NELM", EpicsValue::Long(4)).unwrap();
        r.put_field("INDX", EpicsValue::Long(0)).unwrap();
        // MALM caps how far into the source we look — even if the
        // source has 8 elements, MALM=3 keeps us to indices [0..3).
        r.put_field("MALM", EpicsValue::Long(3)).unwrap();
        let source = EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        r.set_val(source).unwrap();
        assert_eq!(r.nord, 3, "MALM=3 limits visible source to 3 elements");
        if let Some(EpicsValue::DoubleArray(v)) = r.get_field("VAL") {
            assert_eq!(v, vec![1.0, 2.0, 3.0]);
        } else {
            panic!("VAL truncated to MALM-bound prefix");
        }
    }

    #[test]
    fn subarray_indx_malm_fields_round_trip() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        r.put_field("INDX", EpicsValue::Long(5)).unwrap();
        r.put_field("MALM", EpicsValue::Long(100)).unwrap();
        assert_eq!(r.get_field("INDX"), Some(EpicsValue::ULong(5)));
        assert_eq!(r.get_field("MALM"), Some(EpicsValue::ULong(100)));
    }

    #[test]
    fn subarray_malm_defaults_to_one_and_floors_zero() {
        // C `subArrayRecord.dbd` `initial("1")` + `init_record` floor
        // (subArrayRecord.c:96-97): MALM is never 0.
        let r = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert_eq!(r.get_field("MALM"), Some(EpicsValue::ULong(1)));
        let mut z = WaveformRecord::with_kind(ArrayKind::SubArray);
        z.put_field("MALM", EpicsValue::Long(0)).unwrap();
        assert_eq!(
            z.get_field("MALM"),
            Some(EpicsValue::ULong(1)),
            "MALM put of 0 floors back to 1"
        );
    }

    #[test]
    fn subarray_init_record_clamps_nelm_to_malm_independent_of_load_order() {
        // The defect this closes: per-put clamping made the clamped NELM depend
        // on whether the .db set NELM before or after MALM. C clamps NELM->MALM
        // in init_record (96-104) post-load, so the order is irrelevant. Both
        // orders must converge to NELM == MALM after init_record.
        let mut nelm_first = WaveformRecord::with_kind(ArrayKind::SubArray);
        nelm_first.put_field("NELM", EpicsValue::Long(50)).unwrap();
        nelm_first.put_field("MALM", EpicsValue::Long(8)).unwrap();
        nelm_first.init_record(0).unwrap();
        assert_eq!(nelm_first.nelm, 8, "NELM clamped down to MALM at init");

        let mut malm_first = WaveformRecord::with_kind(ArrayKind::SubArray);
        malm_first.put_field("MALM", EpicsValue::Long(8)).unwrap();
        malm_first.put_field("NELM", EpicsValue::Long(50)).unwrap();
        malm_first.init_record(0).unwrap();
        assert_eq!(
            malm_first.nelm, 8,
            "same clamp regardless of NELM/MALM .db load order"
        );
    }

    #[test]
    fn waveform_does_not_expose_indx_malm() {
        // Non-subArray record kinds must NOT expose INDX/MALM via the
        // field map — those fields are subArray-specific.
        let r = WaveformRecord::with_kind(ArrayKind::Waveform);
        assert!(r.get_field("INDX").is_none());
        assert!(r.get_field("MALM").is_none());
    }

    #[test]
    fn br_r13_waveform_ftvl_uint64_storage_and_field_type() {
        // a `waveform` with `FTVL = UINT64` (menuFtype index 8)
        // must allocate a `UInt64Array` VAL buffer and advertise VAL as
        // `DbFieldType::UInt64`. On main FTVL 8 fell through to
        // `DoubleArray` / `DbFieldType::Double`, so unsigned-64 waveforms
        // were not representable.
        let mut r = WaveformRecord::with_kind(ArrayKind::Waveform);
        r.put_field("NELM", EpicsValue::Long(3)).unwrap();
        r.put_field("FTVL", EpicsValue::Short(8)).unwrap(); // UINT64

        // VAL buffer is a UInt64Array (NORD=0 fresh → empty, still typed).
        match r.get_field("VAL") {
            Some(EpicsValue::UInt64Array(_)) => {}
            other => panic!("FTVL=UINT64 VAL must be UInt64Array, got {other:?}"),
        }

        // The declaration cannot answer "what type is VAL" for a waveform, and
        // must not pretend to: `waveformRecord.dbd` gives VAL a placeholder
        // type and `special(SPC_DBADDR)`, and C's `cvt_dbaddr` overwrites
        // `paddr->field_type` from FTVL at name-resolution time. So the
        // generated FieldDesc is `runtime_typed`, `declared_field_type` is
        // `None`, and every consumer falls back to the stored value — which is
        // the UInt64Array asserted above. (The hand-written table used to
        // FTVL-switch the FieldDesc itself and answer `UInt64` here; that was a
        // second declaration of VAL, contradicting the `.dbd`'s.)
        let val = r.field_list().iter().find(|f| f.name == "VAL").unwrap();
        assert!(
            val.runtime_typed,
            "waveform VAL is typed by FTVL, not by the .dbd"
        );
        assert_eq!(
            crate::server::record::record_instance::declared_field_type_of(&r, "VAL"),
            None,
            "a runtime-typed field has no declared type to hand out"
        );

        // A value above i64::MAX round-trips without precision loss.
        let big = u64::MAX - 9;
        r.put_field("VAL", EpicsValue::UInt64Array(vec![big, 0, 1]))
            .unwrap();
        match r.get_field("VAL") {
            Some(EpicsValue::UInt64Array(v)) => assert_eq!(v[0], big),
            other => panic!("expected UInt64Array, got {other:?}"),
        }

        // INT64 (index 7) likewise allocates a typed Int64Array buffer.
        let mut r2 = WaveformRecord::with_kind(ArrayKind::Waveform);
        r2.put_field("NELM", EpicsValue::Long(2)).unwrap();
        r2.put_field("FTVL", EpicsValue::Short(7)).unwrap(); // INT64
        assert!(matches!(
            r2.get_field("VAL"),
            Some(EpicsValue::Int64Array(_))
        ));
        assert_eq!(
            crate::server::record::record_instance::declared_field_type_of(&r2, "VAL"),
            None,
            "a runtime-typed field has no declared type to hand out"
        );
    }

    /// MPST/APST are `menu(waveformPOST)` served as DBR_ENUM. The base
    /// snapshot path promotes the stored Short to `Enum` and attaches the
    /// labels in `.dbd` value order — which is REVERSED vs `menu(menuPost)`:
    /// "Always" is index 0, "On Change" is index 1.
    #[test]
    fn waveform_mpst_apst_snapshot_is_enum_with_reversed_post_labels() {
        use crate::server::record::RecordInstance;
        let mut rec = WaveformRecord::with_kind(ArrayKind::Waveform);
        rec.put_field("MPST", EpicsValue::Short(0)).unwrap();
        assert_eq!(rec.get_field("MPST"), Some(EpicsValue::Short(0)));
        let inst = RecordInstance::new("WF:MPST".into(), rec);
        let snap = inst.snapshot_for_field("MPST").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(0));
        assert_eq!(
            snap.enums.as_ref().unwrap().strings,
            vec!["Always", "On Change"],
            "waveformPOST index 0 must be \"Always\" (reverse of menuPost)"
        );
    }

    /// The simulation block (SIMM/SIML/SIOL/SIMS/OLDSIMM) is served on
    /// waveform/aai/aao but NOT subArray. SIMM is menu(menuYesNo); SIMS
    /// (menuAlarmSevr) and OLDSIMM (menuSimm) resolve via the shared
    /// registry, so their wire labels come from the central tables.
    #[test]
    fn waveform_sim_block_served_per_kind() {
        use crate::server::record::RecordInstance;

        for kind in [ArrayKind::Waveform, ArrayKind::Aai, ArrayKind::Aao] {
            let mut rec = WaveformRecord::with_kind(kind);
            rec.put_field("SIMM", EpicsValue::Short(1)).unwrap();
            assert_eq!(rec.get_field("SIMM"), Some(EpicsValue::Short(1)));
            rec.put_field("SIML", EpicsValue::String("sim:mode".into()))
                .unwrap();
            assert_eq!(
                rec.get_field("SIML"),
                Some(EpicsValue::String("sim:mode".into()))
            );
            rec.put_field("SIOL", EpicsValue::String("sim:in".into()))
                .unwrap();
            assert_eq!(
                rec.get_field("SIOL"),
                Some(EpicsValue::String("sim:in".into()))
            );
            rec.put_field("SIMS", EpicsValue::Short(2)).unwrap();
            assert_eq!(rec.get_field("SIMS"), Some(EpicsValue::Short(2)));
            // OLDSIMM is special(SPC_NOMOD) and lives in the common fields
            // (the simulation owner's latch) — readable, not client-writable.
            let mut inst = RecordInstance::new("WF:OLDSIMM".into(), rec);
            assert!(matches!(
                inst.put_common_field("OLDSIMM", EpicsValue::Short(1)),
                Err(crate::error::CaError::ReadOnlyField(_))
            ));
            assert_eq!(inst.get_common_field("OLDSIMM"), Some(EpicsValue::Short(0)));
        }

        // SIMM snapshot carries the NO/YES menuYesNo labels on these records.
        let mut wf = WaveformRecord::with_kind(ArrayKind::Waveform);
        wf.put_field("SIMM", EpicsValue::Short(1)).unwrap();
        let inst = RecordInstance::new("WF:SIMM".into(), wf);
        let snap = inst.snapshot_for_field("SIMM").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(snap.enums.as_ref().unwrap().strings, vec!["NO", "YES"]);
        // OLDSIMM resolves to the three-choice menuSimm via the shared registry.
        let snap_old = inst.snapshot_for_field("OLDSIMM").unwrap();
        assert_eq!(
            snap_old.enums.as_ref().unwrap().strings,
            vec!["NO", "YES", "RAW"]
        );

        // subArray has no sim block — those names must not resolve.
        let sub = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert_eq!(sub.get_field("SIMM"), None);
        let mut sub_mut = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert!(matches!(
            sub_mut.put_field("SIMM", EpicsValue::Short(1)),
            Err(crate::error::CaError::FieldNotFound(_))
        ));
    }

    #[test]
    fn br_r13_waveform_new_from_uint64_dbf_type() {
        // `WaveformRecord::new(_, DbFieldType::UInt64)` must mint
        // a UInt64Array VAL and FTVL index 8, not fall through to Double.
        let r = WaveformRecord::new(4, DbFieldType::UInt64);
        assert_eq!(r.ftvl, Ftype::UInt64);
        assert_eq!(r.ftvl.index(), 8);
        // The VAL buffer is a UInt64Array sized to NELM; `get_field`
        // truncates to NORD (0 when fresh), so check the buffer directly.
        assert!(matches!(&r.val, EpicsValue::UInt64Array(v) if v.len() == 4));
    }
}
