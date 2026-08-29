//! Femto amplifier gain control — native Rust port of `femto.st`.
//!
//! This implements the gain selection state machine for Femto low-noise
//! current amplifiers. The amplifier gain is controlled by 4 digital bits
//! (G1, G2, G3, NO) which map to a gain index via a lookup table.
//!
//! # Gain Lookup Table
//!
//! | Index | Bits (NO:G3:G2:G1) | Gain (V/A) |
//! |-------|-------------------|------------|
//! |   0   | 0:0:0:0           | 10^5       |
//! |   1   | 0:0:0:1           | 10^6       |
//! |   2   | 0:0:1:0           | 10^7       |
//! |   3   | 0:0:1:1           | 10^8       |
//! |   4   | 0:1:0:0           | 10^9       |
//! |   5   | 0:1:0:1           | 10^10      |
//! |   6   | 0:1:1:0           | 10^11      |
//! |   7   | 0:1:1:1           | (unused)   |
//! |   8   | 1:0:0:0           | 10^3       |
//! |   9   | 1:0:0:1           | 10^4       |
//! |  10   | 1:0:1:0           | 10^5       |
//! |  11   | 1:0:1:1           | 10^6       |
//! |  12   | 1:1:0:0           | 10^7       |
//! |  13   | 1:1:0:1           | 10^8       |
//! |  14   | 1:1:1:0           | 10^9       |
//! |  15   | 1:1:1:1           | (unused)   |

/// Gain power lookup: `gain = 10^POWERS[gainidx]`.
/// Index 7 and 15 are unused (mapped to power 0).
pub const POWERS: [u32; 16] = [5, 6, 7, 8, 9, 10, 11, 0, 3, 4, 5, 6, 7, 8, 9, 0];

pub const MIN_GAIN: i32 = 0;
pub const MAX_GAIN: i32 = 15;
pub const UNUSED_GAIN: i32 = 7;

/// Decode 4 gain bits into a gain index (0–15).
pub fn bits_to_gain_index(g1: bool, g2: bool, g3: bool, no: bool) -> i32 {
    let t0 = g1 as i32;
    let t1 = g2 as i32;
    let t2 = g3 as i32;
    let tx = no as i32;
    (tx << 3) | (t2 << 2) | (t1 << 1) | t0
}

/// Encode a gain index into 4 gain bits (g1, g2, g3, no).
pub fn gain_index_to_bits(idx: i32) -> (bool, bool, bool, bool) {
    let g1 = (idx & 1) != 0;
    let g2 = (idx & 2) != 0;
    let g3 = (idx & 4) != 0;
    let no = (idx & 8) != 0;
    (g1, g2, g3, no)
}

/// Validate a gain index. Returns `true` if valid.
pub fn is_valid_gain_index(idx: i32) -> bool {
    (MIN_GAIN..MAX_GAIN).contains(&idx) && idx != UNUSED_GAIN
}

/// Compute the gain value for a given index.
pub fn gain_for_index(idx: i32) -> f64 {
    if !(0..16).contains(&idx) {
        return 0.0;
    }
    10.0_f64.powi(POWERS[idx as usize] as i32)
}

/// State of the femto amplifier state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FemtoState {
    Init,
    Idle,
    ChangeGain,
    UpdateGain,
}

/// The PVs one [`FemtoController::step`] writes, and only those.
///
/// `femto.st` writes different subsets in different clauses — `init` and
/// `updateGain` put `gainidx` and `gain` (`:77-78`, `:109-110`), the reverting
/// `changeGain` arms put `gainidx` and `gain` (`:127-128`, `:135-136`,
/// `:143-144`), its applying arm puts the four bits and `gain` but NOT
/// `gainidx` (`:157-281`), and its "No gain change required" arm puts nothing
/// at all (`:120-123`). A step that returned only the new state could not tell
/// those apart, and a runner that wrote every field every time would drive the
/// bit PVs on the arm C leaves silent — so the step returns what it wrote and
/// the runner puts exactly that.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FemtoPuts {
    /// `PVPUT(gainidx, ..)`
    pub gain_index: Option<i32>,
    /// `PVPUT(gain, pow(10, powers[..]))`
    pub gain: Option<f64>,
    /// `PVPUT(t0, ..)` … `PVPUT(tx, ..)` as `(g1, g2, g3, no)`.
    pub bits: Option<(bool, bool, bool, bool)>,
}

