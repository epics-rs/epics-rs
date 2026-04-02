/// Linear fit: returns (slope, intercept).
pub fn lfit(x: &[f64], y: &[f64]) -> (f64, f64) {
    let n = x.len();
    if n < 2 || x.len() != y.len() {
        return (0.0, 0.0);
    }
    let xbar = x.iter().sum::<f64>() / n as f64;
    let ybar = y.iter().sum::<f64>() / n as f64;
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..n {
        let dx = x[i] - xbar;
        num += dx * (y[i] - ybar);
        den += dx * dx;
    }
    if den.abs() < 1e-30 {
        return (0.0, ybar);
    }
    let slope = num / den;
    let intercept = ybar - slope * xbar;
    (slope, intercept)
}

/// Quadratic polynomial fit: returns (a0, a1, a2) where y ≈ a0 + a1*x + a2*x^2.
/// Uses optional mask (if provided, only fits points where mask\[i\] != 0).
pub fn fitpoly(x: &[f64], y: &[f64], mask: Option<&[f64]>) -> (f64, f64, f64) {
    let n = x.len();
    if n < 3 || x.len() != y.len() {
        let (slope, intercept) = lfit(x, y);
        return (intercept, slope, 0.0);
    }

    // Build normal equations: sum x^0..x^4 and sum x^i * y
    let mut sx = [0.0f64; 5]; // sum x^0 through x^4
    let mut sy = [0.0f64; 3]; // sum x^0*y through x^2*y

    for i in 0..n {
        let use_point = match mask {
            Some(m) => m.get(i).map_or(true, |&v| v != 0.0),
            None => true,
        };
        if !use_point {
            continue;
        }
        let xi = x[i];
        let yi = y[i];
        let mut xp = 1.0;
        for j in 0..5 {
            sx[j] += xp;
            if j < 3 {
                sy[j] += xp * yi;
            }
            xp *= xi;
        }
    }

    // Solve 3x3 system via Cramer's rule
    let a = [
        [sx[0], sx[1], sx[2]],
        [sx[1], sx[2], sx[3]],
        [sx[2], sx[3], sx[4]],
    ];
    let det = det3x3(&a);
    if det.abs() < 1e-30 {
        let (slope, intercept) = lfit(x, y);
        return (intercept, slope, 0.0);
    }

    let a0 = det3x3(&[
        [sy[0], sx[1], sx[2]],
        [sy[1], sx[2], sx[3]],
        [sy[2], sx[3], sx[4]],
    ]) / det;
    let a1 = det3x3(&[
        [sx[0], sy[0], sx[2]],
        [sx[1], sy[1], sx[3]],
        [sx[2], sy[2], sx[4]],
    ]) / det;
    let a2 = det3x3(&[
        [sx[0], sx[1], sy[0]],
        [sx[1], sx[2], sy[1]],
        [sx[2], sx[3], sy[2]],
    ]) / det;

    (a0, a1, a2)
}

fn det3x3(m: &[[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lfit_linear() {
        let x: Vec<f64> = (0..10).map(|i| i as f64).collect();
        let y: Vec<f64> = x.iter().map(|&xi| 2.0 * xi + 3.0).collect();
        let (slope, intercept) = lfit(&x, &y);
        assert!((slope - 2.0).abs() < 1e-10, "slope={}", slope);
        assert!((intercept - 3.0).abs() < 1e-10, "intercept={}", intercept);
    }

    #[test]
    fn test_fitpoly_quadratic() {
        let x: Vec<f64> = (0..11).map(|i| i as f64).collect();
        let y: Vec<f64> = x.iter().map(|&xi| 1.0 + 2.0 * xi + 3.0 * xi * xi).collect();
        let (a0, a1, a2) = fitpoly(&x, &y, None);
        assert!((a0 - 1.0).abs() < 1e-6, "a0={}", a0);
        assert!((a1 - 2.0).abs() < 1e-6, "a1={}", a1);
        assert!((a2 - 3.0).abs() < 1e-6, "a2={}", a2);
    }

    #[test]
    fn test_fitpoly_with_mask() {
        let x: Vec<f64> = (0..11).map(|i| i as f64).collect();
        let mut y: Vec<f64> = x.iter().map(|&xi| 1.0 + 2.0 * xi + 3.0 * xi * xi).collect();
        y[5] = 1000.0; // outlier
        let mut mask = vec![1.0; 11];
        mask[5] = 0.0; // mask out outlier
        let (a0, a1, a2) = fitpoly(&x, &y, Some(&mask));
        assert!((a0 - 1.0).abs() < 1e-4, "a0={}", a0);
        assert!((a1 - 2.0).abs() < 1e-3, "a1={}", a1);
        assert!((a2 - 3.0).abs() < 1e-3, "a2={}", a2);
    }

    #[test]
    fn test_lfit_empty() {
        assert_eq!(lfit(&[], &[]), (0.0, 0.0));
    }
}
