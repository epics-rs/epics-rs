//! The `mca` record type — state and field access.
//!
//! C: `mca/mcaApp/mcaSrc/mcaRecord.c` (upstream `687d563`).
//!
//! The field DECLARATION is not here. It is generated from the vendored
//! `dbd/mcaRecord.dbd` into [`dbd_generated`] and reached through
//! `FieldDeclaration::field_list`, the one resolver every consumer goes
//! through — so this record type, like every other, has exactly one declaration
//! and it is the `.dbd`.

pub mod cycle;
pub mod dbd_generated;
mod roi;

use std::any::Any;
use std::sync::OnceLock;
use std::time::SystemTime;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{
    CommonFields, FieldDesc, FieldMetadataOverride, Ftype, ProcessAction, ProcessOutcome, Record,
};
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

pub use cycle::{McaCommand, McaStatus};

/// C `mcaRecord.c:197` `#define NUM_ROI 32`.
pub const NUM_ROI: usize = 32;

/// C `mcaRecord.c:90` `#define VERSION 6.01`, which `init_record` pass 0 stores
/// into `VERS` — overwriting the `.dbd`'s `initial("1")`.
pub const VERSION: f64 = 6.01;

/// One region of interest: the four fields `R{i}LO`, `R{i}HI`, `R{i}BG`,
/// `R{i}IP` a client sets, and the three the record computes (`R{i}`, `R{i}N`,
/// `R{i}P`) plus its name (`R{i}NM`).
///
/// C splits these across three C structs whose layout must "EXACTLY match the
/// equivalent structures in the record" (`mcaRecord.c:181-195`) — the ROI
/// controls, the ROI sums, and the names are three separate runs of `.dbd`
/// fields, walked by pointer arithmetic. Nothing outside C's struct layout
/// requires that split, and inside it, one mis-ordered `.dbd` field silently
/// mis-reads every ROI. Here the ROI is ONE value; the `.dbd` order is a
/// property of the generated field table alone.
#[derive(Debug, Clone, PartialEq)]
pub struct Roi {
    /// `R{i}LO` — low channel. `.dbd` `initial("-1")`; a negative LO disables
    /// the region (C `mcaRecord.c:371` `if (lo >= 0 && hi >= lo)`).
    pub lo: i32,
    /// `R{i}HI` — high channel. `.dbd` `initial("-1")`.
    pub hi: i32,
    /// `R{i}BG` — number of background channels averaged either side of LO/HI.
    /// Negative means "no background": C `mcaRecord.c:373` `if (proi->nbg >= 0)`.
    pub nbg: i16,
    /// `R{i}IP` — `menu(mcaR0IP)`: is this region's preset count armed?
    pub is_preset: u16,
    /// `R{i}` — the region's total counts.
    pub sum: f64,
    /// `R{i}N` — the region's counts net of the interpolated background.
    pub net: f64,
    /// `R{i}P` — the preset: when `is_preset` and `net >= preset`, the record
    /// stops acquisition.
    pub preset: f64,
    /// `R{i}NM` — the region's name (`size(16)`).
    pub name: PvString,
}

impl Default for Roi {
    fn default() -> Self {
        Self {
            lo: -1,
            hi: -1,
            nbg: 0,
            is_preset: 0,
            sum: 0.0,
            net: 0.0,
            preset: 0.0,
            name: PvString::new(),
        }
    }
}

/// The `mca` record.
///
/// Field names below are the `.dbd`'s, lower-cased; the generated table is the
/// declaration of every one of them.
#[derive(Debug, Clone)]
pub struct McaRecord {
    /// `VERS` — C `init_record` pass 0 stores [`VERSION`].
    pub vers: f64,
    /// `VAL` — the spectrum. `NMAX` elements wide, element type `FTVL`
    /// (`special(SPC_DBADDR)`; C re-types it from FTVL in `cvt_dbaddr`,
    /// `mcaRecord.c:847-863`).
    pub val: EpicsValue,
    /// `BG` — the interpolated background curve `sum_ROIs` writes under each
    /// enabled region. Same width and element type as [`Self::val`].
    pub bg: EpicsValue,
    pub hopr: f64,
    pub lopr: f64,
    /// `NMAX` — the allocated channel count. `special(SPC_NOMOD)`: fixed at load.
    pub nmax: i32,
    /// `NORD` — channels actually read. `special(SPC_NOMOD)` to a client; device
    /// support and `put_array_info` set it.
    pub nord: i32,
    pub prec: i16,
    /// `FTVL` — the spectrum's element type. `special(SPC_NOMOD)`, `.dbd`
    /// `initial("5")` = `LONG`.
    pub ftvl: Ftype,

    /// `STRT` — start acquisition (`menu(mcaSTRT)`, `pp(TRUE)`).
    pub strt: u16,
    /// `ERST` — erase and start (`menu(mcaSTRT)`, `special(SPC_MOD)`).
    pub erst: u16,
    /// `STOP` — stop acquisition.
    pub stop: u16,
    /// `ACQG` — acquiring. The record's readback of the device's state.
    pub acqg: u16,
    /// `READ` — read the spectrum on the next process.
    pub read: u16,
    /// `RDNG` — a read-data callback is outstanding (device support sets it).
    pub rdng: u16,
    /// `RDNS` — a read-status callback is outstanding (device support sets it).
    pub rdns: u16,
    /// `ERAS` — erase (`special(SPC_MOD)`).
    pub eras: u16,
    /// `CHAS` — channel-advance source (`menu(mcaCHAS)`).
    pub chas: u16,
    /// `NUSE` — channels in use. Clamped to `NMAX` on every path.
    pub nuse: i32,
    pub seq: i32,
    /// `DWEL` — dwell time per channel. Device support may write back the
    /// ACTUAL dwell, which is why the record re-reads it from the status.
    pub dwel: f64,
    pub pscl: i32,
    pub prtm: f64,
    pub pltm: f64,
    pub pct: f64,
    pub pctl: i32,
    pub pcth: i32,
    pub pswp: i32,
    /// `MODE` — `menu(mcaMODE)`: PHA / MCS / List.
    pub mode: u16,

    pub calo: f64,
    pub cals: f64,
    pub calq: f64,
    pub egu: PvString,
    pub tth: f64,

    /// `ERTM` — elapsed real time, from the device status.
    pub ertm: f64,
    /// `ELTM` — elapsed live time, from the device status.
    pub eltm: f64,
    /// `DTIM` — average dead time, percent.
    pub dtim: f64,
    /// `IDTIM` — instantaneous dead time, percent. The record's ALARM field:
    /// HIHI/HIGH/LOW/LOLO are compared against THIS, not against VAL
    /// (`mcaRecord.c:962-1003`).
    pub idtim: f64,
    /// `STIM` — the time acquisition stopped, rendered to millisecond precision.
    pub stim: PvString,
    /// `RTIM` — the time the last spectrum read began, as seconds past the EPICS
    /// epoch.
    pub rtim: f64,
    /// `ACT` — total counts, from the device status.
    pub act: i32,
    /// `NACK` — the last device-support message was not acknowledged.
    pub nack: i16,

    pub hihi: f64,
    pub lolo: f64,
    pub high: f64,
    pub low: f64,
    pub hhsv: u16,
    pub llsv: u16,
    pub hsv: u16,
    pub lsv: u16,
    pub hyst: f64,
    pub lalm: f64,

    pub simm: u16,
    pub sims: u16,

    /// `MMAP` / `RMAP` — C's per-cycle "post this field" bitmaps
    /// (`mcaRecord.c:221-306`). They exist in C because a C record must call
    /// `db_post_events` itself; the port's framework owns posting, so nothing
    /// writes these. C clears both at the end of every `monitor()`
    /// (`UNMARK_ALL`), so a client has never been able to observe them as
    /// anything but zero — see [`Record::get_field`].
    pub mmap: u32,
    pub rmap: u32,
    /// `NEWV` — the setup fields written since the last process, which the next
    /// process must send to device support. Real state, not bookkeeping: it is
    /// written by `special()` and consumed by `process()`.
    pub newv: u32,
    /// `NEWR` — the regions whose sums must be recomputed.
    pub newr: u32,

    /// The 32 regions of interest.
    pub roi: [Roi; NUM_ROI],

