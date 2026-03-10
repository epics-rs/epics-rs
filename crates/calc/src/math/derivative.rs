/// Central difference derivative. Boundary points use forward/backward difference.
pub fn deriv(data: &[f64]) -> Vec<f64> {
    let n = data.len();
    if n < 2 {
        return vec![0.0; n];
    }
    let mut result = vec![0.0; n];
    result[0] = data[1] - data[0];
    for i in 1..n - 1 {
        result[i] = (data[i + 1] - data[i - 1]) / 2.0;
    }
    result[n - 1] = data[n - 1] - data[n - 2];
    result
}

/// N-point derivative using least-squares linear fit over a sliding window.
pub fn nderiv(data: &[f64], npts: usize) -> Vec<f64> {
    let n = data.len();
    if n < 2 || npts < 2 {
        return deriv(data);
    }
    let half = npts / 2;
    let mut result = vec![0.0; n];
    for i in 0..n {
        let start = if i >= half { i - half } else { 0 };
        let end = (i + half + 1).min(n);
        let window = &data[start..end];
        let wn = window.len();
        if wn < 2 {
            result[i] = 0.0;
            continue;
        }
        // Linear fit: slope = sum((x-xbar)(y-ybar)) / sum((x-xbar)^2)
        let xbar = (wn - 1) as f64 / 2.0;
        let ybar: f64 = window.iter().sum::<f64>() / wn as f64;
        let mut num = 0.0;
        let mut den = 0.0;
        for (j, &y) in window.iter().enumerate() {
            let x = j as f64 - xbar;
            num += x * (y - ybar);
            den += x * x;
        }
        result[i] = if den.abs() > 1e-30 { num / den } else { 0.0 };
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deriv_linear() {
        // y = 2x -> deriv = 2 everywhere
        let data: Vec<f64> = (0..10).map(|i| 2.0 * i as f64).collect();
        let d = deriv(&data);
        for v in &d {
            assert!((*v - 2.0).abs() < 1e-10);
        }
    }

    #[test]
    fn test_deriv_quadratic() {
        // y = x^2 -> deriv ≈ 2x (central difference)
        let data: Vec<f64> = (0..11).map(|i| (i as f64).powi(2)).collect();
        let d = deriv(&data);
        // Interior points should be close to 2*x
        for i in 1..10 {
            assert!((d[i] - 2.0 * i as f64).abs() < 1e-10, "d[{}]={}", i, d[i]);
        }
    }

    #[test]
    fn test_deriv_empty() {
        assert_eq!(deriv(&[]), Vec::<f64>::new());
    }

    #[test]
    fn test_nderiv_linear() {
        let data: Vec<f64> = (0..10).map(|i| 3.0 * i as f64).collect();
        let d = nderiv(&data, 5);
        for (i, &v) in d.iter().enumerate() {
            assert!((v - 3.0).abs() < 0.5, "nderiv[{}]={}", i, v);
        }
    }
}
