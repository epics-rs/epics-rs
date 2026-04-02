//! Automatic filter selection state machine.
//!
//! Pure Rust port of `filterDrive.st` — drives a set of beamline filters to
//! achieve a requested transmission, using Chantler table data from
//! [`crate::data::chantler`].

use epics_base_rs::server::database::PvDatabase;

use crate::data::chantler::{find_material, transmission};
use crate::db_access::{DbChannel, DbMultiMonitor, alloc_origin};

/// Maximum number of filters supported.
pub const MAX_FILTERS: usize = 32;

/// Maximum number of unlocked+enabled filters for permutation enumeration.
pub const MAX_ENABLED: usize = 16;

/// Threshold for floating-point "same transmission" comparison.
const SMALL_FRAC: f64 = 1.0e-8;

/// A single filter blade configuration.
#[derive(Debug, Clone)]
pub struct FilterBlade {
    /// Material name (e.g. "Al", "Cu").
    pub material: String,
    /// Thickness in micrometres.
    pub thickness: f64,
    /// Whether this blade is enabled (participates in calculations).
    pub enabled: bool,
    /// Whether this blade is locked in its current position.
    pub locked: bool,
    /// Current state: true = inserted in beam, false = removed.
    pub inserted: bool,
    /// Per-blade transmission at current energy.
    pub transmission: f64,
}

impl Default for FilterBlade {
    fn default() -> Self {
        Self {
            material: "Al".into(),
            thickness: 0.0,
            enabled: true,
            locked: false,
            inserted: false,
            transmission: 1.0,
        }
    }
}

/// State of the filter drive state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterDriveState {
    Init,
    Idle,
    UpdateFilters,
    Action,
    ChangeFilters,
}

/// Actions that can be requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterAction {
    /// Go to the transmission setpoint.
    SetTransmission,
    /// Step to next higher transmission.
    StepUp,
    /// Step to next lower transmission.
    StepDown,
    /// Go to a specific filter mask.
    SetFilterMask,
}

/// Configuration for the filter_drive state machine.
#[derive(Debug, Clone)]
pub struct FilterDriveConfig {
    pub prefix: String,
    pub record: String,
    pub num_filters: usize,
}

impl FilterDriveConfig {
    pub fn new(p: &str, r: &str, n: usize) -> Self {
        Self {
            prefix: p.to_string(),
            record: r.to_string(),
            num_filters: n,
        }
    }

    /// PV name for filter blade `i` (1-indexed), field `f`.
    pub fn blade_pv(&self, i: usize, field: &str) -> String {
        format!("{}{}Fi{}:{}", self.prefix, self.record, i, field)
    }
}

/// One entry in the permutation table: filter mask and total transmission.
#[derive(Debug, Clone, Copy)]
pub struct Permutation {
    /// Bitmask of which filters are inserted.
    pub mask: u32,
    /// Total transmission for this combination.
    pub transmission: f64,
}

/// Precomputed filter permutation table.
#[derive(Debug, Clone)]
pub struct PermutationTable {
    pub entries: Vec<Permutation>,
    /// Index of current filter combination.
    pub current_idx: Option<usize>,
    /// Index of next-higher transmission.
    pub up_idx: Option<usize>,
    /// Index of next-lower transmission.
    pub down_idx: Option<usize>,
}

/// Calculate per-blade transmission values.
pub fn calc_blade_transmissions(blades: &mut [FilterBlade], energy_kev: f64) {
    for blade in blades.iter_mut() {
        blade.transmission = calc_single_transmission(&blade.material, energy_kev, blade.thickness);
    }
}

/// Calculate transmission for a single blade.
pub fn calc_single_transmission(material: &str, energy_kev: f64, thickness_um: f64) -> f64 {
    match find_material(material) {
        Some(mat) => {
            let thickness_cm = thickness_um * 1.0e-4; // um -> cm
            transmission(mat, energy_kev, thickness_cm).unwrap_or(0.0)
        }
        None => 0.0,
    }
}

