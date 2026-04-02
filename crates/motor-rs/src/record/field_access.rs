#![allow(unused_imports)]
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::FieldDesc;
use epics_base_rs::types::{DbFieldType, EpicsValue};

use crate::coordinate;
use crate::fields::*;
use crate::flags::*;

use super::MotorRecord;

pub(crate) static FIELDS: &[FieldDesc] = &[
    // Position
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "RBV",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "RLV",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "OFF",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DIFF",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "RDIF",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "DVAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DRBV",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "RVAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "RRBV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "RMP",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "REP",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    // Conversion
    FieldDesc {
        name: "DIR",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "FOFF",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SET",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IGSET",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "MRES",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ERES",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SREV",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "UREV",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "UEIP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "URIP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "RRES",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // Velocity
    FieldDesc {
        name: "VELO",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "VBAS",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "VMAX",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "S",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SBAS",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SMAX",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ACCL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "BVEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "BACC",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HVEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "JVEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "JAR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SBAK",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // Retry
    FieldDesc {
        name: "BDST",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "FRAC",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "RDBD",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SPDB",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "RTRY",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "RMOD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "RCNT",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "MISS",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    // Limits
    FieldDesc {
        name: "HLM",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LLM",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DHLM",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DLLM",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LVIO",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "HLS",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "LLS",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "HLSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // Control
    FieldDesc {
        name: "SPMG",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "STOP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HOMF",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HOMR",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "JOGF",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "JOGR",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TWF",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TWR",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TWV",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "CNEN",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // Status
    FieldDesc {
        name: "DMOV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "MOVN",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "MSTA",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "MIP",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "CDIR",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "TDIR",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "ATHM",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "STUP",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // PID
    FieldDesc {
        name: "PCOF",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ICOF",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DCOF",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // Display
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ADEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "MDEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // Timing
    FieldDesc {
        name: "DLY",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "NTM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "NTMF",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
];

pub(crate) fn motor_get_field(rec: &MotorRecord, name: &str) -> Option<EpicsValue> {
    match name {
        // Position
        "VAL" => Some(EpicsValue::Double(rec.pos.val)),
        "RBV" => Some(EpicsValue::Double(rec.pos.rbv)),
        "RLV" => Some(EpicsValue::Double(rec.pos.rlv)),
        "OFF" => Some(EpicsValue::Double(rec.pos.off)),
        "DIFF" => Some(EpicsValue::Double(rec.pos.diff)),
        "RDIF" => Some(EpicsValue::Double(rec.pos.rdif)),
        "DVAL" => Some(EpicsValue::Double(rec.pos.dval)),
        "DRBV" => Some(EpicsValue::Double(rec.pos.drbv)),
        "RVAL" => Some(EpicsValue::Long(rec.pos.rval)),
        "RRBV" => Some(EpicsValue::Long(rec.pos.rrbv)),
        "RMP" => Some(EpicsValue::Long(rec.pos.rmp)),
        "REP" => Some(EpicsValue::Long(rec.pos.rep)),
        // Conversion
        "DIR" => Some(EpicsValue::Short(rec.conv.dir as i16)),
        "FOFF" => Some(EpicsValue::Short(rec.conv.foff as i16)),
        "SET" => Some(EpicsValue::Short(if rec.conv.set { 1 } else { 0 })),
        "IGSET" => Some(EpicsValue::Short(if rec.conv.igset { 1 } else { 0 })),
        "MRES" => Some(EpicsValue::Double(rec.conv.mres)),
        "ERES" => Some(EpicsValue::Double(rec.conv.eres)),
        "SREV" => Some(EpicsValue::Long(rec.conv.srev)),
        "UREV" => Some(EpicsValue::Double(rec.conv.urev)),
        "UEIP" => Some(EpicsValue::Short(if rec.conv.ueip { 1 } else { 0 })),
        "URIP" => Some(EpicsValue::Short(if rec.conv.urip { 1 } else { 0 })),
        "RRES" => Some(EpicsValue::Double(rec.conv.rres)),
        // Velocity
        "VELO" => Some(EpicsValue::Double(rec.vel.velo)),
        "VBAS" => Some(EpicsValue::Double(rec.vel.vbas)),
        "VMAX" => Some(EpicsValue::Double(rec.vel.vmax)),
        "S" => Some(EpicsValue::Double(rec.vel.s)),
        "SBAS" => Some(EpicsValue::Double(rec.vel.sbas)),
        "SMAX" => Some(EpicsValue::Double(rec.vel.smax)),
        "ACCL" => Some(EpicsValue::Double(rec.vel.accl)),
        "BVEL" => Some(EpicsValue::Double(rec.vel.bvel)),
        "BACC" => Some(EpicsValue::Double(rec.vel.bacc)),
        "HVEL" => Some(EpicsValue::Double(rec.vel.hvel)),
        "JVEL" => Some(EpicsValue::Double(rec.vel.jvel)),
        "JAR" => Some(EpicsValue::Double(rec.vel.jar)),
        "SBAK" => Some(EpicsValue::Double(rec.vel.sbak)),
        // Retry
        "BDST" => Some(EpicsValue::Double(rec.retry.bdst)),
        "FRAC" => Some(EpicsValue::Double(rec.retry.frac)),
        "RDBD" => Some(EpicsValue::Double(rec.retry.rdbd)),
        "SPDB" => Some(EpicsValue::Double(rec.retry.spdb)),
        "RTRY" => Some(EpicsValue::Short(rec.retry.rtry)),
        "RMOD" => Some(EpicsValue::Short(rec.retry.rmod as i16)),
        "RCNT" => Some(EpicsValue::Short(rec.retry.rcnt)),
        "MISS" => Some(EpicsValue::Short(if rec.retry.miss { 1 } else { 0 })),
        // Limits
        "HLM" => Some(EpicsValue::Double(rec.limits.hlm)),
        "LLM" => Some(EpicsValue::Double(rec.limits.llm)),
        "DHLM" => Some(EpicsValue::Double(rec.limits.dhlm)),
        "DLLM" => Some(EpicsValue::Double(rec.limits.dllm)),
        "LVIO" => Some(EpicsValue::Short(if rec.limits.lvio { 1 } else { 0 })),
        "HLS" => Some(EpicsValue::Short(if rec.limits.hls { 1 } else { 0 })),
        "LLS" => Some(EpicsValue::Short(if rec.limits.lls { 1 } else { 0 })),
        "HLSV" => Some(EpicsValue::Short(rec.limits.hlsv)),
        // Control
        "SPMG" => Some(EpicsValue::Short(rec.ctrl.spmg as i16)),
        "STOP" => Some(EpicsValue::Short(if rec.ctrl.stop { 1 } else { 0 })),
        "HOMF" => Some(EpicsValue::Short(if rec.ctrl.homf { 1 } else { 0 })),
        "HOMR" => Some(EpicsValue::Short(if rec.ctrl.homr { 1 } else { 0 })),
        "JOGF" => Some(EpicsValue::Short(if rec.ctrl.jogf { 1 } else { 0 })),
        "JOGR" => Some(EpicsValue::Short(if rec.ctrl.jogr { 1 } else { 0 })),
        "TWF" => Some(EpicsValue::Short(if rec.ctrl.twf { 1 } else { 0 })),
        "TWR" => Some(EpicsValue::Short(if rec.ctrl.twr { 1 } else { 0 })),
        "TWV" => Some(EpicsValue::Double(rec.ctrl.twv)),
        "CNEN" => Some(EpicsValue::Short(if rec.ctrl.cnen { 1 } else { 0 })),
        // Status
        "DMOV" => Some(EpicsValue::Short(if rec.stat.dmov { 1 } else { 0 })),
        "MOVN" => Some(EpicsValue::Short(if rec.stat.movn { 1 } else { 0 })),
        "MSTA" => Some(EpicsValue::Long(rec.stat.msta.bits() as i32)),
        "MIP" => Some(EpicsValue::Short(rec.stat.mip.bits() as i16)),
        "CDIR" => Some(EpicsValue::Short(if rec.stat.cdir { 1 } else { 0 })),
        "TDIR" => Some(EpicsValue::Short(if rec.stat.tdir { 1 } else { 0 })),
        "ATHM" => Some(EpicsValue::Short(if rec.stat.athm { 1 } else { 0 })),
        "STUP" => Some(EpicsValue::Short(rec.stat.stup)),
        // PID
        "PCOF" => Some(EpicsValue::Double(rec.pid.pcof)),
        "ICOF" => Some(EpicsValue::Double(rec.pid.icof)),
        "DCOF" => Some(EpicsValue::Double(rec.pid.dcof)),
        // Display
        "EGU" => Some(EpicsValue::String(rec.disp.egu.clone())),
        "PREC" => Some(EpicsValue::Short(rec.disp.prec)),
        "ADEL" => Some(EpicsValue::Double(rec.disp.adel)),
        "MDEL" => Some(EpicsValue::Double(rec.disp.mdel)),
        // Timing
        "DLY" => Some(EpicsValue::Double(rec.timing.dly)),
        "NTM" => Some(EpicsValue::Short(if rec.timing.ntm { 1 } else { 0 })),
        "NTMF" => Some(EpicsValue::Double(rec.timing.ntmf)),
        _ => None,
    }
}

pub(crate) fn motor_put_field(
    rec: &mut MotorRecord,
    name: &str,
    value: EpicsValue,
) -> CaResult<()> {
    match name {
        // Position writes -- cascade and set command source
        "VAL" => {
            let v = match value {
                EpicsValue::Double(v) => v,
                _ => return Err(CaError::TypeMismatch(name.into())),
            };
            if rec.conv.set && !rec.conv.igset {
                // SET mode: recalculate offset, signal SetPosition
                if let Ok((dval, rval, off)) = coordinate::cascade_from_val(
                    v,
                    rec.conv.dir,
                    rec.pos.off,
                    rec.conv.foff,
                    rec.conv.mres,
                    true,
                    rec.pos.dval,
                ) {
                    rec.pos.val = v;
                    rec.pos.dval = dval;
                    rec.pos.rval = rval;
                    rec.pos.off = off;
                }
                rec.last_write = Some(CommandSource::Set);
            } else {
                if let Ok((dval, rval, off)) = coordinate::cascade_from_val(
                    v,
                    rec.conv.dir,
                    rec.pos.off,
                    rec.conv.foff,
                    rec.conv.mres,
                    false,
                    rec.pos.dval,
                ) {
                    rec.pos.val = v;
                    rec.pos.dval = dval;
                    rec.pos.rval = rval;
                    rec.pos.off = off;
                }
                rec.last_write = Some(CommandSource::Val);
            }
            Ok(())
        }
        "DVAL" => {
            let v = match value {
                EpicsValue::Double(v) => v,
                _ => return Err(CaError::TypeMismatch(name.into())),
            };
            if rec.conv.set && !rec.conv.igset {
                if let Ok((val, rval, off)) = coordinate::cascade_from_dval(
                    v,
                    rec.conv.dir,
                    rec.pos.off,
                    rec.conv.foff,
                    rec.conv.mres,
                    true,
                    rec.pos.val,
                ) {
                    rec.pos.dval = v;
                    rec.pos.val = val;
                    rec.pos.rval = rval;
                    rec.pos.off = off;
                }
                rec.last_write = Some(CommandSource::Set);
            } else {
                if let Ok((val, rval, off)) = coordinate::cascade_from_dval(
                    v,
                    rec.conv.dir,
                    rec.pos.off,
                    rec.conv.foff,
                    rec.conv.mres,
                    false,
                    rec.pos.val,
                ) {
                    rec.pos.dval = v;
                    rec.pos.val = val;
                    rec.pos.rval = rval;
                    rec.pos.off = off;
                }
                rec.last_write = Some(CommandSource::Dval);
            }
            Ok(())
        }
        "RVAL" => {
            let v = match value {
                EpicsValue::Long(v) => v,
                _ => return Err(CaError::TypeMismatch(name.into())),
            };
            if rec.conv.set && !rec.conv.igset {
                let (val, dval, off) = coordinate::cascade_from_rval(
                    v,
                    rec.conv.dir,
                    rec.pos.off,
                    rec.conv.foff,
                    rec.conv.mres,
                    true,
                    rec.pos.val,
                );
                rec.pos.rval = v;
                rec.pos.val = val;
                rec.pos.dval = dval;
                rec.pos.off = off;
                rec.last_write = Some(CommandSource::Set);
            } else {
                let (val, dval, off) = coordinate::cascade_from_rval(
                    v,
                    rec.conv.dir,
                    rec.pos.off,
                    rec.conv.foff,
                    rec.conv.mres,
                    false,
                    rec.pos.val,
                );
                rec.pos.rval = v;
                rec.pos.val = val;
                rec.pos.dval = dval;
                rec.pos.off = off;
                rec.last_write = Some(CommandSource::Rval);
            }
            Ok(())
        }
        "RLV" => {
            let v = match value {
                EpicsValue::Double(v) => v,
                _ => return Err(CaError::TypeMismatch(name.into())),
            };
            rec.pos.rlv = v;
            rec.last_write = Some(CommandSource::Rlv);
            Ok(())
        }
        "OFF" => {
            match value {
                EpicsValue::Double(v) => {
                    rec.pos.off = v;
                    // Recalculate user coords from dial
                    rec.pos.val = coordinate::dial_to_user(rec.pos.dval, rec.conv.dir, rec.pos.off);
                    rec.pos.rbv = coordinate::dial_to_user(rec.pos.drbv, rec.conv.dir, rec.pos.off);
                    let (hlm, llm) = coordinate::dial_limits_to_user(
                        rec.limits.dhlm,
                        rec.limits.dllm,
                        rec.conv.dir,
                        rec.pos.off,
                    );
                    rec.limits.hlm = hlm;
                    rec.limits.llm = llm;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            }
        }
        // Conversion
        "DIR" => {
            match value {
                EpicsValue::Short(v) => {
                    rec.conv.dir = MotorDir::from_i16(v);
                    // Recalculate user coords from dial using new direction
                    rec.pos.val = coordinate::dial_to_user(rec.pos.dval, rec.conv.dir, rec.pos.off);
                    rec.pos.rbv = coordinate::dial_to_user(rec.pos.drbv, rec.conv.dir, rec.pos.off);
                    let (hlm, llm) = coordinate::dial_limits_to_user(
                        rec.limits.dhlm,
                        rec.limits.dllm,
                        rec.conv.dir,
                        rec.pos.off,
                    );
                    rec.limits.hlm = hlm;
                    rec.limits.llm = llm;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            }
        }
        "FOFF" => match value {
            EpicsValue::Short(v) => {
                rec.conv.foff = FreezeOffset::from_i16(v);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SET" => match value {
            EpicsValue::Short(v) => {
                rec.conv.set = v != 0;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "IGSET" => match value {
            EpicsValue::Short(v) => {
                rec.conv.igset = v != 0;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "MRES" => match value {
            EpicsValue::Double(v) => {
                rec.conv.mres = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ERES" => match value {
            EpicsValue::Double(v) => {
                rec.conv.eres = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SREV" => match value {
            EpicsValue::Long(v) => {
                rec.conv.srev = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "UREV" => match value {
            EpicsValue::Double(v) => {
                rec.conv.urev = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "UEIP" => match value {
            EpicsValue::Short(v) => {
                rec.conv.ueip = v != 0;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "URIP" => match value {
            EpicsValue::Short(v) => {
                rec.conv.urip = v != 0;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "RRES" => match value {
            EpicsValue::Double(v) => {
                rec.conv.rres = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Velocity
        "VELO" => match value {
            EpicsValue::Double(v) => {
                rec.vel.velo = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "VBAS" => match value {
            EpicsValue::Double(v) => {
                rec.vel.vbas = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "VMAX" => match value {
            EpicsValue::Double(v) => {
                rec.vel.vmax = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "S" => match value {
            EpicsValue::Double(v) => {
                rec.vel.s = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SBAS" => match value {
            EpicsValue::Double(v) => {
                rec.vel.sbas = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SMAX" => match value {
            EpicsValue::Double(v) => {
                rec.vel.smax = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ACCL" => match value {
            EpicsValue::Double(v) => {
                rec.vel.accl = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "BVEL" => match value {
            EpicsValue::Double(v) => {
                rec.vel.bvel = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "BACC" => match value {
            EpicsValue::Double(v) => {
                rec.vel.bacc = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "HVEL" => match value {
            EpicsValue::Double(v) => {
                rec.vel.hvel = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "JVEL" => match value {
            EpicsValue::Double(v) => {
                rec.vel.jvel = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "JAR" => match value {
            EpicsValue::Double(v) => {
                rec.vel.jar = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SBAK" => match value {
            EpicsValue::Double(v) => {
                rec.vel.sbak = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Retry
        "BDST" => match value {
            EpicsValue::Double(v) => {
                rec.retry.bdst = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "FRAC" => match value {
            EpicsValue::Double(v) => {
                rec.retry.frac = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "RDBD" => match value {
            EpicsValue::Double(v) => {
                rec.retry.rdbd = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SPDB" => match value {
            EpicsValue::Double(v) => {
                rec.retry.spdb = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "RTRY" => match value {
            EpicsValue::Short(v) => {
                rec.retry.rtry = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "RMOD" => match value {
            EpicsValue::Short(v) => {
                rec.retry.rmod = RetryMode::from_i16(v);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Limits
        "HLM" => match value {
            EpicsValue::Double(v) => {
                rec.limits.hlm = v;
                let (dhlm, dllm) = coordinate::user_limits_to_dial(
                    rec.limits.hlm,
                    rec.limits.llm,
                    rec.conv.dir,
                    rec.pos.off,
                );
                rec.limits.dhlm = dhlm;
                rec.limits.dllm = dllm;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "LLM" => match value {
            EpicsValue::Double(v) => {
                rec.limits.llm = v;
                let (dhlm, dllm) = coordinate::user_limits_to_dial(
                    rec.limits.hlm,
                    rec.limits.llm,
                    rec.conv.dir,
                    rec.pos.off,
                );
                rec.limits.dhlm = dhlm;
                rec.limits.dllm = dllm;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "DHLM" => match value {
            EpicsValue::Double(v) => {
                rec.limits.dhlm = v;
                let (hlm, llm) = coordinate::dial_limits_to_user(
                    rec.limits.dhlm,
                    rec.limits.dllm,
                    rec.conv.dir,
                    rec.pos.off,
                );
                rec.limits.hlm = hlm;
                rec.limits.llm = llm;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "DLLM" => match value {
            EpicsValue::Double(v) => {
                rec.limits.dllm = v;
                let (hlm, llm) = coordinate::dial_limits_to_user(
                    rec.limits.dhlm,
                    rec.limits.dllm,
                    rec.conv.dir,
                    rec.pos.off,
                );
                rec.limits.hlm = hlm;
                rec.limits.llm = llm;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "HLSV" => match value {
            EpicsValue::Short(v) => {
                rec.limits.hlsv = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Control
        "SPMG" => match value {
            EpicsValue::Short(v) => {
                rec.ctrl.spmg = SpmgMode::from_i16(v);
                rec.last_write = Some(CommandSource::Spmg);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "STOP" => match value {
            EpicsValue::Short(v) => {
                if v != 0 {
                    rec.ctrl.stop = true;
                    rec.last_write = Some(CommandSource::Stop);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "HOMF" => match value {
            EpicsValue::Short(v) => {
                if v != 0 {
                    rec.ctrl.homf = true;
                    rec.last_write = Some(CommandSource::Homf);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "HOMR" => match value {
            EpicsValue::Short(v) => {
                if v != 0 {
                    rec.ctrl.homr = true;
                    rec.last_write = Some(CommandSource::Homr);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "JOGF" => match value {
            EpicsValue::Short(v) => {
                rec.ctrl.jogf = v != 0;
                rec.last_write = Some(CommandSource::Jogf);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "JOGR" => match value {
            EpicsValue::Short(v) => {
                rec.ctrl.jogr = v != 0;
                rec.last_write = Some(CommandSource::Jogr);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "TWF" => match value {
            EpicsValue::Short(v) => {
                if v != 0 {
                    rec.ctrl.twf = true;
                    rec.last_write = Some(CommandSource::Twf);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "TWR" => match value {
            EpicsValue::Short(v) => {
                if v != 0 {
                    rec.ctrl.twr = true;
                    rec.last_write = Some(CommandSource::Twr);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "TWV" => match value {
            EpicsValue::Double(v) => {
                rec.ctrl.twv = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "CNEN" => match value {
            EpicsValue::Short(v) => {
                rec.ctrl.cnen = v != 0;
                rec.last_write = Some(CommandSource::Cnen);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Status (read-only handled by validate_put)
        "STUP" => match value {
            EpicsValue::Short(v) => {
                rec.stat.stup = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // PID
        "PCOF" => match value {
            EpicsValue::Double(v) => {
                rec.pid.pcof = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ICOF" => match value {
            EpicsValue::Double(v) => {
                rec.pid.icof = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "DCOF" => match value {
            EpicsValue::Double(v) => {
                rec.pid.dcof = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Display
        "EGU" => match value {
            EpicsValue::String(v) => {
                rec.disp.egu = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "PREC" => match value {
            EpicsValue::Short(v) => {
                rec.disp.prec = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ADEL" => match value {
            EpicsValue::Double(v) => {
                rec.disp.adel = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "MDEL" => match value {
            EpicsValue::Double(v) => {
                rec.disp.mdel = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Timing
        "DLY" => match value {
            EpicsValue::Double(v) => {
                rec.timing.dly = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "NTM" => match value {
            EpicsValue::Short(v) => {
                rec.timing.ntm = v != 0;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "NTMF" => match value {
            EpicsValue::Double(v) => {
                rec.timing.ntmf = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Sync
        "SYNC" => {
            rec.last_write = Some(CommandSource::Sync);
            Ok(())
        }
        _ => Err(CaError::FieldNotFound(name.into())),
    }
}
