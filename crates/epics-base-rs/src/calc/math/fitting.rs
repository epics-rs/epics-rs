//! The quadratic fit behind aCalc's FITPOLY / FITMPOLY / FITQ / FITMQ
//! (`calcUtil.c:262-303`).

/// C `calcUtil.c:7` — `#define SMALL 1e-8`. This is calcUtil's own constant; it is
/// NOT aCalcPerform's `SMALL` (1e-9, `aCalcPerform.c:56`), and the two are used for
/// different things. Here it is both the MASK threshold and the singular-matrix
/// threshold.
const SMALL: f64 = 1e-8;

/// C `fitpoly(x, y, n, &c, &b, &a, mask)` (`calcUtil.c:263-307`): least-squares fit
/// of `y = c + b*x + a*x*x`, answering `(c, b, a)` — constant first.
///
/// `None` is C's `return(-1)`, and C has exactly two ways to reach it:
///
/// * **fewer than 3 points** (`:271`) — three coefficients need three equations;
/// * **a singular normal matrix** (`:295-297`, via `invert3x3`'s
///   `fabs(det) < SMALL`) —
///   which is what a fully-masked-out window, or one with fewer than 3 unmasked
///   points, produces.
///
/// There is no linear-fit fallback in C. The port used to drop to `lfit` in both
/// cases and answer a straight line with `a = 0` and a status of 0; C's callers
/// instead leave their coefficients at 0, emit an all-zero curve and return -1.
///
/// The mask is a THRESHOLD, not a truthiness test: a point takes part only where
/// `mask[i] > SMALL` (`:280`). An element of 1e-9 is BELOW the threshold and masks
/// its point OUT, where a `!= 0.0` test would have kept it. Negative mask elements
/// mask out too.
pub fn fitpoly(x: &[f64], y: &[f64], mask: Option<&[f64]>) -> Option<(f64, f64, f64)> {
    let n = x.len().min(y.len());
    if n < 3 {
        return None;
    }

    let mut beta = [0.0f64; 3];
    // C accumulates only the upper triangle plus alpha[0][1], then mirrors.
    let (mut a00, mut a01, mut a11, mut a12, mut a22) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    for i in 0..n {
        if let Some(m) = mask
            && !admits(m[i])
        {
            continue;
        }
        let (xi, yi) = (x[i], y[i]);
        beta[0] += yi;
        beta[1] += yi * xi;
        beta[2] += yi * xi * xi;
        a00 += 1.0;
        a01 += xi;
        a11 += xi * xi;
        a12 += xi * xi * xi;
        a22 += xi * xi * xi * xi;
    }
    // C `:290-292`: alpha[1][0]=alpha[0][1]; alpha[2][0]=alpha[0][2]=alpha[1][1];
    // alpha[2][1]=alpha[1][2].
    let alpha = [[a00, a01, a11], [a01, a11, a12], [a11, a12, a22]];

    let inv = invert3x3(&alpha)?;
    // C `:299-302`: c/b/a are columns 0/1/2 of `beta * inverse`.
    let mut c = 0.0;
    let mut b = 0.0;
    let mut a = 0.0;
    for j in 0..3 {
        c += beta[j] * inv[j][0];
        b += beta[j] * inv[j][1];
        a += beta[j] * inv[j][2];
    }
    Some((c, b, a))
}

/// C's admission test, `mask[i] > SMALL` (`calcUtil.c:280`). Spelled through
/// `partial_cmp` so a NaN mask element answers false — as C's `>` does — rather
/// than through `<= SMALL`, which would answer false for NaN and let the point IN.
fn admits(m: f64) -> bool {
    matches!(m.partial_cmp(&SMALL), Some(std::cmp::Ordering::Greater))
}

