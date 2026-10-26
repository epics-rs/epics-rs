//! The Knuth 16-bit LCG the calc engines draw RNDM from — TWO generators,
//! because C has two and they do not agree.
//!
//! Both advance the same state, `seed = seed * RAND_MULTY + RAND_ADDY`
//! wrapping at 16 bits. They part company on the mapping onto the unit
//! interval, and that is an observable difference on every draw:
//!
//! | engine | C | mapping | range | first draw from C's seed |
//! |---|---|---|---|---|
//! | base (calc, calcout, swait) | `calcRandom`, calcPerform.c:518-523 | `seed / 65535.0` | `[0, 1]` | 0.75007248… |
//! | sCalc / aCalc | `local_random`, sCalcPerform.c:2079-2097, aCalcPerform.c:1649-1685 | `(float)(seed + 1) / 65536.0` | `(0, 1]` | 0.75007629… |
//!
//! synApps excludes zero deliberately — `NORMAL_RNDM` takes
//! `log(local_random())` — and base, which has no `NRNDM`, does not. Every
//! value of `seed + 1` (≤ 65536) is exact in `float`, so the `f64` arithmetic
//! in `local_random` is bit-identical to C's `float` cast.
//!
//! Two deliberate deviations:
//! - base's C seed is a plain file-static `unsigned short` shared by every
//!   thread, a data race. Both generators here keep the seed THREAD-PRIVATE,
//!   which is what synApps does (`epicsThreadPrivateGet`).
//! - C starts every generator at the fixed `0xa3bf`, so every IOC draws the
//!   same sequence after every boot (CBUG-A4). Each thread's generators start
//!   from the clock instead, as base's `calcRandom` does on 7.0 since
//!   `77a3b8110`, which is after R7.0.10.

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

const RAND_MULTY: u16 = 191 * 8 + 5; // 1533; `191 % 8 == 5` per Knuth
const RAND_ADDY: u16 = 0x3141;

thread_local! {
    /// synApps `local_random`'s seed.
    static SEED: Cell<u16> = Cell::new(clock_seed());
    /// base `calcRandom`'s seed — a SEPARATE sequence, as in C, where the two
    /// live in different translation units and neither advances the other.
    static BASE_SEED: Cell<u16> = Cell::new(clock_seed());
}

/// The clock's nanoseconds folded onto the LCG's 16-bit state.
fn clock_seed() -> u16 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    (nanos ^ (nanos >> 16) ^ (nanos >> 32) ^ (nanos >> 48)) as u16
}

fn advance(cell: &'static std::thread::LocalKey<Cell<u16>>) -> u16 {
    cell.with(|seed| {
        let s = seed.get().wrapping_mul(RAND_MULTY).wrapping_add(RAND_ADDY);
        seed.set(s);
        s
    })
}

/// One draw of synApps `local_random()` (sCalc/aCalc): uniform in `(0, 1]`.
pub(crate) fn local_random() -> f64 {
    f64::from(u32::from(advance(&SEED)) + 1) / 65536.0
}

/// One draw of base `calcRandom()` (calcPerform.c:518-523): uniform in
/// `[0, 1]` — `seed / 65535.0`, with no `+1` and a divisor one smaller than
/// synApps'. The base numeric engine must never draw from [`local_random`]:
/// the two disagree in the last digits of every single draw.
pub(crate) fn calc_random() -> f64 {
    f64::from(advance(&BASE_SEED)) / 65535.0
}

