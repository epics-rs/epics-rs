//! XRT beamline simulation: Undulator → DCM Si(111) → HFM → VFM → Screen.
//!
//! Builds a beamline from current motor positions, runs ray tracing,
//! and returns the screen capture (2D intensity) plus beam statistics.

use xrt_core::consts::CH;
use xrt_materials::crystal::CrystalGeometry;
use xrt_materials::crystal_variants::CrystalSi;
use xrt_materials::data::ScatteringTable;
use xrt_materials::material::{Material, MaterialKind};
#[cfg(test)]
use xrt_oes::beamline::Beamline;
use xrt_oes::beamline::{BeamlineOutput, OeParamsBuilder};
use xrt_oes::crystal_oe::CrystalOpticalElement;
use xrt_oes::material_oe::MaterialOpticalElement;
use xrt_oes::screen::{Screen, ScreenCapture};
use xrt_oes::surfaces::bent_flat::BentFlatSurface;
use xrt_oes::surfaces::flat::FlatSurface;
use xrt_sources::distributions::{EnergyDist, SpatialDist};
use xrt_sources::geometric::GeometricSource;

/// Beamline geometry constants (distances in mm).
pub struct BeamlineGeometry {
    /// Source to DCM distance `mm`
    pub source_to_dcm: f64,
    /// DCM crystal gap `mm`
    pub dcm_gap: f64,
    /// DCM to HFM distance `mm`
    pub dcm_to_hfm: f64,
    /// HFM to VFM distance `mm`
    pub hfm_to_vfm: f64,
    /// VFM to sample distance `mm`
    pub vfm_to_sample: f64,
}

impl Default for BeamlineGeometry {
    fn default() -> Self {
        Self {
            source_to_dcm: 25_000.0, // 25 m
            dcm_gap: 15.0,           // 15 mm between crystals
            dcm_to_hfm: 2_000.0,     // 2 m
            hfm_to_vfm: 3_000.0,     // 3 m
            vfm_to_sample: 3_000.0,  // 3 m
        }
    }
}

/// Undulator parameters for energy calculation.
pub struct UndulatorConfig {
    /// Electron beam energy `GeV`
    pub electron_energy: f64,
    /// Undulator period `mm`
    pub period: f64,
    /// Maximum K parameter (at minimum gap)
    pub k_max: f64,
    /// Minimum gap `mm`
    pub gap_min: f64,
    /// Gap coefficient for exponential decay
    pub gap_coeff: f64,
}

impl Default for UndulatorConfig {
    fn default() -> Self {
        Self {
            electron_energy: 3.0, // 3 GeV
            period: 10.0,         // 10 mm
            k_max: 2.5,
            gap_min: 5.0, // 5 mm
            gap_coeff: std::f64::consts::PI,
        }
    }
}

impl UndulatorConfig {
    /// Calculate K parameter from gap.
    ///
    /// K = K_max * exp(-π * gap / period)  (simplified Halbach model)
    pub fn k_from_gap(&self, gap: f64) -> f64 {
        self.k_max * (-self.gap_coeff * gap / self.period).exp()
    }

    /// Calculate fundamental energy `eV` from gap `mm`.
    ///
    /// E₁(keV) = 0.9496 * E_e²(GeV) / (λ_u(cm) * (1 + K²/2))
    /// E₁(eV)  = 9496 * E_e²(GeV) / (λ_u(mm) * (1 + K²/2))
    pub fn energy_from_gap(&self, gap: f64) -> f64 {
        let k = self.k_from_gap(gap);
        9496.0 * self.electron_energy * self.electron_energy / (self.period * (1.0 + k * k / 2.0))
    }
}

/// All motor positions for one simulation step.
#[derive(Debug, Clone)]
pub struct MotorPositions {
    // Undulator
    pub und_gap: f64, // mm
    pub und_x: f64,   // mm
    pub und_z: f64,   // mm