/// C `invert3x3` (`calcUtil.c:77-95`). `None` is its `return(-1)`, and the threshold
/// is `fabs(det) < SMALL` — 1e-8, an absolute test on an UNSCALED determinant, so
/// it is not a condition-number check and a merely small-but-invertible matrix is
/// rejected along with a singular one.
fn invert3x3(m: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let (a, b, c) = (m[0][0], m[0][1], m[0][2]);
    let (d, e, f) = (m[1][0], m[1][1], m[1][2]);
    let (g, h, i) = (m[2][0], m[2][1], m[2][2]);
    let det = a * (e * i - h * f) + b * (f * g - i * d) + c * (d * h - g * e);
    if det.abs() < SMALL {
        return None;
    }
    Some([
        [
            (e * i - f * h) / det,
            (c * h - b * i) / det,
            (b * f - c * e) / det,
        ],
        [
            (f * g - d * i) / det,
            (a * i - c * g) / det,
            (c * d - a * f) / det,
        ],
        [
            (d * h - e * g) / det,
            (b * g - a * h) / det,
            (a * e - b * d) / det,
        ],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fitpoly_quadratic() {
        let x: Vec<f64> = (0..11).map(|i| i as f64).collect();
        let y: Vec<f64> = x.iter().map(|&xi| 1.0 + 2.0 * xi + 3.0 * xi * xi).collect();
        let (c, b, a) = fitpoly(&x, &y, None).expect("11 points, non-singular");
        assert!((c - 1.0).abs() < 1e-6, "c={c}");
        assert!((b - 2.0).abs() < 1e-6, "b={b}");
        assert!((a - 3.0).abs() < 1e-6, "a={a}");
    }

    #[test]
    fn test_fitpoly_with_mask() {
        let x: Vec<f64> = (0..11).map(|i| i as f64).collect();
        let mut y: Vec<f64> = x.iter().map(|&xi| 1.0 + 2.0 * xi + 3.0 * xi * xi).collect();
        y[5] = 1000.0; // outlier
        let mut mask = vec![1.0; 11];
        mask[5] = 0.0; // mask out outlier
        let (c, b, a) = fitpoly(&x, &y, Some(&mask)).expect("10 unmasked points");
        assert!((c - 1.0).abs() < 1e-4, "c={c}");
        assert!((b - 2.0).abs() < 1e-3, "b={b}");
        assert!((a - 3.0).abs() < 1e-3, "a={a}");
    }

    /// C `calcUtil.c:280` gates on `mask[i] > SMALL`, so a mask element BELOW 1e-8
    /// masks its point out — an exact `!= 0.0` test would have kept it and let the
    /// outlier back into the fit.
    #[test]
    fn test_mask_is_a_threshold_not_a_truthiness_test() {
        let x: Vec<f64> = (0..11).map(|i| i as f64).collect();
        let mut y: Vec<f64> = x.iter().map(|&xi| 1.0 + 2.0 * xi + 3.0 * xi * xi).collect();
        y[5] = 1000.0;

        let mut mask = vec![1.0; 11];
        mask[5] = 1e-9; // non-zero, but below SMALL
        let (c, _, a) = fitpoly(&x, &y, Some(&mask)).expect("10 unmasked points");
        assert!(
            (c - 1.0).abs() < 1e-4,
            "1e-9 must mask the outlier out: c={c}"
        );
        assert!((a - 3.0).abs() < 1e-3, "a={a}");

        // Negative control: 1e-7 is ABOVE the threshold, so the outlier is in the
        // fit and drags the coefficients away.
        mask[5] = 1e-7;
        let (c, _, _) = fitpoly(&x, &y, Some(&mask)).expect("11 unmasked points");
        assert!(
            (c - 1.0).abs() > 1.0,
            "1e-7 must keep the outlier in: c={c}"
        );
    }

    /// C `:270` — fewer than three points is `return(-1)`, not a linear fallback.
    #[test]
    fn test_fewer_than_three_points_fails() {
        assert_eq!(fitpoly(&[0.0, 1.0], &[1.0, 2.0], None), None);
        assert_eq!(fitpoly(&[], &[], None), None);
    }

    /// A window with fewer than three UNMASKED points leaves the normal matrix
    /// singular, which C rejects in `invert3x3` (`:296`) — again with no fallback.
    #[test]
    fn test_all_masked_out_fails() {
        let x: Vec<f64> = (0..11).map(|i| i as f64).collect();
        let y: Vec<f64> = x.iter().map(|&xi| 1.0 + 2.0 * xi).collect();
        assert_eq!(fitpoly(&x, &y, Some(&[0.0; 11])), None);

        let mut mask = vec![0.0; 11];
        mask[0] = 1.0;
        mask[1] = 1.0;
        assert_eq!(
            fitpoly(&x, &y, Some(&mask)),
            None,
            "two unmasked points cannot determine three coefficients"
        );
    }
}
