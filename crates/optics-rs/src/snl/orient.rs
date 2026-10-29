//! `orient_st.st`, compiled by snc-rs from `build.rs` and run on `epics-seq-rs`.
//!
//! The escaped C is the program's use of `orient.c` and `matrix3.c`: the
//! `#include`s, the calls, and the copies between the matrix and vector
//! arrays and the variables of their PVs. `orient_h` holds the C functions
//! with C's names and arguments, over [`crate::math`]. Their debug prints
//! are not ported: `orientDebug` and `matrix3Debug` in those files are C
//! globals no shell command sets.

use epics_seq_rs::c::*;
use epics_seq_rs::{CharPtr, Place};

use orient_h::*;

/// `orient.h` and `matrix3.h` as the program calls them. An array argument
/// is the variable itself, read and written back as C reads and writes it.
#[allow(non_snake_case, clippy::too_many_arguments)]
mod orient_h {
    use epics_seq_rs::Place;

    use crate::math::matrix3::{IDENTITY, Mat3, Vec3, invert};
    use crate::math::orient::{
        Constraint, angles_to_hkl, calc_a0, calc_omtx, check_omtx, hkl_to_angles,
    };

    pub const TTH_INDEX: usize = 0;
    pub const TH_INDEX: usize = 1;
    pub const CHI_INDEX: usize = 2;
    pub const PHI_INDEX: usize = 3;
    pub const H_INDEX: usize = 0;
    pub const K_INDEX: usize = 1;
    pub const L_INDEX: usize = 2;

    /// A failure zeroes `hkl`.
    pub fn angles_to_HKL(
        angles: &impl Place<[f64; 4]>,
        omtx_inv: &impl Place<Mat3>,
        a0_inv: &impl Place<Mat3>,
        hkl: &impl Place<Vec3>,
    ) -> i32 {
        let r = angles_to_hkl(&angles.get(), &omtx_inv.get(), &a0_inv.get());
        hkl.set(r.unwrap_or([0.0; 3]));
        if r.is_some() { 0 } else { -1 }
    }

    /// All four angles are written; 1 when any is NaN. A constraint other
    /// than `MIN_CHI_PHIm90` (1) or `PHI_CONST` (2) is `OMEGA_ZERO`.
    pub fn HKL_to_angles(
        hkl: &impl Place<Vec3>,
        a0: &impl Place<Mat3>,
        omtx: &impl Place<Mat3>,
        angles: &impl Place<[f64; 4]>,
        constraint: i32,
    ) -> i32 {
        let constraint = match constraint {
            1 => Constraint::MinChiPhiMinus90,
            2 => Constraint::PhiConst,
            _ => Constraint::OmegaZero,
        };
        let (hkl, a0, omtx) = (hkl.get(), a0.get(), omtx.get());
        let mut out = angles.get();
        let r = hkl_to_angles(&hkl, &a0, &omtx, &mut out, constraint);
        angles.set(out);
        i32::from(r.is_none())
    }

    /// A failure leaves both matrices the identity.
    pub fn calc_A0(
        a: f64,
        b: f64,
        c: f64,
        alpha: f64,
        beta: f64,
        gamma: f64,
        lambda: f64,
        a0: &impl Place<Mat3>,
        a0_i: &impl Place<Mat3>,
    ) -> i32 {
        let (r, r_i, status) = match calc_a0(a, b, c, alpha, beta, gamma, lambda) {
            Some((r, r_i)) => (r, r_i, 0),
            None => (IDENTITY, IDENTITY, -1),
        };
        a0.set(r);
        a0_i.set(r_i);
        status
    }

    /// A failure leaves both matrices the identity.
    pub fn calc_OMTX(
        v1_hkl: &impl Place<Vec3>,
        v1_angles: &impl Place<[f64; 4]>,
        v2_hkl: &impl Place<Vec3>,
        v2_angles: &impl Place<[f64; 4]>,
        a0: &impl Place<Mat3>,
        a0_i: &impl Place<Mat3>,
        o: &impl Place<Mat3>,
        o_i: &impl Place<Mat3>,
    ) -> i32 {
        let r = calc_omtx(
            &v1_hkl.get(),
            &v1_angles.get(),
            &v2_hkl.get(),
            &v2_angles.get(),
            &a0.get(),
            &a0_i.get(),
        );
        let (m, m_i, status) = match r {
            Some((m, m_i)) => (m, m_i, 0),
            None => (IDENTITY, IDENTITY, -1),
        };
        o.set(m);
        o_i.set(m_i);
        status
    }

