/// Compute the average of a slice. Returns 0.0 for empty input.
pub fn average(data: &[f64]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    data.iter().sum::<f64>() / data.len() as f64
}

/// Compute the standard deviation of a slice. Returns 0.0 for fewer than 2 elements.
pub fn std_dev(data: &[f64]) -> f64 {
    if data.len() < 2 {
        return 0.0;
    }
    let mean = average(data);
    let variance = data.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (data.len() - 1) as f64;
    variance.sqrt()
}

/// Compute the full width at half maximum (FWHM) of a data array.
/// Assumes data represents a peak profile. Returns 0.0 if no valid FWHM found.
pub fn fwhm(data: &[f64]) -> f64 {
    if data.len() < 3 {
        return 0.0;
    }

    // Find the maximum value and its index
    let mut max_val = data[0];
    let mut max_idx = 0;
    for (i, &v) in data.iter().enumerate() {
        if v > max_val {
            max_val = v;
            max_idx = i;
        }
    }

    let min_val = data.iter().cloned().fold(f64::INFINITY, f64::min);
    if (max_val - min_val).abs() < f64::EPSILON {
        return 0.0;
    }
    let half_max = (max_val + min_val) / 2.0;

    // Find left half-max crossing
    let mut left = max_idx as f64;
    for i in (0..max_idx).rev() {
        if data[i] <= half_max {
            // Linear interpolation; guard against a flat segment
            // (data[i+1] == data[i]) that would divide by zero.
            let span = data[i + 1] - data[i];
            let frac = if span == 0.0 {
                0.0
            } else {
                (half_max - data[i]) / span
            };
            left = i as f64 + frac;
            break;
        }
    }

    // Find right half-max crossing
    let mut right = max_idx as f64;
    for i in (max_idx + 1)..data.len() {
        if data[i] <= half_max {
            let span = data[i - 1] - data[i];
            let frac = if span == 0.0 {
                0.0
            } else {
                (half_max - data[i]) / span
            };
            right = i as f64 - frac;
            break;
        }
    }

    right - left
}

/// C's SMOOTH (`aCalcPerform.c:968-975`) — the 5-point \[1,4,6,4,1\]/16 kernel,
/// applied IN PLACE and only where all five taps fit:
///
/// ```c
/// d = ps->a[firstEl]; e = ps->a[firstEl+1]; f = ps->a[firstEl+2];
/// for (i=firstEl+2; i<=lastEl-2; i++) {
///     ps->a[i] = d/16 + e/4 + 3*f/8 + ps->a[i+1]/4 + ps->a[i+2]/16;
///     d=e; e=f; f=ps->a[i+1];
/// }
/// ```
///
/// The rolling `d`/`e`/`f` hold the PRE-pass values of `a[i-2]`, `a[i-1]`, `a[i]`
/// (each is saved before the write that would clobber it), and `a[i+1]`/`a[i+2]` have
/// not been reached yet — so despite the in-place write it is a straight convolution
/// over the array as it was, exactly what an out-of-place buffer gives.
///
/// The two things it does NOT do are what the port got wrong:
///
/// * **the borders are left alone** — the loop simply does not cover the first two
///   and last two elements, so they keep their ORIGINAL values. The port seeded a
///   zero buffer and wrote only the interior, so `SMOO(AA)` zeroed four elements.
/// * **a window under 5 elements is unchanged** — the loop body never runs (its
///   bounds cross), so C returns the array untouched. The port answered all zeros.
pub fn smooth(data: &[f64]) -> Vec<f64> {
    let n = data.len();
    let mut result = data.to_vec();
    if n < 5 {
        return result;
    }
    for i in 2..n - 2 {
        result[i] =
            (data[i - 2] + 4.0 * data[i - 1] + 6.0 * data[i] + 4.0 * data[i + 1] + data[i + 2])
                / 16.0;
    }
    result
}

/// C's NSMOOTH (`aCalcPerform.c:583-589`) — the SMOOTH pass run `n` times, each pass
/// over the array the previous one left behind (C loops `for (k=firstEl; k<j+firstEl;
/// k++)` around the very same in-place body). Border preservation compounds: a
/// border element that SMOOTH keeps is a border element the next pass reads.
pub fn nsmooth(data: &[f64], n: usize) -> Vec<f64> {
    let mut result = data.to_vec();
    for _ in 0..n {
        result = smooth(&result);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_average() {
        assert_eq!(average(&[1.0, 2.0, 3.0, 4.0, 5.0]), 3.0);
    }

    #[test]
    fn test_average_empty() {
        assert_eq!(average(&[]), 0.0);
    }

    #[test]
    fn test_average_single() {
        assert_eq!(average(&[42.0]), 42.0);
    }

    #[test]
    fn test_std_dev() {
        let data = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let sd = std_dev(&data);
        // Sample std dev of this data ≈ 2.138
        assert!((sd - 2.138).abs() < 0.01, "sd={}", sd);
    }

    #[test]
    fn test_std_dev_empty() {
        assert_eq!(std_dev(&[]), 0.0);
    }

    #[test]
    fn test_std_dev_single() {
        assert_eq!(std_dev(&[5.0]), 0.0);
    }

    #[test]
    fn test_fwhm_gaussian() {
        // Create a simple Gaussian-like peak
        let n = 101;
        let center = 50.0;
        let sigma = 10.0;
        let data: Vec<f64> = (0..n)
            .map(|i| {
                let x = i as f64;
                (-0.5 * ((x - center) / sigma).powi(2)).exp()
            })
            .collect();
        let result = fwhm(&data);
        // FWHM of Gaussian = 2 * sqrt(2 * ln(2)) * sigma ≈ 2.3548 * sigma
        let expected = 2.3548 * sigma;
        assert!(
            (result - expected).abs() < 0.5,
            "FWHM={}, expected≈{}",
            result,
            expected
        );
    }

    #[test]
    fn test_fwhm_empty() {
        assert_eq!(fwhm(&[]), 0.0);
        assert_eq!(fwhm(&[1.0]), 0.0);
        assert_eq!(fwhm(&[1.0, 2.0]), 0.0);
    }
}