    /// C's `PSTATUS` — the last status device support read back. It is declared
    /// `DBF_NOACCESS` (a bare `void *` to a heap `mcaStatus`), so it has no CA
    /// representation and no row in the generated table: it is state, not a
    /// field.
    ///
    /// The record keeps it because `process()` must commit `ACQG` from the SAME
    /// status the read was decided on ([`McaRecord::apply_status`] must
    /// not commit it early, or a client sees acquisition stop before the final
    /// spectrum is posted — C `mcaRecord.c:735-742`).
    pub status: McaStatus,
}

impl Default for McaRecord {
    fn default() -> Self {
        // The `.dbd` `initial(...)`s are applied by the db loader
        // (`apply_dbd_initials`) on the `.db` path; they are repeated here so a
        // record built directly in Rust starts in the same state as one loaded
        // from a `.db`, rather than in a zeroed state no C record ever has.
        Self {
            vers: 1.0,
            val: EpicsValue::LongArray(Vec::new()),
            bg: EpicsValue::LongArray(Vec::new()),
            hopr: 0.0,
            lopr: 0.0,
            nmax: 1,
            nord: 0,
            prec: 0,
            ftvl: Ftype::Long,
            strt: 0,
            erst: 0,
            stop: 0,
            acqg: 0,
            read: 0,
            rdng: 0,
            rdns: 0,
            eras: 0,
            chas: 0,
            nuse: 0,
            seq: 0,
            dwel: 1.0,
            pscl: 1,
            prtm: 0.0,
            pltm: 0.0,
            pct: 0.0,
            pctl: 0,
            pcth: 0,
            pswp: 1,
            mode: 0,
            calo: 0.0,
            cals: 1.0,
            calq: 0.0,
            egu: PvString::new(),
            tth: 10.0,
            ertm: 0.0,
            eltm: 0.0,
            dtim: 0.0,
            idtim: 0.0,
            stim: PvString::new(),
            rtim: 0.0,
            act: 0,
            nack: 0,
            hihi: 0.0,
            lolo: 0.0,
            high: 0.0,
            low: 0.0,
            hhsv: 0,
            llsv: 0,
            hsv: 0,
            lsv: 0,
            hyst: 0.0,
            lalm: 0.0,
            simm: 0,
            sims: 0,
            mmap: 0,
            rmap: 0,
            newv: 0,
            newr: 0,
            roi: std::array::from_fn(|_| Roi::default()),
            status: McaStatus::default(),
        }
    }
}

/// A field name that addresses one member of one region: `R7LO` -> `(7, "LO")`,
/// `R31` -> `(31, "")`, `R0NM` -> `(0, "NM")`.
///
/// The suffix set is closed, so the record's own `R`-prefixed scalars (`RDNG`,
/// `RDNS`, `RTIM`, `RMAP`) cannot be mistaken for a region: they carry no digits
/// after the `R`. An index outside `0..NUM_ROI` is not a field.
fn roi_field(name: &str) -> Option<(usize, &'static str)> {
    let rest = name.strip_prefix('R')?;
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let index: usize = rest[..digits].parse().ok()?;
    if index >= NUM_ROI {
        return None;
    }
    let member = match &rest[digits..] {
        "" => "",
        "LO" => "LO",
        "HI" => "HI",
        "BG" => "BG",
        "IP" => "IP",
        "N" => "N",
        "P" => "P",
        "NM" => "NM",
        _ => return None,
    };
    Some((index, member))
}

impl McaRecord {
    /// The element type the spectrum is stored and served as — C's
    /// `paddr->field_type = pmca->ftvl` (`mcaRecord.c:858`).
    pub fn element_type(&self) -> DbFieldType {
        self.ftvl.element_type()
    }

    /// How many channels this record's two spectrum buffers hold — C's `NMAX`
    /// under the floor `init_record` puts on it (`mcaRecord.c:424`):
    ///
    /// ```c
    /// if (pmca->nmax <= 0) pmca->nmax=1;
    /// ```
    ///
    /// Everything that sizes a buffer or advertises a channel width reads it
    /// from here, so the allocation, the served cut and the capacity CA is told
    /// cannot drift apart. `NMAX` is `special(SPC_NOMOD)`
    /// (`mcaRecord.dbd:78-82`), so this is fixed for the life of the record.
    fn capacity(&self) -> usize {
        self.nmax.max(1) as usize
    }

    /// An all-zero buffer `NMAX` elements deep, in the `FTVL` element type — C's
    /// `calloc(pmca->nmax, sizeofTypes[pmca->ftvl])` (`mcaRecord.c:430-431`).
    fn zeroed_buffer(&self) -> EpicsValue {
        let n = self.capacity();
        match self.ftvl {
            Ftype::String => EpicsValue::StringArray(vec![PvString::new(); n]),
            Ftype::Char => EpicsValue::CharArray(vec![0; n]),
            Ftype::UChar => EpicsValue::UCharArray(vec![0; n]),
            Ftype::Short => EpicsValue::ShortArray(vec![0; n]),
            Ftype::UShort => EpicsValue::UShortArray(vec![0; n]),
            Ftype::Long => EpicsValue::LongArray(vec![0; n]),
            Ftype::ULong => EpicsValue::ULongArray(vec![0; n]),
            Ftype::Int64 => EpicsValue::Int64Array(vec![0; n]),
            Ftype::UInt64 => EpicsValue::UInt64Array(vec![0; n]),
            Ftype::Float => EpicsValue::FloatArray(vec![0.0; n]),
            Ftype::Double => EpicsValue::DoubleArray(vec![0.0; n]),
            Ftype::Enum => EpicsValue::EnumArray(vec![0; n]),
        }
    }

    /// Zero the first `n` channels of the spectrum, leaving the buffer's `NMAX`
    /// width alone — C's `memset(pmca->bptr, 0, pmca->nuse*sizeofTypes[ftvl])`
    /// (`mcaRecord.c:614`).
    ///
    /// C's `sizeofTypes[]` (`:171`) is a NINE-entry table indexed by `FTVL`, a
    /// `menuFtype` index. In EPICS 7 `menuFtype` runs to ELEVEN (`INT64` and
    /// `UINT64` were added after that table was written) and `DOUBLE` is index
    /// 10 — so `sizeofTypes[pmca->ftvl]` reads two elements PAST the end of the
    /// array for the two 64-bit types, and lands on `sizeofTypes[8] == 8` for
    /// `DOUBLE` only by the accident that the table's last entry happens to be
    /// 8. The port has no such table: the buffer is a typed `EpicsValue`, so a
    /// channel count is a channel count for every element type.
    fn zero_spectrum(&mut self, n: usize) {
        macro_rules! clear {
            ($v:expr, $zero:expr) => {{
                let end = n.min($v.len());
                $v[..end].fill($zero);
            }};
        }
        match &mut self.val {
            EpicsValue::CharArray(v) => clear!(v, 0),
            EpicsValue::UCharArray(v) => clear!(v, 0),
            EpicsValue::ShortArray(v) => clear!(v, 0),
            EpicsValue::UShortArray(v) => clear!(v, 0),
            EpicsValue::LongArray(v) => clear!(v, 0),
            EpicsValue::ULongArray(v) => clear!(v, 0),
            EpicsValue::Int64Array(v) => clear!(v, 0),
            EpicsValue::UInt64Array(v) => clear!(v, 0),
            EpicsValue::FloatArray(v) => clear!(v, 0.0),
            EpicsValue::DoubleArray(v) => clear!(v, 0.0),
            EpicsValue::EnumArray(v) => clear!(v, 0),
            EpicsValue::StringArray(v) => clear!(v, PvString::new()),
            _ => {}
        }
    }

