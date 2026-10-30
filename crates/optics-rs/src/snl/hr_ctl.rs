//! `hrCtl.st`, compiled by snc-rs from `build.rs` and run on `epics-seq-rs`.
//!
//! The escaped C follows: the `%{ }%` subroutines as Rust, then one
//! `seqg_escaped!` arm per fragment, its text as cpp hands it to snc.

use std::ffi::c_int;

use epics_seq_rs::c::*;

/// `%%  static short  hrCtl_LS_TEST = 0;`: the real limit switches.
const LS_TEST: i16 = 0;

/// `hrCtl_calc2dSpacing`.
fn calc_2d_spacing(v: &seqg::seqg_vars, xtal: c_int, a: f64, h: f64, k: f64, l: f64) -> f64 {
    let result = (2.0 * a) / sqrt(h * h + k * k + l * l);
    v.opAlert.set(0);
    if h % 2.0 != k % 2.0 || h % 2.0 != l % 2.0 {
        v.opAlert.set(1);
    }
    let avg = f64::from(((h + k + l) / 2.0) as f32);
    let nint = avg.round();
    if fabs(avg - nint) <= 0.25 && nint % 2.0 != 0.0 {
        v.opAlert.set(1);
    }
    if v.opAlert.get() != 0 {
        sprintf(
            &v.seqMsg1,
            "Invalid HKL combination for crystal %1d",
            &[CArg::from(xtal)],
        );
    }
    result
}

/// `hrCtl_calcReadback`.
fn calc_readback(v: &seqg::seqg_vars) {
    let (d1, d2, d2r, r2d, ur2d) = (
        v.D1.get(),
        v.D2.get(),
        v.D2R.get(),
        v.R2D.get(),
        v.uR2D.get(),
    );
    let world = v.worldOff.get() * ur2d;
    v.phi1Rdbk
        .set(v.phi1MotRdbk.get() * ur2d + (v.phi1Off.get() + world));
    v.theta1Rdbk.set(v.phi1Rdbk.get());
    let theta1 = v.theta1Rdbk.get();
    let set_phi2_rdbk = || {
        let off = if v.Geom.get() == 0 {
            v.phi2Off.get() + world
        } else {
            v.phi2Off.get() - world
        };
        v.phi2Rdbk.set(v.phi2MotRdbk.get() * ur2d + off);
    };
    match v.opMode.get() {
        0 => v.lambdaRdbk.set(d1 * sin(theta1 * d2r)),
        1 => {
            set_phi2_rdbk();
            v.lambdaRdbk.set(d1 * sin(theta1 * d2r));
        }
        2 => {
            set_phi2_rdbk();
            v.theta2Nom.set(asin(d1 * sin(theta1 * d2r) / d2) * r2d);
            let nom = v.theta2Nom.get();
            let dev = if v.Geom.get() == 0 {
                v.phi2Rdbk.get() - v.phi1Rdbk.get() - theta1 - nom
            } else {
                v.phi2Rdbk.get() - nom
            };
            v.lambdaRdbk.set(
                d1 * sin(theta1 * d2r)
                    + d2r * dev / (1.0 / (d1 * cos(theta1 * d2r)) + 1.0 / (d2 * cos(nom * d2r))),
            );
        }
        _ => {}
    }
    let lambda = v.lambdaRdbk.get();
    v.theta1Rdbk.set(asin(lambda / d1) * r2d);
    if v.opMode.get() != 0 {
        v.theta2Rdbk.set(asin(lambda / d2) * r2d);
    }
    v.ERdbk.set(v.hc.get() / lambda);
}

