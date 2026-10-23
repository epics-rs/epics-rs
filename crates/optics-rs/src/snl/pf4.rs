//! `pf4.st`, compiled by snc-rs from `build.rs` and run on `epics-seq-rs`.
//! Its escaped C is the `%%#include` lines, the prototypes (`:134-146`) and
//! the functions after the program (`:491-847`); the functions are here in
//! Rust under their C names, and `chantler.h`'s table is
//! [`crate::data::chantler`].
//!
//! Defects fixed in the `.st` copy (the Other blade's interval and its
//! missing data, the glass term below 2 keV, the show-array count) are named
//! there.

// SNL calls these by their C names, with C's arity.
#![allow(non_snake_case, clippy::too_many_arguments)]

use crate::data::chantler::{FILTER_MATERIALS, FilterMaterial, other_absorption_length_um};
use epics_seq_rs::Place;
use epics_seq_rs::c::*;

macro_rules! seqg_escaped {
    (r#"#include <stdio.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"#include <string.h>"#) => {};
    (r#"#include "chantler.h""#) => {};
    // The prototypes (`:134-146`).
    (r#"static double TiAbsorptionLength(double); static double GlassAbsorptionLength(double); static double AlAbsorptionLength(double); static int isLegalOther(char *s); static double OtherAbsorptionLength(double keV, char *species); static int fillShowArrays(char *species, int N, float *E, float *T); static int numInArray(int,short*,short); static void sortDecreasing(int,double*,short*,int); static double thickZ(int,int,int,int,int,int,int,int,int,int,double,double,double,double); static void RecalcFilters(double,short*,double*,int,int,int,int,int,int,double,double,double,double, char *, char *, char *, char *);"#) => {};
    // The functions (`:491-847`), below in Rust.
    (r#"double AlAbsorptionLength(double keV) { double Wcoef0[]={1.90195,-0.00120447,4.3745e-7,8.68635e-11,3.40793e-15,-1.05816e-19,5.83389e-25}; double Wcoef1[]={-1625.33,0.328256,-2.68391e-5,1.26554e-9,-2.41557e-14,2.12864e-19,-7.28743e-25}; double kink = 26797.5; double* pointer; double sum,power; double eV; int i; eV = keV * 1000; if (eV>60000.) eV = 60000.; pointer = (eV<kink) ? Wcoef0 : Wcoef1; sum = 0.; power=1.; for( i=0; i<7; i++ ) { sum += pointer[i] * power; power *= eV; } return sum; } double TiAbsorptionLength(double keV) { double eV; double mu; eV = keV * 1000.; if( eV < 1e3 ) mu = 0; else if( eV < 4966.4 ) { double c0=0.00092284; double c1=2.5891e+08; double powA=-2.6651; mu = c0+c1*pow(eV,powA); } else { double offset = 5.63768167444831e-5; double amp = 24061652313.4169; double powB = -2.91380053083527; double intercept = -0.268162843203489; double slope = 3.74221014277593e-5; double ampExp = -1.05663835782997; double invTau = -0.000570785180739491; double extra = (eV < 6456) ? (intercept+slope*eV) : (ampExp*exp(invTau*eV)); mu = (offset + amp*pow(eV,powB) + extra); } return( 1./mu ); } double GlassAbsorptionLength(double keV) { double absLength; double c0,c1,c2; double c3,c4,c5,c6,c7; double keV2, keVn; keV2 = keV*keV; if( keV < 2 ) absLength = 0; else if( keV < 2.472 ) { c0 = 0.5059463974; c1 = -0.1259565387; c2 = 0.01763933889; absLength = c0 + c1*keV + c2*keV2; } else if( keV < 3.6084 ) { c0 = 0.4570603245; c1 = -0.08869920063; c2 = 0.01032934773; absLength = c0 + c1*keV + c2*keV2; } else if( keV < 4.0385 ) { c0 = 0.3708574258; c1 = -0.04453063888; c2 = 3.979930821e-3; absLength = c0 + c1*keV + c2*keV2; } else if( keV < 7.112 ) { c0 = 0.2830642538; c1 = -0.0223186563; c2 = 1.412011413e-3; absLength = c0 + c1*keV + c2*keV2; } else { c0 = 0.2715022686; c1 = -0.02428526798; c2 = 2.984228845e-3; c3 = -2.003675391e-4; c4 = 7.983398893e-6; c5 = -1.869726202e-7; c6 = 2.378962632e-9; c7 = -1.270082060e-11; absLength = c0 + c1*keV; keVn = keV2; absLength += c2*keVn; keVn *= keV; absLength += c3*keVn; keVn *= keV; absLength += c4*keVn; keVn *= keV; absLength += c5*keVn; keVn *= keV; absLength += c6*keVn; keVn *= keV; absLength += c7*keVn; } absLength *= keV*keV*keV; return( absLength ); } int isLegalOther(char *s) { int i; for (i=0; i<NUM_SPECIES; i++) { if (strcmp(filtermat[i].name, s) == 0) break; } if (i >= NUM_SPECIES) return(0); return(1); } double OtherAbsorptionLength(double keV, char *species) { int i, j; double mu, frac, absLen; for (i=0; i<NUM_SPECIES; i++) { if (strcmp(filtermat[i].name, species) == 0) break; } if (i >= NUM_SPECIES) { return(0.); } for (j=0; j<filtermat[i].numEntries; j++) { if (keV < filtermat[i].keV[j]) break; } if ((j < 1) || (j >= filtermat[i].numEntries)) { return(0.); } frac = (keV - filtermat[i].keV[j-1]) / (filtermat[i].keV[j] - filtermat[i].keV[j-1]); mu = filtermat[i].mu[j-1] + frac * (filtermat[i].mu[j] - filtermat[i].mu[j-1]); absLen = 1/(matdensity[i]*mu); absLen *= 1.e4; return(absLen); } void RecalcFilters(double keV,short* bits,double* xmit,int d,int b,int z1, int z2,int z3,int z4,double f1,double f2,double f3,double f4, char *other1, char *other2, char *other3, char *other4) { double absLenAl,absLenTi,absLenGlass; double xAl,xTi,xGlass; double xOther1, xOther2, xOther3, xOther4; int i; double absLenOther1=0, absLenOther2=0, absLenOther3=0, absLenOther4=0; if (d >= 10) printf("\nRecalcFilters: keV=%g\n",keV); absLenAl = AlAbsorptionLength(keV); absLenTi = TiAbsorptionLength(keV); absLenGlass = GlassAbsorptionLength(keV); if (z1==3) absLenOther1 = OtherAbsorptionLength(keV, other1); if (z2==3) absLenOther2 = OtherAbsorptionLength(keV, other2); if (z3==3) absLenOther3 = OtherAbsorptionLength(keV, other3); if (z4==3) absLenOther4 = OtherAbsorptionLength(keV, other4); if (z1==3 && f1 > 0 && absLenOther1 == 0) printf("RecalcFilters: no data for Other blade 1 '%s' at %g keV; it is left out\n", other1, keV); if (z2==3 && f2 > 0 && absLenOther2 == 0) printf("RecalcFilters: no data for Other blade 2 '%s' at %g keV; it is left out\n", other2, keV); if (z3==3 && f3 > 0 && absLenOther3 == 0) printf("RecalcFilters: no data for Other blade 3 '%s' at %g keV; it is left out\n", other3, keV); if (z4==3 && f4 > 0 && absLenOther4 == 0) printf("RecalcFilters: no data for Other blade 4 '%s' at %g keV; it is left out\n", other4, keV); for (i=0;i<16;i++) { if (b) { xAl = (z1==0 && i&1) ? f1 : 0; xAl += (z2==0 && i&2) ? f2 : 0; xAl += (z3==0 && i&4) ? f3 : 0; xAl += (z4==0 && i&8) ? f4 : 0; xTi = (z1==1 && i&1) ? f1 : 0; xTi += (z2==1 && i&2) ? f2 : 0; xTi += (z3==1 && i&4) ? f3 : 0; xTi += (z4==1 && i&8) ? f4 : 0; xGlass = (z1==2 && i&1) ? f1 : 0; xGlass += (z2==2 && i&2) ? f2 : 0; xGlass += (z3==2 && i&4) ? f3 : 0; xGlass += (z4==2 && i&8) ? f4 : 0; xOther1 = (z1==3 && i&1) ? f1 : 0; xOther2 = (z2==3 && i&2) ? f2 : 0; xOther3 = (z3==3 && i&4) ? f3 : 0; xOther4 = (z4==3 && i&8) ? f4 : 0; if (d >= 10) { printf("       in RecalcFilters %3d, Aluminum = %g,   Titanium = %g,   Glass = %g\n",i,xAl,xTi,xGlass); } xmit[i] = exp(-xAl*1000./absLenAl); xmit[i] *= exp(-xTi*1000./absLenTi); if (xGlass > 0) xmit[i] *= exp(-xGlass*1000./absLenGlass); if (xOther1 > 0 && absLenOther1 > 0) xmit[i] *= exp(-xOther1*1000./absLenOther1); if (xOther2 > 0 && absLenOther2 > 0) xmit[i] *= exp(-xOther2*1000./absLenOther2); if (xOther3 > 0 && absLenOther3 > 0) xmit[i] *= exp(-xOther3*1000./absLenOther3); if (xOther4 > 0 && absLenOther4 > 0) xmit[i] *= exp(-xOther4*1000./absLenOther4); } else xmit[i] = 1.; bits[i] = i; } if (b) sortDecreasing(d,xmit,bits,16); } void sortDecreasing(int d,double* arr,short* bits,int n) { int ii, jj; double a; short b; if (d >= 9) { printf("       before sorting, bits=%d",bits[0]); for (ii=1; ii<n; ii++) { printf(",%d", bits[ii]); } printf("\n"); if (d >= 10) { printf("                   transmit=%g",arr[0]); for (ii=1; ii<n/3; ii++) { printf(", %g", arr[ii]); } printf("\n"); } } for (jj=1;jj<n;jj++) { a = arr[jj]; b = bits[jj]; ii = jj-1; while(ii>=0 && arr[ii]<a) { arr[ii+1] = arr[ii]; bits[ii+1] = bits[ii]; ii--; } arr[ii+1] = a; bits[ii+1] = b; } if (d >= 8) { printf("       after sorting, bits=%d",bits[0]); for (ii=1; ii<n; ii++) { printf(",%d", bits[ii]); } printf("\n"); if (d >= 10) { printf("                   transmit=%g",arr[0]); for (ii=1; ii<n/3; ii++) { printf(", %g", arr[ii]); } printf("\n"); } } } double thickZ(int z,int b,int b1,int b2,int b3,int b4,int z1,int z2,int z3, int z4,double f1,double f2,double f3,double f4) { double sum = 0.0; if( b ) { sum += (b1 && (z1==z)) ? f1 : 0; sum += (b2 && (z2==z)) ? f2 : 0; sum += (b3 && (z3==z)) ? f3 : 0; sum += (b4 && (z4==z)) ? f4 : 0; } return sum; } int numInArray(int N,short arr[],short value) { int i; for( i=0; i<N; i++ ) if( value==arr[i] ) return i; return 0; } static int fillShowArrays(char *species, int N, float *E, float *T) { int i, j, k; for (i=0; i<NUM_SPECIES; i++) { if (strcmp(filtermat[i].name, species) == 0) break; } if (i >= NUM_SPECIES) { return(1); } for (j=0; j<filtermat[i].numEntries; j++) { E[j] = filtermat[i].keV[j]; T[j] = filtermat[i].mu[j]; } k = j-1; for (; j<N; j++) { E[j] = filtermat[i].keV[k]; T[j] = filtermat[i].mu[k]; } return(0); }"#) => {};
}

epics_seq_rs::include_program!("pf4");

/// `AlAbsorptionLength` (`pf4.st:495-516`), microns.
fn AlAbsorptionLength(keV: f64) -> f64 {
    const WCOEF0: [f64; 7] = [
        1.90195,
        -0.00120447,
        4.3745e-7,
        8.68635e-11,
        3.40793e-15,
        -1.05816e-19,
        5.83389e-25,
    ];
    const WCOEF1: [f64; 7] = [
        -1625.33,
        0.328256,
        -2.68391e-5,
        1.26554e-9,
        -2.41557e-14,
        2.12864e-19,
        -7.28743e-25,
    ];
    const KINK: f64 = 26797.5;

    let mut eV = keV * 1000.0;
    if eV > 60000.0 {
        eV = 60000.0;
    }
    let coef = if eV < KINK { &WCOEF0 } else { &WCOEF1 };
    let mut sum = 0.0;
    let mut power = 1.0;
    for &c in coef {
        sum += c * power;
        power *= eV;
    }
    sum
}

/// `TiAbsorptionLength` (`pf4.st:520-549`), microns: `1/0`, +inf, below
/// 1 keV.
fn TiAbsorptionLength(keV: f64) -> f64 {
    let eV = keV * 1000.0;
    let mu = if eV < 1e3 {
        0.0
    } else if eV < 4966.4 {
        let c0 = 0.00092284;
        let c1 = 2.5891e+08;
        let powA = -2.6651;
        c0 + c1 * eV.powf(powA)
    } else {
        let offset = 5.63768167444831e-5;
        let amp = 24061652313.4169;
        let powB = -2.91380053083527;
        let intercept = -0.268162843203489;
        let slope = 3.74221014277593e-5;
        let ampExp = -1.05663835782997;
        let invTau = -0.000570785180739491;
        let extra = if eV < 6456.0 {
            intercept + slope * eV
        } else {
            ampExp * (invTau * eV).exp()
        };
        offset + amp * eV.powf(powB) + extra
    };
    1.0 / mu
}

/// `GlassAbsorptionLength` (`pf4.st:564-621`), microns: 0 below 2 keV.
fn GlassAbsorptionLength(keV: f64) -> f64 {
    let keV2 = keV * keV;
    let mut absLength;
    if keV < 2.0 {
        absLength = 0.0;
    } else if keV < 2.472 {
        absLength = 0.5059463974 + -0.1259565387 * keV + 0.01763933889 * keV2;
    } else if keV < 3.6084 {
        absLength = 0.4570603245 + -0.08869920063 * keV + 0.01032934773 * keV2;
    } else if keV < 4.0385 {
        absLength = 0.3708574258 + -0.04453063888 * keV + 3.979930821e-3 * keV2;
    } else if keV < 7.112 {
        absLength = 0.2830642538 + -0.0223186563 * keV + 1.412011413e-3 * keV2;
    } else {
        let c0 = 0.2715022686;
        let c1 = -0.02428526798;
        let c2 = 2.984228845e-3;
        let c3 = -2.003675391e-4;
        let c4 = 7.983398893e-6;
        let c5 = -1.869726202e-7;
        let c6 = 2.378962632e-9;
        let c7 = -1.270082060e-11;
        absLength = c0 + c1 * keV;
        let mut keVn = keV2;
        absLength += c2 * keVn;
        keVn *= keV;
        absLength += c3 * keVn;
        keVn *= keV;
        absLength += c4 * keVn;
        keVn *= keV;
        absLength += c5 * keVn;
        keVn *= keV;
        absLength += c6 * keVn;
        keVn *= keV;
        absLength += c7 * keVn;
    }
    absLength *= keV * keV * keV;
    absLength
}

/// C's `strcmp(filtermat[i].name, species) == 0` search, case-sensitive.
fn filtermat(species: &[u8]) -> Option<&'static FilterMaterial> {
    FILTER_MATERIALS
        .iter()
        .find(|m| m.name.as_bytes() == species)
}

/// `isLegalOther` (`pf4.st:624-631`).
pub fn isLegalOther(s: impl CSrc) -> i32 {
    s.with_c_bytes(filtermat).is_some() as i32
}

/// `OtherAbsorptionLength` (`pf4.st:633-661`), microns, on `[j-1, j]` as
/// the `.st` copy fixes it (CBUG-B1, B2: see [`other_absorption_length_um`]);
/// `0.` for an unknown species or an energy outside its table.
fn OtherAbsorptionLength(keV: f64, species: &[u8]) -> f64 {
    filtermat(species)
        .and_then(|mat| other_absorption_length_um(mat, keV))
        .unwrap_or(0.)
}

/// `RecalcFilters` (`pf4.st:664-733`): the transmission of the 16 blade
/// combinations, each material's thickness summed into one exponent, sorted
/// by decreasing transmission while the bank is on.
pub fn RecalcFilters(
    keV: f64,
    bits: &impl Place<[i16; 16]>,
    xmit: &impl Place<[f64; 16]>,
    d: i32,
    b: i32,
    z1: i32,
    z2: i32,
    z3: i32,
    z4: i32,
    f1: f64,
    f2: f64,
    f3: f64,
    f4: f64,
    other1: impl CSrc,
    other2: impl CSrc,
    other3: impl CSrc,
    other4: impl CSrc,
) {
    let z = [z1, z2, z3, z4];
    let f = [f1, f2, f3, f4];
    if d >= 10 {
        printf("\nRecalcFilters: keV=%g\n", &[CArg::from(keV)]);
    }
    let absLenAl = AlAbsorptionLength(keV);
    let absLenTi = TiAbsorptionLength(keV);
    let absLenGlass = GlassAbsorptionLength(keV);
    let species = [
        (z1 == 3).then(|| other1.to_c_vec()),
        (z2 == 3).then(|| other2.to_c_vec()),
        (z3 == 3).then(|| other3.to_c_vec()),
        (z4 == 3).then(|| other4.to_c_vec()),
    ];
    let absLenOther = species
        .each_ref()
        .map(|s| s.as_deref().map_or(0., |s| OtherAbsorptionLength(keV, s)));
    for k in 0..4 {
        if let Some(s) = &species[k]
            && f[k] > 0.0
            && absLenOther[k] == 0.0
        {
            printf(
                "RecalcFilters: no data for Other blade %d '%s' at %g keV; it is left out\n",
                &[
                    CArg::from(k as i32 + 1),
                    CArg::from(&s[..]),
                    CArg::from(keV),
                ],
            );
        }
    }

    let mut x = [0.0; 16];
    let mut bit = [0i16; 16];
    for i in 0..16 {
        if b != 0 {
            let inBeam = |k: usize, mat: i32| z[k] == mat && i & (1 << k) != 0;
            let sum =
                |mat: i32| (0..4).fold(0.0, |s, k| s + if inBeam(k, mat) { f[k] } else { 0.0 });
            let (xAl, xTi, xGlass) = (sum(0), sum(1), sum(2));
            if d >= 10 {
                printf(
                    "       in RecalcFilters %3d, Aluminum = %g,   Titanium = %g,   Glass = %g\n",
                    &[
                        CArg::from(i as i32),
                        CArg::from(xAl),
                        CArg::from(xTi),
                        CArg::from(xGlass),
                    ],
                );
            }
            x[i] = (-xAl * 1000. / absLenAl).exp();
            x[i] *= (-xTi * 1000. / absLenTi).exp();
            if xGlass > 0.0 {
                x[i] *= (-xGlass * 1000. / absLenGlass).exp();
            }
            for k in 0..4 {
                let xOther = if inBeam(k, 3) { f[k] } else { 0.0 };
                if xOther > 0.0 && absLenOther[k] > 0.0 {
                    x[i] *= (-xOther * 1000. / absLenOther[k]).exp();
                }
            }
        } else {
            x[i] = 1.;
        }
        bit[i] = i as i16;
    }
    if b != 0 {
        sortDecreasing(d, &mut x, &mut bit);
    }
    xmit.set(x);
    bits.set(bit);
}

/// `sortDecreasing` (`pf4.st:736-794`), an insertion sort.
fn sortDecreasing(d: i32, arr: &mut [f64; 16], bits: &mut [i16; 16]) {
    let print = |head: &str, arr: &[f64; 16], bits: &[i16; 16]| {
        printf(head, &[CArg::from(bits[0])]);
        for &b in &bits[1..] {
            printf(",%d", &[CArg::from(b)]);
        }
        printf("\n", &[]);
        if d >= 10 {
            printf("                   transmit=%g", &[CArg::from(arr[0])]);
            for &t in &arr[1..16 / 3] {
                printf(", %g", &[CArg::from(t)]);
            }
            printf("\n", &[]);
        }
    };
    if d >= 9 {
        print("       before sorting, bits=%d", arr, bits);
    }
    for jj in 1..16 {
        let a = arr[jj];
        let b = bits[jj];
        let mut ii = jj;
        while ii > 0 && arr[ii - 1] < a {
            arr[ii] = arr[ii - 1];
            bits[ii] = bits[ii - 1];
            ii -= 1;
        }
        arr[ii] = a;
        bits[ii] = b;
    }
    if d >= 8 {
        print("       after sorting, bits=%d", arr, bits);
    }
}

/// `thickZ` (`pf4.st:798-812`): the thickness of material `z` in the beam.
pub fn thickZ(
    z: i32,
    b: i32,
    b1: i32,
    b2: i32,
    b3: i32,
    b4: i32,
    z1: i32,
    z2: i32,
    z3: i32,
    z4: i32,
    f1: f64,
    f2: f64,
    f3: f64,
    f4: f64,
) -> f64 {
    let mut sum = 0.0;
    if b != 0 {
        for (bn, zn, fk) in [(b1, z1, f1), (b2, z2, f2), (b3, z3, f3), (b4, z4, f4)] {
            sum += if bn != 0 && zn == z { fk } else { 0.0 };
        }
    }
    sum
}

/// `numInArray` (`pf4.st:817-823`): the first index of `arr` holding
/// `value`, else 0.
pub fn numInArray(N: i32, arr: &impl Place<[i16; 16]>, value: i16) -> i32 {
    arr.modify(|arr| {
        arr.iter()
            .take(N.max(0) as usize)
            .position(|&a| a == value)
            .map_or(0, |i| i as i32)
    })
}

/// `fillShowArrays` (`pf4.st:825-845`): a species' table into `E`/`T`,
/// padded with its last entry up to `N`; 1 for an unknown species.
pub fn fillShowArrays(
    species: impl CSrc,
    N: i32,
    E: &impl Place<[f32; 300]>,
    T: &impl Place<[f32; 300]>,
) -> i32 {
    let Some(mat) = species.with_c_bytes(filtermat) else {
        return 1;
    };
    // The `.st` copy bounds `N` by the arrays at its call.
    let fill = |dst: &mut [f32; 300], src: &[f32]| {
        let n = N.max(0) as usize;
        let entries = src.len().min(dst.len());
        dst[..entries].copy_from_slice(&src[..entries]);
        let k = entries.saturating_sub(1);
        dst.iter_mut()
            .take(n)
            .skip(entries)
            .for_each(|v| *v = src[k]);
    };
    E.modify(|e| fill(e, mat.kev));
    T.modify(|t| fill(t, mat.mu));
    0
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;

    use super::*;
    use epics_libcom_rs::runtime::stdio::redirect_stdout;
    use epics_seq_rs::SeqVar;

    /// [`RecalcFilters`] with the bank on (`bankctl` 3, as `bankControl`
    /// calls it): the transmissions and bit patterns it leaves.
    fn recalc(keV: f64, z: [i32; 4], f: [f64; 4], other: [&str; 4]) -> ([f64; 16], [i16; 16]) {
        let (bits, xmit) = (SeqVar::new([0i16; 16]), SeqVar::new([0.0; 16]));
        let [z1, z2, z3, z4] = z;
        let [f1, f2, f3, f4] = f;
        let [o1, o2, o3, o4] = other;
        RecalcFilters(
            keV, &bits, &xmit, 0, 3, z1, z2, z3, z4, f1, f2, f3, f4, o1, o2, o3, o4,
        );
        (xmit.get(), bits.get())
    }

    /// One blade of `mm` in bank position 1, the rest empty Al.
    fn one_blade(keV: f64, mm: f64, z: i32, other: &str) -> f64 {
        let (xmit, bits) = recalc(keV, [z, 0, 0, 0], [mm, 0.0, 0.0, 0.0], [other, "", "", ""]);
        xmit[bits.iter().position(|&b| b == 1).unwrap()]
    }

    /// C `RecalcFilters` at 10 keV for Al 0.2 / Ti 0.02 / glass 0.5 / Al 0.4 mm,
    /// printed with `%.17g` from `pf4.st`'s own functions. Positions 9, 11, 13
    /// and 15 hold both Al blades: C sums them into one exponent, and a product
    /// of per-blade factors is off in the last bits there.
    #[test]
    fn recalc_filters_is_c_recalc_filters_to_the_bit() {
        let c_bits = [0, 2, 1, 3, 4, 8, 6, 10, 5, 9, 7, 11, 12, 14, 13, 15];
        let c_xmit = [
            1.0,
            0.3695423943703097,
            0.2506658388144326,
            0.09263165426232754,
            0.07205806894912094,
            0.0628333627485431,
            0.02662851133315902,
            0.023219591316434845,
            0.01806249629647962,
            0.01575017757889507,
            0.0066748581297059306,
            0.005820358334262452,
            0.004527650785239646,
            0.0016731589120500719,
            0.0011349273819409196,
            0.00041940378215887445,
        ];
        assert_eq!(
            recalc(10.0, [0, 1, 2, 0], [0.2, 0.02, 0.5, 0.4], ["", "", "", ""]),
            (c_xmit, c_bits)
        );
    }

    /// The `.st` copy's `RecalcFilters` at 1.5 keV for the same blades,
    /// printed with `%.17g`: glass absorbs fully below 2 keV, and the eight
    /// combinations without it are numbers where upstream C gave NaN.
    #[test]
    fn recalc_filters_below_2_kev_is_the_st_c_to_the_bit() {
        let c_bits = [0, 2, 1, 3, 8, 10, 9, 11, 4, 5, 6, 7, 12, 13, 14, 15];
        let mut c_xmit = [0.0; 16];
        c_xmit[..8].copy_from_slice(&[
            1.0,
            1.890233923500925e-08,
            2.967151325687518e-63,
            5.608610091975288e-71,
            8.803986989529196e-126,
            1.664159486966887e-133,
            2.6122761667314988e-188,
            4.937813027908837e-196,
        ]);
        assert_eq!(
            recalc(1.5, [0, 1, 2, 0], [0.2, 0.02, 0.5, 0.4], ["", "", "", ""]),
            (c_xmit, c_bits)
        );
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

    /// C's console lines at `d` 10 and 8, the Other blade without data
    /// included, byte for byte.
    #[test]
    fn recalc_filters_prints_c_s_lines() {
        let (bits, xmit) = (SeqVar::new([0i16; 16]), SeqVar::new([0.0; 16]));
        let out = stdout_of(|| {
            RecalcFilters(
                10.0,
                &bits,
                &xmit,
                10,
                1,
                0,
                1,
                3,
                3,
                0.2,
                0.02,
                0.5,
                0.4,
                "",
                "",
                "Cu",
                "Unobtainium",
            );
            RecalcFilters(
                10.0, &bits, &xmit, 8, 1, 0, 1, 2, 0, 0.2, 0.02, 0.5, 0.4, "", "", "", "",
            );
        });
        let c = include_str!("../../tests/oracle/pf4_RecalcFilters_print.txt");
        assert_eq!(out, oracle(c));
    }

    /// A bank that is off transmits fully, in bit order.
    #[test]
    fn recalc_filters_bank_off() {
        let (bits, xmit) = (SeqVar::new([7i16; 16]), SeqVar::new([0.0; 16]));
        RecalcFilters(
            10.0, &bits, &xmit, 0, 0, 0, 0, 0, 0, 1.0, 1.0, 1.0, 1.0, "", "", "", "",
        );
        assert_eq!(xmit.get(), [1.0; 16]);
        assert_eq!(bits.get(), std::array::from_fn(|i| i as i16));
    }

    #[test]
    fn is_legal_other_is_a_case_sensitive_lookup() {
        assert_eq!(isLegalOther("Cu"), 1);
        assert_eq!(isLegalOther("Al"), 1);
        assert_eq!(isLegalOther("cu"), 0);
        assert_eq!(isLegalOther("Unobtainium"), 0);
        assert_eq!(isLegalOther(""), 0);
    }

    /// CBUG-B4: an Other blade of an unknown species is left out (1.0),
    /// where upstream C divided by its `0.` (`pf4.st:640-642`) and reported
    /// it opaque.
    #[test]
    fn an_other_blade_of_an_unknown_species_is_left_out() {
        assert_eq!(one_blade(10.0, 1.0, 3, "Unobtainium"), 1.0);
        assert_eq!(one_blade(10.0, 1.0, 3, "cu"), 1.0);
    }

    /// CBUG-B4 for an energy at or above the end of the table.
    #[test]
    fn an_other_blade_above_its_table_is_left_out() {
        let cu = crate::data::chantler::find_material("Cu").unwrap();
        let last = cu.kev[cu.kev.len() - 1] as f64;
        assert_eq!(one_blade(500.0, 1.0, 3, "Cu"), 1.0);
        assert_eq!(one_blade(last, 1.0, 3, "Cu"), 1.0);
        let inside = one_blade(last - 0.001, 1.0, 3, "Cu");
        assert!(inside > 0.0 && inside < 1.0);
    }

    /// C guards the Other multiply with `if (xOther1 > 0 ...)` (`pf4.st:723`),
    /// so a blade of zero thickness is 1.0 whatever its species.
    #[test]
    fn an_other_blade_of_zero_thickness_transmits() {
        assert_eq!(one_blade(10.0, 0.0, 3, "Unobtainium"), 1.0);
    }

    /// The `.st` copy's `OtherAbsorptionLength`, built with `chantler.c`, to
    /// the bit: Cu at 8, 10, 100 and 432.9 keV (its top interval), and its
    /// `0.` below the first node and from the last one.
    #[test]
    fn other_absorption_length_is_the_st_c_to_the_bit() {
        for (keV, c) in [
            (8.0, 0x4035f2e07fa76aaf),
            (10.0, 0x4014b6c320d93793),
            (100.0, 0x40a32842dce22521),
            (432.9, 0x40c83b37843f5c4b),
        ] {
            assert_eq!(OtherAbsorptionLength(keV, b"Cu"), f64::from_bits(c));
        }
        for keV in [0.005, 433.0, 1000.0] {
            assert_eq!(OtherAbsorptionLength(keV, b"Cu"), 0.0);
        }
    }

    /// CBUG-B1: the Other blade comes from the interval holding the
    /// energy, where C extrapolates off the one above it:
    /// `absLen(Cu, 10 keV) = 5.28411457133 um -> 0.827582511947` for 1 um.
    #[test]
    fn an_other_blade_uses_the_interval_holding_the_energy() {
        let cu = crate::data::chantler::find_material("Cu").unwrap();
        let absLen = 1.0e4 / (cu.density * 215.521099278);
        let want = (-1.0 / absLen).exp();
        let t = one_blade(10.0, 0.001, 3, "Cu");
        assert!(
            (t - want).abs() < 1e-9,
            "Cu 1 um at 10 keV: {t}, want {want}"
        );
        assert!((t - 0.827582511947).abs() > 1e-3);
    }

    #[test]
    fn al_absorption_length_is_the_polynomial_capped_at_60_kev() {
        let len = AlAbsorptionLength(10.0);
        assert!((144.0..145.0).contains(&len), "Al at 10 keV: {len}");
        assert_eq!(AlAbsorptionLength(70.0), AlAbsorptionLength(60.0));
    }

    /// Ti is +inf below 1 keV, so it transmits; glass is 0 below 2 keV, so
    /// it absorbs.
    #[test]
    fn ti_and_glass_below_their_fits() {
        assert!(TiAbsorptionLength(0.5).is_infinite());
        assert_eq!(one_blade(0.5, 1.0, 1, ""), 1.0);
        assert_eq!(GlassAbsorptionLength(1.5), 0.0);
        assert_eq!(one_blade(1.5, 1.0, 2, ""), 0.0);
    }

    #[test]
    fn glass_is_the_fit_above_the_iron_edge() {
        let len = GlassAbsorptionLength(10.0);
        assert!((185.0..195.0).contains(&len), "glass at 10 keV: {len}");
        let t = one_blade(10.0, 1.0, 2, "");
        assert!((4.5e-3..6.0e-3).contains(&t), "glass 1 mm at 10 keV: {t}");
    }

    /// The first index holding the value, else 0.
    #[test]
    fn num_in_array_is_the_first_index_or_zero() {
        let bits = SeqVar::new(std::array::from_fn(|i| (15 - i) as i16));
        assert_eq!(numInArray(16, &bits, 15), 0);
        assert_eq!(numInArray(16, &bits, 0), 15);
        assert_eq!(numInArray(16, &bits, 16), 0);
    }

    #[test]
    fn thick_z_sums_the_blades_of_a_material_in_the_beam() {
        let t = |z, b| thickZ(z, b, 1, 0, 1, 0, 0, 1, 0, 1, 1.0, 2.0, 3.0, 4.0);
        assert_eq!(t(0, 2), 4.0);
        assert_eq!(t(1, 2), 0.0);
        assert_eq!(t(0, 0), 0.0);
    }

    /// The table, then its last entry up to `N`; 1 and nothing written for
    /// an unknown species.
    #[test]
    fn fill_show_arrays_pads_with_the_last_entry() {
        let cu = crate::data::chantler::find_material("Cu").unwrap();
        let n = cu.kev.len();
        let (e, t) = (SeqVar::new([0.0f32; 300]), SeqVar::new([0.0f32; 300]));
        assert_eq!(fillShowArrays("Unobtainium", 300, &e, &t), 1);
        assert_eq!(e.get(), [0.0; 300]);
        assert_eq!(fillShowArrays("Cu", 300, &e, &t), 0);
        let (e, t) = (e.get(), t.get());
        assert_eq!(&e[..n], cu.kev);
        assert_eq!(&t[..n], cu.mu);
        assert!(e[n..].iter().all(|&v| v == cu.kev[n - 1]));
        assert!(t[n..].iter().all(|&v| v == cu.mu[n - 1]));
    }

    /// An `N` past the arrays fills them and stops.
    #[test]
    fn fill_show_arrays_stops_at_the_array_end() {
        let cu = crate::data::chantler::find_material("Cu").unwrap();
        let (e, t) = (SeqVar::new([0.0f32; 300]), SeqVar::new([0.0f32; 300]));
        assert_eq!(fillShowArrays("Cu", 400, &e, &t), 0);
        assert_eq!(e.get()[299], cu.kev[cu.kev.len() - 1]);
    }
}
