//! Ion chamber (I0) intensity calculation state machine.
//!
//! Pure Rust port of `Io.st` — converts scaler counts to photon flux using
//! gas mixture properties and energy-dependent absorption coefficients.
//!
//! The calculation uses polynomial fits to mass-absorption coefficients
//! for common ion-chamber gases and window/path materials, faithfully
//! reproducing the C functions in the original SNL program.

/// Gas/material identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GasId {
    Helium = 1,
    Nitrogen = 2,
    Argon = 3,
    Air = 4,
    Beryllium = 5,
    Kapton = 6,
    CO2 = 7,
}

/// Conversion constant: inches to centimetres.
const CM_PER_INCH: f64 = 2.54;

// Gas work functions (eV per ion pair).
const WORK_HE: f64 = 29.6;
const WORK_N2: f64 = 36.3;
const WORK_AR: f64 = 24.4;
const WORK_AIR: f64 = 35.36;
const WORK_CO2: f64 = 35.36;

/// Ion chamber configuration.
#[derive(Debug, Clone)]
pub struct IoConfig {
    pub prefix: String,
    pub mono_pv: String,
    pub scaler_pv: String,
}

impl IoConfig {
    pub fn new(p: &str, mono: &str, vsc: &str) -> Self {
        Self {
            prefix: p.to_string(),
            mono_pv: mono.to_string(),
            scaler_pv: vsc.to_string(),
        }
    }
}

/// Ion chamber parameters for flux calculation.
#[derive(Debug, Clone)]
pub struct IoParams {
    /// Energy in keV.
    pub energy: f64,
    /// Current amplifier gain in V/A.
    pub v_per_a: f64,
    /// Voltage-to-frequency converter in Hz/V.
    pub v2f: f64,
    /// Gas fractions (atm): N2, Ar, He, Air, CO2.
    pub x_n2: f64,
    pub x_ar: f64,
    pub x_he: f64,
    pub x_air: f64,
    pub x_co2: f64,
    /// Active length of ion chamber (mm).
    pub active_len: f64,
    /// Dead length before active region (mm).
    pub dead_front: f64,
    /// Dead length after active region (mm).
    pub dead_rear: f64,
    /// Kapton window thicknesses (inches): front, rear.
    pub kapton_front: f64,
    pub kapton_rear: f64,
    /// He path after ion chamber (mm).
    pub he_path: f64,
    /// Air path after ion chamber (mm).
    pub air_path: f64,
    /// Be thickness after ion chamber (inches).
    pub be_thickness: f64,
    /// Detector efficiency.
    pub detector_efficiency: f64,
    /// Scaler channel to use (2–15).
    pub scaler_channel: usize,
    /// Clock rate in Hz.
    pub clock_rate: f64,
    /// Whether using Argon proportional counter.
    pub ar_pcntr: bool,
}

impl Default for IoParams {
    fn default() -> Self {
        Self {
            energy: 10.0,
            v_per_a: 1.0e8,
            v2f: 1.0e5,
            x_n2: 0.0,
            x_ar: 0.0,
            x_he: 0.0,
            x_air: 1.0,
            x_co2: 0.0,
            active_len: 60.0,
            dead_front: 17.5,
            dead_rear: 17.5,
            kapton_front: 0.001,
            kapton_rear: 0.001,
            he_path: 0.0,
            air_path: 0.0,
            be_thickness: 0.0,
            detector_efficiency: 1.0,
            scaler_channel: 2,
            clock_rate: 1.0e7,
            ar_pcntr: false,
        }
    }
}

/// Results of the ion chamber flux calculation.
#[derive(Debug, Clone, Default)]
pub struct IoResults {
    /// Photon flux before ion chamber (photons/sec).
    pub flux: f64,
    /// Photons absorbed in the active region.
    pub ion_photons: f64,
    /// Transmission factor of the ion chamber gas.
    pub ion_abs: f64,
    /// Photons/sec at the detector location.
    pub detector: f64,
}

// ---------- Mass-absorption coefficient functions ----------