/// Build the permutation table for all combinations of unlocked+enabled filters.
///
/// Locked+enabled+inserted filters contribute their transmission to every
/// permutation. Disabled filters are completely ignored.
pub fn build_permutation_table(blades: &[FilterBlade]) -> PermutationTable {
    let _n_filters = blades.len();

    // Identify unlocked+enabled filter indices
    let mut free_indices: Vec<usize> = Vec::new();
    for (i, b) in blades.iter().enumerate() {
        if b.enabled && !b.locked {
            free_indices.push(i);
        }
    }

    let n_enabled = free_indices.len().min(MAX_ENABLED);
    let n_perms = 1_usize << n_enabled;

    // Base transmission from locked+inserted+enabled filters
    let mut base_trans = 1.0;
    let mut base_mask: u32 = 0;
    for (i, b) in blades.iter().enumerate() {
        if b.enabled && b.locked && b.inserted {
            base_trans *= b.transmission;
            base_mask |= 1 << i;
        }
    }

    let mut entries = Vec::with_capacity(n_perms);

    for perm in 0..n_perms {
        let mut trans = base_trans;
        let mut mask = base_mask;

        for (k, &fi) in free_indices.iter().enumerate().take(n_enabled) {
            if perm & (1 << k) != 0 {
                trans *= blades[fi].transmission;
                mask |= 1 << fi;
            }
        }

        entries.push(Permutation {
            mask,
            transmission: trans,
        });
    }

    PermutationTable {
        entries,
        current_idx: None,
        up_idx: None,
        down_idx: None,
    }
}

/// Find the current filter combination in the permutation table.
pub fn find_current_index(table: &[Permutation], current_mask: u32) -> Option<usize> {
    table.iter().position(|p| p.mask == current_mask)
}

/// Update the "step up" and "step down" indices based on the current transmission.
pub fn update_step_indices(
    table: &[Permutation],
    current_trans: f64,
) -> (Option<usize>, f64, Option<usize>, f64) {
    let mut up_idx: Option<usize> = None;
    let mut trans_up = 10.0;

    let mut down_idx: Option<usize> = None;
    let mut trans_down = -1.0;

    for (i, p) in table.iter().enumerate() {
        // Step up: smallest transmission > current
        if p.transmission > current_trans * (1.0 + SMALL_FRAC) && p.transmission < trans_up {
            trans_up = p.transmission;
            up_idx = Some(i);
        }
        // Step down: largest transmission < current
        if p.transmission < current_trans * (1.0 - SMALL_FRAC) && p.transmission > trans_down {
            trans_down = p.transmission;
            down_idx = Some(i);
        }
    }

    // Clamp if no valid step exists
    if trans_up > 1.0 {
        trans_up = current_trans;
        up_idx = None;
    }
    if trans_down < 0.0 {
        trans_down = current_trans;
        down_idx = None;
    }

    (up_idx, trans_up, down_idx, trans_down)
}

/// Find the best permutation index for a given transmission setpoint.
///
/// Selects the highest transmission that is <= the setpoint.
/// If all are above the setpoint, returns the minimum transmission.
pub fn find_best_for_setpoint(table: &[Permutation], setpoint: f64) -> Option<usize> {
    let mut best_idx: Option<usize> = None;
    let mut best_trans = -1.0;
    let mut min_idx: Option<usize> = None;
    let mut min_trans = f64::MAX;

    for (i, p) in table.iter().enumerate() {
        if p.transmission <= setpoint && p.transmission > best_trans {
            best_trans = p.transmission;
            best_idx = Some(i);
        }
        if p.transmission < min_trans {
            min_trans = p.transmission;
            min_idx = Some(i);
        }
    }

    if best_trans < min_trans {
        min_idx
    } else {
        best_idx
    }
}

/// Find the permutation index that matches a given filter mask.
pub fn find_mask_index(table: &[Permutation], mask: u32) -> Option<usize> {
    table.iter().position(|p| p.mask == mask)
}

/// Build the current filter mask from blade states.
pub fn current_mask(blades: &[FilterBlade]) -> u32 {
    let mut mask: u32 = 0;
    for (i, b) in blades.iter().enumerate() {
        if b.inserted && b.enabled {
            mask |= 1 << i;
        }
    }
    mask
}

/// Calculate the total transmission for the current blade configuration.
pub fn current_transmission(blades: &[FilterBlade]) -> f64 {
    let mut trans = 1.0;
    for b in blades {
        if b.inserted && b.enabled {
            trans *= b.transmission;
        }
    }
    trans
}