    /// Land a written array in one of the two `special(SPC_DBADDR)` buffers and
    /// set `NORD` from it — C `put_array_info` (`mcaRecord.c:875-881`):
    ///
    /// ```c
    /// pmca->nord = nNew;
    /// if (pmca->nord > pmca->nmax) pmca->nord = pmca->nmax;
    /// ```
    ///
    /// There is no `fieldIndex` branch, and `dbPut` calls the hook for every
    /// `SPC_DBADDR` field it writes (`dbAccess.c:1370-1373`), so the record's
    /// ONE `NORD` follows whichever of `VAL`/`BG` was written last. Nor does
    /// `get_array_info` branch, so that count then governs what both fields
    /// serve. Both writes go through here so no arm can move a buffer without
    /// moving the count that reads it. The `NMAX` clamp is already applied:
    /// [`McaRecord::land_buffer`] cuts the reported count to the capacity.
    fn land_array_field(
        &mut self,
        value: EpicsValue,
        pick: fn(&mut Self) -> &mut EpicsValue,
    ) -> CaResult<()> {
        let (buf, written) = self.land_buffer(value)?;
        *pick(self) = buf;
        self.nord = written as i32;
        Ok(())
    }

    /// The `VAL` arm of [`McaRecord::land_array_field`].
    fn land_spectrum(&mut self, value: EpicsValue) -> CaResult<()> {
        self.land_array_field(value, |r| &mut r.val)
    }

    /// Convert a written array to the `FTVL` element type and give it the
    /// record's fixed geometry — `capacity()` channels, zero-filled past what
    /// was written — reporting how many channels the writer actually supplied.
    ///
    /// This is the ONLY place either spectrum buffer is built from a value. C
    /// `calloc`s `bptr` and `pbg` once in `init_record` (`mcaRecord.c:426-431`)
    /// and never reallocates either, so their width is a property of the
    /// record's capacity, not of the last value written; `cvt_dbaddr` then
    /// hands CA `no_elements = pmca->nmax` against that standing allocation.
    /// Routing both puts through one call is what keeps a short `caput` from
    /// leaving a buffer narrower than the channel advertising it.
    fn land_buffer(&self, value: EpicsValue) -> CaResult<(EpicsValue, usize)> {
        let cap = self.capacity();
        let converted = value.convert_to(self.element_type());
        macro_rules! land {
            ($src:expr, $variant:ident, $zero:expr) => {{
                let mut arr = $src;
                let written = arr.len().min(cap);
                arr.resize(cap, $zero);
                Ok((EpicsValue::$variant(arr), written))
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
                "mca spectrum buffer: {other:?} does not convert to the FTVL \
                 element type"
            ))),
        }
    }

    /// The valid head of the spectrum — C `get_array_info`
    /// (`mcaRecord.c:865-873`):
    ///
    /// ```c
    /// *no_elements =  pmca->nord;
    /// if (*no_elements == 0) *no_elements = 1;
    /// ```
    ///
    /// The floor is C's, not a rounding convenience: `nord` is 0 on a record
    /// that has not acquired yet, and a zero-length array is not a value any
    /// CA client can take — `oldChannelNotify.cpp:287` refuses a request for
    /// zero elements outright. C therefore serves the first (zeroed) channel
    /// of the `NMAX`-wide buffer `init_record` allocated. `get_array_info` has
    /// no `fieldIndex` branch, so the same count governs `VAL` and `BG`.
    fn served_array(&self, buf: &EpicsValue) -> EpicsValue {
        let mut out = buf.clone();
        let n = self.nord.max(1) as usize;
        macro_rules! cut {
            ($v:expr) => {{
                $v.truncate(n);
            }};
        }
        match &mut out {
            EpicsValue::CharArray(v) => cut!(v),
            EpicsValue::UCharArray(v) => cut!(v),
            EpicsValue::ShortArray(v) => cut!(v),
            EpicsValue::UShortArray(v) => cut!(v),
            EpicsValue::LongArray(v) => cut!(v),
            EpicsValue::ULongArray(v) => cut!(v),
            EpicsValue::Int64Array(v) => cut!(v),
            EpicsValue::UInt64Array(v) => cut!(v),
            EpicsValue::FloatArray(v) => cut!(v),
            EpicsValue::DoubleArray(v) => cut!(v),
            EpicsValue::EnumArray(v) => cut!(v),
            EpicsValue::StringArray(v) => cut!(v),
            _ => {}
        }
        out
    }
}

/// The `EpicsValue` -> `f64` coercion every numeric put goes through, so a
/// `caput` of a string, a short or a double all land the same way.
fn as_f64(name: &str, v: &EpicsValue) -> CaResult<f64> {
    v.to_f64()
        .ok_or_else(|| CaError::TypeMismatch(format!("{name}: {v:?} is not numeric")))
}

fn as_i32(name: &str, v: &EpicsValue) -> CaResult<i32> {
    Ok(as_f64(name, v)? as i32)
}

fn as_i16(name: &str, v: &EpicsValue) -> CaResult<i16> {
    Ok(as_f64(name, v)? as i16)
}

fn as_u16(name: &str, v: &EpicsValue) -> CaResult<u16> {
    Ok(as_f64(name, v)?.max(0.0) as u16)
}

fn as_u32(name: &str, v: &EpicsValue) -> CaResult<u32> {
    Ok(as_f64(name, v)?.max(0.0) as u32)
}

fn as_string(v: &EpicsValue) -> PvString {
    match v {
        EpicsValue::String(s) => s.clone(),
        other => PvString::from(other.to_string().as_str()),
    }
}