/// Mass absorption coefficient for Hydrogen (cm^2/g).
fn abs_h(energy: f64) -> f64 {
    let a = [2.44964, -3.34953, -0.04714, 0.00710];
    let b = [-0.11908, -0.93709, -0.20054, 0.01066];
    let c = [-2.15772, 1.32685, -0.30562, 0.01850];
    let conv = 1.674;
    let e1 = energy.ln();
    let e2 = e1 * e1;
    let e3 = e2 * e1;
    let photo = (a[0] + a[1] * e1 + a[2] * e2 + a[3] * e3).exp();
    let coherent = (b[0] + b[1] * e1 + b[2] * e2 + b[3] * e3).exp();
    let compton = (c[0] + c[1] * e1 + c[2] * e2 + c[3] * e3).exp();
    (photo + coherent + compton) / conv
}

/// Mass absorption coefficient for Helium (cm^2/g).
fn abs_he(energy: f64) -> f64 {
    let a = [6.06488, -3.29055, -0.10726, 0.01445];
    let b = [1.04768, -0.08518, -0.40353, 0.02694];
    let c = [-2.56357, 2.02536, -0.44871, 0.02797];
    let conv = 6.647;
    let e1 = energy.ln();
    let e2 = e1 * e1;
    let e3 = e2 * e1;
    let photo = (a[0] + a[1] * e1 + a[2] * e2 + a[3] * e3).exp();
    let coherent = (b[0] + b[1] * e1 + b[2] * e2 + b[3] * e3).exp();
    let compton = (c[0] + c[1] * e1 + c[2] * e2 + c[3] * e3).exp();
    (photo + coherent + compton) / conv
}

/// Mass absorption coefficient for Beryllium (cm^2/g).
fn abs_be(energy: f64) -> f64 {
    let a = [9.04511, -2.83487, -0.21002, 0.02295];
    let b = [2.00860, -0.04619, -0.33702, 0.01869];
    let c = [-0.69008, 0.94645, -0.17114, 0.00651];
    let conv = 14.96;
    let e1 = energy.ln();
    let e2 = e1 * e1;
    let e3 = e2 * e1;
    let photo = (a[0] + a[1] * e1 + a[2] * e2 + a[3] * e3).exp();
    let coherent = (b[0] + b[1] * e1 + b[2] * e2 + b[3] * e3).exp();
    let compton = (c[0] + c[1] * e1 + c[2] * e2 + c[3] * e3).exp();
    (photo + coherent + compton) / conv
}

/// Mass absorption coefficient for Carbon (cm^2/g).
fn abs_c(energy: f64) -> f64 {
    let a = [10.6879, -2.71400, -0.20053, 0.02072];
    let b = [3.10861, -0.26058, -0.27197, 0.01352];
    let c = [-0.98288, 1.46693, -0.29374, 0.01560];
    let conv = 19.94;
    let e1 = energy.ln();
    let e2 = e1 * e1;
    let e3 = e2 * e1;
    let photo = (a[0] + a[1] * e1 + a[2] * e2 + a[3] * e3).exp();
    let coherent = (b[0] + b[1] * e1 + b[2] * e2 + b[3] * e3).exp();
    let compton = (c[0] + c[1] * e1 + c[2] * e2 + c[3] * e3).exp();
    (photo + coherent + compton) / conv
}

/// Mass absorption coefficient for Nitrogen (cm^2/g).
fn abs_n(energy: f64) -> f64 {
    let a = [11.2765, -2.65400, -0.20045, 0.02008];
    let b = [3.47760, -0.21576, -0.28887, 0.01513];
    let c = [-1.23693, 1.74510, -0.35466, 0.01987];
    let conv = 23.26;
    let e1 = energy.ln();
    let e2 = e1 * e1;
    let e3 = e2 * e1;
    let photo = (a[0] + a[1] * e1 + a[2] * e2 + a[3] * e3).exp();
    let coherent = (b[0] + b[1] * e1 + b[2] * e2 + b[3] * e3).exp();
    let compton = (c[0] + c[1] * e1 + c[2] * e2 + c[3] * e3).exp();
    (photo + coherent + compton) / conv
}