    // DCM
    pub dcm_theta: f64,  // degrees
    pub dcm_theta2: f64, // arcsec offset
    pub dcm_y: f64,      // mm (crystal gap)
    pub dcm_chi1: f64,   // mrad
    pub dcm_chi2: f64,   // mrad
    pub dcm_z: f64,      // mm (translation)

    // HFM (Horizontally Focusing Mirror)
    pub hfm_pitch: f64,   // mrad (grazing angle)
    pub hfm_roll: f64,    // mrad
    pub hfm_yaw: f64,     // mrad
    pub hfm_x: f64,       // mm
    pub hfm_y: f64,       // mm
    pub hfm_z: f64,       // mm
    pub hfm_r_major: f64, // mm (meridional bending radius)
    pub hfm_r_minor: f64, // mm (sagittal radius)

    // VFM (Vertically Focusing Mirror)
    pub vfm_pitch: f64,   // mrad
    pub vfm_roll: f64,    // mrad
    pub vfm_yaw: f64,     // mrad
    pub vfm_x: f64,       // mm
    pub vfm_y: f64,       // mm
    pub vfm_z: f64,       // mm
    pub vfm_r_major: f64, // mm (bending radius)
    pub vfm_r_minor: f64, // mm (sagittal radius)
}

impl Default for MotorPositions {
    fn default() -> Self {
        Self {
            und_gap: 6.1, // 8 keV at period=10mm
            und_x: 0.0,
            und_z: 0.0,
            dcm_theta: 14.31, // Si(111) Bragg angle for 8 keV
            dcm_theta2: 0.0,
            dcm_y: 15.0,
            dcm_chi1: 0.0,
            dcm_chi2: 0.0,
            dcm_z: 0.0,
            hfm_pitch: 3.0,
            hfm_roll: 0.0,
            hfm_yaw: 0.0,
            hfm_x: 0.0,
            hfm_y: 0.0,
            hfm_z: 0.0,
            // Coddington: R = 2*p*q / (sin(α)*(p+q))
            // HFM: p=27m, q=6m, α=3mrad → R=3.27km
            hfm_r_major: 3_272_727.0,
            hfm_r_minor: 1e9, // no sagittal focusing
            vfm_pitch: 3.0,
            vfm_roll: 0.0,
            vfm_yaw: 0.0,
            vfm_x: 0.0,
            vfm_y: 0.0,
            vfm_z: 0.0,
            // VFM: p=30m, q=3m, α=3mrad → R=1.82km
            vfm_r_major: 1_818_182.0,
            vfm_r_minor: 1e9, // no sagittal focusing
        }
    }
}

/// Simulation configuration.
pub struct SimConfig {
    pub nrays: usize,
    pub geometry: BeamlineGeometry,
    pub undulator: UndulatorConfig,
    /// Screen half-width `mm`
    pub screen_dx: f64,
    /// Screen half-height `mm`
    pub screen_dz: f64,
    /// Screen bins in x
    pub screen_nx: usize,
    /// Screen bins in z
    pub screen_nz: usize,
    /// Source divergence σ_x `rad`
    pub source_div_x: f64,
    /// Source divergence σ_z `rad`
    pub source_div_z: f64,
    /// Source size σ_x `mm`
    pub source_size_x: f64,
    /// Source size σ_z `mm`
    pub source_size_z: f64,
    /// Energy bandwidth ΔE/E for the source
    pub energy_bandwidth: f64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            nrays: 50000,
            geometry: BeamlineGeometry::default(),
            undulator: UndulatorConfig::default(),
            screen_dx: 0.2, // ±0.2 mm, 128 bins → 3.1 µm/pixel
            screen_dz: 0.2,
            screen_nx: 128,
            screen_nz: 128,
            source_div_x: 50e-6,    // 50 µrad
            source_div_z: 20e-6,    // 20 µrad
            source_size_x: 0.3,     // 0.3 mm
            source_size_z: 0.02,    // 0.02 mm
            energy_bandwidth: 0.02, // 2% ΔE/E
        }
    }
}

