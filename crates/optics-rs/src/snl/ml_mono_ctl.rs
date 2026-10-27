//! `ml_monoCtl.st`, compiled by snc-rs from `build.rs` and run on
//! `epics-seq-rs`.
//!
//! The escaped C follows: the `%{ }%` subroutine as Rust, then one
//! `seqg_escaped!` arm per fragment, its text as cpp hands it to snc. The
//! program is `-r`, so the C's file-scope variables are the block
//! `seqg::seqg_static_vars()` returns.

use epics_seq_rs::c::*;

/// `ml_monoCtl_calc2dSpacing`, by the name the SNL code calls.
#[allow(non_snake_case)]
fn ml_monoCtl_calc2dSpacing() -> f64 {
    let v = seqg::seqg_static_vars();
    let result = (2.0 * v.D.get()) / v.Order.get();
    v.opAlert.set(0);
    if v.Order.get() < 1.0 {
        v.opAlert.set(1);
    }
    if v.opAlert.get() != 0 {
        sprintf(&v.seqMsg1, "Order == %f?", &[CArg::from(v.Order.get())]);
    } else {
        sprintf(&v.seqMsg1, "New effective d spacing", &[]);
    }
    result
}

macro_rules! seqg_escaped {
    (r#"#include <string.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"#include <stdlib.h>"#) => {};
    (r#"double ml_monoCtl_calc2dSpacing();"#) => {};
    (r#"char *ml_monoCtl_pmacP;"#) => {};
    (r#"char *ml_monoCtl_pmacM;"#) => {};
    (r#"char *ml_monoCtl_pmacGEOM;"#) => {};
    (r#"char *ml_monoCtl_pmacY_OFF;"#) => {};
    ($env:ident, r#"printf("ml_monoCtl:initSequence:initSeq=0\n");"#) => {
        printf("ml_monoCtl:initSequence:initSeq=0\n", &[])
    };
    ($env:ident, r#"ml_monoCtl_pmacP = seq_macValueGet(ssId,"P"); ml_monoCtl_pmacM = seq_macValueGet(ssId,"M_THETA"); if (ml_monoCtl_pmacP == 0 || ml_monoCtl_pmacM == 0) { sprintf(thetaMotName,"None"); } else { snprintf(thetaMotName, sizeof(thetaMotName), "%s%s", ml_monoCtl_pmacP, ml_monoCtl_pmacM); } ml_monoCtl_pmacM = seq_macValueGet(ssId,"M_THETA2"); if (ml_monoCtl_pmacP == 0 || ml_monoCtl_pmacM == 0) { sprintf(theta2MotName,"None"); } else { snprintf(theta2MotName, sizeof(theta2MotName), "%s%s", ml_monoCtl_pmacP, ml_monoCtl_pmacM); } ml_monoCtl_pmacM = seq_macValueGet(ssId,"M_Z"); if (ml_monoCtl_pmacP == 0 || ml_monoCtl_pmacM == 0) { sprintf(zMotName,"None"); } else { snprintf(zMotName, sizeof(zMotName), "%s%s", ml_monoCtl_pmacP, ml_monoCtl_pmacM); } ml_monoCtl_pmacM = seq_macValueGet(ssId,"M_Y"); if (ml_monoCtl_pmacP == 0 || ml_monoCtl_pmacM == 0) { sprintf(yMotName,"None"); } else { snprintf(yMotName, sizeof(yMotName), "%s%s", ml_monoCtl_pmacP, ml_monoCtl_pmacM); } ml_monoCtl_pmacY_OFF = seq_macValueGet(ssId,"Y_OFF"); yOffsetInit = atof(ml_monoCtl_pmacY_OFF); if ((yOffsetInit < 1) || (yOffsetInit > 60.)) yOffsetInit = 35.; ml_monoCtl_pmacGEOM = seq_macValueGet(ssId,"GEOM"); geom = 1;"#) => {{
        let v = $env.var();
        let p = $env.mac_value_get("P");
        for (name, motor) in [
            (&v.thetaMotName, "M_THETA"),
            (&v.theta2MotName, "M_THETA2"),
            (&v.zMotName, "M_Z"),
            (&v.yMotName, "M_Y"),
        ] {
            let m = $env.mac_value_get(motor);
            if p.is_null() || m.is_null() {
                sprintf(name, "None", &[]);
            } else {
                sprintf(name, "%s%s", &[CArg::from(p.clone()), CArg::from(m)]);
            }
        }
        v.yOffsetInit.set(atof($env.mac_value_get("Y_OFF")));
        if v.yOffsetInit.get() < 1.0 || v.yOffsetInit.get() > 60. {
            v.yOffsetInit.set(35.);
        }
        // C fetches GEOM into ml_monoCtl_pmacGEOM and never reads it.
        v.geom.set(1);
    }};
    ($env:ident, r#"printf("ml_monoCtl:initSequence:initSeq=1\n");"#) => {
        printf("ml_monoCtl:initSequence:initSeq=1\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:initSequence:initSeq=2\n");"#) => {
        printf("ml_monoCtl:initSequence:initSeq=2\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:initSequence:initSeq=3\n");"#) => {
        printf("ml_monoCtl:initSequence:initSeq=3\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmndEnter:initSeqDone\n");"#) => {
        printf("ml_monoCtl:waitForCmndEnter:initSeqDone\n", &[])
    };
    (
        $env:ident,
        r#"printf("ml_monoCtl:waitForCmndEnter: ml_monoMoving=%d; ml_monoDone=%d\n","#,
        r#"ml_monoMoving, ml_monoDone);"#
    ) => {{
        let v = $env.var();
        printf(
            "ml_monoCtl:waitForCmndEnter: ml_monoMoving=%d; ml_monoDone=%d\n",
            &[
                CArg::from(v.ml_monoMoving.get()),
                CArg::from(v.ml_monoDone.get()),
            ],
        )
    }};
    ($env:ident, r#"printf("ml_monoCtl:waitForCmndEnter: asserting 'done'.\n");"#) => {
        printf("ml_monoCtl:waitForCmndEnter: asserting 'done'.\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:Order/D_mon\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:Order/D_mon\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:thetaMotXxLim_mon\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:thetaMotXxLim_mon\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:theta2MotXxLim_mon\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:theta2MotXxLim_mon\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:thetaMotRdbk_mon\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:thetaMotRdbk_mon\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:theta2MotRdbk_mon\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:theta2MotRdbk_mon\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:E_mon\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:E_mon\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:lambda_mon\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:lambda_mon\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:theta_mon\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:theta_mon\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:putVals\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:putVals\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:Tweak\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:Tweak\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:yOffset_mon\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:yOffset_mon\n", &[])
    };
    ($env:ident, r#"snprintf(seqMsg1, sizeof(seqMsg1), "y offset changed to %f", yOffset);"#) => {{
        let v = $env.var();
        sprintf(
            &v.seqMsg1,
            "y offset changed to %f",
            &[CArg::from(v.yOffset.get())],
        )
    }};
    ($env:ident, r#"printf("ml_monoCtl:waitForCmnd:opAck\n");"#) => {
        printf("ml_monoCtl:waitForCmnd:opAck\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:tweak:E+\n");"#) => {
        printf("ml_monoCtl:tweak:E+\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:tweak:L\n");"#) => {
        printf("ml_monoCtl:tweak:L\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:tweak:th\n");"#) => {
        printf("ml_monoCtl:tweak:th\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:dInputChanged\n");"#) => {
        printf("ml_monoCtl:dInputChanged\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:thetaLimits\n");"#) => {
        printf("ml_monoCtl:thetaLimits\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:eChanged\n");"#) => {
        printf("ml_monoCtl:eChanged\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:lChanged: lambda>2*D\n");"#) => {
        printf("ml_monoCtl:lChanged: lambda>2*D\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:lChanged\n");"#) => {
        printf("ml_monoCtl:lChanged\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:thChanged\n");"#) => {
        printf("ml_monoCtl:thChanged\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:calcMovements\n");"#) => {
        printf("ml_monoCtl:calcMovements\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:move_ml_mono:auto|put\n");"#) => {
        printf("ml_monoCtl:move_ml_mono:auto|put\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:move_ml_mono\n");"#) => {
        printf("ml_monoCtl:move_ml_mono\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:checkDone: done\n");"#) => {
        printf("ml_monoCtl:checkDone: done\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:checkDone: not done\n");"#) => {
        printf("ml_monoCtl:checkDone: not done\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:updateRdbk:thetaMotRdbk==0\n");"#) => {
        printf("ml_monoCtl:updateRdbk:thetaMotRdbk==0\n", &[])
    };
    (
        $env:ident,
        r#"printf("ml_monoCtl:updateRdbk:*Dmov (done=%d,moving=%d)\n","#,
        r#"ml_monoDone, ml_monoMoving);"#
    ) => {{
        let v = $env.var();
        printf(
            "ml_monoCtl:updateRdbk:*Dmov (done=%d,moving=%d)\n",
            &[
                CArg::from(v.ml_monoDone.get()),
                CArg::from(v.ml_monoMoving.get()),
            ],
        )
    }};
    ($env:ident, r#"printf("ml_monoCtl:updateRdbk: done\n");"#) => {
        printf("ml_monoCtl:updateRdbk: done\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:updateRdbk: Th limit switch\n");"#) => {
        printf("ml_monoCtl:updateRdbk: Th limit switch\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:updateRdbk: Z limit switch\n");"#) => {
        printf("ml_monoCtl:updateRdbk: Z limit switch\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:updateRdbk:moving\n");"#) => {
        printf("ml_monoCtl:updateRdbk:moving\n", &[])
    };
    (
        $env:ident,
        r#"printf("ml_monoCtl:thetaMotStopped:causedMove (done=%d,moving=%d)\n","#,
        r#"ml_monoDone, ml_monoMoving);"#
    ) => {{
        let v = $env.var();
        printf(
            "ml_monoCtl:thetaMotStopped:causedMove (done=%d,moving=%d)\n",
            &[
                CArg::from(v.ml_monoDone.get()),
                CArg::from(v.ml_monoMoving.get()),
            ],
        )
    }};
    (
        $env:ident,
        r#"printf("ml_monoCtl:thetaMotStopped:!causedMove (done=%d,moving=%d)\n","#,
        r#"ml_monoDone, ml_monoMoving);"#
    ) => {{
        let v = $env.var();
        printf(
            "ml_monoCtl:thetaMotStopped:!causedMove (done=%d,moving=%d)\n",
            &[
                CArg::from(v.ml_monoDone.get()),
                CArg::from(v.ml_monoMoving.get()),
            ],
        )
    }};
    ($env:ident, r#"printf("ml_monoCtl:chkMotorLimits\n");"#) => {
        printf("ml_monoCtl:chkMotorLimits\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:chkMotorLimitsExit: OK\n");"#) => {
        printf("ml_monoCtl:chkMotorLimitsExit: OK\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:chkMotorLimitsExit: opAlert\n");"#) => {
        printf("ml_monoCtl:chkMotorLimitsExit: opAlert\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:stop_ml_mono\n");"#) => {
        printf("ml_monoCtl:stop_ml_mono\n", &[])
    };
    ($env:ident, r#"printf("ml_monoCtl:stopped_ml_monoWait\n");"#) => {
        printf("ml_monoCtl:stopped_ml_monoWait\n", &[])
    };
    (r#"double ml_monoCtl_calc2dSpacing() { double result; result = (2*D)/Order; opAlert = 0; if (Order < 1) opAlert = 1; if (opAlert) { snprintf(seqMsg1, sizeof(seqMsg1), "Order == %f?", Order); } else { sprintf(seqMsg1, "New effective d spacing"); } return(result); }"#) => {};
}

epics_seq_rs::include_program!("ml_monoCtl");

#[cfg(test)]
mod tests {
    use super::*;

    /// C divides unconditionally and words the alert with `%f`.
    #[test]
    fn calc_2d_spacing_is_c_s() {
        let v = seqg::seqg_static_vars();
        for (order, two_d, alert, msg) in [
            (1.0, 50.0, 0, "New effective d spacing"),
            (2.0, 25.0, 0, "New effective d spacing"),
            (0.0, f64::INFINITY, 1, "Order == 0.000000?"),
            (0.5, 100.0, 1, "Order == 0.500000?"),
            // C overflowed seqMsg1 by the terminator here.
            (
                -1e21,
                50.0 / -1e21,
                1,
                "Order == -1000000000000000000000.000000",
            ),
        ] {
            v.D.set(25.0);
            v.Order.set(order);
            assert_eq!(ml_monoCtl_calc2dSpacing(), two_d, "Order {order}");
            assert_eq!(v.opAlert.get(), alert, "Order {order}");
            assert_eq!(v.seqMsg1.to_c_vec(), msg.as_bytes(), "Order {order}");
        }
    }
}
