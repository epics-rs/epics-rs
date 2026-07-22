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

/// C's FWHM (`aCalcPerform.c:928-967`) — the distance between the two half-maximum
/// crossings, in fractional element indices.
///
/// `buf` is the whole `arraySize` buffer and `last_el` is C's `lastEl`, because the
/// operator does not confine itself to the window: it seeds from `a[firstEl]` (an
/// in-bounds read even when the window is EMPTY, and firstEl is always 0) and then
/// answers `lastEl - 0` if it finds no crossing at all. An empty window therefore has
/// `lastEl == -1` and C answers **-1**, which a window-slice signature could not
/// express.
///
/// Three things the port had wrong, all of them at the boundaries the operator is
/// made of:
///
/// * **the crossing test is STRICT**, `a[i] < half` (`:946`, `:956`). A sample sitting
///   exactly AT half-max is not a crossing, so the walk steps over a half-max plateau
///   instead of stopping on it. Compiled C, `AA=[0,2,4,2,2,0]` (half = 2): FWHM is 3,
///   where a `<=` test answers 2. The strictness also makes the interpolation safe by
///   construction: the loop breaks at the first element BELOW half-max, so its
///   neighbour towards the peak is at or above half-max and the denominator cannot be
///   zero — the `span == 0` guard the port needed was an artefact of `<=`.
/// * **no forward crossing means `lastEl`, not "zero width"** (`:953`) — a monotonic
///   ramp has its peak at the last element and no descent, so C measures from the
///   backward crossing to the END of the window. Compiled C, `AA=[0,1,2,3,4]`: 2.
/// * **no backward crossing means 0** (`:964`) — likewise a descending ramp measures
///   from the START. Compiled C, `AA=[4,3,2,1,0]`: 2.
///
/// Both fallbacks together are what a FLAT array gets: nothing is strictly below
/// half-max, so the answer is `lastEl - 0`. Compiled C, `AA=[3,3,3,3,3]`: 4 — where the
/// port's `max == min` early return answered 0.
pub fn fwhm(buf: &[f64], last_el: i64) -> f64 {
    let Some(&seed) = buf.first() else {
        return 0.0;
    };
    let last = last_el.clamp(-1, buf.len() as i64 - 1);

    // C `:931-941`: max (d), min (e) and the index of the max (j), all seeded from
    // a[firstEl] and advanced only on a STRICT comparison, so ties keep the FIRST
    // maximum.
    let (mut max, mut min, mut peak) = (seed, seed, 0i64);
    for i in 1..=last {
        let v = buf[i as usize];
        if v > max {
            max = v;
            peak = i;
        }
        if v < min {
            min = v;
        }
    }
    // C `:943` — `min + (max-min)/2`, not `(max+min)/2`.
    let half = min + (max - min) / 2.0;

    // Forward from the peak (`:945-953`).
    let mut right = last as f64;
    for i in peak + 1..=last {
        let (i, prev) = (i as usize, (i - 1) as usize);
        if buf[i] < half {
            right = prev as f64 + (half - buf[prev]) / (buf[i] - buf[prev]);
            break;
        }
    }

    // Backward from the peak (`:955-964`).
    let mut left = 0.0;
    for i in (0..peak).rev() {
        let (i, next) = (i as usize, (i + 1) as usize);
        if buf[i] < half {
            left = i as f64 + (half - buf[i]) / (buf[next] - buf[i]);
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
///
/// The pass count comes from the expression, so it is unbounded: `NSMOO(AA,2e9)`
/// asks for two billion passes, and C's own `int j` happily holds that. Running
/// them is pointless — [`smooth`] is a pure function of its input, so the moment a
/// pass hands back the bits it was given, every remaining pass is a no-op and the
/// buffer is already the value all `n` passes would have left. Stopping there is
/// therefore result-preserving, not a cap: no reachable count changes the answer,
/// and an absurd one returns instead of running for hours.
///
/// The comparison is over `to_bits`, not `==`: a buffer holding NaN is never `==`
/// itself, and a NaN is exactly the input for which the count would otherwise be
/// run out in full.
pub fn nsmooth(data: &[f64], n: usize) -> Vec<f64> {
    let mut result = data.to_vec();
    for _ in 0..n {
        let next = smooth(&result);
        let fixed_point = next
            .iter()
            .zip(&result)
            .all(|(a, b)| a.to_bits() == b.to_bits());
        result = next;
        if fixed_point {
            break;
        }
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
        let result = fwhm(&data, n as i64 - 1);
        // FWHM of Gaussian = 2 * sqrt(2 * ln(2)) * sigma ≈ 2.3548 * sigma
        let expected = 2.3548 * sigma;
        assert!(
            (result - expected).abs() < 0.5,
            "FWHM={}, expected≈{}",
            result,
            expected
        );
    }

    /// The short-buffer boundaries. C has no minimum length: it seeds from `a[0]` and
    /// runs the two walks, so a two-element array is a legitimate half-crossing.
    /// Compiled C: AA=[1] -> 0, AA=[1,2] -> 0.5, empty window -> -1.
    #[test]
    fn test_fwhm_short_buffers() {
        assert_eq!(fwhm(&[], -1), 0.0);
        assert_eq!(fwhm(&[1.0], 0), 0.0);
        assert_eq!(fwhm(&[1.0, 2.0], 1), 0.5);
        assert_eq!(
            fwhm(&[0.0, 1.0, 4.0], -1),
            -1.0,
            "an empty window is lastEl"
        );
    }
}