macro_rules! seqg_escaped {
    (r#"static short hrCtl_LS_TEST = 0;"#) => {};
    (r#"#include <string.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"static double hrCtl_calc2dSpacing(struct UserVar *pVar, int xtal, double A, double H, double K, double L);"#) => {};
    (r#"static void hrCtl_calcReadback(struct UserVar *pVar);"#) => {};
    ($env:ident, r#"printf("hrCtl:init: entry\n");"#) => {
        printf("hrCtl:init: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:initSequence: entry\n");"#) => {
        printf("hrCtl:initSequence: entry\n", &[])
    };
    ($env:ident, r#"char *hrCtl_pmacP = seq_macValueGet(ssId,"P"); char *hrCtl_pmacM = seq_macValueGet(ssId,"M_PHI1"); if (hrCtl_pmacP == 0 || hrCtl_pmacM == 0) { sprintf(pVar->phi1MotName,"??????"); } else { snprintf(pVar->phi1MotName, sizeof(pVar->phi1MotName), "%s%s", hrCtl_pmacP, hrCtl_pmacM); } hrCtl_pmacM = seq_macValueGet(ssId,"M_PHI2"); if (hrCtl_pmacP == 0 || hrCtl_pmacM == 0) { sprintf(pVar->phi2MotName,"??????"); } else { snprintf(pVar->phi2MotName, sizeof(pVar->phi2MotName), "%s%s", hrCtl_pmacP, hrCtl_pmacM); }"#) => {{
        let v = $env.var();
        let p = $env.mac_value_get("P");
        for (name, motor) in [(&v.phi1MotName, "M_PHI1"), (&v.phi2MotName, "M_PHI2")] {
            let m = $env.mac_value_get(motor);
            if p.is_null() || m.is_null() {
                sprintf(name, "??????", &[]);
            } else {
                sprintf(name, "%s%s", &[CArg::from(p.clone()), CArg::from(m)]);
            }
        }
    }};
    (
        $env:ident,
        r#"if (hrCtl_LS_TEST) {"#,
        $test:block,
        r#"} else {"#,
        $real:block,
        r#"}"#
    ) => {
        if LS_TEST != 0 $test else $real
    };
    ($env:ident, r#"printf("hrCtl:initSequence: opMode=%d, Geom=%d, phi2=%f\n", pVar->opMode, pVar->Geom, pVar->phi2);"#) => {{
        let v = $env.var();
        printf(
            "hrCtl:initSequence: opMode=%d, Geom=%d, phi2=%f\n",
            &[
                CArg::from(v.opMode.get()),
                CArg::from(v.Geom.get()),
                CArg::from(v.phi2.get()),
            ],
        )
    }};
    ($env:ident, r#"printf("hrCtl:waitForCmndEnter: entry\n");"#) => {
        printf("hrCtl:waitForCmndEnter: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmdEnter: initSeqDone\n");"#) => {
        printf("hrCtl:waitForCmdEnter: initSeqDone\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmdEnter: Geom=%d, phi2=%f\n", pVar->Geom, pVar->phi2);"#) => {{
        let v = $env.var();
        printf(
            "hrCtl:waitForCmdEnter: Geom=%d, phi2=%f\n",
            &[
                CArg::from(v.Geom.get()),
                CArg::from(v.phi2.get()),
            ],
        )
    }};
    ($env:ident, r#"hrCtl_calcReadback(pVar);"#) => {
        calc_readback($env.var())
    };
    ($env:ident, r#"printf("hrCtl:waitForCmndEnter: Moving=%d; Done=%d\n", pVar->hr_Moving, pVar->hr_Done);"#) => {{
        let v = $env.var();
        printf(
            "hrCtl:waitForCmndEnter: Moving=%d; Done=%d\n",
            &[
                CArg::from(v.hr_Moving.get()),
                CArg::from(v.hr_Done.get()),
            ],
        )
    }};
    ($env:ident, r#"printf("hrCtl:waitForCmndEnter: asserting 'done'.\n");"#) => {
        printf("hrCtl:waitForCmndEnter: asserting 'done'.\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmnd: entry\n");"#) => {
        printf("hrCtl:waitForCmnd: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmd: phi1MotRdbkPseudo_mon\n");"#) => {
        printf("hrCtl:waitForCmd: phi1MotRdbkPseudo_mon\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmd: phi2MotRdbkPseudo_mon\n");"#) => {
        printf("hrCtl:waitForCmd: phi2MotRdbkPseudo_mon\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmd: E_mon\n");"#) => {
        printf("hrCtl:waitForCmd: E_mon\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmd: lambda_mon\n");"#) => {
        printf("hrCtl:waitForCmd: lambda_mon\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmd: theta*_mon\n");"#) => {
        printf("hrCtl:waitForCmd: theta*_mon\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmd: reqUntaken\n");"#) => {
        printf("hrCtl:waitForCmd: reqUntaken\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmd: worldOff_mon\n");"#) => {
        printf("hrCtl:waitForCmd: worldOff_mon\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmd: phi*Off_mon\n");"#) => {
        printf("hrCtl:waitForCmd: phi*Off_mon\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmnd:putVals\n");"#) => {
        printf("hrCtl:waitForCmnd:putVals\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmnd: opMode event.\n");"#) => {
        printf("hrCtl:waitForCmnd: opMode event.\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:waitForCmnd: Geom event.\n");"#) => {
        printf("hrCtl:waitForCmnd: Geom event.\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:checkAutoMode: entry\n");"#) => {
        printf("hrCtl:checkAutoMode: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:phiLimits: entry\n");"#) => {
        printf("hrCtl:phiLimits: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:phiLimits\n");"#) => {
        printf("hrCtl:phiLimits\n", &[])
    };
    ($env:ident, r#"snprintf(pVar->seqMsg2, sizeof(pVar->seqMsg2), "Theta1HiLim clipped at %.3f", pVar->thetaMax);"#) => {{
        let v = $env.var();
        sprintf(&v.seqMsg2, "Theta1HiLim clipped at %.3f", &[CArg::from(v.thetaMax.get())])
    }};
    ($env:ident, r#"snprintf(pVar->seqMsg2, sizeof(pVar->seqMsg2), "Theta1LoLim clipped at %.3f", pVar->thetaMin);"#) => {{
        let v = $env.var();
        sprintf(&v.seqMsg2, "Theta1LoLim clipped at %.3f", &[CArg::from(v.thetaMin.get())])
    }};
    ($env:ident, r#"snprintf(pVar->seqMsg2, sizeof(pVar->seqMsg2), "Theta2HiLim clipped at %.3f", pVar->thetaMax);"#) => {{
        let v = $env.var();
        sprintf(&v.seqMsg2, "Theta2HiLim clipped at %.3f", &[CArg::from(v.thetaMax.get())])
    }};
    ($env:ident, r#"snprintf(pVar->seqMsg2, sizeof(pVar->seqMsg2), "Theta2LoLim clipped at %.3f", pVar->thetaMin);"#) => {{
        let v = $env.var();
        sprintf(&v.seqMsg2, "Theta2LoLim clipped at %.3f", &[CArg::from(v.thetaMin.get())])
    }};
    ($env:ident, r#"printf("hrCtl:checkDone: entry\n");"#) => {
        printf("hrCtl:checkDone: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:checkDone: done\n");"#) => {
        printf("hrCtl:checkDone: done\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:checkDone: not done\n");"#) => {
        printf("hrCtl:checkDone: not done\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:dInputChanged: entry\n");"#) => {
        printf("hrCtl:dInputChanged: entry\n", &[])
    };
    ($env:ident, r#"pVar->D1 = hrCtl_calc2dSpacing(pVar, 1, pVar->A1, pVar->H1, pVar->K1, pVar->L1);"#) => {{
        let v = $env.var();
        let d = calc_2d_spacing(v, 1, v.A1.get(), v.H1.get(), v.K1.get(), v.L1.get());
        v.D1.set(d);
    }};
    ($env:ident, r#"pVar->D2 = hrCtl_calc2dSpacing(pVar, 2, pVar->A2, pVar->H2, pVar->K2, pVar->L2);"#) => {{
        let v = $env.var();
        let d = calc_2d_spacing(v, 2, v.A2.get(), v.H2.get(), v.K2.get(), v.L2.get());
        v.D2.set(d);
    }};
    ($env:ident, r#"printf("hrCtl:tweak: entry\n");"#) => {
        printf("hrCtl:tweak: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:eChanged: entry\n");"#) => {
        printf("hrCtl:eChanged: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:eChanged\n");"#) => {
        printf("hrCtl:eChanged\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:lChanged: entry\n");"#) => {
        printf("hrCtl:lChanged: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:lChanged, opMode 0\n");"#) => {
        printf("hrCtl:lChanged, opMode 0\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:lChanged, opMode 1\n");"#) => {
        printf("hrCtl:lChanged, opMode 1\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:lChanged, opMode 2\n");"#) => {
        printf("hrCtl:lChanged, opMode 2\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:thChanged: entry\n");"#) => {
        printf("hrCtl:thChanged: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:thChanged\n");"#) => {
        printf("hrCtl:thChanged\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:thChanged: opMode=%d, Geom=%d, theta2=%f\n", pVar->opMode, pVar->Geom, pVar->theta2);"#) => {{
        let v = $env.var();
        printf(
            "hrCtl:thChanged: opMode=%d, Geom=%d, theta2=%f\n",
            &[
                CArg::from(v.opMode.get()),
                CArg::from(v.Geom.get()),
                CArg::from(v.theta2.get()),
            ],
        )
    }};
    ($env:ident, r#"printf("hrCtl:thChanged: opMode=%d, Geom=%d, phi2=%f\n", pVar->opMode, pVar->Geom, pVar->phi2);"#) => {{
        let v = $env.var();
        printf(
            "hrCtl:thChanged: opMode=%d, Geom=%d, phi2=%f\n",
            &[
                CArg::from(v.opMode.get()),
                CArg::from(v.Geom.get()),
                CArg::from(v.phi2.get()),
            ],
        )
    }};
    ($env:ident, r#"printf("hrCtl:calcMovements: entry\n");"#) => {
        printf("hrCtl:calcMovements: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:calcMovements, USE\n");"#) => {
        printf("hrCtl:calcMovements, USE\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:calcMovements, SET\n");"#) => {
        printf("hrCtl:calcMovements, SET\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:moveHR entry\n");"#) => {
        printf("hrCtl:moveHR entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:moveHR: putting to motors\n");"#) => {
        printf("hrCtl:moveHR: putting to motors\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:moveHR: NOT putting to motors\n");"#) => {
        printf("hrCtl:moveHR: NOT putting to motors\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:updateRdbk: entry\n");"#) => {
        printf("hrCtl:updateRdbk: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:updateRdbk: a motor is moving\n");"#) => {
        printf("hrCtl:updateRdbk: a motor is moving\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:updateRdbk: motors stopped\n");"#) => {
        printf("hrCtl:updateRdbk: motors stopped\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:motorsStopped: entry\n");"#) => {
        printf("hrCtl:motorsStopped: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:motorsStopped (we started them)\n");"#) => {
        printf("hrCtl:motorsStopped (we started them)\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:motorsStopped (we didn't start them)\n");"#) => {
        printf("hrCtl:motorsStopped (we didn't start them)\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:chkMotorLimits: entry\n");"#) => {
        printf("hrCtl:chkMotorLimits: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:chkMotorLimitsExit: entry\n");"#) => {
        printf("hrCtl:chkMotorLimitsExit: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:stopHR: entry\n");"#) => {
        printf("hrCtl:stopHR: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:stoppedHRWait: entry\n");"#) => {
        printf("hrCtl:stoppedHRWait: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:updatePsuedo:init: entry\n");"#) => {
        printf("hrCtl:updatePsuedo:init: entry\n", &[])
    };
    ($env:ident, r#"printf("hrCtl:updatePsuedo:update: entry\n");"#) => {
        printf("hrCtl:updatePsuedo:update: entry\n", &[])
    };
    (r#"static double hrCtl_calc2dSpacing(struct UserVar *pVar, int xtal, double A, double H, double K, double L) { double result; float avg; double nint; result = (2*A)/sqrt(H*H + K*K + L*L); pVar->opAlert = 0; if (((fmod(H,2)) != (fmod(K,2))) || ((fmod(H,2)) != (fmod(L,2)))) pVar->opAlert = 1; avg = (H+K+L)/2.0; nint = round(avg); if ((fabs(avg-nint) <= .25) && (fmod(nint,2))) pVar->opAlert = 1; if (pVar->opAlert) { sprintf(pVar->seqMsg1, "Invalid HKL combination for crystal %1d", xtal); } return(result); } static void hrCtl_calcReadback(struct UserVar *pVar) { pVar->phi1Rdbk = pVar->phi1MotRdbk * pVar->uR2D + (pVar->phi1Off + pVar->worldOff*pVar->uR2D); pVar->theta1Rdbk = pVar->phi1Rdbk; switch (pVar->opMode) { case 0: pVar->lambdaRdbk = pVar->D1*sin(pVar->theta1Rdbk*pVar->D2R); break; case 1: if (pVar->Geom == 0) { pVar->phi2Rdbk = pVar->phi2MotRdbk * pVar->uR2D + (pVar->phi2Off + pVar->worldOff*pVar->uR2D); } else { pVar->phi2Rdbk = pVar->phi2MotRdbk * pVar->uR2D + (pVar->phi2Off - pVar->worldOff*pVar->uR2D); } pVar->lambdaRdbk = pVar->D1*sin(pVar->theta1Rdbk*pVar->D2R); break; case 2: if (pVar->Geom == 0) { pVar->phi2Rdbk = pVar->phi2MotRdbk * pVar->uR2D + (pVar->phi2Off + pVar->worldOff*pVar->uR2D); } else { pVar->phi2Rdbk = pVar->phi2MotRdbk * pVar->uR2D + (pVar->phi2Off - pVar->worldOff*pVar->uR2D); } pVar->theta2Nom = asin(pVar->D1*sin(pVar->theta1Rdbk*pVar->D2R)/pVar->D2) * pVar->R2D; if (pVar->Geom == 0) { pVar->lambdaRdbk = pVar->D1*sin(pVar->theta1Rdbk*pVar->D2R) + pVar->D2R * (pVar->phi2Rdbk - pVar->phi1Rdbk - pVar->theta1Rdbk - pVar->theta2Nom) / ( 1/(pVar->D1*cos(pVar->theta1Rdbk*pVar->D2R)) + 1/(pVar->D2*cos(pVar->theta2Nom*pVar->D2R)) ); } else { pVar->lambdaRdbk = pVar->D1*sin(pVar->theta1Rdbk*pVar->D2R) + pVar->D2R * (pVar->phi2Rdbk - pVar->theta2Nom) / ( 1/(pVar->D1*cos(pVar->theta1Rdbk*pVar->D2R)) + 1/(pVar->D2*cos(pVar->theta2Nom*pVar->D2R)) ); } break; } pVar->theta1Rdbk = asin(pVar->lambdaRdbk/pVar->D1) * pVar->R2D; if (pVar->opMode != 0) pVar->theta2Rdbk = asin(pVar->lambdaRdbk/pVar->D2) * pVar->R2D; pVar->ERdbk = pVar->hc/pVar->lambdaRdbk; return; }"#) => {};
}

