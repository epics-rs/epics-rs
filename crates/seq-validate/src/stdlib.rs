//! What the validate programs call from `<stdlib.h>`.

use std::ffi::c_int;

use parking_lot::Mutex;

/// glibc's `random_r` state (`TYPE_3`: degree 31, separation 3), as
/// `srandom(1)` leaves it, the seed a program that never seeds runs with.
struct Random {
    r: [u32; 34],
    /// The next `r` index to fill, modulo 34.
    k: usize,
}

impl Random {
    fn seeded(seed: u32) -> Random {
        let mut r = [0u32; 34];
        r[0] = seed;
        for i in 1..31 {
            // 16807 * r[i - 1] % (2^31 - 1), by Schrage's method as glibc.
            let prev = r[i - 1] as i32;
            let word = 16807 * (prev % 127773) - 2836 * (prev / 127773);
            r[i] = if word < 0 { word + i32::MAX } else { word } as u32;
        }
        for i in 31..34 {
            r[i] = r[i - 31];
        }
        let mut random = Random { r, k: 34 };
        for _ in 34..344 {
            random.next();
        }
        random
    }

    fn next(&mut self) -> u32 {
        let k = self.k % 34;
        let v = self.r[(k + 34 - 31) % 34].wrapping_add(self.r[(k + 34 - 3) % 34]);
        self.r[k] = v;
        self.k = k + 1;
        v >> 1
    }
}

static RANDOM: Mutex<Option<Random>> = Mutex::new(None);

/// `rand`: glibc's, so a program's draws are the C build's.
pub fn rand() -> c_int {
    RANDOM
        .lock()
        .get_or_insert_with(|| Random::seeded(1))
        .next() as c_int
}

#[cfg(test)]
mod tests {
    /// The first draws of glibc's unseeded `rand()`.
    #[test]
    fn rand_is_glibc_s() {
        let draws: Vec<_> = (0..5).map(|_| super::rand()).collect();
        assert_eq!(
            draws,
            [1804289383, 846930886, 1681692777, 1714636915, 1957747793]
        );
    }
}
