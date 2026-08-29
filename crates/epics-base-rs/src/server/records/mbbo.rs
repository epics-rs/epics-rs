use crate::error::{CaError, CaResult};
use crate::server::record::{MENU_SIMM, ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

/// Multi-bit binary output record — manual Record impl for raw↔index conversion.
pub struct MbboRecord {
    pub val: u16,
    // RVAL/ORAW/RBV/ORBV/MASK are DBF_ULONG (mbboRecord.dbd.pod:620-638) —
    // u32 storage so high-bit (>= 2^31) raw/mask values round-trip without
    // sign loss; served as EpicsValue::ULong.
    pub rval: u32,
    pub oraw: u32,
    pub rbv: u32,
    pub orbv: u32,
    pub mask: u32,
    // SHFT/NOBT are DBF_USHORT (mbboRecord.dbd.pod:658,211).
    pub shft: u16,
    pub nobt: u16,
    // MLST/LALM/IVOV are DBF_USHORT (mbboRecord.dbd.pod:643,648,717).
    pub mlst: u16,
    pub lalm: u16,
    pub ivoa: i16,
    pub ivov: u16,
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
    pub omsl: i16,
    pub dol: String,
    // State raw values ZRVL..FFVL are DBF_ULONG (mbboRecord.dbd.pod:222-342).
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
    pub sims: i16,
    pub sdly: f64,
    /// Set by `convert()` when VAL is an illegal state (`> 15` with a
    /// defined state table). C `mbboRecord.c::convert` raises
    /// `SOFT_ALARM/INVALID` for this case; the alarm is actually
    /// raised in `check_alarms()` since `convert()` has no access to
    /// `CommonFields`.
    soft_alarm: bool,
    /// Set by `set_device_did_compute(true)` when a device readback has
    /// already produced both RVAL and VAL (`apply_raw_readback`). One-shot:
    /// `process()` then skips the forward `VAL -> RVAL` `convert()` that
    /// would recompute RVAL from VAL and discard the readback — C
    /// `processMbbo` sets `rval`/`val` from the callback and returns without
    /// re-converting (devAsynInt32.c:1310-1330 / devAsynUInt32Digital.c:945-962).
    skip_convert: bool,
}

impl Default for MbboRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            rbv: 0,
            orbv: 0,
            mask: 0,
            shft: 0,
            nobt: 0,
            mlst: 0,
            lalm: 0,
            ivoa: 0,
            ivov: 0,
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
            omsl: 0,
            dol: String::new(),
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
            sims: 0,
            sdly: -1.0,
            soft_alarm: false,
            skip_convert: false,
        }
    }
}

impl MbboRecord {
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

    /// Reverse of `convert()`: map an already-`SHFT`-shifted raw to a state
    /// index. With a defined state table, the index whose ZRVL..FFVL matches,
    /// else `65535` (unknown state); without one, the raw is the value
    /// directly. C `processMbbo` readback (devAsynInt32.c:1314-1330 /
    /// devAsynUInt32Digital.c:948-963); the input twin is `mbbi::raw_to_val`.
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

    /// C `mbboRecord.c::init_common` — "check if any states are defined": TRUE
    /// as soon as one of ZRVL..FFVL is non-zero or one of ZRST..FFST is
    /// non-empty.
    ///
    /// DERIVED, never stored. C keeps `prec->sdef` as a struct member and
    /// recomputes it from `init_common` — at init, and from `special()` after
    /// every write to a state field. A cached copy of a value that is a pure
    /// function of 32 other fields can go stale, and here staleness is not
    /// cosmetic: `sdef` selects VAL's WIRE TYPE (`cvt_dbaddr`, DBF_USHORT with
    /// no state table, DBF_ENUM with one), so a stale `false` serves an enum
    /// field as a long. Computing it on demand makes that state unreachable
    /// rather than merely unlikely.
    fn sdef(&self) -> bool {
        let rvs = self.raw_values();
        let sts: [&PvString; 16] = [
            &self.zrst, &self.onst, &self.twst, &self.thst, &self.frst, &self.fvst, &self.sxst,
            &self.svst, &self.eist, &self.nist, &self.test, &self.elst, &self.tvst, &self.ttst,
            &self.ftst, &self.ffst,
        ];
        (0..16).any(|i| rvs[i] != 0 || !sts[i].is_empty())
    }