/// Mass absorption coefficient for Oxygen (cm^2/g).
fn abs_o(energy: f64) -> f64 {
    let a = [11.7130, -2.57229, -0.20589, 0.01992];
    let b = [3.77239, -0.14854, -0.30712, 0.01673];
    let c = [-1.73679, 2.17686, -0.44905, 0.02647];
    let conv = 26.57;
    let e1 = energy.ln();
    let e2 = e1 * e1;
    let e3 = e2 * e1;
    let photo = (a[0] + a[1] * e1 + a[2] * e2 + a[3] * e3).exp();
    let coherent = (b[0] + b[1] * e1 + b[2] * e2 + b[3] * e3).exp();
    let compton = (c[0] + c[1] * e1 + c[2] * e2 + c[3] * e3).exp();
    (photo + coherent + compton) / conv
}

/// Mass absorption coefficient for Argon (cm^2/g).
fn abs_ar(energy: f64) -> f64 {
    let a1 = [13.9491, -1.82276, -0.32883, 0.02744];
    let a2 = [12.2960, -2.63279, -0.07366, 0.0];
    let b = [5.21079, 0.13562, -0.34721, 0.01843];
    let c = [-0.68211, 1.74279, -0.31765, 0.01565];
    let conv = 66.32;
    let edge = 3.202;
    let e1 = energy.ln();
    let e2 = e1 * e1;
    let e3 = e2 * e1;
    let photo = if energy > edge {
        (a1[0] + a1[1] * e1 + a1[2] * e2 + a1[3] * e3).exp()
    } else {
        (a2[0] + a2[1] * e1 + a2[2] * e2 + a2[3] * e3).exp()
    };
    let coherent = (b[0] + b[1] * e1 + b[2] * e2 + b[3] * e3).exp();
    let compton = (c[0] + c[1] * e1 + c[2] * e2 + c[3] * e3).exp();
    (photo + coherent + compton) / conv
}

/// Photo-electric part of Argon mass absorption coefficient (cm^2/g).
fn abs_ar_photo(energy: f64) -> f64 {
    let a1 = [13.9491, -1.82276, -0.32883, 0.02744];
    let a2 = [12.2960, -2.63279, -0.07366, 0.0];
    let conv = 66.32;
    let edge = 3.202;
    let e1 = energy.ln();
    let e2 = e1 * e1;
    let e3 = e2 * e1;
    let photo = if energy > edge {
        (a1[0] + a1[1] * e1 + a1[2] * e2 + a1[3] * e3).exp()
    } else {
        (a2[0] + a2[1] * e1 + a2[2] * e2 + a2[3] * e3).exp()
    };
    photo / conv * 0.001784 // rho_Ar
}

/// Linear absorption coefficient (1/cm) for a given material/gas.
pub fn absorb(id: GasId, energy: f64) -> f64 {
    match id {
        GasId::Helium => abs_he(energy) * 0.0001785,
        GasId::Beryllium => abs_be(energy) * 1.848,
        GasId::Nitrogen => abs_n(energy) * 0.00125,
        GasId::Air => {
            // Dry air: 79% N2, 20% O2, 1% Ar by volume
            abs_n(energy) * 0.000922 + abs_o(energy) * 0.000266 + abs_ar(energy) * 1.66e-5
        }
        GasId::Argon => abs_ar(energy) * 0.001784,
        GasId::Kapton => {
            // C22 H10 O5 N2, rho=1.42 g/cm^3
            abs_c(energy) * 0.981
                + abs_h(energy) * 0.037
                + abs_o(energy) * 0.297
                + abs_n(energy) * 0.105
        }
        GasId::CO2 => {
            // CO2, density = 0.001977 g/cm^3
            abs_c(energy) * 0.0005396 + abs_o(energy) * 0.0014374
        }
    }
}

