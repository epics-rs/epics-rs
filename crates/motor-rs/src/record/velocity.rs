//! The velocity family's C special() (velcheckA/velcheckB, motorRecord.cc
//! 2620-2760, 2855-2875, 3057-3146) and its init reconciliation (C
//! check_speed_and_resolution, 3895-4067), worked on a copy of
//! [`VelocityFields`]. Only a copy whose every value is finite becomes a
//! [`FiniteVelocity`], and only that is stored into the record.
//!
//! A C defect not copied: special() and check_speed_and_resolution store a
//! NaN or infinite speed or acceleration unchecked, and the products and
//! quotients they derive (|UREV|·S 2704, 3958; VELO/|UREV| 2693; ACCL
//! 515; JAR 4061) overflow a finite one; the move and jog then send the
//! non-finite rate. A put is dropped and a load takes the defaults.

use crate::fields::{VelocityFields, m};
use crate::flags::AccsUsed;

use super::MotorRecord;

impl VelocityFields {
    /// Effective base velocity for acceleration math: 0 for a driver that
    /// does not support a base velocity (epics-modules/motor #76).
    pub(crate) fn effective_vbas(&self, vbas_unsupported: bool) -> f64 {
        if vbas_unsupported { 0.0 } else { self.vbas }
    }

    /// Whether every speed and acceleration is finite. Destructured so a
    /// new field cannot be left out.
    fn is_finite(&self) -> bool {
        let VelocityFields {
            velo,
            vbas,
            vmax,
            s,
            sbas,
            smax,
            accl,
            accs,
            accu: _,
            bvel,
            bacc,
            hvel,
            jvel,
            jar,
            sbak,
        } = self;
        [
            velo, vbas, vmax, s, sbas, smax, accl, accs, bvel, bacc, hvel, jvel, jar, sbak,
        ]
        .iter()
        .all(|x| x.is_finite())
    }
}

/// A change to the velocity fields, worked on a copy with what C posts
/// and marks along the way. It reaches the record only through
/// [`VelocityChange::admit`].
pub(super) struct VelocityChange {
    vel: VelocityFields,
    urev_abs: f64,
    vbas_unsupported: bool,
    posted: u64,
    marks: u64,
}

/// A [`VelocityChange`] whose every value is finite: the only writer of
/// the record's velocity fields once init has reconciled them.
pub(super) struct FiniteVelocity(VelocityChange);

impl FiniteVelocity {
    pub(super) fn commit(self, rec: &mut MotorRecord) {
        let change = self.0;
        rec.vel = change.vel;
        rec.internal.put_posted |= change.posted;
        rec.mark(change.marks);
    }
}

impl VelocityChange {
    /// A change starting from `vel` at a UREV of `urev`.
    pub(super) fn new(rec: &MotorRecord, vel: VelocityFields, urev: f64) -> Self {
        Self {
            vel,
            urev_abs: urev.abs(),
            vbas_unsupported: rec.stat.vbas_unsupported,
            posted: 0,
            marks: 0,
        }
    }

    pub(super) fn admit(self) -> Option<FiniteVelocity> {
        self.vel.is_finite().then_some(FiniteVelocity(self))
    }

    /// The acceleration-rate numerator every ACCL↔ACCS conversion divides by
    /// its partner: `(velo - vbas)` while `velo > vbas`, else the full `velo`.
    /// C repeats this ternary in `updateACCLfromACCS` / `updateACCSfromACCL` /
    /// `updateACCL_ACCSfromVELO` (motorRecord.cc:499-546); one owner here so
    /// the three cannot drift.
    fn accel_numerator(&self) -> f64 {
        let vbas = self.vel.effective_vbas(self.vbas_unsupported);
        if self.vel.velo > vbas {
            self.vel.velo - vbas
        } else {
            self.vel.velo
        }
    }

    /// C `updateACCLfromACCS` (motorRecord.cc:499-510): ACCS is the master,
    /// ACCL follows. A non-positive ACCS leaves ACCL alone (C's `accs > 0.0`
    /// guard).
    fn update_accl_from_accs(&mut self) {
        if self.vel.accs > 0.0 {
            self.vel.accl = self.accel_numerator() / self.vel.accs;
        }
    }

