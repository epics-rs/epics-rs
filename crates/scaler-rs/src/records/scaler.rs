use std::any::Any;
use std::time::Instant;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{
    FieldDesc, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Maximum number of scaler channels.
pub const MAX_SCALER_CHANNELS: usize = 64;

const VERSION: f32 = 3.19;

// Scaler hardware state
const SCALER_STATE_IDLE: i16 = 0;
const SCALER_STATE_WAITING: i16 = 1;
const SCALER_STATE_COUNTING: i16 = 2;

// User request state
const USER_STATE_IDLE: i16 = 0;
const USER_STATE_WAITING: i16 = 1;
const USER_STATE_REQSTART: i16 = 2;
const USER_STATE_COUNTING: i16 = 3;

/// Device command names used in ProcessAction::DeviceCommand.
pub const CMD_RESET: &str = "scaler_reset";
pub const CMD_ARM: &str = "scaler_arm";
pub const CMD_WRITE_PRESET: &str = "scaler_write_preset";

/// Scaler record — up to 64-channel 32-bit counter with preset and auto-count.
///
/// Ported from EPICS scaler module `scalerRecord.c`.
///
/// Each channel has:
/// - S{n}: current count value (read-only)
/// - PR{n}: preset count value
/// - G{n}: gate/preset enable (N/Y)
/// - D{n}: count direction (Up/Dn)
/// - NM{n}: channel name
///
/// Channel 1 (S1/PR1) is the time-base channel: T = S1 / FREQ.
///
/// **Driver integration**: The record does NOT hold a direct driver reference.
/// - `check_done()` and `read_counts()` are performed by device support's
///   `read()` BEFORE process(), writing results into the record's fields.
/// - `reset`, `write_preset`, `arm` commands are sent as
///   `ProcessAction::DeviceCommand` and executed by the framework via
///   `DeviceSupport::handle_command()` AFTER process().
///
/// **DLY/DLY1:** Implemented via `ProcessAction::ReprocessAfter`.
pub struct ScalerRecord {
    // --- Control/Status ---
    pub val: f64,
    pub freq: f64,
    pub cnt: i16,
    pub pcnt: i16,
    pub ss: i16,
    pub us: i16,
    pub cont: i16,
    pub rate: f32,
    pub rat1: f32,
    pub dly: f32,
    pub dly1: f32,
    pub nch: i16,
    pub tp: f64,
    pub tp1: f64,
    pub t: f64,
    pub vers: f32,
    pub prec: i16,
    pub egu: String,
    pub out: String,
    pub cout: String,
    pub coutp: String,

    // --- Per-channel arrays (64 channels) ---
    pub d: [i16; MAX_SCALER_CHANNELS],
    pub g: [i16; MAX_SCALER_CHANNELS],
    pub pr: [u32; MAX_SCALER_CHANNELS],
    pub s: [u32; MAX_SCALER_CHANNELS],
    pub nm: [String; MAX_SCALER_CHANNELS],

    // --- Delay tracking ---
    delay_start: Option<Instant>,

    // --- Done flag (set by device support read, consumed by process) ---
    /// Set by device support's read() when counting has completed.
    /// process() checks and clears this flag.
    pub(crate) done_flag: bool,
}

impl Default for ScalerRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            freq: 1.0e7,
            cnt: 0,
            pcnt: 0,
            ss: SCALER_STATE_IDLE,
            us: USER_STATE_IDLE,
            cont: 0,
            rate: 10.0,
            rat1: 0.0,
            dly: 0.0,
            dly1: 0.0,
            nch: 0,
            tp: 1.0,
            tp1: 1.0,
            t: 0.0,
            vers: VERSION,
            prec: 0,
            egu: String::new(),
            out: String::new(),
            cout: String::new(),
            coutp: String::new(),
            d: {
                let mut d = [0i16; MAX_SCALER_CHANNELS];
                d[0] = 1;
                d
            },
            g: [0; MAX_SCALER_CHANNELS],
            pr: [0; MAX_SCALER_CHANNELS],
            s: [0; MAX_SCALER_CHANNELS],
            nm: std::array::from_fn(|_| String::new()),
            delay_start: None,
            done_flag: false,
        }
    }
}

