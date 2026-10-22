//! `Io.st`, compiled by snc-rs from `build.rs` and run on `epics-seq-rs`.
//!
//! The escaped C is the flux calculation in the `%{ }%` block and the
//! prototypes `%%` declares for it. `EvalFlux` reads the scaler and chamber
//! variables and writes `flux`, `ionPhotons`, `ionAbs` and `detector`, the
//! statics C's escaped code reaches by name. `Io.st` is compiled `-r`, so
//! they are the block `seqg::seqg_static_vars()` returns.

use epics_seq_rs::c::*;

macro_rules! seqg_escaped {
    (r#"#include "epicsThread.h""#) => {};
    (r#"#include <stdlib.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"static void EvalFlux(double aln, double dln1, double dln2, double dHe, double dAir, double dBe, double energy);"#) => {};
    (r#"static double photon(double cps, double work, double VtoF, double Volt_per_A, double aln, double deadL, double Kapton, int gasId, double energy);"#) => {};
    (r#"static double absorb(int id, double energy);"#) => {};
    (r#"static double absH(double energy);"#) => {};
    (r#"static double absHe(double energy);"#) => {};
    (r#"static double absBe(double energy);"#) => {};
    (r#"static double absC(double energy);"#) => {};
    (r#"static double absN(double energy);"#) => {};
    (r#"static double absO(double energy);"#) => {};
    (r#"static double absAr(double energy);"#) => {};
    (r#"static double absArPhoto(double energy);"#) => {};
    (r#"void EvalFlux( double aln, double dln1, double dln2, double dHe, double dAir, double dBe, double energy) { double AirAbs; double HeAbs; double KapAbs; double BeAbs; double fillAbsorb; double frontAbs; double cps; aln = aln / 10.; dln1 = dln1 / 10.; dln2 = dln2 / 10.; dHe = dHe / 10.; dAir = dAir / 10.; dBe = dBe * 2.54; switch (icChannel) { case 2: cps = s2; break; case 3: cps = s3; break; case 4: cps = s4; break; case 5: cps = s5; break; case 6: cps = s6; break; case 7: cps = s7; break; case 8: cps = s8; break; case 9: cps = s9; break; case 10: cps = s10; break; case 11: cps = s11; break; case 12: cps = s12; break; case 13: cps = s13; break; case 14: cps = s14; break; case 15: cps = s15; break; default: cps = 0; } cps /= (ticks/clock_rate); flux = 0.; flux += xHe * photon(cps,29.6, v2f,VperA,aln,dln1,kapton1,1,energy); flux += xN2 * photon(cps,36.3, v2f,VperA,aln,dln1,kapton1,2,energy); flux += xAr * photon(cps,24.4, v2f,VperA,aln,dln1,kapton1,3,energy); flux += xAir* photon(cps,35.36,v2f,VperA,aln,dln1,kapton1,4,energy); flux += xCO2* photon(cps,35.36,v2f,VperA,aln,dln1,kapton1,7,energy); AirAbs = exp(-dAir*absorb(4,energy)); HeAbs = exp(-dHe * absorb(1,energy) ); KapAbs = exp(-(kapton2+kapton1)*2.54 * absorb(6,energy) ); BeAbs = exp(-dBe * absorb(5,energy) ); fillAbsorb = absorb(1,energy)*xHe + absorb(2,energy)*xN2 + absorb(3,energy)*xAr + absorb(4,energy)*xAir + absorb(7,energy)*xCO2; ionAbs = exp(-(dln1+dln2+aln) * fillAbsorb); detector = flux * AirAbs * HeAbs * KapAbs * BeAbs * ionAbs * dEff; frontAbs = exp( -2.54*kapton1*absorb(6,energy) - dln1*fillAbsorb ); ionPhotons = (1.- exp(-aln * fillAbsorb)) * frontAbs * flux; } double photon( double cps, double work, double VtoF, double Volt_per_A, double aln, double deadL, double Kapton, int gasId, double energy) { double convHe = 6.647; double rhoHe = 0.0001785; double convN = 23.26; double rhoN = 0.00125; double convAr = 66.32; double rhoAr = 0.001784; double edgeAr = 3.202; double sum,E1,E2,E3; double part1,part2,part3; double photo; static double aHe[4] = { 6.06488, -3.29055, -0.107256, 0.0144465 }; static double aN[4] = { 11.2765, -2.65400, -0.200445, 0.0200765 }; static double aAr1[4] = { 13.9491, -1.82276, -0.328827, 0.0274382 }; static double aAr2[4] = { 12.2960, -2.63279, -0.073660, 0.0 }; static double aO[4] = { 11.7130, -2.57229, -0.205893, 0.0199244 }; static double aC[4] = { 10.6879, -2.71400, -0.200530, 0.0207248 }; E1 = log(energy); E2 = E1 * E1; E3 = E2 * E1; photo = 0; if (gasId == 1) { sum = exp(aHe[0] + aHe[1]*E1 + aHe[2]*E2 + aHe[3]*E3); photo = sum * rhoHe / convHe; } else if (gasId == 2) { sum = exp(aN[0] + aN[1]*E1 + aN[2]*E2 + aN[3]*E3); photo = sum * rhoN / convN; } else if (gasId == 3) { if (E1 > edgeAr) sum = exp(aAr1[0] + aAr1[1]*E1 + aAr1[2]*E2 + aAr1[3]*E3); else sum = exp(aAr2[0] + aAr2[1]*E1 + aAr2[2]*E2 + aAr2[3]*E3); photo = sum * rhoAr / convAr; } else if (gasId == 4) { sum = exp(aN[0] + aN[1]*E1 + aN[2]*E2 + aN[3]*E3); photo = sum * 0.000922 / convN; sum = exp(aO[0] + aO[1]*E1 + aO[2]*E2 + aO[3]*E3); photo += sum * 0.000266 / convN; if (E1 > edgeAr) sum = exp(aAr1[0] + aAr1[1]*E1 + aAr1[2]*E2 + aAr1[3]*E3); else sum = exp(aAr2[0] + aAr2[1]*E1 + aAr2[2]*E2 + aAr2[3]*E3); photo += sum * 1.66E-5 / convAr; } else if (gasId == 7) { sum = exp(aC[0] + aC[1]*E1 + aC[2]*E2 + aC[3]*E3); photo = sum * 0.0005396 / convN; sum = exp(aO[0] + aO[1]*E1 + aO[2]*E2 + aO[3]*E3); photo += sum * 0.0014374 / convN; } part1 = cps * work / (1.602E-19 * VtoF * Volt_per_A * energy * 1000.); part2 = 1 - exp(-photo * aln); part3 = exp(absorb(gasId,energy) * deadL) * exp(absorb(6,energy) * Kapton * 2.54); return (part1 * part3 / part2); } double absorb( int id, double energy) { double rhoH; double rhoHe; double rhoBe; double rhoC; double rhoN; double rhoO; double rhoAr; switch (id) { case 1: rhoHe = 0.0001785; return(absHe(energy)*rhoHe); case 5: rhoBe = 1.848; return(absBe(energy)*rhoBe); case 2: rhoN = 0.00125; return(absN(energy)*rhoN); case 4: rhoN = 0.000922; rhoO = 0.000266; rhoAr = 1.66E-5; return(absN(energy)*rhoN + absO(energy)*rhoO + absAr(energy)*rhoAr); case 3: rhoAr = 0.001784; return(absAr(energy)*rhoAr); case 6: rhoC = 0.981; rhoH = 0.037; rhoO = 0.297; rhoN = 0.105; return(absC(energy)*rhoC + absH(energy)*rhoH + absO(energy)*rhoO + absN(energy)*rhoN); case 7: rhoC = 0.0005396; rhoO = 0.0014374; return(absC(energy)*rhoC + absO(energy)*rhoO); } return 0; } double absH( double energy) { double conv; double E1,E2,E3; double photo; double coherent; double compton; static double a[4] = { 2.44964E+00,-3.34953E+00,-4.71370E-02, 7.09962E-03 }; static double b[4] = {-1.19075E-01,-9.37086E-01,-2.00538E-01, 1.06587E-02 }; static double c[4] = {-2.15772E+00, 1.32685E+00,-3.05620E-01, 1.85025E-02 }; conv = 1.674; E1 = log(energy); E2 = E1*E1; E3 = E2*E1; photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3); coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3); compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3); return( (photo+coherent+compton)/conv); } double absHe( double energy) { double conv; double E1,E2,E3; double photo; double coherent; double compton; static double a[4] = { 6.06488E+00,-3.29055E+00,-1.07256E-01, 1.44465E-02 }; static double b[4] = { 1.04768E+00,-8.51805E-02,-4.03527E-01, 2.69398E-02 }; static double c[4] = {-2.56357E+00, 2.02536E+00,-4.48710E-01, 2.79691E-02 }; conv = 6.647; E1 = log(energy); E2 = E1*E1; E3 = E2*E1; photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3); coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3); compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3); return((photo+coherent+compton)/conv); } double absBe( double energy) { double conv; double E1,E2,E3; double photo; double coherent; double compton; static double a[4] = { 9.04511E+00,-2.83487E+00,-2.10021E-01, 2.29526E-02 }; static double b[4] = { 2.00860E+00,-4.61920E-02,-3.37018E-01, 1.86939E-02 }; static double c[4] = {-6.90079E-01, 9.46448E-01,-1.71142E-01, 6.51413E-03 }; conv = 14.96; E1 = log(energy); E2 = E1*E1; E3 = E2*E1; photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3); coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3); compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3); return((photo+coherent+compton)/conv); } double absC( double energy) { double conv; double E1,E2,E3; double photo; double coherent; double compton; static double a[4] = { 1.06879E+01,-2.71400E+00,-2.00530E-01, 2.07248E-02 }; static double b[4] = { 3.10861E+00,-2.60580E-01,-2.71974E-01, 1.35181E-02 }; static double c[4] = {-9.82878E-01, 1.46693E+00,-2.93743E-01, 1.56005E-02 }; conv = 19.94; E1 = log(energy); E2 = E1*E1; E3 = E2*E1; photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3); coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3); compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3); return((photo+coherent+compton)/conv); } double absN( double energy) { double conv; double E1,E2,E3; double photo; double coherent; double compton; static double a[4] = { 1.12765E+01,-2.65400E+00,-2.00445E-01, 2.00765E-02 }; static double b[4] = { 3.47760E+00,-2.15762E-01,-2.88874E-01, 1.51312E-02 }; static double c[4] = {-1.23693E+00, 1.74510E+00,-3.54660E-01, 1.98705E-02 }; conv = 23.26; E1 = log(energy); E2 = E1*E1; E3 = E2*E1; photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3); coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3); compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3); return((photo+coherent+compton)/conv); } double absO( double energy) { double conv; double E1,E2,E3; double photo; double coherent; double compton; static double a[4] = { 1.17130E+01,-2.57229E+00,-2.05893E-01, 1.99244E-02 }; static double b[4] = { 3.77239E+00,-1.48539E-01,-3.07124E-01, 1.67303E-02 }; static double c[4] = {-1.73679E+00, 2.17686E+00,-4.49050E-01, 2.64733E-02 }; conv = 26.57; E1 = log(energy); E2 = E1*E1; E3 = E2*E1; photo = exp( a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3); coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3); compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3); return((photo+coherent+compton)/conv); } double absAr( double energy) { double conv; double E1,E2,E3; double photo; double coherent; double compton; double edge; static double a1[4] = { 1.39491E+01,-1.82276E+00,-3.28827E-01, 2.74382E-02 }; static double a2[4] = { 1.22960E+01,-2.63279E+00,-7.36600E-02, 0.}; static double b[4] = { 5.21079E+00, 1.35618E-01,-3.47214E-01, 1.84333E-02 }; static double c[4] = {-6.82105E-01, 1.74279E+00,-3.17646E-01, 1.56467E-02 }; conv = 66.32; edge = 3.202; E1 = log(energy); E2 = E1*E1; E3 = E2*E1; if (energy>edge) photo = exp( a1[0] + a1[1]*E1 + a1[2]*E2 + a1[3]*E3); else photo = exp( a2[0] + a2[1]*E1 + a2[2]*E2 + a2[3]*E3); coherent = exp( b[0] + b[1]*E1 + b[2]*E2 + b[3]*E3); compton = exp( c[0] + c[1]*E1 + c[2]*E2 + c[3]*E3); return((photo+coherent+compton)/conv); } double absArPhoto( double energy) { double conv; double E1,E2,E3; double photo; double rho; double edge; static double aAr1[4] = {13.9491, -1.82276, -0.328827, 0.0274382 }; static double aAr2[4] = {12.2960, -2.63279, -0.073660, 0.0 }; conv = 66.32; rho = 0.001784; edge = 3.202; E1 = log(energy); E2 = E1*E1; E3 = E2*E1; if (energy>edge) photo = exp( aAr1[0] + aAr1[1]*E1 + aAr1[2]*E2 + aAr1[3]*E3); else photo = exp( aAr2[0] + aAr2[1]*E1 + aAr2[2]*E2 + aAr2[3]*E3); return (photo / conv * rho); }"#) => {};
}

epics_seq_rs::include_program!("Io");

// The `#define`s the block uses; cpp has expanded them in the SNL code.
const _HELIUM_: i32 = 1;
const _NITROGEN_: i32 = 2;
const _ARGON_: i32 = 3;
const _AIR_: i32 = 4;
const _BERYLLIUM_: i32 = 5;
const _KAPTON_: i32 = 6;
const _CO2_: i32 = 7;
const CM_PER_INCH: f64 = 2.54;

/// `exp(a[0] + a[1]*E1 + a[2]*E2 + a[3]*E3)` with `E1 = log(energy)`.
fn fit(a: [f64; 4], energy: f64) -> f64 {
    let e1 = energy.ln();
    let e2 = e1 * e1;
    let e3 = e2 * e1;
    (a[0] + a[1] * e1 + a[2] * e2 + a[3] * e3).exp()
}

/// `EvalFlux`: the flux just before the chamber, and the share of it the
/// chamber gas and the path after it pass, from the selected scaler
/// channel's count rate. Lengths arrive in mm, `d_be` in inches.
#[allow(non_snake_case)]
fn EvalFlux(aln: f64, dln1: f64, dln2: f64, d_he: f64, d_air: f64, d_be: f64, energy: f64) {
    let v = seqg::seqg_static_vars();
    let aln = aln / 10.;
    let dln1 = dln1 / 10.;
    let dln2 = dln2 / 10.;
    let d_he = d_he / 10.;
    let d_air = d_air / 10.;
    let d_be = d_be * CM_PER_INCH;

    let cps = match v.icChannel.get() {
        2 => v.s2.get(),
        3 => v.s3.get(),
        4 => v.s4.get(),
        5 => v.s5.get(),
        6 => v.s6.get(),
        7 => v.s7.get(),
        8 => v.s8.get(),
        9 => v.s9.get(),
        10 => v.s10.get(),
        11 => v.s11.get(),
        12 => v.s12.get(),
        13 => v.s13.get(),
        14 => v.s14.get(),
        15 => v.s15.get(),
        _ => 0.,
    } / (v.ticks.get() / v.clock_rate.get());

    let (v2f, v_per_a, kapton1) = (v.v2f.get(), v.VperA.get(), v.kapton1.get());
    let (x_he, x_n2, x_ar) = (v.xHe.get(), v.xN2.get(), v.xAr.get());
    let (x_air, x_co2) = (v.xAir.get(), v.xCO2.get());
    let gas = |work, id| photon(cps, work, v2f, v_per_a, aln, dln1, kapton1, id, energy);
    let mut flux = 0.;
    flux += x_he * gas(29.6, _HELIUM_);
    flux += x_n2 * gas(36.3, _NITROGEN_);
    flux += x_ar * gas(24.4, _ARGON_);
    flux += x_air * gas(35.36, _AIR_);
    flux += x_co2 * gas(35.36, _CO2_);

    let air_abs = (-d_air * absorb(_AIR_, energy)).exp();
    let he_abs = (-d_he * absorb(_HELIUM_, energy)).exp();
    let kap_abs = (-(v.kapton2.get() + kapton1) * CM_PER_INCH * absorb(_KAPTON_, energy)).exp();
    let be_abs = (-d_be * absorb(_BERYLLIUM_, energy)).exp();
    let fill_absorb = absorb(_HELIUM_, energy) * x_he
        + absorb(_NITROGEN_, energy) * x_n2
        + absorb(_ARGON_, energy) * x_ar
        + absorb(_AIR_, energy) * x_air
        + absorb(_CO2_, energy) * x_co2;

    let ion_abs = (-(dln1 + dln2 + aln) * fill_absorb).exp();
    let detector = flux * air_abs * he_abs * kap_abs * be_abs * ion_abs * v.dEff.get();
    let front_abs = (-CM_PER_INCH * kapton1 * absorb(_KAPTON_, energy) - dln1 * fill_absorb).exp();
    v.flux.set(flux);
    v.ionAbs.set(ion_abs);
    v.detector.set(detector);
    v.ionPhotons
        .set((1. - (-aln * fill_absorb).exp()) * front_abs * flux);
}

const A_HE: [f64; 4] = [6.06488, -3.29055, -0.107256, 0.0144465];
const A_N: [f64; 4] = [11.2765, -2.65400, -0.200445, 0.0200765];
const A_AR1: [f64; 4] = [13.9491, -1.82276, -0.328827, 0.0274382];
const A_AR2: [f64; 4] = [12.2960, -2.63279, -0.073660, 0.0];
const A_O: [f64; 4] = [11.7130, -2.57229, -0.205893, 0.0199244];
const A_C: [f64; 4] = [10.6879, -2.71400, -0.200530, 0.0207248];
const EDGE_AR: f64 = 3.202;

/// `photon`: photons/sec just upstream of the chamber's first Kapton window
/// that give `cps` in a chamber filled with gas `gas_id`.
#[allow(clippy::too_many_arguments)]
fn photon(
    cps: f64,
    work: f64,
    v_to_f: f64,
    volt_per_a: f64,
    aln: f64,
    dead_l: f64,
    kapton: f64,
    gas_id: i32,
    energy: f64,
) -> f64 {
    const CONV_HE: f64 = 6.647;
    const RHO_HE: f64 = 0.0001785;
    const CONV_N: f64 = 23.26;
    const RHO_N: f64 = 0.00125;
    const CONV_AR: f64 = 66.32;
    const RHO_AR: f64 = 0.001784;

    // Io.st tests `E1 > edgeAr`, log(energy) against an edge in keV, which
    // takes the below-edge fit up to 24.6 keV. absAr and absArPhoto test
    // `energy>edge`; so does this.
    let ar = || {
        if energy > EDGE_AR {
            fit(A_AR1, energy)
        } else {
            fit(A_AR2, energy)
        }
    };
    let photo = match gas_id {
        _HELIUM_ => fit(A_HE, energy) * RHO_HE / CONV_HE,
        _NITROGEN_ => fit(A_N, energy) * RHO_N / CONV_N,
        _ARGON_ => ar() * RHO_AR / CONV_AR,
        _AIR_ => {
            fit(A_N, energy) * 0.000922 / CONV_N
                + fit(A_O, energy) * 0.000266 / CONV_N
                + ar() * 1.66E-5 / CONV_AR
        }
        _CO2_ => fit(A_C, energy) * 0.0005396 / CONV_N + fit(A_O, energy) * 0.0014374 / CONV_N,
        _ => 0.,
    };

    let part1 = cps * work / (1.602E-19 * v_to_f * volt_per_a * energy * 1000.);
    let part2 = 1. - (-photo * aln).exp();
    let part3 = (absorb(gas_id, energy) * dead_l).exp()
        * (absorb(_KAPTON_, energy) * kapton * CM_PER_INCH).exp();
    part1 * part3 / part2
}

/// `absorb`: the linear absorption coefficient (1/cm) of absorber `id`.
fn absorb(id: i32, energy: f64) -> f64 {
    match id {
        _HELIUM_ => abs_he(energy) * 0.0001785,
        _BERYLLIUM_ => abs_be(energy) * 1.848,
        _NITROGEN_ => abs_n(energy) * 0.00125,
        // 79% N2, 20% O2 and 1% Ar by volume.
        _AIR_ => abs_n(energy) * 0.000922 + abs_o(energy) * 0.000266 + abs_ar(energy) * 1.66E-5,
        _ARGON_ => abs_ar(energy) * 0.001784,
        // C22 H10 O5 N2, 1.42 g/cm^3.
        _KAPTON_ => {
            abs_c(energy) * 0.981
                + abs_h(energy) * 0.037
                + abs_o(energy) * 0.297
                + abs_n(energy) * 0.105
        }
        _CO2_ => abs_c(energy) * 0.0005396 + abs_o(energy) * 0.0014374,
        _ => 0.,
    }
}

/// Photo-electric, coherent and Compton fits over `conv`: a mass absorption
/// coefficient (cm^2/g).
fn mass_abs(photo: f64, b: [f64; 4], c: [f64; 4], conv: f64, energy: f64) -> f64 {
    (photo + fit(b, energy) + fit(c, energy)) / conv
}

/// `absH`.
fn abs_h(energy: f64) -> f64 {
    let a = [2.44964E+00, -3.34953E+00, -4.71370E-02, 7.09962E-03];
    let b = [-1.19075E-01, -9.37086E-01, -2.00538E-01, 1.06587E-02];
    let c = [-2.15772E+00, 1.32685E+00, -3.05620E-01, 1.85025E-02];
    mass_abs(fit(a, energy), b, c, 1.674, energy)
}

/// `absHe`.
fn abs_he(energy: f64) -> f64 {
    let a = [6.06488E+00, -3.29055E+00, -1.07256E-01, 1.44465E-02];
    let b = [1.04768E+00, -8.51805E-02, -4.03527E-01, 2.69398E-02];
    let c = [-2.56357E+00, 2.02536E+00, -4.48710E-01, 2.79691E-02];
    mass_abs(fit(a, energy), b, c, 6.647, energy)
}

/// `absBe`.
fn abs_be(energy: f64) -> f64 {
    let a = [9.04511E+00, -2.83487E+00, -2.10021E-01, 2.29526E-02];
    let b = [2.00860E+00, -4.61920E-02, -3.37018E-01, 1.86939E-02];
    let c = [-6.90079E-01, 9.46448E-01, -1.71142E-01, 6.51413E-03];
    mass_abs(fit(a, energy), b, c, 14.96, energy)
}

/// `absC`.
fn abs_c(energy: f64) -> f64 {
    let a = [1.06879E+01, -2.71400E+00, -2.00530E-01, 2.07248E-02];
    let b = [3.10861E+00, -2.60580E-01, -2.71974E-01, 1.35181E-02];
    let c = [-9.82878E-01, 1.46693E+00, -2.93743E-01, 1.56005E-02];
    mass_abs(fit(a, energy), b, c, 19.94, energy)
}

/// `absN`.
fn abs_n(energy: f64) -> f64 {
    let a = [1.12765E+01, -2.65400E+00, -2.00445E-01, 2.00765E-02];
    let b = [3.47760E+00, -2.15762E-01, -2.88874E-01, 1.51312E-02];
    let c = [-1.23693E+00, 1.74510E+00, -3.54660E-01, 1.98705E-02];
    mass_abs(fit(a, energy), b, c, 23.26, energy)
}

/// `absO`.
fn abs_o(energy: f64) -> f64 {
    let a = [1.17130E+01, -2.57229E+00, -2.05893E-01, 1.99244E-02];
    let b = [3.77239E+00, -1.48539E-01, -3.07124E-01, 1.67303E-02];
    let c = [-1.73679E+00, 2.17686E+00, -4.49050E-01, 2.64733E-02];
    mass_abs(fit(a, energy), b, c, 26.57, energy)
}

/// `absAr`.
fn abs_ar(energy: f64) -> f64 {
    let a1 = [1.39491E+01, -1.82276E+00, -3.28827E-01, 2.74382E-02];
    let a2 = [1.22960E+01, -2.63279E+00, -7.36600E-02, 0.];
    let b = [5.21079E+00, 1.35618E-01, -3.47214E-01, 1.84333E-02];
    let c = [-6.82105E-01, 1.74279E+00, -3.17646E-01, 1.56467E-02];
    let photo = if energy > EDGE_AR {
        fit(a1, energy)
    } else {
        fit(a2, energy)
    };
    mass_abs(photo, b, c, 66.32, energy)
}

/// `absArPhoto`: the photo-electric part of Argon's linear absorption
/// coefficient (1/cm).
#[allow(non_snake_case)]
fn absArPhoto(energy: f64) -> f64 {
    let photo = if energy > EDGE_AR {
        fit(A_AR1, energy)
    } else {
        fit(A_AR2, energy)
    };
    photo / 66.32 * 0.001784
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use epics_seq_rs::{ProgOptions, VarModel};

    use super::*;

    /// `EvalFlux` works on the program's one static block.
    static BLOCK: Mutex<()> = Mutex::new(());

    /// Io.st:1, compiled with snc's defaults.
    #[test]
    fn the_program_is_the_st_one() {
        assert_eq!(Io.validate(), Ok(()));
        assert_eq!(
            Io.params,
            "P=7idd:Io:,MONO=7id:HHLM:energy_get,VSC=7idd:scaler1"
        );
        assert_eq!(
            Io.options,
            ProgOptions::new(ProgOptions::CONN | ProgOptions::NEWEF)
        );
        assert!(matches!(Io.vars, VarModel::Static(_)));
    }

    #[test]
    fn test_absorb_helium() {
        let mu = absorb(_HELIUM_, 10.0);
        assert!(mu > 0.0, "He absorption at 10 keV should be positive: {mu}");
        assert!(mu < 1.0, "He absorption at 10 keV should be small: {mu}");
    }

    #[test]
    fn test_absorb_nitrogen() {
        let mu = absorb(_NITROGEN_, 10.0);
        assert!(mu > 0.0, "N2 absorption at 10 keV should be positive: {mu}");
    }

    #[test]
    fn test_absorb_argon() {
        let mu = absorb(_ARGON_, 10.0);
        assert!(mu > 0.0);
        assert!(mu > absorb(_HELIUM_, 10.0));
    }

    #[test]
    fn test_absorb_kapton() {
        assert!(absorb(_KAPTON_, 10.0) > 0.0);
    }

    #[test]
    fn test_absorb_air() {
        assert!(absorb(_AIR_, 10.0) > 0.0);
    }

    #[test]
    fn test_absorb_co2() {
        assert!(absorb(_CO2_, 10.0) > 0.0);
    }

    #[test]
    fn test_absorb_beryllium() {
        let mu = absorb(_BERYLLIUM_, 10.0);
        assert!(mu > 0.0);
        assert!(mu > absorb(_HELIUM_, 10.0));
    }

    /// An id outside the switch falls out of it: `return 0`.
    #[test]
    fn absorb_of_an_unknown_absorber_is_zero() {
        assert_eq!(absorb(0, 10.0), 0.0);
        assert_eq!(absorb(8, 10.0), 0.0);
    }

    #[test]
    fn test_abs_ar_photo() {
        assert!(absArPhoto(10.0) > 0.0);
    }

    /// The `ArPcntr` clause's detector efficiency (Io.st:258-259) at 10 keV.
    #[test]
    fn test_ar_pcntr_efficiency() {
        let eff = (1. - (-(4. * CM_PER_INCH) * absArPhoto(10.0)).exp())
            * (-(0.005 * CM_PER_INCH) * absorb(_BERYLLIUM_, 10.0)).exp();
        assert!(eff > 0.0 && eff <= 1.0, "Efficiency = {eff}");
    }

    /// The inputs `UpdateDisplay` reads, the hand port's defaults: 60 mm
    /// active, 17.5 mm dead each side, 1 mil Kapton, 1 atm of air, scaler
    /// channel 2, 10 MHz clock.
    fn chamber(ic_channel: i16, counts: [f64; 14], ticks: f64) {
        let v = seqg::seqg_static_vars();
        v.icChannel.set(ic_channel);
        for (s, n) in [
            &v.s2, &v.s3, &v.s4, &v.s5, &v.s6, &v.s7, &v.s8, &v.s9, &v.s10, &v.s11, &v.s12, &v.s13,
            &v.s14, &v.s15,
        ]
        .into_iter()
        .zip(counts)
        {
            s.set(n);
        }
        v.ticks.set(ticks);
        v.clock_rate.set(1.0e7);
        v.VperA.set(1.0e8);
        v.v2f.set(1.0e5);
        v.xAir.set(1.0);
        for x in [&v.xN2, &v.xAr, &v.xHe, &v.xCO2] {
            x.set(0.0);
        }
        v.kapton1.set(0.001);
        v.kapton2.set(0.001);
        v.dEff.set(1.0);
    }

    /// `EvalFlux(activeLen,deadFront,deadRear,HePath,airPath,Be,E_using)`.
    fn eval(energy: f64) -> [f64; 4] {
        EvalFlux(60.0, 17.5, 17.5, 0.0, 0.0, 0.0, energy);
        let v = seqg::seqg_static_vars();
        [
            v.flux.get(),
            v.ionPhotons.get(),
            v.ionAbs.get(),
            v.detector.get(),
        ]
    }

    #[test]
    fn test_eval_flux_zero_counts() {
        let _block = BLOCK.lock().unwrap();
        chamber(2, [0.0; 14], 1e7);
        let [flux, _, _, detector] = eval(10.0);
        assert_eq!(flux, 0.0);
        assert_eq!(detector, 0.0);
    }

    /// 1e6 counts in 1 s at 10 keV.
    #[test]
    fn test_eval_flux_with_counts() {
        let _block = BLOCK.lock().unwrap();
        chamber(2, [1.0e6; 14], 1.0e7);
        let [flux, ion_photons, ion_abs, detector] = eval(10.0);
        assert!(flux > 0.0, "flux = {flux}");
        assert!(detector > 0.0, "detector = {detector}");
        assert!(ion_photons > 0.0 && ion_photons < flux);
        assert!(ion_abs > 0.0 && ion_abs <= 1.0);
    }

    /// C has no energy guard: `log(0)` is -inf, and every result is NaN.
    #[test]
    fn test_eval_flux_zero_energy() {
        let _block = BLOCK.lock().unwrap();
        chamber(2, [1.0e6; 14], 1.0e7);
        assert!(eval(0.0).iter().all(|r| r.is_nan()));
    }

    /// `cps /= (ticks/clock_rate)` with no guard: zero ticks make the rate,
    /// and every result built on it, NaN (the gases with no fraction add
    /// 0 * inf). `ionAbs` is the gas column's transmission alone.
    #[test]
    fn test_eval_flux_zero_ticks() {
        let _block = BLOCK.lock().unwrap();
        chamber(2, [1.0e6; 14], 0.0);
        let [flux, ion_photons, ion_abs, detector] = eval(10.0);
        assert!(flux.is_nan() && ion_photons.is_nan() && detector.is_nan());
        assert!(ion_abs > 0.0 && ion_abs <= 1.0, "ion_abs = {ion_abs}");
    }

    /// Channels 2-15 select s2..s15 (Io.st:409-423); any other is the
    /// switch default, `cps = 0` (Io.st:424-426).
    #[test]
    fn test_eval_flux_channel_window_boundaries() {
        let _block = BLOCK.lock().unwrap();
        let mut counts15 = [0.0; 14];
        counts15[13] = 1e6;
        let mut counts2 = [0.0; 14];
        counts2[0] = 1e6;
        let flux = |channel, counts| {
            chamber(channel, counts, 1e7);
            eval(10.0)[0]
        };
        assert!(flux(15, counts15) > 0.0, "channel 15 should use s15");
        assert_eq!(flux(16, counts15), 0.0, "channel 16 is out of range");
        assert_eq!(flux(0, counts2), 0.0, "channel 0 must not borrow s2");
        assert_eq!(flux(1, counts2), 0.0, "channel 1 must not borrow s2");
        assert!(flux(2, counts2) > 0.0, "channel 2 should use s2");
    }

    #[test]
    fn test_photon_gas_comparison() {
        let (cps, v2f, vpa, aln, dln, kap, energy) = (1.0e6, 1.0e5, 1.0e8, 6.0, 1.75, 0.001, 10.0);
        for (work, id) in [(29.6, _HELIUM_), (36.3, _NITROGEN_), (24.4, _ARGON_)] {
            assert!(photon(cps, work, v2f, vpa, aln, dln, kap, id, energy) > 0.0);
        }
    }

    /// Argon's photo-electric fit switches at the K edge in keV, in
    /// `photon` as in absAr; Io.st's `E1 > edgeAr` would switch at
    /// e^3.202 = 24.6 keV.
    #[test]
    fn photon_switches_argon_fits_at_the_k_edge_in_kev() {
        let ar = |energy| photon(1.0e6, 24.4, 1.0e5, 1.0e8, 6.0, 1.75, 0.001, _ARGON_, energy);
        let photo_part2 =
            |a: [f64; 4], energy: f64| 1. - (-(fit(a, energy) * 0.001784 / 66.32) * 6.0).exp();
        let energy: f64 = 10.0;
        let part1 = 1.0e6 * 24.4 / (1.602E-19 * 1.0e5 * 1.0e8 * energy * 1000.);
        let part3 = (absorb(_ARGON_, energy) * 1.75).exp()
            * (absorb(_KAPTON_, energy) * 0.001 * CM_PER_INCH).exp();
        assert_eq!(ar(energy), part1 * part3 / photo_part2(A_AR1, energy));
    }

    #[test]
    fn test_absorb_energy_dependence() {
        let mu_5 = absorb(_AIR_, 5.0);
        let mu_20 = absorb(_AIR_, 20.0);
        assert!(
            mu_5 > mu_20,
            "Air absorption should decrease with energy: {mu_5} vs {mu_20}"
        );
    }

    #[test]
    fn test_argon_k_edge() {
        let mu_below = absorb(_ARGON_, 3.0);
        let mu_above = absorb(_ARGON_, 3.5);
        assert!(
            mu_above > mu_below,
            "Argon absorption should jump at K-edge: below={mu_below}, above={mu_above}"
        );
    }
}
