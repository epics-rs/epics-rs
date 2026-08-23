use crate::error::{CaError, CaResult};
use crate::server::record::{MENU_SIMM, ProcessOutcome, RawSoftEntry, Record};
use crate::types::{EpicsValue, PvString};

/// Multi-bit binary input record — manual Record impl for raw↔index conversion.
pub struct MbbiRecord {
    pub val: u16,
    // RVAL/ORAW/MASK are DBF_ULONG (mbbiRecord.dbd.pod:604,608,613) — u32
    // storage so high-bit (>= 2^31) raw/mask values round-trip without
    // sign loss; served as EpicsValue::ULong.
    pub rval: u32,
    pub oraw: u32,
    pub mask: u32,
    // SHFT/NOBT/MLST/LALM are DBF_USHORT (mbbiRecord.dbd.pod:633,133,618,623).
    pub shft: u16,
    pub nobt: u16,
    pub mlst: u16,
    pub lalm: u16,
    pub zrsv: i16,
    pub onsv: i16,
    pub twsv: i16,
    pub thsv: i16,
    pub frsv: i16,
    pub fvsv: i16,
    pub sxsv: i16,
    pub svsv: i16,
    pub eisv: i16,
    pub nisv: i16,
    pub tesv: i16,
    pub elsv: i16,
    pub tvsv: i16,
    pub ttsv: i16,
    pub ftsv: i16,
    pub ffsv: i16,
    pub unsv: i16,
    pub cosv: i16,
    /// Alarm filter time constant (seconds). 0 = disabled.
    /// See `BiRecord::aftc` — same low-pass filter on alarm severity.
    pub aftc: f64,
    /// Alarm filter accumulator. 0 = initial sample.
    pub afvl: f64,
    // State raw values ZRVL..FFVL are DBF_ULONG (mbbiRecord.dbd.pod:144-264).
    pub zrvl: u32,
    pub onvl: u32,
    pub twvl: u32,
    pub thvl: u32,
    pub frvl: u32,
    pub fvvl: u32,
    pub sxvl: u32,
    pub svvl: u32,
    pub eivl: u32,
    pub nivl: u32,
    pub tevl: u32,
    pub elvl: u32,
    pub tvvl: u32,
    pub ttvl: u32,
    pub ftvl: u32,
    pub ffvl: u32,
    pub zrst: PvString,
    pub onst: PvString,
    pub twst: PvString,
    pub thst: PvString,
    pub frst: PvString,
    pub fvst: PvString,
    pub sxst: PvString,
    pub svst: PvString,
    pub eist: PvString,
    pub nist: PvString,
    pub test: PvString,
    pub elst: PvString,
    pub tvst: PvString,
    pub ttst: PvString,
    pub ftst: PvString,
    pub ffst: PvString,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    // SVAL is `DBF_ULONG` (mbbiRecord.dbd.pod:299-301) — the BUFFER C's
    // `readValue` reads SIOL into (`dbGetLink(&prec->siol, DBR_ULONG,
    // &prec->sval)`, mbbiRecord.c:390) before publishing `val = sval`.
    pub sval: u32,
    pub sims: i16,
    pub sdly: f64,
    skip_convert: bool,
    // VAL change gate. C
    // mbbiRecord.c:355-358 monitor() raises DBE_VALUE|DBE_LOG for VAL only
    // when `mlst != val`. Captured during process() because the framework
    // reads monitor_value_changed() after process() has committed mlst.
    value_changed: bool,
}