/// Results from one simulation step.
pub struct SimResult {
    pub capture: ScreenCapture,
    pub beamline_output: BeamlineOutput,
    /// Photon energy at source `eV`
    pub source_energy: f64,
    /// DCM selected energy `eV`
    pub dcm_energy: f64,
}

/// Kill non-Good rays so subsequent OEs don't process them.
/// Not used after xrt-rs state==1 filter fix, but kept for reference.
#[allow(dead_code)]
fn kill_lost_rays(beam: &mut xrt_core::beam::Beam) {
    for i in 0..beam.nrays() {
        if beam.state[i] != 1 {
            beam.state[i] = -1;
        }
    }
}

/// Run one simulation step with the given motor positions.
pub fn simulate(config: &SimConfig, motors: &MotorPositions) -> SimResult {
    let geo = &config.geometry;

    // 1. Photon energy
    let source_energy = config.undulator.energy_from_gap(motors.und_gap);
    let theta_rad = motors.dcm_theta.to_radians();
    let si_d = 3.1356;
    let dcm_energy = CH / (2.0 * si_d * theta_rad.sin());

    // 2. Source
    let de = source_energy * config.energy_bandwidth;
    let source = GeometricSource {
        center: [motors.und_x, 0.0, motors.und_z],
        nrays: config.nrays,
        dist_x: SpatialDist::Normal(config.source_size_x),
        dist_z: SpatialDist::Normal(config.source_size_z),
        dist_xprime: SpatialDist::Normal(config.source_div_x),
        dist_zprime: SpatialDist::Normal(config.source_div_z),
        dist_e: EnergyDist::Flat(source_energy - de / 2.0, source_energy + de / 2.0),
        ..Default::default()
    };
    let mut beam = source.shine();

    // 3. DCM geometry
    let two_theta = 2.0 * theta_rad;
    let dcm_path = motors.dcm_y / two_theta.sin();
    let dcm_dy = motors.dcm_y / two_theta.tan();
    let dcm_dz = motors.dcm_y;
    let theta2_rad = theta_rad + motors.dcm_theta2 / 3600.0_f64 * std::f64::consts::PI / 180.0;
    let y_dcm1 = geo.source_to_dcm;
    let y_dcm2 = y_dcm1 + dcm_dy;

    let table = ScatteringTable::ChantlerTotal;
    let si_mirror = Material::new(&["Si"], None, 2.33, MaterialKind::Mirror, None, table);

    // 4. Manual pipeline: each OE → kill lost → drift → next OE
    //    (xrt-rs reflect() processes Over rays too, so we must kill them)

    // Source → DCM crystal 1
    beam.propagate(geo.source_to_dcm);

    let si1 = CrystalSi::new(
        [1, 1, 1],
        297.15,
        CrystalGeometry::BraggReflected,
        1.0,
        None,
        0.0,
        table,
    );
    let si2 = CrystalSi::new(
        [1, 1, 1],
        297.15,
        CrystalGeometry::BraggReflected,
        1.0,
        None,
        0.0,
        table,
    );

    if let (Ok(c1), Ok(c2)) = (si1, si2) {
        let xtal1 = CrystalOpticalElement::new(
            FlatSurface,
            OeParamsBuilder::new()
                .center(0.0, y_dcm1, 0.0)
                .pitch(theta_rad)
                .roll(motors.dcm_chi1 * 1e-3)
                .build(),
            c1.base.clone(),
        );
        xtal1.reflect(&mut beam, &c1);
        let n1 = (0..beam.nrays()).filter(|&i| beam.state[i] == 1).count();
        let n3 = (0..beam.nrays()).filter(|&i| beam.state[i] == 3).count();
        let nm1 = (0..beam.nrays()).filter(|&i| beam.state[i] == -1).count();
        let nm2 = (0..beam.nrays()).filter(|&i| beam.state[i] == -2).count();
        let nm3 = (0..beam.nrays()).filter(|&i| beam.state[i] == -3).count();
        let n0 = (0..beam.nrays()).filter(|&i| beam.state[i] == 0).count();
        let n2 = (0..beam.nrays()).filter(|&i| beam.state[i] == 2).count();
        eprintln!(
            "  DCM1: Good={n1} Over={n3} Out={nm1} Absorbed={nm2} Dead={nm3} state0={n0} state2={n2} total={}",
            beam.nrays()
        );
        // No kill_lost — keep all rays with amplitude weighting

        beam.propagate(dcm_path);

        let xtal2 = CrystalOpticalElement::new(
            FlatSurface,
            OeParamsBuilder::new()
                .center(0.0, y_dcm2, dcm_dz)
                .pitch(theta2_rad)
                .roll(motors.dcm_chi2 * 1e-3)
                .invert_normal()
                .build(),
            c2.base.clone(),
        );
        xtal2.reflect(&mut beam, &c2);
        let _n = (0..beam.nrays()).filter(|&i| beam.state[i] == 1).count();
        eprintln!("  DCM2: good={_n}");
        // No kill_lost — keep all rays with amplitude weighting
    }

    // DCM → HFM
    let y_hfm = y_dcm1 + geo.dcm_to_hfm;
    let drift_dcm_hfm = y_hfm - y_dcm2;
    beam.propagate(drift_dcm_hfm);

    if let Ok(ref mat) = si_mirror {
        let mut p = OeParamsBuilder::new()
            .center(motors.hfm_x, y_hfm, dcm_dz + motors.hfm_y)
            .pitch(motors.hfm_pitch * 1e-3)
            .roll(motors.hfm_roll * 1e-3)
            .yaw(motors.hfm_yaw * 1e-3)
            .build();
        p.position_roll = std::f64::consts::FRAC_PI_2;
        let hfm = MaterialOpticalElement::new(
            BentFlatSurface::new(motors.hfm_r_major, 0.0),
            p,
            mat.clone(),
        );
        hfm.reflect(&mut beam);
        let _n = (0..beam.nrays()).filter(|&i| beam.state[i] == 1).count();
        eprintln!("  HFM:  good={_n}");
        // No kill_lost — keep all rays with amplitude weighting
    }

    // HFM → VFM
    let hfm_defl = 2.0 * motors.hfm_pitch * 1e-3;
    let x_at_vfm = hfm_defl * geo.hfm_to_vfm;
    let y_vfm = y_hfm + geo.hfm_to_vfm;
    beam.propagate(geo.hfm_to_vfm);

    // Auto-position VFM center on actual beam position
    let vfm_center = {
        let g: Vec<usize> = (0..beam.nrays()).filter(|&i| beam.state[i] == 1).collect();
        if !g.is_empty() {
            let n = g.len() as f64;
            [
                g.iter().map(|&i| beam.x[i]).sum::<f64>() / n,
                g.iter().map(|&i| beam.y[i]).sum::<f64>() / n,
                g.iter().map(|&i| beam.z[i]).sum::<f64>() / n,
            ]
        } else {
            [x_at_vfm + motors.vfm_x, y_vfm, dcm_dz + motors.vfm_y]
        }
    };
    if let Ok(ref mat) = si_mirror {
        let mut p = OeParamsBuilder::new()
            .center(vfm_center[0], vfm_center[1], vfm_center[2])
            .pitch(motors.vfm_pitch * 1e-3)
            .roll(motors.vfm_roll * 1e-3)
            .yaw(motors.vfm_yaw * 1e-3)
            .build();
        p.position_roll = std::f64::consts::PI;
        let vfm = MaterialOpticalElement::new(
            BentFlatSurface::new(motors.vfm_r_major, 0.0),
            p,
            mat.clone(),
        );
        vfm.reflect(&mut beam);
        let _n = (0..beam.nrays()).filter(|&i| beam.state[i] == 1).count();
        eprintln!("  VFM:  good={_n}");
        // No kill_lost — keep all rays with amplitude weighting
    }

    // VFM → Sample
    beam.propagate(geo.vfm_to_sample);

    // 5. Screen — auto-center on beam
    let vfm_defl = 2.0 * motors.vfm_pitch * 1e-3;
    let y_sample = y_vfm + geo.vfm_to_sample;
    let x_sample = x_at_vfm + hfm_defl * geo.vfm_to_sample;
    let z_sample = dcm_dz - vfm_defl * geo.vfm_to_sample;

    let screen_center = {
        let good: Vec<usize> = (0..beam.nrays()).filter(|&i| beam.state[i] == 1).collect();
        if good.len() > 10 {
            let n = good.len() as f64;
            let c = [
                good.iter().map(|&i| beam.x[i]).sum::<f64>() / n,
                good.iter().map(|&i| beam.y[i]).sum::<f64>() / n,
                good.iter().map(|&i| beam.z[i]).sum::<f64>() / n,
            ];
            eprintln!(
                "  screen auto: ({:.1},{:.1},{:.1}) n_good={}",
                c[0],
                c[1],
                c[2],
                good.len()
            );
            c
        } else {
            eprintln!("  screen fallback: ({x_sample:.1},{y_sample:.1},{z_sample:.1})");
            [x_sample, y_sample, z_sample]
        }
    };

    let n_good = (0..beam.nrays()).filter(|&i| beam.state[i] == 1).count();

    let screen = Screen::new(
        screen_center,
        config.screen_dx,
        config.screen_dz,
        config.screen_nx,
        config.screen_nz,
    );
    let capture = screen.capture(&beam);

    let beamline_output = BeamlineOutput {
        elements: vec![],
        final_good_count: n_good,
        initial_count: beam.nrays(),
    };

    SimResult {
        capture,
        beamline_output,
        source_energy,
        dcm_energy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beam_stats(beam: &xrt_core::beam::Beam) -> (f64, f64, f64, f64, f64, f64, usize) {
        let good: Vec<usize> = (0..beam.nrays()).filter(|&i| beam.state[i] == 1).collect(); // Good only, not Over/Dead
        if good.is_empty() {
            return (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0);
        }
        let n = good.len() as f64;
        let mx = good.iter().map(|&i| beam.x[i]).sum::<f64>() / n;
        let my = good.iter().map(|&i| beam.y[i]).sum::<f64>() / n;
        let mz = good.iter().map(|&i| beam.z[i]).sum::<f64>() / n;
        let ma = good.iter().map(|&i| beam.a[i]).sum::<f64>() / n;
        let mb = good.iter().map(|&i| beam.b[i]).sum::<f64>() / n;
        let mc = good.iter().map(|&i| beam.c[i]).sum::<f64>() / n;
        (mx, my, mz, ma, mb, mc, good.len())
    }

    #[test]
    fn test_single_mirror() {
        let source = GeometricSource {
            nrays: 2000,
            dist_x: SpatialDist::Normal(0.3),
            dist_z: SpatialDist::Normal(0.02),
            dist_xprime: SpatialDist::Normal(50e-6),
            dist_zprime: SpatialDist::Normal(20e-6),
            dist_e: EnergyDist::Lines(vec![8000.0], None),
            ..Default::default()
        };
        let mut beam = source.shine();
        beam.propagate(25000.0);

        let si = Material::new(
            &["Si"],
            None,
            2.33,
            MaterialKind::Mirror,
            None,
            ScatteringTable::ChantlerTotal,
        )
        .unwrap();

        // Simple flat mirror: pitch=3mrad at y=25000
        let m1 = MaterialOpticalElement::new(
            FlatSurface,
            OeParamsBuilder::new()
                .center(0.0, 25000.0, 0.0)
                .pitch(0.003)
                .build(),
            si,
        );
        let bl = Beamline::new().add_material("M1", m1).drift(5000.0);
        let output = bl.propagate(&mut beam);

        let (x, y, z, a, b, c, n) = beam_stats(&beam);
        eprintln!("Single mirror: n={n} pos=({x:.1},{y:.1},{z:.1}) dir=({a:.6},{b:.6},{c:.6})");
        eprintln!("  expected: y≈30000, z≈-30 (2*3mrad*5000=30mm deflection)");
        for el in &output.elements {
            eprintln!(
                "  {:10} good={} lost={}",
                el.name, el.good_count, el.lost_count
            );
        }
        assert!(n > 100, "should have good rays");
        assert!((y - 30000.0).abs() < 500.0, "y={y} should be near 30000");
    }

    #[test]
    fn test_mirror_with_position_roll() {
        let source = GeometricSource {
            nrays: 2000,
            dist_x: SpatialDist::Normal(0.3),
            dist_z: SpatialDist::Normal(0.02),
            dist_xprime: SpatialDist::Normal(50e-6),
            dist_zprime: SpatialDist::Normal(20e-6),
            dist_e: EnergyDist::Lines(vec![8000.0], None),
            ..Default::default()
        };
        let mut beam = source.shine();
        beam.propagate(25000.0);

        let si = Material::new(
            &["Si"],
            None,
            2.33,
            MaterialKind::Mirror,
            None,
            ScatteringTable::ChantlerTotal,
        )
        .unwrap();

        // Mirror with positionRoll=pi/2: should deflect horizontally
        let mut params = OeParamsBuilder::new()
            .center(0.0, 25000.0, 0.0)
            .pitch(0.003)
            .build();
        params.position_roll = std::f64::consts::FRAC_PI_2;

        let m1 = MaterialOpticalElement::new(FlatSurface, params, si);
        let bl = Beamline::new().add_material("HFM", m1).drift(5000.0);
        let output = bl.propagate(&mut beam);

        let (x, y, z, a, b, c, n) = beam_stats(&beam);
        eprintln!("posRoll=pi/2: n={n} pos=({x:.1},{y:.1},{z:.1}) dir=({a:.6},{b:.6},{c:.6})");
        eprintln!("  expected: y≈30000, x≈30 (horizontal deflection), z≈0");
        for el in &output.elements {
            eprintln!(
                "  {:10} good={} lost={}",
                el.name, el.good_count, el.lost_count
            );
        }
        assert!(n > 100, "should have good rays");
    }

    #[test]
    fn test_mirror_position_roll_pi() {
        // positionRoll=pi should deflect downward (-z)
        let source = GeometricSource {
            nrays: 2000,
            dist_x: SpatialDist::Normal(0.3),
            dist_z: SpatialDist::Normal(0.02),
            dist_xprime: SpatialDist::Normal(50e-6),
            dist_zprime: SpatialDist::Normal(20e-6),
            dist_e: EnergyDist::Lines(vec![8000.0], None),
            ..Default::default()
        };
        let mut beam = source.shine();
        beam.propagate(25000.0);

        let si = Material::new(
            &["Si"],
            None,
            2.33,
            MaterialKind::Mirror,
            None,
            ScatteringTable::ChantlerTotal,
        )
        .unwrap();

        let mut params = OeParamsBuilder::new()
            .center(0.0, 25000.0, 0.0)
            .pitch(0.003)
            .build();
        params.position_roll = std::f64::consts::PI;

        let m1 = MaterialOpticalElement::new(FlatSurface, params, si);
        let bl = Beamline::new().add_material("VFM_test", m1).drift(5000.0);
        let output = bl.propagate(&mut beam);

        let (x, y, z, a, b, c, n) = beam_stats(&beam);
        eprintln!("posRoll=pi: n={n} pos=({x:.1},{y:.1},{z:.1}) dir=({a:.6},{b:.6},{c:.6})");
        eprintln!("  expected: y≈30000, z≈-30 (downward), x≈0");
        for el in &output.elements {
            eprintln!(
                "  {:10} good={} lost={}",
                el.name, el.good_count, el.lost_count
            );
        }
    }

    #[test]
    fn test_hfm_then_vfm() {
        // HFM → VFM with manual pipeline (kill lost rays between OEs)
        let source = GeometricSource {
            nrays: 2000,
            dist_x: SpatialDist::Normal(0.3),
            dist_z: SpatialDist::Normal(0.02),
            dist_xprime: SpatialDist::Normal(50e-6),
            dist_zprime: SpatialDist::Normal(20e-6),
            dist_e: EnergyDist::Lines(vec![8000.0], None),
            ..Default::default()
        };
        let mut beam = source.shine();
        beam.propagate(27000.0);

        let si = Material::new(
            &["Si"],
            None,
            2.33,
            MaterialKind::Mirror,
            None,
            ScatteringTable::ChantlerTotal,
        )
        .unwrap();

        // HFM at y=27000, posRoll=pi/2
        let mut hp = OeParamsBuilder::new()
            .center(0.0, 27000.0, 0.0)
            .pitch(0.003)
            .build();
        hp.position_roll = std::f64::consts::FRAC_PI_2;
        let hfm =
            MaterialOpticalElement::new(BentFlatSurface::new(3_272_727.0, 0.0), hp, si.clone());

        hfm.reflect(&mut beam);
        // Kill non-Good rays so VFM doesn't process them
        for i in 0..beam.nrays() {
            if beam.state[i] != 1 {
                beam.state[i] = -1;
            }
        }
        let (x, y, z, a, b, c, n) = beam_stats(&beam);
        eprintln!("After HFM: n={n} pos=({x:.1},{y:.1},{z:.1}) dir=({a:.6},{b:.6},{c:.6})");

        beam.propagate(3000.0);

        // VFM at y=30000, posRoll=pi
        let mut vp = OeParamsBuilder::new()
            .center(18.0, 30000.0, 0.0)
            .pitch(0.003)
            .build();
        vp.position_roll = std::f64::consts::PI;
        let vfm = MaterialOpticalElement::new(BentFlatSurface::new(1_818_182.0, 0.0), vp, si);

        vfm.reflect(&mut beam);
        for i in 0..beam.nrays() {
            if beam.state[i] != 1 {
                beam.state[i] = -1;
            }
        }

        beam.propagate(3000.0);

        let (x, y, z, a, b, c, n) = beam_stats(&beam);
        eprintln!("After VFM+drift: n={n} pos=({x:.1},{y:.1},{z:.1}) dir=({a:.6},{b:.6},{c:.6})");
        eprintln!("  expected: y≈33000, x≈36, z≈-18");
        assert!(n > 100, "should have good rays, got {n}");
        assert!((y - 33000.0).abs() < 500.0, "y={y} should be near 33000");
    }

    #[test]
    fn test_undulator_energy() {
        let und = UndulatorConfig::default();
        // At 15mm gap with default params
        let e = und.energy_from_gap(15.0);
        assert!(
            e > 1000.0 && e < 50000.0,
            "energy={e} eV should be reasonable"
        );
    }

    #[test]
    fn test_simulate_default() {
        let config = SimConfig {
            nrays: 2000,
            ..Default::default()
        };
        let motors = MotorPositions::default();
        let result = simulate(&config, &motors);

        eprintln!("source_energy = {:.1} eV", result.source_energy);
        eprintln!("dcm_energy = {:.1} eV", result.dcm_energy);
        eprintln!(
            "efficiency = {:.2}%",
            result.beamline_output.efficiency() * 100.0
        );
        eprintln!("n_captured = {}", result.capture.n_captured);
        eprintln!("n_missed = {}", result.capture.n_missed);
        eprintln!("total_intensity = {:.2}", result.capture.total_intensity());
        let [cx, cz] = result.capture.centroid();
        eprintln!("centroid = ({:.4}, {:.4}) mm", cx, cz);
        eprintln!(
            "screen center = ({}, {}, {})",
            motors.hfm_pitch * 1e-3 * 2.0 * 6000.0,
            0.0,
            motors.dcm_y - motors.vfm_pitch * 1e-3 * 2.0 * 3000.0
        );

        eprintln!("screen_dx = {} mm", config.screen_dx);

        assert!(result.source_energy > 0.0);
        assert!(result.dcm_energy > 0.0);
        assert!(result.capture.n_captured > 0, "no rays captured on screen!");
    }
}