    pub fn checkOMTX(
        v2_hkl: &impl Place<Vec3>,
        v2_angles: &impl Place<[f64; 4]>,
        a0: &impl Place<Mat3>,
        a0_inv: &impl Place<Mat3>,
        o_inv: &impl Place<Mat3>,
    ) -> f64 {
        check_omtx(
            &v2_hkl.get(),
            &v2_angles.get(),
            &a0.get(),
            &a0_inv.get(),
            &o_inv.get(),
        )
    }

    /// A failure leaves `r` as it was.
    pub fn invertArray(a: &impl Place<Mat3>, r: &impl Place<Mat3>) -> i32 {
        match invert(&a.get()) {
            Some(inv) => {
                r.set(inv);
                0
            }
            None => -1,
        }
    }
}

/// `cp = strrchr(s, c)` and then `cp[0] = '\0'`: `s` ends at its last `c`,
/// and `cp`, which pointed there, is the empty string; null when `s` has
/// no `c`.
fn cut_at_last<B: CBuf>(s: &impl Place<B>, c: u8) -> CharPtr {
    let cut = s.modify(|b| {
        let b = b.bytes_mut();
        let len = b.iter().position(|&x| x == 0).unwrap_or(b.len());
        let at = b[..len].iter().rposition(|&x| x == c);
        at.map(|at| b[at] = 0).is_some()
    });
    if cut {
        CharPtr::from_bytes(b"")
    } else {
        CharPtr::default()
    }
}