/// Main filter-drive controller.
#[derive(Debug, Clone)]
pub struct FilterDriveController {
    pub state: FilterDriveState,
    pub blades: Vec<FilterBlade>,
    pub energy_kev: f64,
    pub transmission: f64,
    pub transmission_up: f64,
    pub transmission_down: f64,
    pub filter_mask: u32,
    pub perm_table: PermutationTable,
    pub busy: bool,
    pub message: String,
}

impl FilterDriveController {
    pub fn new(num_filters: usize) -> Self {
        Self {
            state: FilterDriveState::Init,
            blades: vec![FilterBlade::default(); num_filters],
            energy_kev: 10.0,
            transmission: 1.0,
            transmission_up: 1.0,
            transmission_down: 1.0,
            filter_mask: 0,
            perm_table: PermutationTable {
                entries: Vec::new(),
                current_idx: None,
                up_idx: None,
                down_idx: None,
            },
            busy: false,
            message: String::new(),
        }
    }

    /// Recalculate all blade transmissions and rebuild the permutation table.
    pub fn recalculate(&mut self) {
        calc_blade_transmissions(&mut self.blades, self.energy_kev);
        self.perm_table = build_permutation_table(&self.blades);

        self.filter_mask = current_mask(&self.blades);
        self.transmission = current_transmission(&self.blades);

        self.perm_table.current_idx =
            find_current_index(&self.perm_table.entries, self.filter_mask);

        let (up, tu, down, td) = update_step_indices(&self.perm_table.entries, self.transmission);
        self.perm_table.up_idx = up;
        self.transmission_up = tu;
        self.perm_table.down_idx = down;
        self.transmission_down = td;
    }

    /// Apply a target permutation index: set blade inserted states.
    /// Returns the list of (blade_index, inserted) pairs that changed.
    pub fn apply_permutation(&mut self, perm_idx: usize) -> Vec<(usize, bool)> {
        let mask = self.perm_table.entries[perm_idx].mask;
        let mut changes = Vec::new();
        for (i, b) in self.blades.iter_mut().enumerate() {
            if b.enabled {
                let should_insert = mask & (1 << i) != 0;
                if b.inserted != should_insert {
                    b.inserted = should_insert;
                    changes.push((i, should_insert));
                }
            }
        }
        changes
    }

    /// Execute a filter action: returns blade changes to apply.
    pub fn execute_action(
        &mut self,
        action: FilterAction,
        setpoint: f64,
        mask_setpoint: u32,
    ) -> Vec<(usize, bool)> {
        let target_idx = match action {
            FilterAction::SetTransmission => {
                find_best_for_setpoint(&self.perm_table.entries, setpoint)
            }
            FilterAction::StepUp => self.perm_table.up_idx,
            FilterAction::StepDown => self.perm_table.down_idx,
            FilterAction::SetFilterMask => {
                // Adjust mask for locked filters
                let mut adjusted = 0u32;
                for (i, b) in self.blades.iter().enumerate() {
                    if b.enabled {
                        if b.locked {
                            if b.inserted {
                                adjusted |= 1 << i;
                            }
                        } else {
                            adjusted |= mask_setpoint & (1 << i);
                        }
                    }
                }
                find_mask_index(&self.perm_table.entries, adjusted)
            }
        };

        match target_idx {
            Some(idx) => {
                let trans = self.perm_table.entries[idx].transmission;
                if (trans - self.transmission).abs() < SMALL_FRAC {
                    self.message = "OK - No change".into();
                    Vec::new()
                } else {
                    self.message = "OK".into();
                    self.apply_permutation(idx)
                }
            }
            None => {
                self.message = match action {
                    FilterAction::StepUp => "NO CHANGE! Step unavailable.".into(),
                    FilterAction::StepDown => "NO CHANGE! Step unavailable.".into(),
                    FilterAction::SetFilterMask => "ERROR! Filter Mask not found.".into(),
                    _ => "No matching filter combination found.".into(),
                };
                Vec::new()
            }
        }
    }
}

/// Events that drive the filter state machine.
#[derive(Debug, Clone)]
pub enum FilterDriveEvent {
    EnergyChanged(f64),
    ConfigChanged,
    TransmissionSetpoint(f64),
    StepUp,
    StepDown,
    FilterMaskSetpoint(u32),
    ExternalIO,
}