epics_seq_rs::include_program!("hrCtl");

#[cfg(test)]
mod tests {
    use epics_seq_rs::ProgramVars;

    use super::*;

    /// Si, in Å.
    const A: f64 = 5.43102;

    /// One case per rule: mixed parity alerts, an odd `(H+K+L)/2` alerts.
    #[test]
    fn calc_2d_spacing_alerts_as_c() {
        for ((h, k, l), two_d, alert) in [
            ((1.0, 1.0, 1.0), 6.271201717948483, 0),
            ((2.0, 2.0, 0.0), 3.840311070759763, 0),
            ((1.0, 1.0, 0.0), 7.680622141519526, 1),
            ((2.0, 2.0, 2.0), 3.1356008589742417, 1),
        ] {
            let v = seqg::seqg_vars::zeroed();
            assert_eq!(calc_2d_spacing(&v, 1, A, h, k, l), two_d);
            assert_eq!(v.opAlert.get(), alert, "({h} {k} {l})");
            let msg = if alert == 1 {
                &b"Invalid HKL combination for crystal 1"[..]
            } else {
                b""
            };
            assert_eq!(v.seqMsg1.to_c_vec(), msg, "({h} {k} {l})");
        }
    }

    /// avg = 2^31 is even; C's NINT cast was undefined there.
    #[test]
    fn calc_2d_spacing_rounds_past_int_range() {
        let v = seqg::seqg_vars::zeroed();
        calc_2d_spacing(&v, 1, A, 4294967296.0, 0.0, 0.0);
        assert_eq!(v.opAlert.get(), 0);
        assert_eq!(v.seqMsg1.to_c_vec(), b"");
    }