macro_rules! seqg_escaped {
    (r#"#include <string.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"#include <epicsExport.h>"#) => {};
    (r#"#include "matrix3.h""#) => {};
    (r#"#include "orient.h""#) => {};

    ($env:ident, r#"pVar->orient_pmac_PM = seq_macValueGet(ssId, "PM");"#) => {
        $env.var().orient_pmac_PM.set($env.mac_value_get("PM"))
    };
    ($env:ident, r#"pVar->orient_pmac_mTTH = seq_macValueGet(ssId, "mTTH");"#) => {
        $env.var().orient_pmac_mTTH.set($env.mac_value_get("mTTH"))
    };
    ($env:ident, r#"pVar->orient_pmac_mTH = seq_macValueGet(ssId, "mTH");"#) => {
        $env.var().orient_pmac_mTH.set($env.mac_value_get("mTH"))
    };
    ($env:ident, r#"pVar->orient_pmac_mCHI = seq_macValueGet(ssId, "mCHI");"#) => {
        $env.var().orient_pmac_mCHI.set($env.mac_value_get("mCHI"))
    };
    ($env:ident, r#"pVar->orient_pmac_mPHI = seq_macValueGet(ssId, "mPHI");"#) => {
        $env.var().orient_pmac_mPHI.set($env.mac_value_get("mPHI"))
    };

    ($env:ident, r#"strcpy(pVar->tmpstr, pVar->orient_pmac_PM);"#) => {
        strcpy(&$env.var().tmpstr, &$env.var().orient_pmac_PM)
    };
    ($env:ident, r#"strcat(pVar->tmpstr, pVar->orient_pmac_mTTH);"#) => {
        strcat(&$env.var().tmpstr, &$env.var().orient_pmac_mTTH)
    };
    ($env:ident, r#"strcat(pVar->tmpstr, pVar->orient_pmac_mTH);"#) => {
        strcat(&$env.var().tmpstr, &$env.var().orient_pmac_mTH)
    };
    ($env:ident, r#"strcat(pVar->tmpstr, pVar->orient_pmac_mCHI);"#) => {
        strcat(&$env.var().tmpstr, &$env.var().orient_pmac_mCHI)
    };
    ($env:ident, r#"strcat(pVar->tmpstr, pVar->orient_pmac_mPHI);"#) => {
        strcat(&$env.var().tmpstr, &$env.var().orient_pmac_mPHI)
    };
    ($env:ident, r#"if ((pVar->cp = strrchr(pVar->tmpstr, (int)'.'))) pVar->cp[0] = '\0';"#) => {
        $env.var().cp.set(cut_at_last(&$env.var().tmpstr, b'.'))
    };
    ($env:ident, r#"strcat(pVar->tmpstr, ".STOP");"#) => {
        strcat(&$env.var().tmpstr, ".STOP")
    };
    ($env:ident, r#"strcat(pVar->tmpstr, ".RBV");"#) => {
        strcat(&$env.var().tmpstr, ".RBV")
    };

    (
        $env:ident,
        r#"pVar->i = calc_A0(pVar->a, pVar->b, pVar->c, pVar->alpha, pVar->beta, pVar->Gamma,"#,
        r#"pVar->lambda, pVar->A0, pVar->A0_i);"#
    ) => {{
        let v = $env.var();
        let i = calc_A0(
            v.a.get(),
            v.b.get(),
            v.c.get(),
            v.alpha.get(),
            v.beta.get(),
            v.Gamma.get(),
            v.lambda.get(),
            &v.A0,
            &v.A0_i,
        );
        v.i.set(i);
    }};
    (
        $env:ident,
        r#"pVar->i = calc_A0(pVar->a, pVar->b, pVar->c, pVar->alpha, pVar->beta,"#,
        r#"pVar->Gamma, pVar->lambda, pVar->A0, pVar->A0_i);"#
    ) => {{
        let v = $env.var();
        let i = calc_A0(
            v.a.get(),
            v.b.get(),
            v.c.get(),
            v.alpha.get(),
            v.beta.get(),
            v.Gamma.get(),
            v.lambda.get(),
            &v.A0,
            &v.A0_i,
        );
        v.i.set(i);
    }};
    (
        $env:ident,
        r#"pVar->i = calc_OMTX(pVar->v1_hkl, pVar->v1_angles, pVar->v2_hkl, pVar->v2_angles,"#,
        r#"pVar->A0, pVar->A0_i, pVar->OMTX, pVar->OMTX_i);"#
    ) => {{
        let v = $env.var();
        let i = calc_OMTX(
            &v.v1_hkl,
            &v.v1_angles,
            &v.v2_hkl,
            &v.v2_angles,
            &v.A0,
            &v.A0_i,
            &v.OMTX,
            &v.OMTX_i,
        );
        v.i.set(i);
    }};

    ($env:ident, r#"pVar->A0_11 = pVar->A0[0][0]; pVar->A0_12 = pVar->A0[0][1]; pVar->A0_13 = pVar->A0[0][2];"#) => {{
        let v = $env.var();
        v.A0_11.set(v.A0.at(0).at(0).get());
        v.A0_12.set(v.A0.at(0).at(1).get());
        v.A0_13.set(v.A0.at(0).at(2).get());
    }};
    ($env:ident, r#"pVar->A0_21 = pVar->A0[1][0]; pVar->A0_22 = pVar->A0[1][1]; pVar->A0_23 = pVar->A0[1][2];"#) => {{
        let v = $env.var();
        v.A0_21.set(v.A0.at(1).at(0).get());
        v.A0_22.set(v.A0.at(1).at(1).get());
        v.A0_23.set(v.A0.at(1).at(2).get());
    }};
    ($env:ident, r#"pVar->A0_31 = pVar->A0[2][0]; pVar->A0_32 = pVar->A0[2][1]; pVar->A0_33 = pVar->A0[2][2];"#) => {{
        let v = $env.var();
        v.A0_31.set(v.A0.at(2).at(0).get());
        v.A0_32.set(v.A0.at(2).at(1).get());
        v.A0_33.set(v.A0.at(2).at(2).get());
    }};
    ($env:ident, r#"pVar->OMTX_11 = pVar->OMTX[0][0]; pVar->OMTX_12 = pVar->OMTX[0][1]; pVar->OMTX_13 = pVar->OMTX[0][2];"#) => {{
        let v = $env.var();
        v.OMTX_11.set(v.OMTX.at(0).at(0).get());
        v.OMTX_12.set(v.OMTX.at(0).at(1).get());
        v.OMTX_13.set(v.OMTX.at(0).at(2).get());
    }};
    ($env:ident, r#"pVar->OMTX_21 = pVar->OMTX[1][0]; pVar->OMTX_22 = pVar->OMTX[1][1]; pVar->OMTX_23 = pVar->OMTX[1][2];"#) => {{
        let v = $env.var();
        v.OMTX_21.set(v.OMTX.at(1).at(0).get());
        v.OMTX_22.set(v.OMTX.at(1).at(1).get());
        v.OMTX_23.set(v.OMTX.at(1).at(2).get());
    }};
    ($env:ident, r#"pVar->OMTX_31 = pVar->OMTX[2][0]; pVar->OMTX_32 = pVar->OMTX[2][1]; pVar->OMTX_33 = pVar->OMTX[2][2];"#) => {{
        let v = $env.var();
        v.OMTX_31.set(v.OMTX.at(2).at(0).get());
        v.OMTX_32.set(v.OMTX.at(2).at(1).get());
        v.OMTX_33.set(v.OMTX.at(2).at(2).get());
    }};
    ($env:ident, r#"pVar->A0[0][0] = pVar->A0_11; pVar->A0[0][1] = pVar->A0_12; pVar->A0[0][2] = pVar->A0_13;"#) => {{
        let v = $env.var();
        v.A0.at(0).at(0).set(v.A0_11.get());
        v.A0.at(0).at(1).set(v.A0_12.get());
        v.A0.at(0).at(2).set(v.A0_13.get());
    }};
    ($env:ident, r#"pVar->A0[1][0] = pVar->A0_21; pVar->A0[1][1] = pVar->A0_22; pVar->A0[1][2] = pVar->A0_23;"#) => {{
        let v = $env.var();
        v.A0.at(1).at(0).set(v.A0_21.get());
        v.A0.at(1).at(1).set(v.A0_22.get());
        v.A0.at(1).at(2).set(v.A0_23.get());
    }};
    ($env:ident, r#"pVar->A0[2][0] = pVar->A0_31; pVar->A0[2][1] = pVar->A0_32; pVar->A0[2][2] = pVar->A0_33;"#) => {{
        let v = $env.var();
        v.A0.at(2).at(0).set(v.A0_31.get());
        v.A0.at(2).at(1).set(v.A0_32.get());
        v.A0.at(2).at(2).set(v.A0_33.get());
    }};
    ($env:ident, r#"pVar->OMTX[0][0] = pVar->OMTX_11; pVar->OMTX[0][1] = pVar->OMTX_12; pVar->OMTX[0][2] = pVar->OMTX_13;"#) => {{
        let v = $env.var();
        v.OMTX.at(0).at(0).set(v.OMTX_11.get());
        v.OMTX.at(0).at(1).set(v.OMTX_12.get());
        v.OMTX.at(0).at(2).set(v.OMTX_13.get());
    }};
    ($env:ident, r#"pVar->OMTX[1][0] = pVar->OMTX_21; pVar->OMTX[1][1] = pVar->OMTX_22; pVar->OMTX[1][2] = pVar->OMTX_23;"#) => {{
        let v = $env.var();
        v.OMTX.at(1).at(0).set(v.OMTX_21.get());
        v.OMTX.at(1).at(1).set(v.OMTX_22.get());
        v.OMTX.at(1).at(2).set(v.OMTX_23.get());
    }};
    ($env:ident, r#"pVar->OMTX[2][0] = pVar->OMTX_31; pVar->OMTX[2][1] = pVar->OMTX_32; pVar->OMTX[2][2] = pVar->OMTX_33;"#) => {{
        let v = $env.var();
        v.OMTX.at(2).at(0).set(v.OMTX_31.get());
        v.OMTX.at(2).at(1).set(v.OMTX_32.get());
        v.OMTX.at(2).at(2).set(v.OMTX_33.get());
    }};

    ($env:ident, r#"pVar->v1_hkl[H_INDEX] = pVar->H1; pVar->v1_hkl[K_INDEX] = pVar->K1; pVar->v1_hkl[L_INDEX] = pVar->L1;"#) => {{
        let v = $env.var();
        v.v1_hkl.at(H_INDEX).set(v.H1.get());
        v.v1_hkl.at(K_INDEX).set(v.K1.get());
        v.v1_hkl.at(L_INDEX).set(v.L1.get());
    }};
    ($env:ident, r#"pVar->v1_angles[TTH_INDEX] = pVar->TTH1; pVar->v1_angles[TH_INDEX] = pVar->TH1;"#) => {{
        let v = $env.var();
        v.v1_angles.at(TTH_INDEX).set(v.TTH1.get());
        v.v1_angles.at(TH_INDEX).set(v.TH1.get());
    }};
    ($env:ident, r#"pVar->v1_angles[CHI_INDEX] = pVar->CHI1; pVar->v1_angles[PHI_INDEX] = pVar->PHI1;"#) => {{
        let v = $env.var();
        v.v1_angles.at(CHI_INDEX).set(v.CHI1.get());
        v.v1_angles.at(PHI_INDEX).set(v.PHI1.get());
    }};
    ($env:ident, r#"pVar->v2_hkl[H_INDEX] = pVar->H2; pVar->v2_hkl[K_INDEX] = pVar->K2; pVar->v2_hkl[L_INDEX] = pVar->L2;"#) => {{
        let v = $env.var();
        v.v2_hkl.at(H_INDEX).set(v.H2.get());
        v.v2_hkl.at(K_INDEX).set(v.K2.get());
        v.v2_hkl.at(L_INDEX).set(v.L2.get());
    }};
    ($env:ident, r#"pVar->v2_angles[TTH_INDEX] = pVar->TTH2; pVar->v2_angles[TH_INDEX] = pVar->TH2;"#) => {{
        let v = $env.var();
        v.v2_angles.at(TTH_INDEX).set(v.TTH2.get());
        v.v2_angles.at(TH_INDEX).set(v.TH2.get());
    }};
    ($env:ident, r#"pVar->v2_angles[CHI_INDEX] = pVar->CHI2; pVar->v2_angles[PHI_INDEX] = pVar->PHI2;"#) => {{
        let v = $env.var();
        v.v2_angles.at(CHI_INDEX).set(v.CHI2.get());
        v.v2_angles.at(PHI_INDEX).set(v.PHI2.get());
    }};
    ($env:ident, r#"pVar->hkl[H_INDEX] = pVar->H; pVar->hkl[K_INDEX] = pVar->K; pVar->hkl[L_INDEX] = pVar->L;"#) => {{
        let v = $env.var();
        v.hkl.at(H_INDEX).set(v.H.get());
        v.hkl.at(K_INDEX).set(v.K.get());
        v.hkl.at(L_INDEX).set(v.L.get());
    }};
    ($env:ident, r#"pVar->angles[TTH_INDEX] = pVar->TTH; pVar->angles[TH_INDEX] = pVar->TH;"#) => {{
        let v = $env.var();
        v.angles.at(TTH_INDEX).set(v.TTH.get());
        v.angles.at(TH_INDEX).set(v.TH.get());
    }};
    ($env:ident, r#"pVar->angles[CHI_INDEX] = pVar->CHI; pVar->angles[PHI_INDEX] = pVar->PHI;"#) => {{
        let v = $env.var();
        v.angles.at(CHI_INDEX).set(v.CHI.get());
        v.angles.at(PHI_INDEX).set(v.PHI.get());
    }};
    ($env:ident, r#"pVar->i = HKL_to_angles(pVar->hkl, pVar->A0, pVar->OMTX, pVar->angles, (int)pVar->Mode);"#) => {{
        let v = $env.var();
        let i = HKL_to_angles(&v.hkl, &v.A0, &v.OMTX, &v.angles, i32::from(v.Mode.get()));
        v.i.set(i);
    }};
    ($env:ident, r#"pVar->TTH = pVar->angles[TTH_INDEX];"#) => {
        $env.var().TTH.set($env.var().angles.at(TTH_INDEX).get())
    };
    ($env:ident, r#"pVar->TH = pVar->angles[TH_INDEX];"#) => {
        $env.var().TH.set($env.var().angles.at(TH_INDEX).get())
    };
    ($env:ident, r#"pVar->CHI = pVar->angles[CHI_INDEX];"#) => {
        $env.var().CHI.set($env.var().angles.at(CHI_INDEX).get())
    };
    ($env:ident, r#"pVar->PHI = pVar->angles[PHI_INDEX];"#) => {
        $env.var().PHI.set($env.var().angles.at(PHI_INDEX).get())
    };
    ($env:ident, r#"angles_to_HKL(pVar->angles, pVar->OMTX_i, pVar->A0_i, pVar->hkl);"#) => {{
        let v = $env.var();
        angles_to_HKL(&v.angles, &v.OMTX_i, &v.A0_i, &v.hkl);
    }};
    ($env:ident, r#"pVar->H = pVar->hkl[H_INDEX]; pVar->K = pVar->hkl[K_INDEX]; pVar->L = pVar->hkl[L_INDEX];"#) => {{
        let v = $env.var();
        v.H.set(v.hkl.at(H_INDEX).get());
        v.K.set(v.hkl.at(K_INDEX).get());
        v.L.set(v.hkl.at(L_INDEX).get());
    }};
    ($env:ident, r#"pVar->rbv_angles[TTH_INDEX] = pVar->motTTH_RBV; pVar->rbv_angles[TH_INDEX] = pVar->motTH_RBV;"#) => {{
        let v = $env.var();
        v.rbv_angles.at(TTH_INDEX).set(v.motTTH_RBV.get());
        v.rbv_angles.at(TH_INDEX).set(v.motTH_RBV.get());
    }};
    ($env:ident, r#"pVar->rbv_angles[CHI_INDEX] = pVar->motCHI_RBV; pVar->rbv_angles[PHI_INDEX] = pVar->motPHI_RBV;"#) => {{
        let v = $env.var();
        v.rbv_angles.at(CHI_INDEX).set(v.motCHI_RBV.get());
        v.rbv_angles.at(PHI_INDEX).set(v.motPHI_RBV.get());
    }};
    ($env:ident, r#"angles_to_HKL(pVar->rbv_angles, pVar->OMTX_i, pVar->A0_i, pVar->rbv_hkl);"#) => {{
        let v = $env.var();
        angles_to_HKL(&v.rbv_angles, &v.OMTX_i, &v.A0_i, &v.rbv_hkl);
    }};
    ($env:ident, r#"pVar->H_RBV = pVar->rbv_hkl[H_INDEX]; pVar->K_RBV = pVar->rbv_hkl[K_INDEX]; pVar->L_RBV = pVar->rbv_hkl[L_INDEX];"#) => {{
        let v = $env.var();
        v.H_RBV.set(v.rbv_hkl.at(H_INDEX).get());
        v.K_RBV.set(v.rbv_hkl.at(K_INDEX).get());
        v.L_RBV.set(v.rbv_hkl.at(L_INDEX).get());
    }};
}