/// Async entry point — runs the filter_drive state machine against live PVs.
pub async fn run(
    config: FilterDriveConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::time::Duration;

    tokio::time::sleep(Duration::from_secs(3)).await;
    println!(
        "filterDrive: starting for prefix={}{}",
        config.prefix, config.record
    );

    let my_origin = alloc_origin();
    let pr = format!("{}{}", config.prefix, config.record);
    let n = config.num_filters;

    // Connect global PVs
    let ch_status = DbChannel::new(&db, &format!("{pr}Status"));
    let ch_trans = DbChannel::new(&db, &format!("{pr}Transmission"));
    let ch_trans_up = DbChannel::new(&db, &format!("{pr}TransmissionUp"));
    let ch_trans_down = DbChannel::new(&db, &format!("{pr}TransmissionDown"));
    let ch_mask = DbChannel::new(&db, &format!("{pr}FilterMask"));
    let ch_energy = DbChannel::new(&db, &format!("{pr}Energy"));
    let ch_msg = DbChannel::new(&db, &format!("{pr}Message"));
    let _ch_setpt = DbChannel::new(&db, &format!("{pr}TransmissionSetpoint"));
    let ch_step_up = DbChannel::new(&db, &format!("{pr}TransmissionStepUp"));
    let ch_step_down = DbChannel::new(&db, &format!("{pr}TransmissionStepDown"));
    let _ch_mask_setpt = DbChannel::new(&db, &format!("{pr}FilterMaskSetpoint"));
    let ch_wait_time = DbChannel::new(&db, &format!("{pr}WaitTime"));

    // Per-blade channels
    let mut ch_thick: Vec<DbChannel> = Vec::new();
    let mut ch_mater: Vec<DbChannel> = Vec::new();
    let mut ch_blade_trans: Vec<DbChannel> = Vec::new();
    let mut ch_set: Vec<DbChannel> = Vec::new();
    let mut ch_outget: Vec<DbChannel> = Vec::new();
    let mut ch_lock: Vec<DbChannel> = Vec::new();
    let mut ch_enbl: Vec<DbChannel> = Vec::new();

    for i in 1..=n {
        ch_thick.push(DbChannel::new(&db, &config.blade_pv(i, "Thickness")));
        ch_mater.push(DbChannel::new(&db, &config.blade_pv(i, "Material")));
        ch_blade_trans.push(DbChannel::new(&db, &config.blade_pv(i, "Transmission")));
        ch_set.push(DbChannel::new(&db, &config.blade_pv(i, "Set")));
        ch_outget.push(DbChannel::new(&db, &config.blade_pv(i, "OutGet")));
        ch_lock.push(DbChannel::new(&db, &config.blade_pv(i, "Lock")));
        ch_enbl.push(DbChannel::new(&db, &config.blade_pv(i, "Enable")));
    }

    // Build multi-monitor
    let monitored_pvs: Vec<String> = vec![
        format!("{pr}Energy"),
        format!("{pr}TransmissionSetpoint"),
        format!("{pr}TransmissionStepUp"),
        format!("{pr}TransmissionStepDown"),
        format!("{pr}FilterMaskSetpoint"),
    ];
    let mut monitor = DbMultiMonitor::new_filtered(&db, &monitored_pvs, my_origin).await;

    // Initialize
    let mut ctrl = FilterDriveController::new(n);
    ctrl.energy_kev = {
        let v = ch_energy.get_f64().await;
        if v > 0.0 { v } else { 10.0 }
    };

    // Read initial blade configuration
    for i in 0..n {
        ctrl.blades[i].thickness = ch_thick[i].get_f64().await;
        let mat = ch_mater[i].get_string().await;
        ctrl.blades[i].material = if mat.is_empty() { "Al".into() } else { mat };
        ctrl.blades[i].locked = ch_lock[i].get_i16().await as i32 != 0;
        ctrl.blades[i].enabled = ch_enbl[i].get_i16().await as i32 != 0;
        ctrl.blades[i].inserted = ch_outget[i].get_i16().await as i32 != 0;
    }

    ctrl.recalculate();

    // Publish initial values
    let _ = ch_trans.put_f64(ctrl.transmission).await;
    let _ = ch_trans_up.put_f64(ctrl.transmission_up).await;
    let _ = ch_trans_down.put_f64(ctrl.transmission_down).await;
    let _ = ch_mask.put_i16(ctrl.filter_mask as i32 as i16).await;
    for (ch_bt, blade) in ch_blade_trans.iter().zip(ctrl.blades.iter()).take(n) {
        let _ = ch_bt.put_f64(blade.transmission).await;
    }
    let _ = ch_msg.put_string("Initialised").await;
    let _ = ch_status.put_i16(0_i16).await;

    tracing::info!("filter_drive state machine running for {pr}");

    let pv_energy = format!("{pr}Energy");
    let pv_setpt = format!("{pr}TransmissionSetpoint");
    let pv_step_up = format!("{pr}TransmissionStepUp");
    let pv_step_down = format!("{pr}TransmissionStepDown");
    let pv_mask_setpt = format!("{pr}FilterMaskSetpoint");

    loop {
        let (changed_pv, new_val) = monitor.wait_change().await;

        let event: Option<FilterDriveEvent> = if changed_pv == pv_energy {
            Some(FilterDriveEvent::EnergyChanged(new_val))
        } else if changed_pv == pv_setpt {
            Some(FilterDriveEvent::TransmissionSetpoint(new_val))
        } else if changed_pv == pv_step_up {
            if new_val as i32 != 0 {
                Some(FilterDriveEvent::StepUp)
            } else {
                None
            }
        } else if changed_pv == pv_step_down {
            if new_val as i32 != 0 {
                Some(FilterDriveEvent::StepDown)
            } else {
                None
            }
        } else if changed_pv == pv_mask_setpt {
            Some(FilterDriveEvent::FilterMaskSetpoint(new_val as u32))
        } else {
            None
        };

        if let Some(ev) = event {
            match ev {
                FilterDriveEvent::EnergyChanged(e) => {
                    ctrl.energy_kev = e;
                    ctrl.recalculate();
                }

                FilterDriveEvent::ConfigChanged | FilterDriveEvent::ExternalIO => {
                    // Re-read blade states
                    for (blade, ch_og) in ctrl.blades.iter_mut().zip(ch_outget.iter()).take(n) {
                        blade.inserted = ch_og.get_i16().await as i32 != 0;
                    }
                    ctrl.recalculate();
                }

                FilterDriveEvent::TransmissionSetpoint(setpt) => {
                    let _ = ch_status.put_i16(1_i16).await;
                    let changes = ctrl.execute_action(FilterAction::SetTransmission, setpt, 0);
                    let wait_time = {
                        let v = ch_wait_time.get_f64().await;
                        if v > 0.0 { v } else { 0.5 }
                    };
                    apply_filter_changes(&ch_set, &changes, wait_time).await;
                    ctrl.recalculate();
                    let _ = ch_status.put_i16(0_i16).await;
                }

                FilterDriveEvent::StepUp => {
                    let _ = ch_step_up.put_i16(0_i16).await;
                    let _ = ch_status.put_i16(1_i16).await;
                    let changes = ctrl.execute_action(FilterAction::StepUp, 0.0, 0);
                    let wait_time = {
                        let v = ch_wait_time.get_f64().await;
                        if v > 0.0 { v } else { 0.5 }
                    };
                    apply_filter_changes(&ch_set, &changes, wait_time).await;
                    ctrl.recalculate();
                    let _ = ch_status.put_i16(0_i16).await;
                }

                FilterDriveEvent::StepDown => {
                    let _ = ch_step_down.put_i16(0_i16).await;
                    let _ = ch_status.put_i16(1_i16).await;
                    let changes = ctrl.execute_action(FilterAction::StepDown, 0.0, 0);
                    let wait_time = {
                        let v = ch_wait_time.get_f64().await;
                        if v > 0.0 { v } else { 0.5 }
                    };
                    apply_filter_changes(&ch_set, &changes, wait_time).await;
                    ctrl.recalculate();
                    let _ = ch_status.put_i16(0_i16).await;
                }

                FilterDriveEvent::FilterMaskSetpoint(mask) => {
                    let _ = ch_status.put_i16(1_i16).await;
                    let changes = ctrl.execute_action(FilterAction::SetFilterMask, 0.0, mask);
                    let wait_time = {
                        let v = ch_wait_time.get_f64().await;
                        if v > 0.0 { v } else { 0.5 }
                    };
                    apply_filter_changes(&ch_set, &changes, wait_time).await;
                    ctrl.recalculate();
                    let _ = ch_status.put_i16(0_i16).await;
                }
            }

            // Publish updated values
            let _ = ch_trans.put_f64(ctrl.transmission).await;
            let _ = ch_trans_up.put_f64(ctrl.transmission_up).await;
            let _ = ch_trans_down.put_f64(ctrl.transmission_down).await;
            let _ = ch_mask.put_i16(ctrl.filter_mask as i32 as i16).await;
            let _ = ch_msg.put_string(ctrl.message.as_str()).await;
            for (ch_bt, blade) in ch_blade_trans.iter().zip(ctrl.blades.iter()).take(n) {
                let _ = ch_bt.put_f64(blade.transmission).await;
            }
        }
    }
}

