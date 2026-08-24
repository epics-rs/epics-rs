//! compress `N to 1 Low/High Value` seeds from the chunk's first sample.
//!
//! C `compress_array` (`compressRecord.c:183-196`):
//!
//! ```c
//!     value = *psource++;
//!     for (j = 1; j < n; j++, psource++)
//!         if (value > *psource) value = *psource;   /* High uses < */
//! ```
//!
//! There is no `isnan` anywhere in `compressRecord.c`, so NaN is decided by
//! POSITION, not filtered: a NaN seed wins every false comparison and the chunk
//! answers NaN, while a NaN after the seed loses every comparison and is
//! skipped. Neither observable survives a `fold` over a ±INF identity — that
//! identity discards NaN in both positions and, for an all-NaN chunk, leaks out
//! as the identity itself.
//!
//! `selRecord.c:361-377` really does seed `±epicsINF` and guard `!isnan`, and
//! `calcPerform.c:191-207` lets a NaN ARGUMENT win via `isnan(d)`. Those are
//! three different rules in C; this file pins compress's.
//!
//! Ground truth is the C source above; the scalar arm (`compress_scalar`,
//! `compressRecord.c:273-304`) expresses the same rule as `|| (inx == 0)` and
//! is included so the two arms cannot drift apart.

use epics_base_rs::server::records::compress::CompressRecord;

const LOW: i16 = 0;
const HIGH: i16 = 1;

/// One `compress_array` chunk: NSAM=1, N=len, so the whole slice is exactly one
/// chunk and `VAL[0]` is its compressed value.
fn compress_array(alg: i16, chunk: &[f64]) -> f64 {
    let mut rec = CompressRecord::new(1, alg);
    rec.n = chunk.len() as i32;
    rec.push_array(chunk);
    rec.val[0]
}

/// The same samples down C's SCALAR arm — one `push_value` per sample, emitting
/// on the N-th.
fn compress_scalar(alg: i16, samples: &[f64]) -> f64 {
    let mut rec = CompressRecord::new(1, alg);
    rec.n = samples.len() as i32;
    for &s in samples {
        rec.push_value(s);
    }
    rec.val[0]
}

/// Boundary: NaN in position 0. `value = *psource++` takes it, and every later
/// `value > *psource` / `value < *psource` is false, so it is never displaced.
#[test]
fn a_nan_seed_is_never_displaced() {
    assert!(compress_array(LOW, &[f64::NAN, 3.0, 5.0]).is_nan());
    assert!(compress_array(HIGH, &[f64::NAN, 3.0, 5.0]).is_nan());
}

/// Boundary: NaN after position 0. It loses every strict comparison, so the
/// real extremum of the remaining samples stands.
#[test]
fn a_nan_after_the_seed_is_skipped() {
    assert_eq!(compress_array(LOW, &[3.0, f64::NAN, 5.0, 1.0]), 1.0);
    assert_eq!(compress_array(HIGH, &[3.0, f64::NAN, 5.0, 1.0]), 5.0);
}

/// Boundary: every sample NaN. C has no identity element to fall back on — the
/// seed IS a sample — so the chunk answers NaN. A `fold(±INF, min/max)` answers
/// the identity, publishing `+inf` / `-inf` as if it were data.
#[test]
fn an_all_nan_chunk_answers_nan_not_an_infinite_identity() {
    let low = compress_array(LOW, &[f64::NAN, f64::NAN, f64::NAN]);
    let high = compress_array(HIGH, &[f64::NAN, f64::NAN, f64::NAN]);
    assert!(low.is_nan(), "Low answered {low}, not NaN");
    assert!(high.is_nan(), "High answered {high}, not NaN");
}

/// Boundary: `±inf` is a SAMPLE, not a sentinel. A chunk holding a real
/// infinity must publish it, and one holding none must never invent one.
#[test]
fn infinity_is_data() {
    assert_eq!(
        compress_array(LOW, &[3.0, f64::NEG_INFINITY, 5.0]),
        f64::NEG_INFINITY
    );
    assert_eq!(
        compress_array(HIGH, &[3.0, f64::INFINITY, 5.0]),
        f64::INFINITY
    );
    assert_eq!(compress_array(LOW, &[3.0, 5.0, 1.0]), 1.0);
    assert_eq!(compress_array(HIGH, &[3.0, 5.0, 1.0]), 5.0);
}

/// C's scalar arm spells the same rule as `|| (inx == 0)` — the first sample of
/// an accumulation seeds `cvb` unconditionally, and every later sample must win
/// a strict comparison. The two arms must therefore agree sample-for-sample.
#[test]
fn the_scalar_arm_agrees_with_the_array_arm() {
    for chunk in [
        vec![f64::NAN, 3.0, 5.0],
        vec![3.0, f64::NAN, 5.0, 1.0],
        vec![f64::NAN, f64::NAN],
        vec![3.0, 5.0, 1.0],
    ] {
        for alg in [LOW, HIGH] {
            let a = compress_array(alg, &chunk);
            let s = compress_scalar(alg, &chunk);
            assert_eq!(
                a.is_nan(),
                s.is_nan(),
                "alg={alg} chunk={chunk:?}: array={a} scalar={s}"
            );
            if !a.is_nan() {
                assert_eq!(a, s, "alg={alg} chunk={chunk:?}");
            }
        }
    }
}