impl Record for McaRecord {
    fn record_type(&self) -> &'static str {
        "mca"
    }

    /// The `mca` declaration: the table generated from the vendored
    /// `dbd/mcaRecord.dbd`.
    ///
    /// This hook is the framework's fallback for a record type
    /// `epics-base-rs`'s own `.dbd` set does not cover, and `mca` is such a type
    /// — the record lives in this crate, so its `.dbd` is vendored here and its
    /// table is generated here. What it must NEVER be is hand-written: a second
    /// declaration of a field is the defect family this whole line of work
    /// deletes, and `dbd_codegen::tests::generated_files_are_not_stale` covers
    /// this crate's table on exactly the terms it covers base's.
    fn declared_fields(&self) -> &'static [FieldDesc] {
        dbd_generated::MCA_FIELDS
    }

    fn declared_noaccess_fields(&self) -> &'static [&'static str] {
        dbd_generated::MCA_NOACCESS
    }

    /// C `cvt_dbaddr` (`mcaRecord.c:846-863`) ends with
    /// `paddr->no_elements = pmca->nmax;` — outside the `fieldIndex` branch
    /// that picks `bptr` or `pbg`, so the channel capacity is `NMAX` for both
    /// `VAL` and `BG`. Those two are exactly the `special(SPC_DBADDR)` fields
    /// (`mcaRecord.dbd`), which is what routes a channel through `cvt_dbaddr`
    /// at all; every other field keeps its value's own count.
    ///
    /// Without this the channel was sized from the *served* count, which
    /// `get_array_info` floors at 1 — so a client connecting before the first
    /// acquisition fixed its buffer at one channel and never saw the spectrum
    /// widen, because `ca_element_count` is settled at create-channel time.
    ///
    /// `McaRecord::capacity` is what sizes the buffers, so the advertised
    /// capacity cannot drift from the allocation behind it.
    fn dbaddr_capacity(&self, _field: &str) -> Option<u32> {
        Some(self.capacity() as u32)
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        if let Some((i, member)) = roi_field(name) {
            let r = &self.roi[i];
            return Some(match member {
                "LO" => EpicsValue::Long(r.lo),
                "HI" => EpicsValue::Long(r.hi),
                "BG" => EpicsValue::Short(r.nbg),
                "IP" => EpicsValue::Enum(r.is_preset),
                "" => EpicsValue::Double(r.sum),
                "N" => EpicsValue::Double(r.net),
                "P" => EpicsValue::Double(r.preset),
                "NM" => EpicsValue::String(r.name.clone()),
                _ => return None,
            });
        }
        Some(match name {
            "VERS" => EpicsValue::Double(self.vers),
            "VAL" => self.served_array(&self.val),
            "BG" => self.served_array(&self.bg),
            "HOPR" => EpicsValue::Double(self.hopr),
            "LOPR" => EpicsValue::Double(self.lopr),
            "NMAX" => EpicsValue::Long(self.nmax),
            "NORD" => EpicsValue::Long(self.nord),
            "PREC" => EpicsValue::Short(self.prec),
            "FTVL" => EpicsValue::Enum(self.ftvl.index() as u16),
            "STRT" => EpicsValue::Enum(self.strt),
            "ERST" => EpicsValue::Enum(self.erst),
            "STOP" => EpicsValue::Enum(self.stop),
            "ACQG" => EpicsValue::Enum(self.acqg),
            "READ" => EpicsValue::Enum(self.read),
            "RDNG" => EpicsValue::Enum(self.rdng),
            "RDNS" => EpicsValue::Enum(self.rdns),
            "ERAS" => EpicsValue::Enum(self.eras),
            "CHAS" => EpicsValue::Enum(self.chas),
            "NUSE" => EpicsValue::Long(self.nuse),
            "SEQ" => EpicsValue::Long(self.seq),
            "DWEL" => EpicsValue::Double(self.dwel),
            "PSCL" => EpicsValue::Long(self.pscl),
            "PRTM" => EpicsValue::Double(self.prtm),
            "PLTM" => EpicsValue::Double(self.pltm),
            "PCT" => EpicsValue::Double(self.pct),
            "PCTL" => EpicsValue::Long(self.pctl),
            "PCTH" => EpicsValue::Long(self.pcth),
            "PSWP" => EpicsValue::Long(self.pswp),
            "MODE" => EpicsValue::Enum(self.mode),
            "CALO" => EpicsValue::Double(self.calo),
            "CALS" => EpicsValue::Double(self.cals),
            "CALQ" => EpicsValue::Double(self.calq),
            "EGU" => EpicsValue::String(self.egu.clone()),
            "TTH" => EpicsValue::Double(self.tth),
            "ERTM" => EpicsValue::Double(self.ertm),
            "ELTM" => EpicsValue::Double(self.eltm),
            "DTIM" => EpicsValue::Double(self.dtim),
            "IDTIM" => EpicsValue::Double(self.idtim),
            "STIM" => EpicsValue::String(self.stim.clone()),
            "RTIM" => EpicsValue::Double(self.rtim),
            "ACT" => EpicsValue::Long(self.act),
            "NACK" => EpicsValue::Short(self.nack),
            "HIHI" => EpicsValue::Double(self.hihi),
            "LOLO" => EpicsValue::Double(self.lolo),
            "HIGH" => EpicsValue::Double(self.high),
            "LOW" => EpicsValue::Double(self.low),
            "HHSV" => EpicsValue::Enum(self.hhsv),
            "LLSV" => EpicsValue::Enum(self.llsv),
            "HSV" => EpicsValue::Enum(self.hsv),
            "LSV" => EpicsValue::Enum(self.lsv),
            "HYST" => EpicsValue::Double(self.hyst),
            "LALM" => EpicsValue::Double(self.lalm),
            "SIMM" => EpicsValue::Enum(self.simm),
            "SIMS" => EpicsValue::Enum(self.sims),
            "MMAP" => EpicsValue::ULong(self.mmap),
            "RMAP" => EpicsValue::ULong(self.rmap),
            "NEWV" => EpicsValue::ULong(self.newv),
            "NEWR" => EpicsValue::ULong(self.newr),
            _ => return None,
        })
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if let Some((i, member)) = roi_field(name) {
            match member {
                "LO" => self.roi[i].lo = as_i32(name, &value)?,
                "HI" => self.roi[i].hi = as_i32(name, &value)?,
                "BG" => self.roi[i].nbg = as_i16(name, &value)?,
                "IP" => self.roi[i].is_preset = as_u16(name, &value)?,
                "P" => self.roi[i].preset = as_f64(name, &value)?,
                "NM" => self.roi[i].name = as_string(&value),
                // `R{i}` and `R{i}N` are `special(SPC_NOMOD)`: the record
                // computes them. The framework's put gate refuses a client write
                // from the declaration; an internal put lands here.
                "" => self.roi[i].sum = as_f64(name, &value)?,
                "N" => self.roi[i].net = as_f64(name, &value)?,
                _ => return Err(CaError::FieldNotFound(name.to_string())),
            }
            return Ok(());
        }
        match name {
            "VERS" => self.vers = as_f64(name, &value)?,
            "VAL" => self.land_spectrum(value)?,
            "BG" => self.land_array_field(value, |r| &mut r.bg)?,
            "HOPR" => self.hopr = as_f64(name, &value)?,
            "LOPR" => self.lopr = as_f64(name, &value)?,
            "NMAX" => self.nmax = as_i32(name, &value)?,
            "NORD" => self.nord = as_i32(name, &value)?,
            "PREC" => self.prec = as_i16(name, &value)?,
            "FTVL" => {
                let index = as_i16(name, &value)?;
                self.ftvl = Ftype::from_index(index)
                    .ok_or_else(|| CaError::TypeMismatch(format!("FTVL: no menuFtype {index}")))?;
            }
            "STRT" => self.strt = as_u16(name, &value)?,
            "ERST" => self.erst = as_u16(name, &value)?,
            "STOP" => self.stop = as_u16(name, &value)?,
            "ACQG" => self.acqg = as_u16(name, &value)?,
            "READ" => self.read = as_u16(name, &value)?,
            "RDNG" => self.rdng = as_u16(name, &value)?,
            "RDNS" => self.rdns = as_u16(name, &value)?,
            "ERAS" => self.eras = as_u16(name, &value)?,
            "CHAS" => self.chas = as_u16(name, &value)?,
            // C clamps NUSE to NMAX at `init_record` (:439), in `special`'s
            // process arm (:535) and nowhere else — so a `caput NUSE 9999`
            // reads back as 9999 until the record next processes. Clamping at
            // the WRITE makes "NUSE <= NMAX" hold by construction instead of by
            // a later visit, which is the same invariant C is reaching for.
            "NUSE" => self.nuse = as_i32(name, &value)?.min(self.nmax),
            "SEQ" => self.seq = as_i32(name, &value)?,
            "DWEL" => self.dwel = as_f64(name, &value)?,
            "PSCL" => self.pscl = as_i32(name, &value)?,
            "PRTM" => self.prtm = as_f64(name, &value)?,
            "PLTM" => self.pltm = as_f64(name, &value)?,
            "PCT" => self.pct = as_f64(name, &value)?,
            "PCTL" => self.pctl = as_i32(name, &value)?,
            "PCTH" => self.pcth = as_i32(name, &value)?,
            "PSWP" => self.pswp = as_i32(name, &value)?,
            "MODE" => self.mode = as_u16(name, &value)?,
            "CALO" => self.calo = as_f64(name, &value)?,
            "CALS" => self.cals = as_f64(name, &value)?,
            "CALQ" => self.calq = as_f64(name, &value)?,
            "EGU" => self.egu = as_string(&value),
            "TTH" => self.tth = as_f64(name, &value)?,
            "ERTM" => self.ertm = as_f64(name, &value)?,
            "ELTM" => self.eltm = as_f64(name, &value)?,
            "DTIM" => self.dtim = as_f64(name, &value)?,
            "IDTIM" => self.idtim = as_f64(name, &value)?,
            "STIM" => self.stim = as_string(&value),
            "RTIM" => self.rtim = as_f64(name, &value)?,
            "ACT" => self.act = as_i32(name, &value)?,
            "NACK" => self.nack = as_i16(name, &value)?,
            "HIHI" => self.hihi = as_f64(name, &value)?,
            "LOLO" => self.lolo = as_f64(name, &value)?,
            "HIGH" => self.high = as_f64(name, &value)?,
            "LOW" => self.low = as_f64(name, &value)?,
            "HHSV" => self.hhsv = as_u16(name, &value)?,
            "LLSV" => self.llsv = as_u16(name, &value)?,
            "HSV" => self.hsv = as_u16(name, &value)?,
            "LSV" => self.lsv = as_u16(name, &value)?,
            "HYST" => self.hyst = as_f64(name, &value)?,
            "LALM" => self.lalm = as_f64(name, &value)?,
            "SIMM" => self.simm = as_u16(name, &value)?,
            "SIMS" => self.sims = as_u16(name, &value)?,
            "MMAP" => self.mmap = as_u32(name, &value)?,
            "RMAP" => self.rmap = as_u32(name, &value)?,
            "NEWV" => self.newv = as_u32(name, &value)?,
            "NEWR" => self.newr = as_u32(name, &value)?,
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        Ok(())
    }

    /// C `init_record` (`mcaRecord.c:416-489`).
    ///
    /// Pass 0 allocates the two `NMAX`-deep buffers and stamps `VERS`; pass 1
    /// clamps `NUSE`. C's pass 1 also sends every setup field to device support
    /// so the hardware starts in sync with the record — that is the device
    /// support's business and lands with it.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.vers = VERSION;
            if self.nmax <= 0 {
                self.nmax = 1;
            }
            self.val = self.zeroed_buffer();
            self.bg = self.zeroed_buffer();
            self.nord = 0;
            return Ok(());
        }
        if self.nuse > self.nmax {
            self.nuse = self.nmax;
        }
        Ok(())
    }

    /// The `pp(TRUE)` set, read off the SAME generated table the field
    /// declaration comes from.
    ///
    /// The framework's default consults a central table keyed by record type,
    /// which base builds from base's own `.dbd` set and which therefore does not
    /// know `mca` — it would answer `&[]`, and a `caput STRT Acquire` would
    /// never process the record. The declaration already carries `pp`, so this
    /// answers from it rather than from a second list that could disagree with
    /// the `.dbd`.
    fn process_passive_fields(&self) -> &'static [&'static str] {
        static PP: OnceLock<Vec<&'static str>> = OnceLock::new();
        PP.get_or_init(|| {
            dbd_generated::MCA_FIELDS
                .iter()
                .filter(|f| f.pp)
                .map(|f| f.name)
                .collect()
        })
    }

    /// C `special` (`mcaRecord.c:1045-1094`) — a put to a setup field records
    /// WHICH field moved, so the next `process()` sends it (and only it) to the
    /// device; a put to an ROI control field marks that region for
    /// recomputation.
    ///
    /// **Tier-2 deviation — C's ROI range test runs off the end of the ROI
    /// block.** C tests `fieldIndex >= mcaRecordR0LO && fieldIndex <=
    /// mcaRecordR0IP + NUM_ROI*FIELDS_PER_ROI` (`:1069-1070`). The ROI control
    /// block is `R0LO .. R31IP`, i.e. `R0LO + 127`, so the upper bound
    /// (`R0IP + 128` = `R0LO + 131`) reaches four fields PAST it, into whatever
    /// the `.dbd` declares next — and each of those four is then divided into an
    /// "ROI index" of 32..33 and used to shift `M_R0 << i` by up to 33, which is
    /// undefined behaviour for a 32-bit `unsigned long`. The port addresses a
    /// region by NAME through `roi_field`, whose suffix set is closed, so a
    /// field that is not an ROI control field cannot be one.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        if let Some((i, member)) = roi_field(field) {
            if matches!(member, "LO" | "HI" | "BG" | "IP") {
                self.newr |= 1u32 << i;
            }
            return Ok(());
        }
        let bit = match field {
            "ERAS" => cycle::NEWV_ERAS,
            "ERST" => cycle::NEWV_ERST,
            "CHAS" => cycle::NEWV_CHAS,
            "NUSE" => cycle::NEWV_NUSE,
            "SEQ" => cycle::NEWV_SEQ,
            "DWEL" => cycle::NEWV_DWEL,
            "PSCL" => cycle::NEWV_PSCL,
            "PRTM" => cycle::NEWV_PRTM,
            "PLTM" => cycle::NEWV_PLTM,
            "PCT" => cycle::NEWV_PCT,
            "PCTL" => cycle::NEWV_PCTL,
            "PCTH" => cycle::NEWV_PCTH,
            "PSWP" => cycle::NEWV_PSWP,
            "MODE" => cycle::NEWV_MODE,
            _ => return Ok(()),
        };
        self.newv |= bit;
        // A channel-count change is the one setup change C forces a read after:
        // the spectrum a client is holding is now the wrong width. C's comment
        // (`:1082-1089`) is explicit that SEQ and ERAS deliberately do NOT force
        // one, because both are on the hot path of a scan.
        if bit == cycle::NEWV_NUSE {
            self.read = 1;
        }
        Ok(())
    }

    /// C `process` `:792-841` — the tail of the cycle, after device support has
    /// sent the commands ([`McaRecord::take_device_requests`]), read the status
    /// ([`McaRecord::apply_status`]) and landed any new spectrum
    /// ([`McaRecord::land_spectrum_read`]).
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let mut actions = Vec::new();

        // `NEWR` gates whether the regions are summed AT ALL (C `:792-794`), not
        // which region is visited: `sum_ROIs` recomputes all 32 and clears the
        // whole map.
        if self.newr != 0 && self.sum_rois().preset_reached {
            // C sends the stop straight from `process` (`:797-800`). The port's
            // records hold no driver handle, so it goes out as the framework's
            // device command — executed after `process()` returns, which is where
            // C's own send lands relative to the ACQG commit below anyway.
            actions.push(ProcessAction::DeviceCommand {
                command: commands::STOP_ACQUIRE,
                args: Vec::new(),
            });
        }

        // NOW the record admits acquisition has stopped: the final spectrum is
        // in, its ROI sums are computed, and both will be posted by this same
        // cycle's monitor pass (C `:802-806` and the comment at `:736-738`).
        self.acqg = u16::from(self.status.acquiring);

        if self.acqg == 0 {
            self.stim = PvString::from(cycle::format_stim(SystemTime::now()).as_str());
        }

        Ok(ProcessOutcome::complete_with(actions))
    }

    /// C `mcaAlarm` (`:962-1003`) — the analog ladder runs on `IDTIM`, the
    /// instantaneous dead time, not on `VAL`.
    fn check_alarms(&mut self, common: &mut CommonFields) {
        self.check_dead_time_alarms(common);
    }

    /// C `recGblFwdLink` is called ONLY when acquisition has finished
    /// (`mcaRecord.c:825-830`): an mca's forward link means "the spectrum is
    /// complete", not "the record was polled". A 10 Hz status poll during a
    /// 60-second acquisition would otherwise process the whole downstream chain
    /// 600 times.
    fn should_fire_forward_link(&self) -> bool {
        self.acqg == 0
    }

    /// The three RSET metadata slots C answers per field.
    ///
    /// - `get_units` (`:884-890`) copies `EGU` for EVERY field, which is what the
    ///   framework's record-level metadata already does — no override needed.
    /// - `get_precision` (`:892-907`) serves 6 for the calibration fields
    ///   (`CALO`/`CALS`/`CALQ`/`TTH`) and `PREC` for everything else.
    /// - `get_graphic_double` (`:909-928`) and `get_alarm_double` (`:948-960`)
    ///   put `DTIM`/`IDTIM` on a 0..100 percent scale, with the record's alarm
    ///   limits — the two fields whose limits are meaningful, since `HIHI`..`LOLO`
    ///   alarm on `IDTIM`.
    ///
    /// C's `fieldIndex == mcaRecordBPTR` arms in all three are dead code: `BPTR`
    /// is `DBF_NOACCESS` and `dbGetFieldIndex` returns the index of the field the
    /// client NAMED (`VAL`), not the one `cvt_dbaddr` re-pointed `pfield` at. No
    /// client can reach them, so they are not ported.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        match field {
            "CALO" | "CALS" | "CALQ" | "TTH" => Some(FieldMetadataOverride {
                precision: Some(6),
                ..Default::default()
            }),
            "DTIM" | "IDTIM" => Some(FieldMetadataOverride {
                disp_limits: Some((100.0, 0.0)),
                alarm_limits: Some((self.hihi, self.high, self.low, self.lolo)),
                ..Default::default()
            }),
            _ => None,
        }
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

