//! `kohzuCtl_soft.st`, compiled by snc-rs from `build.rs` and run on
//! `epics-seq-rs`.
//!
//! Its escaped C is kohzuCtl's under the `kohzuCtl_soft_` prefix, so the
//! two functions it defines share [`kohzu_ctl`]'s Rust: the initSequence
//! block and `kohzuCtl_soft_calc2dSpacing`. The `%%` declarations need no
//! Rust, as in kohzuCtl.

use epics_seq_rs::c::*;

use crate::snl::kohzu_ctl::{self, Spacing};

macro_rules! seqg_escaped {
    (r#"short kohzuCtl_soft_LS_TEST = 0;"#) => {};
    (r#"#include <string.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"double kohzuCtl_soft_calc2dSpacing();"#) => {};
    (r#"char *kohzuCtl_soft_pmacP;"#) => {};
    (r#"char *kohzuCtl_soft_pmacM;"#) => {};
    (r#"char *kohzuCtl_soft_pmacGEOM;"#) => {};
    ($env:ident, r#"printf("kohzuCtl:initSequence:initSeq=0\n");"#) => {
        printf("kohzuCtl:initSequence:initSeq=0\n", &[])
    };
    ($env:ident, r#"kohzuCtl_soft_pmacP = seq_macValueGet(ssId,"P"); kohzuCtl_soft_pmacM = seq_macValueGet(ssId,"M_THETA"); if (kohzuCtl_soft_pmacP == 0 || kohzuCtl_soft_pmacM == 0) { sprintf(thetaMotName,"??????"); } else { snprintf(thetaMotName, sizeof(thetaMotName), "%s%s", kohzuCtl_soft_pmacP, kohzuCtl_soft_pmacM); } kohzuCtl_soft_pmacM = seq_macValueGet(ssId,"M_Y"); if (kohzuCtl_soft_pmacP == 0 || kohzuCtl_soft_pmacM == 0) { sprintf(yMotName,"??????"); } else { snprintf(yMotName, sizeof(yMotName), "%s%s", kohzuCtl_soft_pmacP, kohzuCtl_soft_pmacM); } kohzuCtl_soft_pmacM = seq_macValueGet(ssId,"M_Z"); if (kohzuCtl_soft_pmacP == 0 || kohzuCtl_soft_pmacM == 0) { sprintf(zMotName,"??????"); } else { snprintf(zMotName, sizeof(zMotName), "%s%s", kohzuCtl_soft_pmacP, kohzuCtl_soft_pmacM); } kohzuCtl_soft_pmacGEOM = seq_macValueGet(ssId,"GEOM"); geom = 1; if (*kohzuCtl_soft_pmacGEOM == '2') geom = 2;"#) => {
        kohzu_ctl::name_motors(
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
    ($env:ident, r#"printf("kohzuCtl:waitForCmnd:putVals\n");"#) => {
        printf("kohzuCtl:waitForCmnd:putVals\n", &[])
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
    ($env:ident, r#"printf("kohzuCtl:dInputChanged\n");"#) => {
        printf("kohzuCtl:dInputChanged\n", &[])
    };
    ($env:ident, r#"printf("kohzuCtl:thetaLimits\n");"#) => {
        printf("kohzuCtl:thetaLimits\n", &[])
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
    (r#"double kohzuCtl_soft_calc2dSpacing() { double result; result = (2*A)/sqrt(H*H + K*K + L*L); opAlert = 0; if (((fmod(H,2)) != (fmod(K,2))) || ((fmod(H,2)) != (fmod(L,2)))) opAlert = 1; avg = (H+K+L)/2.0; nint = round(avg); if ((fabs(avg-nint) <= .25) && (fmod(nint,2))) opAlert = 1; if (opAlert) { sprintf(seqMsg1, "(H,K,L) is 'forbidden' reflection"); } else { sprintf(seqMsg1, "New d spacing"); } return(result); }"#) => {};
}

epics_seq_rs::include_program!("kohzuCtl_soft");

/// `kohzuCtl_soft_calc2dSpacing` (`kohzuCtl_soft.st:1376-1400`), by the name
/// the SNL code calls, on the `-r` block.
#[allow(non_snake_case)]
fn kohzuCtl_soft_calc2dSpacing() -> f64 {
    let v = seqg::seqg_static_vars();
    let s = Spacing::of(v.A.get(), v.H.get(), v.K.get(), v.L.get());
    s.store(&v.opAlert, &v.avg, &v.nint, &v.seqMsg1)
}