    /// C `updateACCSfromACCL` (motorRecord.cc:512-521): ACCL is the master,
    /// ACCS follows, divided by the raw ACCL (515, 539): a negative ACCL,
    /// which init keeps (4041), gives a negative ACCS. An ACCL of 0, derived
    /// from ACCS at a VELO of 0 (503), is C's divide by zero; leaving ACCS
    /// stands in for it.
    fn update_accs_from_accl(&mut self) {
        if self.vel.accl != 0.0 {
            self.vel.accs = self.accel_numerator() / self.vel.accl;
        }
    }

    /// Recalc the slave of ACCL/ACCS after VELO or VBAS changes.
    /// C: `7291b556` (2023-05-19) — when ACCU=Accl, ACCS follows; when
    /// ACCU=Accs, ACCL follows.
    fn apply_accu_cascade(&mut self) {
        // C updateACCL_ACCSfromVELO (motorRecord.cc:523-546): recomputes/posts
        // the ACCU-named slave field in both the velo > vbas and velo <= vbas
        // cases.
        match self.vel.accu {
            AccsUsed::Accl => self.update_accs_from_accl(),
            AccsUsed::Accs => self.update_accl_from_accs(),
        }
    }

    /// C special() `velcheckA` tail (motorRecord.cc:3128-3146): after a
    /// VBAS/SBAS/VMAX/SMAX write changes the velocity window, re-clamp every
    /// dependent velocity into the new [VBAS, VMAX] and re-derive the
    /// rev-unit partners of VELO and BVEL.
    fn revalidate_velocities(&mut self) {
        let urev_abs = self.urev_abs;
        let vel = &mut self.vel;
        // range_check posts what it clamps (4374-4378).
        if range_check(&mut vel.velo, vel.vbas, vel.vmax) {
            self.posted |= m::PUT_VELO;
        }
        if urev_abs > 0.0 {
            vel.s = vel.velo / urev_abs;
        }
        range_check(&mut vel.bvel, vel.vbas, vel.vmax);
        if urev_abs > 0.0 {
            vel.sbak = vel.bvel / urev_abs;
        }
        range_check(&mut vel.jvel, vel.vbas, vel.vmax);
        range_check(&mut vel.hvel, vel.vbas, vel.vmax);
    }

    pub(super) fn velo(&mut self, v: f64) {
        self.vel.velo = v;
        // C motorRecord.cc:2690: VELO is clamped into [VBAS, VMAX].
        if range_check(&mut self.vel.velo, self.vel.vbas, self.vel.vmax) {
            self.posted |= m::PUT_VELO;
        }
        if self.urev_abs > 0.0 {
            self.vel.s = self.vel.velo / self.urev_abs;
        }
        // C: 7291b556 — recalc ACCL/ACCS based on ACCU. Also special()-only:
        // a load must leave the accel pair raw (see `sync_at_init`).
        self.apply_accu_cascade();
    }

    pub(super) fn vbas(&mut self, v: f64) {
        self.vel.vbas = v;
        // C motorRecord.cc:2628-2632: VBAS is forced non-negative.
        if self.vel.vbas < 0.0 {
            self.vel.vbas = 0.0;
            self.posted |= m::PUT_VBAS;
        }
        let urev_abs = self.urev_abs;
        if urev_abs > 0.0 && self.vel.sbas != self.vel.vbas / urev_abs {
            self.vel.sbas = self.vel.vbas / urev_abs;
            self.posted |= m::PUT_SBAS;
        }
        self.apply_accu_cascade();
        // C motorRecord.cc:3121-3127: a VBAS raised above VMAX drags VMAX
        // (and SMAX) up with it.
        if self.vel.vmax != 0.0 && self.vel.vbas > self.vel.vmax {
            self.vel.vmax = self.vel.vbas;
            self.vel.smax = self.vel.sbas;
        }
        self.revalidate_velocities();
    }