    /// C convert(): VAL -> RVAL with SDEF check and SHFT.
    ///
    /// When the state table is defined and `VAL > 15` the conversion
    /// is illegal — C `mbboRecord.c::convert` raises
    /// `SOFT_ALARM/INVALID` and leaves RVAL stale. We flag it via
    /// `soft_alarm` so `check_alarms()` raises the alarm.
    fn convert(&mut self) {
        self.soft_alarm = false;
        if self.sdef() {
            if self.val > 15 {
                self.soft_alarm = true;
                return;
            }
            let rvs = self.raw_values();
            self.rval = rvs[self.val as usize];
        } else {
            self.rval = self.val as u32;
        }
        if self.shft > 0 {
            // C `mbboRecord.c:433-434` does `prec->rval <<= prec->shft`
            // on a 32-bit `epicsUInt32`. A CA-written SHFT >= 32 makes
            // that shift UB in C (it does not crash — the value just
            // becomes 0 / implementation-defined). The Rust `<<`
            // panics in debug builds for the same shift count, so use
            // `checked_shl` and treat an out-of-range shift as a
            // fully-shifted-out 0 — defined, non-panicking, and the
            // value any sane 32-bit shift-out yields.
            self.rval = self.rval.checked_shl(self.shft as u32).unwrap_or(0);
        }
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
    // `String`-tagged fields (the DOL/OUT/SIML/SIOL link grammar) store a
    // Rust `String`; `EpicsValue::String` wraps `PvString`, so convert
    // with `.into()`.
    (@get $self:expr, $field:ident, String) => {
        Some(EpicsValue::String($self.$field.clone().into()))
    };
    // `PvStr`-tagged fields are genuine DBF_STRING state-name data
    // (`field(ZRST..FFST,DBF_STRING) size(26)` in `mbboRecord.dbd`),
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
            // C `mbboRecord.c:300-313` `cvt_dbaddr`: VAL is `special(SPC_DBADDR)`
            // and the record RE-TYPES it from its own state. With no state table
            // defined (`!prec->sdef` — every ZRVL..FFVL zero and every ZRST..FFST
            // empty) there are no labels to hand a client, so C degenerates VAL to
            // `DBF_USHORT`; CA has no unsigned-16 wire type, so it goes out
            // `DBF_LONG`. Measured on the C softIoc: bare `record(mbbo,"P:BARE")`
            // -> `cainfo` says DBF_LONG; the same record with ZRST/ONST -> DBF_ENUM.
            // This is why VAL carries `runtime_typed` — the declaration cannot
            // answer it, only the record can.
            "VAL" if !$self.sdef() => Some(EpicsValue::UShort($self.val)),
            "VAL" => Some(EpicsValue::Enum($self.val)),
            // `field(SDEF,DBF_SHORT) { special(SPC_NOMOD) }`
            // (mbboRecord.dbd.pod) — readable over CA, never writable. It is
            // the very predicate the VAL arms above branch on, so serving it
            // derived keeps the reported flag and the served wire type from
            // ever disagreeing.
            "SDEF" => Some(EpicsValue::Short($self.sdef() as i16)),
            $( $str => mbb_get_field!(@get $self, $field, $variant), )*
            _ => None,
        }
    };
}