impl Default for MbbiRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            mask: 0,
            shft: 0,
            nobt: 0,
            mlst: 0,
            lalm: 0,
            zrsv: 0,
            onsv: 0,
            twsv: 0,
            thsv: 0,
            frsv: 0,
            fvsv: 0,
            sxsv: 0,
            svsv: 0,
            eisv: 0,
            nisv: 0,
            tesv: 0,
            elsv: 0,
            tvsv: 0,
            ttsv: 0,
            ftsv: 0,
            ffsv: 0,
            unsv: 0,
            cosv: 0,
            aftc: 0.0,
            afvl: 0.0,
            zrvl: 0,
            onvl: 0,
            twvl: 0,
            thvl: 0,
            frvl: 0,
            fvvl: 0,
            sxvl: 0,
            svvl: 0,
            eivl: 0,
            nivl: 0,
            tevl: 0,
            elvl: 0,
            tvvl: 0,
            ttvl: 0,
            ftvl: 0,
            ffvl: 0,
            zrst: PvString::new(),
            onst: PvString::new(),
            twst: PvString::new(),
            thst: PvString::new(),
            frst: PvString::new(),
            fvst: PvString::new(),
            sxst: PvString::new(),
            svst: PvString::new(),
            eist: PvString::new(),
            nist: PvString::new(),
            test: PvString::new(),
            elst: PvString::new(),
            tvst: PvString::new(),
            ttst: PvString::new(),
            ftst: PvString::new(),
            ffst: PvString::new(),
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sval: 0,
            sims: 0,
            sdly: -1.0,
            skip_convert: false,
            value_changed: false,
        }
    }
}

impl MbbiRecord {
    pub fn new(val: u16) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }

    fn raw_values(&self) -> [u32; 16] {
        [
            self.zrvl, self.onvl, self.twvl, self.thvl, self.frvl, self.fvvl, self.sxvl, self.svvl,
            self.eivl, self.nivl, self.tevl, self.elvl, self.tvvl, self.ttvl, self.ftvl, self.ffvl,
        ]
    }

    /// C `mbbiRecord.c::init_common` (93-105) — "check if any states are
    /// defined": TRUE as soon as one of ZRVL..FFVL is non-zero or one of
    /// ZRST..FFST is non-empty.
    ///
    /// DERIVED, never stored — the same call `MbboRecord::sdef` makes, and for
    /// the same reason. C keeps `prec->sdef` as a struct member and recomputes it
    /// from `init_common`: at init, and from `special()` after every write to any
    /// of the 32 state fields. A cached copy of a pure function of 32 other
    /// fields can go stale, and a `special()` that forgets one field is exactly
    /// how it does. Computing it on demand makes the stale state unreachable
    /// rather than merely unlikely, and leaves one definition of "are states
    /// defined" for both records instead of two that can drift.
    fn sdef(&self) -> bool {
        let rvs = self.raw_values();
        let sts: [&PvString; 16] = [
            &self.zrst, &self.onst, &self.twst, &self.thst, &self.frst, &self.fvst, &self.sxst,
            &self.svst, &self.eist, &self.nist, &self.test, &self.elst, &self.tvst, &self.ttst,
            &self.ftst, &self.ffst,
        ];
        (0..16).any(|i| rvs[i] != 0 || !sts[i].is_empty())
    }

    fn raw_to_val(&self, raw: u32) -> u16 {
        if !self.sdef() {
            return raw as u16;
        }
        let rvs = self.raw_values();
        for (i, &rv) in rvs.iter().enumerate() {
            if rv == raw {
                return i as u16;
            }
        }
        65535
    }

    /// The mask `devMbbiSoftRaw::read_mbbi` ANDs the raw word with.
    ///
    /// C builds it in the dset's `init_record` (`devMbbiSoftRaw.c:44-49`):
    /// `if (prec->nobt == 0) prec->mask = 0xffffffff;` — which OVERRIDES an
    /// explicitly configured MASK — `prec->mask <<= prec->shft;`. The record's
    /// own `init_record` has already set `mask = (1 << nobt) - 1` when MASK was
    /// left at 0 (`mbbiRecord.c:154-155`).
    ///
    /// C stores the shifted mask back into the MASK *field*; the port applies
    /// the shift here instead and leaves MASK as the record declared it, because
    /// the record's `process()` reads MASK on the (shared) device-readback path
    /// where it must stay unshifted.
    fn raw_soft_mask(&self) -> u32 {
        let base = if self.nobt == 0 {
            0xffff_ffff
        } else {
            self.mask
        };
        base.checked_shl(u32::from(self.shft)).unwrap_or(0)
    }

    /// Per-state severity fields ZRSV..FFSV indexed by state 0..15.
    fn state_severities(&self) -> [i16; 16] {
        [
            self.zrsv, self.onsv, self.twsv, self.thsv, self.frsv, self.fvsv, self.sxsv, self.svsv,
            self.eisv, self.nisv, self.tesv, self.elsv, self.tvsv, self.ttsv, self.ftsv, self.ffsv,
        ]
    }
}

