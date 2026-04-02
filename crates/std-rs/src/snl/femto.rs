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
    /// Returns the new state.
    pub fn step(&mut self, event: Option<FemtoEvent>) -> FemtoState {
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
                    self.state = FemtoState::Idle;
                }
            }

            FemtoState::UpdateGain => {
                // Bits changed externally: recompute gain index
                let idx = bits_to_gain_index(self.g1, self.g2, self.g3, self.no);
                self.gain_index = if !is_valid_gain_index(idx) { 6 } else { idx };
                self.current_gain = self.gain_index;
                self.gain = gain_for_index(self.gain_index);
                self.state = FemtoState::Idle;
            }
        }

        self.state
    }
}