impl FemtoState {
    /// True where the state's `when` clauses are exhaustive, so SNL leaves it
    /// on the same evaluation that entered it: `init` and `updateGain` are a
    /// bare `when ()` (`femto.st:49`, `:96`) and `changeGain` ends in one
    /// (`:149`). Only `idle` waits. A runner must keep stepping while this
    /// holds.
    pub fn is_transient(self) -> bool {
        !matches!(self, FemtoState::Idle)
    }
}

/// Femto amplifier gain controller.
///
/// Port of the `femto.st` SNL program as a pure Rust state machine.
/// Call `step()` to advance the state machine when events occur.
pub struct FemtoController {
    pub state: FemtoState,
    pub gain_index: i32,
    pub current_gain: i32,
    pub g1: bool,
    pub g2: bool,
    pub g3: bool,
    pub no: bool,
    pub gain: f64,
}

impl Default for FemtoController {
    fn default() -> Self {
        Self {
            state: FemtoState::Init,
            gain_index: 0,
            current_gain: -1,
            g1: false,
            g2: false,
            g3: false,
            no: false,
            gain: 0.0,
        }
    }
}

/// Events that drive the femto state machine.
#[derive(Debug, Clone, Copy)]
pub enum FemtoEvent {
    /// Gain bits changed from hardware.
    BitsChanged {
        g1: bool,
        g2: bool,
        g3: bool,
        no: bool,
    },
    /// User requested a specific gain index.
    GainIndexChanged(i32),
}

impl FemtoController {
    /// Advance the state machine by one step given an event.
    /// Returns the new state and the PVs this step wrote.
    pub fn step(&mut self, event: Option<FemtoEvent>) -> (FemtoState, FemtoPuts) {
        let mut puts = FemtoPuts::default();
        match self.state {
            FemtoState::Init => {
                // Initialize from current bit state
                if let Some(FemtoEvent::BitsChanged { g1, g2, g3, no }) = event {
                    self.g1 = g1;
                    self.g2 = g2;
                    self.g3 = g3;
                    self.no = no;
                }

                let idx = bits_to_gain_index(self.g1, self.g2, self.g3, self.no);
                self.gain_index = if !self.g1 && !self.g2 && !self.g3 && !self.no {
                    8 // Default to 1e3 when all bits are off
                } else if !is_valid_gain_index(idx) {
                    6 // Default to 1e11
                } else {
                    idx
                };

                self.current_gain = -1;
                self.gain = gain_for_index(self.gain_index);
                // `femto.st:77-78`
                puts.gain_index = Some(self.gain_index);
                puts.gain = Some(self.gain);
                self.state = FemtoState::ChangeGain;
            }

            FemtoState::Idle => match event {
                Some(FemtoEvent::BitsChanged { g1, g2, g3, no }) => {
                    self.g1 = g1;
                    self.g2 = g2;
                    self.g3 = g3;
                    self.no = no;
                    self.state = FemtoState::UpdateGain;
                }
                Some(FemtoEvent::GainIndexChanged(idx)) => {
                    self.gain_index = idx;
                    self.state = FemtoState::ChangeGain;
                }
                None => {}
            },

            FemtoState::ChangeGain => {
                // Validate requested gain
                if self.current_gain == self.gain_index || !is_valid_gain_index(self.gain_index) {
                    // Invalid or no change: revert to current gain
                    if self.current_gain >= 0 && self.current_gain != self.gain_index {
                        self.gain_index = self.current_gain;
                        self.gain = gain_for_index(self.current_gain);
                        // The three reverting arms (`femto.st:127-128`,
                        // `:135-136`, `:143-144`). "No gain change required"
                        // (`:120-123`) falls through here writing nothing,
                        // which is what that arm does.
                        puts.gain_index = Some(self.gain_index);
                        puts.gain = Some(self.gain);
                    }
                    self.state = FemtoState::Idle;
                } else {
                    // Apply gain: set bits
                    let (g1, g2, g3, no) = gain_index_to_bits(self.gain_index);
                    self.g1 = g1;
                    self.g2 = g2;
                    self.g3 = g3;
                    self.no = no;
                    self.current_gain = self.gain_index;
                    self.gain = gain_for_index(self.gain_index);
                    // The applying arm writes the bits and `gain`, never
                    // `gainidx` — the put that got it here already carries it
                    // (`femto.st:157-281`).
                    puts.bits = Some((g1, g2, g3, no));
                    puts.gain = Some(self.gain);
                    self.state = FemtoState::Idle;
                }
            }

            FemtoState::UpdateGain => {
                // Bits changed externally: recompute gain index
                let idx = bits_to_gain_index(self.g1, self.g2, self.g3, self.no);
                self.gain_index = if !is_valid_gain_index(idx) { 6 } else { idx };
                self.current_gain = self.gain_index;
                self.gain = gain_for_index(self.gain_index);
                // `femto.st:109-110`
                puts.gain_index = Some(self.gain_index);
                puts.gain = Some(self.gain);
                self.state = FemtoState::Idle;
            }
        }

        (self.state, puts)
    }
}

