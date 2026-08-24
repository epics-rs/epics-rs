use crate::error::{CaError, CaResult};
use crate::server::record::{MENU_SIMM, ProcessOutcome, RawSoftEntry, Record};
use crate::types::EpicsValue;

// Multi-bit binary input direct record.
// VAL holds the full unsigned 32-bit value; B0-B1F expose individual bits
// as Char (0/1). C `mbbiDirectRecord.c` defines `NUM_BITS 32`, so the
// bit-field interface spans B0..B1F (32 fields).
// On process: RVAL is shifted right by SHFT and stored in VAL and bit fields.
pub struct MbbiDirectRecord {
    pub val: u32,
    // RVAL/ORAW/MASK are DBF_ULONG (mbbiDirectRecord.dbd.pod:112,116,121) —
    // u32 so high-bit raw/mask values round-trip without sign loss.
    pub rval: u32,
    pub oraw: u32,
    pub mask: u32,
    // SHFT is DBF_USHORT (mbbiDirectRecord.dbd.pod:131). VAL/NOBT/MLST are
    // DBF_LONG/DBF_SHORT (signed) on the Direct variant, unlike mbbi.
    pub shft: u16,
    pub nobt: i16,
    pub mlst: u32,
    pub bits: [u8; 32], // B0-B1F
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    // SVAL is `DBF_LONG` (mbbiDirectRecord.dbd.pod:203-205) — the BUFFER C's
    // `readValue` reads SIOL into (`dbGetLink(&prec->siol, DBR_LONG,
    // &prec->sval)`, mbbiDirectRecord.c:283) before publishing `val = sval`.
    pub sval: i32,
    pub sims: i16,
    pub sdly: f64,
    skip_convert: bool,
    // VAL change gate. C
    // mbbiDirectRecord.c:228-231 monitor() raises DBE_VALUE|DBE_LOG for VAL
    // only when `mlst != val`. Captured during process() because the
    // framework reads monitor_value_changed() after process() commits mlst.
    value_changed: bool,
}

impl Default for MbbiDirectRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            mask: 0,
            shft: 0,
            nobt: 0,
            mlst: 0,
            bits: [0; 32],
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

impl MbbiDirectRecord {
    fn val_to_bits(&mut self) {
        for i in 0..32 {
            self.bits[i] = ((self.val >> i) & 1) as u8;
        }
    }
}

/// Bit field names B0..B1F — 32 entries, matching C `NUM_BITS 32`.
pub(crate) const BIT_NAMES: [&str; 32] = [
    "B0", "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8", "B9", "BA", "BB", "BC", "BD", "BE", "BF",
    "B10", "B11", "B12", "B13", "B14", "B15", "B16", "B17", "B18", "B19", "B1A", "B1B", "B1C",
    "B1D", "B1E", "B1F",
];

impl Record for MbbiDirectRecord {
    /// C `mbbiDirectRecord.c:128` — `prec->mlst = prec->val`, the whole of this
    /// record's init tail. MLST is `special(SPC_NOMOD)` so it has no `put_field`
    /// arm for the trait default to go through; the record seeds its own cell.
    fn seed_deadband_tracking(&mut self) {
        self.mlst = self.val;
    }