    pub(super) fn vmax(&mut self, v: f64) {
        self.vel.vmax = v;
        // C motorRecord.cc:2660-2664: VMAX is forced non-negative.
        if self.vel.vmax < 0.0 {
            self.vel.vmax = 0.0;
        }
        if self.urev_abs > 0.0 {
            self.vel.smax = self.vel.vmax / self.urev_abs;
        }
        // C motorRecord.cc:3110-3117: a VMAX dropped below VBAS drags VBAS
        // (and SBAS) down with it.
        if self.vel.vmax != 0.0 && self.vel.vmax < self.vel.vbas {
            self.vel.vbas = self.vel.vmax;
            self.vel.sbas = self.vel.smax;
            self.marks |= m::VBAS | m::SBAS;
        }
        self.revalidate_velocities();
    }

    pub(super) fn s(&mut self, v: f64) {
        self.vel.s = v;
        // C motorRecord.cc:2702: S is clamped into [SBAS, SMAX].
        range_check(&mut self.vel.s, self.vel.sbas, self.vel.smax);
        let urev_abs = self.urev_abs;
        if urev_abs > 0.0 && self.vel.velo != self.vel.s * urev_abs {
            self.vel.velo = self.vel.s * urev_abs;
            self.posted |= m::PUT_VELO;
        }
        // C motorRecord.cc:2710: ACCL/ACCS follow the VELO change.
        self.apply_accu_cascade();
    }

    pub(super) fn sbas(&mut self, v: f64) {
        self.vel.sbas = v;
        // C motorRecord.cc:2644-2648: SBAS is forced non-negative.
        if self.vel.sbas < 0.0 {
            self.vel.sbas = 0.0;
            self.posted |= m::PUT_SBAS;
        }
        // C 2650 has no UREV guard here.
        let vbas = self.urev_abs * self.vel.sbas;
        if self.vel.vbas != vbas {
            self.vel.vbas = vbas;
            self.posted |= m::PUT_VBAS;
        }
        // C motorRecord.cc:2655: ACCL/ACCS follow the VBAS change.
        self.apply_accu_cascade();
        // C motorRecord.cc:3121-3127 (shared VBAS/SBAS tail).
        if self.vel.vmax != 0.0 && self.vel.vbas > self.vel.vmax {
            self.vel.vmax = self.vel.vbas;
            self.vel.smax = self.vel.sbas;
        }
        self.revalidate_velocities();
    }

    pub(super) fn smax(&mut self, v: f64) {
        self.vel.smax = v;
        // C motorRecord.cc:2675-2679: SMAX is forced non-negative.
        if self.vel.smax < 0.0 {
            self.vel.smax = 0.0;
        }
        if self.urev_abs > 0.0 {
            self.vel.vmax = self.vel.smax * self.urev_abs;
        }
        // C motorRecord.cc:3110-3117 (shared VMAX/SMAX tail).
        if self.vel.vmax != 0.0 && self.vel.vmax < self.vel.vbas {
            self.vel.vbas = self.vel.vmax;
            self.vel.sbas = self.vel.smax;
            self.marks |= m::VBAS | m::SBAS;
        }
        self.revalidate_velocities();
    }

    pub(super) fn accl(&mut self, v: f64) {
        self.vel.accl = v;
        // C special() motorRecordACCL (motorRecord.cc:2735-2742): floor ACCL
        // to 0.1 if <= 0, then updateACCSfromACCL. ACCU is NOT touched —
        // 63bfe5d0 made ACCU a user/autosave control and dropped the
        // 36177f7b auto-switch from the accel helpers. special() never runs
        // during dbLoadRecords, so a load stores the field() value raw and
        // leaves ACCS alone — `sync_at_init` reconciles the pair, keying on
        // "did the .db set ACCS" exactly as C does.
        if self.vel.accl <= 0.0 {
            self.vel.accl = 0.1;
        }
        self.update_accs_from_accl();
    }

    pub(super) fn accs(&mut self, v: f64) {
        // C special() motorRecordACCS (motorRecord.cc:2745-2752): the raw put
        // lands in ACCS; if it is non-positive C derives ACCS from ACCL
        // (updateACCSfromACCL, accs = velo/accl — NOT a literal 1.0), then
        // recomputes ACCL from ACCS (updateACCLfromACCS). ACCU is NOT touched
        // (63bfe5d0 dropped the 36177f7b auto-switch). Runtime-only, as for
        // ACCL.
        self.vel.accs = v;
        if self.vel.accs <= 0.0 {
            self.update_accs_from_accl();
        }
        self.update_accl_from_accs();
    }

