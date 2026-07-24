//! XIA PF4 dual-filter bank state machine.
//!
//! Pure Rust port of `pf4.st` — manages two 4-bit filter banks where each bank
//! has 4 filter blades (16 combinations), using Chantler table data from
//! [`crate::data::chantler`].

use epics_base_rs::server::database::PvDatabase;

use crate::data::chantler::{find_material, other_absorption_length_um};
use crate::db_access::{DbChannel, DbMultiMonitor, alloc_origin};

/// Number of filter combinations per bank (4 bits = 16).
pub const NUM_COMBINATIONS: usize = 16;

/// Material index constants matching the SNL code.
pub const MAT_AL: u8 = 0;
pub const MAT_TI: u8 = 1;
pub const MAT_GLASS: u8 = 2;
pub const MAT_OTHER: u8 = 3;

/// State of the PF4 bank state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pf4State {
    Init,
    Idle,
    FilterBits,
    FilterPos,
    RecalcBank,
    BankControl,
    BankOff,
}

/// Configuration for one PF4 bank.
#[derive(Debug, Clone)]
pub struct Pf4BankConfig {
    /// Filter blade thicknesses in mm (4 blades).
    pub thicknesses: [f64; 4],
    /// Material index for each blade (0=Al, 1=Ti, 2=Glass, 3=Other).
    pub material_indices: [u8; 4],
    /// Material name for "Other" blades.
    pub other_materials: [String; 4],
}

impl Default for Pf4BankConfig {
    fn default() -> Self {
        Self {
            thicknesses: [0.0; 4],
            material_indices: [0; 4],
            other_materials: [String::new(), String::new(), String::new(), String::new()],
        }
    }
}

/// Full PF4 configuration.
#[derive(Debug, Clone)]
pub struct Pf4Config {
    pub prefix: String,
    pub hardware: String,
    pub bank: String,
}

impl Pf4Config {
    pub fn new(p: &str, h: &str, b: &str) -> Self {
        Self {
            prefix: p.to_string(),
            hardware: h.to_string(),
            bank: b.to_string(),
        }
    }
}

/// Absorption length of aluminium in microns at the given photon energy.
///
/// Direct port of `pf4.st`'s `AlAbsorptionLength` (`:484-505`): a 7-term
/// polynomial in eV with two coefficient sets split at the `kink` energy, the
/// input clamped to 60 keV. Returns microns.
fn al_absorption_length_microns(kev: f64) -> f64 {
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

    let mut ev = kev * 1000.0; // convert to eV
    if ev > 60000.0 {
        ev = 60000.0;
    }
    let coef = if ev < KINK { &WCOEF0 } else { &WCOEF1 };
    let mut sum = 0.0;
    let mut power = 1.0;
    for &c in coef.iter() {
        sum += c * power;
        power *= ev;
    }
    sum
}

/// Absorption length of titanium in microns at the given photon energy.
///
/// Direct port of `pf4.st`'s `TiAbsorptionLength` (`:509-538`): below 1 keV the
/// coefficient `mu` is 0 (returns +inf → full transmission); separate fits
/// between the L- and K-edges and above the K-edge. Returns `1/mu` in microns.
fn ti_absorption_length_microns(kev: f64) -> f64 {
    let ev = kev * 1000.0; // convert keV to eV
    let mu = if ev < 1e3 {
        0.0 // this routine only good above 1000 eV
    } else if ev < 4966.4 {
        // above L-edge, and below K-edge
        let c0 = 0.00092284;
        let c1 = 2.5891e+08;
        let pow_a = -2.6651;
        c0 + c1 * ev.powf(pow_a)
    } else {
        // above the K-edge
        let offset = 5.63768167444831e-5;
        let amp = 24061652313.4169;
        let pow_b = -2.91380053083527;
        let intercept = -0.268162843203489;
        let slope = 3.74221014277593e-5;
        let amp_exp = -1.05663835782997;
        let inv_tau = -0.000570785180739491;
        let extra = if ev < 6456.0 {
            intercept + slope * ev
        } else {
            amp_exp * (inv_tau * ev).exp()
        };
        offset + amp * ev.powf(pow_b) + extra
    };
    1.0 / mu
}

/// Absorption length of borosilicate glass in microns at the given photon energy.
///
/// Direct port of `pf4.st`'s `GlassAbsorptionLength` (`:553-610`): a piecewise
/// polynomial in keV with breakpoints at the S, K, Ca and Fe K-edges, finally
/// scaled by energy^3. Below 2 keV returns 0 (full absorption). Returns microns.
fn glass_absorption_length_microns(kev: f64) -> f64 {
    let kev2 = kev * kev;
    let mut abs_length;
    if kev < 2.0 {
        abs_length = 0.0; // this routine only good above 2 keV
    } else if kev < 2.472 {
        // below Sulphur K edge
        abs_length = 0.5059463974 + -0.1259565387 * kev + 0.01763933889 * kev2;
    } else if kev < 3.6084 {
        // above Sulphur K and below Potassium K
        abs_length = 0.4570603245 + -0.08869920063 * kev + 0.01032934773 * kev2;
    } else if kev < 4.0385 {
        // above Potassium K and below Calcium K
        abs_length = 0.3708574258 + -0.04453063888 * kev + 3.979930821e-3 * kev2;
    } else if kev < 7.112 {
        // above Calcium K and below Iron K
        abs_length = 0.2830642538 + -0.0223186563 * kev + 1.412011413e-3 * kev2;
    } else {
        // above Iron K
        let c0 = 0.2715022686;
        let c1 = -0.02428526798;
        let c2 = 2.984228845e-3;
        let c3 = -2.003675391e-4;
        let c4 = 7.983398893e-6;
        let c5 = -1.869726202e-7;
        let c6 = 2.378962632e-9;
        let c7 = -1.270082060e-11;
        abs_length = c0 + c1 * kev;
        let mut kevn = kev2;
        abs_length += c2 * kevn;
        kevn *= kev;
        abs_length += c3 * kevn;
        kevn *= kev;
        abs_length += c4 * kevn;
        kevn *= kev;
        abs_length += c5 * kevn;
        kevn *= kev;
        abs_length += c6 * kevn;
        kevn *= kev;
        abs_length += c7 * kevn;
    }
    abs_length *= kev * kev * kev; // finally scale by energy^3
    abs_length
}

