//! `filterDrive.st`, compiled by snc-rs from `build.rs` and run on
//! `epics-seq-rs`. Its escaped C is the `%%#include` lines and the `%{ }%`
//! block (`:233-319`) holding `cvtToBin` and `calcTrans`, which are here in
//! Rust under their C names on `chantler.h`'s table,
//! [`crate::data::chantler`].

// SNL calls these by their C names.
#![allow(non_snake_case)]

use std::sync::Arc;

use crate::data::chantler::{FILTER_MATERIALS, attenuation};
use epics_seq_rs::c::*;
use epics_seq_rs::{CharPtr, Place};

macro_rules! seqg_escaped {
    (r#"#include <string.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"#include "chantler.h""#) => {};
    // `cvtToBin` and `calcTrans` (`:233-319`), below in Rust.
    (r#"static char *cvtToBin(char *buff, int val, int nBits) { int i, nxtBit, nxtChar; nxtChar = 0; for (i = 0; i < nBits; i++) { nxtBit = 1 << (nBits - i - 1); if ((i != 0) && ((i & 0x0003) == 0)) buff[nxtChar++] = '.'; if (val & nxtBit) { buff[nxtChar++] = '1'; val &= ~nxtBit; }else { buff[nxtChar++] = '0'; } } buff[nxtChar] = '\0'; if (val != 0) { for (i = 0; i < nxtChar; i++) buff[i] = '*'; } return buff; } static double calcTrans(double keV, double thickness, char *species, int debug) { int i, j; double mu, frac, absLen, transm; for(i=0; i<NUM_SPECIES; i++) { if (strcmp(filtermat[i].name, species) == 0) break; } if(i >= NUM_SPECIES) { if(debug) printf("Filter material '%s' not found\n", species); return(0.); } if(debug & 4) printf("  species = '%s', index = %d\n", species, i); if(debug & 4) printf("  density = %.4e gm/cm3, thickness = %.4e um\n", matdensity[i], thickness); if(debug & 4) printf("  energy = %.4e\n", keV); for(j=0; j<filtermat[i].numEntries; j++) { if (keV < filtermat[i].keV[j]) break; } if((j < 1) | (j >= filtermat[i].numEntries)) { if(debug) printf("Energy '%f' not found in filter data\n", keV); return(0.); } frac = (keV - filtermat[i].keV[j-1]) / (filtermat[i].keV[j] - filtermat[i].keV[j-1]); mu = filtermat[i].mu[j-1] + frac * (filtermat[i].mu[j] - filtermat[i].mu[j-1]); absLen = 1.0/(matdensity[i]*mu); absLen *= 1.e4; transm = exp(-(thickness/absLen)); if(debug & 8){ printf("  E1 = %.4e keV, mu1 = %.4e\n", filtermat[i].keV[j-1], filtermat[i].mu[j-1]); printf("  E2 = %.4e keV, mu2 = %.4e\n", filtermat[i].keV[j], filtermat[i].mu[j]); printf("  frac = %.4f, mu = %.4e\n", frac, mu); } if(debug & 4){ printf("  attenuation length ='%.4e' um, transmission=%.4e\n", absLen, transm); } return(transm); }"#) => {};
}

epics_seq_rs::include_program!("filterDrive");

/// `cvtToBin` (`filterDrive.st:240-264`): the low `nBits` of `val` into
/// `buff`, most significant first with a `.` before every fourth, or all
/// `*` when `val` has bits above them; returns `buff`.
fn cvtToBin<B: CBuf>(buff: &impl Place<B>, val: i32, nBits: i32) -> CharPtr {
    let v = val as u32;
    let n = nBits.max(0) as usize;
    let mut s = Vec::with_capacity(n + n / 4);
    for i in 0..n {
        if i != 0 && i % 4 == 0 {
            s.push(b'.');
        }
        s.push(if (v >> (n - i - 1)) & 1 != 0 {
            b'1'
        } else {
            b'0'
        });
    }
    if v.checked_shr(n as u32).unwrap_or(0) != 0 {
        s.fill(b'*');
    }
    strcpy(buff, &s[..]);
    Some(buff.modify(|b| Arc::from(b.bytes_mut().split(|&c| c == 0).next().unwrap_or(&[]))))
}

/// `calcTrans` (`filterDrive.st:267-317`): the transmission of `thickness`
/// um of `species` at `keV`, 0 for an unknown species or energy, with C's
/// console lines for `debug`.
fn calcTrans(keV: f64, thickness: f64, species: impl CSrc, debug: i32) -> f64 {
    let species = species.to_c_vec();
    let name = CArg::from(&species[..]);
    let Some(i) = FILTER_MATERIALS
        .iter()
        .position(|m| m.name.as_bytes() == species)
    else {
        if debug != 0 {
            printf("Filter material '%s' not found\n", &[name]);
        }
        return 0.0;
    };
    let mat = &FILTER_MATERIALS[i];
    if debug & 4 != 0 {
        printf(
            "  species = '%s', index = %d\n",
            &[name, CArg::from(i as i32)],
        );
        printf(
            "  density = %.4e gm/cm3, thickness = %.4e um\n",
            &[CArg::from(mat.density), CArg::from(thickness)],
        );
        printf("  energy = %.4e\n", &[CArg::from(keV)]);
    }
    let Some(a) = attenuation(mat, keV, thickness) else {
        if debug != 0 {
            printf("Energy '%f' not found in filter data\n", &[CArg::from(keV)]);
        }
        return 0.0;
    };
    if debug & 8 != 0 {
        let (j0, j1) = (a.j - 1, a.j);
        printf(
            "  E1 = %.4e keV, mu1 = %.4e\n",
            &[CArg::from(mat.kev[j0]), CArg::from(mat.mu[j0])],
        );
        printf(
            "  E2 = %.4e keV, mu2 = %.4e\n",
            &[CArg::from(mat.kev[j1]), CArg::from(mat.mu[j1])],
        );
        printf(
            "  frac = %.4f, mu = %.4e\n",
            &[CArg::from(a.frac), CArg::from(a.mu)],
        );
    }
    if debug & 4 != 0 {
        printf(
            "  attenuation length ='%.4e' um, transmission=%.4e\n",
            &[CArg::from(a.abs_len_um), CArg::from(a.transmission)],
        );
    }
    a.transmission
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;

    use super::*;
    use epics_libcom_rs::runtime::stdio::redirect_stdout;
    use epics_seq_rs::{SeqString, SeqVar};

    #[test]
    fn al_at_10_kev_is_between_0_and_1() {
        let t = calcTrans(10.0, 100.0, "Al", 0);
        assert!(t > 0.0 && t < 1.0, "Al transmission at 10keV = {t}");
    }

    #[test]
    fn an_unknown_species_is_0() {
        assert_eq!(calcTrans(10.0, 100.0, "Unobtainium", 0), 0.0);
    }

    /// `cvtToBin`'s result, and what it left in the buffer.
    fn bin(val: i32, nBits: i32) -> (String, String) {
        let buff = SeqVar::new(SeqString::new());
        let r = cvtToBin(&buff, val, nBits).unwrap();
        let s = |b: &[u8]| String::from_utf8(b.to_vec()).unwrap();
        (s(&r), buff.modify(|b| s(b.as_bytes())))
    }

    /// The outputs of C's `cvtToBin` built from `filterDrive.st:240-264`.
    #[test]
    fn cvt_to_bin_groups_by_four_and_stars_an_overflow() {
        for (val, nBits, c) in [
            (22, 5, "1011.0"),
            (4, 2, "**"),
            (-1, 32, "1111.1111.1111.1111.1111.1111.1111.1111"),
            (0, 0, ""),
            (0x1234, 16, "0001.0010.0011.0100"),
        ] {
            assert_eq!(bin(val, nBits), (c.into(), c.into()), "{val} {nBits}");
        }
    }

    /// What `f` prints through the thread's stdout.
    fn stdout_of(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Sink(Rc<RefCell<Vec<u8>>>);
        impl io::Write for Sink {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        redirect_stdout(sink.clone(), f);
        String::from_utf8(sink.0.take()).unwrap()
    }

    /// An oracle file without its `#` header.
    fn oracle(text: &str) -> String {
        text.split_inclusive('\n')
            .filter(|l| !l.starts_with('#'))
            .collect()
    }

    /// C's console lines for each `debug` bit and refusal, byte for byte.
    #[test]
    fn calc_trans_prints_c_s_lines() {
        let out = stdout_of(|| {
            calcTrans(8.0, 100.0, "Cu", 12);
            calcTrans(8.0, 100.0, "Unobtainium", 1);
            calcTrans(0.001, 100.0, "Cu", 1);
            calcTrans(10.0, 25.0, "Al", 15);
        });
        let c = include_str!("../../tests/oracle/filterDrive_calcTrans_print.txt");
        assert_eq!(out, oracle(c));
    }

    /// C's `calcTrans` to the bit over every material, out-of-range energies
    /// included: `1.0/(d*mu)` then `*= 1.e4` rounds differently from
    /// `1.e4/(d*mu)` on 138 of these cases. The oracle's transmissions are
    /// glibc's `exp`; C elsewhere calls its own libm's, which rounds a few
    /// the other way (macOS arm64: Fe 2.3491 3.78), so off glibc the
    /// transmission expected is this libm's `exp` of C's absLen.
    #[test]
    fn calc_trans_is_c_to_the_bit() {
        let oracle = include_str!("../../tests/oracle/filterDrive_calcTrans.txt");
        let mut cases = 0;
        for line in oracle.lines().filter(|l| !l.starts_with('#')) {
            let [mat, kev, um, abs_len, bits] = line.split(' ').collect::<Vec<_>>()[..] else {
                panic!("{line}");
            };
            let (kev, um): (f64, f64) = (kev.parse().unwrap(), um.parse().unwrap());
            let t = calcTrans(kev, um, mat, 0);
            let rs_abs_len = FILTER_MATERIALS
                .iter()
                .find(|m| m.name == mat)
                .and_then(|m| attenuation(m, kev, um))
                .map(|a| format!("{:016x}", a.abs_len_um.to_bits()));
            assert_eq!(rs_abs_len.as_deref().unwrap_or("-"), abs_len, "{line}");
            let want = match u64::from_str_radix(abs_len, 16) {
                Ok(c) if !cfg!(all(target_os = "linux", target_env = "gnu")) => {
                    format!("{:016x}", (-(um / f64::from_bits(c))).exp().to_bits())
                }
                _ => bits.to_string(),
            };
            assert_eq!(format!("{:016x}", t.to_bits()), want, "{line}: {t:e}");
            cases += 1;
        }
        assert_eq!(cases, 1056);
    }
}
