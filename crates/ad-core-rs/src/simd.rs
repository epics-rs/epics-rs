//! The run-time SIMD level every kernel in the AD crates dispatches on, and
//! the element-type casts of [`NDDataBuffer`](crate::ndarray::NDDataBuffer)
//! on explicit vectors.
//!
//! `fearless_simd` picks AVX2/AVX-512/NEON at run time; the lane loops the
//! compiler vectorizes on its own only reach the baseline the binary was
//! compiled for (SSE2 on x86_64). The level is detected once per process.

use fearless_simd::{Level, Simd, prelude::*};
use fearless_simd_macros::simd;
use std::sync::OnceLock;

/// The detected level, once per process.
pub fn level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(Level::new)
}

/// `v.iter().map(|&x| x as f64)` on vectors: `$to_f64` splits a chunk of
/// `$vec` into its `f64` vectors in element order.
macro_rules! to_f64_kernel {
    ($t:ty, $vec:ident, $name:ident, |$y:ident| $to_f64:expr) => {
        #[simd]
        pub(crate) fn $name<S: Simd>(simd: S, v: &[$t], out: &mut [f64]) {
            assert_eq!(v.len(), out.len());
            let n = S::f64s::LEN;
            let mut chunks = v.chunks_exact(S::$vec::LEN);
            let mut outs = out.chunks_exact_mut(S::$vec::LEN);
            for (c, o) in (&mut chunks).zip(&mut outs) {
                let $y = S::$vec::from_slice(simd, c);
                for (k, f) in $to_f64.into_iter().enumerate() {
                    f.store_slice(&mut o[k * n..(k + 1) * n]);
                }
            }
            for (&x, o) in chunks.remainder().iter().zip(outs.into_remainder()) {
                *o = x as f64;
            }
        }
    };
}

to_f64_kernel!(f32, f32s, to_f64_f32, |y| {
    let (a, b) = y.widen();
    [a, b]
});
to_f64_kernel!(i64, i64s, to_f64_i64, |y| [S::f64s::float_from(y)]);
to_f64_kernel!(u64, u64s, to_f64_u64, |y| [S::f64s::float_from(y)]);
to_f64_kernel!(i32, i32s, to_f64_i32, |y| {
    let (p0, p1) = y.widen();
    [S::f64s::float_from(p0), S::f64s::float_from(p1)]
});
to_f64_kernel!(u32, u32s, to_f64_u32, |y| {
    let (p0, p1) = y.widen();
    [S::f64s::float_from(p0), S::f64s::float_from(p1)]
});
to_f64_kernel!(i16, i16s, to_f64_i16, |y| {
    let (a, b) = y.widen();
    let (p0, p1) = a.widen();
    let (p2, p3) = b.widen();
    [
        S::f64s::float_from(p0),
        S::f64s::float_from(p1),
        S::f64s::float_from(p2),
        S::f64s::float_from(p3),
    ]
});
to_f64_kernel!(u16, u16s, to_f64_u16, |y| {
    let (a, b) = y.widen();
    let (p0, p1) = a.widen();
    let (p2, p3) = b.widen();
    [
        S::f64s::float_from(p0),
        S::f64s::float_from(p1),
        S::f64s::float_from(p2),
        S::f64s::float_from(p3),
    ]
});
to_f64_kernel!(i8, i8s, to_f64_i8, |y| {
    let (a, b) = y.widen();
    let (a0, a1) = a.widen();
    let (b0, b1) = b.widen();
    let (p0, p1) = a0.widen();
    let (p2, p3) = a1.widen();
    let (p4, p5) = b0.widen();
    let (p6, p7) = b1.widen();
    [
        S::f64s::float_from(p0),
        S::f64s::float_from(p1),
        S::f64s::float_from(p2),
        S::f64s::float_from(p3),
        S::f64s::float_from(p4),
        S::f64s::float_from(p5),
        S::f64s::float_from(p6),
        S::f64s::float_from(p7),
    ]
});
to_f64_kernel!(u8, u8s, to_f64_u8, |y| {
    let (a, b) = y.widen();
    let (a0, a1) = a.widen();
    let (b0, b1) = b.widen();
    let (p0, p1) = a0.widen();
    let (p2, p3) = a1.widen();
    let (p4, p5) = b0.widen();
    let (p6, p7) = b1.widen();
    [
        S::f64s::float_from(p0),
        S::f64s::float_from(p1),
        S::f64s::float_from(p2),
        S::f64s::float_from(p3),
        S::f64s::float_from(p4),
        S::f64s::float_from(p5),
        S::f64s::float_from(p6),
        S::f64s::float_from(p7),
    ]
});