/// The `DeviceCommand` names the record uses to reach its device support — the
/// one place the string is written, so a driver cannot listen for a name the
/// record never sends.
pub mod commands {
    /// C `mcaStopAcquire`, sent from `process()` when an armed ROI preset is
    /// reached.
    pub const STOP_ACQUIRE: &str = "mcaStopAcquire";
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::record::FieldDeclaration;

    /// The declaration a consumer sees is the GENERATED one — the invariant the
    /// blanket `impl FieldDeclaration for R` exists to hold. If this crate ever
    /// grows a hand-written table, this test is what notices.
    #[test]
    fn the_field_list_is_the_generated_table() {
        let rec = McaRecord::default();
        let table = rec.field_list();
        assert!(std::ptr::eq(
            table,
            dbd_generated::record_fields("mca").unwrap()
        ));
    }

    /// The `.dbd` declares 320 fields; `dbCommon` is declared once, by base, so
    /// the record-own table carries the rest. The three `DBF_NOACCESS` internals
    /// (`BPTR`, `PBG`, `PSTATUS`) are C pointers with no CA representation and
    /// are dropped; `VAL` and `BG` are `special(SPC_DBADDR)` and are kept.
    ///
    /// Every declared field must have an owner. For all but three that owner is
    /// the record: it serves the field from its own state. The exceptions are
    /// the LINK fields — `INP` (the device-support link), `SIOL` and `SIML` (the
    /// simulation links) — which the record DECLARES but does not DRIVE: the
    /// framework parses and arms them, exactly as `Record::implements_field`'s
    /// contract says ("what is this field?" and "who owns it?" are different
    /// questions). A record that answered `get_field("INP")` from its own state
    /// would be the second owner of a link the framework is already driving.
    #[test]
    fn every_own_field_of_the_dbd_has_an_owner() {
        let rec = McaRecord::default();
        let framework_owned = ["INP", "SIOL", "SIML"];
        let mut unowned: Vec<&str> = Vec::new();
        for f in rec.field_list() {
            if rec.get_field(f.name).is_none() && !framework_owned.contains(&f.name) {
                unowned.push(f.name);
            }
        }
        assert!(
            unowned.is_empty(),
            "declared by mcaRecord.dbd, served by nobody: {unowned:?}"
        );
        // The other half of the claim: the framework-owned set is exactly the
        // link fields, so this list cannot quietly grow into an excuse.
        for name in framework_owned {
            let desc = rec
                .field_list()
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("{name} is not declared by mcaRecord.dbd"));
            assert_eq!(
                desc.dbf_type,
                DbFieldType::String,
                "{name} is not a link field"
            );
            assert!(rec.get_field(name).is_none(), "{name} has two owners");
        }
    }

    /// An `mca` with `NMAX` channels, initialised the way `iocInit` initialises
    /// it, holding `spectrum` as its `NORD` valid channels.
    fn loaded(nmax: i32, spectrum: &[i32]) -> McaRecord {
        let mut rec = McaRecord {
            nmax,
            nuse: nmax,
            ..Default::default()
        };
        rec.init_record(0).unwrap();
        rec.init_record(1).unwrap();
        rec.put_field("VAL", EpicsValue::LongArray(spectrum.to_vec()))
            .unwrap();
        rec
    }

    /// The `CommonFields` an mca reaches `check_alarms` with: the framework sets
    /// `udf` from `value_is_undefined()` immediately before the call, and an
    /// mca's `VAL` is an array, so a processed mca is always defined. The
    /// `Default` is `udf = true` (the state a record is loaded in), which no
    /// record reaches this hook in.
    fn defined() -> CommonFields {
        CommonFields {
            udf: 0,
            ..Default::default()
        }
    }

    fn status(acquiring: bool) -> McaStatus {
        McaStatus {
            acquiring,
            ..Default::default()
        }
    }

    // ---- the acquisition state machine (C `process`) ----

    /// C `:640-644`. The start command and the status read happen in the SAME
    /// cycle, and a device fast enough to have finished by the time the status is
    /// read would report `acquiring = 0` on that first read. If `ACQG` were taken
    /// from that status, the record would never see a 1 -> 0 edge and would never
    /// read the spectrum. So the start FORCES `ACQG` to 1, before any status.
    #[test]
    fn start_forces_acqg_before_the_first_status_is_read() {
        let mut rec = loaded(8, &[0; 8]);
        rec.strt = 1;
        let cmds = rec.take_device_requests();
        assert_eq!(cmds, vec![McaCommand::StartAcquire]);
        assert_eq!(rec.acqg, 1);

        // The device was faster than the poll: it is already done.
        assert!(rec.apply_status(status(false)), "the final read is forced");
        assert_eq!(rec.acqg, 1, "not committed until the spectrum is in");
        rec.process().unwrap();
        assert_eq!(rec.acqg, 0);
    }

    /// The other side of that edge: a status of "not acquiring" when the record
    /// was ALREADY not acquiring forces no read. Only the 1 -> 0 transition does
    /// (C `:734-742`).
    #[test]
    fn a_read_is_forced_only_on_the_acquiring_edge() {
        let mut rec = loaded(8, &[0; 8]);
        assert_eq!(rec.acqg, 0);
        assert!(!rec.apply_status(status(false)));
        assert_eq!(rec.read, 0);
    }

    /// C `:802-806` and its comment at `:736-738`: a client must not learn that
    /// acquisition stopped before the last spectrum and its ROI sums are posted.
    /// So `ACQG` is committed by `process()`, never by the status read.
    #[test]
    fn acqg_is_committed_after_the_data_not_with_the_status() {
        let mut rec = loaded(8, &[0; 8]);
        rec.strt = 1;
        rec.take_device_requests();
        rec.apply_status(status(true));
        rec.process().unwrap();
        assert_eq!(rec.acqg, 1);

        rec.apply_status(status(false));
        assert_eq!(rec.acqg, 1, "still 1 — the spectrum has not landed yet");
        rec.land_spectrum_read(EpicsValue::LongArray(vec![1, 2, 3, 4, 5, 6, 7, 8]))
            .unwrap();
        assert_eq!(rec.acqg, 1);
        rec.process().unwrap();
        assert_eq!(rec.acqg, 0, "and only now");
    }

    /// C fires `recGblFwdLink` only when acquisition has finished (`:825-830`):
    /// an mca's forward link means "the spectrum is complete".
    #[test]
    fn the_forward_link_fires_only_when_acquisition_is_over() {
        let mut rec = loaded(8, &[0; 8]);
        rec.strt = 1;
        rec.take_device_requests();
        rec.apply_status(status(true));
        rec.process().unwrap();
        assert!(!rec.should_fire_forward_link());

        rec.apply_status(status(false));
        rec.process().unwrap();
        assert!(rec.should_fire_forward_link());
    }

    /// `NEWV` is consumed, not merely read: the setup commands go out once per
    /// put, not once per process cycle for the rest of time.
    #[test]
    fn the_setup_flags_are_consumed_by_the_cycle_that_sends_them() {
        let mut rec = loaded(8, &[0; 8]);
        rec.put_field("DWEL", EpicsValue::Double(0.25)).unwrap();
        rec.special("DWEL", true).unwrap();
        assert_eq!(
            rec.take_device_requests(),
            vec![McaCommand::DwellTime(0.25)]
        );
        assert_eq!(rec.newv, 0);
        assert!(rec.take_device_requests().is_empty());
    }

    /// C `:636` — "Turn acquisition on or off. Do this before reading device
    /// status." Start and stop are the LAST commands of the pre-status block, so
    /// the status read that follows sees them; the setup fields precede them, so
    /// acquisition starts with the settings the client just wrote.
    #[test]
    fn start_is_sent_after_every_setup_field() {
        let mut rec = loaded(8, &[0; 8]);
        rec.put_field("PRTM", EpicsValue::Double(10.0)).unwrap();
        rec.special("PRTM", true).unwrap();
        rec.put_field("ERST", EpicsValue::Enum(1)).unwrap();
        rec.special("ERST", true).unwrap();

        assert_eq!(
            rec.take_device_requests(),
            vec![
                McaCommand::PresetRealTime(10.0),
                McaCommand::Erase,
                McaCommand::StartAcquire,
            ]
        );
    }

    /// C `:614` erases `NUSE` channels, not `NMAX`: channels the client is not
    /// using keep whatever they held.
    #[test]
    fn erase_zeroes_the_channels_in_use_and_no_others() {
        let mut rec = loaded(8, &[1, 2, 3, 4, 5, 6, 7, 8]);
        rec.put_field("NUSE", EpicsValue::Long(4)).unwrap();
        rec.put_field("ERAS", EpicsValue::Enum(1)).unwrap();
        rec.special("ERAS", true).unwrap();
        rec.take_device_requests();

        assert_eq!(rec.nord, 0, "NORD is reset — the data are gone");
        assert_eq!(rec.eras, 0);
        assert_eq!(rec.newr, cycle::NEWR_ALL, "every region must be re-summed");
        // NORD is 0, so the served array is empty; the BUFFER is what the erase
        // touched, and only its first NUSE channels.
        rec.nord = 8;
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::LongArray(vec![0, 0, 0, 0, 5, 6, 7, 8]))
        );
    }

    /// A channel-count change is the one setup change C forces a read after
    /// (`:1090-1093`): the spectrum the client holds is now the wrong width.
    /// `SEQ` and `ERAS` deliberately do not (`:1082-1089`).
    #[test]
    fn only_a_channel_count_change_forces_a_read() {
        let mut rec = loaded(8, &[0; 8]);
        rec.special("SEQ", true).unwrap();
        assert_eq!(rec.read, 0);
        rec.special("ERAS", true).unwrap();
        assert_eq!(rec.read, 0);
        rec.special("NUSE", true).unwrap();
        assert_eq!(rec.read, 1);
    }

    // ---- dead time (C `:710-732`) ----

    /// The deviation this port makes from C, at the boundary that exposes it: a
    /// detector so dead that its live-time clock has stopped. Real time advances
    /// by 1 s, live time by 0 — the instantaneous dead time is 100%.
    ///
    /// C reports 0%, because its `eltp` ("previous live time") is a local that is
    /// only assigned in the branch where ELTM CHANGED, so on this cycle it is
    /// still its initialiser, 0. See [`cycle`]'s `update_dead_time`.
    #[test]
    fn instantaneous_dead_time_reports_a_fully_dead_detector() {
        let mut rec = loaded(8, &[0; 8]);
        rec.strt = 1;
        rec.take_device_requests();
        rec.apply_status(McaStatus {
            acquiring: true,
            elapsed_real: 1.0,
            elapsed_live: 1.0,
            ..Default::default()
        });
        assert_eq!(rec.idtim, 0.0);

        // Second poll: 1 s of real time, no live time at all.
        rec.apply_status(McaStatus {
            acquiring: true,
            elapsed_real: 2.0,
            elapsed_live: 1.0,
            ..Default::default()
        });
        assert_eq!(rec.idtim, 100.0);
        assert_eq!(rec.dtim, 50.0, "and the AVERAGE is 50%");
    }

    /// C `:727-729`: with acquisition off there is no interval to be
    /// instantaneous over, so IDTIM reports the average.
    #[test]
    fn instantaneous_dead_time_falls_back_to_the_average_when_idle() {
        let mut rec = loaded(8, &[0; 8]);
        rec.apply_status(McaStatus {
            acquiring: false,
            elapsed_real: 4.0,
            elapsed_live: 3.0,
            ..Default::default()
        });
        assert_eq!(rec.dtim, 25.0);
        assert_eq!(rec.idtim, 25.0);
    }

    // ---- regions of interest (C `sum_ROIs` / `PROCESS_ROI`) ----

    /// The region is inclusive of both ends, and `HI` is clipped to the last
    /// channel actually read (C `if (hi > max) hi = max`, `max = nord-1`).
    #[test]
    fn a_region_spans_lo_through_hi_inclusive_and_hi_clips_to_nord() {
        let mut rec = loaded(8, &[0, 1, 2, 3, 4, 5, 6, 7]);
        rec.roi[0] = Roi {
            lo: 2,
            hi: 4,
            nbg: -1,
            ..Default::default()
        };
        rec.roi[1] = Roi {
            lo: 2,
            hi: 100,
            nbg: -1,
            ..Default::default()
        };
        rec.newr = cycle::NEWR_ALL;
        rec.process().unwrap();

        assert_eq!(rec.roi[0].sum, 9.0, "channels 2,3,4");
        assert_eq!(rec.roi[1].sum, 27.0, "channels 2..7 — HI clipped to NORD-1");
        assert_eq!(rec.newr, 0, "the marks are consumed");
    }

    /// A region is enabled iff `LO >= 0 && HI >= LO` (C `:371`). Both `.dbd`
    /// initials are `-1`, so an untouched region is disabled and sums to zero.
    #[test]
    fn a_disabled_region_sums_to_zero() {
        let mut rec = loaded(8, &[1; 8]);
        rec.roi[0] = Roi {
            lo: -1,
            hi: 4,
            ..Default::default()
        };
        rec.roi[1] = Roi {
            lo: 5,
            hi: 4,
            ..Default::default()
        };
        rec.newr = cycle::NEWR_ALL;
        rec.process().unwrap();
        assert_eq!(rec.roi[0].sum, 0.0);
        assert_eq!(rec.roi[1].sum, 0.0);
        assert_eq!(rec.roi[2].sum, 0.0, "the untouched default");
    }

    /// `R{i}BG < 0` means "no background" (C `:373`), so the net counts ARE the
    /// total counts.
    #[test]
    fn a_region_with_no_background_has_net_equal_to_sum() {
        let mut rec = loaded(8, &[0, 1, 2, 3, 4, 5, 6, 7]);
        rec.roi[0] = Roi {
            lo: 1,
            hi: 3,
            nbg: -1,
            ..Default::default()
        };
        rec.newr = cycle::NEWR_ALL;
        rec.process().unwrap();
        assert_eq!(rec.roi[0].sum, 6.0);
        assert_eq!(rec.roi[0].net, 6.0);
    }

    /// The background is interpolated linearly between the two end averages, and
    /// under a linear spectrum it lands exactly on the data — net counts zero.
    /// `R{i}BG == 0` is the boundary: a one-channel window either side.
    #[test]
    fn the_background_is_interpolated_between_the_region_ends() {
        let mut rec = loaded(8, &[0, 1, 2, 3, 4, 5, 6, 7]);
        rec.roi[0] = Roi {
            lo: 2,
            hi: 4,
            nbg: 0,
            ..Default::default()
        };
        rec.newr = cycle::NEWR_ALL;
        rec.process().unwrap();
        assert_eq!(rec.roi[0].sum, 9.0);
        assert_eq!(rec.roi[0].net, 0.0, "a straight line has no peak over it");
    }

    /// The background curve is stored in the SPECTRUM's element type, and the net
    /// counts are computed from the value that was stored — C's `net += *p - *pb`
    /// reads `*pb` back through a `DATA_TYPE *` (`:389-394`). Under a `LONG`
    /// spectrum a background of 10/3 is 3, not 3.333, and the net is 7, not
    /// 6.667.
    #[test]
    fn the_background_is_truncated_to_the_spectrum_element_type() {
        let mut rec = loaded(8, &[0, 0, 0, 10, 0, 0, 0, 0]);
        rec.roi[0] = Roi {
            lo: 3,
            hi: 3,
            nbg: 1,
            ..Default::default()
        };
        rec.newr = cycle::NEWR_ALL;
        rec.process().unwrap();
        assert_eq!(rec.roi[0].sum, 10.0);
        assert_eq!(rec.roi[0].net, 7.0);
    }

    /// The whole ROI machinery against the C IOC built for this port (record +
    /// `devMCA_soft`, EPICS base R7.0.10.1-DEV). The numbers below are read off
    /// that IOC, not derived here:
    ///
    /// ```text
    /// record(mca,"TEST:mca1") { field(NMAX,"16") field(NUSE,"16") field(FTVL,"LONG")
    ///                           field(R0LO,"2") field(R0HI,"5") field(R0BG,"1") }
    /// caput -a TEST:mca1.VAL 16  0 0 10 20 30 20 10 0 0 0 0 0 0 0 0 0
    /// caput TEST:mca1.READ 1
    ///   TEST:mca1.R0    80
    ///   TEST:mca1.R0N   21
    ///   TEST:mca1.BG    16  0 0 30 13 16 30 0 0 0 0 0 0 0 0 0 0
    /// ```
    ///
    /// `R0N` is the load-bearing number. The background is 10 at channel 2 and 20
    /// at channel 5, interpolated across the region as 10, 13.33, 16.67, 20 — and
    /// the net counts come out 21, not 20, because C stores that background
    /// through an `epicsInt32 *` and then subtracts what it STORED. `BG`'s 13 and
    /// 16 are the same truncation, visible on the wire. The 30s at channels 2 and
    /// 5 are the region's end markers, planted at the spectrum's peak.
    #[test]
    fn the_c_ioc_is_reproduced_channel_for_channel() {
        let mut rec = loaded(16, &[0, 0, 10, 20, 30, 20, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        rec.roi[0] = Roi {
            lo: 2,
            hi: 5,
            nbg: 1,
            ..Default::default()
        };
        rec.newr = cycle::NEWR_ALL;
        rec.process().unwrap();

        assert_eq!(rec.roi[0].sum, 80.0, "R0");
        assert_eq!(rec.roi[0].net, 21.0, "R0N");
        assert_eq!(
            rec.get_field("BG"),
            Some(EpicsValue::LongArray(vec![
                0, 0, 30, 13, 16, 30, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]))
        );
    }

    /// C `:398` — `net >= preset` fires, and the record stops acquisition. The
    /// boundary is the equality.
    #[test]
    fn an_armed_region_stops_acquisition_when_net_reaches_its_preset() {
        let arm = |preset: f64| {
            let mut rec = loaded(8, &[0, 1, 2, 3, 4, 5, 6, 7]);
            rec.roi[0] = Roi {
                lo: 1,
                hi: 3,
                nbg: -1,
                is_preset: 1,
                preset,
                ..Default::default()
            };
            rec.newr = cycle::NEWR_ALL;
            rec.process().unwrap().actions
        };
        let stop = ProcessAction::DeviceCommand {
            command: commands::STOP_ACQUIRE,
            args: Vec::new(),
        };
        // net == 6.
        assert_eq!(arm(6.0), vec![stop.clone()], "net == preset stops");
        assert!(arm(6.5).is_empty(), "net < preset does not");
    }

    /// A region that is not armed (`R{i}IP == No`) never stops acquisition,
    /// however far past its preset it runs.
    #[test]
    fn an_unarmed_region_never_stops_acquisition() {
        let mut rec = loaded(8, &[0, 1, 2, 3, 4, 5, 6, 7]);
        rec.roi[0] = Roi {
            lo: 1,
            hi: 3,
            nbg: -1,
            is_preset: 0,
            preset: 1.0,
            ..Default::default()
        };
        rec.newr = cycle::NEWR_ALL;
        assert!(rec.process().unwrap().actions.is_empty());
    }

    // ---- alarms (C `mcaAlarm`) ----

    /// The ladder is on IDTIM, not on VAL — and `idtim >= hihi` is the boundary.
    #[test]
    fn the_alarm_ladder_runs_on_the_instantaneous_dead_time() {
        use epics_base_rs::server::recgbl::alarm_status;
        use epics_base_rs::server::record::AlarmSeverity;

        let fire = |idtim: f64| {
            let mut rec = McaRecord {
                idtim,
                hihi: 50.0,
                hhsv: AlarmSeverity::Major as u16,
                ..Default::default()
            };
            let mut common = defined();
            rec.check_alarms(&mut common);
            (common.nsev, common.nsta, rec.lalm)
        };
        assert_eq!(
            fire(50.0),
            (AlarmSeverity::Major, alarm_status::HIHI_ALARM, 50.0),
            "at the limit"
        );
        assert_eq!(
            fire(49.9),
            (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM, 49.9),
            "below it, and LALM tracks the value"
        );
    }

    /// `HYST` latches a fired limit until IDTIM has left the band by `HYST` —
    /// so a value chattering across the limit does not chatter the alarm.
    #[test]
    fn hysteresis_holds_a_fired_alarm_until_the_value_leaves_the_band() {
        use epics_base_rs::server::record::AlarmSeverity;

        let mut rec = McaRecord {
            idtim: 50.0,
            hihi: 50.0,
            hhsv: AlarmSeverity::Major as u16,
            hyst: 5.0,
            ..Default::default()
        };
        rec.check_alarms(&mut defined());
        assert_eq!(rec.lalm, 50.0, "the limit is latched");

        // Inside the hysteresis band: still in alarm.
        rec.idtim = 46.0;
        let mut common = defined();
        rec.check_alarms(&mut common);
        assert_eq!(common.nsev, AlarmSeverity::Major);

        // Out of it by more than HYST: the alarm clears and the latch releases.
        rec.idtim = 44.0;
        let mut common = defined();
        rec.check_alarms(&mut common);
        assert_eq!(common.nsev, AlarmSeverity::NoAlarm);
        assert_eq!(rec.lalm, 44.0);
    }

    /// C `mcaAlarm` returns before the ladder when the record is undefined
    /// (`:968-971`); the framework raises UDF_ALARM itself.
    #[test]
    fn an_undefined_record_reports_udf_and_not_a_dead_time_alarm() {
        use epics_base_rs::server::record::AlarmSeverity;

        let mut rec = McaRecord {
            idtim: 99.0,
            hihi: 50.0,
            hhsv: AlarmSeverity::Major as u16,
            ..Default::default()
        };
        let mut common = CommonFields {
            udf: 1,
            ..Default::default()
        };
        rec.check_alarms(&mut common);
        assert_eq!(common.nsev, AlarmSeverity::NoAlarm);
        assert_eq!(rec.lalm, 0.0, "and the latch is not moved");
    }

    // ---- the declaration drives the machinery ----

    /// The `pp(TRUE)` gate is answered from the generated table, so a put to
    /// `STRT` processes the record and a put to `PRTM` does not — exactly as the
    /// `.dbd` declares.
    #[test]
    fn the_process_passive_set_is_the_dbds_pp_set() {
        let rec = McaRecord::default();
        assert!(rec.processes_after_put("STRT"));
        assert!(rec.processes_after_put("READ"));
        assert!(rec.processes_after_put("PRTM"));
        assert!(!rec.processes_after_put("CALO"));
        assert!(!rec.processes_after_put("HIHI"));

        let declared: Vec<&str> = rec
            .field_list()
            .iter()
            .filter(|f| f.pp)
            .map(|f| f.name)
            .collect();
        assert_eq!(rec.process_passive_fields(), declared.as_slice());
    }

    /// C's `get_precision` serves 6 for the calibration fields and `PREC` for
    /// everything else; `DTIM`/`IDTIM` are percentages, so their display range is
    /// 0..100 and their alarm limits are the record's.
    #[test]
    fn the_percent_fields_and_the_calibration_fields_carry_their_own_metadata() {
        let rec = McaRecord {
            hihi: 90.0,
            high: 80.0,
            low: 10.0,
            lolo: 5.0,
            ..Default::default()
        };
        assert_eq!(
            rec.field_metadata_override("CALS").unwrap().precision,
            Some(6)
        );
        let idtim = rec.field_metadata_override("IDTIM").unwrap();
        assert_eq!(idtim.disp_limits, Some((100.0, 0.0)));
        assert_eq!(idtim.alarm_limits, Some((90.0, 80.0, 10.0, 5.0)));
        assert!(rec.field_metadata_override("ERTM").is_none());
    }

    /// C renders STIM with a 25-byte bound for a 26-byte rendering, so the field
    /// a client reads back ends `.**` — measured on the C IOC. The port emits the
    /// milliseconds the format string asks for.
    #[test]
    fn the_stop_time_carries_the_milliseconds_it_promises() {
        let mut rec = loaded(8, &[0; 8]);
        rec.process().unwrap();
        let stim = rec.stim.to_string();
        assert!(!stim.contains('*'), "{stim}");
        let millis = stim.rsplit('.').next().unwrap();
        assert_eq!(millis.len(), 3, "{stim}");
        assert!(millis.chars().all(|c| c.is_ascii_digit()), "{stim}");
    }

    /// `R`-prefixed scalars are not regions: the suffix set is closed.
    #[test]
    fn roi_field_names_do_not_swallow_the_records_own_r_fields() {
        assert_eq!(roi_field("R0LO"), Some((0, "LO")));
        assert_eq!(roi_field("R31NM"), Some((31, "NM")));
        assert_eq!(roi_field("R7"), Some((7, "")));
        assert_eq!(roi_field("R7N"), Some((7, "N")));
        assert_eq!(roi_field("R7P"), Some((7, "P")));
        assert_eq!(roi_field("RDNG"), None);
        assert_eq!(roi_field("RDNS"), None);
        assert_eq!(roi_field("RTIM"), None);
        assert_eq!(roi_field("RMAP"), None);
        assert_eq!(roi_field("R32"), None);
        assert_eq!(roi_field("R0X"), None);
    }
}
