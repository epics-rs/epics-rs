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
        dbf_type: DbFieldType::Int64,
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
        dbf_type: DbFieldType::Int64,
        read_only: false,
    },
    FieldDesc {
        name: "RRBV",
        dbf_type: DbFieldType::Int64,
        read_only: true,
    },
    FieldDesc {
        name: "RMP",
        dbf_type: DbFieldType::Int64,
        read_only: true,
    },
    FieldDesc {
        name: "REP",
        dbf_type: DbFieldType::Int64,
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
    FieldDesc {
        name: "RDBL_VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "RSTM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LOADPOS_BLOCK",
        dbf_type: DbFieldType::Short,
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
        name: "ACCS",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ACCU",
        dbf_type: DbFieldType::Short,
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
        name: "RHLM",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "RLLM",
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
    FieldDesc {
        name: "RVEL",
        dbf_type: DbFieldType::Int64,
        read_only: true,
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
    // Sync trigger (write-only semantic; C: 82c26005)
    FieldDesc {
        name: "SYNC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // Position-compare output (C: 05b25c1d, PR #248)
    FieldDesc {
        name: "PCO_START",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PCO_END",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PCO_INC",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PCO_PW",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PCO_ENABLE",
        dbf_type: DbFieldType::Short,
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
    // Public C motorRecord.dbd link / menu surface (motorRecord.dbd:233-265,
    // 739-760). DBF_INLINK/DBF_OUTLINK fields appear over CA as DBF_STRING (the
    // link specification); DBF_MENU OMSL appears as the menuOmsl index.
    FieldDesc {
        name: "OUT",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "RDBL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DOL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "RLNK",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STOO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DINP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "RINP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "POST",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OMSL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // Public C motorRecord.dbd alarm-limit / operator-range surface
    // (motorRecord.dbd:370-441). HHSV/LLSV are menuAlarmSevr indices.
    FieldDesc {
        name: "HIHI",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HIGH",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOW",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOLO",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HHSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LLSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // Public C motorRecord.dbd last-value / monitor-map surface
    // (motorRecord.dbd:560-595, 675-682, 829-836). All SPC_NOMOD → read-only.
    FieldDesc {
        name: "LVAL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LDVL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LRVL",
        dbf_type: DbFieldType::Int64,
        read_only: true,
    },
    FieldDesc {
        name: "LRLV",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "ALST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "MMAP",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "NMAP",
        dbf_type: DbFieldType::Long,
        read_only: true,
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
        "RDIF" => Some(EpicsValue::Int64(rec.pos.rdif)),
        "DVAL" => Some(EpicsValue::Double(rec.pos.dval)),
        "DRBV" => Some(EpicsValue::Double(rec.pos.drbv)),
        "RVAL" => Some(EpicsValue::Int64(rec.pos.rval)),
        "RRBV" => Some(EpicsValue::Int64(rec.pos.rrbv)),
        "RMP" => Some(EpicsValue::Int64(rec.pos.rmp)),
        "REP" => Some(EpicsValue::Int64(rec.pos.rep)),
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
        "RDBL_VAL" => Some(EpicsValue::Double(rec.conv.rdbl_value.unwrap_or(0.0))),
        "RSTM" => Some(EpicsValue::Short(rec.conv.rstm as i16)),
        "LOADPOS_BLOCK" => Some(EpicsValue::Short(if rec.conv.loadpos_blocked {
            1
        } else {
            0
        })),
        // Velocity
        "VELO" => Some(EpicsValue::Double(rec.vel.velo)),
        "VBAS" => Some(EpicsValue::Double(rec.vel.vbas)),
        "VMAX" => Some(EpicsValue::Double(rec.vel.vmax)),
        "S" => Some(EpicsValue::Double(rec.vel.s)),
        "SBAS" => Some(EpicsValue::Double(rec.vel.sbas)),
        "SMAX" => Some(EpicsValue::Double(rec.vel.smax)),
        "ACCL" => Some(EpicsValue::Double(rec.vel.accl)),
        "ACCS" => Some(EpicsValue::Double(rec.vel.accs)),
        "ACCU" => Some(EpicsValue::Short(rec.vel.accu as i16)),
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
        "RHLM" => Some(EpicsValue::Double(rec.limits.rhlm)),
        "RLLM" => Some(EpicsValue::Double(rec.limits.rllm)),
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
        "RVEL" => Some(EpicsValue::Int64(rec.stat.rvel)),
        // PID
        "PCOF" => Some(EpicsValue::Double(rec.pid.pcof)),
        "ICOF" => Some(EpicsValue::Double(rec.pid.icof)),
        "DCOF" => Some(EpicsValue::Double(rec.pid.dcof)),
        // Display
        "EGU" => Some(EpicsValue::String(rec.disp.egu.clone())),
        "PREC" => Some(EpicsValue::Short(rec.disp.prec)),
        "ADEL" => Some(EpicsValue::Double(rec.disp.adel)),
        "MDEL" => Some(EpicsValue::Double(rec.disp.mdel)),
        // SYNC is a write-only trigger; readback always 0.
        "SYNC" => Some(EpicsValue::Short(if rec.internal.sync { 1 } else { 0 })),
        // Position-compare output
        "PCO_START" => Some(EpicsValue::Double(rec.pco.start)),
        "PCO_END" => Some(EpicsValue::Double(rec.pco.end)),
        "PCO_INC" => Some(EpicsValue::Double(rec.pco.increment)),
        "PCO_PW" => Some(EpicsValue::Double(rec.pco.pulse_width_us)),
        "PCO_ENABLE" => Some(EpicsValue::Short(if rec.pco.enable { 1 } else { 0 })),
        // Timing
        "DLY" => Some(EpicsValue::Double(rec.timing.dly)),
        "NTM" => Some(EpicsValue::Short(if rec.timing.ntm { 1 } else { 0 })),
        "NTMF" => Some(EpicsValue::Double(rec.timing.ntmf)),
        // Public C motorRecord.dbd link / menu / string surface.
        "OUT" => Some(EpicsValue::String(rec.links.out.clone().into())),
        "RDBL" => Some(EpicsValue::String(rec.links.rdbl.clone().into())),
        "DOL" => Some(EpicsValue::String(rec.links.dol.clone().into())),
        "RLNK" => Some(EpicsValue::String(rec.links.rlnk.clone().into())),
        "STOO" => Some(EpicsValue::String(rec.links.stoo.clone().into())),
        "DINP" => Some(EpicsValue::String(rec.links.dinp.clone().into())),
        "RINP" => Some(EpicsValue::String(rec.links.rinp.clone().into())),
        "POST" => Some(EpicsValue::String(rec.links.post.clone().into())),
        "OMSL" => Some(EpicsValue::Short(rec.links.omsl)),
        // Alarm-limit / operator-range surface.
        "HIHI" => Some(EpicsValue::Double(rec.alarm.hihi)),
        "HIGH" => Some(EpicsValue::Double(rec.alarm.high)),
        "LOW" => Some(EpicsValue::Double(rec.alarm.low)),
        "LOLO" => Some(EpicsValue::Double(rec.alarm.lolo)),
        "HHSV" => Some(EpicsValue::Short(rec.alarm.hhsv)),
        "LLSV" => Some(EpicsValue::Short(rec.alarm.llsv)),
        "HOPR" => Some(EpicsValue::Double(rec.disp.hopr)),
        "LOPR" => Some(EpicsValue::Double(rec.disp.lopr)),
        // Last-value / monitor-map surface (SPC_NOMOD, read-only). LRVL mirrors
        // RVAL's 64-bit exposure; MMAP/NMAP appear as DBF_LONG over CA.
        "LVAL" => Some(EpicsValue::Double(rec.internal.lval)),
        "LDVL" => Some(EpicsValue::Double(rec.internal.ldvl)),
        "LRVL" => Some(EpicsValue::Int64(rec.internal.lrvl)),
        "LRLV" => Some(EpicsValue::Double(rec.internal.lrlv)),
        "ALST" => Some(EpicsValue::Double(rec.disp.alst)),
        "MLST" => Some(EpicsValue::Double(rec.disp.mlst)),
        "MMAP" => Some(EpicsValue::Long(rec.internal.mmap as i32)),
        "NMAP" => Some(EpicsValue::Long(rec.internal.nmap as i32)),
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
                // #231: LOAD_POS blocked — refuse the SET-mode redefinition so
                // DVAL/OFF stay consistent with the controller.
                if rec.conv.loadpos_blocked {
                    return Ok(());
                }
                if rec.conv.foff == FreezeOffset::Variable {
                    // SET+FOFF=Variable: recalculate offset, DVAL stays, SetPosition
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
                    // SET+FOFF=Frozen: cascade VAL->DVAL normally, then SetPosition
                    // C: dval = (val - off) / dir, then load_pos(dval/mres)
                    let dval = coordinate::user_to_dial(v, rec.conv.dir, rec.pos.off);
                    if let Ok(rval) = coordinate::dial_to_raw(dval, rec.conv.mres) {
                        rec.pos.val = v;
                        rec.pos.dval = dval;
                        rec.pos.rval = rval;
                    }
                    rec.last_write = Some(CommandSource::Set);
                }
            } else {
                // Normal move (not in SET mode)
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
                // #231: LOAD_POS blocked — refuse SET-mode redefinition.
                if rec.conv.loadpos_blocked {
                    return Ok(());
                }
                if rec.conv.foff == FreezeOffset::Variable {
                    // SET+FOFF=Variable: recalculate offset, signal SetPosition
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
                } else {
                    // SET+FOFF=Frozen: DVAL changes directly, SetPosition
                    if let Ok(rval) = coordinate::dial_to_raw(v, rec.conv.mres) {
                        rec.pos.dval = v;
                        rec.pos.val = coordinate::dial_to_user(v, rec.conv.dir, rec.pos.off);
                        rec.pos.rval = rval;
                    }
                }
                rec.last_write = Some(CommandSource::Set);
            } else {
                // Normal move
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
            // RVAL is 64-bit (epics-modules/motor #192). Accept Int64 or a
            // 32-bit Long from older clients.
            let v: i64 = match value {
                EpicsValue::Int64(v) => v,
                EpicsValue::Long(v) => v as i64,
                _ => return Err(CaError::TypeMismatch(name.into())),
            };
            if rec.conv.set && !rec.conv.igset {
                // #231: LOAD_POS blocked — refuse SET-mode redefinition.
                if rec.conv.loadpos_blocked {
                    return Ok(());
                }
                if rec.conv.foff == FreezeOffset::Variable {
                    // SET+FOFF=Variable: recalculate offset, signal SetPosition
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
                } else {
                    // SET+FOFF=Frozen: RVAL->DVAL directly, SetPosition
                    let dval = coordinate::raw_to_dial(v, rec.conv.mres);
                    rec.pos.rval = v;
                    rec.pos.dval = dval;
                    rec.pos.val = coordinate::dial_to_user(dval, rec.conv.dir, rec.pos.off);
                }
                rec.last_write = Some(CommandSource::Set);
            } else {
                // Normal move
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
                    // C: also update LVAL so offset change doesn't trigger false retarget
                    rec.internal.lval =
                        coordinate::dial_to_user(rec.internal.ldvl, rec.conv.dir, rec.pos.off);
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
                    // C: branch on FOFF
                    match rec.conv.foff {
                        FreezeOffset::Frozen => {
                            // FOFF=Frozen: recalculate VAL from DVAL
                            rec.pos.val =
                                coordinate::dial_to_user(rec.pos.dval, rec.conv.dir, rec.pos.off);
                        }
                        FreezeOffset::Variable => {
                            // FOFF=Variable: recalculate OFF to preserve VAL
                            rec.pos.off =
                                coordinate::calc_offset(rec.pos.val, rec.pos.dval, rec.conv.dir);
                        }
                    }
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
                if v == 0.0 {
                    return Ok(()); // C: reject zero MRES
                }
                let old_mres = rec.conv.mres;
                rec.conv.mres = v;
                // C: cascade UREV from MRES
                rec.conv.urev = v * rec.conv.srev as f64;
                apply_mres_cascade(rec, old_mres);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ERES" => match value {
            EpicsValue::Double(v) => {
                // C: if ERES==0, set to MRES
                rec.conv.eres = if v == 0.0 { rec.conv.mres } else { v };
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SREV" => match value {
            EpicsValue::Long(v) => {
                if v <= 0 {
                    return Ok(()); // C: reject non-positive SREV
                }
                let old_mres = rec.conv.mres;
                rec.conv.srev = v;
                // C: recalculate MRES from UREV/SREV
                if rec.conv.urev != 0.0 {
                    rec.conv.mres = rec.conv.urev / v as f64;
                }
                // Cascade velocity and limits like MRES handler
                apply_mres_cascade(rec, old_mres);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "UREV" => match value {
            EpicsValue::Double(v) => {
                let old_mres = rec.conv.mres;
                rec.conv.urev = v;
                // C: recalculate MRES from UREV/SREV
                if rec.conv.srev > 0 {
                    rec.conv.mres = v / rec.conv.srev as f64;
                }
                // C: cascade velocities and limits from new UREV
                apply_mres_cascade(rec, old_mres);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "UEIP" => match value {
            EpicsValue::Short(v) => {
                let ueip = v != 0;
                if ueip {
                    // C: if UEIP=Yes and encoder present, set URIP=No
                    // If no encoder present, override UEIP back to No
                    if rec.stat.msta.contains(MstaFlags::ENCODER_PRESENT) {
                        rec.conv.urip = false;
                    } else {
                        // No encoder available, cannot use UEIP
                        rec.conv.ueip = false;
                        return Ok(());
                    }
                }
                rec.conv.ueip = ueip;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "URIP" => match value {
            EpicsValue::Short(v) => {
                let urip = v != 0;
                if urip {
                    // C: if URIP=Yes and UEIP=Yes, set UEIP=No
                    rec.conv.ueip = false;
                }
                rec.conv.urip = urip;
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
        "RDBL_VAL" => match value {
            EpicsValue::Double(v) => {
                rec.conv.rdbl_value = Some(v);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "RSTM" => match value {
            EpicsValue::Short(v) => {
                rec.conv.rstm = RestoreMode::from_i16(v);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "LOADPOS_BLOCK" => match value {
            EpicsValue::Short(v) => {
                rec.conv.loadpos_blocked = v != 0;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Velocity -- C: cross-calculate EGU/s <-> rev/s pairs
        "VELO" => match value {
            EpicsValue::Double(v) => {
                rec.vel.velo = v;
                // The clamp and rev↔EGU cross-calc are C special() — runtime
                // only. During dbLoadRecords each field() lands raw (C writes
                // the struct directly); `motor_sync_speed_at_init` reconciles
                // the pairs once all fields are applied.
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:2690: VELO is clamped into [VBAS, VMAX].
                    range_check(&mut rec.vel.velo, rec.vel.vbas, rec.vel.vmax);
                    let urev_abs = rec.conv.urev.abs();
                    if urev_abs > 0.0 {
                        rec.vel.s = rec.vel.velo / urev_abs;
                    }
                }
                // C: 7291b556 — recalc ACCL/ACCS based on ACCU
                apply_accu_cascade(rec);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "VBAS" => match value {
            EpicsValue::Double(v) => {
                rec.vel.vbas = v;
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:2629-2633: VBAS is forced non-negative.
                    if rec.vel.vbas < 0.0 {
                        rec.vel.vbas = 0.0;
                    }
                    let urev_abs = rec.conv.urev.abs();
                    if urev_abs > 0.0 {
                        rec.vel.sbas = rec.vel.vbas / urev_abs;
                    }
                }
                apply_accu_cascade(rec);
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:3121-3127: a VBAS raised above VMAX
                    // drags VMAX (and SMAX) up with it.
                    if rec.vel.vmax != 0.0 && rec.vel.vbas > rec.vel.vmax {
                        rec.vel.vmax = rec.vel.vbas;
                        rec.vel.smax = rec.vel.sbas;
                    }
                    revalidate_velocities(rec);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "VMAX" => match value {
            EpicsValue::Double(v) => {
                rec.vel.vmax = v;
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:2660-2664: VMAX is forced non-negative.
                    if rec.vel.vmax < 0.0 {
                        rec.vel.vmax = 0.0;
                    }
                    let urev_abs = rec.conv.urev.abs();
                    if urev_abs > 0.0 {
                        rec.vel.smax = rec.vel.vmax / urev_abs;
                    }
                    // C motorRecord.cc:3110-3117: a VMAX dropped below VBAS
                    // drags VBAS (and SBAS) down with it.
                    if rec.vel.vmax != 0.0 && rec.vel.vmax < rec.vel.vbas {
                        rec.vel.vbas = rec.vel.vmax;
                        rec.vel.sbas = rec.vel.smax;
                    }
                    revalidate_velocities(rec);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "S" => match value {
            EpicsValue::Double(v) => {
                rec.vel.s = v;
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:2702: S is clamped into [SBAS, SMAX].
                    range_check(&mut rec.vel.s, rec.vel.sbas, rec.vel.smax);
                    let urev_abs = rec.conv.urev.abs();
                    if urev_abs > 0.0 {
                        rec.vel.velo = rec.vel.s * urev_abs;
                    }
                }
                // C motorRecord.cc:2710: ACCL/ACCS follow the VELO change.
                apply_accu_cascade(rec);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SBAS" => match value {
            EpicsValue::Double(v) => {
                rec.vel.sbas = v;
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:2644-2648: SBAS is forced non-negative.
                    if rec.vel.sbas < 0.0 {
                        rec.vel.sbas = 0.0;
                    }
                    let urev_abs = rec.conv.urev.abs();
                    if urev_abs > 0.0 {
                        rec.vel.vbas = rec.vel.sbas * urev_abs;
                    }
                }
                // C motorRecord.cc:2655: ACCL/ACCS follow the VBAS change.
                apply_accu_cascade(rec);
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:3121-3127 (shared VBAS/SBAS tail).
                    if rec.vel.vmax != 0.0 && rec.vel.vbas > rec.vel.vmax {
                        rec.vel.vmax = rec.vel.vbas;
                        rec.vel.smax = rec.vel.sbas;
                    }
                    revalidate_velocities(rec);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SMAX" => match value {
            EpicsValue::Double(v) => {
                rec.vel.smax = v;
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:2675-2679: SMAX is forced non-negative.
                    if rec.vel.smax < 0.0 {
                        rec.vel.smax = 0.0;
                    }
                    let urev_abs = rec.conv.urev.abs();
                    if urev_abs > 0.0 {
                        rec.vel.vmax = rec.vel.smax * urev_abs;
                    }
                    // C motorRecord.cc:3110-3117 (shared VMAX/SMAX tail).
                    if rec.vel.vmax != 0.0 && rec.vel.vmax < rec.vel.vbas {
                        rec.vel.vbas = rec.vel.vmax;
                        rec.vel.sbas = rec.vel.smax;
                    }
                    revalidate_velocities(rec);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ACCL" => match value {
            EpicsValue::Double(v) => {
                // C: ACCL must be > 0 (forces to 0.1 if <= 0)
                rec.vel.accl = if v <= 0.0 { 0.1 } else { v };
                // C: 36177f7b — writing ACCL switches ACCU to Accl and recalcs ACCS
                rec.vel.accu = AccsUsed::Accl;
                let span = rec.vel.velo - rec.effective_vbas();
                if rec.vel.accl > 0.0 && span > 0.0 {
                    rec.vel.accs = span / rec.vel.accl;
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ACCS" => match value {
            EpicsValue::Double(v) => {
                // C: ACCS must be > 0 (use 1.0 fallback)
                rec.vel.accs = if v <= 0.0 { 1.0 } else { v };
                // C: 36177f7b — writing ACCS switches ACCU to Accs and recalcs ACCL
                rec.vel.accu = AccsUsed::Accs;
                let span = rec.vel.velo - rec.effective_vbas();
                if rec.vel.accs > 0.0 && span > 0.0 {
                    rec.vel.accl = span / rec.vel.accs;
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ACCU" => match value {
            EpicsValue::Short(v) => {
                // C: 63bfe5d0 — ACCU is autosave/CA-writable; does not recompute.
                rec.vel.accu = AccsUsed::from_i16(v);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "BVEL" => match value {
            EpicsValue::Double(v) => {
                rec.vel.bvel = v;
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:2714: BVEL is clamped into [VBAS, VMAX].
                    range_check(&mut rec.vel.bvel, rec.vel.vbas, rec.vel.vmax);
                    let urev_abs = rec.conv.urev.abs();
                    if urev_abs > 0.0 {
                        rec.vel.sbak = rec.vel.bvel / urev_abs;
                    }
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "BACC" => match value {
            EpicsValue::Double(v) => {
                // C: BACC must be > 0 (forces to 0.1 if <= 0)
                rec.vel.bacc = if v <= 0.0 { 0.1 } else { v };
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "HVEL" => match value {
            EpicsValue::Double(v) => {
                rec.vel.hvel = v;
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:3081: HVEL is clamped into [VBAS, VMAX].
                    range_check(&mut rec.vel.hvel, rec.vel.vbas, rec.vel.vmax);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "JVEL" => match value {
            EpicsValue::Double(v) => {
                rec.vel.jvel = v;
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:3057: JVEL is clamped into [VBAS, VMAX].
                    range_check(&mut rec.vel.jvel, rec.vel.vbas, rec.vel.vmax);
                }
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
                if rec.internal.init_invariants_synced {
                    // C motorRecord.cc:2725: SBAK is clamped into [SBAS, SMAX].
                    range_check(&mut rec.vel.sbak, rec.vel.sbas, rec.vel.smax);
                    let urev_abs = rec.conv.urev.abs();
                    if urev_abs > 0.0 {
                        rec.vel.bvel = rec.vel.sbak * urev_abs;
                    }
                }
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
                // C: FRAC clamped to [0.1, 1.5]
                rec.retry.frac = v.clamp(0.1, 1.5);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "RDBD" => match value {
            EpicsValue::Double(v) => {
                rec.retry.rdbd = v;
                // C: enforceMinRetryDeadband — RDBD must be >= |MRES|.
                rec.enforce_min_retry_deadband();
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
                // C set_user_highlimit (motorRecord.cc:4076-4147): a user
                // high-limit write moves exactly ONE dial limit — DHLM
                // when DIR=Pos, DLLM when DIR=Neg — plus that side's raw
                // register. The pair is never re-ordered: writing HLM
                // below LLM leaves an inverted dial pair, which latches
                // LVIO below and blocks every move until corrected.
                let dial = coordinate::user_to_dial(v, rec.conv.dir, rec.pos.off);
                if rec.conv.dir == MotorDir::Pos {
                    rec.limits.dhlm = dial;
                    if rec.conv.mres != 0.0 {
                        rec.limits.rhlm = dial / rec.conv.mres;
                    }
                } else {
                    rec.limits.dllm = dial;
                    if rec.conv.mres != 0.0 {
                        rec.limits.rllm = dial / rec.conv.mres;
                    }
                }
                detect_inverted_limits(&mut rec.limits, rec.pos.dval);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "LLM" => match value {
            EpicsValue::Double(v) => {
                rec.limits.llm = v;
                // C set_user_lowlimit (motorRecord.cc:4155-4225): mirror
                // of HLM — DLLM when DIR=Pos, DHLM when DIR=Neg.
                let dial = coordinate::user_to_dial(v, rec.conv.dir, rec.pos.off);
                if rec.conv.dir == MotorDir::Pos {
                    rec.limits.dllm = dial;
                    if rec.conv.mres != 0.0 {
                        rec.limits.rllm = dial / rec.conv.mres;
                    }
                } else {
                    rec.limits.dhlm = dial;
                    if rec.conv.mres != 0.0 {
                        rec.limits.rhlm = dial / rec.conv.mres;
                    }
                }
                detect_inverted_limits(&mut rec.limits, rec.pos.dval);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "DHLM" => match value {
            EpicsValue::Double(v) => {
                rec.limits.dhlm = v;
                // Update raw limit for MRES cascade invariance
                if rec.conv.mres != 0.0 {
                    rec.limits.rhlm = v / rec.conv.mres;
                }
                // C set_dial_highlimit (motorRecord.cc:4236-4277): a dial
                // high-limit write updates exactly ONE user limit — HLM
                // when DIR=Pos, LLM when DIR=Neg. No re-ordering.
                let user = coordinate::dial_to_user(v, rec.conv.dir, rec.pos.off);
                if rec.conv.dir == MotorDir::Pos {
                    rec.limits.hlm = user;
                } else {
                    rec.limits.llm = user;
                }
                detect_inverted_limits(&mut rec.limits, rec.pos.dval);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "DLLM" => match value {
            EpicsValue::Double(v) => {
                rec.limits.dllm = v;
                if rec.conv.mres != 0.0 {
                    rec.limits.rllm = v / rec.conv.mres;
                }
                // C set_dial_lowlimit (motorRecord.cc:4287-4328): mirror
                // of DHLM — LLM when DIR=Pos, HLM when DIR=Neg.
                let user = coordinate::dial_to_user(v, rec.conv.dir, rec.pos.off);
                if rec.conv.dir == MotorDir::Pos {
                    rec.limits.llm = user;
                } else {
                    rec.limits.hlm = user;
                }
                detect_inverted_limits(&mut rec.limits, rec.pos.dval);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "RHLM" => match value {
            EpicsValue::Double(v) => {
                // C: 2e89b552 — raw input drives dial/user (raw-master path).
                rec.limits.rhlm = v;
                rec.limits.dhlm = v * rec.conv.mres;
                normalize_raw_limit_pair(&mut rec.limits, rec.conv.mres);
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
        "RLLM" => match value {
            EpicsValue::Double(v) => {
                rec.limits.rllm = v;
                rec.limits.dllm = v * rec.conv.mres;
                normalize_raw_limit_pair(&mut rec.limits, rec.conv.mres);
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
                // C special() motorRecordJOGF (3042-3047): a release
                // clears a parked MIP_JOG_REQ; a press arms it only on an
                // idle record (mip == MIP_DONE) away from the limit
                // switch. The armed request parks across Stop/Pause until
                // the jog dispatch consumes MIP wholesale (2106).
                if v == 0 {
                    rec.stat.mip.remove(MipFlags::JOG_REQ);
                } else if rec.stat.mip.is_empty() && !rec.limits.hls {
                    rec.stat.mip.insert(MipFlags::JOG_REQ);
                }
                rec.last_write = Some(CommandSource::Jogf);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "JOGR" => match value {
            EpicsValue::Short(v) => {
                rec.ctrl.jogr = v != 0;
                // C special() motorRecordJOGR (3049-3054), mirror of JOGF
                // with the low limit switch.
                if v == 0 {
                    rec.stat.mip.remove(MipFlags::JOG_REQ);
                } else if rec.stat.mip.is_empty() && !rec.limits.lls {
                    rec.stat.mip.insert(MipFlags::JOG_REQ);
                }
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
        // MLST/ALST are SPC_NOMOD toward CA (read_only in FIELDS), but
        // the framework's deadband owner updates them through
        // `put_field` after a monitor/archive trigger — C monitor()
        // 3485-3501: `mlst = rbv` / `alst = rbv` track the last POSTED
        // readback. Without these arms the update was silently
        // swallowed and the anchor stayed 0.0 forever: with the
        // default MDEL=0 every process pass with RBV != 0 fired the
        // VAL monitor/archive triggers, and a nonzero MDEL anchored
        // the deadband at zero instead of at the last posted RBV.
        "MLST" => match value {
            EpicsValue::Double(v) => {
                rec.disp.mlst = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ALST" => match value {
            EpicsValue::Double(v) => {
                rec.disp.alst = v;
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
                // C: NTMF minimum is 2.0
                rec.timing.ntmf = if v < 2.0 { 2.0 } else { v };
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Sync — write-only trigger. C: `82c26005` (2010-04). Only fires on
        // non-zero put; VAL/DVAL/RVAL get reseeded from RBV/DRBV/RRBV in
        // command_planner::sync_positions().
        "SYNC" => match value {
            EpicsValue::Short(v) => {
                if v != 0 {
                    // C `pmr->sync` is the field value itself — latch it
                    // so the request survives a busy/paused pass (the
                    // apply is idle-gated, motorRecord.cc:2540-2544) and
                    // a last_write overtaken by a later put.
                    rec.internal.sync = true;
                    rec.last_write = Some(CommandSource::Sync);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Position-compare output config. C: 05b25c1d (PR #248).
        // Config fields just store; PCO_ENABLE triggers SetPcoConfig+EnablePco.
        "PCO_START" => match value {
            EpicsValue::Double(v) => {
                rec.pco.start = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "PCO_END" => match value {
            EpicsValue::Double(v) => {
                rec.pco.end = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "PCO_INC" => match value {
            EpicsValue::Double(v) => {
                rec.pco.increment = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "PCO_PW" => match value {
            EpicsValue::Double(v) => {
                rec.pco.pulse_width_us = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "PCO_ENABLE" => match value {
            EpicsValue::Short(v) => {
                rec.pco.enable = v != 0;
                rec.last_write = Some(CommandSource::PcoEnable);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Public C motorRecord.dbd link / string surface. The link string is
        // stored verbatim (the DBF_INLINK/DBF_OUTLINK specification); behavioral
        // link processing (closed-loop DOL drive, RDBL readback, RLNK firing) is
        // not wired here.
        "OUT" => put_link_string(value, &mut rec.links.out, name),
        "RDBL" => put_link_string(value, &mut rec.links.rdbl, name),
        "DOL" => put_link_string(value, &mut rec.links.dol, name),
        "RLNK" => put_link_string(value, &mut rec.links.rlnk, name),
        "STOO" => put_link_string(value, &mut rec.links.stoo, name),
        "DINP" => put_link_string(value, &mut rec.links.dinp, name),
        "RINP" => put_link_string(value, &mut rec.links.rinp, name),
        "POST" => put_link_string(value, &mut rec.links.post, name),
        // menuOmsl: 0 = supervisory, 1 = closed_loop.
        "OMSL" => match value {
            EpicsValue::Short(v) => {
                rec.links.omsl = v.clamp(0, 1);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // Alarm-limit / operator-range surface.
        "HIHI" => match value {
            EpicsValue::Double(v) => {
                rec.alarm.hihi = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "HIGH" => match value {
            EpicsValue::Double(v) => {
                rec.alarm.high = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "LOW" => match value {
            EpicsValue::Double(v) => {
                rec.alarm.low = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "LOLO" => match value {
            EpicsValue::Double(v) => {
                rec.alarm.lolo = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // menuAlarmSevr: 0 = NO_ALARM, 1 = MINOR, 2 = MAJOR, 3 = INVALID.
        "HHSV" => match value {
            EpicsValue::Short(v) => {
                rec.alarm.hhsv = v.clamp(0, 3);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "LLSV" => match value {
            EpicsValue::Short(v) => {
                rec.alarm.llsv = v.clamp(0, 3);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "HOPR" => match value {
            EpicsValue::Double(v) => {
                rec.disp.hopr = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "LOPR" => match value {
            EpicsValue::Double(v) => {
                rec.disp.lopr = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        _ => Err(CaError::FieldNotFound(name.into())),
    }
}

/// Store a DBF_INLINK/DBF_OUTLINK specification string verbatim, mirroring how
/// C `dbPutString` records the link before `recGblInitConstantLink`/resolution.
fn put_link_string(value: EpicsValue, slot: &mut String, name: &str) -> CaResult<()> {
    match value {
        EpicsValue::String(v) => {
            *slot = v.as_str_lossy().into_owned();
            Ok(())
        }
        _ => Err(CaError::TypeMismatch(name.into())),
    }
}

/// Recalc the slave of ACCL/ACCS after VELO or VBAS changes.
/// C: `7291b556` (2023-05-19) — when ACCU=Accl, ACCS follows; when ACCU=Accs, ACCL follows.
fn apply_accu_cascade(rec: &mut MotorRecord) {
    let span = rec.vel.velo - rec.effective_vbas();
    if span <= 0.0 {
        return; // C: span must be positive; otherwise leave master untouched
    }
    match rec.vel.accu {
        AccsUsed::Accl => {
            if rec.vel.accl > 0.0 {
                rec.vel.accs = span / rec.vel.accl;
            }
        }
        AccsUsed::Accs => {
            if rec.vel.accs > 0.0 {
                rec.vel.accl = span / rec.vel.accs;
            }
        }
    }
}

/// Apply velocity and limit cascade after MRES changes.
/// Used by MRES, SREV, and UREV handlers to avoid duplication.
///
/// C: `2e89b552` (PR #193) — raw limits (RHLM/RLLM in motor steps) are the
/// invariant across MRES changes. Dial and user limits are recomputed.
/// `fd808eb2` (PR #206) — when MRES < 0, the high/low pair must be ordered
/// so DHLM >= DLLM.
fn apply_mres_cascade(rec: &mut MotorRecord, old_mres: f64) {
    // Both re-derivations below are C special() semantics for a *runtime*
    // MRES/UREV/SREV change. During `dbLoadRecords` neither invariant is
    // established yet: `apply_fields` feeds every `field()` through
    // `put_field` (unlike C, which applies `field()` as raw struct writes),
    // so `field(MRES,…)` may run after `field(VELO,…)` / `field(DHLM,…)`.
    // Cascading mid-load would rewrite the freshly-loaded VELO from an S
    // derived against the pre-MRES default UREV, and rescale the
    // freshly-loaded DHLM against the pre-MRES default resolution.
    // `motor_sync_speed_at_init` / `motor_sync_limits_at_init` reconcile
    // once loading completes (C check_speed_and_resolution /
    // set_dial_highlimit / set_dial_lowlimit).
    //
    // C velcheckB (motorRecord.cc:2855-2875): across a resolution change
    // the rev-unit speeds are invariant — re-derive the EGU speeds.
    if rec.internal.init_invariants_synced {
        let urev_abs = rec.conv.urev.abs();
        if urev_abs > 0.0 {
            rec.vel.velo = urev_abs * rec.vel.s;
            rec.vel.vbas = urev_abs * rec.vel.sbas;
            rec.vel.bvel = urev_abs * rec.vel.sbak;
            rec.vel.vmax = urev_abs * rec.vel.smax;
        }
    }
    if rec.conv.mres == 0.0 {
        return;
    }
    if rec.internal.init_invariants_synced {
        // Seed raw limits from the dial limits the first time MRES changes
        // after init: RHLM/RLLM default to 0. Once any
        // HLM/LLM/DHLM/DLLM/RHLM/RLLM put has run, rhlm/rllm hold a
        // meaningful value and seeding is skipped.
        //
        // Invariant: every put that can leave rhlm==rllm==0 also leaves
        // dhlm==dllm==0 (HLM/LLM/DHLM/DLLM/RHLM/RLLM handlers always update
        // the raw/dial pair together). So when raw_unset is true, dhlm/dllm
        // are 0 too and the seed below is a 0->0 no-op. 0/0 is the "limits
        // disabled" convention (see check_soft_limits), never an active
        // limit pair.
        let raw_unset = rec.limits.rhlm == 0.0 && rec.limits.rllm == 0.0;
        if raw_unset && old_mres != 0.0 {
            rec.limits.rhlm = rec.limits.dhlm / old_mres;
            rec.limits.rllm = rec.limits.dllm / old_mres;
        }
        // Raw is invariant — recompute dial.
        rec.limits.dhlm = rec.limits.rhlm * rec.conv.mres;
        rec.limits.dllm = rec.limits.rllm * rec.conv.mres;
        normalize_raw_limit_pair(&mut rec.limits, rec.conv.mres);
        let (hlm, llm) = crate::coordinate::dial_limits_to_user(
            rec.limits.dhlm,
            rec.limits.dllm,
            rec.conv.dir,
            rec.pos.off,
        );
        rec.limits.hlm = hlm;
        rec.limits.llm = llm;
    }
    // C: special() calls enforceMinRetryDeadband on MRES change.
    rec.enforce_min_retry_deadband();
}

/// C `range_check` (motorRecord.cc:4358): clamp `parm` up to `min`, and
/// down to `max` unless `max` is 0 (no ceiling configured).
fn range_check(parm: &mut f64, min: f64, max: f64) {
    if *parm < min {
        *parm = min;
    }
    if max != 0.0 && *parm > max {
        *parm = max;
    }
}

/// C special() `velcheckA` tail (motorRecord.cc:3128-3148): after a
/// VBAS/SBAS/VMAX/SMAX write changes the velocity window, re-clamp every
/// dependent velocity into the new [VBAS, VMAX] and re-derive the rev-unit
/// partners of VELO and BVEL.
fn revalidate_velocities(rec: &mut MotorRecord) {
    let urev_abs = rec.conv.urev.abs();
    range_check(&mut rec.vel.velo, rec.vel.vbas, rec.vel.vmax);
    if urev_abs > 0.0 {
        rec.vel.s = rec.vel.velo / urev_abs;
    }
    range_check(&mut rec.vel.bvel, rec.vel.vbas, rec.vel.vmax);
    if urev_abs > 0.0 {
        rec.vel.sbak = rec.vel.bvel / urev_abs;
    }
    range_check(&mut rec.vel.jvel, rec.vel.vbas, rec.vel.vmax);
    range_check(&mut rec.vel.hvel, rec.vel.vbas, rec.vel.vmax);
}

/// Reconcile the rev↔EGU speed pairs at IOC init — the speed half of C
/// `check_speed_and_resolution` (motorRecord.cc:3895), which C
/// `init_record` runs once after `dbLoadRecords` has applied every
/// `field()` as a raw struct write.
///
/// The cross-calcs in `put_field` stay inert during load, so each loaded
/// field still holds exactly its `field()` value here. A nonzero rev-unit
/// field (S/SBAS/SMAX/SBAK) therefore means the .db specified it and, as in
/// C, it wins over its EGU partner (motorRecord.cc:3929-3966, 4019-4029);
/// otherwise the EGU value is authoritative and the rev value is derived at
/// the final UREV. Without this split, `field(VELO,…)` applied before
/// `field(MRES,…)` derives S against the default UREV and the MRES cascade
/// then rewrites VELO from that stale S — a `motor.template` load scaled
/// every VELO by final-UREV/default-UREV (0.2× for the default template).
///
/// C's preceding SREV/MRES/UREV reconcile (motorRecord.cc:3904-3927) is not
/// ported: the MRES/SREV/UREV handlers reject zero/non-positive puts and
/// re-derive the other two on every put, so the resolution triple is
/// already mutually consistent when init runs.
pub(crate) fn motor_sync_speed_at_init(rec: &mut MotorRecord) {
    let urev_abs = rec.conv.urev.abs();
    if urev_abs > 0.0 {
        // SMAX (rev/s) <-> VMAX (EGU/s)
        if rec.vel.smax > 0.0 {
            rec.vel.vmax = rec.vel.smax * urev_abs;
        } else if rec.vel.vmax > 0.0 {
            rec.vel.smax = rec.vel.vmax / urev_abs;
        } else {
            rec.vel.smax = 0.0;
            rec.vel.vmax = 0.0;
        }
        // SBAS (rev/s) <-> VBAS (EGU/s)
        if rec.vel.sbas != 0.0 {
            range_check(&mut rec.vel.sbas, 0.0, rec.vel.smax);
            rec.vel.vbas = rec.vel.sbas * urev_abs;
        } else {
            range_check(&mut rec.vel.vbas, 0.0, rec.vel.vmax);
            rec.vel.sbas = rec.vel.vbas / urev_abs;
        }
        // S (rev/s) <-> VELO (EGU/s)
        if rec.vel.s != 0.0 {
            range_check(&mut rec.vel.s, rec.vel.sbas, rec.vel.smax);
            rec.vel.velo = rec.vel.s * urev_abs;
        } else {
            range_check(&mut rec.vel.velo, rec.vel.vbas, rec.vel.vmax);
            rec.vel.s = rec.vel.velo / urev_abs;
        }
        // SBAK (rev/s) <-> BVEL (EGU/s)
        if rec.vel.sbak != 0.0 {
            range_check(&mut rec.vel.sbak, rec.vel.sbas, rec.vel.smax);
            rec.vel.bvel = rec.vel.sbak * urev_abs;
        } else {
            range_check(&mut rec.vel.bvel, rec.vel.vbas, rec.vel.vmax);
            rec.vel.sbak = rec.vel.bvel / urev_abs;
        }
    }
    // ACCS <-> ACCL. C keys on ACCS > 0 — its dbd default is 0, so nonzero
    // means the .db set it (motorRecord.cc:4033-4047). The Rust default
    // ACCS is derived (nonzero), so the master is whichever field ACCU
    // names; the .db loading ACCS or ACCU flips it to Accs.
    apply_accu_cascade(rec);
}

/// Establish the limit invariant at IOC init — the load-time counterpart of
/// [`apply_mres_cascade`].
///
/// C `init_record` calls `set_dial_highlimit`/`set_dial_lowlimit`, which take
/// the dial limits DHLM/DLLM loaded by `dbLoadRecords` as authoritative and
/// derive the raw limits (RHLM/RLLM = DHLM/DLLM ÷ MRES) and user limits
/// (HLM/LLM) from them. From this point on a runtime MRES change keeps
/// RHLM/RLLM invariant and rescales DHLM/DLLM (C PR #193,
/// [`apply_mres_cascade`]).
///
/// This must run after all `field()` values are applied: it repairs the
/// RHLM/RLLM that the DHLM/DLLM `put_field` handlers computed against
/// whatever MRES happened to be in effect at the time (the default 1.0 when
/// `field(DHLM,…)` precedes `field(MRES,…)`, as in `motor.template`).
pub(crate) fn motor_sync_limits_at_init(rec: &mut MotorRecord) {
    if rec.conv.mres != 0.0 {
        rec.limits.rhlm = rec.limits.dhlm / rec.conv.mres;
        rec.limits.rllm = rec.limits.dllm / rec.conv.mres;
    }
    normalize_raw_limit_pair(&mut rec.limits, rec.conv.mres);
    let (hlm, llm) = crate::coordinate::dial_limits_to_user(
        rec.limits.dhlm,
        rec.limits.dllm,
        rec.conv.dir,
        rec.pos.off,
    );
    rec.limits.hlm = hlm;
    rec.limits.llm = llm;
}

/// Re-evaluate LVIO after a soft-limit put.
/// C: `270347df` (PR #108) — an inverted high/low pair (LLM > HLM or
/// DLLM > DHLM) sets LVIO immediately, without waiting for the next poll.
/// When the pair is valid again, LVIO is recomputed from the current DVAL
/// so a corrected limit clears the latched violation even on an idle axis
/// that is not being polled.
fn detect_inverted_limits(limits: &mut LimitFields, dval: f64) {
    if limits.dllm > limits.dhlm || limits.llm > limits.hlm {
        limits.lvio = true;
    } else {
        limits.lvio = coordinate::check_soft_limits(dval, limits.dhlm, limits.dllm);
    }
}

/// When MRES < 0, dial limits derived from raw end up with DHLM < DLLM.
/// C `fd808eb2` (PR #206) swaps the pair so the high/low semantics hold.
fn normalize_raw_limit_pair(limits: &mut LimitFields, mres: f64) {
    if mres < 0.0 && limits.dhlm < limits.dllm {
        std::mem::swap(&mut limits.dhlm, &mut limits.dllm);
        std::mem::swap(&mut limits.rhlm, &mut limits.rllm);
    }
}