/// `values.iter().map(|&x| x as $t)` on vectors, for the integer types up to
/// 32 bits: a NaN lane becomes 0, every lane is clamped to the type's range
/// (whose bounds are exact in `f64`), and the truncating conversion to 64-bit
/// lanes is then in range, so it is exact on every level. `$w(k)` is the
/// `k`-th `f64` vector of a chunk converted that way, and `$narrow` packs
/// `$per` of them into one vector of `$t`, low lanes first.
macro_rules! from_f64_kernel {
    ($t:ty, $wide:ident, $name:ident, $per:expr, |$w:ident| $narrow:expr) => {
        #[simd]
        pub(crate) fn $name<S: Simd>(simd: S, values: &[f64], out: &mut [$t]) {
            assert_eq!(values.len(), out.len());
            let n = S::f64s::LEN;
            let zero = S::f64s::splat(simd, 0.0);
            let lo = S::f64s::splat(simd, <$t>::MIN as f64);
            let hi = S::f64s::splat(simd, <$t>::MAX as f64);
            let mut chunks = values.chunks_exact(n * $per);
            let mut outs = out.chunks_exact_mut(n * $per);
            for (c, o) in (&mut chunks).zip(&mut outs) {
                macro_rules! $w {
                    ($k:expr) => {{
                        let v = S::f64s::from_slice(simd, &c[$k * n..($k + 1) * n]);
                        let v = v.simd_eq(v).select(v, zero);
                        S::$wide::truncate_from(v.max(lo).min(hi))
                    }};
                }
                $narrow.store_slice(o);
            }
            for (&x, o) in chunks.remainder().iter().zip(outs.into_remainder()) {
                *o = x as $t;
            }
        }
    };
}

from_f64_kernel!(i32, i64s, from_f64_i32, 2, |w| w!(0).narrow(w!(1)));
from_f64_kernel!(u32, u64s, from_f64_u32, 2, |w| w!(0).narrow(w!(1)));
from_f64_kernel!(i16, i64s, from_f64_i16, 4, |w| w!(0)
    .narrow(w!(1))
    .narrow(w!(2).narrow(w!(3))));
from_f64_kernel!(u16, u64s, from_f64_u16, 4, |w| w!(0)
    .narrow(w!(1))
    .narrow(w!(2).narrow(w!(3))));
from_f64_kernel!(i8, i64s, from_f64_i8, 8, |w| w!(0)
    .narrow(w!(1))
    .narrow(w!(2).narrow(w!(3)))
    .narrow(w!(4).narrow(w!(5)).narrow(w!(6).narrow(w!(7)))));
from_f64_kernel!(u8, u64s, from_f64_u8, 8, |w| w!(0)
    .narrow(w!(1))
    .narrow(w!(2).narrow(w!(3)))
    .narrow(w!(4).narrow(w!(5)).narrow(w!(6).narrow(w!(7)))));

/// The 64-bit integers: their bounds are not exact in `f64`, so the clamp
/// above cannot be used; `truncate_from_precise` is the `as` conversion
/// itself (saturating, NaN to 0) on every level.
macro_rules! from_f64_wide_kernel {
    ($t:ty, $wide:ident, $name:ident) => {
        #[simd]
        pub(crate) fn $name<S: Simd>(simd: S, values: &[f64], out: &mut [$t]) {
            assert_eq!(values.len(), out.len());
            let mut chunks = values.chunks_exact(S::f64s::LEN);
            let mut outs = out.chunks_exact_mut(S::f64s::LEN);
            for (c, o) in (&mut chunks).zip(&mut outs) {
                let v = S::f64s::from_slice(simd, c);
                S::$wide::truncate_from_precise(v).store_slice(o);
            }
            for (&x, o) in chunks.remainder().iter().zip(outs.into_remainder()) {
                *o = x as $t;
            }
        }
    };
}

from_f64_wide_kernel!(i64, i64s, from_f64_i64);
from_f64_wide_kernel!(u64, u64s, from_f64_u64);

