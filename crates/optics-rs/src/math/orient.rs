use std::f64::consts::PI;

use super::matrix3::*;

const D2R: f64 = PI / 180.0;

/// Angle constraint for HKL → angles conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    /// TH = TTH/2 (omega = 0).
    OmegaZero = 0,
    /// Use supplied PHI, calculate TH and CHI.
    PhiConst = 1,
    /// Minimize CHI and (PHI − π/2).
    MinChiPhiMinus90 = 2,
}

/// Angle vector indices.
const TTH: usize = 0;
const TH: usize = 1;
const CHI: usize = 2;
const PHI: usize = 3;

/// Avoid division by zero.
fn check_small(x: f64) -> f64 {
    if x.abs() < SMALL {
        if x < 0.0 { -SMALL } else { SMALL }
    } else {
        x
    }
}

/// Rotation matrix about the Z axis.
pub fn calc_rot_z(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[c, s, 0.0], [-s, c, 0.0], [0.0, 0.0, 1.0]]
}

/// Rotation matrix about the Y axis.
pub fn calc_rot_y(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}

/// Calculate HKL from diffraction angles (in degrees).
///
/// Returns `None` if the rotation matrix cannot be inverted.
pub fn angles_to_hkl(angles_deg: &[f64; 4], omtx_inv: &Mat3, a0_inv: &Mat3) -> Option<Vec3> {
    let angles: Vec<f64> = angles_deg.iter().map(|a| a * D2R).collect();

    let vec = [(angles[TTH] / 2.0).sin(), 0.0, 0.0];

    let r1 = calc_rot_z(angles[TH] - angles[TTH] / 2.0);
    let r2 = calc_rot_y(angles[CHI]);
    let r3 = calc_rot_z(angles[PHI]);
    let rot = mult_mat_mat(&mult_mat_mat(&r1, &r2), &r3);

    let rot_i = invert(&rot)?;
    let tmp = mult_mat_mat(&mult_mat_mat(a0_inv, omtx_inv), &rot_i);
    Some(mult_mat_vec(&tmp, &vec))
}

/// Calculate diffraction angles (in degrees) from HKL.
///
/// For `PhiConst`, `angles_deg[PHI]` is used as the supplied phi value.
/// Returns `None` on math errors (NaN results).
pub fn hkl_to_angles(
    hkl: &Vec3,
    a0: &Mat3,
    omtx: &Mat3,
    angles_deg: &mut [f64; 4],
    constraint: Constraint,
) -> Option<()> {
    let mut angles: [f64; 4] = [0.0; 4];
    for i in 0..4 {
        angles[i] = angles_deg[i] * D2R;
    }

    let hklp = mult_mat_vec(omtx, &mult_mat_vec(a0, hkl));

    // Length of HKL vector
    let r = (hklp[0] * hklp[0] + hklp[1] * hklp[1] + hklp[2] * hklp[2]).sqrt();
    angles[TTH] = 2.0 * r.asin();

    match constraint {
        Constraint::MinChiPhiMinus90 => {
            angles[PHI] = (-hklp[0] * hklp[1] / (hklp[2] * hklp[2] + hklp[1] * hklp[1])).atan();
            // fall through to PhiConst logic
            let xx = check_small(hklp[2]);
            angles[CHI] = PI / 2.0
                + (-(hklp[0] * angles[PHI].cos() + hklp[1] * angles[PHI].sin()) / xx).atan();
            let ry = calc_rot_y(angles[CHI]);
            let rz = calc_rot_z(angles[PHI]);
            let tmp = mult_mat_vec(&ry, &mult_mat_vec(&rz, &hklp));
            angles[TH] = tmp[1].atan2(tmp[0]) + angles[TTH] / 2.0;
        }
        Constraint::PhiConst => {
            let xx = check_small(hklp[2]);
            angles[CHI] = PI / 2.0
                + (-(hklp[0] * angles[PHI].cos() + hklp[1] * angles[PHI].sin()) / xx).atan();
            let ry = calc_rot_y(angles[CHI]);
            let rz = calc_rot_z(angles[PHI]);
            let tmp = mult_mat_vec(&ry, &mult_mat_vec(&rz, &hklp));
            angles[TH] = tmp[1].atan2(tmp[0]) + angles[TTH] / 2.0;
        }
        Constraint::OmegaZero => {
            angles[TH] = angles[TTH] / 2.0;
            let xx = check_small((hklp[0] * hklp[0] + hklp[1] * hklp[1]).sqrt());
            angles[CHI] = (hklp[2] / xx).atan();
            let xx = check_small(hklp[0]);
            angles[PHI] = hklp[1].atan2(xx);
        }
    }

    for i in 0..4 {
        angles_deg[i] = angles[i] / D2R;
        if angles_deg[i].is_nan() {
            return None;
        }
    }
    Some(())
}