    /// Inputs `s` steps along; the expected values are the C subroutine's.
    fn readback(op_mode: i16, geom: i16, s: f64, lambda: f64) -> [f64; 7] {
        let v = seqg::seqg_vars::zeroed();
        v.hc.set(12.3984244);
        // `PI = 4*atan(1.0)` in the init state.
        v.R2D.set(180.0 / std::f64::consts::PI);
        v.uR2D.set(v.R2D.get() / 1000000.0);
        v.D2R.set(std::f64::consts::PI / 180.0);
        v.opMode.set(op_mode);
        v.Geom.set(geom);
        v.D1.set(6.2712 + s * 0.3);
        v.D2.set(3.1356 + s * 0.7);
        v.phi1MotRdbk.set(200000.0 + s * 12345.6);
        v.phi2MotRdbk.set(-150000.0 + s * 23456.7);
        v.phi1Off.set(0.1 * s);
        v.phi2Off.set(-0.2 * s);
        v.worldOff.set(1000.0 * s - 1500.0);
        v.phi2Rdbk.set(-1.0);
        v.theta2Rdbk.set(-1.0);
        v.theta2Nom.set(-1.0);
        v.lambdaRdbk.set(lambda);
        calc_readback(&v);
        [
            v.phi1Rdbk.get(),
            v.phi2Rdbk.get(),
            v.theta1Rdbk.get(),
            v.theta2Rdbk.get(),
            v.theta2Nom.get(),
            v.lambdaRdbk.get(),
            v.ERdbk.get(),
        ]
    }

    /// Both two-crystal geometries, and an opMode with no case, which keeps
    /// lambdaRdbk and leaves phi2Rdbk and theta2Nom alone.
    #[test]
    fn calc_readback_is_c_s() {
        assert_eq!(
            readback(2, 0, 1.0, -1.0),
            [
                12.237858788416633,
                -7.479044905414372,
                -6.47415171215186,
                -11.138031260724732,
                21.293822721966226,
                -0.7409354228060743,
                -16.733475034901996,
            ]
        );
        assert_eq!(
            readback(2, 1, 1.0, -1.0),
            [
                12.237858788416633,
                -7.4217491259012895,
                2.112089922975289,
                3.6200486021395673,
                21.293822721966226,
                0.2421787841370635,
                51.1953367186078,
            ]
        );
        assert_eq!(
            readback(3, 0, 4.0, 1.1),
            [
                14.831798453626005,
                -1.0,
                8.466552993410478,
                10.679933156667214,
                -1.0,
                1.1,
                11.271294909090908,
            ]
        );
    }
}