macro_rules! mbb_put_field {
    // `String`-tagged fields (the DOL/OUT/SIML/SIOL link grammar) store a
    // Rust `String`; the extracted payload is a `PvString`, so convert
    // with `.as_str_lossy().into_owned()`.
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
    // DBF_ULONG fields (RVAL/ORAW/RBV/ORBV/MASK/ZRVL..FFVL): accept the
    // native unsigned carrier and tolerate the legacy signed `Long` that
    // device support / autosave presented before these were retyped to
    // their true unsigned dbd type (mbboRecord.dbd.pod). The reinterpret
    // preserves the bit pattern, so a high-bit value round-trips.
    (@put $self:expr, $field:ident, ULong, $value:expr, $name:expr) => {
        $self.$field = match $value {
            EpicsValue::ULong(v) => v,
            EpicsValue::Long(v) => v as u32,
            _ => return Err(CaError::TypeMismatch($name.into())),
        };
    };
    // DBF_USHORT fields (NOBT/SHFT/MLST/LALM/IVOV): accept the native
    // unsigned carrier and tolerate the legacy `Enum`/`Short` carriers.
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
                    // The put twin of the `cvt_dbaddr` degeneration above: a
                    // stateless mbbo is SERVED `DBF_USHORT`, so the write path
                    // coerces an inbound put to `UShort` before it lands here.
                    EpicsValue::UShort(v) => { $self.val = v; }
                    EpicsValue::Long(v) => { $self.val = v as u16; }
                    EpicsValue::Short(v) => { $self.val = v as u16; }
                    // C rset `put_enum_str` (mbboRecord.c:354-371) — the one
                    // string→state converter, over `enum_state_strings`.
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

impl Record for MbboRecord {
    fn record_type(&self) -> &'static str {
        "mbbo"
    }

    /// C `mbboRecord.c:196-210`: the scalar `dbGetLink(&prec->dol, ..., &prec->val, 0, 0)`
    /// under `dol.type != CONSTANT && omsl == menuOmslclosed_loop`.
    fn fetches_dol_closed_loop(&self) -> bool {
        true
    }

    /// C `mbboRecord.c:286-291` — the `db_post_events` inside `special()`
    /// after a `SPC_MOD` write:
    ///
    /// ```c
    /// if (fieldIndex >= mbboRecordZRST && fieldIndex <= mbboRecordFFST
    ///         && prec->val == fieldIndex - mbboRecordZRST) {
    ///     db_post_events(prec, &prec->val, DBE_VALUE | DBE_LOG);
    /// }
    /// ```
    ///
    /// Re-labelling the state VAL currently sits on changes what a
    /// `DBR_*_STRING` subscriber renders without changing VAL, so C posts VAL
    /// to push the new label out; every operator display reads a `mbbo` as
    /// a string. The equality is the whole gate — editing any OTHER state's
    /// label posts nothing, because no subscriber's rendering moved.
    ///
    /// ZRVL..FFVL are `SPC_MOD` too, and C's own comment there says so, but they
    /// fall outside the index range and post nothing. What they DO reach is
    /// `init_common`, whose `sdef` this port derives on demand
    /// (`Self::sdef`) rather than caching, so that half needs no hook.
    fn monitor_side_effect_fields(&self, put_field: &str) -> &'static [&'static str] {
        match crate::server::record::multibit_state_string_index(put_field) {
            Some(state) if state == self.val => &["VAL"],
            _ => &[],
        }
    }

    /// C `mbboRecord.c:199-215`: `process()` clears `udf` to FALSE only when a
    /// value SOURCE ran this cycle — a successful closed-loop DOL fetch. With no
    /// DOL (or a constant one), the `else if (prec->udf)` arm raises UDF_ALARM at
    /// UDFS and `goto CONTINUE`s, leaving `udf` untouched. `udf` is NEVER
    /// re-derived from the stored VAL every cycle, so the framework's blanket
    /// per-cycle clear (`processing.rs`) is wrong for mbbo: a bare
    /// `record(mbbo,"M"){}` with a client put to any PP field (e.g. COSV) must
    /// stay UDF=1/INVALID, not silently clear to NO_ALARM. The two real definers
    /// are covered elsewhere — a direct VAL put clears `udf` in `dbPut`
    /// (`field_io.rs`), and a closed-loop DOL fetch clears it at the DOL-apply
    /// site (`processing.rs`, C `:213`). So mbbo opts out of the per-cycle clear.
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `devMbboSoftRaw::write_mbbo` (`devMbboSoftRaw.c:40-46`):
    /// `data = prec->rval & prec->mask; dbPutLink(&prec->out, DBR_ULONG, &data,
    /// 1)` — the RAW word, not the state index `devMbboSoft` writes. The dset's
    /// `init_record` built the mask: `if (nobt == 0) mask = 0xffffffff;` (which
    /// overrides a configured MASK) then `mask <<= shft`.
    /// C `devMbboSoftRaw.c::write_mbbo` (71-75): `data = prec->rval &
    /// prec->mask; dbPutLink(&prec->out, DBR_ULONG, &data, 1)`. The dset's
    /// `init_record` (62-67) is what makes MASK usable — `if (nobt == 0) mask =
    /// 0xffffffff; mask <<= shft` — so the shifted mask is computed here rather
    /// than written back into the MASK field (see the commit note).
    fn raw_soft_output_value(&self) -> Option<EpicsValue> {
        let base = if self.nobt == 0 {
            0xffff_ffff
        } else {
            self.mask
        };
        let mask = base.checked_shl(u32::from(self.shft)).unwrap_or(0);
        Some(EpicsValue::ULong(self.rval & mask))
    }

    /// `SIMM` is `DBF_MENU menu(menuSimm)` (`mbboRecord.dbd.pod:673-677`):
    /// the multibit records carry the three-choice NO/YES/RAW simulation
    /// menu. Served as `DBR_ENUM` with these labels. `SIMS` (menuAlarmSevr)
    /// and the state severities are shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_SIMM),
            _ => None,
        }
    }

    /// C rset `get_enum_strs`/`put_enum_str` (mbboRecord.c:330-371) — ZRST..FFST
    /// cut at the last non-empty state.
    fn enum_state_strings(&self) -> Option<Vec<PvString>> {
        Some(crate::server::record::multibit_enum_states([
            &self.zrst, &self.onst, &self.twst, &self.thst, &self.frst, &self.fvst, &self.sxst,
            &self.svst, &self.eist, &self.nist, &self.test, &self.elst, &self.tvst, &self.ttst,
            &self.ftst, &self.ffst,
        ]))
    }

    /// C `get_enum_str` (mbboRecord.c:315-333): any `val <= 15` reads its state slot,
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

    /// C `mbboRecord.c:232-236` is `prec->val = prec->ivov;` followed by
    /// `convert(prec)` — the record's own VAL->RVAL translation
    /// (`mbboRecord.c:418-435`), which is the ZRVL..FFVL state-value lookup
    /// when SDEF is set, the `val > 15` SOFT/INVALID arm, and the `rval <<=
    /// shft` shift. RVAL is therefore IVOV translated, never IVOV itself.
    ///
    /// Writing `RVAL = IVOV` directly put the STATE INDEX on the wire in place
    /// of the raw word the state table names, so an INVALID `mbbo` with
    /// `field(TWVL,"0x40")` drove 2 instead of 0x40, and every SHFT was
    /// dropped.
    ///
    /// IVOV is DBF_USHORT (mbboRecord.dbd.pod:717); coerce the incoming
    /// carrier to the unsigned state index before storing it.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        let v: u16 = match ivov {
            EpicsValue::UShort(v) => v,
            EpicsValue::Enum(v) => v,
            EpicsValue::Short(v) => v as u16,
            EpicsValue::Long(v) => v as u16,
            _ => return Err(CaError::TypeMismatch("IVOV".into())),
        };
        self.put_field("VAL", EpicsValue::Enum(v))?;
        self.convert();
        Ok(())
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    /// C `mbboRecord.c:400-403` `monitor()`: `if (prec->mlst != prec->val) { events |=
    /// DBE_VALUE | DBE_LOG; prec->mlst = prec->val; }` — compared and
    /// committed HERE, at C's position, never captured during `process()`.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let changed = self.mlst != self.val;
        if changed {
            self.mlst = self.val;
        }
        Some(changed)
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            if self.mask == 0 && self.nobt > 0 && self.nobt <= 32 {
                self.mask = ((1i64 << self.nobt) - 1) as u32;
            }
        }
        Ok(())
    }

    /// C `mbboRecord.c:133-134`:
    /// `if (recGblInitConstantLink(&prec->dol, DBF_USHORT, &prec->val))
    ///      prec->udf = FALSE;`
    /// The framework gate (`processing.rs`) excludes a constant DOL from the
    /// per-cycle closed-loop fetch (C `!dbLinkIsConstant`), so the init-seed
    /// owner is the only place the constant state index can reach VAL — and a
    /// record whose VAL came from it is DEFINED.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        vec![crate::server::record::ConstantInitLink::dol_to_val(
            "DOL", "VAL",
        )]
    }

    /// C `mbboRecord.c:177,181-182` — the derived half of the init tail,
    /// run right after the constant load: `convert(prec)` maps the seeded state
    /// index to RVAL, then `oraw = rval; orbv = rbv`.
    fn init_record_tail(&mut self) {
        self.convert();
        self.oraw = self.rval;
        self.orbv = self.rbv;
    }

    /// C `mbboRecord.c:179-180` — `mlst = lalm = val`. No ALST: mbbo has no
    /// ADEL.
    fn seed_deadband_tracking(&mut self) {
        self.mlst = self.val;
        self.lalm = self.val;
    }

    /// C `mbboRecord.c::process` (line 217) calls `convert(prec)`
    /// UNCONDITIONALLY on every non-pact process — the VAL→RVAL output
    /// translation. It is never gated on whether VAL was just written
    /// (CA put, DOL fetch, or device support). A CA put to `mbbo.VAL`
    /// must therefore recompute `RVAL`/`ORAW`/`ORBV`. `mbbo` is an
    /// output record, so it does NOT override `set_device_did_compute`
    /// or `soft_channel_skips_convert` — the soft-channel convert-skip
    /// applies only to INPUT records (ai/bi/mbbi), where VAL is the
    /// engineering value.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // A device readback (`apply_raw_readback`) has already set both RVAL
        // and VAL from the hardware; skip the forward VAL -> RVAL convert that
        // would recompute RVAL from VAL and discard it. One-shot (C
        // `processMbbo` readback returns without re-converting; a normal
        // process always converts).
        if !self.skip_convert {
            self.convert();
        }
        self.skip_convert = false;
        self.oraw = self.rval;
        self.orbv = self.rbv;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        mbb_get_field!(self, name,
            "RVAL" => rval: ULong, "ORAW" => oraw: ULong,
            "RBV" => rbv: ULong, "ORBV" => orbv: ULong,
            "MASK" => mask: ULong, "SHFT" => shft: UShort,
            "MLST" => mlst: UShort, "LALM" => lalm: UShort,
            "IVOA" => ivoa: Short, "IVOV" => ivov: UShort,
            "NOBT" => nobt: UShort,
            "ZRSV" => zrsv: Short, "ONSV" => onsv: Short, "TWSV" => twsv: Short, "THSV" => thsv: Short,
            "FRSV" => frsv: Short, "FVSV" => fvsv: Short, "SXSV" => sxsv: Short, "SVSV" => svsv: Short,
            "EISV" => eisv: Short, "NISV" => nisv: Short, "TESV" => tesv: Short, "ELSV" => elsv: Short,
            "TVSV" => tvsv: Short, "TTSV" => ttsv: Short, "FTSV" => ftsv: Short, "FFSV" => ffsv: Short,
            "UNSV" => unsv: Short, "COSV" => cosv: Short,
            "OMSL" => omsl: Short, "DOL" => dol: String,
            "SIMM" => simm: Short, "SIML" => siml: String, "SIOL" => siol: String, "SIMS" => sims: Short,
            "SDLY" => sdly: Double,
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
            "RVAL" => rval: ULong, "ORAW" => oraw: ULong,
            "RBV" => rbv: ULong, "ORBV" => orbv: ULong,
            "MASK" => mask: ULong, "SHFT" => shft: UShort,
            "MLST" => mlst: UShort, "LALM" => lalm: UShort,
            "IVOA" => ivoa: Short, "IVOV" => ivov: UShort,
            "NOBT" => nobt: UShort,
            "ZRSV" => zrsv: Short, "ONSV" => onsv: Short, "TWSV" => twsv: Short, "THSV" => thsv: Short,
            "FRSV" => frsv: Short, "FVSV" => fvsv: Short, "SXSV" => sxsv: Short, "SVSV" => svsv: Short,
            "EISV" => eisv: Short, "NISV" => nisv: Short, "TESV" => tesv: Short, "ELSV" => elsv: Short,
            "TVSV" => tvsv: Short, "TTSV" => ttsv: Short, "FTSV" => ftsv: Short, "FFSV" => ffsv: Short,
            "UNSV" => unsv: Short, "COSV" => cosv: Short,
            "OMSL" => omsl: Short, "DOL" => dol: String,
            "SIMM" => simm: Short, "SIML" => siml: String, "SIOL" => siol: String, "SIMS" => sims: Short,
            "SDLY" => sdly: Double,
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

    fn can_device_write(&self) -> bool {
        true
    }

    fn set_device_did_compute(&mut self, did: bool) {
        self.skip_convert = did;
    }

    /// C `mbboRecord.c:204-206` — a failed closed-loop DOL read takes
    /// `goto CONTINUE`, jumping past `prec->udf = FALSE`, `convert(prec)` and
    /// the pre-output `recGblGetTimeStampSimm`. Suppressing the convert is what
    /// keeps a `caput REC.RVAL` from being recomputed out of a VAL the dead link
    /// never refreshed; the timestamp half is gated by
    /// [`Record::skips_timestamp_when_undefined`], which the framework consults
    /// on the same jump.
    fn closed_loop_dol_read_failed(&mut self) {
        self.skip_convert = true;
    }

    /// C `mbboRecord.c:210-213` — `else if (prec->udf) goto CONTINUE` skips the
    /// forward `convert()` when the record is undefined and no value was sourced
    /// this cycle. So a `caput REC.RVAL <v>` on a bare `record(mbbo,"M"){}`
    /// (UDF=1, no VAL put, no closed-loop DOL) stores `<v>` and reads it back —
    /// `convert` never recomputes `RVAL` from `VAL(=0)`. Opts mbbo into the
    /// framework's undefined-skip so `process()`'s convert is suppressed for
    /// exactly that cycle. Verified against the compiled softIoc.
    fn skips_forward_convert_when_undefined(&self) -> bool {
        true
    }

    /// C `mbboRecord.c:210-221` — the `else if (prec->udf) goto CONTINUE` that
    /// skips the forward convert ALSO skips the pre-output
    /// `recGblGetTimeStampSimm` (mbboRecord.c:221). The only post-`CONTINUE:`
    /// stamp is `if (pact)`-guarded (mbboRecord.c:256-258), so a soft (sync)
    /// UDF mbbo never stamps TIME — it stays at the EPICS epoch until a VAL put
    /// clears UDF. Opts mbbo into the framework's undefined timestamp-skip.
    fn skips_timestamp_when_undefined(&self) -> bool {
        true
    }

    /// Device readback (`asyn:READBACK` / SCAN="I/O Intr" / init seed): store
    /// the raw and resolve VAL through the state table, mirroring C
    /// `processMbbo`/`initMbbo` (devAsynInt32.c:1311-1330,1296 /
    /// devAsynUInt32Digital.c:945-963,930). `RVAL` keeps the masked-but-
    /// unshifted raw; VAL is the state index of the shifted raw. Returns
    /// `true` so the store reports `computed` and the framework skips the
    /// forward convert (via `set_device_did_compute`).
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

    /// C `mbboRecord.c::checkAlarms` + the `SOFT_ALARM` raised by
    /// `convert()` — STATE alarm from the per-state severity
    /// (ZRSV..FFSV, or UNSV for an unknown state), COS alarm (COSV),
    /// and `SOFT_ALARM/INVALID` when `VAL > 15` with a defined state
    /// table.
    ///
    /// The UDF alarm leads. C raises it in `process()` BEFORE `checkAlarms`
    /// (`mbboRecord.c:210-212`: `else if (prec->udf) { recGblSetSevr(prec,
    /// UDF_ALARM, prec->udfs); goto CONTINUE; }`), and `recGblSetSevr` overrides
    /// only on strictly greater severity (recGbl.c:242), so on a fresh record
    /// with `UDFS == ZRSV == INVALID` the equal-severity STATE alarm cannot
    /// displace it — STAT stays UDF. Raising it here (idempotent with the
    /// framework's `rec_gbl_check_udf`, which runs after this hook) puts UDF
    /// ahead of STATE. mbbo's C test is `if (prec->udf)` — truthy, not the
    /// exact-one form bo/stringout use.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        // C `mbboRecord.c:210` — UDF (truthy) raised before STATE.
        if recgbl::udf_alarm_active(common.udf, false) {
            recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::UDF_ALARM,
                AlarmSeverity::from_u16(common.udfs as u16),
            );
        }

        // SOFT_ALARM for an illegal VAL — C convert() path.
        if self.soft_alarm {
            recgbl::rec_gbl_set_sevr(common, alarm_status::SOFT_ALARM, AlarmSeverity::Invalid);
        }

        // STATE/COS use the RAW severity ordinal (ZRSV..FFSV/UNSV/COSV are
        // DBF_MENU stored raw i16): C `recGblSetSevr(prec, STATE_ALARM,
        // severities[val])` compares the raw `epicsEnum16`, so an out-of-range
        // `ZRSV=4`/`65535` numerically exceeds a prior UDF's INVALID(3) and
        // overrides it. See `rec_gbl_set_sevr_raw`.
        let val = self.val;
        let raw_sev = if val > 15 {
            self.unsv
        } else {
            self.state_severities()[val as usize]
        };
        recgbl::rec_gbl_set_sevr_raw(common, alarm_status::STATE_ALARM, raw_sev as u16);
        if val != self.lalm {
            recgbl::rec_gbl_set_sevr_raw(common, alarm_status::COS_ALARM, self.cosv as u16);
            self.lalm = val;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::record::dbd_generated;

    /// Simulation block (mbboRecord.dbd.pod:663-695) must be served:
    /// SIMM/SIMS as DBR_ENUM shorts, SIML/SIOL as the in/out link strings.
    /// SIMM's menu is menuSimm — three choices NO/YES/RAW, the RAW choice the
    /// two-choice menuYesNo lacks — so the wire-visible enum labels include RAW.
    #[test]
    fn test_sim_block_served() {
        let mut rec = MbboRecord::default();

        // SIMM/SIMS are DBF_MENU served as DBR_ENUM shorts.
        rec.put_field("SIMM", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("SIMM"), Some(EpicsValue::Short(1)));
        rec.put_field("SIMS", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.get_field("SIMS"), Some(EpicsValue::Short(2)));

        // SIML/SIOL are the simulation in/out links.
        rec.put_field("SIML", EpicsValue::String("sim:mode".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("SIML"),
            Some(EpicsValue::String("sim:mode".into()))
        );
        rec.put_field("SIOL", EpicsValue::String("sim:out".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("SIOL"),
            Some(EpicsValue::String("sim:out".into()))
        );

        // SIMM carries the three-choice menuSimm (NO/YES/RAW), not menuYesNo.
        let choices = rec.menu_field_choices("SIMM").unwrap();
        assert_eq!(choices, &["NO", "YES", "RAW"]);
        assert_eq!(choices, MENU_SIMM);

        // All four are advertised in the field list.
        for name in ["SIMM", "SIML", "SIOL", "SIMS"] {
            assert!(
                dbd_generated::MBBO_FIELDS.iter().any(|f| f.name == name),
                "{name} missing from field_list"
            );
        }
    }
}