/// Calculate A0 matrix from lattice parameters and wavelength.
///
/// Lattice spacings `a`, `b`, `c` and `lambda` in same units (e.g. Angstroms).
/// Lattice angles `alpha`, `beta`, `gamma` in degrees.
/// Returns `(a0, a0_inv)` or `None` if singular.
pub fn calc_a0(
    a: f64,
    b: f64,
    c: f64,
    alpha_deg: f64,
    beta_deg: f64,
    gamma_deg: f64,
    lambda: f64,
) -> Option<(Mat3, Mat3)> {
    let alpha = alpha_deg * D2R;
    let beta = beta_deg * D2R;
    let gamma = gamma_deg * D2R;

    let a_vec: Vec3 = [a, 0.0, 0.0];
    let b_vec: Vec3 = [b * gamma.cos(), b * gamma.sin(), 0.0];
    let tmp = (alpha.cos() - beta.cos() * gamma.cos()) / gamma.sin();
    let c_vec: Vec3 = [
        c * beta.cos(),
        c * tmp,
        c * (1.0 - beta.cos() * beta.cos() - tmp * tmp).sqrt(),
    ];

    let factor = lambda / (2.0 * dotcross(&a_vec, &b_vec, &c_vec));
    let bxc = cross(&b_vec, &c_vec);
    let cxa = cross(&c_vec, &a_vec);
    let axb = cross(&a_vec, &b_vec);

    let r: Mat3 = [
        [factor * bxc[0], factor * cxa[0], factor * axb[0]],
        [factor * bxc[1], factor * cxa[1], factor * axb[1]],
        [factor * bxc[2], factor * cxa[2], factor * axb[2]],
    ];
    let r_i = invert(&r)?;
    Some((r, r_i))
}

/// Calculate orientation matrix from two reference reflections.
///
/// Returns `(omtx, omtx_inv)` or `None` if singular.
pub fn calc_omtx(
    v1_hkl: &Vec3,
    v1_angles: &[f64; 4],
    v2_hkl: &Vec3,
    v2_angles: &[f64; 4],
    a0: &Mat3,
    a0_inv: &Mat3,
) -> Option<(Mat3, Mat3)> {
    // Calc Vp
    let v1p = mult_mat_vec(a0, v1_hkl);
    let tmp = mult_mat_vec(a0, v2_hkl);
    let v2p = cross(&v1p, &tmp);
    let v3p = cross(&v2p, &v1p);

    let vp: Mat3 = [
        [v1p[0], v1p[1], v1p[2]],
        [v2p[0], v2p[1], v2p[2]],
        [v3p[0], v3p[1], v3p[2]],
    ];

    // Calc Vpp
    let v1pp_hkl = angles_to_hkl(v1_angles, &IDENTITY, a0_inv)?;
    let v1pp = mult_mat_vec(a0, &v1pp_hkl);

    let v2pp_hkl = angles_to_hkl(v2_angles, &IDENTITY, a0_inv)?;
    let tmp = mult_mat_vec(a0, &v2pp_hkl);
    let v2pp = cross(&v1pp, &tmp);
    let v3pp = cross(&v2pp, &v1pp);

    let vpp: Mat3 = [
        [v1pp[0], v1pp[1], v1pp[2]],
        [v2pp[0], v2pp[1], v2pp[2]],
        [v3pp[0], v3pp[1], v3pp[2]],
    ];

    let vpp_i = invert(&vpp)?;
    let o = mult_mat_mat(&vpp_i, &vp);
    let o_i = invert(&o)?;
    Some((o, o_i))
}