// ---------------------------------------------------------------------------
// Runner — binds the state machine above to `femto.db`
// ---------------------------------------------------------------------------

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::{DbChannel, DbMultiMonitor, alloc_origin};

/// The macros of
/// `program femto("name=femto,P=,H=,F=,G1=,G2=,G3=,NO=")` (`femto.st:15`).
///
/// `P`/`H`/`F` compose the amplifier's own PVs as `{P}{H}{F}<leaf>`; the four
/// gain-bit PVs are whole names given by the caller, because they belong to
/// whatever binary I/O carries the amplifier's control lines.
#[derive(Debug, Clone)]
pub struct FemtoConfig {
    pub prefix: String,
    pub hardware: String,
    pub function: String,
    pub gain_bit_pvs: [String; 3],
    pub noise_bit_pv: String,
}

impl FemtoConfig {
    pub fn new(p: &str, h: &str, f: &str, g1: &str, g2: &str, g3: &str, no: &str) -> Self {
        Self {
            prefix: p.to_string(),
            hardware: h.to_string(),
            function: f.to_string(),
            gain_bit_pvs: [g1.to_string(), g2.to_string(), g3.to_string()],
            noise_bit_pv: no.to_string(),
        }
    }

    /// `{P}{H}{F}<leaf>` (`femto.st:34-36`). No separator: the C macro
    /// concatenates, and `femto.db` names its records the same way.
    pub fn pv(&self, leaf: &str) -> String {
        format!("{}{}{}{}", self.prefix, self.hardware, self.function, leaf)
    }
}

/// `DEBUG_PRINT(level, msg)` (`seqPVmacros.h:231-236`).
fn debug_print(debug_flag: i32, level: i32, msg: &str) {
    if debug_flag >= level {
        println!("<femto.st,{level},femto> {msg}");
    }
}

