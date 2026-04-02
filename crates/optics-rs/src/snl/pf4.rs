//! XIA PF4 dual-filter bank state machine.
//!
//! Pure Rust port of `pf4.st` — manages two 4-bit filter banks where each bank
//! has 4 filter blades (16 combinations), using Chantler table data from
//! [`crate::data::chantler`].

use epics_base_rs::server::database::PvDatabase;

use crate::data::chantler::{find_material, transmission};
use crate::db_access::{DbChannel, DbMultiMonitor, alloc_origin};

/// Number of filter combinations per bank (4 bits = 16).
pub const NUM_COMBINATIONS: usize = 16;

/// Material index constants matching the SNL code.
pub const MAT_AL: u8 = 0;
pub const MAT_TI: u8 = 1;
pub const MAT_GLASS: u8 = 2;
pub const MAT_OTHER: u8 = 3;

/// Built-in material names for the fixed material indices.
#[allow(dead_code)]
const BUILTIN_MATERIALS: [&str; 3] = ["Al", "Ti", "Si"]; // Si used as proxy for borosilicate glass

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

/// Look up the material name for a given material index and "other" name.
fn material_name(mat_idx: u8, other_name: &str) -> &str {
    match mat_idx {
        0 => "Al",
        1 => "Ti",
        2 => "Si", // Glass approximated as Si for Chantler data
        3 => {
            if other_name.is_empty() {
                "Al" // fallback
            } else {
                other_name
            }
        }
        _ => "Al",
    }
}

/// Check whether an "Other" material name is legal (found in Chantler tables).
pub fn is_legal_other(name: &str) -> bool {
    find_material(name).is_some()
}

/// Calculate the transmission for a single filter blade.
///
/// `thickness_mm` is in millimetres. The material is looked up from Chantler data.
pub fn calc_blade_transmission(
    energy_kev: f64,
    thickness_mm: f64,
    mat_idx: u8,
    other_name: &str,
) -> f64 {
    if thickness_mm <= 0.0 || energy_kev <= 0.0 {
        return 1.0;
    }
    let name = material_name(mat_idx, other_name);
    match find_material(name) {
        Some(mat) => {
            // Convert mm to cm: 1 mm = 0.1 cm
            let thickness_cm = thickness_mm * 0.1;
            transmission(mat, energy_kev, thickness_cm).unwrap_or(1.0)
        }
        None => 1.0,
    }
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
        self.transmission = self.xmit[self.filter_pos];
        self.inv_transmission = if self.transmission > 0.0 {
            1.0 / self.transmission
        } else {
            f64::INFINITY
        };

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
        actions.write_transmission = Some(self.transmission);
        actions.write_inv_transmission = Some(self.inv_transmission);
        actions.write_filter_al = Some(self.filter_al);
        actions.write_filter_ti = Some(self.filter_ti);
        actions.write_filter_glass = Some(self.filter_glass);

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
                self.transmission = self.xmit[self.filter_pos];
                self.inv_transmission = if self.transmission > 0.0 {
                    1.0 / self.transmission
                } else {
                    f64::INFINITY
                };
                actions.write_transmission = Some(self.transmission);
                actions.write_inv_transmission = Some(self.inv_transmission);
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
                    self.transmission = self.xmit[pos];
                    self.inv_transmission = if self.transmission > 0.0 {
                        1.0 / self.transmission
                    } else {
                        f64::INFINITY
                    };
                    actions.write_transmission = Some(self.transmission);
                    actions.write_inv_transmission = Some(self.inv_transmission);
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
    let ch_b1 = DbChannel::new(&db, &format!("{ph}displayBit1{b}"));
    let ch_b2 = DbChannel::new(&db, &format!("{ph}displayBit2{b}"));
    let ch_b3 = DbChannel::new(&db, &format!("{ph}displayBit3{b}"));
    let ch_b4 = DbChannel::new(&db, &format!("{ph}displayBit4{b}"));
    let ch_trans = DbChannel::new(&db, &format!("{ph}trans{b}"));
    let ch_inv_trans = DbChannel::new(&db, &format!("{ph}invTrans{b}"));
    let ch_bankctl = DbChannel::new(&db, &format!("{ph}bank{b}"));
    let _ch_filpos = DbChannel::new(&db, &format!("{ph}fPos{b}"));
    let ch_select_energy = DbChannel::new(&db, &format!("{ph}useMono"));
    let ch_local_energy = DbChannel::new(&db, &format!("{ph}E:local"));
    let ch_filter_al = DbChannel::new(&db, &format!("{ph}filterAl"));
    let ch_filter_ti = DbChannel::new(&db, &format!("{ph}filterTi"));
    let ch_filter_glass = DbChannel::new(&db, &format!("{ph}filterGlass"));

    let ch_f1 = DbChannel::new(&db, &format!("{ph}f1{b}"));
    let ch_f2 = DbChannel::new(&db, &format!("{ph}f2{b}"));
    let ch_f3 = DbChannel::new(&db, &format!("{ph}f3{b}"));
    let ch_f4 = DbChannel::new(&db, &format!("{ph}f4{b}"));

    let ch_z1 = DbChannel::new(&db, &format!("{ph}Z1{b}"));
    let ch_z2 = DbChannel::new(&db, &format!("{ph}Z2{b}"));
    let ch_z3 = DbChannel::new(&db, &format!("{ph}Z3{b}"));
    let ch_z4 = DbChannel::new(&db, &format!("{ph}Z4{b}"));

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
                    let _ = ch_b1.put_i16(1_i16).await;
                }
                if bits[1] {
                    let _ = ch_b2.put_i16(1_i16).await;
                }
                if bits[2] {
                    let _ = ch_b3.put_i16(1_i16).await;
                }
                if bits[3] {
                    let _ = ch_b4.put_i16(1_i16).await;
                }
                sleep(Duration::from_millis(200)).await;
                if !bits[0] {
                    let _ = ch_b1.put_i16(0_i16).await;
                }
                if !bits[1] {
                    let _ = ch_b2.put_i16(0_i16).await;
                }
                if !bits[2] {
                    let _ = ch_b3.put_i16(0_i16).await;
                }
                if !bits[3] {
                    let _ = ch_b4.put_i16(0_i16).await;
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
        let _ = ch_trans.put_f64_post(t).await;
    }
    if let Some(t) = actions.write_inv_transmission {
        let _ = ch_inv_trans.put_f64_post(t).await;
    }
    if let Some(t) = actions.write_filter_al {
        let _ = ch_filter_al.put_f64_post(t).await;
    }
    if let Some(t) = actions.write_filter_ti {
        let _ = ch_filter_ti.put_f64_post(t).await;
    }
    if let Some(t) = actions.write_filter_glass {
        let _ = ch_filter_glass.put_f64_post(t).await;
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