fn parse_indexed_field(name: &str, prefix: &str) -> Option<usize> {
    name.strip_prefix(prefix)
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&i| (1..=MAX_SCALER_CHANNELS).contains(&i))
        .map(|i| i - 1)
}

impl ScalerRecord {
    pub fn update_time(&mut self) {
        if self.freq > 0.0 {
            self.t = self.s[0] as f64 / self.freq;
        }
    }

    fn tp_to_pr1(&mut self) {
        self.pr[0] = (self.tp * self.freq) as u32;
        self.d[0] = 1;
        self.g[0] = 1;
    }

    fn pr1_to_tp(&mut self) {
        if self.freq > 0.0 {
            self.tp = self.pr[0] as f64 / self.freq;
        }
    }

    /// Check if counting is done via done_flag (set by device support)
    /// or internal preset check (when no device support).
    fn check_done(&self) -> bool {
        if self.done_flag {
            return true;
        }
        // Fallback: check if any gated channel reached preset
        self.g
            .iter()
            .enumerate()
            .any(|(i, &gate)| gate != 0 && self.pr[i] > 0 && self.s[i] >= self.pr[i])
    }

    /// Build DeviceCommand actions for a count start sequence:
    /// reset → write_preset for each gated channel → arm(true)
    fn build_start_actions(&self) -> Vec<ProcessAction> {
        let mut actions = Vec::new();

        // Reset
        actions.push(ProcessAction::DeviceCommand {
            command: CMD_RESET,
            args: vec![],
        });

        // Write presets for gated channels
        for i in 0..self.nch as usize {
            if self.g[i] != 0 {
                actions.push(ProcessAction::DeviceCommand {
                    command: CMD_WRITE_PRESET,
                    args: vec![
                        EpicsValue::Long(i as i32),
                        EpicsValue::Long(self.pr[i] as i32),
                    ],
                });
            }
        }

        // Arm
        actions.push(ProcessAction::DeviceCommand {
            command: CMD_ARM,
            args: vec![EpicsValue::Long(1)],
        });

        actions
    }

    /// Build DeviceCommand action to disarm.
    fn build_disarm_action() -> ProcessAction {
        ProcessAction::DeviceCommand {
            command: CMD_ARM,
            args: vec![EpicsValue::Long(0)],
        }
    }

    /// Build actions for auto-count start.
    fn build_autocount_actions(&self) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        actions.push(ProcessAction::DeviceCommand {
            command: CMD_RESET,
            args: vec![],
        });
        if self.tp1 >= 1.0e-3 {
            let auto_pr1 = (self.tp1 * self.freq) as u32;
            actions.push(ProcessAction::DeviceCommand {
                command: CMD_WRITE_PRESET,
                args: vec![EpicsValue::Long(0), EpicsValue::Long(auto_pr1 as i32)],
            });
        } else {
            for i in 0..self.nch as usize {
                if self.g[i] != 0 {
                    actions.push(ProcessAction::DeviceCommand {
                        command: CMD_WRITE_PRESET,
                        args: vec![
                            EpicsValue::Long(i as i32),
                            EpicsValue::Long(self.pr[i] as i32),
                        ],
                    });
                }
            }
        }
        actions.push(ProcessAction::DeviceCommand {
            command: CMD_ARM,
            args: vec![EpicsValue::Long(1)],
        });
        actions
    }
}

// Full FIELDS including indexed fields
use std::sync::LazyLock;