/// Run `femto.st` against the records `femto.db` loaded.
///
/// `idle` is the only state that waits — `init`, `changeGain` and
/// `updateGain` all end in a bare `when ()` — so the runner steps while
/// [`FemtoState::is_transient`] holds and then blocks on the monitors. Each
/// step reports the PVs it wrote as a [`FemtoPuts`], and the runner writes
/// exactly those: the applying `changeGain` arm drives the amplifier's bit
/// PVs, and the arm that finds no change needed drives nothing.
///
/// A `{P}{H}{F}debug` monitor updates the level without re-evaluating, which
/// is what C's first `idle` clause amounts to — it prints and re-enters
/// `idle` (`femto.st:295-299`).
///
/// Same one deviation as `delayDo`: a PV the database never loaded is an
/// error return here, where C sits in `init` waiting for a connection that
/// never comes.
pub async fn run(
    config: FemtoConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let origin = alloc_origin();

    let pv_t0 = config.gain_bit_pvs[0].clone();
    let pv_t1 = config.gain_bit_pvs[1].clone();
    let pv_t2 = config.gain_bit_pvs[2].clone();
    let pv_tx = config.noise_bit_pv.clone();
    let pv_gainidx = config.pv("gainidx");
    let pv_debug = config.pv("debug");
    // `PV(double, gain, "{P}{H}{F}gain", NoMon)` — written, never monitored.
    let ch_gain = DbChannel::with_origin(&db, &config.pv("gain"), origin);

    let ch_t0 = DbChannel::with_origin(&db, &pv_t0, origin);
    let ch_t1 = DbChannel::with_origin(&db, &pv_t1, origin);
    let ch_t2 = DbChannel::with_origin(&db, &pv_t2, origin);
    let ch_tx = DbChannel::with_origin(&db, &pv_tx, origin);
    let ch_gainidx = DbChannel::with_origin(&db, &pv_gainidx, origin);
    let ch_debug = DbChannel::with_origin(&db, &pv_debug, origin);

    let monitored = vec![
        pv_t0.clone(),
        pv_t1.clone(),
        pv_t2.clone(),
        pv_tx.clone(),
        pv_gainidx.clone(),
        pv_debug.clone(),
    ];
    let mut monitor = DbMultiMonitor::new_filtered(&db, &monitored, origin).await;
    if monitor.sub_count() != monitored.len() {
        return Err(format!(
            "femto: {} of the {} PVs it assigns are not in the database ({})",
            monitored.len() - monitor.sub_count(),
            monitored.len(),
            monitored.join(", ")
        )
        .into());
    }

    let mut debug_flag = ch_debug.get_i32().await;
    let mut ctrl = FemtoController::default();
    // `pvGet(t0..tx, SYNC)` at the top of `init` (`femto.st:53-56`).
    let mut event = Some(FemtoEvent::BitsChanged {
        g1: ch_t0.get_i32().await != 0,
        g2: ch_t1.get_i32().await != 0,
        g3: ch_t2.get_i32().await != 0,
        no: ch_tx.get_i32().await != 0,
    });

    loop {
        loop {
            let previous = ctrl.state;
            let (state, puts) = ctrl.step(event.take());
            if let Some((g1, g2, g3, no)) = puts.bits {
                let _ = ch_t0.put_i32_process(g1 as i32).await;
                let _ = ch_t1.put_i32_process(g2 as i32).await;
                let _ = ch_t2.put_i32_process(g3 as i32).await;
                let _ = ch_tx.put_i32_process(no as i32).await;
            }
            if let Some(idx) = puts.gain_index {
                let _ = ch_gainidx.put_i32_process(idx).await;
            }
            if let Some(gain) = puts.gain {
                let _ = ch_gain.put_f64_process(gain).await;
            }
            if state != previous {
                debug_print(debug_flag, 2, &format!("{previous:?} -> {state:?}"));
            }
            if !state.is_transient() {
                break;
            }
        }

        loop {
            let (pv, value) = monitor.wait_change().await;
            if pv == pv_debug {
                // `when( efTestAndClear(debug_flag_mon) )` prints and re-enters
                // `idle` (`femto.st:295-299`) — no clause tests it, so
                // re-evaluating would only consume flags nothing acted on.
                debug_flag = value as i32;
                debug_print(
                    debug_flag,
                    1,
                    &format!("Debug level changed to {debug_flag}"),
                );
                continue;
            }
            if pv == pv_gainidx {
                event = Some(FemtoEvent::GainIndexChanged(value as i32));
            } else {
                // Any of the four bit monitors: C's second `idle` clause tests
                // all four together (`femto.st:301-303`) and `updateGain`
                // recomputes from the whole set, so the event carries it.
                let mut g1 = ctrl.g1;
                let mut g2 = ctrl.g2;
                let mut g3 = ctrl.g3;
                let mut no = ctrl.no;
                let level = (value as i32) != 0;
                if pv == pv_t0 {
                    g1 = level;
                } else if pv == pv_t1 {
                    g2 = level;
                } else if pv == pv_t2 {
                    g3 = level;
                } else if pv == pv_tx {
                    no = level;
                }
                event = Some(FemtoEvent::BitsChanged { g1, g2, g3, no });
            }
            break;
        }
    }
}