    fn record_type(&self) -> &'static str {
        "mbbiDirect"
    }

    /// `SIMM` is `DBF_MENU menu(menuSimm)` (`mbbiDirectRecord.dbd.pod`): the
    /// three-choice NO/YES/RAW simulation menu. Served as `DBR_ENUM` with
    /// these labels. `SIMS`/`OLDSIMM` are shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_SIMM),
            _ => None,
        }
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    /// C `devMbbiDirectSoftRaw` — `recGblInitConstantLink(&prec->inp,
    /// DBF_ULONG, &prec->rval)` at init (`devMbbiDirectSoftRaw.c:42`, unmasked),
    /// and per read `dbGetLink(&prec->inp, DBR_ULONG, &prec->rval, 0, 0)` then
    /// `prec->rval &= prec->mask` (`:60-61`). `read_mbbi` returns 0, so the
    /// record's `RVAL >> SHFT` → VAL/bit-field convert runs.
    fn raw_soft_input(&mut self, entry: RawSoftEntry, value: EpicsValue) -> Option<CaResult<()>> {
        self.rval = match super::raw_soft_rval_u32("mbbiDirect", &value) {
            Ok(rval) => rval,
            Err(e) => return Some(Err(e)),
        };
        if entry == RawSoftEntry::Read {
            // The dset's init builds the mask: `if (nobt == 0) mask =
            // 0xffffffff;` (overriding a configured MASK) then `mask <<= shft`.
            let base = if self.nobt == 0 {
                0xffff_ffff
            } else {
                self.mask
            };
            self.rval &= base.checked_shl(u32::from(self.shft)).unwrap_or(0);
        }
        Some(Ok(()))
    }

    /// VAL posts DBE_VALUE|DBE_LOG
    /// only when it changed (C mbbiDirectRecord.c:228-231 `mlst != val`), not
    /// every process cycle. The comparison is captured in process(); see
    /// `value_changed`.
    fn monitor_value_changed(&self) -> Option<bool> {
        Some(self.value_changed)
    }

    /// C `mbbiDirectRecord.c:168-169` raises UDF with the literal `INVALID_ALARM`, not
    /// `prec->udfs`:
    ///
    /// ```c
    /// if (prec->udf)
    ///     recGblSetSevr(prec, UDF_ALARM, INVALID_ALARM);
    /// ```
    ///
    /// so `UDFS` cannot weaken or silence this record's UDF alarm. `mbboDirect`, the other half of the Direct pair, passes
    /// `prec->udfs` (`mbboDirectRecord.c:191`), and so does `mbbi`
    /// (`mbbiRecord.c:302`) — the split is per-record, not per-family.
    fn udf_alarm_severity(&self) -> Option<crate::server::record::AlarmSeverity> {
        Some(crate::server::record::AlarmSeverity::Invalid)
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // C `mbbiDirectRecord.c::init_record` — MASK from NOBT,
            // NOBT may span 1..32 (NUM_BITS 32).
            if self.mask == 0 && self.nobt > 0 && self.nobt <= 32 {
                self.mask = ((1i64 << self.nobt) - 1) as u32;
            }
            self.mlst = self.val;
            self.oraw = self.rval;
            self.val_to_bits();
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if !self.skip_convert {
            // C `mbbiDirectRecord.c:155-163` is the whole convert:
            //
            //     epicsUInt32 rval = prec->rval;
            //     if (prec->shft > 0) rval >>= prec->shft;
            //     prec->val = rval;
            //
            // MASK is not read. It belongs to device support, which positions
            // it (`prec->mask <<= prec->shft`, devMbbiDirectSoftRaw.c:48) and
            // applies it to RVAL before the record ever sees it (`:55`), or —
            // under asyn — overwrites MASK with the driver's already-positioned
            // hardware mask (devAsynUInt32Digital.c:1008-1009). Masking here
            // with the record's own UNSHIFTED NOBT mask destroyed the value it
            // was meant to protect: NOBT=4 SHFT=4 RVAL=240 gives `240 & 0x0F`
            // = 0 instead of 15.
            let mut raw = self.rval;
            if self.shft > 0 {
                // Same defect family as mbbi/mbbo: a CA-written SHFT
                // >= 32 panics a bare `>>` in debug builds. `checked_shr`
                // mapped to 0 matches the C UB-but-no-crash result.
                raw = raw.checked_shr(self.shft as u32).unwrap_or(0);
            }
            self.val = raw;
        }
        self.skip_convert = false;
        // C `mbbiDirectRecord.c:217-226` monitor() re-derives every bit
        // B0..B1F FROM VAL each cycle (`*pBn = !!(val & 1)`), whether or not
        // the RVAL->VAL convert ran. In this INPUT record the bit fields are
        // DERIVED from VAL — a `pp(TRUE)` Bx put processes the record but never
        // changes VAL (Bx is not a value field), so the transient bit is
        // overwritten by the VAL-derived bit. Re-derive UNCONDITIONALLY here
        // (a soft-channel process sets `skip_convert`, leaving VAL untouched);
        // gating the rebuild on the convert left a Bx put's transient bit
        // standing, so `caput B0 1` with VAL=0 ended B0=1 instead of C's 0.
        // Opposite direction from mbboDirect, where Bx puts DERIVE VAL.
        self.val_to_bits();
        self.oraw = self.rval;
        // Capture the VAL-change
        // gate now (C mbbiDirectRecord.c:228-231 `mlst != val`); the framework
        // reads monitor_value_changed() after process().
        self.value_changed = self.mlst != self.val;
        if self.value_changed {
            self.mlst = self.val;
        }
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val as i32)),
            "RVAL" => Some(EpicsValue::ULong(self.rval)),
            "ORAW" => Some(EpicsValue::ULong(self.oraw)),
            "MASK" => Some(EpicsValue::ULong(self.mask)),
            "SHFT" => Some(EpicsValue::UShort(self.shft)),
            "NOBT" => Some(EpicsValue::Short(self.nobt)),
            "MLST" => Some(EpicsValue::Long(self.mlst as i32)),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            // B0..B1F are DBF_UCHAR (mbbiDirectRecord.dbd.pod). Serve the bit as
            // the native `UChar` (unsigned) — it still projects to `DBR_CHAR` on
            // the wire (`dbr.rs`: `UChar -> Char`), so a client's `caget` reports
            // DBF_CHAR exactly as C does, but the put-coercion target derived
            // from this value (`dbput_request`) is now the unsigned 0..=255 range
            // instead of signed i8. C `dbFastPutConvert[DBR_STRING][DBF_UCHAR]`
            // accepts `caput .Bn 255`; the prior `Char` representation made the
            // port refuse everything above i8-max (128..=255).
            _ => BIT_NAMES
                .iter()
                .position(|&n| n == name)
                .map(|idx| EpicsValue::UChar(self.bits[idx])),
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                match value {
                    EpicsValue::Long(v) => self.val = v as u32,
                    EpicsValue::Short(v) => self.val = v as u32,
                    EpicsValue::Char(v) => self.val = v as u32,
                    _ => return Err(CaError::TypeMismatch("VAL".into())),
                }
                self.val_to_bits();
            }
            // RVAL/MASK are DBF_ULONG: accept the native ULong and tolerate
            // the legacy signed Long (device support / autosave).
            "RVAL" => {
                self.rval = match value {
                    EpicsValue::ULong(v) => v,
                    EpicsValue::Long(v) => v as u32,
                    _ => return Err(CaError::TypeMismatch("RVAL".into())),
                };
            }
            "MASK" => {
                self.mask = match value {
                    EpicsValue::ULong(v) => v,
                    EpicsValue::Long(v) => v as u32,
                    _ => return Err(CaError::TypeMismatch("MASK".into())),
                };
            }
            // SHFT is DBF_USHORT: accept UShort, tolerate Enum/Short.
            "SHFT" => {
                self.shft = match value {
                    EpicsValue::UShort(v) => v,
                    EpicsValue::Enum(v) => v,
                    EpicsValue::Short(v) => v as u16,
                    _ => return Err(CaError::TypeMismatch("SHFT".into())),
                };
            }
            "NOBT" => {
                if let EpicsValue::Short(v) = value {
                    self.nobt = v;
                } else {
                    return Err(CaError::TypeMismatch("NOBT".into()));
                }
            }
            "SIMM" => {
                if let EpicsValue::Short(v) = value {
                    self.simm = v;
                } else {
                    return Err(CaError::TypeMismatch("SIMM".into()));
                }
            }
            "SIML" => {
                if let EpicsValue::String(v) = value {
                    self.siml = v.as_str_lossy().into_owned();
                } else {
                    return Err(CaError::TypeMismatch("SIML".into()));
                }
            }
            "SIOL" => {
                if let EpicsValue::String(v) = value {
                    self.siol = v.as_str_lossy().into_owned();
                } else {
                    return Err(CaError::TypeMismatch("SIOL".into()));
                }
            }
            "SIMS" => {
                if let EpicsValue::Short(v) = value {
                    self.sims = v;
                } else {
                    return Err(CaError::TypeMismatch("SIMS".into()));
                }
            }
            "SDLY" => {
                if let EpicsValue::Double(v) = value {
                    self.sdly = v;
                } else {
                    return Err(CaError::TypeMismatch("SDLY".into()));
                }
            }
            _ => {
                if let Some(idx) = BIT_NAMES.iter().position(|&n| n == name) {
                    // B0..B1F are DBF_UCHAR, so a numeric CA put coerces to the
                    // native `UChar`; accept it alongside the signed variants
                    // device support / autosave may send. C stores the coerced
                    // byte into `*pBn` and `monitor()` re-derives it as
                    // `!! (val & 1)` (mbbiDirectRecord.c:221) — a NONZERO byte is
                    // bit 1, not the low bit. This transient store is overwritten
                    // by the re-derive below on the very next process, so it only
                    // affects a readback taken before process; mirror C's nonzero
                    // rule regardless.
                    let bit = match value {
                        EpicsValue::UChar(v) => u8::from(v != 0),
                        EpicsValue::Char(v) => u8::from(v != 0),
                        EpicsValue::Short(v) => u8::from(v != 0),
                        EpicsValue::Long(v) => u8::from(v != 0),
                        _ => return Err(CaError::TypeMismatch(name.into())),
                    };
                    // INPUT record: Bx is DERIVED from VAL, so a Bx put must
                    // NOT fold back into VAL (unlike mbboDirect). Store the raw
                    // bit transiently; the `pp(TRUE)` put processes the record,
                    // and process() re-derives every bit from the unchanged VAL,
                    // overwriting this (C `mbbiDirectRecord.c` has no `special`
                    // for Bx and never updates VAL on a bit put — VAL stays put).
                    self.bits[idx] = bit;
                } else {
                    return Err(CaError::FieldNotFound(name.to_string()));
                }
            }
        }
        Ok(())
    }

    fn set_device_did_compute(&mut self, did: bool) {
        self.skip_convert = did;
    }

    /// asyn device readback (asynUInt32Digital only — `asynMbbiDirectUInt32Digital`
    /// is the sole mbbiDirect dset, devAsynUInt32Digital.c:165): `processMbbiDirect`
    /// sets `pr->rval = value & mask` (devAsynUInt32Digital.c:1031) and returns 0,
    /// so mbbiDirectRecord's `rval -> (>> SHFT) -> VAL` + bit-field convert resolves
    /// VAL. The hook runs that convert inline and returns `true` so the framework's
    /// `set_device_did_compute(true)` makes `process()` skip the (identical) forward
    /// pass. Without it the raw word lands verbatim in VAL via the default `set_val`
    /// (no MASK, no SHFT, wrong bits). Device-distinct entry — the Soft Channel path
    /// stays on `set_val` (C `devMbbiDirectSoft` returns 2). Input twin of
    /// `mbbo_direct::apply_raw_readback`.
    fn apply_raw_readback(&mut self, raw: i32) -> bool {
        let masked = if self.mask != 0 {
            (raw as u32) & self.mask
        } else {
            raw as u32
        };
        self.rval = masked;
        self.val = if self.shft > 0 {
            masked.checked_shr(self.shft as u32).unwrap_or(0)
        } else {
            masked
        };
        self.val_to_bits();
        true
    }

    /// `mbbiDirect` has an `RVAL → VAL` `convert()` step. A `Soft
    /// Channel` `mbbiDirect` must skip it — C `devMbbiDirectSoft.c`
    /// `read_mbbiDirect` returns 2.
    fn soft_channel_skips_convert(&self) -> bool {
        true
    }
}