epics_seq_rs::include_program!("orient_st");

#[cfg(test)]
mod tests {
    use std::ffi::c_char;

    use epics_seq_rs::SeqVar;

    use super::*;
    use crate::math::matrix3::{IDENTITY, Mat3};

    const SOME: Mat3 = [[2.0, 0.0, 0.0], [0.0, 3.0, 0.0], [0.0, 0.0, 4.0]];

    /// orient.c:188-200: on failure both matrices are reset to the identity.
    #[test]
    fn a_failed_calc_a0_leaves_the_identity() {
        let (a0, a0_i) = (SeqVar::new(SOME), SeqVar::new(SOME));
        let i = calc_A0(0.0, 5.431, 5.431, 90.0, 90.0, 90.0, 0.5725, &a0, &a0_i);
        assert_eq!(i, -1);
        assert_eq!(a0.get(), IDENTITY);
        assert_eq!(a0_i.get(), IDENTITY);
    }

    /// orient.c:279-285: as calc_A0.
    #[test]
    fn a_failed_calc_omtx_leaves_the_identity() {
        let (o, o_i) = (SeqVar::new(SOME), SeqVar::new(SOME));
        let zero_hkl = SeqVar::new([0.0; 3]);
        let zero_angles = SeqVar::new([0.0; 4]);
        let i = calc_OMTX(
            &zero_hkl,
            &zero_angles,
            &zero_hkl,
            &zero_angles,
            &SeqVar::new(IDENTITY),
            &SeqVar::new(IDENTITY),
            &o,
            &o_i,
        );
        assert_eq!(i, -1);
        assert_eq!(o.get(), IDENTITY);
        assert_eq!(o_i.get(), IDENTITY);
    }