/// Apply filter blade changes: first insert, wait, then remove.
async fn apply_filter_changes(ch_set: &[DbChannel], changes: &[(usize, bool)], wait_time: f64) {
    use tokio::time::{Duration, sleep};

    // Phase 1: insert filters
    let mut any_insert = false;
    for &(idx, inserted) in changes {
        if inserted {
            let _ = ch_set[idx].put_i16(1).await;
            any_insert = true;
        }
    }
    if any_insert {
        sleep(Duration::from_secs_f64(wait_time)).await;
    }

    // Phase 2: remove filters
    let mut any_remove = false;
    for &(idx, inserted) in changes {
        if !inserted {
            let _ = ch_set[idx].put_i16(0).await;
            any_remove = true;
        }
    }
    if any_remove {
        sleep(Duration::from_secs_f64(wait_time)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calc_single_transmission_al() {
        let t = calc_single_transmission("Al", 10.0, 100.0);
        // 100 um Al at 10 keV should have high transmission
        assert!(t > 0.0 && t < 1.0, "Al transmission at 10keV = {t}");
    }

    #[test]
    fn test_calc_single_transmission_unknown() {
        let t = calc_single_transmission("Unobtainium", 10.0, 100.0);
        assert_eq!(t, 0.0);
    }

    #[test]
    fn test_build_permutation_table_no_filters() {
        let blades: Vec<FilterBlade> = Vec::new();
        let table = build_permutation_table(&blades);
        assert_eq!(table.entries.len(), 1); // Only the "all out" permutation
        assert!((table.entries[0].transmission - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_build_permutation_table_two_filters() {
        let blades = vec![
            FilterBlade {
                material: "Al".into(),
                thickness: 100.0,
                enabled: true,
                locked: false,
                inserted: false,
                transmission: 0.5,
            },
            FilterBlade {
                material: "Cu".into(),
                thickness: 50.0,
                enabled: true,
                locked: false,
                inserted: false,
                transmission: 0.3,
            },
        ];

        let table = build_permutation_table(&blades);
        assert_eq!(table.entries.len(), 4); // 2^2 = 4 combinations

        // Check that mask=0 gives transmission=1.0
        let none = table.entries.iter().find(|p| p.mask == 0).unwrap();
        assert!((none.transmission - 1.0).abs() < 1e-10);

        // Check that mask=3 gives transmission=0.5*0.3=0.15
        let both = table.entries.iter().find(|p| p.mask == 3).unwrap();
        assert!((both.transmission - 0.15).abs() < 1e-10);
    }

    #[test]
    fn test_build_permutation_table_locked_filter() {
        let blades = vec![
            FilterBlade {
                material: "Al".into(),
                thickness: 100.0,
                enabled: true,
                locked: true,
                inserted: true, // Locked in beam
                transmission: 0.5,
            },
            FilterBlade {
                material: "Cu".into(),
                thickness: 50.0,
                enabled: true,
                locked: false,
                inserted: false,
                transmission: 0.3,
            },
        ];

        let table = build_permutation_table(&blades);
        // Only 1 free filter => 2 permutations
        assert_eq!(table.entries.len(), 2);

        // Both should include the locked filter's contribution
        for p in &table.entries {
            assert!(p.mask & 1 != 0, "locked filter should always be in mask");
        }
    }

    #[test]
    fn test_build_permutation_table_disabled_filter() {
        let blades = vec![
            FilterBlade {
                material: "Al".into(),
                thickness: 100.0,
                enabled: false, // Disabled
                locked: false,
                inserted: false,
                transmission: 0.5,
            },
            FilterBlade {
                material: "Cu".into(),
                thickness: 50.0,
                enabled: true,
                locked: false,
                inserted: false,
                transmission: 0.3,
            },
        ];

        let table = build_permutation_table(&blades);
        // Only 1 enabled filter => 2 permutations
        assert_eq!(table.entries.len(), 2);
    }

    #[test]
    fn test_find_best_for_setpoint() {
        let entries = vec![
            Permutation {
                mask: 0,
                transmission: 1.0,
            },
            Permutation {
                mask: 1,
                transmission: 0.5,
            },
            Permutation {
                mask: 2,
                transmission: 0.3,
            },
            Permutation {
                mask: 3,
                transmission: 0.15,
            },
        ];

        // Setpoint 0.4 -> should pick 0.3 (highest <= 0.4)
        let idx = find_best_for_setpoint(&entries, 0.4).unwrap();
        assert_eq!(entries[idx].transmission, 0.3);

        // Setpoint 1.0 -> should pick 1.0
        let idx = find_best_for_setpoint(&entries, 1.0).unwrap();
        assert_eq!(entries[idx].transmission, 1.0);

        // Setpoint 0.1 -> should pick 0.15 (minimum, since no entry <= 0.1 other than 0.15)
        let idx = find_best_for_setpoint(&entries, 0.1).unwrap();
        // 0.15 > 0.1, so no entry <= setpoint exists. Return minimum.
        assert_eq!(entries[idx].transmission, 0.15);
    }

    #[test]
    fn test_update_step_indices() {
        let entries = vec![
            Permutation {
                mask: 0,
                transmission: 1.0,
            },
            Permutation {
                mask: 1,
                transmission: 0.5,
            },
            Permutation {
                mask: 2,
                transmission: 0.3,
            },
            Permutation {
                mask: 3,
                transmission: 0.15,
            },
        ];

        let (up, tu, down, td) = update_step_indices(&entries, 0.5);
        assert!(up.is_some());
        assert!((tu - 1.0).abs() < 1e-10);
        assert!(down.is_some());
        assert!((td - 0.3).abs() < 1e-10);
    }

    #[test]
    fn test_step_up_at_max_returns_none() {
        let entries = vec![
            Permutation {
                mask: 0,
                transmission: 1.0,
            },
            Permutation {
                mask: 1,
                transmission: 0.5,
            },
        ];

        let (up, tu, _, _) = update_step_indices(&entries, 1.0);
        assert!(up.is_none());
        assert!((tu - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_step_down_at_min_returns_none() {
        let entries = vec![
            Permutation {
                mask: 0,
                transmission: 1.0,
            },
            Permutation {
                mask: 1,
                transmission: 0.5,
            },
        ];

        let (_, _, down, td) = update_step_indices(&entries, 0.5);
        assert!(down.is_none());
        assert!((td - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_controller_recalculate() {
        let mut ctrl = FilterDriveController::new(2);
        ctrl.energy_kev = 10.0;
        ctrl.blades[0] = FilterBlade {
            material: "Al".into(),
            thickness: 100.0,
            enabled: true,
            locked: false,
            inserted: false,
            transmission: 1.0,
        };
        ctrl.blades[1] = FilterBlade {
            material: "Cu".into(),
            thickness: 50.0,
            enabled: true,
            locked: false,
            inserted: false,
            transmission: 1.0,
        };

        ctrl.recalculate();
        assert_eq!(ctrl.perm_table.entries.len(), 4);
        assert!((ctrl.transmission - 1.0).abs() < 1e-10); // Nothing inserted
    }

    #[test]
    fn test_controller_execute_setpoint() {
        let mut ctrl = FilterDriveController::new(2);
        ctrl.energy_kev = 10.0;
        ctrl.blades[0].transmission = 0.5;
        ctrl.blades[1].transmission = 0.3;
        ctrl.perm_table = build_permutation_table(&ctrl.blades);
        ctrl.transmission = 1.0;
        ctrl.filter_mask = 0;

        let changes = ctrl.execute_action(FilterAction::SetTransmission, 0.4, 0);
        // Should try to reach 0.3 (highest <= 0.4)
        assert!(!changes.is_empty() || ctrl.message.contains("No change"));
    }

    #[test]
    fn test_current_mask() {
        let blades = vec![
            FilterBlade {
                inserted: true,
                enabled: true,
                ..Default::default()
            },
            FilterBlade {
                inserted: false,
                enabled: true,
                ..Default::default()
            },
            FilterBlade {
                inserted: true,
                enabled: false,
                ..Default::default()
            },
        ];
        assert_eq!(current_mask(&blades), 0b01);
    }
}
