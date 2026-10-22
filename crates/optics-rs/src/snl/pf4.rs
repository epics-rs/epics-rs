//! XIA PF4 dual-filter bank state machine.
//!
//! Port of `pf4.st`: one program per bank of 4 blades (16 combinations),
//! run through C's states (`init`, `idle`, `filterBits`, `filterPos`,
//! `recalcBank`, `bankControl`, `bankOff`) with one flag per C event flag.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_libcom_rs::cfmt::CFloat;

use crate::data::chantler::{find_material, other_absorption_length_um};
use crate::db_access::DbMultiMonitor;
use epics_base_rs::server::snl::{PutQueue, SeqPv};

/// Number of filter combinations per bank (4 bits = 16).
pub const NUM_COMBINATIONS: usize = 16;

/// Material index constants matching the SNL code.
pub const MAT_AL: u8 = 0;
pub const MAT_TI: u8 = 1;
pub const MAT_GLASS: u8 = 2;
pub const MAT_OTHER: u8 = 3;

/// `IDLE_WAIT`, `PVPUT_WAIT`, `ENERGY_WAIT`, `FILTER_WAIT` (`pf4.st:31-34`).
const IDLE_WAIT: f64 = 0.10;
const PVPUT_WAIT: f64 = 0.01;
const ENERGY_WAIT: f64 = 1.50;
const FILTER_WAIT: f64 = 0.20;

/// `float showEnergy[300]` / `showTransmission[300]` (`pf4.st:112-115`).
const SHOW_LEN: usize = 300;

/// One bank's blades as `RecalcFilters` takes them.
#[derive(Debug, Clone, Default)]
pub struct Pf4BankConfig {
    /// Filter blade thicknesses in mm (4 blades).
    pub thicknesses: [f64; 4],
    /// Material index for each blade (0=Al, 1=Ti, 2=Glass, 3=Other).
    pub material_indices: [u8; 4],
    /// Material name for "Other" blades.
    pub other_materials: [String; 4],
}

/// The program's macros (`pf4.st:17`).
#[derive(Debug, Clone)]
pub struct Pf4Config {
    pub name: String,
    pub prefix: String,
    pub hardware: String,
    pub bank: String,
    /// `M`, the monochromator energy PV.
    pub mono: String,
    /// `BP`, prefixed to each of `B1..B4`.
    pub bit_prefix: String,
    /// `B1..B4`, the filter control bits.
    pub bits: [String; 4],
}

impl Pf4Config {
    pub fn new(p: &str, h: &str, b: &str) -> Self {
        Self {
            name: "pf4".into(),
            prefix: p.to_string(),
            hardware: h.to_string(),
            bank: b.to_string(),
            mono: String::new(),
            bit_prefix: String::new(),
            bits: Default::default(),
        }
    }

