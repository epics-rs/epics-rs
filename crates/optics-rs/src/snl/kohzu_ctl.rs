//! `kohzuCtl.st`, compiled by snc-rs from `build.rs` and run on
//! `epics-seq-rs`.
//!
//! Its escaped C is `kohzuCtl_calc2dSpacing`, which the SNL calls, the
//! initSequence block that names the motors from the macros and reads
//! GEOM, the bounded "y offset changed" message, and the debug `printf`s.
//! The `%%` declarations need no Rust: the `kohzuCtl_pmac*` pointers are
//! scratch for that block alone, and `kohzuCtl_LS_TEST` is never read.

use epics_seq_rs::c::*;
use epics_seq_rs::{ProgramVars, SeqString, SeqVar, StateSet};

macro_rules! seqg_escaped {
    (r#"short kohzuCtl_LS_TEST = 0;"#) => {};
    (r#"#include <string.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"double kohzuCtl_calc2dSpacing();"#) => {};
    (r#"char *kohzuCtl_pmacP;"#) => {};
    (r#"char *kohzuCtl_pmacM;"#) => {};
    (r#"char *kohzuCtl_pmacGEOM;"#) => {};
    ($env:ident, r#"printf("kohzuCtl:initSequence:initSeq=0\n");"#) => {
        printf("kohzuCtl:initSequence:initSeq=0\n", &[])
    };
    ($env:ident, r#"kohzuCtl_pmacP = seq_macValueGet(ssId,"P"); kohzuCtl_pmacM = seq_macValueGet(ssId,"M_THETA"); if (kohzuCtl_pmacP == 0 || kohzuCtl_pmacM == 0) { sprintf(thetaMotName,"??????"); } else { snprintf(thetaMotName, sizeof(thetaMotName), "%s%s", kohzuCtl_pmacP, kohzuCtl_pmacM); } kohzuCtl_pmacM = seq_macValueGet(ssId,"M_Y"); if (kohzuCtl_pmacP == 0 || kohzuCtl_pmacM == 0) { sprintf(yMotName,"??????"); } else { snprintf(yMotName, sizeof(yMotName), "%s%s", kohzuCtl_pmacP, kohzuCtl_pmacM); } kohzuCtl_pmacM = seq_macValueGet(ssId,"M_Z"); if (kohzuCtl_pmacP == 0 || kohzuCtl_pmacM == 0) { sprintf(zMotName,"??????"); } else { snprintf(zMotName, sizeof(zMotName), "%s%s", kohzuCtl_pmacP, kohzuCtl_pmacM); } kohzuCtl_pmacGEOM = seq_macValueGet(ssId,"GEOM"); geom = 1; if (*kohzuCtl_pmacGEOM == '2') geom = 2;"#) => {
        name_motors(
            $env,
            [
                &$env.var().thetaMotName,
                &$env.var().yMotName,
                &$env.var().zMotName,
            ],
            &$env.var().geom,
        )
    };
    ($env:ident, r#"printf("kohzuCtl:initSequence:initSeq=1\n");"#) => {
        printf("kohzuCtl:initSequence:initSeq=1\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:initSequence:initSeq=2\n");"#) => {
        printf("kohzuCtl:initSequence:initSeq=2\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:initSequence:initSeq=3\n");"#) => {
        printf("kohzuCtl:initSequence:initSeq=3\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmndEnter:initSeqDone\n");"#) => {
        printf("kohzuCtl:waitForCmndEnter:initSeqDone\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmndEnter: kohzuMoving=%d; kohzuDone=%d\n", kohzuMoving, kohzuDone);"#) => {
        printf(
            "kohzuCtl:waitForCmndEnter: kohzuMoving=%d; kohzuDone=%d\n",
            &[
                CArg::from($env.var().kohzuMoving.get()),
                CArg::from($env.var().kohzuDone.get()),
            ],
        )
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmndEnter: asserting 'done'.\n");"#) => {
        printf("kohzuCtl:waitForCmndEnter: asserting 'done'.\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:HKLA_mon\n");"#) => {
        printf("kohzuCtl:waitForCmnd:HKLA_mon\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:thetaMotXxLim_mon\n");"#) => {
        printf("kohzuCtl:waitForCmnd:thetaMotXxLim_mon\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:thetaMotRdbk_mon\n");"#) => {
        printf("kohzuCtl:waitForCmnd:thetaMotRdbk_mon\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:E_mon\n");"#) => {
        printf("kohzuCtl:waitForCmnd:E_mon\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:lambda_mon\n");"#) => {
        printf("kohzuCtl:waitForCmnd:lambda_mon\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:theta_mon\n");"#) => {
        printf("kohzuCtl:waitForCmnd:theta_mon\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:kMbusy\n");"#) => {
        printf("kohzuCtl:waitForCmnd:kMbusy\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:putVals\n");"#) => {
        printf("kohzuCtl:waitForCmnd:putVals\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:Tweak\n");"#) => {
        printf("kohzuCtl:waitForCmnd:Tweak\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:yOffset_mon\n");"#) => {
        printf("kohzuCtl:waitForCmnd:yOffset_mon\n", &[])
    };
    ($env:ident, r#"snprintf(seqMsg1, sizeof(seqMsg1), "y offset changed to %f", yOffset);"#) => {{
        let v = $env.var();
        sprintf(
            &v.seqMsg1,
            "y offset changed to %f",
            &[CArg::from(v.yOffset.get())],
        )
    }};
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:opAck\n");"#) => {
        printf("kohzuCtl:waitForCmnd:opAck\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:tweak:E+\n");"#) => {
        printf("kohzuCtl:tweak:E+\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:tweak:L\n");"#) => {
        printf("kohzuCtl:tweak:L\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:tweak:th\n");"#) => {
        printf("kohzuCtl:tweak:th\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:dInputChanged\n");"#) => {
        printf("kohzuCtl:dInputChanged\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:thetaLimits\n");"#) => {
        printf("kohzuCtl:thetaLimits\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:busyChanged\n to Moving and E changed");"#) => {
        printf("kohzuCtl:busyChanged\n to Moving and E changed", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:busyChanged\n to Moving and lambda changed");"#) => {
        printf("kohzuCtl:busyChanged\n to Moving and lambda changed", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:busyChanged\n to Moving and theta changed");"#) => {
        printf("kohzuCtl:busyChanged\n to Moving and theta changed", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:busyChanged\n to Moving but unclear why");"#) => {
        printf("kohzuCtl:busyChanged\n to Moving but unclear why", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:busyChanged\n to Done");"#) => {
        printf("kohzuCtl:busyChanged\n to Done", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:eChanged\n");"#) => {
        printf("kohzuCtl:eChanged\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:lChanged: lambda>D\n");"#) => {
        printf("kohzuCtl:lChanged: lambda>D\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:lChanged\n");"#) => {
        printf("kohzuCtl:lChanged\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:thChanged\n");"#) => {
        printf("kohzuCtl:thChanged\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:calcMovements\n");"#) => {
        printf("kohzuCtl:calcMovements\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:moveKohzu:auto|put\n");"#) => {
        printf("kohzuCtl:moveKohzu:auto|put\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:moveKohzu\n");"#) => {
        printf("kohzuCtl:moveKohzu\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:checkDone: done\n");"#) => {
        printf("kohzuCtl:checkDone: done\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:checkDone: not done\n");"#) => {
        printf("kohzuCtl:checkDone: not done\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:updateRdbk:thetaMotRdbk==0\n");"#) => {
        printf("kohzuCtl:updateRdbk:thetaMotRdbk==0\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:updateRdbk:*Dmov (done=%d,moving=%d)\n", kohzuDone, kohzuMoving);"#) => {
        printf(
            "kohzuCtl:updateRdbk:*Dmov (done=%d,moving=%d)\n",
            &[
                CArg::from($env.var().kohzuDone.get()),
                CArg::from($env.var().kohzuMoving.get()),
            ],
        )
    };
    ($env:ident, r#"printf("kohzuCtl:updateRdbk: done\n");"#) => {
        printf("kohzuCtl:updateRdbk: done\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:updateRdbk: Th limit switch\n");"#) => {
        printf("kohzuCtl:updateRdbk: Th limit switch\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:updateRdbk: Y limit switch\n");"#) => {
        printf("kohzuCtl:updateRdbk: Y limit switch\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:updateRdbk: Z limit switch\n");"#) => {
        printf("kohzuCtl:updateRdbk: Z limit switch\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:updateRdbk:moving\n");"#) => {
        printf("kohzuCtl:updateRdbk:moving\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:thetaMotStopped:causedMove (done=%d,moving=%d)\n", kohzuDone, kohzuMoving);"#) => {
        printf(
            "kohzuCtl:thetaMotStopped:causedMove (done=%d,moving=%d)\n",
            &[
                CArg::from($env.var().kohzuDone.get()),
                CArg::from($env.var().kohzuMoving.get()),
            ],
        )
    };
    ($env:ident, r#"printf("kohzuCtl:thetaMotStopped: ySpeed = %f\n", ySpeed);"#) => {
        printf(
            "kohzuCtl:thetaMotStopped: ySpeed = %f\n",
            &[CArg::from($env.var().ySpeed.get())],
        )
    };
    ($env:ident, r#"printf("kohzuCtl:thetaMotStopped:!causedMove (done=%d,moving=%d)\n", kohzuDone, kohzuMoving);"#) => {
        printf(
            "kohzuCtl:thetaMotStopped:!causedMove (done=%d,moving=%d)\n",
            &[
                CArg::from($env.var().kohzuDone.get()),
                CArg::from($env.var().kohzuMoving.get()),
            ],
        )
    };
    ($env:ident, r#"printf("kohzuCtl:chkMotorLimits\n");"#) => {
        printf("kohzuCtl:chkMotorLimits\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:chkMotorLimitsExit: OK\n");"#) => {
        printf("kohzuCtl:chkMotorLimitsExit: OK\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:chkMotorLimitsExit: opAlert\n");"#) => {
        printf("kohzuCtl:chkMotorLimitsExit: opAlert\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:stopKohzu\n");"#) => {
        printf("kohzuCtl:stopKohzu\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:stoppedKohzuWait\n");"#) => {
        printf("kohzuCtl:stoppedKohzuWait\n", &[])
    };
    (r#"double kohzuCtl_calc2dSpacing() { double result; result = (2*A)/sqrt(H*H + K*K + L*L); opAlert = 0; if (((fmod(H,2)) != (fmod(K,2))) || ((fmod(H,2)) != (fmod(L,2)))) opAlert = 1; avg = (H+K+L)/2.0; nint = round(avg); if ((fabs(avg-nint) <= .25) && (fmod(nint,2))) opAlert = 1; if (opAlert) { sprintf(seqMsg1, "(H,K,L) is 'forbidden' reflection"); } else { sprintf(seqMsg1, "New d spacing"); } return(result); }"#) => {};
}

epics_seq_rs::include_program!("kohzuCtl");

/// `kohzuCtl_calc2dSpacing` (`kohzuCtl.st:1460-1484`), by the name the SNL
/// code calls, on the `-r` block.
#[allow(non_snake_case)]
fn kohzuCtl_calc2dSpacing() -> f64 {
    let v = seqg::seqg_static_vars();
    let s = Spacing::of(v.A.get(), v.H.get(), v.K.get(), v.L.get());
    s.store(&v.opAlert, &v.avg, &v.nint, &v.seqMsg1)
}

/// initSequence's block (`kohzuCtl.st:540-563`, `kohzuCtl_soft.st:496-519`):
/// each motor is `{P}{M_x}`, or "??????" when either macro is undefined,
/// and geom is 2 only for a GEOM whose first character is '2'.
pub(crate) fn name_motors<V: ProgramVars>(
    env: &StateSet<V>,
    names: [&SeqVar<SeqString>; 3],
    geom: &SeqVar<i16>,
) {
    let p = env.mac_value_get("P");
    for (macro_name, name) in ["M_THETA", "M_Y", "M_Z"].into_iter().zip(names) {
        let m = env.mac_value_get(macro_name);
        if p.is_null() || m.is_null() {
            strcpy(name, "??????");
        } else {
            sprintf(name, "%s%s", &[CArg::from(p.clone()), CArg::from(m)]);
        }
    }
    let geom_macro = env.mac_value_get("GEOM");
    geom.set(1);
    if geom_macro.with_c_bytes(|g| g.first() == Some(&b'2')) {
        geom.set(2);
    }
}

/// What `kohzuCtl_calc2dSpacing` computes: 2A/sqrt(H²+K²+L²), and opAlert
/// for a mixed-parity or odd-half-sum reflection, through C's `float avg`
/// and `double nint`.
#[derive(Debug, PartialEq)]
pub(crate) struct Spacing {
    two_d: f64,
    op_alert: i16,
    avg: f32,
    nint: f64,
}

impl Spacing {
    pub(crate) fn of(a: f64, h: f64, k: f64, l: f64) -> Self {
        let two_d = (2.0 * a) / sqrt(h * h + k * k + l * l);
        let mut op_alert = i16::from(h % 2.0 != k % 2.0 || h % 2.0 != l % 2.0);
        let avg = ((h + k + l) / 2.0) as f32;
        let nint = f64::from(avg).round();
        if fabs(f64::from(avg) - nint) <= 0.25 && nint % 2.0 != 0.0 {
            op_alert = 1;
        }
        Self {
            two_d,
            op_alert,
            avg,
            nint,
        }
    }

    /// The globals the function writes, then its return value.
    pub(crate) fn store(
        &self,
        op_alert: &SeqVar<i16>,
        avg: &SeqVar<f32>,
        nint: &SeqVar<f64>,
        msg1: &SeqVar<SeqString>,
    ) -> f64 {
        op_alert.set(self.op_alert);
        avg.set(self.avg);
        nint.set(self.nint);
        if self.op_alert != 0 {
            strcpy(msg1, "(H,K,L) is 'forbidden' reflection");
        } else {
            strcpy(msg1, "New d spacing");
        }
        self.two_d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(h: f64, k: f64, l: f64) -> bool {
        Spacing::of(5.4309, h, k, l).op_alert != 0
    }

    /// Si(111), a = 5.4309 A: 2d = 2a/sqrt(3), an allowed reflection.
    #[test]
    fn si_111_is_allowed() {
        let s = Spacing::of(5.4309, 1.0, 1.0, 1.0);
        assert!((s.two_d - 6.2712).abs() < 0.01, "{s:?}");
        assert_eq!(s.op_alert, 0);
    }

    /// Mixed parity alerts; all-even HKL alerts when the half sum rounds
    /// to an odd integer.
    #[test]
    fn forbidden_reflections_alert() {
        assert!(alert(1.0, 0.0, 0.0));
        assert!(alert(2.0, 0.0, 0.0));
        assert!(!alert(2.0, 2.0, 0.0));
    }

    /// C's arithmetic: `fmod` keeps the sign, so -1 and 1 differ, and 1.005
    /// is not odd; `avg` is a float rounded half away from zero, so 0.75
    /// is within .25 of nint 1. All-zero HKL is 2A/0 with no alert.
    #[test]
    fn the_alert_follows_c_arithmetic() {
        assert!(alert(-1.0, 1.0, 1.0));
        assert!(alert(1.005, 1.0, 1.0));
        assert!(alert(0.5, 0.5, 0.5));
        assert!(!alert(-2.0, -2.0, 0.0));
        assert_eq!(
            Spacing::of(5.4309, 0.0, 0.0, 0.0),
            Spacing {
                two_d: f64::INFINITY,
                op_alert: 0,
                avg: 0.0,
                nint: 0.0
            }
        );
        let s = Spacing::of(0.0, 0.0, 0.0, 0.0);
        assert!(s.two_d.is_nan() && s.op_alert == 0);
    }

    /// avg = 2^31 is even; C's NINT cast was undefined there.
    #[test]
    fn the_half_sum_rounds_past_int_range() {
        assert!(!alert(4294967296.0, 0.0, 0.0));
    }
}
