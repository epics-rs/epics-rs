use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, MENU_SIMM, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

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

fn bit_field_descs() -> &'static [FieldDesc] {
    macro_rules! bf {
        ($name:literal) => {
            FieldDesc {
                name: $name,
                dbf_type: DbFieldType::Char,
                read_only: false,
            }
        };
    }
    static BITS: [FieldDesc; 32] = [
        bf!("B0"),
        bf!("B1"),
        bf!("B2"),
        bf!("B3"),
        bf!("B4"),
        bf!("B5"),
        bf!("B6"),
        bf!("B7"),
        bf!("B8"),
        bf!("B9"),
        bf!("BA"),
        bf!("BB"),
        bf!("BC"),
        bf!("BD"),
        bf!("BE"),
        bf!("BF"),
        bf!("B10"),
        bf!("B11"),
        bf!("B12"),
        bf!("B13"),
        bf!("B14"),
        bf!("B15"),
        bf!("B16"),
        bf!("B17"),
        bf!("B18"),
        bf!("B19"),
        bf!("B1A"),
        bf!("B1B"),
        bf!("B1C"),
        bf!("B1D"),
        bf!("B1E"),
        bf!("B1F"),
    ];
    &BITS
}

static MBBO_DIRECT_HEAD_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "RVAL",
        dbf_type: DbFieldType::ULong,
        read_only: false,
    },
    FieldDesc {
        name: "ORAW",
        dbf_type: DbFieldType::ULong,
        read_only: true,
    },
    FieldDesc {
        name: "RBV",
        dbf_type: DbFieldType::ULong,
        read_only: true,
    },
    FieldDesc {
        name: "ORBV",
        dbf_type: DbFieldType::ULong,
        read_only: true,
    },
    FieldDesc {
        name: "MASK",
        dbf_type: DbFieldType::ULong,
        read_only: false,
    },
    FieldDesc {
        name: "SHFT",
        dbf_type: DbFieldType::UShort,
        read_only: false,
    },
    FieldDesc {
        name: "NOBT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "IVOA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVOV",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "OMSL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DOL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIMM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SIML",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIOL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIMS",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

fn mbbo_direct_fields() -> &'static [FieldDesc] {
    use std::sync::OnceLock;
    static ALL: OnceLock<Vec<FieldDesc>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut v: Vec<FieldDesc> = MBBO_DIRECT_HEAD_FIELDS.to_vec();
        v.extend_from_slice(bit_field_descs());
        v
    })
}

impl Record for MbboDirectRecord {
    fn record_type(&self) -> &'static str {
        "mbboDirect"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        mbbo_direct_fields()
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
            _ => BIT_NAMES
                .iter()
                .position(|&n| n == name)
                .map(|idx| EpicsValue::Char(self.bits[idx])),
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
            _ => {
                if let Some(idx) = BIT_NAMES.iter().position(|&n| n == name) {
                    let bit = match value {
                        EpicsValue::Char(v) => v & 1,
                        EpicsValue::Short(v) => (v & 1) as u8,
                        EpicsValue::Long(v) => (v & 1) as u8,
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