/// Start both of this thread's generators at `seed`, so a test can replay C.
#[cfg(test)]
pub(crate) fn seed_thread(seed: u16) {
    SEED.with(|s| s.set(seed));
    BASE_SEED.with(|s| s.set(seed));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C's fixed seed (`calcPerform.c:516` at R7.0.10, `sCalcPerform.c:2071`,
    /// `aCalcPerform.c:1659`).
    const C_SEED: u16 = 0xa3bf;

    /// CBUG-A4: C starts every generator at `C_SEED`, so each boot replays one
    /// sequence. Eight fresh threads all drawing C's first value from either
    /// generator would mean the port still does.
    #[test]
    fn fresh_threads_do_not_start_from_the_c_seed() {
        let s1 = C_SEED.wrapping_mul(RAND_MULTY).wrapping_add(RAND_ADDY);
        let firsts: Vec<(f64, f64)> = (0..8)
            .map(|_| {
                std::thread::spawn(|| (calc_random(), local_random()))
                    .join()
                    .unwrap()
            })
            .collect();
        assert!(firsts.iter().any(|(b, _)| *b != f64::from(s1) / 65535.0));
        assert!(
            firsts
                .iter()
                .any(|(_, l)| *l != f64::from(u32::from(s1) + 1) / 65536.0)
        );
    }

    /// The exact first draws of compiled C from its seed:
    /// 0xa3bf*1533+0x3141 = 0x1c7c (wrapping u16), +1 = 0x1c7d → /65536.
    #[test]
    fn replays_c_sequence_from_the_c_seed() {
        let expected = {
            let mut s: u16 = C_SEED;
            let mut out = Vec::new();
            for _ in 0..4 {
                s = s.wrapping_mul(RAND_MULTY).wrapping_add(RAND_ADDY);
                out.push(f64::from(u32::from(s) + 1) / 65536.0);
            }
            out
        };
        let got = std::thread::spawn(|| {
            seed_thread(C_SEED);
            (0..4).map(|_| local_random()).collect::<Vec<_>>()
        })
        .join()
        .unwrap();
        assert_eq!(got, expected);
    }

    /// C's mapping excludes 0 (`log` in NORMAL_RNDM) and includes 1
    /// (`seed == 65535` → 65536/65536).
    #[test]
    fn range_is_zero_exclusive_one_inclusive() {
        std::thread::spawn(|| {
            // The 16-bit LCG has period 65536 (full for m=2^16 with odd
            // addend and multy ≡ 1 mod 4... exercised exhaustively here).
            let mut hit_one = false;
            for _ in 0..65536 {
                let r = local_random();
                assert!(r > 0.0 && r <= 1.0, "out of (0,1]: {r}");
                if r == 1.0 {
                    hit_one = true;
                }
            }
            assert!(hit_one, "seed 65535 must map to exactly 1.0");
        })
        .join()
        .unwrap();
    }

    /// base `calcRandom` is NOT `local_random`: same LCG, different mapping.
    /// The first draw of compiled C is 49156/65535, and synApps' is
    /// 49157/65536 — they differ in the sixth decimal.
    #[test]
    fn base_generator_replays_calc_random_not_local_random() {
        let got = std::thread::spawn(|| {
            seed_thread(C_SEED);
            (0..4).map(|_| calc_random()).collect::<Vec<_>>()
        })
        .join()
        .unwrap();
        let expected = {
            let mut s: u16 = C_SEED;
            let mut out = Vec::new();
            for _ in 0..4 {
                s = s.wrapping_mul(RAND_MULTY).wrapping_add(RAND_ADDY);
                out.push(f64::from(s) / 65535.0);
            }
            out
        };
        assert_eq!(got, expected);
        assert_eq!(got[0], 49156.0 / 65535.0);
        assert_ne!(got[0], 49157.0 / 65536.0); // what local_random answers
    }

    /// The two generators keep SEPARATE seeds: drawing from one must not
    /// advance the other, as in C (two statics in two translation units).
    #[test]
    fn the_two_seeds_are_independent() {
        let (base_first, s_first) = std::thread::spawn(|| {
            seed_thread(C_SEED);
            let _ = local_random(); // advance synApps' seed only
            let _ = local_random();
            (calc_random(), local_random())
        })
        .join()
        .unwrap();
        let s1 = C_SEED.wrapping_mul(RAND_MULTY).wrapping_add(RAND_ADDY);
        assert_eq!(base_first, f64::from(s1) / 65535.0, "base seed advanced");
        // s_first is the THIRD draw of the synApps seed (two before it).
        let s3 = s1
            .wrapping_mul(RAND_MULTY)
            .wrapping_add(RAND_ADDY)
            .wrapping_mul(RAND_MULTY)
            .wrapping_add(RAND_ADDY);
        assert_eq!(s_first, f64::from(u32::from(s3) + 1) / 65536.0);
    }

    /// base's range INCLUDES zero — it has no `NRNDM`, so nothing takes a log
    /// of the draw and C never excluded it (`seed / 65535.0`, seed 0 → 0.0).
    #[test]
    fn base_range_includes_zero_and_one() {
        std::thread::spawn(|| {
            let (mut hit_zero, mut hit_one) = (false, false);
            for _ in 0..65536 {
                let r = calc_random();
                assert!((0.0..=1.0).contains(&r), "out of [0,1]: {r}");
                if r == 0.0 {
                    hit_zero = true;
                }
                if r == 1.0 {
                    hit_one = true;
                }
            }
            assert!(hit_zero, "seed 0 must map to exactly 0.0");
            assert!(hit_one, "seed 65535 must map to exactly 1.0");
        })
        .join()
        .unwrap();
    }
}
