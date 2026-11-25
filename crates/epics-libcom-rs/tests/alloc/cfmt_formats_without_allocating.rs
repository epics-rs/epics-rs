//! `CFloat` allocates nothing for a conversion that fits its stack text, so
//! a put of a number into a `DBF_STRING` field (`cvtDoubleToString`'s
//! `sprintf` fall-backs) stays allocation-free through it. Counted per thread
//! with the alloc binary's allocator.

use std::fmt::Write;

use alloc_count::thread;
use epics_libcom_rs::cfmt::CFloat;

/// A fixed sink, so the count sees only the conversion.
struct Sink {
    buf: [u8; 128],
    len: usize,
}

impl Write for Sink {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        self.buf
            .get_mut(self.len..end)
            .ok_or(std::fmt::Error)?
            .copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[test]
fn every_conversion_that_fits_allocates_nothing() {
    let values = [
        0.0,
        -0.0,
        1.5,
        -2.5e-7,
        123456789.125,
        1e16,
        -1e-300,
        f64::NAN,
        -f64::NAN,
        f64::NEG_INFINITY,
    ];
    let mut sink = Sink {
        buf: [0; 128],
        len: 0,
    };
    let ((), counts) = thread::counted(|| {
        for x in values {
            let x = CFloat(x);
            for p in [0usize, 3, 6, 17] {
                sink.len = 0;
                write!(sink, "{x:.p$}|{x:>24.p$e}|{x:.p$E}").unwrap();
                sink.len = 0;
                write!(sink, "{:.p$}|{:#012.p$}", x.g(), x.g()).unwrap();
            }
        }
    });
    assert_eq!(counts.allocs, 0);
}