    /// orient.c:63-69: a NaN angle is the one way the rotation fails
    /// invertArray, and hkl is zeroed.
    #[test]
    fn a_failed_angles_to_hkl_zeroes_hkl() {
        let hkl = SeqVar::new([1.0, 2.0, 3.0]);
        let m = SeqVar::new(IDENTITY);
        let angles = SeqVar::new([f64::NAN, 0.0, 0.0, 0.0]);
        let i = angles_to_HKL(&angles, &m, &m, &hkl);
        assert_eq!(i, -1);
        assert_eq!(hkl.get(), [0.0; 3]);
    }

    /// orient.c:101-119: a Mode other than 1 or 2 is OMEGA_ZERO.
    #[test]
    fn an_unknown_mode_is_omega_zero() {
        let run = |mode: i32| {
            let angles = SeqVar::new([0.0, 0.0, 0.0, 30.0]);
            let hkl = SeqVar::new([0.2, 0.3, 0.1]);
            let m = SeqVar::new(IDENTITY);
            assert_eq!(HKL_to_angles(&hkl, &m, &m, &angles, mode), 0);
            angles.get()
        };
        assert_eq!(run(3), run(0));
        assert_eq!(run(-1), run(0));
        assert_ne!(run(2), run(0));
    }

    /// orient.c:128-132: `errno || err`, as 1, with all four written.
    #[test]
    fn a_nan_angle_returns_one() {
        let angles = SeqVar::new([5.0, 5.0, 5.0, 5.0]);
        let m = SeqVar::new(IDENTITY);
        let i = HKL_to_angles(&SeqVar::new([2.0, 0.0, 0.0]), &m, &m, &angles, 0);
        assert_eq!(i, 1);
        let a = angles.get();
        assert!(a[TTH_INDEX].is_nan() && a[TH_INDEX].is_nan());
        assert_eq!((a[CHI_INDEX], a[PHI_INDEX]), (0.0, 0.0));
    }