/// Helper macro: maps EPICS field name strings to struct fields.
macro_rules! mbb_get_field {
    // `String`-tagged fields (the SIML/SIOL link grammar) store a Rust
    // `String`; `EpicsValue::String` wraps `PvString`, so convert with
    // `.into()`.
    (@get $self:expr, $field:ident, String) => {
        Some(EpicsValue::String($self.$field.clone().into()))
    };
    // `PvStr`-tagged fields are genuine DBF_STRING state-name data
    // (`field(ZRST..FFST,DBF_STRING) size(26)` in `mbbiRecord.dbd`),
    // stored byte-for-byte in a `PvString`, so the bytes pass through
    // verbatim — a non-UTF-8 state string round-trips unchanged.
    (@get $self:expr, $field:ident, PvStr) => {
        Some(EpicsValue::String($self.$field.clone()))
    };
    (@get $self:expr, $field:ident, $variant:ident) => {
        Some(EpicsValue::$variant($self.$field.clone()))
    };
    ($self:expr, $name:expr, $( $str:literal => $field:ident : $variant:ident ),* $(,)?) => {
        match $name {
            // C `mbbiRecord.c:53` is `#define cvt_dbaddr NULL` — unlike mbbo,
            // mbbi never re-types VAL, so it is DBF_ENUM with or without a
            // state table.
            "VAL" => Some(EpicsValue::Enum($self.val)),
            // `field(SDEF,DBF_SHORT) { special(SPC_NOMOD) }`
            // (mbbiRecord.dbd.pod:628) — readable over CA, never writable.
            // Derived, so it cannot disagree with the state table it reports on.
            "SDEF" => Some(EpicsValue::Short($self.sdef() as i16)),
            $( $str => mbb_get_field!(@get $self, $field, $variant), )*
            _ => None,
        }
    };
}

macro_rules! mbb_put_field {
    // `String`-tagged fields (the SIML/SIOL link grammar) store a Rust
    // `String`; the extracted payload is a `PvString`, so convert with
    // `.as_str_lossy().into_owned()`.
    (@put $self:expr, $field:ident, String, $value:expr, $name:expr) => {
        if let EpicsValue::String(v) = $value {
            $self.$field = v.as_str_lossy().into_owned();
        } else {
            return Err(CaError::TypeMismatch($name.into()));
        }
    };
    // `PvStr`-tagged fields are genuine DBF_STRING state-name data stored
    // in a `PvString`; move the wire bytes in verbatim so a non-UTF-8
    // state string is preserved.
    (@put $self:expr, $field:ident, PvStr, $value:expr, $name:expr) => {
        if let EpicsValue::String(v) = $value {
            $self.$field = v;
        } else {
            return Err(CaError::TypeMismatch($name.into()));
        }
    };
    // DBF_ULONG fields (RVAL/ORAW/MASK/ZRVL..FFVL): accept the native
    // unsigned carrier and tolerate the legacy signed `Long` that device
    // support / autosave presented before these were retyped to their true
    // unsigned dbd type (mbbiRecord.dbd.pod). The reinterpret preserves the
    // bit pattern, so a high-bit value round-trips.
    (@put $self:expr, $field:ident, ULong, $value:expr, $name:expr) => {
        $self.$field = match $value {
            EpicsValue::ULong(v) => v,
            EpicsValue::Long(v) => v as u32,
            _ => return Err(CaError::TypeMismatch($name.into())),
        };
    };
    // DBF_USHORT fields (NOBT/SHFT/MLST/LALM): accept the native unsigned
    // carrier and tolerate the legacy `Enum`/`Short` carriers.
    (@put $self:expr, $field:ident, UShort, $value:expr, $name:expr) => {
        $self.$field = match $value {
            EpicsValue::UShort(v) => v,
            EpicsValue::Enum(v) => v,
            EpicsValue::Short(v) => v as u16,
            _ => return Err(CaError::TypeMismatch($name.into())),
        };
    };
    (@put $self:expr, $field:ident, $variant:ident, $value:expr, $name:expr) => {
        if let EpicsValue::$variant(v) = $value {
            $self.$field = v;
        } else {
            return Err(CaError::TypeMismatch($name.into()));
        }
    };
    ($self:expr, $name:expr, $value:expr, $( $str:literal => $field:ident : $variant:ident ),* $(,)?) => {
        match $name {
            "VAL" => {
                match $value {
                    EpicsValue::Enum(v) => { $self.val = v; }
                    EpicsValue::Long(v) => { $self.val = v as u16; }
                    EpicsValue::Short(v) => { $self.val = v as u16; }
                    // C rset `put_enum_str` — the one string→state converter.
                    EpicsValue::String(ref s) => {
                        let resolved = crate::server::record::resolve_enum_state_string(
                            "VAL",
                            $self.enum_state_strings().as_deref(),
                            s,
                        )?;
                        if let EpicsValue::Enum(v) = resolved { $self.val = v; }
                    }
                    _ => return Err(CaError::TypeMismatch("VAL".into())),
                }
            }
            $( $str => { mbb_put_field!(@put $self, $field, $variant, $value, $str); } )*
            _ => return Err(CaError::FieldNotFound($name.to_string())),
        }
    };
}