/// `f64` to `f32`: the narrowing conversion rounds to nearest as `as` does.
#[simd]
pub(crate) fn from_f64_f32<S: Simd>(simd: S, values: &[f64], out: &mut [f32]) {
    assert_eq!(values.len(), out.len());
    let n = S::f64s::LEN;
    let mut chunks = values.chunks_exact(2 * n);
    let mut outs = out.chunks_exact_mut(2 * n);
    for (c, o) in (&mut chunks).zip(&mut outs) {
        let lo = S::f64s::from_slice(simd, &c[..n]);
        let hi = S::f64s::from_slice(simd, &c[n..]);
        lo.narrow(hi).store_slice(o);
    }
    for (&x, o) in chunks.remainder().iter().zip(outs.into_remainder()) {
        *o = x as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every level this machine can run, so a kernel is checked on each
    /// lowering and not only on the one `level()` picks.
    fn levels() -> Vec<Level> {
        let top = level();
        let mut out = vec![top];
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            out.extend(top.as_avx2().map(Level::Avx2));
            out.extend(top.as_sse4_2().map(Level::Sse4_2));
            out.extend(top.as_sse2().map(Level::Sse2));
        }
        out
    }

    /// The `f64` values whose casts differ between a truncating, a
    /// saturating and a NaN-clearing conversion, plus enough ordinary ones
    /// to fill several vectors of the widest lowering and leave a tail.
    fn edge_values() -> Vec<f64> {
        let mut v = vec![
            0.0,
            -0.0,
            0.5,
            -0.5,
            1.5,
            -1.5,
            127.9,
            -128.9,
            255.9,
            256.0,
            -1.0,
            32767.9,
            -32768.9,
            65535.9,
            65536.0,
            2147483647.9,
            -2147483648.9,
            4294967295.9,
            4294967296.0,
            9.3e18,
            -9.3e18,
            1.9e19,
            -1.9e19,
            1e300,
            -1e300,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::MAX,
            f64::MIN,
            f64::EPSILON,
            9007199254740993.0,
        ];
        v.extend((0..77).map(|i| (i as f64 - 30.0) * 1234.5678));
        v
    }

    macro_rules! check_from_f64 {
        ($t:ty, $kernel:ident) => {
            let values = edge_values();
            let want: Vec<$t> = values.iter().map(|&x| x as $t).collect();
            for lvl in levels() {
                let mut got = vec![0 as $t; values.len()];
                fearless_simd::dispatch!(lvl, s => $kernel(s, &values, &mut got));
                assert_eq!(got, want, "{} at {lvl:?}", stringify!($t));
            }
        };
    }

    #[test]
    fn from_f64_matches_as_on_every_level() {
        check_from_f64!(i8, from_f64_i8);
        check_from_f64!(u8, from_f64_u8);
        check_from_f64!(i16, from_f64_i16);
        check_from_f64!(u16, from_f64_u16);
        check_from_f64!(i32, from_f64_i32);
        check_from_f64!(u32, from_f64_u32);
        check_from_f64!(i64, from_f64_i64);
        check_from_f64!(u64, from_f64_u64);
    }

    #[test]
    fn from_f64_f32_matches_as_on_every_level() {
        let values = edge_values();
        let want: Vec<f32> = values.iter().map(|&x| x as f32).collect();
        for lvl in levels() {
            let mut got = vec![0.0f32; values.len()];
            fearless_simd::dispatch!(lvl, s => from_f64_f32(s, &values, &mut got));
            assert_eq!(got.len(), want.len());
            for (g, w) in got.iter().zip(&want) {
                assert!(
                    g.to_bits() == w.to_bits() || (g.is_nan() && w.is_nan()),
                    "{g} vs {w} at {lvl:?}"
                );
            }
        }
    }

    macro_rules! check_to_f64 {
        ($t:ty, $kernel:ident, $values:expr) => {
            let values: Vec<$t> = $values;
            let want: Vec<f64> = values.iter().map(|&x| x as f64).collect();
            for lvl in levels() {
                let mut got = vec![0.0; values.len()];
                fearless_simd::dispatch!(lvl, s => $kernel(s, &values, &mut got));
                for (g, w) in got.iter().zip(&want) {
                    assert!(g.to_bits() == w.to_bits(), "{g} vs {w} for {} at {lvl:?}", stringify!($t));
                }
            }
        };
    }

    #[test]
    fn to_f64_matches_as_on_every_level() {
        let ints: Vec<i64> = (-70..71)
            .map(|i| i * 1_000_003)
            .chain([i64::MIN, i64::MAX, 0, 1, -1])
            .collect();
        check_to_f64!(i8, to_f64_i8, ints.iter().map(|&x| x as i8).collect());
        check_to_f64!(u8, to_f64_u8, ints.iter().map(|&x| x as u8).collect());
        check_to_f64!(i16, to_f64_i16, ints.iter().map(|&x| x as i16).collect());
        check_to_f64!(u16, to_f64_u16, ints.iter().map(|&x| x as u16).collect());
        check_to_f64!(i32, to_f64_i32, ints.iter().map(|&x| x as i32).collect());
        check_to_f64!(u32, to_f64_u32, ints.iter().map(|&x| x as u32).collect());
        check_to_f64!(i64, to_f64_i64, ints.clone());
        check_to_f64!(u64, to_f64_u64, ints.iter().map(|&x| x as u64).collect());
        check_to_f64!(
            f32,
            to_f64_f32,
            edge_values().iter().map(|&x| x as f32).collect()
        );
    }
}
