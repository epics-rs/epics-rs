use crate::error::{CaError, CaResult};
use crate::server::record::{MENU_SIMM, ProcessOutcome, Record};
use crate::types::EpicsValue;

use super::mbbi_direct::BIT_NAMES;

// Multi-bit binary output direct record.
// VAL holds the full unsigned 32-bit value; B0-B1F expose individual bits
// as Char (0/1). C `mbboDirectRecord.c` defines `NUM_BITS 32`.
// On process: VAL is shifted left by SHFT and written as RVAL.
// Writing to any BX field recomputes VAL from individual bits before the
// next process.
pub struct MbboDirectRecord {
    pub val: u32,
    // RVAL/ORAW/RBV/ORBV/MASK are DBF_ULONG (mbboDirectRecord.dbd.pod:
    // 167,172,177,181,186) — u32 so high-bit raw/mask values round-trip.
    pub rval: u32,
    pub oraw: u32,
    pub rbv: u32,
    pub orbv: u32,
    pub mask: u32,
    // SHFT is DBF_USHORT (mbboDirectRecord.dbd.pod:201). VAL/NOBT/MLST/IVOV
    // are DBF_LONG/DBF_SHORT (signed) on the Direct variant, unlike mbbo.
    pub shft: u16,
    pub nobt: i16,
    pub mlst: u32,
    pub ivoa: i16,
    pub ivov: u32,
    pub omsl: i16,
    pub dol: String,
    pub bits: [u8; 32], // B0-B1F
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    pub sdly: f64,
    // VAL change gate. C
    // mbboDirectRecord.c:311-314 monitor() raises DBE_VALUE|DBE_LOG for VAL
    // only when `mlst != val`. Captured during process() because the
    // framework reads monitor_value_changed() after process() commits mlst.
    value_changed: bool,
    /// Set by `set_device_did_compute(true)` when a device readback has
    /// already produced both RVAL and VAL (`apply_raw_readback`). One-shot:
    /// `process()` then skips the forward `VAL -> RVAL` convert that would
    /// recompute RVAL from VAL and discard the readback — C `processMbboDirect`
    /// sets `rval`/`val` from the callback and returns without re-converting
    /// (devAsynUInt32Digital.c:1084-1090).
    skip_convert: bool,
}

impl Default for MbboDirectRecord {
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
            ivoa: 0,
            ivov: 0,
            omsl: 0,
            dol: String::new(),
            bits: [0; 32],
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            sdly: -1.0,
            value_changed: false,
            skip_convert: false,
        }
    }
}

impl MbboDirectRecord {
    fn val_to_bits(&mut self) {
        for i in 0..32 {
            self.bits[i] = ((self.val >> i) & 1) as u8;
        }
    }

    fn bits_to_val(&mut self) {
        self.val = 0;
        for i in 0..32 {
            self.val |= (self.bits[i] as u32 & 1) << i;
        }
    }
}