    /// matrix3.c:91-104: a singular matrix leaves `r` as it was.
    #[test]
    fn a_singular_invert_array_leaves_r() {
        let r = SeqVar::new(SOME);
        assert_eq!(invertArray(&SeqVar::new([[0.0; 3]; 3]), &r), -1);
        assert_eq!(r.get(), SOME);
        assert_eq!(invertArray(&SeqVar::new(SOME), &r), 0);
        assert_eq!(r.get()[0][0], 0.5);
    }

    /// orient.c:298-310: a zero reflection divides by a zero norm.
    #[test]
    fn check_omtx_of_a_zero_reflection_is_nan() {
        let m = SeqVar::new(IDENTITY);
        let d = checkOMTX(&SeqVar::new([0.0; 3]), &SeqVar::new([0.0; 4]), &m, &m, &m);
        assert!(d.is_nan());
    }

    /// orient_st.st's `.STOP`/`.RBV` names: `X:m:tth.VAL` → `X:m:tth`.
    #[test]
    fn the_strrchr_cut_ends_tmpstr_at_its_last_dot() {
        let tmpstr = SeqVar::<[c_char; 100]>::new([0; 100]);
        strcpy(&tmpstr, "X:m:tth.VAL");
        assert_eq!(cut_at_last(&tmpstr, b'.').to_c_vec(), b"");
        strcat(&tmpstr, ".STOP");
        assert_eq!(tmpstr.to_c_vec(), b"X:m:tth.STOP");

        strcpy(&tmpstr, "nodot");
        assert!(cut_at_last(&tmpstr, b'.').is_null());
        assert_eq!(tmpstr.to_c_vec(), b"nodot");
    }
}