    pub(super) fn bvel(&mut self, v: f64) {
        self.vel.bvel = v;
        // C motorRecord.cc:2714: BVEL is clamped into [VBAS, VMAX].
        range_check(&mut self.vel.bvel, self.vel.vbas, self.vel.vmax);
        if self.urev_abs > 0.0 {
            self.vel.sbak = self.vel.bvel / self.urev_abs;
        }
    }

    pub(super) fn bacc(&mut self, v: f64) {
        // C: BACC must be > 0 (forces to 0.1 if <= 0)
        self.vel.bacc = if v <= 0.0 { 0.1 } else { v };
    }

    pub(super) fn hvel(&mut self, v: f64) {
        self.vel.hvel = v;
        // C motorRecord.cc:3081: HVEL is clamped into [VBAS, VMAX].
        range_check(&mut self.vel.hvel, self.vel.vbas, self.vel.vmax);
    }

    /// The clamp only; a JVEL landing on an active jog retunes it once
    /// stored (C 3059-3072, `motor_put_field`).
    pub(super) fn jvel(&mut self, v: f64) {
        self.vel.jvel = v;
        // C motorRecord.cc:3057: JVEL is clamped into [VBAS, VMAX].
        range_check(&mut self.vel.jvel, self.vel.vbas, self.vel.vmax);
    }

    pub(super) fn jar(&mut self, v: f64) {
        self.vel.jar = v;
        // C motorRecord.cc:3074-3078: a non-positive JAR is replaced by
        // JVEL / 0.1. Runtime special() only — during load field() lands raw,
        // and an unconfigured JAR (== 0) is derived from VELO/ACCL at init.
        if self.vel.jar <= 0.0 {
            self.vel.jar = self.vel.jvel / 0.1;
        }
    }

    pub(super) fn sbak(&mut self, v: f64) {
        self.vel.sbak = v;
        // C motorRecord.cc:2725: SBAK is clamped into [SBAS, SMAX].
        range_check(&mut self.vel.sbak, self.vel.sbas, self.vel.smax);
        if self.urev_abs > 0.0 {
            self.vel.bvel = self.vel.sbak * self.urev_abs;
        }
    }

    /// C velcheckB (2855-2875): across a resolution change the rev-unit
    /// speeds are invariant — re-derive the EGU speeds at the new |UREV|.
    pub(super) fn rescale(&mut self) {
        let urev_abs = self.urev_abs;
        let vel = &mut self.vel;
        let velo = urev_abs * vel.s;
        if vel.velo != velo {
            vel.velo = velo;
            self.marks |= m::VELO;
        }
        let vbas = urev_abs * vel.sbas;
        if vel.vbas != vbas {
            vel.vbas = vbas;
            self.marks |= m::VBAS;
        }
        vel.bvel = urev_abs * vel.sbak;
        vel.vmax = urev_abs * vel.smax;
    }

