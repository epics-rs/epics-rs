//! C `local_random()` — the Knuth 16-bit LCG all three calc engines share
//! verbatim (aCalcPerform.c:1649-1685, sCalcPerform.c:2074-2100). One
//! definition here so the three engines cannot drift apart.
//!
//! C parity, exactly:
//! - a THREAD-PRIVATE `unsigned short` seed (`epicsThreadPrivateGet`),
//!   initialised to `RAND_SEED = 0xa3bf` the first time a thread draws —
//!   so the sequence is deterministic and replays compiled C run for run;
//! - `seed = seed * RAND_MULTY + RAND_ADDY` wrapping at 16 bits;
//! - `(float)(seed + 1) / 65536.0` — the result is in `(0, 1]`, zero
//!   excluded because `NORMAL_RNDM` takes `log(local_random())`. Every
//!   value of `seed + 1` (≤ 65536) is exact in `float`, so the `f64`
//!   arithmetic below is bit-identical to C's `float` cast.
//!
//! [`seed_random_from_time`] is a Rust EXTENSION (documented deviation):
//! C has no reseeding facility at all, so a caller that wants
//! non-reproducible sequences must opt in explicitly. The default stays
//! C's fixed seed.

use std::cell::Cell;

const RAND_MULTY: u16 = 191 * 8 + 5; // 1533; `191 % 8 == 5` per Knuth
const RAND_ADDY: u16 = 0x3141;
const RAND_SEED: u16 = 0xa3bf;

thread_local! {
    static SEED: Cell<u16> = const { Cell::new(RAND_SEED) };
}

/// One draw of C `local_random()`: uniformly distributed in `(0, 1]`.
pub(crate) fn local_random() -> f64 {
    SEED.with(|seed| {
        let s = seed.get().wrapping_mul(RAND_MULTY).wrapping_add(RAND_ADDY);
        seed.set(s);
        f64::from(u32::from(s) + 1) / 65536.0
    })
}

/// Opt-in, per-thread time seeding — a Rust extension with no C
/// counterpart (C's seed is always `0xa3bf`). Call it on a thread whose
/// RNDM/ARNDM draws must not replay across runs; every other thread keeps
/// C's deterministic sequence.
pub fn seed_random_from_time() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    // Fold 64 bits onto the LCG's 16-bit state.
    let folded = (nanos ^ (nanos >> 16) ^ (nanos >> 32) ^ (nanos >> 48)) as u16;
    SEED.with(|seed| seed.set(folded));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact first draws of compiled C from the fixed seed:
    /// 0xa3bf*1533+0x3141 = 0x1c7c (wrapping u16), +1 = 0x1c7d → /65536.
    #[test]
    fn replays_c_sequence_from_the_fixed_seed() {
        let expected = {
            let mut s: u16 = RAND_SEED;
            let mut out = Vec::new();
            for _ in 0..4 {
                s = s.wrapping_mul(RAND_MULTY).wrapping_add(RAND_ADDY);
                out.push(f64::from(u32::from(s) + 1) / 65536.0);
            }
            out
        };
        // Fresh thread: seed starts at RAND_SEED regardless of what other
        // tests on this thread drew (C's seed is thread-private).
        let got = std::thread::spawn(|| (0..4).map(|_| local_random()).collect::<Vec<_>>())
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

    /// The opt-in reseed changes this thread's sequence only.
    #[test]
    fn time_seeding_is_opt_in_and_per_thread() {
        std::thread::spawn(|| {
            seed_random_from_time();
            let _ = local_random(); // draws fine from any seed
        })
        .join()
        .unwrap();
        // A fresh thread still replays C.
        let first = std::thread::spawn(local_random).join().unwrap();
        let expected =
            f64::from(u32::from(RAND_SEED.wrapping_mul(RAND_MULTY).wrapping_add(RAND_ADDY)) + 1)
                / 65536.0;
        assert_eq!(first, expected);
    }
}