/// Check whether an "Other" material name is legal (found in Chantler tables).
pub fn is_legal_other(name: &str) -> bool {
    find_material(name).is_some()
}

/// Calculate the transmission for a single filter blade.
///
/// `thickness_mm` is in millimetres. Al, Ti and Glass use the analytic
/// absorption-length fits ported from `pf4.st`; only "Other" reads the Chantler
/// table, mirroring `RecalcFilters` (`pf4.st:661-699`) where the three named
/// materials use `AlAbsorptionLength`/`TiAbsorptionLength`/`GlassAbsorptionLength`
/// and only `z==3` calls `OtherAbsorptionLength`.
///
/// **DEVIATION from C, deliberate — CBUG-B4.** An "Other" blade whose material
/// name is not in the Chantler table, or whose energy is outside that material's
/// tabulated range, has **no absorption data** — and C answers that with `0.`
/// (`pf4.st:629-631`, `:637-639`), which reaches the divisor below as
/// `exp(-x*1000./0.)` = `exp(-inf)` = `0.0`: the blade is reported **perfectly
/// opaque**, which is the maximally wrong answer, delivered with no error and no
/// alarm. Both of C's `printf` diagnostics for it are commented out in the
/// shipped source. A mistyped material name is an ordinary operator error.
///
/// The port cannot compute a transmission it has no data for and does not invent
/// one: the blade is **not modelled** (transmission `1.0`, contributing no
/// attenuation) and the condition is logged. It is already visible to the
/// operator too — an unknown name drives `otherLegal` false
/// ([`is_legal_other`], published via [`Pf4Actions::write_other_legal`]), which
/// is the field the record has for exactly this.
///
/// Not modelling the blade is also the safe direction of the two wrong answers:
/// a blade believed opaque is one the ranking would happily select to attenuate,
/// letting far more beam through than predicted.
pub fn calc_blade_transmission(
    energy_kev: f64,
    thickness_mm: f64,
    mat_idx: u8,
    other_name: &str,
) -> f64 {
    if thickness_mm <= 0.0 || energy_kev <= 0.0 {
        return 1.0;
    }
    let abs_len_microns = match mat_idx {
        MAT_AL => al_absorption_length_microns(energy_kev),
        MAT_TI => ti_absorption_length_microns(energy_kev),
        MAT_GLASS => glass_absorption_length_microns(energy_kev),
        MAT_OTHER => {
            match find_material(other_name)
                .and_then(|mat| other_absorption_length_um(mat, energy_kev))
            {
                Some(len) => len,
                None => {
                    tracing::error!(
                        material = other_name,
                        energy_kev,
                        "pf4: no absorption data for this Other blade \
                         (unknown material, or energy outside its Chantler table); \
                         the blade is NOT modelled — its transmission is reported as 1.0"
                    );
                    return 1.0;
                }
            }
        }
        _ => return 1.0,
    };
    // C: xmit *= exp(-thickness_mm*1000 / absLen_microns) (pf4.st:693-699).
    (-thickness_mm * 1000.0 / abs_len_microns).exp()
}

/// Calculate transmissions for all 16 combinations in a bank, sorted by
/// decreasing transmission. Returns `(transmissions, bit_patterns)`.
pub fn recalc_filters(
    energy_kev: f64,
    bank_config: &Pf4BankConfig,
    bank_on: bool,
) -> ([f64; NUM_COMBINATIONS], [u8; NUM_COMBINATIONS]) {
    let mut xmit = [1.0_f64; NUM_COMBINATIONS];
    let mut bits = [0_u8; NUM_COMBINATIONS];

    if !bank_on || energy_kev <= 0.0 {
        for (i, b) in bits.iter_mut().enumerate() {
            *b = i as u8;
        }
        return (xmit, bits);
    }

    // Calculate per-blade transmissions
    let mut blade_trans = [1.0_f64; 4];
    for (b, bt) in blade_trans.iter_mut().enumerate() {
        *bt = calc_blade_transmission(
            energy_kev,
            bank_config.thicknesses[b],
            bank_config.material_indices[b],
            &bank_config.other_materials[b],
        );
    }

    // Calculate all 16 combinations
    for (i, (x, bi)) in xmit.iter_mut().zip(bits.iter_mut()).enumerate() {
        *x = 1.0;
        *bi = i as u8;
        for (b, &bt) in blade_trans.iter().enumerate() {
            if i & (1 << b) != 0 {
                *x *= bt;
            }
        }
    }

    // Sort by decreasing transmission (insertion sort to match SNL)
    sort_decreasing(&mut xmit, &mut bits);

    (xmit, bits)
}