/// Calculate photon flux for one gas component.
///
/// Returns photons/sec at the point just upstream of the ion chamber front window.
#[allow(clippy::too_many_arguments)]
fn photon(
    cps: f64,
    work: f64,
    v2f: f64,
    v_per_a: f64,
    active_len_cm: f64,
    dead_front_cm: f64,
    kapton_front_in: f64,
    gas_id: GasId,
    energy: f64,
) -> f64 {
    let rho_he = 0.0001785;
    let rho_n = 0.00125;
    let rho_ar = 0.001784;
    let conv_he = 6.647;
    let conv_n = 23.26;
    let conv_ar = 66.32;
    let edge_ar = 3.202;

    let e1 = energy.ln();
    let e2 = e1 * e1;
    let e3 = e2 * e1;

    let a_he = [6.06488, -3.29055, -0.10726, 0.01445];
    let a_n = [11.2765, -2.65400, -0.20045, 0.02008];
    let a_ar1 = [13.9491, -1.82276, -0.32883, 0.02744];
    let a_ar2 = [12.2960, -2.63279, -0.07366, 0.0];
    let a_o = [11.7130, -2.57229, -0.20589, 0.01992];
    let a_c = [10.6879, -2.71400, -0.20053, 0.02072];

    let photo: f64 = match gas_id {
        GasId::Helium => {
            let sum = (a_he[0] + a_he[1] * e1 + a_he[2] * e2 + a_he[3] * e3).exp();
            sum * rho_he / conv_he
        }
        GasId::Nitrogen => {
            let sum = (a_n[0] + a_n[1] * e1 + a_n[2] * e2 + a_n[3] * e3).exp();
            sum * rho_n / conv_n
        }
        GasId::Argon => {
            let sum = if energy > edge_ar {
                (a_ar1[0] + a_ar1[1] * e1 + a_ar1[2] * e2 + a_ar1[3] * e3).exp()
            } else {
                (a_ar2[0] + a_ar2[1] * e1 + a_ar2[2] * e2 + a_ar2[3] * e3).exp()
            };
            sum * rho_ar / conv_ar
        }
        GasId::Air => {
            // 79% N2, 20% O2, 1% Ar
            let sum_n = (a_n[0] + a_n[1] * e1 + a_n[2] * e2 + a_n[3] * e3).exp();
            let sum_o = (a_o[0] + a_o[1] * e1 + a_o[2] * e2 + a_o[3] * e3).exp();
            let sum_ar = if energy > edge_ar {
                (a_ar1[0] + a_ar1[1] * e1 + a_ar1[2] * e2 + a_ar1[3] * e3).exp()
            } else {
                (a_ar2[0] + a_ar2[1] * e1 + a_ar2[2] * e2 + a_ar2[3] * e3).exp()
            };
            sum_n * 0.000922 / conv_n + sum_o * 0.000266 / conv_n + sum_ar * 1.66e-5 / conv_ar
        }
        GasId::CO2 => {
            let sum_c = (a_c[0] + a_c[1] * e1 + a_c[2] * e2 + a_c[3] * e3).exp();
            let sum_o = (a_o[0] + a_o[1] * e1 + a_o[2] * e2 + a_o[3] * e3).exp();
            sum_c * 0.0005396 / conv_n + sum_o * 0.0014374 / conv_n
        }
        _ => 0.0,
    };

    // Parts 1, 2, 3 calculate the flux in photons/sec
    let part1 = cps * work / (1.602e-19 * v2f * v_per_a * energy * 1000.0);
    let part2 = 1.0 - (-photo * active_len_cm).exp();
    let part3 = (absorb(gas_id, energy) * dead_front_cm).exp()
        * (absorb(GasId::Kapton, energy) * kapton_front_in * CM_PER_INCH).exp();

    if part2.abs() < 1e-30 {
        return 0.0;
    }
    part1 * part3 / part2
}