static ALL_FIELDS: LazyLock<Vec<FieldDesc>> = LazyLock::new(|| {
    let mut fields = vec![
        FieldDesc {
            name: "VAL",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "FREQ",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "CNT",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "PCNT",
            dbf_type: DbFieldType::Short,
            read_only: true,
        },
        FieldDesc {
            name: "SS",
            dbf_type: DbFieldType::Short,
            read_only: true,
        },
        FieldDesc {
            name: "US",
            dbf_type: DbFieldType::Short,
            read_only: true,
        },
        FieldDesc {
            name: "CONT",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "RATE",
            dbf_type: DbFieldType::Float,
            read_only: false,
        },
        FieldDesc {
            name: "RAT1",
            dbf_type: DbFieldType::Float,
            read_only: false,
        },
        FieldDesc {
            name: "DLY",
            dbf_type: DbFieldType::Float,
            read_only: false,
        },
        FieldDesc {
            name: "DLY1",
            dbf_type: DbFieldType::Float,
            read_only: false,
        },
        FieldDesc {
            name: "NCH",
            dbf_type: DbFieldType::Short,
            read_only: true,
        },
        FieldDesc {
            name: "TP",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "TP1",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "T",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "VERS",
            dbf_type: DbFieldType::Float,
            read_only: true,
        },
        FieldDesc {
            name: "PREC",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "EGU",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "OUT",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "COUT",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "COUTP",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
    ];
    for i in 1..=MAX_SCALER_CHANNELS {
        let s: &'static str = Box::leak(format!("S{}", i).into_boxed_str());
        fields.push(FieldDesc {
            name: s,
            dbf_type: DbFieldType::Long,
            read_only: true,
        });
    }
    for i in 1..=MAX_SCALER_CHANNELS {
        let pr: &'static str = Box::leak(format!("PR{}", i).into_boxed_str());
        fields.push(FieldDesc {
            name: pr,
            dbf_type: DbFieldType::Long,
            read_only: false,
        });
    }
    for i in 1..=MAX_SCALER_CHANNELS {
        let g: &'static str = Box::leak(format!("G{}", i).into_boxed_str());
        fields.push(FieldDesc {
            name: g,
            dbf_type: DbFieldType::Short,
            read_only: false,
        });
    }
    for i in 1..=MAX_SCALER_CHANNELS {
        let d: &'static str = Box::leak(format!("D{}", i).into_boxed_str());
        fields.push(FieldDesc {
            name: d,
            dbf_type: DbFieldType::Short,
            read_only: false,
        });
    }
    for i in 1..=MAX_SCALER_CHANNELS {
        let nm: &'static str = Box::leak(format!("NM{}", i).into_boxed_str());
        fields.push(FieldDesc {
            name: nm,
            dbf_type: DbFieldType::String,
            read_only: false,
        });
    }
    fields
});

impl Record for ScalerRecord {
    fn record_type(&self) -> &'static str {
        "scaler"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let prev_scaler_state = self.ss;
        let mut just_finished_user_count = false;
        let mut just_started_user_count = false;
        let mut actions = Vec::new();
        let mut fire_coutp = false;

        // Check if done counting (done_flag set by device support read,
        // or internal preset check when no device support)
        if self.ss == SCALER_STATE_COUNTING && self.check_done() {
            self.done_flag = false;
            self.ss = SCALER_STATE_IDLE;
            if self.us == USER_STATE_COUNTING {
                self.cnt = 0;
                self.us = USER_STATE_IDLE;
                just_finished_user_count = true;
            }
        }

        // DLY delay check
        if self.us == USER_STATE_WAITING && self.cnt != 0 {
            if let Some(start) = self.delay_start {
                let dly = self.dly.max(0.0) as f64;
                let elapsed = start.elapsed().as_secs_f64();
                if elapsed >= dly {
                    self.us = USER_STATE_REQSTART;
                    self.delay_start = None;
                } else {
                    let remaining = std::time::Duration::from_secs_f64(dly - elapsed);
                    return Ok(ProcessOutcome {
                        result: RecordProcessResult::AsyncPending,
                        actions: vec![ProcessAction::ReprocessAfter(remaining)],
                        device_did_compute: false,
                    });
                }
            }
        }

        // Handle CNT state change
        if self.cnt != self.pcnt {
            let mut handled = false;
            if self.cnt != 0 && (self.us == USER_STATE_REQSTART || self.us == USER_STATE_WAITING) {
                // Stop any existing auto-count via DeviceCommand
                if self.ss == SCALER_STATE_COUNTING {
                    actions.push(Self::build_disarm_action());
                    self.ss = SCALER_STATE_IDLE;
                }

                if self.us == USER_STATE_REQSTART {
                    // Ensure PR1 matches TP * FREQ
                    let expected_pr1 = (self.tp * self.freq + 0.5) as u32;
                    if self.pr[0] != expected_pr1 {
                        self.pr[0] = expected_pr1;
                    }

                    // Set directions from gates
                    for i in 0..MAX_SCALER_CHANNELS {
                        self.d[i] = self.g[i];
                    }

                    // Queue reset → write_presets → arm via DeviceCommands
                    actions.extend(self.build_start_actions());
                    self.ss = SCALER_STATE_COUNTING;
                    self.us = USER_STATE_COUNTING;
                    just_started_user_count = true;
                    handled = true;
                }
            } else if self.cnt == 0 {
                if self.ss != SCALER_STATE_IDLE {
                    actions.push(Self::build_disarm_action());
                }
                self.ss = SCALER_STATE_IDLE;
                self.us = USER_STATE_IDLE;
                just_finished_user_count = true;
                handled = true;
            }
            if handled {
                self.pcnt = self.cnt;
            }
        }

        // Update elapsed time (counts already read by device support's read())
        self.update_time();

        // Periodic display update during counting
        if self.ss == SCALER_STATE_COUNTING {
            let rate = if self.us == USER_STATE_COUNTING {
                self.rate
            } else {
                self.rat1
            };
            if rate > 0.1 {
                let interval = std::time::Duration::from_secs_f64(1.0 / rate as f64);
                actions.push(ProcessAction::ReprocessAfter(interval));
            }
        }

        // COUT/COUTP
        if just_started_user_count || just_finished_user_count {
            actions.push(ProcessAction::WriteDbLink {
                link_field: "COUT",
                value: EpicsValue::Short(self.cnt),
            });
            if just_finished_user_count {
                fire_coutp = true;
            }
        }
        if fire_coutp {
            actions.push(ProcessAction::WriteDbLink {
                link_field: "COUTP",
                value: EpicsValue::Short(self.cnt),
            });
        }

        // VAL = T on completion
        if self.ss == SCALER_STATE_IDLE && self.pcnt == 0 && self.us == USER_STATE_IDLE {
            if prev_scaler_state == SCALER_STATE_COUNTING {
                self.val = self.t;
            }
        }

        // AutoCount
        if self.us == USER_STATE_IDLE && self.cont != 0 && self.ss != SCALER_STATE_COUNTING {
            if self.ss != SCALER_STATE_WAITING {
                if self.dly1 > 0.0 {
                    self.ss = SCALER_STATE_WAITING;
                    self.delay_start = Some(Instant::now());
                    let delay = std::time::Duration::from_secs_f64(self.dly1.max(0.0) as f64);
                    actions.push(ProcessAction::ReprocessAfter(delay));
                    return Ok(ProcessOutcome {
                        result: RecordProcessResult::Complete,
                        actions,
                        device_did_compute: false,
                    });
                } else {
                    actions.extend(self.build_autocount_actions());
                    self.ss = SCALER_STATE_COUNTING;
                }
            } else {
                let elapsed = self
                    .delay_start
                    .map(|s| s.elapsed().as_secs_f64())
                    .unwrap_or(f64::MAX);
                if elapsed >= self.dly1.max(0.0) as f64 {
                    self.delay_start = None;
                    actions.extend(self.build_autocount_actions());
                    self.ss = SCALER_STATE_COUNTING;
                } else {
                    let remaining = (self.dly1.max(0.0) as f64) - elapsed;
                    actions.push(ProcessAction::ReprocessAfter(
                        std::time::Duration::from_secs_f64(remaining),
                    ));
                    return Ok(ProcessOutcome {
                        result: RecordProcessResult::Complete,
                        actions,
                        device_did_compute: false,
                    });
                }
            }
        }

        Ok(ProcessOutcome::complete_with(actions))
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        match field {
            "CNT" => {
                if self.cnt != 0 && self.us != USER_STATE_IDLE {
                    return Ok(());
                }
                if self.cnt != 0 {
                    let dly = self.dly.max(0.0);
                    if dly == 0.0 {
                        self.us = USER_STATE_REQSTART;
                    } else {
                        self.us = USER_STATE_WAITING;
                        self.delay_start = Some(Instant::now());
                    }
                } else {
                    match self.us {
                        USER_STATE_WAITING | USER_STATE_REQSTART => {
                            self.us = USER_STATE_IDLE;
                        }
                        _ => {}
                    }
                }
            }
            "CONT" => {}
            "TP" => {
                self.tp_to_pr1();
            }
            "TP1" => {}
            "RATE" => {
                self.rate = self.rate.clamp(0.0, 60.0);
            }
            "RAT1" => {
                self.rat1 = self.rat1.clamp(0.0, 60.0);
            }
            _ => {
                if field == "PR1" {
                    self.pr1_to_tp();
                    if self.tp > 0.0 {
                        self.d[0] = 1;
                        self.g[0] = 1;
                    }
                } else if let Some(i) = parse_indexed_field(field, "PR") {
                    if self.pr[i] > 0 {
                        self.d[i] = 1;
                        self.g[i] = 1;
                    }
                } else if let Some(i) = parse_indexed_field(field, "G") {
                    if self.g[i] != 0 && self.pr[i] == 0 {
                        self.pr[i] = 1000;
                    }
                }
            }
        }
        Ok(())
    }

    fn should_fire_forward_link(&self) -> bool {
        self.ss == SCALER_STATE_IDLE && self.us == USER_STATE_IDLE && self.pcnt == 0
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => return Some(EpicsValue::Double(self.val)),
            "FREQ" => return Some(EpicsValue::Double(self.freq)),
            "CNT" => return Some(EpicsValue::Short(self.cnt)),
            "PCNT" => return Some(EpicsValue::Short(self.pcnt)),
            "SS" => return Some(EpicsValue::Short(self.ss)),
            "US" => return Some(EpicsValue::Short(self.us)),
            "CONT" => return Some(EpicsValue::Short(self.cont)),
            "RATE" => return Some(EpicsValue::Float(self.rate)),
            "RAT1" => return Some(EpicsValue::Float(self.rat1)),
            "DLY" => return Some(EpicsValue::Float(self.dly)),
            "DLY1" => return Some(EpicsValue::Float(self.dly1)),
            "NCH" => return Some(EpicsValue::Short(self.nch)),
            "TP" => return Some(EpicsValue::Double(self.tp)),
            "TP1" => return Some(EpicsValue::Double(self.tp1)),
            "T" => return Some(EpicsValue::Double(self.t)),
            "VERS" => return Some(EpicsValue::Float(self.vers)),
            "PREC" => return Some(EpicsValue::Short(self.prec)),
            "EGU" => return Some(EpicsValue::String(self.egu.clone())),
            "OUT" => return Some(EpicsValue::String(self.out.clone())),
            "COUT" => return Some(EpicsValue::String(self.cout.clone())),
            "COUTP" => return Some(EpicsValue::String(self.coutp.clone())),
            _ => {}
        }
        if let Some(i) = parse_indexed_field(name, "NM") {
            return Some(EpicsValue::String(self.nm[i].clone()));
        }
        if let Some(i) = parse_indexed_field(name, "PR") {
            return Some(EpicsValue::Long(self.pr[i] as i32));
        }
        if let Some(i) = parse_indexed_field(name, "S") {
            return Some(EpicsValue::Long(self.s[i] as i32));
        }
        if let Some(i) = parse_indexed_field(name, "G") {
            return Some(EpicsValue::Short(self.g[i]));
        }
        if let Some(i) = parse_indexed_field(name, "D") {
            return Some(EpicsValue::Short(self.d[i]));
        }
        None
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "FREQ" => match value {
                EpicsValue::Double(v) => {
                    self.freq = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "CNT" => match value {
                EpicsValue::Short(v) => {
                    self.cnt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "CONT" => match value {
                EpicsValue::Short(v) => {
                    self.cont = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "RATE" => match value {
                EpicsValue::Float(v) => {
                    self.rate = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "RAT1" => match value {
                EpicsValue::Float(v) => {
                    self.rat1 = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DLY" => match value {
                EpicsValue::Float(v) => {
                    self.dly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DLY1" => match value {
                EpicsValue::Float(v) => {
                    self.dly1 = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "TP" => match value {
                EpicsValue::Double(v) => {
                    self.tp = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "TP1" => match value {
                EpicsValue::Double(v) => {
                    self.tp1 = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGU" => match value {
                EpicsValue::String(v) => {
                    self.egu = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OUT" => match value {
                EpicsValue::String(v) => {
                    self.out = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "COUT" => match value {
                EpicsValue::String(v) => {
                    self.cout = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "COUTP" => match value {
                EpicsValue::String(v) => {
                    self.coutp = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "PCNT" | "SS" | "US" | "NCH" | "T" | "VERS" => Err(CaError::ReadOnlyField(name.into())),
            _ => {
                if let Some(i) = parse_indexed_field(name, "NM") {
                    match value {
                        EpicsValue::String(v) => {
                            self.nm[i] = v;
                            Ok(())
                        }
                        _ => Err(CaError::TypeMismatch(name.into())),
                    }
                } else if let Some(i) = parse_indexed_field(name, "PR") {
                    match value {
                        EpicsValue::Long(v) => {
                            self.pr[i] = v as u32;
                            Ok(())
                        }
                        _ => Err(CaError::TypeMismatch(name.into())),
                    }
                } else if let Some(i) = parse_indexed_field(name, "G") {
                    match value {
                        EpicsValue::Short(v) => {
                            self.g[i] = v;
                            Ok(())
                        }
                        _ => Err(CaError::TypeMismatch(name.into())),
                    }
                } else if let Some(i) = parse_indexed_field(name, "D") {
                    match value {
                        EpicsValue::Short(v) => {
                            self.d[i] = v;
                            Ok(())
                        }
                        _ => Err(CaError::TypeMismatch(name.into())),
                    }
                } else if parse_indexed_field(name, "S").is_some() {
                    Err(CaError::ReadOnlyField(name.into()))
                } else {
                    Err(CaError::FieldNotFound(name.into()))
                }
            }
        }
    }

    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        // Allow framework to write S1-S64 (read-only) from device support read
        if let Some(i) = parse_indexed_field(name, "S") {
            match value {
                EpicsValue::Long(v) => {
                    self.s[i] = v as u32;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            }
        } else {
            self.put_field(name, value)
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        let fields: &Vec<FieldDesc> = &ALL_FIELDS;
        unsafe { std::slice::from_raw_parts(fields.as_ptr(), fields.len()) }
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.vers = VERSION;
            return Ok(());
        }
        if self.freq == 0.0 {
            self.freq = 1.0e7;
        }
        if self.tp > 0.0 {
            self.pr[0] = (self.tp * self.freq) as u32;
        } else if self.pr[0] > 0 && self.freq > 0.0 {
            self.tp = self.pr[0] as f64 / self.freq;
        } else {
            self.tp = 1.0;
            self.pr[0] = (self.tp * self.freq) as u32;
        }
        Ok(())
    }
}
