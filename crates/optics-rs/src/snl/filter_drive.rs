//! Automatic filter selection state machine.
//!
//! Port of `filterDrive.st` — drives a set of beamline filters to achieve a
//! requested transmission, using Chantler table data from
//! [`crate::data::chantler`]. The runner walks C's states (`start`, `init`,
//! `waiting`, `update_filters`, `action`, `change_filters`) in order;
//! [`FilterDrive`] holds C's program variables and does the arithmetic.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;

use crate::data::chantler::{find_material, transmission};
use crate::db_access::DbMultiMonitor;
use epics_base_rs::server::snl::{PutQueue, SeqPv};

/// Maximum number of filters supported.
pub const MAX_FILTERS: usize = 32;

/// Maximum number of unlocked+enabled filters for permutation enumeration.
pub const MAX_ENABLED: usize = 16;

/// Threshold for floating-point "same transmission" comparison.
const SMALL_FRAC: f64 = 1.0e-8;

/// Configuration for the filter_drive state machine.
#[derive(Debug, Clone)]
pub struct FilterDriveConfig {
    /// The `NAME` macro, which prefixes C's console lines.
    pub name: String,
    pub prefix: String,
    pub record: String,
    pub num_filters: usize,
}

impl FilterDriveConfig {
    pub fn new(p: &str, r: &str, n: usize) -> Self {
        Self {
            name: "filterDrive".to_string(),
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

/// C `calcTrans` (`filterDrive.st:270-310`): transmission of `thickness_um`
/// of `material` at `energy_kev`, 0 for an unknown material or energy.
pub fn calc_single_transmission(material: &str, energy_kev: f64, thickness_um: f64) -> f64 {
    match find_material(material) {
        Some(mat) => transmission(mat, energy_kev, thickness_um).unwrap_or(0.0),
        None => 0.0,
    }
}

/// One blade's slot in C's `THICK`/`MATER`/`LOCK`/`SET`/`ENBL`/`TRANSM`
/// arrays.
#[derive(Debug, Clone, Default)]
pub struct Blade {
    pub thick: f64,
    pub mater: String,
    pub lock: i32,
    pub set: i32,
    pub enbl: i32,
    pub transm: f64,
}

/// The puts `update_filters` makes after its blade sync, in C's order.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdatePuts {
    pub transmission: f64,
    pub filter_mask: i32,
    /// `ERROR: Too many unlocked`, put between the mask and the step values.
    pub too_many: Option<String>,
    pub transmission_up: f64,
    pub transmission_down: f64,
    /// `STATUS_BUSY = 0`, put only when `changeFlag` was set.
    pub clear_busy: bool,
}

/// C's program variables that outlive one state (`filterDrive.st:194-229`).
/// SNL zeroes them at start, and the indices keep their last value across
/// states as C's do.
#[derive(Debug, Clone)]
pub struct FilterDrive {
    pub change_flag: bool,
    pub msge: String,
    pub n_perms: usize,
    pub ind: i32,
    pub ind_now: i32,
    pub ind_up: i32,
    pub ind_down: i32,
    pub ind_min: i32,
    pub trans_now: f64,
    pub trans_up: f64,
    pub trans_down: f64,
    /// `transMis`/`fltrsIn`: only the first `n_perms` are rewritten, so an
    /// index left from a larger table reads what that table wrote, as in C.
    pub trans_mis: Vec<f64>,
    pub fltrs_in: Vec<i32>,
}

impl Default for FilterDrive {
    fn default() -> Self {
        Self {
            change_flag: false,
            msge: String::new(),
            n_perms: 0,
            ind: 0,
            ind_now: 0,
            ind_up: 0,
            ind_down: 0,
            ind_min: 0,
            trans_now: 0.0,
            trans_up: 0.0,
            trans_down: 0.0,
            trans_mis: Vec::new(),
            fltrs_in: Vec::new(),
        }
    }
}

impl FilterDrive {
    fn trans_at(&self, i: i32) -> f64 {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.trans_mis.get(i).copied())
            .unwrap_or(0.0)
    }

    /// The filter pattern `ind` names, or `None` for C's `ind < 0`.
    pub fn pattern(&self) -> Option<i32> {
        let i = usize::try_from(self.ind).ok()?;
        Some(self.fltrs_in.get(i).copied().unwrap_or(0))
    }

    /// `update_filters` after the blade sync (`filterDrive.st:487-640`):
    /// blade transmissions at `energy`, the current transmission and mask, the
    /// permutation table, and the step up/down neighbours.
    pub fn update(&mut self, blades: &mut [Blade], energy: f64) -> UpdatePuts {
        for b in blades.iter_mut() {
            b.transm = calc_single_transmission(&b.mater, energy, b.thick);
        }
        self.tabulate(blades)
    }

    /// [`Self::update`] from the blades' `transm` as they stand.
    pub fn tabulate(&mut self, blades: &[Blade]) -> UpdatePuts {
        self.trans_now = 1.0;
        let mut fltrs_in_now = 0i32;
        let mut n_enabled = 0usize;
        for (i, b) in blades.iter().enumerate() {
            if b.set != 0 && b.enbl != 0 {
                fltrs_in_now |= 1 << i;
                self.trans_now *= b.transm;
            }
            if b.lock == 0 && b.enbl != 0 {
                n_enabled += 1;
            }
        }
        let too_many = (n_enabled > MAX_ENABLED)
            .then(|| format!("ERROR: Too many unlocked ({n_enabled}/{MAX_ENABLED})"));

        // C sizes its arrays for MAX_ENABLED and overruns them past it.
        self.n_perms = 1 << n_enabled.min(MAX_ENABLED);
        if self.trans_mis.len() < self.n_perms {
            self.trans_mis.resize(self.n_perms, 0.0);
            self.fltrs_in.resize(self.n_perms, 0);
        }
        for i in 0..self.n_perms {
            let mut t = 1.0;
            let mut m = 0i32;
            let mut k = 0;
            for (j, b) in blades.iter().enumerate() {
                if b.enbl == 0 {
                    continue;
                }
                if b.lock == 1 {
                    if b.set != 0 {
                        t *= b.transm;
                        m |= 1 << j;
                    }
                } else {
                    if i & (1 << k) != 0 {
                        t *= b.transm;
                        m |= 1 << j;
                    }
                    k += 1;
                }
            }
            self.trans_mis[i] = t;
            self.fltrs_in[i] = m;
        }

        // The last pattern whose product equals the current one: both multiply
        // the same blades in the same order, so equality is exact.
        self.ind_now = -1;
        for ind in 0..self.n_perms {
            if self.trans_mis[ind] == self.trans_now {
                self.ind_now = ind as i32;
            }
        }
        if self.ind_now < 0 {
            println!("ERROR: Could not find current filter index");
        }

        self.ind_up = -1;
        self.trans_up = 10.0;
        for ind in 0..self.n_perms {
            let t = self.trans_mis[ind];
            if t > self.trans_now * (1.0 + SMALL_FRAC) && t < self.trans_up {
                self.trans_up = t;
                self.ind_up = ind as i32;
            }
        }
        if self.trans_up > 1.0 {
            self.trans_up = self.trans_now;
            self.ind_up = self.ind_now;
        }

        self.ind_down = -1;
        self.trans_down = -1.0;
        for ind in 0..self.n_perms {
            let t = self.trans_mis[ind];
            if t < self.trans_now * (1.0 - SMALL_FRAC) && t > self.trans_down {
                self.trans_down = t;
                self.ind_down = ind as i32;
            }
        }
        if self.trans_down < 0.0 {
            self.trans_down = self.trans_now;
            self.ind_down = self.ind_now;
        }
        // The `for (ind = 0; ind < nPerms; ...)` loops leave it here.
        self.ind = self.n_perms as i32;

        let clear_busy = std::mem::take(&mut self.change_flag);
        UpdatePuts {
            transmission: self.trans_now,
            filter_mask: fltrs_in_now,
            too_many,
            transmission_up: self.trans_up,
            transmission_down: self.trans_down,
            clear_busy,
        }
    }

    /// `action`'s choice of `ind` for `my_action` (`filterDrive.st:663-760`),
    /// leaving in `msge` the message C puts for it. `name` prefixes the one
    /// unconditional console line.
    pub fn action(
        &mut self,
        my_action: i32,
        blades: &[Blade],
        transm_setpt: f64,
        filtermask_setpt: i32,
        name: &str,
    ) {
        self.msge = match my_action {
            0 => {
                let mut trans = -1.0;
                let mut trans_min = 1.0;
                for k in 0..self.n_perms {
                    let t = self.trans_mis[k];
                    if t <= transm_setpt && t > trans {
                        self.ind = k as i32;
                        trans = t;
                    }
                    if t < trans_min {
                        trans_min = t;
                        self.ind_min = k as i32;
                    }
                }
                if trans < trans_min {
                    self.ind = self.ind_min;
                }
                if self.trans_at(self.ind) == self.trans_now {
                    "OK - No change".into()
                } else {
                    "OK".into()
                }
            }
            1 => {
                self.ind = self.ind_up;
                if self.trans_up == self.trans_now {
                    "NO CHANGE! Step unavailable.".into()
                } else {
                    "OK".into()
                }
            }
            2 => {
                self.ind = self.ind_down;
                if self.trans_down == self.trans_now {
                    "NO CHANGE! Step unavailable.".into()
                } else {
                    "OK".into()
                }
            }
            _ => {
                let n = blades.len() as u32;
                let max_mask = 1i64.checked_shl(n).map_or(i64::MAX, |m| m - 1);
                if filtermask_setpt < 0 || i64::from(filtermask_setpt) > max_mask {
                    self.ind = self.ind_now;
                    "ERROR! Invalid mask.".into()
                } else {
                    let mut fltrs_req = 0i32;
                    for (j, b) in blades.iter().enumerate() {
                        if b.enbl == 0 {
                            continue;
                        }
                        if b.lock == 1 {
                            if b.set != 0 {
                                fltrs_req |= 1 << j;
                            }
                        } else {
                            fltrs_req |= (1 << j) & filtermask_setpt;
                        }
                    }
                    self.ind = -1;
                    for i in 0..self.n_perms {
                        if self.fltrs_in[i] == fltrs_req {
                            self.ind = i as i32;
                        }
                    }
                    if self.ind < 0 {
                        println!("{name}: Cannot find the requested filter pattern!");
                        "ERROR! Filter Mask not found.".into()
                    } else {
                        "OK".into()
                    }
                }
            }
        };
    }
}

/// The 16 `mbbo` state-string fields, which C's `string MATER` monitor reads
/// as the `DBR_STRING` form of `Material`.
pub(super) const MBBO_STATE_FIELDS: [&str; 16] = [
    "ZRST", "ONST", "TWST", "THST", "FRST", "FVST", "SXST", "SVST", "EIST", "NIST", "TEST", "ELST",
    "TVST", "TTST", "FTST", "FFST",
];

/// One channel per blade field the program assigns.
struct BladeChannels {
    thick: SeqPv,
    mater: String,
    transm: SeqPv,
    lock: SeqPv,
    set: SeqPv,
    enbl: SeqPv,
    outget: SeqPv,
}

impl BladeChannels {
    async fn read(&self, queue: &PutQueue) -> Blade {
        Blade {
            thick: self.thick.get_f64().await,
            mater: mbbo_string(queue, &self.mater).await,
            lock: self.lock.get_i32().await,
            set: self.set.get_i32().await,
            enbl: self.enbl.get_i32().await,
            transm: 0.0,
        }
    }
}

async fn mbbo_string(queue: &PutQueue, record: &str) -> String {
    let idx = queue.pv(record).get_i32().await;
    match usize::try_from(idx)
        .ok()
        .and_then(|i| MBBO_STATE_FIELDS.get(i))
    {
        Some(field) => queue.pv(&format!("{record}.{field}")).get_string().await,
        None => "Illegal Value".into(),
    }
}

/// C's event flags (`filterDrive.st:107-189`): one per `sync`, raised by any
/// monitor of the variables synced to it.
#[derive(Debug, Default)]
struct Flags {
    new_energy: bool,
    need_recalc: bool,
    transm_setpt: bool,
    filtermask_setpt: bool,
    external_io: bool,
}

struct Names {
    energy: String,
    setpt: String,
    mask_setpt: String,
    recalc: Vec<String>,
    outget: Vec<String>,
}

impl Names {
    fn raise(&self, flags: &mut Flags, pv: &str) {
        if pv == self.energy {
            flags.new_energy = true;
        } else if pv == self.setpt {
            flags.transm_setpt = true;
        } else if pv == self.mask_setpt {
            flags.filtermask_setpt = true;
        } else if self.recalc.iter().any(|n| n == pv) {
            flags.need_recalc = true;
        } else if self.outget.iter().any(|n| n == pv) {
            flags.external_io = true;
        }
    }
}

/// Take every monitor already queued, raising the flags it is synced to.
async fn poll(monitor: &mut DbMultiMonitor, names: &Names, flags: &mut Flags) {
    while let Ok((pv, _)) =
        epics_base_rs::runtime::task::timeout(Duration::ZERO, monitor.wait_change()).await
    {
        names.raise(flags, &pv);
    }
}

fn sleep_secs(secs: f64) -> impl std::future::Future<Output = ()> {
    epics_base_rs::runtime::task::sleep(epics_base_rs::runtime::time::duration_from_secs(secs))
}

/// Async entry point — runs the filter_drive state machine against live PVs.
pub async fn run(
    config: FilterDriveConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let name = config.name.as_str();
    let pr = format!("{}{}", config.prefix, config.record);
    let n = config.num_filters;
    let queue = PutQueue::new(&db, "filterDrive");
    let ch = |leaf: &str| queue.pv(&format!("{pr}{leaf}"));

    let ch_status = ch("Status");
    let ch_nfilters = ch("NumFilters");
    let ch_debug = ch("Debug");
    let ch_trans = ch("Transmission");
    let ch_trans_up = ch("TransmissionUp");
    let ch_trans_down = ch("TransmissionDown");
    let ch_mask = ch("FilterMask");
    let ch_setpt = ch("TransmissionSetpoint");
    let ch_mask_setpt = ch("FilterMaskSetpoint");
    let ch_step_up = ch("TransmissionStepUp");
    let ch_step_down = ch("TransmissionStepDown");
    let ch_energy = ch("Energy");
    let ch_msg = ch("Message");
    let ch_wait_time = ch("WaitTime");

    // `start` (`filterDrive.st:327-372`).
    let mut prog = FilterDrive {
        change_flag: true,
        ..Default::default()
    };
    sleep_secs(1.0).await;
    let blades_ch: Vec<BladeChannels> = (1..=n)
        .map(|i| BladeChannels {
            thick: queue.pv(&config.blade_pv(i, "Thickness")),
            mater: config.blade_pv(i, "Material"),
            transm: queue.pv(&config.blade_pv(i, "Transmission")),
            lock: queue.pv(&config.blade_pv(i, "Lock")),
            set: queue.pv(&config.blade_pv(i, "Set")),
            enbl: queue.pv(&config.blade_pv(i, "Enable")),
            outget: queue.pv(&config.blade_pv(i, "OutGet")),
        })
        .collect();
    let names = Names {
        energy: format!("{pr}Energy"),
        setpt: format!("{pr}TransmissionSetpoint"),
        mask_setpt: format!("{pr}FilterMaskSetpoint"),
        recalc: (1..=n)
            .flat_map(|i| {
                ["Thickness", "Material", "Lock", "Enable"].map(|f| config.blade_pv(i, f))
            })
            .collect(),
        outget: (1..=n).map(|i| config.blade_pv(i, "OutGet")).collect(),
    };
    // Every monitored variable wakes the `waiting` clauses; the synced ones
    // also raise their flag.
    let mut monitored = vec![
        format!("{pr}Status"),
        format!("{pr}Debug"),
        names.energy.clone(),
        names.setpt.clone(),
        names.mask_setpt.clone(),
        format!("{pr}TransmissionStepUp"),
        format!("{pr}TransmissionStepDown"),
    ];
    monitored.extend(names.recalc.iter().cloned());
    monitored.extend(names.outget.iter().cloned());
    monitored.extend((1..=n).map(|i| config.blade_pv(i, "Set")));
    let mut monitor = DbMultiMonitor::new(&db, &monitored).await;
    // The first monitor of each synced variable raises its flag at connect.
    let mut flags = Flags {
        new_energy: true,
        need_recalc: true,
        transm_setpt: true,
        filtermask_setpt: true,
        external_io: true,
    };
    let _ = ch_nfilters.put_i32_process(n as i32).await;
    println!("{name}: SNL program started for {pr}");
    println!("{name}:   # of filters = {n}");
    sleep_secs(1.0).await;

    // `init` (`filterDrive.st:377-389`).
    sleep_secs(2.0).await;
    let _ = ch_msg.put_string_process("Initialising ...").await;
    prog.msge.clear();
    poll(&mut monitor, &names, &mut flags).await;
    flags.transm_setpt = false;
    let _ = ch_step_up.put_i32_process(0).await;
    let _ = ch_step_down.put_i32_process(0).await;
    flags.need_recalc = true;

    loop {
        // `waiting` (`filterDrive.st:396-450`): the first true clause wins.
        let my_action = loop {
            poll(&mut monitor, &names, &mut flags).await;
            if std::mem::take(&mut flags.new_energy) || std::mem::take(&mut flags.need_recalc) {
                break None;
            }
            if std::mem::take(&mut flags.transm_setpt) {
                break Some(0);
            }
            if ch_step_up.get_i32().await != 0 {
                let _ = ch_step_up.put_i32_process(0).await;
                break Some(1);
            }
            if ch_step_down.get_i32().await != 0 {
                let _ = ch_step_down.put_i32_process(0).await;
                break Some(2);
            }
            if std::mem::take(&mut flags.filtermask_setpt) {
                break Some(3);
            }
            if ch_status.get_i32().await != 0 {
                prog.change_flag = true;
                break None;
            }
            if std::mem::take(&mut flags.external_io) {
                break None;
            }
            let (pv, _) = monitor.wait_change().await;
            names.raise(&mut flags, &pv);
        };

        let Some(my_action) = my_action else {
            // `update_filters` (`filterDrive.st:454-640`).
            let _ = ch_msg.put_string_process(&prog.msge).await;
            prog.msge = "OK".into();
            let mut blades = Vec::with_capacity(n);
            for b in &blades_ch {
                let outget = b.outget.get_f64().await as i32;
                poll(&mut monitor, &names, &mut flags).await;
                flags.external_io = false;
                let mut blade = b.read(&queue).await;
                if blade.set != outget {
                    // Only on a difference: the put processes Set, whose busy
                    // link would bring the program straight back here.
                    blade.set = outget;
                    let _ = b.set.put_i32_process(outget).await;
                    prog.msge = "Filter state set by external agent.".into();
                    let _ = ch_msg.put_string_process(&prog.msge).await;
                }
                blades.push(blade);
            }
            let energy = ch_energy.get_f64().await;
            let puts = prog.update(&mut blades, energy);
            for (b, blade) in blades_ch.iter().zip(&blades) {
                let _ = b.transm.put_f64_process(blade.transm).await;
            }
            let _ = ch_trans.put_f64_process(puts.transmission).await;
            let _ = ch_mask.put_f64_process(f64::from(puts.filter_mask)).await;
            if let Some(msg) = puts.too_many {
                prog.msge = msg;
                let _ = ch_msg.put_string_process(&prog.msge).await;
            }
            let _ = ch_trans_up.put_f64_process(puts.transmission_up).await;
            let _ = ch_trans_down.put_f64_process(puts.transmission_down).await;
            if puts.clear_busy {
                let _ = ch_status.put_i32_process(0).await;
            }
            continue;
        };

        // `action` (`filterDrive.st:645-765`).
        let _ = ch_msg.put_string_process("").await;
        prog.change_flag = true;
        if ch_debug.get_i32().await & 0x1 != 0 {
            let _ = ch_msg
                .put_string_process(&format!("Action = {my_action}"))
                .await;
            println!("Action = {my_action}");
        }
        let mut blades = Vec::with_capacity(n);
        for b in &blades_ch {
            blades.push(b.read(&queue).await);
        }
        let setpt = ch_setpt.get_f64().await;
        let mask_setpt = ch_mask_setpt.get_f64().await as i32;
        prog.action(my_action, &blades, setpt, mask_setpt, name);
        let _ = ch_msg.put_string_process(&prog.msge).await;
        let _ = ch_step_up.put_i32_process(0).await;
        let _ = ch_step_down.put_i32_process(0).await;

        // `change_filters` (`filterDrive.st:770-815`): put in, wait, take out.
        if let Some(pattern) = prog.pattern() {
            for wanted in [1, 0] {
                let mut do_wait = false;
                for (j, b) in blades_ch.iter().enumerate() {
                    if b.enbl.get_i32().await == 0 || (pattern >> j & 1) != wanted {
                        continue;
                    }
                    if b.set.get_i32().await != wanted {
                        do_wait = true;
                    }
                    let _ = b.set.put_i32_process(wanted).await;
                }
                if do_wait {
                    sleep_secs(ch_wait_time.get_f64().await).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blade(transm: f64, set: i32, lock: i32, enbl: i32) -> Blade {
        Blade {
            set,
            lock,
            enbl,
            transm,
            ..Default::default()
        }
    }

    fn al(set: i32, lock: i32, enbl: i32) -> Blade {
        Blade {
            mater: "Al".into(),
            thick: 100.0,
            ..blade(0.0, set, lock, enbl)
        }
    }

    #[test]
    fn test_calc_single_transmission_al() {
        let t = calc_single_transmission("Al", 10.0, 100.0);
        assert!(t > 0.0 && t < 1.0, "Al transmission at 10keV = {t}");
    }

    #[test]
    fn test_calc_single_transmission_unknown() {
        assert_eq!(calc_single_transmission("Unobtainium", 10.0, 100.0), 0.0);
    }

    /// No enabled blade leaves one pattern, all out, and the step values
    /// fall back to the current transmission.
    #[test]
    fn no_enabled_blade_is_one_pattern() {
        let mut prog = FilterDrive::default();
        let puts = prog.update(&mut [al(1, 0, 0)], 10.0);
        assert_eq!(prog.n_perms, 1);
        assert_eq!(prog.trans_mis[0], 1.0);
        assert_eq!((puts.transmission_up, puts.transmission_down), (1.0, 1.0));
        assert_eq!(prog.ind_now, 0);
    }

    /// A locked inserted blade is in every pattern; a disabled one in none.
    #[test]
    fn locked_and_disabled_blades_shape_the_patterns() {
        let mut prog = FilterDrive::default();
        let puts = prog.update(&mut [al(1, 1, 1), al(0, 0, 1), al(1, 0, 0)], 10.0);
        assert_eq!(prog.n_perms, 2);
        assert_eq!(&prog.fltrs_in[..2], &[0b001, 0b011]);
        assert_eq!(puts.filter_mask, 0b001);
    }

    /// Action 0 takes the highest transmission at or below the setpoint, and
    /// the lowest one when none is.
    #[test]
    fn a_setpoint_takes_the_highest_pattern_at_or_below_it() {
        let mut prog = FilterDrive::default();
        let blades = [blade(0.5, 0, 0, 1), blade(0.3, 0, 0, 1)];
        prog.tabulate(&blades);
        prog.action(0, &blades, 0.4, 0, "t");
        assert_eq!((prog.msge.as_str(), prog.pattern()), ("OK", Some(0b10)));
        prog.action(0, &blades, 0.1, 0, "t");
        assert_eq!(prog.pattern(), Some(0b11));
        prog.action(0, &blades, 1.0, 0, "t");
        assert_eq!(
            (prog.msge.as_str(), prog.pattern()),
            ("OK - No change", Some(0))
        );
    }

    /// A mask past the blade count keeps the current pattern; a mask asking
    /// to move a locked blade is adjusted to leave it where it is.
    #[test]
    fn a_mask_setpoint_is_bounded_and_respects_locks() {
        let mut prog = FilterDrive::default();
        let blades = [blade(0.5, 1, 1, 1), blade(0.3, 0, 0, 1)];
        prog.tabulate(&blades);
        prog.action(3, &blades, 0.0, 4, "t");
        assert_eq!(prog.msge, "ERROR! Invalid mask.");
        assert_eq!(prog.ind, prog.ind_now);
        prog.action(3, &blades, 0.0, 0b10, "t");
        assert_eq!((prog.msge.as_str(), prog.pattern()), ("OK", Some(0b11)));
    }

    /// At the top step there is no neighbour above, and the step points at
    /// the current pattern with C's "unavailable" message.
    #[test]
    fn a_step_past_the_end_is_unavailable() {
        let mut prog = FilterDrive::default();
        let blades = [blade(0.5, 0, 0, 1)];
        prog.tabulate(&blades);
        assert_eq!((prog.trans_up, prog.ind_up), (1.0, prog.ind_now));
        prog.action(1, &blades, 0.0, 0, "t");
        assert_eq!(prog.msge, "NO CHANGE! Step unavailable.");
        prog.action(2, &blades, 0.0, 0, "t");
        assert_eq!((prog.msge.as_str(), prog.pattern()), ("OK", Some(1)));
    }
}