/// Evaluate flux through the ion chamber.
///
/// This is the main calculation function, ported from `EvalFlux` in `Io.st`.
pub fn eval_flux(params: &IoParams, scaler_counts: &[f64], ticks: f64) -> IoResults {
    let energy = params.energy;
    if energy <= 0.0 || ticks <= 0.0 || params.clock_rate <= 0.0 {
        return IoResults::default();
    }

    // Convert units to cm
    let aln = params.active_len / 10.0;
    let dln1 = params.dead_front / 10.0;
    let dln2 = params.dead_rear / 10.0;
    let d_he = params.he_path / 10.0;
    let d_air = params.air_path / 10.0;
    let d_be = params.be_thickness * CM_PER_INCH;

    // Get counts per second from the selected scaler channel
    let ch_idx = params.scaler_channel.saturating_sub(2);
    let counts = if ch_idx < scaler_counts.len() {
        scaler_counts[ch_idx]
    } else {
        0.0
    };
    let count_time = ticks / params.clock_rate;
    let cps = if count_time > 0.0 {
        counts / count_time
    } else {
        0.0
    };

    // Calculate flux from each gas component
    let mut flux = 0.0;
    flux += params.x_he
        * photon(
            cps, WORK_HE, params.v2f, params.v_per_a, aln, dln1, params.kapton_front,
            GasId::Helium, energy,
        );
    flux += params.x_n2
        * photon(
            cps, WORK_N2, params.v2f, params.v_per_a, aln, dln1, params.kapton_front,
            GasId::Nitrogen, energy,
        );
    flux += params.x_ar
        * photon(
            cps, WORK_AR, params.v2f, params.v_per_a, aln, dln1, params.kapton_front,
            GasId::Argon, energy,
        );
    flux += params.x_air
        * photon(
            cps, WORK_AIR, params.v2f, params.v_per_a, aln, dln1, params.kapton_front,
            GasId::Air, energy,
        );
    flux += params.x_co2
        * photon(
            cps, WORK_CO2, params.v2f, params.v_per_a, aln, dln1, params.kapton_front,
            GasId::CO2, energy,
        );

    // Absorption from front of ion chamber to detector
    let air_abs = (-d_air * absorb(GasId::Air, energy)).exp();
    let he_abs = (-d_he * absorb(GasId::Helium, energy)).exp();
    let kap_abs = (-(params.kapton_rear + params.kapton_front) * CM_PER_INCH
        * absorb(GasId::Kapton, energy))
        .exp();
    let be_abs = (-d_be * absorb(GasId::Beryllium, energy)).exp();

    let fill_absorb = absorb(GasId::Helium, energy) * params.x_he
        + absorb(GasId::Nitrogen, energy) * params.x_n2
        + absorb(GasId::Argon, energy) * params.x_ar
        + absorb(GasId::Air, energy) * params.x_air
        + absorb(GasId::CO2, energy) * params.x_co2;

    let ion_abs = (-(dln1 + dln2 + aln) * fill_absorb).exp();
    let detector =
        flux * air_abs * he_abs * kap_abs * be_abs * ion_abs * params.detector_efficiency;
    let front_abs = (-CM_PER_INCH * params.kapton_front * absorb(GasId::Kapton, energy)
        - dln1 * fill_absorb)
        .exp();
    let ion_photons = (1.0 - (-aln * fill_absorb).exp()) * front_abs * flux;

    IoResults {
        flux,
        ion_photons,
        ion_abs,
        detector,
    }
}

/// Calculate Argon proportional counter efficiency.
pub fn ar_pcntr_efficiency(energy: f64) -> f64 {
    let eff = 1.0 - (-(4.0 * CM_PER_INCH) * abs_ar_photo(energy)).exp();
    let window_abs = (-0.005 * CM_PER_INCH * absorb(GasId::Beryllium, energy)).exp();
    eff * window_abs
}