    /// `"{BP}{Bn}"` for blade `i` (0-based).
    fn bit_pv(&self, i: usize) -> String {
        format!("{}{}", self.bit_prefix, self.bits[i])
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

/// `isLegalOther` (`pf4.st:613-620`).
pub fn is_legal_other(name: &str) -> bool {
    find_material(name).is_some()
}

/// `OtherAbsorptionLength` (`pf4.st:622-647`), `None` where C has no data.
///
/// **DEVIATION from C, deliberate — CBUG-B4.** C answers an unknown material
/// name, or an energy outside its table, with `0.` (`pf4.st:629-631`,
/// `:637-639`), which `RecalcFilters` divides by: `exp(-x*1000./0.)` = 0, the
/// blade reported **perfectly opaque**, silently (both C diagnostics are
/// commented out). The port has no data to compute a transmission from and
/// does not invent one: [`recalc_filters`] leaves the blade **not modelled**
/// (factor 1.0) and logs it. The operator already sees it through `otherLegal`.
/// Not modelling the blade is also the safe direction of the two wrong
/// answers: a blade believed opaque is one the ranking would select to
/// attenuate, letting far more beam through than predicted.
fn other_absorption_length(kev: f64, species: &str) -> Option<f64> {
    find_material(species).and_then(|mat| other_absorption_length_um(mat, kev))
}

/// `RecalcFilters` (`pf4.st:650-706`): the transmission of all 16 blade
/// combinations, each material's thickness summed into one exponent as C
/// does, then sorted by decreasing transmission when the bank is on.
pub fn recalc_filters(
    kev: f64,
    cfg: &Pf4BankConfig,
    bank_on: bool,
    debug: i32,
) -> ([f64; NUM_COMBINATIONS], [u8; NUM_COMBINATIONS]) {
    let z = cfg.material_indices;
    let f = cfg.thicknesses;
    if debug >= 10 {
        println!("\nRecalcFilters: keV={}", CFloat(kev).g());
    }
    let abs_len_al = al_absorption_length_microns(kev);
    let abs_len_ti = ti_absorption_length_microns(kev);
    let abs_len_glass = glass_absorption_length_microns(kev);
    let abs_len_other: [Option<f64>; 4] = std::array::from_fn(|k| {
        if z[k] == MAT_OTHER {
            other_absorption_length(kev, &cfg.other_materials[k])
        } else {
            Some(0.0)
        }
    });

    let mut xmit = [0.0; NUM_COMBINATIONS];
    let mut bits = [0u8; NUM_COMBINATIONS];
    for i in 0..NUM_COMBINATIONS {
        if bank_on {
            let in_beam = |k: usize, mat: u8| z[k] == mat && i & (1 << k) != 0;
            let sum =
                |mat: u8| (0..4).fold(0.0, |s, k| s + if in_beam(k, mat) { f[k] } else { 0.0 });
            let (x_al, x_ti, x_glass) = (sum(MAT_AL), sum(MAT_TI), sum(MAT_GLASS));
            if debug >= 10 {
                println!(
                    "       in RecalcFilters {i:3}, Aluminum = {},   Titanium = {},   Glass = {}",
                    CFloat(x_al).g(),
                    CFloat(x_ti).g(),
                    CFloat(x_glass).g()
                );
            }
            let mut x = (-x_al * 1000. / abs_len_al).exp();
            x *= (-x_ti * 1000. / abs_len_ti).exp();
            if x_glass > 0.0 {
                x *= (-x_glass * 1000. / abs_len_glass).exp();
            }
            for k in 0..4 {
                let x_other = if in_beam(k, MAT_OTHER) { f[k] } else { 0.0 };
                if x_other > 0.0 {
                    match abs_len_other[k] {
                        Some(len) => x *= (-x_other * 1000. / len).exp(),
                        None => tracing::error!(
                            material = cfg.other_materials[k].as_str(),
                            kev,
                            "pf4: no absorption data for this Other blade \
                             (unknown material, or energy outside its Chantler table); \
                             the blade is NOT modelled"
                        ),
                    }
                }
            }
            xmit[i] = x;
        } else {
            xmit[i] = 1.0;
        }
        bits[i] = i as u8;
    }
    if bank_on {
        sort_decreasing(debug, &mut xmit, &mut bits);
    }
    (xmit, bits)
}

/// `sortDecreasing` (`pf4.st:709-767`), an insertion sort.
fn sort_decreasing(
    debug: i32,
    xmit: &mut [f64; NUM_COMBINATIONS],
    bits: &mut [u8; NUM_COMBINATIONS],
) {
    let print = |what: &str, xmit: &[f64; NUM_COMBINATIONS], bits: &[u8; NUM_COMBINATIONS]| {
        let b: Vec<String> = bits.iter().map(u8::to_string).collect();
        println!("       {what} sorting, bits={}", b.join(","));
        if debug >= 10 {
            let t: Vec<String> = xmit[..NUM_COMBINATIONS / 3]
                .iter()
                .map(|&v| CFloat(v).g().to_string())
                .collect();
            println!("                   transmit={}", t.join(", "));
        }
    };
    if debug >= 9 {
        print("before", xmit, bits);
    }
    for j in 1..NUM_COMBINATIONS {
        let a = xmit[j];
        let b = bits[j];
        let mut i = j;
        while i > 0 && xmit[i - 1] < a {
            xmit[i] = xmit[i - 1];
            bits[i] = bits[i - 1];
            i -= 1;
        }
        xmit[i] = a;
        bits[i] = b;
    }
    if debug >= 8 {
        print("after", xmit, bits);
    }
}

/// `numInArray` (`pf4.st:790-796`): the first index holding `value`, else 0.
pub fn find_position(bits: &[u8; NUM_COMBINATIONS], value: i32) -> usize {
    bits.iter()
        .position(|&b| i32::from(b) == value)
        .unwrap_or(0)
}

/// `thickZ` (`pf4.st:771-785`): the thickness of material `mat` in the beam.
pub fn thickness_by_material(
    mat: u8,
    bank_on: bool,
    in_beam: [bool; 4],
    mat_indices: [u8; 4],
    thicknesses: [f64; 4],
) -> f64 {
    let mut sum = 0.0;
    if bank_on {
        for k in 0..4 {
            sum += if in_beam[k] && mat_indices[k] == mat {
                thicknesses[k]
            } else {
                0.0
            };
        }
    }
    sum
}

/// `fillShowArrays` (`pf4.st:798-818`): a material's table into `e`/`t`,
/// padded with its last entry up to `n`. `false` for an unknown material.
fn fill_show_arrays(species: &str, n: usize, e: &mut [f32], t: &mut [f32]) -> bool {
    let Some(mat) = find_material(species) else {
        return false;
    };
    let n = n.min(e.len());
    let entries = mat.kev.len().min(e.len());
    e[..entries].copy_from_slice(&mat.kev[..entries]);
    t[..entries].copy_from_slice(&mat.mu[..entries]);
    let k = entries.saturating_sub(1);
    for j in entries..n {
        e[j] = mat.kev[k];
        t[j] = mat.mu[k];
    }
    true
}

/// C's event flags: one per `EvFlag` variable (`seqPVmacros.h` `EvFlag`),
/// raised by every monitor of it, the program's own puts included.
#[derive(Debug, Default)]
struct Flags {
    b: [bool; 4],
    mono: bool,
    select: bool,
    local: bool,
    debug: bool,
    bankctl: bool,
    filpos: bool,
    f: [bool; 4],
    z: [bool; 4],
    other: [bool; 4],
    other_show: bool,
}

struct Names {
    b: [String; 4],
    mono: String,
    select: String,
    local: String,
    debug: String,
    bankctl: String,
    filpos: String,
    f: [String; 4],
    z: [String; 4],
    other: [String; 4],
    other_show: String,
}

impl Names {
    fn raise(&self, flags: &mut Flags, pv: &str) {
        let hit = |n: &String| n == pv;
        for k in 0..4 {
            flags.b[k] |= hit(&self.b[k]);
            flags.f[k] |= hit(&self.f[k]);
            flags.z[k] |= hit(&self.z[k]);
            flags.other[k] |= hit(&self.other[k]);
        }
        flags.mono |= hit(&self.mono);
        flags.select |= hit(&self.select);
        flags.local |= hit(&self.local);
        flags.debug |= hit(&self.debug);
        flags.bankctl |= hit(&self.bankctl);
        flags.filpos |= hit(&self.filpos);
        flags.other_show |= hit(&self.other_show);
    }

    fn all(&self) -> Vec<String> {
        let mut v: Vec<String> = [
            &self.mono,
            &self.select,
            &self.local,
            &self.debug,
            &self.bankctl,
            &self.filpos,
            &self.other_show,
        ]
        .into_iter()
        .cloned()
        .collect();
        for set in [&self.b, &self.f, &self.z, &self.other] {
            v.extend(set.iter().cloned());
        }
        v
    }
}

/// `efTestAndClear(a) || efTestAndClear(b) || ...`: clears the first set flag only.
fn take_first(flags: &mut [bool], order: impl IntoIterator<Item = usize>) -> bool {
    order.into_iter().any(|k| std::mem::take(&mut flags[k]))
}

/// Take every monitor already queued, raising the flags it is synced to.
fn poll(monitor: &mut DbMultiMonitor, names: &Names, flags: &mut Flags) {
    for (pv, _) in monitor.try_changes() {
        names.raise(flags, &pv);
    }
}

fn sleep_secs(secs: f64) -> impl std::future::Future<Output = ()> {
    epics_base_rs::runtime::task::sleep(epics_base_rs::runtime::time::duration_from_secs(secs))
}

/// Wait for a monitor or for `deadline`, whichever is first.
async fn wait(
    monitor: &mut DbMultiMonitor,
    names: &Names,
    flags: &mut Flags,
    deadline: epics_base_rs::runtime::task::Instant,
) {
    tokio::select! {
        changes = monitor.wait_changes() => {
            for (pv, _) in changes {
                names.raise(flags, &pv);
            }
        }
        _ = epics_base_rs::runtime::task::sleep_until(deadline) => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    FilterBits,
    FilterPos,
    RecalcBank,
    BankControl,
    BankOff,
}

/// `DEBUG_PRINT` (`seqPVmacros.h`): the line, then a 10 ms sleep.
struct Debug<'a> {
    name: &'a str,
    flag: SeqPv,
}

impl Debug<'_> {
    async fn print(&self, level: i32, line: u32, msg: &str) {
        if self.flag.get_i32().await >= level {
            println!("<../pf4.st,{line},{},{level}> {msg}", self.name);
            sleep_secs(0.01).await;
        }
    }
}

/// Async entry point — runs one bank's `ss pf4` against live PVs.
pub async fn run(
    config: Pf4Config,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ph = format!("{}{}", config.prefix, config.hardware);
    let bk = config.bank.as_str();
    let queue = PutQueue::new(&db, "pf4");
    let ch = |var: &str, pv: &str| queue.pv(var, pv);
    let blade = |what: &str, k: usize| format!("{ph}{what}{}{bk}", k + 1);
    let names = Names {
        b: std::array::from_fn(|k| config.bit_pv(k)),
        mono: config.mono.clone(),
        select: format!("{ph}useMono"),
        local: format!("{ph}E:local"),
        debug: format!("{ph}debug{bk}"),
        bankctl: format!("{ph}bank{bk}"),
        filpos: format!("{ph}fPos{bk}"),
        f: std::array::from_fn(|k| blade("f", k)),
        z: std::array::from_fn(|k| blade("Z", k)),
        other: std::array::from_fn(|k| blade("Other", k)),
        other_show: format!("{ph}OtherShow"),
    };
    let ch_b: [SeqPv; 4] = std::array::from_fn(|k| ch(["b1", "b2", "b3", "b4"][k], &names.b[k]));
    let ch_display_bit: [SeqPv; 4] = std::array::from_fn(|k| {
        let var = ["displayBit1", "displayBit2", "displayBit3", "displayBit4"][k];
        ch(var, &blade("displayBit", k))
    });
    let ch_f: [SeqPv; 4] = std::array::from_fn(|k| ch(["f1", "f2", "f3", "f4"][k], &names.f[k]));
    let ch_z: [SeqPv; 4] = std::array::from_fn(|k| ch(["z1", "z2", "z3", "z4"][k], &names.z[k]));
    let ch_other: [SeqPv; 4] =
        std::array::from_fn(|k| ch(["other1", "other2", "other3", "other4"][k], &names.other[k]));
    let ch_other_legal: [SeqPv; 4] = std::array::from_fn(|k| {
        let var = ["otherLegal1", "otherLegal2", "otherLegal3", "otherLegal4"][k];
        ch(var, &blade("OtherLegal", k))
    });
    let ch_label: [SeqPv; NUM_COMBINATIONS] = std::array::from_fn(|i| {
        ch(
            &format!("a{i:02}"),
            &format!(
                "{}.{}",
                names.filpos,
                super::filter_drive::MBBO_STATE_FIELDS[i]
            ),
        )
    });
    let ch_mono = ch("monoenergy", &names.mono);
    let ch_display_energy = ch("displayEnergy", &format!("{ph}displayEnergy"));
    let ch_select = ch("selectenergy", &names.select);
    let ch_local = ch("localenergy", &names.local);
    let ch_filter: [SeqPv; 3] = [
        ("filterAl", "filterAl"),
        ("filterTi", "filterTi"),
        ("filterGl", "filterGlass"),
    ]
    .map(|(var, n)| ch(var, &format!("{ph}{n}")));
    let ch_trans = ch("trans", &format!("{ph}trans{bk}"));
    let ch_invtrans = ch("invtrans", &format!("{ph}invTrans{bk}"));
    let ch_bitflg = ch("bitflg", &format!("{ph}bitFlag{bk}"));
    let ch_bitsts = ch("bitsts", &format!("{ph}status{bk}"));
    let ch_bankctl = ch("bankctl", &names.bankctl);
    let ch_filpos = ch("filpos", &names.filpos);
    let ch_other_show = ch("otherShow", &names.other_show);
    let ch_num_show = ch("numShow", &format!("{ph}E.NELM"));
    let show_records = [
        ch("showEnergy", &format!("{ph}E")),
        ch("showTransmission", &format!("{ph}T")),
    ];
    let debug = Debug {
        name: &config.name,
        flag: ch("debug_flag", &names.debug),
    };
    let mut monitor = DbMultiMonitor::new(&db, &names.all()).await;

    // C's globals: zero until the first `RecalcFilters`.
    let mut bits = [0u8; NUM_COMBINATIONS];
    let mut xmit = [0.0f64; NUM_COMBINATIONS];
    let mut energytimeout;
    let mut show = [[0.0f32; SHOW_LEN]; 2];
    let read_b = async || -> [i32; 4] {
        let mut v = [0; 4];
        for k in 0..4 {
            v[k] = ch_b[k].get_i32().await;
        }
        v
    };
    let put_bankctl = async |on: i16, off: i16| {
        let v = if ch_bankctl.get_i16().await != 0 {
            on
        } else {
            off
        };
        let _ = ch_bankctl.put_i16_sync(v).await;
        v
    };

    // `init` (`pf4.st:146-197`).
    energytimeout = 0.0;
    let b = read_b().await;
    for k in 0..4 {
        let _ = ch_display_bit[k].put_i32_sync(b[k]).await;
    }
    let mono = ch_mono.get_f64().await;
    let _ = ch_display_energy.put_f64_sync(mono).await;
    let _ = ch_local.put_f64_sync(mono).await;
    let mut flags = Flags::default();
    poll(&mut monitor, &names, &mut flags);
    // init clears every flag but these, which their connect-time monitors raised.
    flags = Flags {
        other: [true; 4],
        other_show: true,
        ..Flags::default()
    };
    debug.print(2, 195, "init complete").await;

    let mut state = State::Idle;
    loop {
        state = match state {
            State::Idle => {
                let entered = epics_base_rs::runtime::task::Instant::now();
                let delay = epics_base_rs::runtime::time::duration_from_secs(IDLE_WAIT);
                loop {
                    poll(&mut monitor, &names, &mut flags);
                    if std::mem::take(&mut flags.debug) {
                        let msg = format!("Debug level changed to {}", debug.flag.get_i32().await);
                        debug.print(1, 349, &msg).await;
                        break State::Idle;
                    }
                    if std::mem::take(&mut flags.local) {
                        let _ = ch_select.put_i32_sync(0).await;
                        sleep_secs(PVPUT_WAIT).await;
                        poll(&mut monitor, &names, &mut flags);
                        flags.select = false;
                        let msg = format!(
                            "local energy changed to {}",
                            CFloat(ch_local.get_f64().await)
                        );
                        debug.print(2, 360, &msg).await;
                        break State::RecalcBank;
                    }
                    if std::mem::take(&mut flags.select) {
                        let select = ch_select.get_i32().await;
                        if select != 0 {
                            let _ = ch_local.put_f64_sync(ch_mono.get_f64().await).await;
                            sleep_secs(PVPUT_WAIT).await;
                            poll(&mut monitor, &names, &mut flags);
                            flags.local = false;
                            put_bankctl(3, 0).await;
                        }
                        let msg = format!("using energy local/monochromator changed to {select}");
                        debug.print(2, 377, &msg).await;
                        break State::Idle;
                    }
                    if std::mem::take(&mut flags.mono) {
                        let mono = ch_mono.get_f64().await;
                        let _ = ch_display_energy.put_f64_sync(mono).await;
                        if ch_select.get_i32().await != 0 {
                            let _ = ch_local.put_f64_sync(mono).await;
                            sleep_secs(PVPUT_WAIT).await;
                            poll(&mut monitor, &names, &mut flags);
                            flags.local = false;
                            energytimeout = ENERGY_WAIT;
                            let msg = format!("monochromator energy changed to {}", CFloat(mono));
                            debug.print(2, 389, &msg).await;
                        }
                        break State::Idle;
                    }
                    if std::mem::take(&mut flags.bankctl) {
                        let msg = format!(
                            "filter bank changed to {}, going to state bankControl",
                            ch_bankctl.get_i16().await
                        );
                        debug.print(2, 397, &msg).await;
                        break State::BankControl;
                    }
                    if take_first(&mut flags.f, 0..4) {
                        let mut f = [String::new(), String::new(), String::new(), String::new()];
                        for k in 0..4 {
                            f[k] = CFloat(ch_f[k].get_f64().await).g().to_string();
                        }
                        let msg = format!(
                            "filters thickness changed to f1={}, f2={}, f3={}, f3={}",
                            f[0], f[1], f[2], f[3]
                        );
                        debug.print(2, 405, &msg).await;
                        put_bankctl(3, 0).await;
                        break State::Idle;
                    }
                    if take_first(&mut flags.z, 0..4) {
                        let mut z = [0; 4];
                        for k in 0..4 {
                            z[k] = ch_z[k].get_i16().await;
                        }
                        let msg = format!(
                            "filters material index changed to z1={}, z2={}, z3={}, z4={}",
                            z[0], z[1], z[2], z[3]
                        );
                        debug.print(2, 416, &msg).await;
                        put_bankctl(3, 0).await;
                        break State::Idle;
                    }
                    if take_first(&mut flags.other, 0..4) {
                        let mut other =
                            [String::new(), String::new(), String::new(), String::new()];
                        for k in 0..4 {
                            other[k] = ch_other[k].get_string().await;
                        }
                        let msg = format!(
                            "filters material index changed to other1='{}', o2='{}', o3='{}', o4='{}'",
                            other[0], other[1], other[2], other[3]
                        );
                        debug.print(2, 428, &msg).await;
                        for k in 0..4 {
                            let legal = i16::from(is_legal_other(&other[k]));
                            let _ = ch_other_legal[k].put_i16_sync(legal).await;
                        }
                        put_bankctl(3, 0).await;
                        break State::Idle;
                    }
                    if take_first(&mut flags.b, [3, 2, 1, 0]) {
                        let b = read_b().await;
                        let msg = format!(
                            "filter bits changed to ({}{}{}{}), going to state filterBits",
                            b[3], b[2], b[1], b[0]
                        );
                        debug.print(2, 443, &msg).await;
                        break State::FilterBits;
                    }
                    if std::mem::take(&mut flags.filpos) && ch_bankctl.get_i16().await != 0 {
                        let msg = format!(
                            "filter position changed to {}, going to state filterPos",
                            ch_filpos.get_i16().await
                        );
                        debug.print(2, 450, &msg).await;
                        break State::FilterPos;
                    }
                    if entered.elapsed() >= delay {
                        if energytimeout > 0.0 && {
                            energytimeout -= IDLE_WAIT;
                            energytimeout <= 0.0
                        } {
                            put_bankctl(3, 0).await;
                            debug
                                .print(2, 460, "energy timeout expired, force recalculation")
                                .await;
                        }
                        break State::Idle;
                    }
                    if std::mem::take(&mut flags.other_show) {
                        let species = ch_other_show.get_string().await;
                        let n = usize::try_from(ch_num_show.get_i32().await).unwrap_or(0);
                        let [e, t] = &mut show;
                        if fill_show_arrays(&species, n, e, t) {
                            // `pvPut(showEnergy)`, `pvPut(showTransmission)`: DEFAULT.
                            for (pv, arr) in show_records.iter().zip(&show) {
                                let _ = pv.put(EpicsValue::FloatArray(arr.to_vec().into()));
                            }
                        }
                        break State::Idle;
                    }
                    wait(&mut monitor, &names, &mut flags, entered + delay).await;
                }
            }

            // `filterBits` (`pf4.st:201-220`).
            State::FilterBits => {
                let b = read_b().await;
                for k in 0..4 {
                    let _ = ch_display_bit[k].put_i32_sync(b[k]).await;
                }
                let pattern = (b[3] << 3) | (b[2] << 2) | (b[1] << 1) | b[0];
                let filpos = find_position(&bits, pattern);
                let _ = ch_filpos.put_i16_sync(filpos as i16).await;
                let _ = ch_bitflg.put_i32_sync(1 << filpos).await;
                let _ = ch_bitsts.put_i16_sync(i16::from(bits[filpos])).await;
                put_bankctl(2, 0).await;
                poll(&mut monitor, &names, &mut flags);
                flags.filpos = false;
                State::Idle
            }

            // `filterPos` (`pf4.st:222-250`).
            State::FilterPos => {
                let filpos = filpos_index(ch_filpos.get_i16().await);
                let pattern = bits[filpos];
                for k in 0..4 {
                    if pattern & (1 << k) != 0 {
                        let _ = ch_b[k].put_i32_sync(1).await;
                        let _ = ch_display_bit[k].put_i32_sync(1).await;
                    }
                }
                sleep_secs(FILTER_WAIT).await;
                for k in 0..4 {
                    if pattern & (1 << k) == 0 {
                        let _ = ch_b[k].put_i32_sync(0).await;
                        let _ = ch_display_bit[k].put_i32_sync(0).await;
                    }
                }
                let _ = ch_bitflg.put_i32_sync(1 << filpos).await;
                let _ = ch_bitsts.put_i16_sync(i16::from(pattern)).await;
                poll(&mut monitor, &names, &mut flags);
                flags.b = [false; 4];
                State::RecalcBank
            }

            // `recalcBank` (`pf4.st:252-261`).
            State::RecalcBank => {
                debug.print(2, 256, "recalcBank").await;
                put_bankctl(3, 0).await;
                State::Idle
            }

            // `bankControl` (`pf4.st:263-324`).
            State::BankControl => {
                let bankctl = ch_bankctl.get_i16().await;
                if bankctl == 0 {
                    let msg = format!("filter bank switched off ({bankctl})");
                    debug.print(2, 268, &msg).await;
                    State::BankOff
                } else {
                    let msg = match bankctl {
                        1 => format!("filter bank changed value to {bankctl}"),
                        2 => {
                            let b = read_b().await.map(|v| v != 0);
                            let (z, f) = blades(&ch_z, &ch_f).await;
                            for (mat, ch) in [MAT_AL, MAT_TI, MAT_GLASS].into_iter().zip(&ch_filter)
                            {
                                if z.contains(&mat) {
                                    let thick = thickness_by_material(mat, true, b, z, f);
                                    let _ = ch.put_f64_sync(thick).await;
                                }
                            }
                            let filpos = filpos_index(ch_filpos.get_i16().await);
                            let trans = xmit[filpos];
                            let _ = ch_trans.put_f64_sync(trans).await;
                            if trans > 0.0 {
                                let _ = ch_invtrans.put_f64_sync(1.0 / trans).await;
                            }
                            let _ = ch_bitsts.put_i16_sync(i16::from(bits[filpos])).await;
                            let _ = ch_bitflg.put_i32_sync(1 << filpos).await;
                            let v = put_bankctl(1, 0).await;
                            format!("filter bank changed value to {v}")
                        }
                        3 => {
                            let local = ch_local.get_f64().await;
                            poll(&mut monitor, &names, &mut flags);
                            flags.local = false;
                            let (z, f) = blades(&ch_z, &ch_f).await;
                            let mut other: [String; 4] = Default::default();
                            for k in 0..4 {
                                other[k] = ch_other[k].get_string().await;
                            }
                            let cfg = Pf4BankConfig {
                                thicknesses: f,
                                material_indices: z,
                                other_materials: other,
                            };
                            (xmit, bits) =
                                recalc_filters(local, &cfg, true, debug.flag.get_i32().await);
                            for (label, x) in ch_label.iter().zip(xmit) {
                                let _ = label.put_string_sync(&format!("{:.3e}", CFloat(x))).await;
                            }
                            let _ = ch_bankctl.put_i16_sync(2).await;
                            "filter bank changed value to 2".to_string()
                        }
                        _ => format!("invalid filter bank value of {bankctl}"),
                    };
                    debug.print(2, 322, &msg).await;
                    State::Idle
                }
            }

            // `bankOff` (`pf4.st:326-341`).
            State::BankOff => {
                let mut entered = epics_base_rs::runtime::task::Instant::now();
                let delay = epics_base_rs::runtime::time::duration_from_secs(IDLE_WAIT);
                loop {
                    poll(&mut monitor, &names, &mut flags);
                    let bankctl = ch_bankctl.get_i16().await;
                    if bankctl != 0 {
                        let mono = ch_mono.get_f64().await;
                        let _ = ch_display_energy.put_f64_sync(mono).await;
                        poll(&mut monitor, &names, &mut flags);
                        flags.local = false;
                        let msg = format!("filter bank switched on ({bankctl})");
                        debug.print(2, 337, &msg).await;
                        break State::RecalcBank;
                    }
                    if entered.elapsed() >= delay {
                        entered = epics_base_rs::runtime::task::Instant::now();
                        continue;
                    }
                    wait(&mut monitor, &names, &mut flags, entered + delay).await;
                }
            }
        };
    }
}

/// `bits[filpos]` / `1 << filpos` index. C indexes with the monitored value
/// unchecked; an mbbo `VAL` is at most 15.
fn filpos_index(filpos: i16) -> usize {
    usize::try_from(filpos)
        .unwrap_or(0)
        .min(NUM_COMBINATIONS - 1)
}

async fn blades(ch_z: &[SeqPv; 4], ch_f: &[SeqPv; 4]) -> ([u8; 4], [f64; 4]) {
    let mut z = [0u8; 4];
    let mut f = [0.0; 4];
    for k in 0..4 {
        z[k] = ch_z[k].get_i16().await as u8;
        f[k] = ch_f[k].get_f64().await;
    }
    (z, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One blade of `mm` in bank position `1`, the rest empty.
    fn one_blade(kev: f64, mm: f64, z: u8, other: &str) -> f64 {
        let cfg = Pf4BankConfig {
            thicknesses: [mm, 0.0, 0.0, 0.0],
            material_indices: [z, MAT_AL, MAT_AL, MAT_AL],
            other_materials: [other.into(), String::new(), String::new(), String::new()],
        };
        let (xmit, bits) = recalc_filters(kev, &cfg, true, 0);
        xmit[find_position(&bits, 1)]
    }

    /// C `RecalcFilters` at 10 keV for Al 0.2 / Ti 0.02 / glass 0.5 / Al 0.4 mm,
    /// printed with `%.17g` from `pf4.st`'s own functions. Positions 9, 11, 13
    /// and 15 hold both Al blades: C sums them into one exponent, and a product
    /// of per-blade factors is off in the last bits there.
    #[test]
    fn recalc_filters_is_c_recalc_filters_to_the_bit() {
        let cfg = Pf4BankConfig {
            thicknesses: [0.2, 0.02, 0.5, 0.4],
            material_indices: [MAT_AL, MAT_TI, MAT_GLASS, MAT_AL],
            other_materials: Default::default(),
        };
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
        assert_eq!(recalc_filters(10.0, &cfg, true, 0), (c_xmit, c_bits));
    }

    /// A bank that is off transmits fully, in bit order.
    #[test]
    fn recalc_filters_bank_off() {
        let (xmit, bits) = recalc_filters(10.0, &Pf4BankConfig::default(), false, 0);
        assert_eq!(xmit, [1.0; NUM_COMBINATIONS]);
        assert_eq!(bits, std::array::from_fn(|i| i as u8));
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
    /// C divides by its `0.` sentinel (`pf4.st:629-631`, `:696`) and reports
    /// the blade fully opaque, silently.
    #[test]
    fn test_b4_other_unknown_material_is_not_modelled() {
        assert_eq!(one_blade(10.0, 1.0, MAT_OTHER, "Unobtainium"), 1.0); // C: 0.0
        // C's strcmp is case-sensitive (pf4.st:627), so "cu" is unknown too.
        assert_eq!(one_blade(10.0, 1.0, MAT_OTHER, "cu"), 1.0);
        assert!(!is_legal_other("cu"));
    }

    /// CBUG-B4 — the same for an energy outside the material's tabulated range.
    #[test]
    fn test_b4_other_energy_above_the_table_is_not_modelled() {
        let cu = crate::data::chantler::find_material("Cu").unwrap();
        let last = cu.kev[cu.kev.len() - 1] as f64;
        assert_eq!(one_blade(500.0, 1.0, MAT_OTHER, "Cu"), 1.0);
        assert_eq!(one_blade(last, 1.0, MAT_OTHER, "Cu"), 1.0);
        let inside = one_blade(last - 0.001, 1.0, MAT_OTHER, "Cu");
        assert!(inside > 0.0 && inside < 1.0);
    }

    /// C guards the multiply with `if (xOther1 > 0)` (pf4.st:696), so a blade
    /// of zero thickness contributes 1.0 even with a bad material name.
    #[test]
    fn test_r6_63_other_zero_thickness_stays_transparent() {
        assert_eq!(one_blade(10.0, 0.0, MAT_OTHER, "Unobtainium"), 1.0);
    }

    /// CBUG-B1 — the "Other" blade's transmission comes from the interval
    /// containing the energy, where C extrapolates off the one above it:
    /// `absLen(Cu, 10 keV) = 5.28411457133 um -> 0.827582511947` for 1 um.
    #[test]
    fn test_b1_other_in_range_uses_the_containing_interval() {
        let cu = crate::data::chantler::find_material("Cu").unwrap();
        let abs_len_um = 1.0e4 / (cu.density * 215.521099278);
        let want = (-1.0 / abs_len_um).exp();
        let t = one_blade(10.0, 0.001, MAT_OTHER, "Cu");
        assert!(
            (t - want).abs() < 1e-9,
            "Cu 1um at 10 keV: t={t}, want={want}"
        );
        assert!((t - 0.827582511947).abs() > 1e-3);
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

    /// TiAbsorptionLength returns 1/0 = +inf below 1 keV, so Ti transmits fully;
    /// GlassAbsorptionLength returns 0 below 2 keV, so glass absorbs fully.
    #[test]
    fn test_ti_and_glass_below_their_fits() {
        assert!(ti_absorption_length_microns(0.5).is_infinite());
        assert_eq!(one_blade(0.5, 1.0, MAT_TI, ""), 1.0);
        assert_eq!(glass_absorption_length_microns(1.5), 0.0);
        assert_eq!(one_blade(1.5, 1.0, MAT_GLASS, ""), 0.0);
    }

    #[test]
    fn test_glass_is_analytic_fit_not_silicon() {
        // GlassAbsorptionLength(10 keV) ~ 190 microns (above-Iron-K branch * keV^3).
        let len = glass_absorption_length_microns(10.0);
        assert!(
            (185.0..195.0).contains(&len),
            "Glass absLen at 10keV = {len}"
        );
        let t = one_blade(10.0, 1.0, MAT_GLASS, "");
        assert!((4.5e-3..6.0e-3).contains(&t), "Glass 1mm at 10keV = {t}");
    }

    /// `numInArray`: the first index holding the value, else 0.
    #[test]
    fn test_find_position() {
        let bits: [u8; NUM_COMBINATIONS] = std::array::from_fn(|i| (15 - i) as u8);
        assert_eq!(find_position(&bits, 15), 0);
        assert_eq!(find_position(&bits, 0), 15);
        assert_eq!(find_position(&bits, 16), 0);
    }

    #[test]
    fn test_thickness_by_material() {
        let in_beam = [true, false, true, false];
        let z = [MAT_AL, MAT_TI, MAT_AL, MAT_TI];
        let f = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(thickness_by_material(MAT_AL, true, in_beam, z, f), 4.0);
        assert_eq!(thickness_by_material(MAT_TI, true, in_beam, z, f), 0.0);
        assert_eq!(thickness_by_material(MAT_AL, false, in_beam, z, f), 0.0);
    }

    /// `fillShowArrays`: the table, then its last entry up to `N`; an unknown
    /// species fills nothing.
    #[test]
    fn fill_show_arrays_pads_with_the_last_entry() {
        let cu = crate::data::chantler::find_material("Cu").unwrap();
        let n = cu.kev.len();
        let (mut e, mut t) = ([0.0f32; SHOW_LEN], [0.0f32; SHOW_LEN]);
        assert!(!fill_show_arrays("Unobtainium", SHOW_LEN, &mut e, &mut t));
        assert_eq!(e, [0.0; SHOW_LEN]);
        assert!(fill_show_arrays("Cu", SHOW_LEN, &mut e, &mut t));
        assert_eq!(&e[..n], cu.kev);
        assert_eq!(&t[..n], cu.mu);
        assert!(e[n..].iter().all(|&v| v == cu.kev[n - 1]));
        assert!(t[n..].iter().all(|&v| v == cu.mu[n - 1]));
    }

    /// `efTestAndClear(a) || efTestAndClear(b) || ...` takes one flag a pass.
    #[test]
    fn take_first_clears_only_the_first_set_flag() {
        let mut f = [false, true, true, false];
        assert!(take_first(&mut f, 0..4));
        assert_eq!(f, [false, false, true, false]);
        let mut b = [true, false, false, true];
        assert!(take_first(&mut b, [3, 2, 1, 0]));
        assert_eq!(b, [true, false, false, false]);
        assert!(!take_first(&mut [false; 4], 0..4));
    }
}