    /// The speed half of C `check_speed_and_resolution` (motorRecord.cc:
    /// 3895), on the fields as `dbLoadRecords` left them; see
    /// `motor_sync_speed_at_init`.
    pub(super) fn sync_at_init(&mut self) {
        let urev_abs = self.urev_abs;
        let vel = &mut self.vel;
        if urev_abs > 0.0 {
            // SMAX (rev/s) <-> VMAX (EGU/s)
            if vel.smax > 0.0 {
                vel.vmax = vel.smax * urev_abs;
            } else if vel.vmax > 0.0 {
                vel.smax = vel.vmax / urev_abs;
            } else {
                vel.smax = 0.0;
                vel.vmax = 0.0;
            }
            // SBAS (rev/s) <-> VBAS (EGU/s)
            if vel.sbas != 0.0 {
                range_check(&mut vel.sbas, 0.0, vel.smax);
                vel.vbas = vel.sbas * urev_abs;
            } else {
                range_check(&mut vel.vbas, 0.0, vel.vmax);
                vel.sbas = vel.vbas / urev_abs;
            }
            // S (rev/s) <-> VELO (EGU/s)
            if vel.s != 0.0 {
                range_check(&mut vel.s, vel.sbas, vel.smax);
                vel.velo = vel.s * urev_abs;
            } else {
                range_check(&mut vel.velo, vel.vbas, vel.vmax);
                vel.s = vel.velo / urev_abs;
            }
            // SBAK (rev/s) <-> BVEL (EGU/s)
            if vel.sbak != 0.0 {
                range_check(&mut vel.sbak, vel.sbas, vel.smax);
                vel.bvel = vel.sbak * urev_abs;
            } else {
                range_check(&mut vel.bvel, vel.vbas, vel.vmax);
                vel.sbak = vel.bvel / urev_abs;
            }
        }
        // ACCS <-> ACCL, C check_speed_and_resolution (motorRecord.cc:
        // 4034-4047). The key is `accs > 0.0` — the loaded ACCS value — NOT
        // ACCU: ACCS's dbd default is 0.0 (as is `VelocityFields::default`),
        // the accel cross-calcs are special()-only and so never fire during
        // dbLoadRecords, and therefore a nonzero ACCS here can only mean the
        // .db wrote `field(ACCS,…)`. It then wins and ACCL is derived from
        // it; otherwise ACCL is the master (a 0 floored to 0.1 first,
        // C:4041-4045) and ACCS is derived. ACCU stays a pure user/autosave
        // control (63bfe5d0) — it selects the master only for the *runtime*
        // VELO/VBAS cascade (`apply_accu_cascade`), not at init.
        if self.vel.accs > 0.0 {
            self.update_accl_from_accs();
        } else {
            if self.vel.accl == 0.0 {
                self.vel.accl = 0.1;
            }
            self.update_accs_from_accl();
        }
        let vel = &mut self.vel;
        // C 4048-4053: the backlash acceleration time's sanity check.
        if vel.bacc == 0.0 {
            vel.bacc = 0.1;
        }

        // C motorRecord.cc:4055-4067 — jog/home velocity sanity checks, after
        // the speed pairs and accelerations settle. A zero field means "not
        // configured" (JVEL/JAR/HVEL have no initial() in the dbd): JVEL
        // defaults to VELO, JAR to VELO/ACCL, HVEL to VBAS; a configured
        // value is clamped into [VBAS, VMAX].
        if vel.jvel == 0.0 {
            vel.jvel = vel.velo;
        } else {
            range_check(&mut vel.jvel, vel.vbas, vel.vmax);
        }
        if vel.jar == 0.0 {
            vel.jar = vel.velo / vel.accl;
        }
        if vel.hvel == 0.0 {
            vel.hvel = vel.vbas;
        } else {
            range_check(&mut vel.hvel, vel.vbas, vel.vmax);
        }
    }
}

/// C `range_check` (motorRecord.cc:4358): clamp `parm` up to `min`, and
/// down to `max` unless `max` is 0 (no ceiling configured).
fn range_check(parm: &mut f64, min: f64, max: f64) -> bool {
    let old = *parm;
    if *parm < min {
        *parm = min;
    }
    if max != 0.0 && *parm > max {
        *parm = max;
    }
    *parm != old
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VELO below VBAS with a stale ACCU slave: a state C reaches only by a
    /// struct write, since special() and init clamp VELO into [VBAS, VMAX]
    /// (motorRecord.cc:2690, 3128-3146), so it is set on the change itself.
    fn below_vbas(accl: f64, accs: f64) -> VelocityChange {
        let vel = VelocityFields {
            velo: 5.0,
            vbas: 8.0,
            accl,
            accs,
            ..VelocityFields::default()
        };
        VelocityChange::new(&MotorRecord::new(), vel, 1.0)
    }

    #[test]
    fn accl_below_vbas_derives_accs_from_the_full_velo() {
        // C updateACCSfromACCL (motorRecord.cc:512-521).
        let mut change = below_vbas(0.2, 99.0);
        change.accl(2.0);
        assert_eq!(change.vel.accu, AccsUsed::Accl);
        assert_eq!(change.vel.accs, 5.0 / 2.0);
    }

    #[test]
    fn accs_below_vbas_derives_accl_from_the_full_velo() {
        // C updateACCLfromACCS (motorRecord.cc:499-510); ACCU stays the
        // master it was (63bfe5d0).
        let mut change = below_vbas(99.0, 0.0);
        change.accs(4.0);
        assert_eq!(change.vel.accu, AccsUsed::Accl);
        assert_eq!(change.vel.accl, 5.0 / 4.0);
    }
}