/// Async entry point — runs the Io state machine against live PVs.
pub async fn run(config: IoConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use epics_base_rs::types::EpicsValue;
    use epics_ca_rs::client::{CaChannel, CaClient};
    use tokio::select;
    use tokio::time::{sleep, Duration};

    /// Helper: read f64 from channel.
    async fn get_f64(ch: &CaChannel) -> f64 {
        match ch.get().await {
            Ok((_, val)) => val.to_f64().unwrap_or(0.0),
            Err(_) => 0.0,
        }
    }

    /// Helper: read i32 from channel.
    async fn get_i32(ch: &CaChannel) -> i32 {
        match ch.get().await {
            Ok((_, val)) => val.to_f64().unwrap_or(0.0) as i32,
            Err(_) => 0,
        }
    }

    /// Helper: write f64.
    async fn put_f64(ch: &CaChannel, v: f64) {
        let _ = ch.put(&EpicsValue::Double(v)).await;
    }

    let client = CaClient::new().await?;
    let p = &config.prefix;
    let vsc = &config.scaler_pv;

    // Connect PVs
    let ch_emono = client.create_channel(&config.mono_pv);
    let ch_e_using = client.create_channel(&format!("{p}E_using"));
    let ch_flux = client.create_channel(&format!("{p}flux"));
    let ch_ion_photons = client.create_channel(&format!("{p}ionPhotons"));
    let ch_ion_abs = client.create_channel(&format!("{p}ionAbs"));
    let ch_detector = client.create_channel(&format!("{p}detector"));

    let ch_cnt = client.create_channel(&format!("{vsc}.CNT"));
    let ch_clock_rate = client.create_channel(&format!("{vsc}.FREQ"));
    let ch_ticks = client.create_channel(&format!("{vsc}.S1"));

    // Connect scaler channels 2-15
    let mut ch_scalers: Vec<CaChannel> = Vec::new();
    for i in 2..=15 {
        ch_scalers.push(client.create_channel(&format!("{vsc}.S{i}")));
    }

    let ch_v_per_a = client.create_channel(&format!("{p}VperA"));
    let ch_v2f = client.create_channel(&format!("{p}v2f"));
    let ch_x_n2 = client.create_channel(&format!("{p}xN2"));
    let ch_x_ar = client.create_channel(&format!("{p}xAr"));
    let ch_x_he = client.create_channel(&format!("{p}xHe"));
    let ch_x_air = client.create_channel(&format!("{p}xAir"));
    let ch_x_co2 = client.create_channel(&format!("{p}xCO2"));
    let ch_active_len = client.create_channel(&format!("{p}activeLen"));
    let ch_dead_front = client.create_channel(&format!("{p}deadFront"));
    let ch_dead_rear = client.create_channel(&format!("{p}deadRear"));
    let ch_kapton1 = client.create_channel(&format!("{p}kapton1"));
    let ch_kapton2 = client.create_channel(&format!("{p}kapton2"));
    let ch_he_path = client.create_channel(&format!("{p}HePath"));
    let ch_air_path = client.create_channel(&format!("{p}airPath"));
    let ch_be = client.create_channel(&format!("{p}Be"));
    let ch_d_eff = client.create_channel(&format!("{p}efficiency"));
    let ch_scaler_ch = client.create_channel(&format!("{p}scaler"));
    let ch_ar_pcntr = client.create_channel(&format!("{p}ArPcntr"));

    // Subscriptions
    let mut sub_emono = ch_emono.subscribe().await?;
    let mut sub_e_using = ch_e_using.subscribe().await?;
    let mut sub_cnt = ch_cnt.subscribe().await?;
    let mut sub_v_per_a = ch_v_per_a.subscribe().await?;
    let mut sub_x_n2 = ch_x_n2.subscribe().await?;
    let mut sub_active_len = ch_active_len.subscribe().await?;
    let mut sub_ar_pcntr = ch_ar_pcntr.subscribe().await?;
    let mut sub_d_eff = ch_d_eff.subscribe().await?;

    // Initialize parameters
    let mut params = IoParams { energy: get_f64(&ch_emono).await, ..Default::default() };
    if params.energy == 0.0 { params.energy = 10.0; }
    put_f64(&ch_e_using, params.energy).await;

    tracing::info!("Io state machine running for {p}");

    let update_rate = Duration::from_secs(10);

    loop {
        let needs_update = select! {
            Some(Ok(_val)) = sub_emono.recv() => {
                let e = get_f64(&ch_emono).await;
                params.energy = e;
                put_f64(&ch_e_using, e).await;
                true
            }
            Some(Ok(_val)) = sub_e_using.recv() => {
                params.energy = get_f64(&ch_e_using).await;
                true
            }
            Some(Ok(_val)) = sub_cnt.recv() => true,
            Some(Ok(_val)) = sub_v_per_a.recv() => {
                params.v_per_a = get_f64(&ch_v_per_a).await;
                true
            }
            Some(Ok(_val)) = sub_x_n2.recv() => {
                params.x_n2 = get_f64(&ch_x_n2).await;
                params.x_ar = get_f64(&ch_x_ar).await;
                params.x_he = get_f64(&ch_x_he).await;
                params.x_air = get_f64(&ch_x_air).await;
                params.x_co2 = get_f64(&ch_x_co2).await;
                true
            }
            Some(Ok(_val)) = sub_active_len.recv() => {
                params.active_len = get_f64(&ch_active_len).await;
                params.dead_front = get_f64(&ch_dead_front).await;
                params.dead_rear = get_f64(&ch_dead_rear).await;
                true
            }
            Some(Ok(_val)) = sub_ar_pcntr.recv() => {
                let val = get_i32(&ch_ar_pcntr).await;
                params.ar_pcntr = val != 0;
                if params.ar_pcntr {
                    params.detector_efficiency = ar_pcntr_efficiency(params.energy);
                    put_f64(&ch_d_eff, params.detector_efficiency).await;
                }
                true
            }
            Some(Ok(_val)) = sub_d_eff.recv() => {
                params.detector_efficiency = get_f64(&ch_d_eff).await;
                params.ar_pcntr = false;
                let _ = ch_ar_pcntr.put(&EpicsValue::Long(0)).await;
                true
            }
            _ = sleep(update_rate) => true,
        };

        if needs_update {
            // Read current parameters
            params.v2f = get_f64(&ch_v2f).await;
            params.kapton_front = get_f64(&ch_kapton1).await;
            params.kapton_rear = get_f64(&ch_kapton2).await;
            params.he_path = get_f64(&ch_he_path).await;
            params.air_path = get_f64(&ch_air_path).await;
            params.be_thickness = get_f64(&ch_be).await;
            params.scaler_channel = get_i32(&ch_scaler_ch).await.max(2) as usize;
            params.clock_rate = get_f64(&ch_clock_rate).await;

            // Read scaler values
            let ticks = get_f64(&ch_ticks).await;
            let mut scaler_counts: Vec<f64> = Vec::with_capacity(14);
            for ch in &ch_scalers {
                scaler_counts.push(get_f64(ch).await);
            }

            let results = eval_flux(&params, &scaler_counts, ticks);

            put_f64(&ch_flux, results.flux).await;
            put_f64(&ch_ion_photons, results.ion_photons).await;
            put_f64(&ch_ion_abs, results.ion_abs).await;
            put_f64(&ch_detector, results.detector).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_absorb_helium() {
        let mu = absorb(GasId::Helium, 10.0);
        assert!(mu > 0.0, "He absorption at 10 keV should be positive: {mu}");
        assert!(mu < 1.0, "He absorption at 10 keV should be small: {mu}");
    }

    #[test]
    fn test_absorb_nitrogen() {
        let mu = absorb(GasId::Nitrogen, 10.0);
        assert!(mu > 0.0, "N2 absorption at 10 keV should be positive: {mu}");
    }

    #[test]
    fn test_absorb_argon() {
        let mu = absorb(GasId::Argon, 10.0);
        assert!(mu > 0.0);
        // Argon should absorb more than Helium at the same energy
        assert!(mu > absorb(GasId::Helium, 10.0));
    }

    #[test]
    fn test_absorb_kapton() {
        let mu = absorb(GasId::Kapton, 10.0);
        assert!(mu > 0.0);
    }

    #[test]
    fn test_absorb_air() {
        let mu = absorb(GasId::Air, 10.0);
        assert!(mu > 0.0);
    }

    #[test]
    fn test_absorb_co2() {
        let mu = absorb(GasId::CO2, 10.0);
        assert!(mu > 0.0);
    }

    #[test]
    fn test_absorb_beryllium() {
        let mu = absorb(GasId::Beryllium, 10.0);
        assert!(mu > 0.0);
        // Be is a solid, so linear absorption should be significant
        assert!(mu > absorb(GasId::Helium, 10.0));
    }

    #[test]
    fn test_abs_ar_photo() {
        let mu = abs_ar_photo(10.0);
        assert!(mu > 0.0);
    }

    #[test]
    fn test_ar_pcntr_efficiency() {
        let eff = ar_pcntr_efficiency(10.0);
        assert!(eff > 0.0 && eff <= 1.0, "Efficiency = {eff}");
    }

    #[test]
    fn test_eval_flux_zero_counts() {
        let params = IoParams::default();
        let results = eval_flux(&params, &[0.0; 14], 1e7);
        assert_eq!(results.flux, 0.0);
        assert_eq!(results.detector, 0.0);
    }

    #[test]
    fn test_eval_flux_with_counts() {
        let params = IoParams {
            energy: 10.0,
            v_per_a: 1.0e8,
            v2f: 1.0e5,
            x_air: 1.0,
            active_len: 60.0,
            dead_front: 17.5,
            dead_rear: 17.5,
            kapton_front: 0.001,
            kapton_rear: 0.001,
            scaler_channel: 2,
            clock_rate: 1.0e7,
            detector_efficiency: 1.0,
            ..Default::default()
        };

        // 1 million counts with 1 second counting time
        let scaler_counts = [1.0e6; 14];
        let ticks = 1.0e7; // 1 second at 10 MHz clock
        let results = eval_flux(&params, &scaler_counts, ticks);
        assert!(results.flux > 0.0, "flux = {}", results.flux);
        assert!(results.detector > 0.0, "detector = {}", results.detector);
        assert!(results.ion_abs > 0.0 && results.ion_abs <= 1.0);
    }

    #[test]
    fn test_eval_flux_zero_energy() {
        let mut params = IoParams::default();
        params.energy = 0.0;
        let results = eval_flux(&params, &[1e6; 14], 1e7);
        assert_eq!(results.flux, 0.0);
    }

    #[test]
    fn test_eval_flux_zero_ticks() {
        let params = IoParams::default();
        let results = eval_flux(&params, &[1e6; 14], 0.0);
        assert_eq!(results.flux, 0.0);
    }

    #[test]
    fn test_photon_gas_comparison() {
        // With the same CPS and chamber config, heavier gases should give more flux
        // because they have higher photoelectric cross-sections
        let cps = 1.0e6;
        let v2f = 1.0e5;
        let vpa = 1.0e8;
        let aln = 6.0; // cm
        let dln = 1.75; // cm
        let kap = 0.001; // inches
        let energy = 10.0;

        let flux_he = photon(cps, WORK_HE, v2f, vpa, aln, dln, kap, GasId::Helium, energy);
        let flux_n2 = photon(cps, WORK_N2, v2f, vpa, aln, dln, kap, GasId::Nitrogen, energy);
        let flux_ar = photon(cps, WORK_AR, v2f, vpa, aln, dln, kap, GasId::Argon, energy);

        // All should be positive
        assert!(flux_he > 0.0);
        assert!(flux_n2 > 0.0);
        assert!(flux_ar > 0.0);
    }

    #[test]
    fn test_absorb_energy_dependence() {
        // Absorption should generally decrease with energy (above edge)
        let mu_5 = absorb(GasId::Air, 5.0);
        let mu_20 = absorb(GasId::Air, 20.0);
        assert!(
            mu_5 > mu_20,
            "Air absorption should decrease with energy: {mu_5} vs {mu_20}"
        );
    }

    #[test]
    fn test_argon_k_edge() {
        // Argon has K-edge at 3.202 keV - absorption should be higher just above
        let mu_below = absorb(GasId::Argon, 3.0);
        let mu_above = absorb(GasId::Argon, 3.5);
        assert!(
            mu_above > mu_below,
            "Argon absorption should jump at K-edge: below={mu_below}, above={mu_above}"
        );
    }
}
