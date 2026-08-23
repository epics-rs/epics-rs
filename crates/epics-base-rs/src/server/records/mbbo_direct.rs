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
    /// C `mbboDirectRecord.c:160` — `prec->mlst = prec->val`. MLST is
    /// `special(SPC_NOMOD)` so it has no `put_field` arm for the trait default
    /// to go through; the record seeds its own cell.
    fn seed_deadband_tracking(&mut self) {
        self.mlst = self.val;
    }

    fn record_type(&self) -> &'static str {
        "mbboDirect"
    }

    /// C `mbboDirectRecord.c:176-190`: the scalar `dbGetLink(&prec->dol, ..., &prec->val, 0, 0)`
    /// under `dol.type != CONSTANT && omsl == menuOmslclosed_loop`.
    fn fetches_dol_closed_loop(&self) -> bool {
        true
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

    /// C `mbboDirectRecord.c::special` (263-269), the pre-store pass:
    ///
    /// ```c
    ///     if(after==0 && fieldIndex >= mbboDirectRecordB0
    ///                 && fieldIndex <= mbboDirectRecordB1F) {
    ///         if(prec->omsl == menuOmslclosed_loop) {
    ///             return S_db_noMod;
    ///         }
    ///     }
    /// ```
    ///
    /// `dbPut` propagates a non-zero pass-0 status straight out before the
    /// store (`dbAccess.c:1350-1352`), so the bit never lands and `bitsToVAL`
    /// never runs — VAL keeps the setpoint DOL is driving. Without this a
    /// `caput M.B3 1` on a closed-loop record moved VAL until the next process
    /// cycle overwrote it, which is the confusion C is refusing to allow.
    ///
    /// Only the pass-0 arm is a refusal. The pass-1 arm (`:271-291`, the bit
    /// into VAL plus OBIT and `convert`) is the framework's `put_field` +
    /// `bits_to_val`, which is why this hook has nothing to do when `after`.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after && self.omsl == 1 && BIT_NAMES.contains(&field) {
            return Err(CaError::ReadOnlyField(field.to_string()));
        }
        Ok(())
    }

    /// C `mbboDirectRecord.c:184-186` — a failed closed-loop DOL read takes
    /// `goto CONTINUE`, jumping past `prec->udf = FALSE`, `bitsFromVAL(prec)`,
    /// `convert(prec)` and the pre-output `recGblGetTimeStampSimm`. Suppressing
    /// the convert holds RVAL and the B0..B1F bit fields at what the last
    /// successful read produced.
    fn closed_loop_dol_read_failed(&mut self) {
        self.skip_convert = true;
    }

    /// C `mbboDirectRecord.c:190-202` — the `else if (prec->udf) goto CONTINUE`
    /// skips the pre-output `recGblGetTimeStampSimm` (mbboDirectRecord.c:202);
    /// the only post-`CONTINUE:` stamp is `if (pact)`-guarded
    /// (mbboDirectRecord.c:234-236), so a soft (sync) UDF mbboDirect never
    /// stamps TIME — it stays at the EPICS epoch until VAL is defined. Opts
    /// mbboDirect into the framework's undefined timestamp-skip.
    ///
    /// NOTE: unlike mbbo, mbboDirect does NOT override
    /// `skips_forward_convert_when_undefined` (its VAL is bit-derived, so the
    /// convert is not suppressed). The timestamp-skip is a SEPARATE hook; the
    /// two are not tied together.
    fn skips_timestamp_when_undefined(&self) -> bool {
        true
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

    /// epics-base PR `dabcf89` (2021), C `mbboDirectRecord.c:144-157`: when
    /// the record initialises with no VAL set (UDF=true) but the operator
    /// populated B0..B1F bits in the .db file, fold those bits into VAL and
    /// clear UDF.
    ///
    /// The `!prec->udf` arm of the same `if` — `bitsFromVAL(prec)` — is NOT
    /// here: it must observe the constant-DOL seed, and the seed owner runs
    /// after this hook. It lives in [`Self::seed_deadband_tracking`], the
    /// post-seed tail, where C's own `bitsFromVAL` sits relative to the seed.
    fn post_init_finalize_undef(&mut self, common_udf: &mut bool) -> CaResult<()> {
        if *common_udf && self.bits.iter().any(|&b| b != 0) {
            self.bits_to_val();
            *common_udf = false;
        }
        Ok(())
    }

    /// C `mbboDirectRecord.c:119-120`:
    /// `if (recGblInitConstantLink(&prec->dol, DBF_ULONG, &prec->val))
    ///      prec->udf = FALSE;`
    /// The framework gate (`processing.rs`) excludes a constant DOL from the
    /// per-cycle closed-loop fetch (C `!dbLinkIsConstant`), so the init-seed
    /// owner is the only place a constant DOL can reach VAL.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        vec![crate::server::record::ConstantInitLink::dol_to_val(
            "DOL", "VAL",
        )]
    }

    /// C `mbboDirectRecord.c:142-143,160-162` — the init tail, run right after
    /// the constant load: `bitsFromVAL(prec)` re-derives B0..B1F from whatever
    /// VAL now holds, then `mlst = val; oraw = rval; orbv = rbv`. C runs
    /// `bitsFromVAL` only on the `!udf` arm, but every path that leaves UDF set
    /// also leaves VAL and the bits both zero, so the unconditional form is the
    /// same derivation. No `convert()`: C does not translate VAL to RVAL at
    /// init on this record.
    fn seed_deadband_tracking(&mut self) {
        self.val_to_bits();
        self.mlst = self.val;
        self.oraw = self.rval;
        self.orbv = self.rbv;
    }

    /// C `mbboDirectRecord.c::process` (line 198) calls `convert(prec)`
    /// UNCONDITIONALLY on every non-pact process — the VAL→RVAL output
    /// translation. A CA put to `mbboDirect.VAL` must therefore
    /// recompute `RVAL`/`ORAW`. `mbboDirect` is an output record, so it
    /// does NOT override `set_device_did_compute` or
    /// `soft_channel_skips_convert` — the soft-channel convert-skip
    /// applies only to INPUT records.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `mbboDirectRecord.c:342-349` is the whole convert:
        //
        //     prec->rval = prec->val;
        //     if (prec->shft > 0) prec->rval <<= prec->shft;
        //
        // MASK is not read. It belongs to device support, which positions it
        // (`prec->mask <<= prec->shft`, devMbboDirectSoftRaw.c:31) and applies
        // it on the way OUT (`data = prec->rval & prec->mask`, `:40`) — so the
        // record's RVAL stays the full shifted value and only the wire word is
        // trimmed. Masking here with the record's own UNSHIFTED NOBT mask
        // cleared exactly the bits the shift had just placed.
        //
        // Skipped when a device readback (`apply_raw_readback`) already set
        // both RVAL and VAL, and when a failed closed-loop DOL read took C's
        // `goto CONTINUE` (`closed_loop_dol_read_failed`).
        if !self.skip_convert {
            let mut raw = self.val;
            if self.shft > 0 {
                raw = raw.wrapping_shl(self.shft as u32);
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
            rec.udf_alarm_severity(),
            rec.udf_alarm_message(),
        );
        assert_eq!(common.nsta, alarm_status::UDF_ALARM);
        assert_eq!(common.nsev, AlarmSeverity::from_u16(common.udfs as u16));
        assert_eq!(common.namsg, "UDFS");
    }
}