impl Record for MbbiRecord {
    fn record_type(&self) -> &'static str {
        "mbbi"
    }

    /// C `mbbiRecord.c:221-226` — the `db_post_events` inside `special()`
    /// after a `SPC_MOD` write:
    ///
    /// ```c
    /// if (fieldIndex >= mbbiRecordZRST && fieldIndex <= mbbiRecordFFST
    ///         && prec->val == fieldIndex - mbbiRecordZRST) {
    ///     db_post_events(prec, &prec->val, DBE_VALUE | DBE_LOG);
    /// }
    /// ```
    ///
    /// Re-labelling the state VAL currently sits on changes what a
    /// `DBR_*_STRING` subscriber renders without changing VAL, so C posts VAL
    /// to push the new label out; every operator display reads a `mbbi` as
    /// a string. The equality is the whole gate — editing any OTHER state's
    /// label posts nothing, because no subscriber's rendering moved.
    ///
    /// ZRVL..FFVL are `SPC_MOD` too, and C's own comment there says so, but they
    /// fall outside the index range and post nothing. What they DO reach is
    /// `init_common`, whose `sdef` this port derives on demand
    /// ([`Self::sdef`]) rather than caching, so that half needs no hook.
    fn monitor_side_effect_fields(&self, put_field: &str) -> &'static [&'static str] {
        match crate::server::record::multibit_state_string_index(put_field) {
            Some(state) if state == self.val => &["VAL"],
            _ => &[],
        }
    }

    /// `SIMM` is `DBF_MENU menu(menuSimm)` (`mbbiRecord.dbd.pod`): the
    /// multibit records carry the three-choice NO/YES/RAW simulation menu.
    /// Served as `DBR_ENUM` with these labels. The state severities and
    /// `SIMS`/`OLDSIMM` are shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_SIMM),
            _ => None,
        }
    }

    /// C `devMbbiSoftRaw` — `recGblInitConstantLink(&prec->inp, DBF_ULONG,
    /// &prec->rval)` at init (`devMbbiSoftRaw.c:42`, unmasked), and per read
    /// `dbGetLink(pinp, DBR_LONG, &prec->rval, 0, 0)` followed by
    /// `prec->rval &= prec->mask` (`:78-79`, UNCONDITIONAL — unlike `bi`, whose
    /// dset masks only when MASK is non-zero). `read_mbbi` returns 0, so the
    /// record's `RVAL >> SHFT` → state-index convert then runs.
    fn raw_soft_input(&mut self, entry: RawSoftEntry, value: EpicsValue) -> Option<CaResult<()>> {
        self.rval = match super::raw_soft_rval_u32("mbbi", &value) {
            Ok(rval) => rval,
            Err(e) => return Some(Err(e)),
        };
        if entry == RawSoftEntry::Read {
            self.rval &= self.raw_soft_mask();
        }
        Some(Ok(()))
    }

    /// C rset `get_enum_strs`/`put_enum_str` (mbbiRecord.c:251-291) — ZRST..FFST
    /// cut at the last non-empty state.
    fn enum_state_strings(&self) -> Option<Vec<PvString>> {
        Some(crate::server::record::multibit_enum_states([
            &self.zrst, &self.onst, &self.twst, &self.thst, &self.frst, &self.fvst, &self.sxst,
            &self.svst, &self.eist, &self.nist, &self.test, &self.elst, &self.tvst, &self.ttst,
            &self.ftst, &self.ffst,
        ]))
    }

    /// C `get_enum_str` (mbbiRecord.c:235-255): any `val <= 15` reads its state slot,
    /// defined or not, so an unset state renders EMPTY; past 15 it is
    /// `"Illegal Value"`. `enum_state_strings` above stops at the last
    /// non-empty state because `no_str` says how many labels are meaningful —
    /// that trim must never reach this read, or an undefined state comes back
    /// as its index, which no C IOC emits.
    fn enum_string_form(&self) -> Option<crate::server::snapshot::EnumStringForm> {
        Some(crate::server::record::multibit_enum_string_form([
            &self.zrst, &self.onst, &self.twst, &self.thst, &self.frst, &self.fvst, &self.sxst,
            &self.svst, &self.eist, &self.nist, &self.test, &self.elst, &self.tvst, &self.ttst,
            &self.ftst, &self.ffst,
        ]))
    }

    /// VAL posts DBE_VALUE|DBE_LOG
    /// only when it changed (C mbbiRecord.c:355-358 `mlst != val`), not every
    /// process cycle. The comparison is captured in process(); see
    /// `value_changed`.
    fn monitor_value_changed(&self) -> Option<bool> {
        Some(self.value_changed)
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            if self.mask == 0 && self.nobt > 0 && self.nobt <= 32 {
                self.mask = ((1i64 << self.nobt) - 1) as u32;
            }
            self.mlst = self.val;
            self.lalm = self.val;
            self.oraw = self.rval;
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if !self.skip_convert {
            let mut rval = self.rval;
            if self.shft > 0 {
                // C `mbbiRecord.c:175-176` does `rval >>= prec->shft`
                // on a 32-bit `epicsUInt32`. A CA-written SHFT >= 32
                // makes that shift UB in C (no crash). Rust `>>`
                // panics in debug builds; `checked_shr` mapped to 0
                // matches the defined fully-shifted-out result.
                rval = rval.checked_shr(self.shft as u32).unwrap_or(0);
            }
            self.val = self.raw_to_val(rval);
        }
        self.skip_convert = false;
        self.oraw = self.rval;
        // Capture the VAL-change
        // gate now (C mbbiRecord.c:355-358 `mlst != val`); the framework reads
        // monitor_value_changed() after process().
        self.value_changed = self.mlst != self.val;
        if self.value_changed {
            self.mlst = self.val;
        }
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        mbb_get_field!(self, name,
            "RVAL" => rval: ULong, "ORAW" => oraw: ULong, "MASK" => mask: ULong,
            "SHFT" => shft: UShort, "MLST" => mlst: UShort, "LALM" => lalm: UShort,
            "NOBT" => nobt: UShort,
            "SIMM" => simm: Short, "SIML" => siml: String, "SIOL" => siol: String,
            "SIMS" => sims: Short,
            "SDLY" => sdly: Double,
            "ZRSV" => zrsv: Short, "ONSV" => onsv: Short, "TWSV" => twsv: Short, "THSV" => thsv: Short,
            "FRSV" => frsv: Short, "FVSV" => fvsv: Short, "SXSV" => sxsv: Short, "SVSV" => svsv: Short,
            "EISV" => eisv: Short, "NISV" => nisv: Short, "TESV" => tesv: Short, "ELSV" => elsv: Short,
            "TVSV" => tvsv: Short, "TTSV" => ttsv: Short, "FTSV" => ftsv: Short, "FFSV" => ffsv: Short,
            "UNSV" => unsv: Short, "COSV" => cosv: Short,
            "AFTC" => aftc: Double, "AFVL" => afvl: Double,
            "ZRVL" => zrvl: ULong, "ONVL" => onvl: ULong, "TWVL" => twvl: ULong, "THVL" => thvl: ULong,
            "FRVL" => frvl: ULong, "FVVL" => fvvl: ULong, "SXVL" => sxvl: ULong, "SVVL" => svvl: ULong,
            "EIVL" => eivl: ULong, "NIVL" => nivl: ULong, "TEVL" => tevl: ULong, "ELVL" => elvl: ULong,
            "TVVL" => tvvl: ULong, "TTVL" => ttvl: ULong, "FTVL" => ftvl: ULong, "FFVL" => ffvl: ULong,
            "ZRST" => zrst: PvStr, "ONST" => onst: PvStr, "TWST" => twst: PvStr, "THST" => thst: PvStr,
            "FRST" => frst: PvStr, "FVST" => fvst: PvStr, "SXST" => sxst: PvStr, "SVST" => svst: PvStr,
            "EIST" => eist: PvStr, "NIST" => nist: PvStr, "TEST" => test: PvStr, "ELST" => elst: PvStr,
            "TVST" => tvst: PvStr, "TTST" => ttst: PvStr, "FTST" => ftst: PvStr, "FFST" => ffst: PvStr,
        )
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        mbb_put_field!(self, name, value,
            "RVAL" => rval: ULong, "ORAW" => oraw: ULong, "MASK" => mask: ULong,
            "SHFT" => shft: UShort, "MLST" => mlst: UShort, "LALM" => lalm: UShort,
            "NOBT" => nobt: UShort,
            "SIMM" => simm: Short, "SIML" => siml: String, "SIOL" => siol: String,
            "SIMS" => sims: Short,
            "SDLY" => sdly: Double,
            "ZRSV" => zrsv: Short, "ONSV" => onsv: Short, "TWSV" => twsv: Short, "THSV" => thsv: Short,
            "FRSV" => frsv: Short, "FVSV" => fvsv: Short, "SXSV" => sxsv: Short, "SVSV" => svsv: Short,
            "EISV" => eisv: Short, "NISV" => nisv: Short, "TESV" => tesv: Short, "ELSV" => elsv: Short,
            "TVSV" => tvsv: Short, "TTSV" => ttsv: Short, "FTSV" => ftsv: Short, "FFSV" => ffsv: Short,
            "UNSV" => unsv: Short, "COSV" => cosv: Short,
            "AFTC" => aftc: Double, "AFVL" => afvl: Double,
            "ZRVL" => zrvl: ULong, "ONVL" => onvl: ULong, "TWVL" => twvl: ULong, "THVL" => thvl: ULong,
            "FRVL" => frvl: ULong, "FVVL" => fvvl: ULong, "SXVL" => sxvl: ULong, "SVVL" => svvl: ULong,
            "EIVL" => eivl: ULong, "NIVL" => nivl: ULong, "TEVL" => tevl: ULong, "ELVL" => elvl: ULong,
            "TVVL" => tvvl: ULong, "TTVL" => ttvl: ULong, "FTVL" => ftvl: ULong, "FFVL" => ffvl: ULong,
            "ZRST" => zrst: PvStr, "ONST" => onst: PvStr, "TWST" => twst: PvStr, "THST" => thst: PvStr,
            "FRST" => frst: PvStr, "FVST" => fvst: PvStr, "SXST" => sxst: PvStr, "SVST" => svst: PvStr,
            "EIST" => eist: PvStr, "NIST" => nist: PvStr, "TEST" => test: PvStr, "ELST" => elst: PvStr,
            "TVST" => tvst: PvStr, "TTST" => ttst: PvStr, "FTST" => ftst: PvStr, "FFST" => ffst: PvStr,
        );
        Ok(())
    }

    fn set_device_did_compute(&mut self, did: bool) {
        self.skip_convert = did;
    }

    /// `mbbi` has an `RVAL → VAL` `convert()` step. A `Soft Channel`
    /// `mbbi` must skip it — C `devMbbiSoft.c` `read_mbbi` returns 2.
    fn soft_channel_skips_convert(&self) -> bool {
        true
    }

    /// C `mbbiRecord.c::checkAlarms` — UDF alarm, STATE alarm from the
    /// per-state severity (ZRSV..FFSV, or UNSV for an unknown state)
    /// with the AFTC alarm-range low-pass filter (2009 Codeathon
    /// `824d37811`; mbbiRecord.c:319-337), and COS alarm (COSV).
    /// C `checkAlarms:300-305` raises `UDF_ALARM/udfs`, zeroes AFVL,
    /// and returns early when `udf` is set; we mirror that (raising
    /// UDF is idempotent with the framework's `rec_gbl_check_udf`).
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        if common.udf != 0 {
            recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::UDF_ALARM,
                AlarmSeverity::from_u16(common.udfs as u16),
            );
            self.afvl = 0.0;
            return;
        }
        let val = self.val;
        let raw_sev = if val > 15 {
            self.unsv
        } else {
            self.state_severities()[val as usize]
        };
        let (filtered, new_afvl) = super::alarm_filter::aftc_filter(
            raw_sev as u16,
            self.aftc,
            self.afvl,
            common.time,
            crate::runtime::general_time::get_current(),
        );
        self.afvl = new_afvl;
        let sev = AlarmSeverity::from_u16(filtered);
        if sev != AlarmSeverity::NoAlarm {
            recgbl::rec_gbl_set_sevr(common, alarm_status::STATE_ALARM, sev);
        }
        if val != self.lalm {
            let cos_sev = AlarmSeverity::from_u16(self.cosv as u16);
            if cos_sev != AlarmSeverity::NoAlarm {
                recgbl::rec_gbl_set_sevr(common, alarm_status::COS_ALARM, cos_sev);
            }
            self.lalm = val;
        }
    }

    /// Soft Channel VAL entry — a pass-through, NOT a raw->state map. C
    /// `devMbbiSoft::read_mbbi` (devMbbiSoft.c:51) does
    /// `dbGetLink(DBR_USHORT, &prec->val); return 2`: the link value is
    /// written straight to VAL as the state index, with no mask/shift/state
    /// lookup. The asyn DEVICE raw path is the separate `apply_raw_readback`
    /// hook below, so `set_val` carries one meaning only (soft-link / SIOL-sim
    /// pass-through). A Soft Channel mbbi whose INP resolves to a numeric
    /// source is therefore no longer reverse-mapped through the state table
    /// (the prior dual meaning, which diverged from C `return 2`).
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        // VAL is DBF_ENUM (mbbiRecord.dbd.pod): the state index. C reads the
        // link as DBR_USHORT, so every numeric carrier coerces to u16.
        match value {
            EpicsValue::Enum(v) => self.val = v,
            EpicsValue::ULong(v) => self.val = v as u16,
            EpicsValue::Long(v) => self.val = v as u16,
            EpicsValue::Short(v) => self.val = v as u16,
            // C rset `put_enum_str` (mbbiRecord.c:273-291), reached from
            // `dbConvert.c::putStringEnum` — the one string→state converter,
            // over `enum_state_strings` (the same table `get_enum_strs` serves).
            EpicsValue::String(ref s) => {
                let resolved = crate::server::record::resolve_enum_state_string(
                    "VAL",
                    self.enum_state_strings().as_deref(),
                    s,
                )?;
                if let EpicsValue::Enum(v) = resolved {
                    self.val = v;
                }
            }
            _ => return Err(CaError::TypeMismatch("VAL".into())),
        }
        Ok(())
    }

    /// asyn device readback: a raw hardware word read through `asynInt32` or
    /// `asynUInt32Digital`. C `processMbbi` sets `rval = value & mask` on
    /// BOTH ifaces (devAsynInt32.c:1270 with the record's NOBT-derived mask /
    /// devAsynUInt32Digital.c:903 with the `@asynMask`), and mbbiRecord's
    /// convert (mbbiRecord.c:172-190) then shifts `rval >> SHFT` and resolves
    /// the state index. The hook performs the (identical) mask + shift +
    /// lookup inline and returns `true` so the framework skips the forward
    /// convert.
    ///
    /// This is the *device-distinct* entry, separate from the Soft Channel
    /// `set_val` pass-through above — routing the device raw here is what lets
    /// `set_val` stay single-meaning. Input twin of `mbbo::apply_raw_readback`,
    /// with an **unconditional** `& mask` (unlike `bi`, whose asynInt32 dset
    /// leaves mask 0 and reads the value unmasked — `processMbbi` masks on both
    /// ifaces). The `& mask` here is the masking the prior `set_val` omitted.
    fn apply_raw_readback(&mut self, raw: i32) -> bool {
        let masked = (raw as u32) & self.mask;
        self.rval = masked;
        let shifted = if self.shft > 0 {
            masked.checked_shr(self.shft as u32).unwrap_or(0)
        } else {
            masked
        };
        self.val = self.raw_to_val(shifted);
        true
    }
}