/// Check orientation matrix consistency.
///
/// Returns error in degrees between expected and calculated directions
/// for the second reflection.
pub fn check_omtx(
    v2_hkl: &Vec3,
    v2_angles: &[f64; 4],
    a0: &Mat3,
    a0_inv: &Mat3,
    o_inv: &Mat3,
) -> Option<f64> {
    let v2p = mult_mat_vec(a0, v2_hkl);
    let norm = dot(&v2p, &v2p).sqrt();
    let v2p: Vec3 = [v2p[0] / norm, v2p[1] / norm, v2p[2] / norm];

    let v2pp_hkl = angles_to_hkl(v2_angles, o_inv, a0_inv)?;
    let v2pp = mult_mat_vec(a0, &v2pp_hkl);
    let norm = dot(&v2pp, &v2pp).sqrt();
    let v2pp: Vec3 = [v2pp[0] / norm, v2pp[1] / norm, v2pp[2] / norm];

    Some(dot(&v2p, &v2pp).acos() / D2R)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANGLE_EPS: f64 = 0.01; // degrees

    /// Test data from ~/codes/optics/docs/orient_test.txt
    /// Mode: TH=TTH/2 (OMEGA_ZERO), λ = 0.572495 Å
    const LAMBDA: f64 = 0.572495;

    struct TestCase {
        h: f64,
        k: f64,
        l: f64,
        tth: f64,
        th: f64,
        chi: f64,
        phi: f64,
    }

    fn assert_angles_close(computed: &[f64; 4], expected: &TestCase) {
        assert!(
            (computed[TTH] - expected.tth).abs() < ANGLE_EPS,
            "TTH: {} != {}",
            computed[TTH],
            expected.tth
        );
        assert!(
            (computed[TH] - expected.th).abs() < ANGLE_EPS,
            "TH: {} != {}",
            computed[TH],
            expected.th
        );
        assert!(
            (computed[CHI] - expected.chi).abs() < ANGLE_EPS,
            "CHI: {} != {}",
            computed[CHI],
            expected.chi
        );
        assert!(
            (computed[PHI] - expected.phi).abs() < ANGLE_EPS,
            "PHI: {} != {}",
            computed[PHI],
            expected.phi
        );
    }

    // --- Si cubic: a=b=c=5.431, α=β=γ=90° ---
    const SI_CASES: &[TestCase] = &[
        TestCase {
            h: 4.0,
            k: 0.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 0.0,
        },
        TestCase {
            h: 0.0,
            k: 4.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 90.0,
        },
        TestCase {
            h: 0.0,
            k: 0.0,
            l: 4.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 90.0,
            phi: 0.0,
        },
        TestCase {
            h: -4.0,
            k: 0.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: 180.0,
        },
        TestCase {
            h: 0.0,
            k: -4.0,
            l: 0.0,
            tth: 24.3414,
            th: 12.1707,
            chi: 0.0,
            phi: -90.0,
        },
        TestCase {
            h: 0.0,
            k: 0.0,
            l: -4.0,
            tth: 24.3414,
            th: 12.1707,
            chi: -90.0,
            phi: 0.0,
        },
        TestCase {
            h: 1.0,
            k: 2.0,
            l: 3.0,
            tth: 22.7475,
            th: 11.3738,
            chi: 53.3008,
            phi: 63.4349,
        },
        TestCase {
            h: -1.0,
            k: 2.0,
            l: 3.0,
            tth: 22.7475,
            th: 11.3738,
            chi: 53.3008,
            phi: 116.5651,
        },
        TestCase {
            h: 1.0,
            k: -2.0,
            l: 3.0,
            tth: 22.7475,
            th: 11.3738,
            chi: 53.3008,
            phi: -63.4349,
        },
        TestCase {
            h: 1.0,
            k: 2.0,
            l: -3.0,
            tth: 22.7475,
            th: 11.3738,
            chi: -53.3008,
            phi: 63.4349,
        },
        TestCase {
            h: -1.0,
            k: -2.0,
            l: -3.0,
            tth: 22.7475,
            th: 11.3738,
            chi: -53.3008,
            phi: -116.5651,
        },
    ];

    #[test]
    fn test_si_cubic_hkl_to_angles() {
        let (a0, _a0_inv) = calc_a0(5.431, 5.431, 5.431, 90.0, 90.0, 90.0, LAMBDA).unwrap();
        for tc in SI_CASES {
            let hkl: Vec3 = [tc.h, tc.k, tc.l];
            let mut angles = [0.0; 4];
            hkl_to_angles(&hkl, &a0, &IDENTITY, &mut angles, Constraint::OmegaZero).unwrap();
            assert_angles_close(&angles, tc);
        }
    }

    #[test]
    fn test_si_cubic_round_trip() {
        let (a0, a0_inv) = calc_a0(5.431, 5.431, 5.431, 90.0, 90.0, 90.0, LAMBDA).unwrap();
        for tc in SI_CASES {
            let hkl: Vec3 = [tc.h, tc.k, tc.l];
            let mut angles = [0.0; 4];
            hkl_to_angles(&hkl, &a0, &IDENTITY, &mut angles, Constraint::OmegaZero).unwrap();
            let hkl_back = angles_to_hkl(&angles, &IDENTITY, &a0_inv).unwrap();
            for i in 0..3 {
                assert!(
                    (hkl[i] - hkl_back[i]).abs() < 0.01,
                    "HKL round-trip index {i}: {} != {}",
                    hkl[i],
                    hkl_back[i]
                );
            }
        }
    }

    // --- Be hcp: a=2.2858, b=2.2858, c=3.5843, γ=120° ---
    const BE_CASES: &[TestCase] = &[
        TestCase {
            h: 4.0,
            k: 0.0,
            l: 0.0,
            tth: 70.6770,
            th: 35.3385,
            chi: 0.0,
            phi: 30.0,
        },
        TestCase {
            h: 0.0,
            k: 4.0,
            l: 0.0,
            tth: 70.6770,
            th: 35.3385,
            chi: 0.0,
            phi: 90.0,
        },
        TestCase {
            h: 0.0,
            k: 0.0,
            l: 4.0,
            tth: 37.2588,
            th: 18.6294,
            chi: 90.0,
            phi: 0.0,
        },
    ];

    #[test]
    fn test_be_hcp_hkl_to_angles() {
        let (a0, _) = calc_a0(2.2858, 2.2858, 3.5843, 90.0, 90.0, 120.0, LAMBDA).unwrap();
        for tc in BE_CASES {
            let hkl: Vec3 = [tc.h, tc.k, tc.l];
            let mut angles = [0.0; 4];
            hkl_to_angles(&hkl, &a0, &IDENTITY, &mut angles, Constraint::OmegaZero).unwrap();
            assert_angles_close(&angles, tc);
        }
    }

    // --- VO2 monoclinic: a=5.743, b=4.517, c=5.375, β=122.6° ---
    const VO2_CASES: &[TestCase] = &[
        TestCase {
            h: 4.0,
            k: 0.0,
            l: 0.0,
            tth: 27.3785,
            th: 13.6893,
            chi: 32.6,
            phi: 0.0,
        },
        TestCase {
            h: 0.0,
            k: 4.0,
            l: 0.0,
            tth: 29.3676,
            th: 14.6838,
            chi: 0.0,
            phi: 90.0,
        },
        TestCase {
            h: 0.0,
            k: 0.0,
            l: 4.0,
            tth: 29.2935,
            th: 14.6467,
            chi: 90.0,
            phi: 0.0,
        },
    ];

    #[test]
    fn test_vo2_monoclinic_hkl_to_angles() {
        let (a0, _) = calc_a0(5.743, 4.517, 5.375, 90.0, 122.6, 90.0, LAMBDA).unwrap();
        for tc in VO2_CASES {
            let hkl: Vec3 = [tc.h, tc.k, tc.l];
            let mut angles = [0.0; 4];
            hkl_to_angles(&hkl, &a0, &IDENTITY, &mut angles, Constraint::OmegaZero).unwrap();
            assert_angles_close(&angles, tc);
        }
    }

    #[test]
    fn test_calc_omtx_and_check() {
        let (a0, a0_inv) = calc_a0(5.431, 5.431, 5.431, 90.0, 90.0, 90.0, LAMBDA).unwrap();

        // Use two Si reflections to build OMTX
        let v1_hkl: Vec3 = [4.0, 0.0, 0.0];
        let v1_angles: [f64; 4] = [24.3414, 12.1707, 0.0, 0.0];
        let v2_hkl: Vec3 = [0.0, 4.0, 0.0];
        let v2_angles: [f64; 4] = [24.3414, 12.1707, 0.0, 90.0];

        let (omtx, omtx_inv) =
            calc_omtx(&v1_hkl, &v1_angles, &v2_hkl, &v2_angles, &a0, &a0_inv).unwrap();

        // Check OMTX consistency
        let err = check_omtx(&v2_hkl, &v2_angles, &a0, &a0_inv, &omtx_inv).unwrap();
        assert!(err < 0.1, "OMTX check error: {err} degrees");

        // Use OMTX to convert (1,2,3) and verify round-trip
        let hkl: Vec3 = [1.0, 2.0, 3.0];
        let mut angles = [0.0; 4];
        hkl_to_angles(&hkl, &a0, &omtx, &mut angles, Constraint::OmegaZero).unwrap();
        let hkl_back = angles_to_hkl(&angles, &omtx_inv, &a0_inv).unwrap();
        for i in 0..3 {
            assert!(
                (hkl[i] - hkl_back[i]).abs() < 0.01,
                "OMTX round-trip index {i}: {} != {}",
                hkl[i],
                hkl_back[i]
            );
        }
    }
}