/// Sort arrays by decreasing transmission, keeping bits synchronized.
fn sort_decreasing(xmit: &mut [f64; NUM_COMBINATIONS], bits: &mut [u8; NUM_COMBINATIONS]) {
    for j in 1..NUM_COMBINATIONS {
        let a = xmit[j];
        let b = bits[j];
        let mut i = j as isize - 1;
        while i >= 0 && xmit[i as usize] < a {
            xmit[(i + 1) as usize] = xmit[i as usize];
            bits[(i + 1) as usize] = bits[i as usize];
            i -= 1;
        }
        xmit[(i + 1) as usize] = a;
        bits[(i + 1) as usize] = b;
    }
}

/// Find the position index for a given bit pattern in the sorted bits array.
pub fn find_position(bits: &[u8; NUM_COMBINATIONS], pattern: u8) -> usize {
    bits.iter().position(|&b| b == pattern).unwrap_or(0)
}

/// Calculate total thickness of a given material type currently in beam.
pub fn thickness_by_material(
    target_mat: u8,
    bank_on: bool,
    bit_states: [bool; 4],
    mat_indices: [u8; 4],
    thicknesses: [f64; 4],
) -> f64 {
    if !bank_on {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..4 {
        if bit_states[i] && mat_indices[i] == target_mat {
            sum += thicknesses[i];
        }
    }
    sum
}

/// Extract the 4 bit states from a bit pattern.
pub fn pattern_to_bits(pattern: u8) -> [bool; 4] {
    [
        pattern & 1 != 0,
        pattern & 2 != 0,
        pattern & 4 != 0,
        pattern & 8 != 0,
    ]
}

/// Encode 4 bit states into a bit pattern.
pub fn bits_to_pattern(b1: bool, b2: bool, b3: bool, b4: bool) -> u8 {
    (b1 as u8) | ((b2 as u8) << 1) | ((b3 as u8) << 2) | ((b4 as u8) << 3)
}

/// PF4 bank controller — pure logic.
#[derive(Debug, Clone)]
pub struct Pf4Controller {
    pub state: Pf4State,
    pub bank_config: Pf4BankConfig,
    pub energy_kev: f64,
    pub bank_on: bool,
    pub use_mono: bool,
    pub local_energy: f64,
    pub mono_energy: f64,

    /// Current filter bit states.
    pub bit_states: [bool; 4],
    /// Current filter position index (0-15 in sorted order).
    pub filter_pos: usize,
    /// Sorted transmissions.
    pub xmit: [f64; NUM_COMBINATIONS],
    /// Sorted bit patterns.
    pub bits: [u8; NUM_COMBINATIONS],
    /// Current bank transmission.
    pub transmission: f64,
    /// Current bank inverse transmission.
    pub inv_transmission: f64,

    /// Combined Al thickness in beam (mm).
    pub filter_al: f64,
    /// Combined Ti thickness in beam (mm).
    pub filter_ti: f64,
    /// Combined Glass thickness in beam (mm).
    pub filter_glass: f64,
}

impl Default for Pf4Controller {
    fn default() -> Self {
        let mut bits = [0u8; NUM_COMBINATIONS];
        for (i, b) in bits.iter_mut().enumerate() {
            *b = i as u8;
        }
        Self {
            state: Pf4State::Init,
            bank_config: Pf4BankConfig::default(),
            energy_kev: 10.0,
            bank_on: false,
            use_mono: true,
            local_energy: 10.0,
            mono_energy: 10.0,
            bit_states: [false; 4],
            filter_pos: 0,
            xmit: [1.0; NUM_COMBINATIONS],
            bits,
            transmission: 1.0,
            inv_transmission: 1.0,
            filter_al: 0.0,
            filter_ti: 0.0,
            filter_glass: 0.0,
        }
    }
}

/// Events that drive the PF4 state machine.
#[derive(Debug, Clone)]
pub enum Pf4Event {
    /// Filter control bits changed from hardware.
    BitsChanged([bool; 4]),
    /// Mono energy changed.
    MonoEnergyChanged(f64),
    /// Local energy changed.
    LocalEnergyChanged(f64),
    /// Energy source selection changed (true = use mono).
    EnergySelectChanged(bool),
    /// Bank on/off control changed.
    BankControlChanged(bool),
    /// Filter thicknesses changed.
    ThicknessChanged([f64; 4]),
    /// Material indices changed.
    MaterialChanged([u8; 4]),
    /// "Other" material names changed.
    OtherMaterialChanged([String; 4]),
    /// Filter position selection changed (user picks from sorted list).
    FilterPosChanged(usize),
}

/// Actions the caller should take after processing a PF4 event.
#[derive(Debug, Clone, Default)]
pub struct Pf4Actions {
    /// New bit states to write to hardware.
    pub set_bits: Option<[bool; 4]>,
    /// Updated transmission labels for all 16 positions.
    pub write_labels: Option<[String; NUM_COMBINATIONS]>,
    /// Updated transmission value.
    pub write_transmission: Option<f64>,
    /// Updated inverse transmission value.
    pub write_inv_transmission: Option<f64>,
    /// Other material legality flags.
    pub write_other_legal: Option<[bool; 4]>,
    /// Material thickness readbacks.
    pub write_filter_al: Option<f64>,
    pub write_filter_ti: Option<f64>,
    pub write_filter_glass: Option<f64>,
}

impl Pf4Controller {
    /// Recompute transmission / inverse-transmission for the current filter
    /// position and stage the matching writes.
    ///
    /// C (pf4.st:281-282) PVPUTs `trans` unconditionally but `invtrans` only when
    /// `trans > 0.0`; a zero transmission (e.g. a glass blade below 2 keV) leaves
    /// `{H}invTrans{B}` at its prior value rather than publishing +inf. This is the
    /// single owner of both writes so the gate holds on every path.
    fn emit_transmission(&mut self, actions: &mut Pf4Actions) {
        self.transmission = self.xmit[self.filter_pos];
        self.inv_transmission = if self.transmission > 0.0 {
            1.0 / self.transmission
        } else {
            f64::INFINITY
        };
        actions.write_transmission = Some(self.transmission);
        if self.transmission > 0.0 {
            actions.write_inv_transmission = Some(self.inv_transmission);
        }
    }

    /// Recalculate all filter transmissions and update derived values.
    pub fn recalculate(&mut self) -> Pf4Actions {
        let mut actions = Pf4Actions::default();

        let effective_energy = if self.use_mono {
            self.mono_energy
        } else {
            self.local_energy
        };
        self.energy_kev = effective_energy;

        let (xmit, bits) = recalc_filters(self.energy_kev, &self.bank_config, self.bank_on);
        self.xmit = xmit;
        self.bits = bits;

        // Update filter position
        let current_pattern = bits_to_pattern(
            self.bit_states[0],
            self.bit_states[1],
            self.bit_states[2],
            self.bit_states[3],
        );
        self.filter_pos = find_position(&self.bits, current_pattern);
        self.emit_transmission(&mut actions);

        // Material thicknesses
        self.filter_al = thickness_by_material(
            MAT_AL,
            self.bank_on,
            self.bit_states,
            self.bank_config.material_indices,
            self.bank_config.thicknesses,
        );
        self.filter_ti = thickness_by_material(
            MAT_TI,
            self.bank_on,
            self.bit_states,
            self.bank_config.material_indices,
            self.bank_config.thicknesses,
        );
        self.filter_glass = thickness_by_material(
            MAT_GLASS,
            self.bank_on,
            self.bit_states,
            self.bank_config.material_indices,
            self.bank_config.thicknesses,
        );

        // Build labels
        let mut labels: [String; NUM_COMBINATIONS] = Default::default();
        for (label, x) in labels.iter_mut().zip(self.xmit.iter()) {
            *label = format!("{:.3e}", x);
        }
        actions.write_labels = Some(labels);
        // trans / invtrans were staged by emit_transmission above (invtrans gated
        // on trans > 0).
        // C only PVPUTs filterAl/Ti/Gl when a blade uses that material
        // (pf4.st:277-279: `if(z1==0||z2==0||z3==0||z4==0)` etc.), and only in
        // the bankctl==2 recompute branch, which is reached solely while the bank
        // is on. A material with no blade — or any material while the bank is off
        // (thickZ returns 0) — is left at its prior value, not overwritten with 0.
        let mats = &self.bank_config.material_indices;
        if self.bank_on && mats.contains(&MAT_AL) {
            actions.write_filter_al = Some(self.filter_al);
        }
        if self.bank_on && mats.contains(&MAT_TI) {
            actions.write_filter_ti = Some(self.filter_ti);
        }
        if self.bank_on && mats.contains(&MAT_GLASS) {
            actions.write_filter_glass = Some(self.filter_glass);
        }

        actions
    }

    /// Process a single event.
    pub fn step(&mut self, event: Pf4Event) -> Pf4Actions {
        match event {
            Pf4Event::BitsChanged(new_bits) => {
                self.bit_states = new_bits;
                let current_pattern =
                    bits_to_pattern(new_bits[0], new_bits[1], new_bits[2], new_bits[3]);
                self.filter_pos = find_position(&self.bits, current_pattern);
                let mut actions = Pf4Actions::default();
                self.emit_transmission(&mut actions);
                actions
            }

            Pf4Event::MonoEnergyChanged(e) => {
                self.mono_energy = e;
                if self.use_mono {
                    self.local_energy = e;
                    self.recalculate()
                } else {
                    Pf4Actions::default()
                }
            }

            Pf4Event::LocalEnergyChanged(e) => {
                self.local_energy = e;
                self.use_mono = false;
                self.recalculate()
            }

            Pf4Event::EnergySelectChanged(use_mono) => {
                self.use_mono = use_mono;
                if use_mono {
                    self.local_energy = self.mono_energy;
                }
                self.recalculate()
            }

            Pf4Event::BankControlChanged(on) => {
                self.bank_on = on;
                if on {
                    self.recalculate()
                } else {
                    Pf4Actions::default()
                }
            }

            Pf4Event::ThicknessChanged(t) => {
                self.bank_config.thicknesses = t;
                self.recalculate()
            }

            Pf4Event::MaterialChanged(m) => {
                self.bank_config.material_indices = m;
                self.recalculate()
            }

            Pf4Event::OtherMaterialChanged(names) => {
                let legal: [bool; 4] = [
                    is_legal_other(&names[0]),
                    is_legal_other(&names[1]),
                    is_legal_other(&names[2]),
                    is_legal_other(&names[3]),
                ];
                self.bank_config.other_materials = names;
                let mut actions = self.recalculate();
                actions.write_other_legal = Some(legal);
                actions
            }

            Pf4Event::FilterPosChanged(pos) => {
                if pos < NUM_COMBINATIONS && self.bank_on {
                    self.filter_pos = pos;
                    let pattern = self.bits[pos];
                    let new_bits = pattern_to_bits(pattern);

                    // Insert first, then remove
                    let mut actions = Pf4Actions {
                        set_bits: Some(new_bits),
                        ..Default::default()
                    };
                    self.bit_states = new_bits;
                    self.emit_transmission(&mut actions);
                    actions
                } else {
                    Pf4Actions::default()
                }
            }
        }
    }
}

/// Async entry point — runs the PF4 bank state machine against live PVs.
pub async fn run(
    config: Pf4Config,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::time::{Duration, sleep};

    tokio::time::sleep(Duration::from_secs(3)).await;
    println!(
        "pf4: starting for prefix={}{} bank {}",
        config.prefix, config.hardware, config.bank
    );

    let my_origin = alloc_origin();
    let ph = format!("{}{}", config.prefix, config.hardware);
    let b = &config.bank;

    // Connect PVs
    let ch_b1 = DbChannel::with_origin(&db, &format!("{ph}displayBit1{b}"), my_origin);
    let ch_b2 = DbChannel::with_origin(&db, &format!("{ph}displayBit2{b}"), my_origin);
    let ch_b3 = DbChannel::with_origin(&db, &format!("{ph}displayBit3{b}"), my_origin);
    let ch_b4 = DbChannel::with_origin(&db, &format!("{ph}displayBit4{b}"), my_origin);
    let ch_trans = DbChannel::with_origin(&db, &format!("{ph}trans{b}"), my_origin);
    let ch_inv_trans = DbChannel::with_origin(&db, &format!("{ph}invTrans{b}"), my_origin);
    let ch_bankctl = DbChannel::with_origin(&db, &format!("{ph}bank{b}"), my_origin);
    let _ch_filpos = DbChannel::with_origin(&db, &format!("{ph}fPos{b}"), my_origin);
    let ch_select_energy = DbChannel::with_origin(&db, &format!("{ph}useMono"), my_origin);
    let ch_local_energy = DbChannel::with_origin(&db, &format!("{ph}E:local"), my_origin);
    let ch_filter_al = DbChannel::with_origin(&db, &format!("{ph}filterAl"), my_origin);
    let ch_filter_ti = DbChannel::with_origin(&db, &format!("{ph}filterTi"), my_origin);
    let ch_filter_glass = DbChannel::with_origin(&db, &format!("{ph}filterGlass"), my_origin);

    let ch_f1 = DbChannel::with_origin(&db, &format!("{ph}f1{b}"), my_origin);
    let ch_f2 = DbChannel::with_origin(&db, &format!("{ph}f2{b}"), my_origin);
    let ch_f3 = DbChannel::with_origin(&db, &format!("{ph}f3{b}"), my_origin);
    let ch_f4 = DbChannel::with_origin(&db, &format!("{ph}f4{b}"), my_origin);

    let ch_z1 = DbChannel::with_origin(&db, &format!("{ph}Z1{b}"), my_origin);
    let ch_z2 = DbChannel::with_origin(&db, &format!("{ph}Z2{b}"), my_origin);
    let ch_z3 = DbChannel::with_origin(&db, &format!("{ph}Z3{b}"), my_origin);
    let ch_z4 = DbChannel::with_origin(&db, &format!("{ph}Z4{b}"), my_origin);

    // Build multi-monitor
    let monitored_pvs: Vec<String> = vec![
        format!("{ph}displayBit1{b}"),
        format!("{ph}bank{b}"),
        format!("{ph}fPos{b}"),
        format!("{ph}E:local"),
        format!("{ph}useMono"),
        format!("{ph}f1{b}"),
        format!("{ph}Z1{b}"),
    ];
    let mut monitor = DbMultiMonitor::new_filtered(&db, &monitored_pvs, my_origin).await;

    let mut ctrl = Pf4Controller::default();

    // Read initial values
    ctrl.mono_energy = {
        let v = ch_local_energy.get_f64().await;
        if v > 0.0 { v } else { 10.0 }
    };
    ctrl.local_energy = ctrl.mono_energy;
    ctrl.bank_config.thicknesses = [
        ch_f1.get_f64().await,
        ch_f2.get_f64().await,
        ch_f3.get_f64().await,
        ch_f4.get_f64().await,
    ];
    ctrl.bank_config.material_indices = [
        ch_z1.get_i16().await as i32 as u8,
        ch_z2.get_i16().await as i32 as u8,
        ch_z3.get_i16().await as i32 as u8,
        ch_z4.get_i16().await as i32 as u8,
    ];
    ctrl.bit_states = [
        ch_b1.get_i16().await as i32 != 0,
        ch_b2.get_i16().await as i32 != 0,
        ch_b3.get_i16().await as i32 != 0,
        ch_b4.get_i16().await as i32 != 0,
    ];
    ctrl.bank_on = ch_bankctl.get_i16().await as i32 != 0;
    ctrl.use_mono = ch_select_energy.get_i16().await as i32 != 0;

    let init_actions = ctrl.recalculate();
    apply_pf4_actions(
        &init_actions,
        &ch_trans,
        &ch_inv_trans,
        &ch_filter_al,
        &ch_filter_ti,
        &ch_filter_glass,
    )
    .await;

    tracing::info!("pf4 state machine running for {ph} bank {b}");

    let pv_b1 = format!("{ph}displayBit1{b}");
    let pv_bankctl = format!("{ph}bank{b}");
    let pv_filpos = format!("{ph}fPos{b}");
    let pv_local_energy = format!("{ph}E:local");
    let pv_select_energy = format!("{ph}useMono");
    let pv_f1 = format!("{ph}f1{b}");
    let pv_z1 = format!("{ph}Z1{b}");

    loop {
        let (changed_pv, new_val) = monitor.wait_change().await;

        let event: Option<Pf4Event> = if changed_pv == pv_b1 {
            let bits = [
                ch_b1.get_i16().await as i32 != 0,
                ch_b2.get_i16().await as i32 != 0,
                ch_b3.get_i16().await as i32 != 0,
                ch_b4.get_i16().await as i32 != 0,
            ];
            Some(Pf4Event::BitsChanged(bits))
        } else if changed_pv == pv_bankctl {
            Some(Pf4Event::BankControlChanged(new_val as i32 != 0))
        } else if changed_pv == pv_filpos {
            Some(Pf4Event::FilterPosChanged(new_val as i32 as usize))
        } else if changed_pv == pv_local_energy {
            Some(Pf4Event::LocalEnergyChanged(new_val))
        } else if changed_pv == pv_select_energy {
            Some(Pf4Event::EnergySelectChanged(new_val as i32 != 0))
        } else if changed_pv == pv_f1 {
            let t = [
                ch_f1.get_f64().await,
                ch_f2.get_f64().await,
                ch_f3.get_f64().await,
                ch_f4.get_f64().await,
            ];
            Some(Pf4Event::ThicknessChanged(t))
        } else if changed_pv == pv_z1 {
            let m = [
                ch_z1.get_i16().await as i32 as u8,
                ch_z2.get_i16().await as i32 as u8,
                ch_z3.get_i16().await as i32 as u8,
                ch_z4.get_i16().await as i32 as u8,
            ];
            Some(Pf4Event::MaterialChanged(m))
        } else {
            None
        };

        if let Some(ev) = event {
            let actions = ctrl.step(ev);
            apply_pf4_actions(
                &actions,
                &ch_trans,
                &ch_inv_trans,
                &ch_filter_al,
                &ch_filter_ti,
                &ch_filter_glass,
            )
            .await;

            if let Some(bits) = actions.set_bits {
                // Insert first, then remove (as per original SNL)
                if bits[0] {
                    let _ = ch_b1.put_i16_process(1_i16).await;
                }
                if bits[1] {
                    let _ = ch_b2.put_i16_process(1_i16).await;
                }
                if bits[2] {
                    let _ = ch_b3.put_i16_process(1_i16).await;
                }
                if bits[3] {
                    let _ = ch_b4.put_i16_process(1_i16).await;
                }
                sleep(Duration::from_millis(200)).await;
                if !bits[0] {
                    let _ = ch_b1.put_i16_process(0_i16).await;
                }
                if !bits[1] {
                    let _ = ch_b2.put_i16_process(0_i16).await;
                }
                if !bits[2] {
                    let _ = ch_b3.put_i16_process(0_i16).await;
                }
                if !bits[3] {
                    let _ = ch_b4.put_i16_process(0_i16).await;
                }
            }
        }
    }
}

async fn apply_pf4_actions(
    actions: &Pf4Actions,
    ch_trans: &DbChannel,
    ch_inv_trans: &DbChannel,
    ch_filter_al: &DbChannel,
    ch_filter_ti: &DbChannel,
    ch_filter_glass: &DbChannel,
) {
    if let Some(t) = actions.write_transmission {
        let _ = ch_trans.put_f64_process(t).await;
    }
    if let Some(t) = actions.write_inv_transmission {
        let _ = ch_inv_trans.put_f64_process(t).await;
    }
    if let Some(t) = actions.write_filter_al {
        let _ = ch_filter_al.put_f64_process(t).await;
    }
    if let Some(t) = actions.write_filter_ti {
        let _ = ch_filter_ti.put_f64_process(t).await;
    }
    if let Some(t) = actions.write_filter_glass {
        let _ = ch_filter_glass.put_f64_process(t).await;
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default, clippy::needless_range_loop)]
mod tests {
    use super::*;

    #[test]
    fn test_bits_to_pattern() {
        assert_eq!(bits_to_pattern(false, false, false, false), 0);
        assert_eq!(bits_to_pattern(true, false, false, false), 1);
        assert_eq!(bits_to_pattern(false, true, false, false), 2);
        assert_eq!(bits_to_pattern(true, true, true, true), 15);
    }

    #[test]
    fn test_pattern_to_bits() {
        assert_eq!(pattern_to_bits(0), [false, false, false, false]);
        assert_eq!(pattern_to_bits(5), [true, false, true, false]);
        assert_eq!(pattern_to_bits(15), [true, true, true, true]);
    }

    #[test]
    fn test_roundtrip_bits() {
        for i in 0..16u8 {
            let bits = pattern_to_bits(i);
            assert_eq!(bits_to_pattern(bits[0], bits[1], bits[2], bits[3]), i);
        }
    }

    #[test]
    fn test_is_legal_other() {
        assert!(is_legal_other("Cu"));
        assert!(is_legal_other("Al"));
        assert!(!is_legal_other("Unobtainium"));
        assert!(!is_legal_other(""));
    }

    /// CBUG-B4 — an "Other" blade with an unknown material name has no
    /// absorption data, so it is NOT MODELLED (1.0) and `otherLegal` says so.
    ///
    /// This test used to pin C's answer, `0.0`: C returns `0.` for the unknown
    /// species (`pf4.st:629-631`) and then divides by it — `exp(-t*1000/0)` =
    /// `exp(-inf)` = 0 — reporting the blade **fully opaque**, silently.
    #[test]
    fn test_b4_other_unknown_material_is_not_modelled() {
        assert_eq!(
            calc_blade_transmission(10.0, 1.0, MAT_OTHER, "Unobtainium"),
            1.0 // C: 0.0 (fully opaque)
        );
        // C's strcmp is case-sensitive (pf4.st:627), so "cu" is unknown too.
        assert_eq!(calc_blade_transmission(10.0, 1.0, MAT_OTHER, "cu"), 1.0);
        // ... and the operator sees it: the record publishes otherLegal.
        assert!(!is_legal_other("cu"));
        assert!(!is_legal_other("Unobtainium"));
    }

    /// CBUG-B4 — the same for an energy outside the material's tabulated range.
    /// This test used to pin C's `0.0` (fully opaque).
    #[test]
    fn test_b4_other_energy_above_the_table_is_not_modelled() {
        // Cu's table ends at 432.95 keV; at or above the last node the scan for
        // `keV < keV[j]` never breaks, so there is no interval and no data.
        let cu = crate::data::chantler::find_material("Cu").unwrap();
        let last = cu.kev[cu.kev.len() - 1] as f64;
        assert_eq!(calc_blade_transmission(500.0, 1.0, MAT_OTHER, "Cu"), 1.0);
        assert_eq!(calc_blade_transmission(last, 1.0, MAT_OTHER, "Cu"), 1.0);
        // Just below the last node is inside the table (the top bin), so the
        // blade IS modelled — the boundary is the node itself.
        let inside = calc_blade_transmission(last - 0.001, 1.0, MAT_OTHER, "Cu");
        assert!(inside > 0.0 && inside < 1.0);
    }

    #[test]
    fn test_r6_63_other_zero_thickness_stays_transparent() {
        // C guards the multiply with `if (xOther1 > 0)` (pf4.st:696), so a blade
        // of zero thickness contributes 1.0 even with a bad material name.
        assert_eq!(
            calc_blade_transmission(10.0, 0.0, MAT_OTHER, "Unobtainium"),
            1.0
        );
    }

    /// CBUG-B1 — the "Other" blade's transmission now comes from the interval
    /// containing the energy. This test used to pin C's backwards extrapolation:
    /// `absLen(Cu, 10 keV) = 5.28411457133 um -> exp(-1/5.28411457133) =
    /// 0.827582511947`. The correct absorption length is 1e4/(rho * mu) with
    /// `mu = 215.521099278` (the value C's own `calcTrans` gives at 10 keV).
    #[test]
    fn test_b1_other_in_range_uses_the_containing_interval() {
        let cu = crate::data::chantler::find_material("Cu").unwrap();
        let abs_len_um = 1.0e4 / (cu.density * 215.521099278);
        let want = (-1.0 / abs_len_um).exp();
        let t = calc_blade_transmission(10.0, 0.001, MAT_OTHER, "Cu");
        assert!(
            (t - want).abs() < 1e-9,
            "Cu 1um at 10 keV: t={t}, want={want}"
        );
        // C's answer is a different number: it read the interval ABOVE 10 keV.
        assert!((t - 0.827582511947).abs() > 1e-3);
    }

    #[test]
    fn test_calc_blade_transmission_al() {
        let t = calc_blade_transmission(10.0, 1.0, MAT_AL, "");
        // 1mm Al at 10 keV should transmit a meaningful amount
        assert!(t > 0.0 && t < 1.0, "Al 1mm at 10keV: t={t}");
    }

    #[test]
    fn test_calc_blade_transmission_zero_thickness() {
        let t = calc_blade_transmission(10.0, 0.0, MAT_AL, "");
        assert_eq!(t, 1.0);
    }

    #[test]
    fn test_al_absorption_length_analytic() {
        // AlAbsorptionLength(10 keV): WCOEF0 poly at eV=10000 ~ 144.55 microns.
        let len = al_absorption_length_microns(10.0);
        assert!((144.0..145.0).contains(&len), "Al absLen at 10keV = {len}");
        // 60 keV cap: above 60 keV clamps to the 60 keV value.
        assert_eq!(
            al_absorption_length_microns(70.0),
            al_absorption_length_microns(60.0)
        );
    }

    #[test]
    fn test_ti_below_1kev_transmits_fully() {
        // TiAbsorptionLength returns 1/0 = +inf below 1 keV → exp(0) = 1.0.
        assert!(ti_absorption_length_microns(0.5).is_infinite());
        let t = calc_blade_transmission(0.5, 1.0, MAT_TI, "");
        assert_eq!(t, 1.0);
    }

    #[test]
    fn test_glass_below_2kev_absorbs_fully() {
        // GlassAbsorptionLength returns 0 below 2 keV → exp(-inf) = 0.0.
        assert_eq!(glass_absorption_length_microns(1.5), 0.0);
        let t = calc_blade_transmission(1.5, 1.0, MAT_GLASS, "");
        assert_eq!(t, 0.0);
    }

    #[test]
    fn test_glass_is_analytic_fit_not_silicon() {
        // GlassAbsorptionLength(10 keV) ~ 190 microns (above-Iron-K branch * keV^3),
        // distinct from the Si Chantler proxy the port previously used (~130 microns
        // → a ~10x smaller transmission for a 1 mm blade).
        let len = glass_absorption_length_microns(10.0);
        assert!(
            (185.0..195.0).contains(&len),
            "Glass absLen at 10keV = {len}"
        );
        let t = calc_blade_transmission(10.0, 1.0, MAT_GLASS, "");
        // exp(-1000/190.1) ~ 5.18e-3, well above the ~4.5e-4 a Si proxy would give.
        assert!((4.5e-3..6.0e-3).contains(&t), "Glass 1mm at 10keV = {t}");
    }

    #[test]
    fn test_recalc_filters_bank_off() {
        let cfg = Pf4BankConfig::default();
        let (xmit, bits) = recalc_filters(10.0, &cfg, false);
        for i in 0..NUM_COMBINATIONS {
            assert_eq!(xmit[i], 1.0);
            assert_eq!(bits[i], i as u8);
        }
    }

    #[test]
    fn test_recalc_filters_sorted() {
        let cfg = Pf4BankConfig {
            thicknesses: [0.5, 1.0, 2.0, 4.0], // mm Al
            material_indices: [0, 0, 0, 0],
            other_materials: Default::default(),
        };
        let (xmit, _bits) = recalc_filters(10.0, &cfg, true);

        // Should be sorted in decreasing order
        for i in 1..NUM_COMBINATIONS {
            assert!(
                xmit[i] <= xmit[i - 1] + 1e-15,
                "Not sorted at {i}: {} > {}",
                xmit[i],
                xmit[i - 1]
            );
        }

        // First entry should be highest (no filters = 1.0)
        assert!((xmit[0] - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_find_position() {
        let mut bits = [0u8; NUM_COMBINATIONS];
        for i in 0..NUM_COMBINATIONS {
            bits[i] = (15 - i) as u8; // Reversed order
        }
        assert_eq!(find_position(&bits, 15), 0);
        assert_eq!(find_position(&bits, 0), 15);
    }

    #[test]
    fn test_thickness_by_material() {
        let al = thickness_by_material(
            MAT_AL,
            true,
            [true, false, true, false],
            [MAT_AL, MAT_TI, MAT_AL, MAT_TI],
            [1.0, 2.0, 3.0, 4.0],
        );
        assert_eq!(al, 4.0); // 1.0 + 3.0

        let ti = thickness_by_material(
            MAT_TI,
            true,
            [true, false, true, false],
            [MAT_AL, MAT_TI, MAT_AL, MAT_TI],
            [1.0, 2.0, 3.0, 4.0],
        );
        assert_eq!(ti, 0.0); // b2 and b4 not inserted
    }

    #[test]
    fn test_controller_default() {
        let ctrl = Pf4Controller::default();
        assert_eq!(ctrl.state, Pf4State::Init);
        assert!(!ctrl.bank_on);
        assert_eq!(ctrl.filter_pos, 0);
    }

    #[test]
    fn test_controller_recalculate() {
        let mut ctrl = Pf4Controller::default();
        ctrl.bank_on = true;
        ctrl.energy_kev = 10.0;
        ctrl.bank_config.thicknesses = [0.5, 1.0, 2.0, 4.0];
        ctrl.bank_config.material_indices = [0, 0, 0, 0]; // All Al

        let actions = ctrl.recalculate();
        assert!(actions.write_transmission.is_some());
        assert!(actions.write_labels.is_some());

        // With no filters inserted, transmission should be 1.0
        assert!((ctrl.transmission - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_filter_thickness_writes_gated_by_material_and_bank() {
        // Bank on, all blades Al → only filterAl is posted; Ti/Glass left alone.
        let mut ctrl = Pf4Controller::default();
        ctrl.bank_on = true;
        ctrl.energy_kev = 10.0;
        ctrl.bank_config.thicknesses = [0.5, 1.0, 2.0, 4.0];
        ctrl.bank_config.material_indices = [MAT_AL, MAT_AL, MAT_AL, MAT_AL];
        let actions = ctrl.recalculate();
        assert!(actions.write_filter_al.is_some());
        assert!(actions.write_filter_ti.is_none());
        assert!(actions.write_filter_glass.is_none());

        // Bank off → none of the three are posted (no spurious 0.0 overwrite).
        ctrl.bank_on = false;
        let actions = ctrl.recalculate();
        assert!(actions.write_filter_al.is_none());
        assert!(actions.write_filter_ti.is_none());
        assert!(actions.write_filter_glass.is_none());
    }

    #[test]
    fn test_zero_transmission_skips_inv_write() {
        // A glass blade in beam below 2 keV gives transmission 0. C writes trans
        // but skips invtrans (pf4.st:282 `if(trans>0.0)`); we must do the same
        // rather than publishing +inf to {H}invTrans{B}.
        let mut ctrl = Pf4Controller::default();
        ctrl.bank_on = true;
        ctrl.use_mono = true;
        ctrl.mono_energy = 1.5;
        ctrl.local_energy = 1.5;
        ctrl.bank_config.thicknesses = [1.0, 0.0, 0.0, 0.0];
        ctrl.bank_config.material_indices = [MAT_GLASS, MAT_AL, MAT_AL, MAT_AL];
        ctrl.bit_states = [true, false, false, false];
        let actions = ctrl.recalculate();
        assert_eq!(ctrl.transmission, 0.0);
        assert_eq!(actions.write_transmission, Some(0.0));
        assert!(actions.write_inv_transmission.is_none());
    }

    #[test]
    fn test_controller_bits_changed() {
        let mut ctrl = Pf4Controller::default();
        ctrl.bank_on = true;
        ctrl.energy_kev = 10.0;
        ctrl.bank_config.thicknesses = [0.5, 1.0, 2.0, 4.0];
        ctrl.bank_config.material_indices = [0, 0, 0, 0];
        ctrl.recalculate();

        let actions = ctrl.step(Pf4Event::BitsChanged([true, false, false, false]));
        assert!(actions.write_transmission.is_some());
        // Transmission should be < 1.0 since bit 0 is inserted
        assert!(ctrl.transmission < 1.0);
    }

    #[test]
    fn test_controller_filter_pos_changed() {
        let mut ctrl = Pf4Controller::default();
        ctrl.bank_on = true;
        ctrl.energy_kev = 10.0;
        ctrl.bank_config.thicknesses = [0.5, 1.0, 2.0, 4.0];
        ctrl.bank_config.material_indices = [0, 0, 0, 0];
        ctrl.recalculate();

        // Position 0 is highest transmission (no filters)
        let actions = ctrl.step(Pf4Event::FilterPosChanged(0));
        assert!(actions.set_bits.is_some());
        let bits = actions.set_bits.unwrap();
        // Highest transmission = all filters out
        let pattern = bits_to_pattern(bits[0], bits[1], bits[2], bits[3]);
        assert_eq!(pattern, ctrl.bits[0]);
    }
}
