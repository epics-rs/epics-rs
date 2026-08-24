use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::recgbl::{rec_gbl_get_alarm_double, rec_gbl_get_graphic_double};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use crate::coordinate;
use crate::fields::*;
use crate::flags::*;

use super::MotorRecord;
use super::dbd_generated::MOTOR_FIELDS;

/// The motorRecord version this port tracks (C `#define VERSION 7.4`,
/// stamped into VERS at init_record:608; SPC_NOMOD).
const MOTOR_RECORD_VERSION: f32 = 7.4;

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
        "FOF" => Some(EpicsValue::Short(rec.conv.fof)),
        "VOF" => Some(EpicsValue::Short(rec.conv.vof)),
        "SET" => Some(EpicsValue::Short(if rec.conv.set { 1 } else { 0 })),
        "SSET" => Some(EpicsValue::Short(rec.conv.sset)),
        "SUSE" => Some(EpicsValue::Short(rec.conv.suse)),
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
        "RHLS" => Some(EpicsValue::Short(if rec.limits.rhls { 1 } else { 0 })),
        "RLLS" => Some(EpicsValue::Short(if rec.limits.rlls { 1 } else { 0 })),
        // C 196/608: init_record stamps the ported motorRecord version.
        "VERS" => Some(EpicsValue::Float(MOTOR_RECORD_VERSION)),
        "HLSV" => Some(EpicsValue::Short(rec.limits.hlsv)),
        // Control
        "SPMG" => Some(EpicsValue::Short(rec.ctrl.spmg as i16)),
        // C `pmr->lspg`, the SPMG the record last acted on
        // (motorRecord.cc:725 init, :1859 sync). Same menu as SPMG, so
        // same served form; the declared DBF_MENU makes it a DBR_ENUM.
        "LSPG" => Some(EpicsValue::Short(rec.internal.lspg as i16)),
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
        // Timing
        "DLY" => Some(EpicsValue::Double(rec.timing.dly)),
        "NTM" => Some(EpicsValue::Short(if rec.timing.ntm { 1 } else { 0 })),
        "NTMF" => Some(EpicsValue::UShort(rec.timing.ntmf)),
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
        // C `pmr->pp` — post-process armed for the motion in flight
        // (15 write sites, motorRecord.cc:825..2523).
        "PP" => Some(EpicsValue::Short(if rec.internal.pp { 1 } else { 0 })),
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
                if rec.conv.foff == FreezeOffset::Variable {
                    // C 2206-2227: redefine VAL without moving the motor
                    // and without touching DVAL — offset-only, completes
                    // on the spot. No last_write: C returns from the
                    // collection block (2227), nothing to dispatch. No
                    // controller command is involved, so the #231
                    // LOAD_POS block does not apply.
                    rec.set_mode_redefine_val(v);
                } else {
                    // #231: LOAD_POS blocked — refuse the Frozen-leg
                    // redefinition, which needs the controller write
                    // (C load_pos sends LOAD_POS unconditionally, 3811),
                    // so DVAL/OFF stay consistent with the controller.
                    if rec.conv.loadpos_blocked {
                        return Ok(());
                    }
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
                        // C load_pos Variable leg (motorRecord.cc:3800):
                        // the recomputed offset retranslates user limits.
                        rec.set_userlimits();
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
            // C special() has no RVAL cascade — the RVAL->DVAL
            // propagation lives in the do_work collection block
            // (2196-2197), inside the else that closed-loop OMSL with a
            // DB-link DOL bypasses (1994). Under that gate a put is a raw
            // struct write that stays inert (both move and SET-mode
            // redefinition) until OMSL leaves closed loop.
            if rec.closed_loop_dol_collection() {
                rec.pos.rval = v;
                rec.last_write = Some(CommandSource::Rval);
                return Ok(());
            }
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
                    // C load_pos Variable leg (motorRecord.cc:3800).
                    rec.set_userlimits();
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
                    rec.set_userlimits();
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
                    rec.set_userlimits();
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
        // C special (motorRecord.cc:2975-2984): FOF/VOF are momentary
        // command fields — ANY write (including 0) forces FOFF to
        // Frozen/Variable; the written value is only stored for reads.
        // special() never runs during dbLoadRecords, so a load-time
        // field() write stores the value raw without forcing FOFF.
        "FOF" => match value {
            EpicsValue::Short(v) => {
                rec.conv.fof = v;
                if rec.internal.init_invariants_synced {
                    rec.conv.foff = FreezeOffset::Frozen;
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "VOF" => match value {
            EpicsValue::Short(v) => {
                rec.conv.vof = v;
                if rec.internal.init_invariants_synced {
                    rec.conv.foff = FreezeOffset::Variable;
                }
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
        // C special (motorRecord.cc:2963-2972): SSET/SUSE are momentary
        // command fields — ANY write forces SET to Set/Use mode.
        // special() never runs during dbLoadRecords, so a load-time
        // field() write stores the value raw without forcing SET.
        "SSET" => match value {
            EpicsValue::Short(v) => {
                rec.conv.sset = v;
                if rec.internal.init_invariants_synced {
                    rec.conv.set = true;
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SUSE" => match value {
            EpicsValue::Short(v) => {
                rec.conv.suse = v;
                if rec.internal.init_invariants_synced {
                    rec.conv.set = false;
                }
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
                if !rec.internal.init_invariants_synced {
                    // dbLoadRecords lands field() raw (C writes the struct
                    // directly — special() never runs at load);
                    // `motor_sync_resolution_at_init` reconciles the
                    // SREV/UREV/MRES triple once loading completes
                    // (C check_speed_and_resolution, 3904-3927).
                    rec.conv.mres = v;
                    return Ok(());
                }
                // Rust guard, not C: special MRES (2834-2843) accepts a
                // zero put and lets the record degenerate; the Rust
                // division sites (raw conversions, RDBD-in-steps) require
                // a nonzero resolution, so a zero put is dropped.
                if v == 0.0 {
                    return Ok(());
                }
                rec.conv.mres = v;
                // C special MRES (2837-2842): make UREV agree.
                rec.conv.urev = v * rec.conv.srev as f64;
                apply_mres_cascade(rec);
                // C special MRES (2835): MARK(M_MRES) unconditionally —
                // the pp(TRUE) process pass re-anchors (do_work 1936).
                rec.internal.res_reanchor = true;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ERES" => match value {
            EpicsValue::Double(v) => {
                if !rec.internal.init_invariants_synced {
                    // Raw store during load; init seeds a zero ERES from
                    // the reconciled MRES (C init_record 692-696) —
                    // mapping here would capture whatever MRES holds
                    // mid-load.
                    rec.conv.eres = v;
                    return Ok(());
                }
                // C special ERES (2927-2929): don't allow ERES = 0.
                rec.conv.eres = if v == 0.0 { rec.conv.mres } else { v };
                // C special ERES (2930): MARK(M_ERES) unconditionally.
                rec.internal.res_reanchor = true;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "SREV" => match value {
            EpicsValue::Long(v) => {
                if !rec.internal.init_invariants_synced {
                    // Raw store during load (even non-positive — C raw-writes
                    // it and the init reconcile clamps, 3904-3909).
                    rec.conv.srev = v;
                    return Ok(());
                }
                // C special SREV (2914-2918): a non-positive put is
                // clamped to 200, not rejected.
                let v = if v <= 0 { 200 } else { v };
                let old_mres = rec.conv.mres;
                rec.conv.srev = v;
                // C special SREV (2919-2923): make MRES agree — and
                // nothing else. Unlike the MRES/UREV cases, SREV breaks
                // straight out: no velcheckB velocity re-derive, no
                // dial-limit rescale (UREV is unchanged, so the EGU
                // speeds already agree).
                // The urev != 0 gate is a Rust guard, not C: C divides
                // unconditionally and lets MRES go to 0; the Rust
                // division sites require a nonzero resolution.
                if rec.conv.urev != 0.0 {
                    rec.conv.mres = rec.conv.urev / v as f64;
                }
                // C special SREV (2919-2922): MARK(M_MRES) only when
                // MRES actually changed.
                if rec.conv.mres != old_mres {
                    rec.internal.res_reanchor = true;
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "UREV" => match value {
            EpicsValue::Double(v) => {
                if !rec.internal.init_invariants_synced {
                    rec.conv.urev = v;
                    return Ok(());
                }
                // Rust guard, not C: special UREV (2848-2853) accepts a
                // zero put and derives MRES = 0; the Rust division sites
                // require a nonzero resolution, so drop it — uniform
                // with the zero-MRES-put reject above.
                if v == 0.0 {
                    return Ok(());
                }
                let old_mres = rec.conv.mres;
                rec.conv.urev = v;
                // C special UREV (2849-2853): make MRES agree.
                if rec.conv.srev > 0 {
                    rec.conv.mres = v / rec.conv.srev as f64;
                }
                apply_mres_cascade(rec);
                // C special UREV (2848-2853): MARK(M_MRES) only when
                // MRES actually changed.
                if rec.conv.mres != old_mres {
                    rec.internal.res_reanchor = true;
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "UEIP" => match value {
            EpicsValue::Short(v) => {
                let ueip = v != 0;
                // C special UEIP (2934-2950) runs only on runtime puts —
                // dbLoadRecords writes field() values raw, so a template's
                // UEIP=Yes survives load (MSTA is still empty) until the
                // first poll's encoder check (process_motor_info 3671-3675).
                if ueip && rec.internal.init_invariants_synced {
                    // C: if UEIP=Yes and encoder present, set URIP=No
                    // If no encoder present, override UEIP back to No
                    if rec.stat.msta.contains(MstaFlags::ENCODER_PRESENT) {
                        rec.conv.urip = false;
                    } else {
                        // No encoder available, cannot use UEIP.
                        // C special UEIP (2947-2948): only this override
                        // path MARKs M_UEIP — a plain UEIP change never
                        // fires the do_work re-anchor.
                        rec.conv.ueip = false;
                        rec.internal.res_reanchor = true;
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
                // C: if URIP=Yes and UEIP=Yes, set UEIP=No.
                // C special URIP (2955-2959): MARK(M_UEIP) only when UEIP
                // was actually forced off. Runtime-only — at .db load both
                // may land Yes (raw field() writes); the first poll
                // resolves precedence (UEIP is checked first at 3676,
                // unless the encoder check demotes it).
                if urip && rec.conv.ueip && rec.internal.init_invariants_synced {
                    rec.conv.ueip = false;
                    rec.internal.res_reanchor = true;
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
                    // C: 7291b556 — recalc ACCL/ACCS based on ACCU. Also
                    // special()-only: a load must leave the accel pair raw
                    // (see `motor_sync_speed_at_init`).
                    apply_accu_cascade(rec);
                }
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
                    apply_accu_cascade(rec);
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
                    // C motorRecord.cc:2710: ACCL/ACCS follow the VELO change.
                    apply_accu_cascade(rec);
                }
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
                    // C motorRecord.cc:2655: ACCL/ACCS follow the VBAS change.
                    apply_accu_cascade(rec);
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
                rec.vel.accl = v;
                // C special() motorRecordACCL (motorRecord.cc:2735-2742): floor
                // ACCL to 0.1 if <= 0, then updateACCSfromACCL. ACCU is NOT
                // touched — 63bfe5d0 made ACCU a user/autosave control and
                // dropped the 36177f7b auto-switch from the accel helpers.
                // special() never runs during dbLoadRecords, so a load stores
                // the field() value raw and leaves ACCS alone —
                // `motor_sync_speed_at_init` reconciles the pair, keying on
                // "did the .db set ACCS" exactly as C does.
                if rec.internal.init_invariants_synced {
                    if rec.vel.accl <= 0.0 {
                        rec.vel.accl = 0.1;
                    }
                    update_accs_from_accl(rec);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ACCS" => match value {
            EpicsValue::Double(v) => {
                // C special() motorRecordACCS (motorRecord.cc:2745-2752): the
                // raw put lands in ACCS; if it is non-positive C derives ACCS
                // from ACCL (updateACCSfromACCL, accs = velo/accl — NOT a
                // literal 1.0), then recomputes ACCL from ACCS
                // (updateACCLfromACCS). ACCU is NOT touched (63bfe5d0 dropped
                // the 36177f7b auto-switch). Runtime-only, as for ACCL.
                rec.vel.accs = v;
                if rec.internal.init_invariants_synced {
                    if rec.vel.accs <= 0.0 {
                        update_accs_from_accl(rec);
                    }
                    update_accl_from_accs(rec);
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
                    // C motorRecord.cc:3059-3072: a JVEL write landing on an
                    // active jog retunes it in place. The retune command is
                    // emitted by the process pass that follows this put.
                    rec.jog_retune_pending = true;
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "JAR" => match value {
            EpicsValue::Double(v) => {
                rec.vel.jar = v;
                // C motorRecord.cc:3074-3078: a non-positive JAR is replaced
                // by JVEL / 0.1. Runtime special() only — during load field()
                // lands raw, and an unconfigured JAR (== 0) is derived from
                // VELO/ACCL at init.
                if rec.internal.init_invariants_synced && rec.vel.jar <= 0.0 {
                    rec.vel.jar = rec.vel.jvel / 0.1;
                }
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
                // C special BDST (2986-2989): "New backlash distance.
                // Make sure retry deadband is achievable." Runtime only,
                // same load/init split as RDBD.
                if rec.internal.init_invariants_synced {
                    rec.enforce_min_retry_deadband();
                }
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
                // C special RDBD (2764-2766): enforceMinRetryDeadband —
                // RDBD must be >= |MRES|. Runtime only: during load the
                // value lands raw (C field() is a raw struct write) and
                // init_record enforces once against the final MRES
                // (C 642) — enforcing mid-load would clamp against
                // whatever MRES happens to hold at that point in the .db.
                if rec.internal.init_invariants_synced {
                    rec.enforce_min_retry_deadband();
                }
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
                // when DIR=Pos, DLLM when DIR=Neg — plus the raw register
                // selected by `dir_positive ^ (mres < 0)` (4098-4107).
                // The pair is never re-ordered: writing HLM below LLM
                // leaves an inverted dial pair, which latches LVIO below
                // and blocks every move until corrected.
                let dial = coordinate::user_to_dial(v, rec.conv.dir, rec.pos.off);
                if rec.conv.dir == MotorDir::Pos {
                    rec.limits.dhlm = dial;
                    update_raw_from_dial_high(rec, dial);
                    queue_limit_forward(rec, DialLimit::High, dial);
                } else {
                    rec.limits.dllm = dial;
                    update_raw_from_dial_low(rec, dial);
                    queue_limit_forward(rec, DialLimit::Low, dial);
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
                    update_raw_from_dial_low(rec, dial);
                    queue_limit_forward(rec, DialLimit::Low, dial);
                } else {
                    rec.limits.dhlm = dial;
                    update_raw_from_dial_high(rec, dial);
                    queue_limit_forward(rec, DialLimit::High, dial);
                }
                detect_inverted_limits(&mut rec.limits, rec.pos.dval);
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "DHLM" => match value {
            EpicsValue::Double(v) => {
                rec.limits.dhlm = v;
                update_raw_from_dial_high(rec, v);
                queue_limit_forward(rec, DialLimit::High, v);
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
                update_raw_from_dial_low(rec, v);
                queue_limit_forward(rec, DialLimit::Low, v);
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
                // C dbd (2e89b552): SPC_NOMOD — a database field() load
                // is a raw struct write; the raw-wins rule at init
                // (motor_sync_limits_at_init) derives the dial pair.
                // Runtime CA writes never reach here (read_only).
                rec.limits.rhlm = v;
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "RLLM" => match value {
            EpicsValue::Double(v) => {
                rec.limits.rllm = v;
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
                // C special() (motorRecord.cc:2610-2614): a HOMF/HOMR put
                // while a home is in flight (mip & MIP_HOME) returns
                // ERROR — the button is not written and the record does
                // not process.
                if rec.stat.mip.intersects(MipFlags::HOMF | MipFlags::HOMR) {
                    return Err(CaError::InvalidValue("HOMF: home in progress".into()));
                }
                // C writes the field value: a 0-write un-latches a parked
                // button (e.g. one blocked at its limit switch) without
                // triggering a dispatch pass.
                rec.ctrl.homf = v != 0;
                if v != 0 {
                    rec.last_write = Some(CommandSource::Homf);
                }
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "HOMR" => match value {
            EpicsValue::Short(v) => {
                // C special() (motorRecord.cc:2610-2614), same veto as
                // HOMF.
                if rec.stat.mip.intersects(MipFlags::HOMF | MipFlags::HOMR) {
                    return Err(CaError::InvalidValue("HOMR: home in progress".into()));
                }
                // C writes the field value, like HOMF above.
                rec.ctrl.homr = v != 0;
                if v != 0 {
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
        // C motorSTUP menu: OFF=0, ON=1, BUSY=2.
        "STUP" => match value {
            EpicsValue::Short(v) => {
                // C special() before-write (2615-2617): a STUP put while
                // the previous request is still in flight (stup != OFF)
                // returns ERROR — the field is not written.
                if rec.stat.stup != 0 {
                    return Err(CaError::InvalidValue(
                        "STUP: status update in progress".into(),
                    ));
                }
                // C special() after-write (3084-3090): any value other
                // than ON is forced back to OFF and does not trigger the
                // protocol.
                rec.stat.stup = if v == 1 { 1 } else { 0 };
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        // PID
        // C special pidcof (3003-3026): with GAIN_SUPPORT the gain is
        // clamped to 0.0 <= gain <= 1.0 and forwarded as SET_PGAIN/
        // SET_IGAIN/SET_DGAIN; without it the write is a raw store (no
        // clamp, no command).
        "PCOF" => match value {
            EpicsValue::Double(v) => {
                rec.pid.pcof = if rec.stat.msta.contains(MstaFlags::GAIN_SUPPORT) {
                    let gain = v.clamp(0.0, 1.0);
                    queue_pid_gain_forward(rec, PidGainKind::Proportional, gain);
                    gain
                } else {
                    v
                };
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "ICOF" => match value {
            EpicsValue::Double(v) => {
                rec.pid.icof = if rec.stat.msta.contains(MstaFlags::GAIN_SUPPORT) {
                    let gain = v.clamp(0.0, 1.0);
                    queue_pid_gain_forward(rec, PidGainKind::Integral, gain);
                    gain
                } else {
                    v
                };
                Ok(())
            }
            _ => Err(CaError::TypeMismatch(name.into())),
        },
        "DCOF" => match value {
            EpicsValue::Double(v) => {
                rec.pid.dcof = if rec.stat.msta.contains(MstaFlags::GAIN_SUPPORT) {
                    let gain = v.clamp(0.0, 1.0);
                    queue_pid_gain_forward(rec, PidGainKind::Derivative, gain);
                    gain
                } else {
                    v
                };
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
            EpicsValue::UShort(v) => {
                // C motorRecord.cc:3093-3100: integer compare, minimum 2.
                // The DBF_USHORT field already truncated any fractional CA
                // put before special() runs.
                rec.timing.ntmf = if v < 2 { 2 } else { v };
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

/// The acceleration-rate numerator every ACCL↔ACCS conversion divides by its
/// partner: `(velo - vbas)` while `velo > vbas`, else the full `velo`. C repeats
/// this ternary in `updateACCLfromACCS` / `updateACCSfromACCL` /
/// `updateACCL_ACCSfromVELO` (motorRecord.cc:499-546); one owner here so the
/// three cannot drift.
fn accel_numerator(rec: &MotorRecord) -> f64 {
    let vbas = rec.effective_vbas();
    if rec.vel.velo > vbas {
        rec.vel.velo - vbas
    } else {
        rec.vel.velo
    }
}

/// C `updateACCLfromACCS` (motorRecord.cc:499-510): ACCS is the master, ACCL
/// follows. A non-positive ACCS leaves ACCL alone (C's `accs > 0.0` guard).
fn update_accl_from_accs(rec: &mut MotorRecord) {
    if rec.vel.accs > 0.0 {
        rec.vel.accl = accel_numerator(rec) / rec.vel.accs;
    }
}

/// C `updateACCSfromACCL` (motorRecord.cc:512-521): ACCL is the master, ACCS
/// follows. The `accl > 0.0` guard is Rust-side: C divides unconditionally, and
/// every C caller has already floored ACCL to 0.1 — the guard keeps a raw
/// `field(ACCL,"0")` load from producing an infinite ACCS before the init floor
/// runs, and is unreachable on the runtime paths.
fn update_accs_from_accl(rec: &mut MotorRecord) {
    if rec.vel.accl > 0.0 {
        rec.vel.accs = accel_numerator(rec) / rec.vel.accl;
    }
}

/// Recalc the slave of ACCL/ACCS after VELO or VBAS changes.
/// C: `7291b556` (2023-05-19) — when ACCU=Accl, ACCS follows; when ACCU=Accs, ACCL follows.
fn apply_accu_cascade(rec: &mut MotorRecord) {
    // C updateACCL_ACCSfromVELO (motorRecord.cc:523-546): recomputes/posts the
    // ACCU-named slave field in both the velo > vbas and velo <= vbas cases.
    match rec.vel.accu {
        AccsUsed::Accl => update_accs_from_accl(rec),
        AccsUsed::Accs => update_accl_from_accs(rec),
    }
}

/// Apply velocity and limit cascade after MRES changes.
/// Used by MRES, SREV, and UREV handlers to avoid duplication.
///
/// C: `2e89b552` (PR #193) — raw limits (RHLM/RLLM in motor steps) are the
/// invariant across MRES changes. Dial and user limits are recomputed.
/// `fd808eb2` (PR #206) — when MRES < 0, the high/low pair must be ordered
/// so DHLM >= DLLM.
/// C velcheckB (motorRecord.cc:2855-2909) — the runtime tail of a
/// resolution change. Callers (the MRES/UREV/SREV runtime put branches)
/// own the load gate: during `dbLoadRecords` the resolution handlers
/// store raw and never reach here; `motor_sync_resolution_at_init` /
/// `motor_sync_speed_at_init` / `motor_sync_limits_at_init` reconcile
/// once loading completes (C check_speed_and_resolution /
/// set_dial_highlimit / set_dial_lowlimit).
fn apply_mres_cascade(rec: &mut MotorRecord) {
    // C velcheckB (motorRecord.cc:2855-2875): across a resolution change
    // the rev-unit speeds are invariant — re-derive the EGU speeds.
    let urev_abs = rec.conv.urev.abs();
    if urev_abs > 0.0 {
        rec.vel.velo = urev_abs * rec.vel.s;
        rec.vel.vbas = urev_abs * rec.vel.sbas;
        rec.vel.bvel = urev_abs * rec.vel.sbak;
        rec.vel.vmax = urev_abs * rec.vel.smax;
    }
    if rec.conv.mres == 0.0 {
        return;
    }
    // C special MRES (2877-2906): the raw pair is the invariant —
    // recompute dial from raw. MRES < 0 crosses the assignment with
    // the SIGNED resolution (dhlm = rllm * mres, dllm = rhlm * mres,
    // "MRES < 0 swaps DHLM DLLM"), leaving the raw registers
    // untouched. Raw is always populated here: init seeds it and
    // every runtime dial/user limit put maintains it.
    if rec.conv.mres > 0.0 {
        rec.limits.dllm = rec.limits.rllm * rec.conv.mres;
        rec.limits.dhlm = rec.limits.rhlm * rec.conv.mres;
    } else {
        rec.limits.dhlm = rec.limits.rllm * rec.conv.mres;
        rec.limits.dllm = rec.limits.rhlm * rec.conv.mres;
    }
    rec.set_userlimits();
    // C restores the RDBD >= |MRES| invariant at the next process pass
    // (do_work 1971); enforcing here is the same invariant, one pass
    // earlier.
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

/// Reconcile the SREV/UREV/MRES resolution triple at IOC init — the head
/// of C `check_speed_and_resolution` (motorRecord.cc:3904-3927), run once
/// after `dbLoadRecords` has applied every `field()` as a raw struct
/// write. A nonzero loaded UREV wins over a loaded MRES; SREV is clamped
/// sane first so both derivations divide by the final value.
///
/// The Rust default MRES is 1.0 where the dbd leaves it 0.0: the C
/// `mres == 0 → 1.0` arm (3917-3921) makes both starting points converge,
/// and the UREV-wins arm overwrites MRES regardless, so no load scenario
/// can tell the defaults apart.
pub(crate) fn motor_sync_resolution_at_init(rec: &mut MotorRecord) {
    // C 3904-3909: SREV (steps/revolution) must be sane.
    if rec.conv.srev <= 0 {
        rec.conv.srev = 200;
    }
    // C 3911-3916: UREV (EGU/revolution) <--> MRES (EGU/step).
    if rec.conv.urev != 0.0 {
        rec.conv.mres = rec.conv.urev / rec.conv.srev as f64;
    }
    // C 3917-3921: MRES must end up nonzero.
    if rec.conv.mres == 0.0 {
        rec.conv.mres = 1.0;
    }
    // C 3922-3927: keep the triple consistent at the final MRES.
    if rec.conv.urev != rec.conv.mres * rec.conv.srev as f64 {
        rec.conv.urev = rec.conv.mres * rec.conv.srev as f64;
    }
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
/// Must run after [`motor_sync_resolution_at_init`] — the rev↔EGU pairs
/// divide by the final UREV (C runs both inside the same
/// check_speed_and_resolution call, resolution first).
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
    // ACCS <-> ACCL, C check_speed_and_resolution (motorRecord.cc:4033-4047).
    // The key is `accs > 0.0` — the loaded ACCS value — NOT ACCU: ACCS's dbd
    // default is 0.0 (as is `VelocityFields::default`), the accel cross-calcs
    // are special()-only and so never fire during dbLoadRecords, and therefore
    // a nonzero ACCS here can only mean the .db wrote `field(ACCS,…)`. It then
    // wins and ACCL is derived from it; otherwise ACCL is the master (floored
    // to 0.1 first, C:4041-4045) and ACCS is derived. ACCU stays a pure
    // user/autosave control (63bfe5d0) — it selects the master only for the
    // *runtime* VELO/VBAS cascade (`apply_accu_cascade`), not at init.
    if rec.vel.accs > 0.0 {
        update_accl_from_accs(rec);
    } else {
        if rec.vel.accl == 0.0 {
            rec.vel.accl = 0.1;
        }
        update_accs_from_accl(rec);
    }

    // C motorRecord.cc:4054-4067 — jog/home velocity sanity checks, after
    // the speed pairs and accelerations settle. A zero field means "not
    // configured" (JVEL/JAR/HVEL have no initial() in the dbd): JVEL
    // defaults to VELO, JAR to VELO/ACCL, HVEL to VBAS; a configured value
    // is clamped into [VBAS, VMAX].
    if rec.vel.jvel == 0.0 {
        rec.vel.jvel = rec.vel.velo;
    } else {
        range_check(&mut rec.vel.jvel, rec.vel.vbas, rec.vel.vmax);
    }
    if rec.vel.jar == 0.0 {
        rec.vel.jar = rec.vel.velo / rec.vel.accl;
    }
    if rec.vel.hvel == 0.0 {
        rec.vel.hvel = rec.vel.vbas;
    } else {
        range_check(&mut rec.vel.hvel, rec.vel.vbas, rec.vel.vmax);
    }
}

/// Establish the limit invariant at IOC init — the load-time counterpart of
/// [`apply_mres_cascade`].
///
/// Two C steps, in order:
///
/// 1. `check_speed_and_resolution` (motorRecord.cc:3968-4017): the raw-wins
///    rule — a NONZERO raw limit loaded from the database rescales its dial
///    partner; a zero raw is seeded from the dial value. Under MRES < 0 the
///    pairs cross (raw low <-> dial high) and scale by |MRES|.
/// 2. `init_record` 716-718 ("Reset limits in case database values are
///    invalid"): `set_dial_highlimit`/`set_dial_lowlimit` run last and
///    re-derive the raw pair with the SIGNED resolution — under MRES < 0
///    the |MRES|-seeded raw value from step 1 flips sign here (C verbatim).
///
/// Must run after all `field()` values are applied (the dial put handlers
/// leave the raw pair untouched until init), and before
/// `init_invariants_synced` is set — the helpers below are init-gated, so
/// step 2 inlines their arithmetic.
pub(crate) fn motor_sync_limits_at_init(rec: &mut MotorRecord) {
    let mres = rec.conv.mres;
    if mres != 0.0 {
        if mres > 0.0 {
            // C 3968-3992.
            if rec.limits.rllm != 0.0 {
                rec.limits.dllm = rec.limits.rllm * mres;
            }
            rec.limits.rllm = rec.limits.dllm / mres;
            if rec.limits.rhlm != 0.0 {
                rec.limits.dhlm = rec.limits.rhlm * mres;
            }
            rec.limits.rhlm = rec.limits.dhlm / mres;
        } else {
            // C 3993-4017: MRES < 0 register convention.
            let abs_mres = mres.abs();
            if rec.limits.rllm != 0.0 {
                rec.limits.dhlm = rec.limits.rllm * abs_mres;
            }
            rec.limits.rllm = rec.limits.dhlm / abs_mres;
            if rec.limits.rhlm != 0.0 {
                rec.limits.dllm = rec.limits.rhlm * abs_mres;
            }
            rec.limits.rhlm = rec.limits.dllm / abs_mres;
        }
        // Step 2 (C 716-718 via set_dial_high/lowlimit 4243/4294):
        // signed re-derivation, crossed under MRES < 0.
        let (raw_high, raw_low) = (rec.limits.dhlm / mres, rec.limits.dllm / mres);
        if mres < 0.0 {
            rec.limits.rllm = raw_high;
            rec.limits.rhlm = raw_low;
        } else {
            rec.limits.rhlm = raw_high;
            rec.limits.rllm = raw_low;
        }
    }
    rec.set_userlimits();
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

/// Which side of the soft-travel-limit pair a put landed on, in the
/// dial frame (after the DIR fold for user-limit puts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialLimit {
    High,
    Low,
}

/// Queue the driver forward for a soft-limit put (C set_dial_highlimit
/// 4236-4277 / set_dial_lowlimit 4287-4328 send SET_HIGH/LOW_LIMIT from
/// special() before the pp pass runs do_work; `do_process` drains the
/// buffer in front of the pass's own commands). Init-gated like the raw
/// helpers: during dbLoadRecords C applies field() as raw struct writes
/// and special never runs, so a load must not emit. The boundary speaks
/// dial-frame EGU (like `MotorCommand::SetPosition`) — C's raw-steps
/// wire value and the MRES-sign register swap live only in the raw pair
/// (`update_raw_from_dial_high`/`_low`), so the dial side maps to the
/// command unswapped.
fn queue_limit_forward(rec: &mut MotorRecord, side: DialLimit, dial: f64) {
    if !rec.internal.init_invariants_synced {
        return;
    }
    rec.internal.special_cmds.push(match side {
        DialLimit::High => MotorCommand::SetHighLimit { position: dial },
        DialLimit::Low => MotorCommand::SetLowLimit { position: dial },
    });
}

/// Queue the driver forward for a PID-coefficient put (C special pidcof,
/// motorRecord.cc 3003-3026: build_trans SET_PGAIN/SET_IGAIN/SET_DGAIN
/// at put time). Callers gate on GAIN_SUPPORT and clamp to 0.0–1.0
/// first, matching C's order (gate 3003, clamp 3005-3014, send 3017).
fn queue_pid_gain_forward(rec: &mut MotorRecord, kind: PidGainKind, gain: f64) {
    rec.internal
        .special_cmds
        .push(MotorCommand::SetPidGain { kind, gain });
}

/// C set_dial_highlimit (motorRecord.cc:4243-4275, fd808eb2 PR #206):
/// the raw image of a dial HIGH limit is `dhlm / mres` with the SIGNED
/// resolution; under MRES < 0 it addresses the LOW raw register
/// (SET_LOW_LIMIT -> RLLM). Gated on init: during dbLoadRecords a dial
/// field() must leave the raw pair at 0 so the raw-wins rule at init
/// can tell a loaded RHLM/RLLM apart from a derived one (C applies
/// field() as raw struct writes — special never runs at load).
fn update_raw_from_dial_high(rec: &mut MotorRecord, dial: f64) {
    if !rec.internal.init_invariants_synced || rec.conv.mres == 0.0 {
        return;
    }
    let tmp_raw = dial / rec.conv.mres;
    if rec.conv.mres < 0.0 {
        rec.limits.rllm = tmp_raw;
    } else {
        rec.limits.rhlm = tmp_raw;
    }
}

/// C set_dial_lowlimit (motorRecord.cc:4294-4326): mirror — under
/// MRES < 0 the dial LOW limit addresses the HIGH raw register.
fn update_raw_from_dial_low(rec: &mut MotorRecord, dial: f64) {
    if !rec.internal.init_invariants_synced || rec.conv.mres == 0.0 {
        return;
    }
    let tmp_raw = dial / rec.conv.mres;
    if rec.conv.mres < 0.0 {
        rec.limits.rhlm = tmp_raw;
    } else {
        rec.limits.rllm = tmp_raw;
    }
}

/// Per-field RSET metadata (C motorRecord.cc): get_units (3156-3208),
/// get_precision (3313-3337), get_graphic_double (3213-3258),
/// get_control_double (3263-3308), get_alarm_double (3344-3361).
/// The graphic and control switches are identical in C, so one
/// `limits_for` serves both.
pub(crate) fn metadata_override(
    rec: &MotorRecord,
    field: &str,
) -> epics_base_rs::server::record::FieldMetadataOverride {
    let limits = limits_for(rec, field);
    epics_base_rs::server::record::FieldMetadataOverride {
        units: units_for(rec, field),
        precision: precision_for(rec, field),
        disp_limits: limits,
        ctrl_limits: limits,
        alarm_limits: Some(alarm_limits_for(rec, field)),
    }
}

/// C get_units (motorRecord.cc:3156-3208): velocity-class fields
/// decorate EGU; acceleration/speed fields carry fixed unit strings.
/// Default (`None`) is the bare EGU, which the record-level metadata
/// already serves. Byte-level concat — EGU is not guaranteed UTF-8.
fn units_for(rec: &MotorRecord, field: &str) -> Option<epics_base_rs::types::PvString> {
    use epics_base_rs::types::PvString;
    let decorate = |suffix: &[u8]| PvString::from_bytes([rec.disp.egu.as_bytes(), suffix].concat());
    match field {
        "VELO" | "VMAX" | "BVEL" | "VBAS" | "JVEL" | "HVEL" => Some(decorate(b"/sec")),
        "JAR" => Some(decorate(b"/s/s")),
        "ACCL" | "BACC" => Some(PvString::from_bytes(&b"sec"[..])),
        "S" | "SBAS" | "SBAK" => Some(PvString::from_bytes(&b"rev/sec"[..])),
        "SREV" => Some(PvString::from_bytes(&b"steps/rev"[..])),
        "UREV" => Some(decorate(b"/rev")),
        _ => None,
    }
}

/// C get_precision (motorRecord.cc:3313-3337): raw readbacks and the
/// encoder/motor pulse counts are integers (0), VERS is stamped with
/// 2 digits; every other field seeds PREC and runs recGblGetPrec
/// (recGbl.c:119-141) — integer DBF types force 0, float/double clamp
/// an out-of-range PREC to 15, string fields keep PREC untouched
/// (`None` keeps the record-level PREC).
fn precision_for(rec: &MotorRecord, field: &str) -> Option<i16> {
    match field {
        "RRBV" | "RMP" | "REP" => Some(0),
        "VERS" => Some(2),
        _ => match MOTOR_FIELDS.iter().find(|f| f.name == field)?.dbf_type {
            DbFieldType::Short
            | DbFieldType::Long
            | DbFieldType::Int64
            | DbFieldType::Char
            | DbFieldType::UChar
            | DbFieldType::Enum
            | DbFieldType::UShort
            | DbFieldType::ULong
            | DbFieldType::UInt64 => Some(0),
            DbFieldType::Float | DbFieldType::Double => {
                if (0..=15).contains(&rec.disp.prec) {
                    None
                } else {
                    Some(15)
                }
            }
            DbFieldType::String => None,
        },
    }
}

/// C get_graphic_double (motorRecord.cc:3213-3258) and
/// get_control_double (3263-3308) — identical switches: positions get
/// the matching limit pair, the raw pair divides the dial limits by
/// the SIGNED resolution (crossed under MRES < 0, C 3235-3244), VELO
/// ranges over VMAX/VBAS, and unlisted fields fall back to
/// recGblGetGraphicDouble's type range.
///
/// That fallback is NOT motor's to define: it is `recGbl.c`'s
/// `getMaxRangeValues` table, shared by every record type in C. This used
/// to be a second copy of those constants here, and it had drifted —
/// `DBF_CHAR` was mapped to 255/0, where C's `CHAR_MAX`/`CHAR_MIN`
/// (recGbl.c:377-380) give 127/-128. Delegate to the one owner
/// ([`rec_gbl_get_graphic_double`]) so the constants cannot drift again.
fn limits_for(rec: &MotorRecord, field: &str) -> Option<(f64, f64)> {
    match field {
        "VAL" | "RBV" => Some((rec.limits.hlm, rec.limits.llm)),
        "DVAL" | "DRBV" => Some((rec.limits.dhlm, rec.limits.dllm)),
        "RVAL" | "RRBV" => {
            if rec.conv.mres >= 0.0 {
                Some((
                    rec.limits.dhlm / rec.conv.mres,
                    rec.limits.dllm / rec.conv.mres,
                ))
            } else {
                Some((
                    rec.limits.dllm / rec.conv.mres,
                    rec.limits.dhlm / rec.conv.mres,
                ))
            }
        }
        "VELO" => Some((rec.vel.vmax, rec.vel.vbas)),
        _ => {
            let desc = MOTOR_FIELDS.iter().find(|f| f.name == field)?;
            // Same two discriminators C keys on, and the same ones
            // `PropertySupport::narrowed_to_field` takes: a DBF_MENU field is
            // `DbFieldType::Enum` in this port but has no case in C's switch,
            // and a runtime-typed field is DBF_NOACCESS in the .dbd, which C
            // reads statically (recGbl.c:151/:169) and also has no case.
            rec_gbl_get_graphic_double(
                (!desc.runtime_typed).then_some(desc.dbf_type),
                desc.menu.is_some(),
            )
        }
    }
}

/// C get_alarm_double (motorRecord.cc:3344-3361): VAL/DVAL serve
/// HIHI/HIGH/LOW/LOLO unconditionally (no severity gate); every other
/// field gets recGblGetAlarmDouble's "no alarm limits" NaNs
/// (recGbl.c:155-162).
fn alarm_limits_for(rec: &MotorRecord, field: &str) -> (f64, f64, f64, f64) {
    if field == "VAL" || field == "DVAL" {
        (
            rec.alarm.hihi,
            rec.alarm.high,
            rec.alarm.low,
            rec.alarm.lolo,
        )
    } else {
        rec_gbl_get_alarm_double()
    }
}