impl Record for MbboDirectRecord {
    fn record_type(&self) -> &'static str {
        "mbboDirect"
    }

    /// C `devMbboDirectSoftRaw::write_mbbo` (`devMbboDirectSoftRaw.c:40-46`):
    /// `data = prec->rval & prec->mask; dbPutLink(&prec->out, DBR_ULONG, &data,
    /// 1)`, with the same dset-init mask (`nobt == 0` ⇒ `0xffffffff`, then
    /// `<<= shft`).
    /// C `devMbboDirectSoftRaw.c::write_mbbo` (71-75): `data = prec->rval &
    /// prec->mask; dbPutLink(&prec->out, DBR_ULONG, &data, 1)`, with the dset's
    /// `init_record` mask rule (`nobt == 0 -> 0xffffffff`, then `<<= shft`).
    fn raw_soft_output_value(&self) -> Option<EpicsValue> {
        let base = if self.nobt == 0 {
            0xffff_ffff
        } else {
            self.mask
        };
        let mask = base.checked_shl(u32::from(self.shft)).unwrap_or(0);
        Some(EpicsValue::ULong(self.rval & mask))
    }

    fn set_device_did_compute(&mut self, did: bool) {
        self.skip_convert = did;
    }

    /// Device readback (`asyn:READBACK` / SCAN="I/O Intr" / init seed): store
    /// the raw and set VAL to the shifted masked value, mirroring C
    /// `processMbboDirect`/`initMbboDirect` (devAsynUInt32Digital.c:1084-1090,
    /// 1058-1065). RVAL keeps the masked-but-unshifted raw; VAL is the shifted
    /// value (no state table — Direct is the raw value). The B0..B1F bits are
    /// re-derived from the new VAL. Returns `true` so the store reports
    /// `computed` and the framework skips the forward convert (via
    /// `set_device_did_compute`).
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

    /// `SIMM` is `DBF_MENU menu(menuSimm)` (`mbboDirectRecord.dbd.pod`): the
    /// three-choice NO/YES/RAW simulation menu. Served as `DBR_ENUM` with
    /// these labels. `SIMS`/`OLDSIMM`/`OMSL`/`IVOA` are shared menus
    /// resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_SIMM),
            _ => None,
        }
    }

    // C recMbboDirect.c IVOA=set_to_IVOV: val = ivov; rval = ivov.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        // IVOV is Long on mbboDirect; both VAL and RVAL accept Long.
        self.put_field("RVAL", ivov.clone())?;
        self.put_field("VAL", ivov)
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    /// C `mbboDirectRecord.c:190-195`: `process()` clears `udf` to FALSE only
    /// when a value SOURCE ran this cycle — a successful closed-loop DOL
    /// fetch. With no DOL (or a constant one), the `else if (prec->udf)` arm
    /// raises UDF_ALARM at UDFS and `goto CONTINUE`s, leaving `udf`
    /// untouched. `udf` is NEVER re-derived from the stored VAL every cycle,
    /// so the framework's blanket per-cycle clear (`processing.rs`) is wrong
    /// for mbboDirect: a bare `record(mbboDirect,"M"){}` with a client put to
    /// a PP field must stay UDF=1/INVALID, not silently clear to NO_ALARM.
    /// The real definers are covered elsewhere — a VAL put clears `udf` in
    /// `dbPut` (`field_io.rs`), a Bn bit-field put clears it via
    /// [`Self::is_udf_defining_put`] below, and a closed-loop DOL fetch
    /// clears it at the DOL-apply site (`processing.rs`, C `:188`). So
    /// mbboDirect opts out of the per-cycle clear.
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `mbboDirectRecord.c:191` is the ONE base record that raises UDF
    /// with a bespoke message: `recGblSetSevrMsg(prec, UDF_ALARM,
    /// prec->udfs, "UDFS")`. Every other record uses plain `recGblSetSevr`
    /// (empty namsg). So a PVA read of an undefined mbboDirect serves
    /// `alarm.message = "UDFS"` (pvxs `iocsource.cpp:230-236` prefers the
    /// non-empty amsg), not the "UDF" condition string.
    fn udf_alarm_message(&self) -> &str {
        "UDFS"
    }

    /// C `mbboDirectRecord.c::special` (`after==1`, B0..B1F, line 290):
    /// `prec->udf = FALSE` — a bit-field put defines the record exactly like
    /// a VAL put, independent of `dbIsValueField` (VAL only, `field_io.rs`'s
    /// generic dbPut UDF clear). A client put to B0..B1F must clear UDF the
    /// same way a VAL put does.
    fn is_udf_defining_put(&self, field: &str) -> bool {
        field == self.primary_field() || BIT_NAMES.contains(&field)
    }

    /// VAL posts DBE_VALUE|DBE_LOG
    /// only when it changed (C mbboDirectRecord.c:311-314 `mlst != val`), not
    /// every process cycle. The comparison is captured in process(); see
    /// `value_changed`.
    fn monitor_value_changed(&self) -> Option<bool> {
        Some(self.value_changed)
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // C `mbboDirectRecord.c::init_record` — MASK from NOBT,
            // NOBT may span 1..32 (NUM_BITS 32).
            if self.mask == 0 && self.nobt > 0 && self.nobt <= 32 {
                self.mask = ((1i64 << self.nobt) - 1) as u32;
            }
            self.mlst = self.val;
            self.oraw = self.rval;
            // Don't derive bits from VAL yet — `post_init_finalize_undef`
            // runs after both passes and chooses VAL→bits or bits→VAL
            // based on the framework's UDF flag.
        }
        Ok(())
    }

    /// epics-base PR `dabcf89` (2021): when the record initialises
    /// with no VAL set (UDF=true) but the operator populated B0..B1F
    /// bits in the .db file, fold those bits into VAL and clear UDF.
    /// Otherwise (VAL was set explicitly), derive bits from VAL.
    fn post_init_finalize_undef(&mut self, common_udf: &mut bool) -> CaResult<()> {
        // C `mbboDirectRecord.c::init_record` applies a constant DOL to VAL
        // once via `recGblInitConstantLink(&prec->dol, DBF_LONG, &prec->val)`,
        // which clears UDF on success; the bit fields B0..B1F are then
        // derived from VAL. The framework gate (`processing.rs`) excludes a
        // constant DOL from the per-cycle closed-loop fetch (C
        // `!dbLinkIsConstant`), so the constant must be seeded here. Clearing
        // `common_udf` both matches C (recGblInitConstantLink → udf=FALSE)
        // and routes the finalize below through `val_to_bits`, so the
        // observable bit fields reflect the seeded VAL.
        if let crate::server::record::ParsedLink::Constant(s) =
            crate::server::record::parse_link_v2(&self.dol)
        {
            if let Ok(v) = s.trim().parse::<f64>() {
                self.val = v as u32;
                *common_udf = false;
            }
        }
        if !*common_udf {
            self.val_to_bits();
        } else {
            let any_bit_set = self.bits.iter().any(|&b| b != 0);
            if any_bit_set {
                self.bits_to_val();
                *common_udf = false;
            }
        }
        Ok(())
    }

    /// C `mbboDirectRecord.c::process` (line 198) calls `convert(prec)`
    /// UNCONDITIONALLY on every non-pact process — the VAL→RVAL output
    /// translation. A CA put to `mbboDirect.VAL` must therefore
    /// recompute `RVAL`/`ORAW`. `mbboDirect` is an output record, so it
    /// does NOT override `set_device_did_compute` or
    /// `soft_channel_skips_convert` — the soft-channel convert-skip
    /// applies only to INPUT records.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `mbboDirectRecord.c::convert` — RVAL = (VAL << SHFT) & MASK on a
        // 32-bit epicsUInt32 (RVAL/MASK are DBF_ULONG) — unless a device
        // readback (`apply_raw_readback`) already set both RVAL and VAL, in
        // which case skip the forward convert that would recompute RVAL from
        // VAL and discard it. One-shot (C `processMbboDirect` readback returns
        // without re-converting; a normal process always converts).
        if !self.skip_convert {
            let mut raw = self.val;
            if self.shft > 0 {
                raw = raw.wrapping_shl(self.shft as u32);
            }
            if self.mask != 0 {
                raw &= self.mask;
            }
            self.rval = raw;
        }
        self.skip_convert = false;
        // C `mbboDirectRecord.c` — RBV is updated ONLY by device support
        // (the hardware read-back); record support never assigns it.
        // Forcing `RBV = RVAL` here would mask hardware disagreement,
        // so we only roll `orbv` forward and leave `rbv` untouched.
        self.orbv = self.rbv;
        self.oraw = self.rval;
        // Capture the VAL-change
        // gate now (C mbboDirectRecord.c:311-314 `mlst != val`); the framework
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
            "RBV" => Some(EpicsValue::ULong(self.rbv)),
            "ORBV" => Some(EpicsValue::ULong(self.orbv)),
            "MASK" => Some(EpicsValue::ULong(self.mask)),
            "SHFT" => Some(EpicsValue::UShort(self.shft)),
            "NOBT" => Some(EpicsValue::Short(self.nobt)),
            "MLST" => Some(EpicsValue::Long(self.mlst as i32)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Long(self.ivov as i32)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone().into())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            // B0..B1F are DBF_UCHAR (mbboDirectRecord.dbd.pod). Serve the bit as
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
            // RVAL/RBV/MASK are DBF_ULONG: accept the native ULong and
            // tolerate the legacy signed Long (device support / autosave).
            "RVAL" => {
                self.rval = match value {
                    EpicsValue::ULong(v) => v,
                    EpicsValue::Long(v) => v as u32,
                    _ => return Err(CaError::TypeMismatch("RVAL".into())),
                };
            }
            "RBV" => {
                self.rbv = match value {
                    EpicsValue::ULong(v) => v,
                    EpicsValue::Long(v) => v as u32,
                    _ => return Err(CaError::TypeMismatch("RBV".into())),
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
            "IVOA" => {
                if let EpicsValue::Short(v) = value {
                    self.ivoa = v;
                } else {
                    return Err(CaError::TypeMismatch("IVOA".into()));
                }
            }
            "IVOV" => {
                if let EpicsValue::Long(v) = value {
                    self.ivov = v as u32;
                } else {
                    return Err(CaError::TypeMismatch("IVOV".into()));
                }
            }
            "OMSL" => {
                if let EpicsValue::Short(v) = value {
                    self.omsl = v;
                } else {
                    return Err(CaError::TypeMismatch("OMSL".into()));
                }
            }
            "DOL" => {
                if let EpicsValue::String(v) = value {
                    self.dol = v.as_str_lossy().into_owned();
                } else {
                    return Err(CaError::TypeMismatch("DOL".into()));
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
                    // B0..B1F are DBF_UCHAR (mbboDirectRecord.dbd.pod), so a
                    // numeric CA put coerces to the native `UChar`; accept it
                    // alongside the signed variants device support / autosave
                    // may send. C `mbboDirectRecord.c::special` (after==1, line
                    // 282) sets the bit with `if (*pBn)` — the coerced value
                    // defines the bit when NONZERO, not by its low bit — and
                    // `bitsFromVAL` (line 96) then normalizes it to 0/1. So the
                    // store rule is `value != 0`, uniform across all 32 bits.
                    let bit = match value {
                        EpicsValue::UChar(v) => u8::from(v != 0),
                        EpicsValue::Char(v) => u8::from(v != 0),
                        EpicsValue::Short(v) => u8::from(v != 0),
                        EpicsValue::Long(v) => u8::from(v != 0),
                        _ => return Err(CaError::TypeMismatch(name.into())),
                    };
                    self.bits[idx] = bit;
                    self.bits_to_val();
                } else {
                    return Err(CaError::FieldNotFound(name.to_string()));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::recgbl::{alarm_status, rec_gbl_check_udf};
    use crate::server::record::{AlarmSeverity, CommonFields};

    /// C `mbboDirectRecord.c:191` — the one base record that raises UDF with a
    /// bespoke message. The trait seam must carry it so the framework's central
    /// `rec_gbl_check_udf` sets `namsg = "UDFS"`, which pvxs prefers as
    /// `alarm.message`. The generic default is `""` (plain `recGblSetSevr`).
    #[test]
    fn mbbo_direct_udf_alarm_message_is_udfs() {
        let rec = MbboDirectRecord::default();
        assert_eq!(rec.udf_alarm_message(), "UDFS");

        // Feeding the seam through the central helper sets namsg = "UDFS".
        let mut common = CommonFields::default();
        assert!(common.udf != 0, "a fresh record is undefined");
        rec_gbl_check_udf(
            &mut common,
            rec.udf_alarm_on_exact_one(),
            rec.udf_alarm_message(),
        );
        assert_eq!(common.nsta, alarm_status::UDF_ALARM);
        assert_eq!(common.nsev, AlarmSeverity::from_u16(common.udfs as u16));
        assert_eq!(common.namsg, "UDFS");
    }
}
