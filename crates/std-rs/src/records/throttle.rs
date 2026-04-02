use std::time::Instant;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{
    FieldDesc, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Throttle record — rate-limits value changes to prevent device damage.
///
/// Ported from EPICS std module `throttleRecord.c`.
///
/// When VAL is written, the record checks drive limits, optionally clips
/// the value, sets WAIT=True, then writes SENT to the OUT link only after
/// the minimum delay (DLY) has elapsed since the last output. If a new
/// value arrives during the delay, it queues the latest value and sends
/// it when the delay expires.
pub struct ThrottleRecord {
    /// Set value (VAL)
    pub val: f64,
    /// Previous set value (OVAL), read-only
    pub oval: f64,
    /// Last sent value (SENT), read-only
    pub sent: f64,
    /// Previous sent value (OSENT), read-only
    pub osent: f64,
    /// Busy flag (WAIT): 0=False, 1=True, read-only
    pub wait: i16,
    /// High operating range (HOPR)
    pub hopr: f64,
    /// Low operating range (LOPR)
    pub lopr: f64,
    /// High drive limit (DRVLH)
    pub drvlh: f64,
    /// Low drive limit (DRVLL)
    pub drvll: f64,
    /// Limit status: 0=Normal, 1=Low, 2=High (DRVLS), read-only
    pub drvls: i16,
    /// Limit clipping: 0=Off, 1=On (DRVLC)
    pub drvlc: i16,
    /// Code version string (VER), read-only
    pub ver: String,
    /// Record status: 0=Unknown, 1=Error, 2=Success (STS), read-only
    pub sts: i16,
    /// Display precision (PREC)
    pub prec: i16,
    /// Delay display precision (DPREC)
    pub dprec: i16,
    /// Delay between outputs in seconds (DLY)
    pub dly: f64,
    /// Output link (OUT)
    pub out: String,
    /// Output link valid: 0=ExtNC, 1=Ext, 2=Local, 3=Constant (OV), read-only
    pub ov: i16,
    /// Sync input link (SINP)
    pub sinp: String,
    /// Sync input link valid (SIV), read-only
    pub siv: i16,
    /// Sync trigger: 0=Idle, 1=Process (SYNC)
    pub sync: i16,

    // --- Private runtime state ---
    /// Whether limits are active (drvlh > drvll)
    limit_flag: bool,
    /// Whether a delay is currently in progress
    delay_active: bool,
    /// When the last output was sent (for delay enforcement)
    last_send_time: Option<Instant>,
    /// Value queued during delay period (sent when delay expires)
    pending_value: Option<f64>,
}

impl Default for ThrottleRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            oval: 0.0,
            sent: 0.0,
            osent: 0.0,
            wait: 0,
            hopr: 0.0,
            lopr: 0.0,
            drvlh: 0.0,
            drvll: 0.0,
            drvls: 0, // Normal
            drvlc: 0, // Off
            ver: "1.0.0".to_string(),
            sts: 0, // Unknown
            prec: 0,
            dprec: 0,
            dly: 0.0,
            out: String::new(),
            ov: 3, // Constant
            sinp: String::new(),
            siv: 3,  // Constant
            sync: 0, // Idle
            limit_flag: false,
            delay_active: false,
            last_send_time: None,
            pending_value: None,
        }
    }
}

impl ThrottleRecord {
    /// Check drive limits and optionally clip the value.
    /// Returns Ok(value) if the value is acceptable, Err if rejected.
    fn check_limits(&mut self, val: f64) -> Result<f64, ()> {
        if !self.limit_flag {
            self.drvls = 0; // Normal
            return Ok(val);
        }

        if val > self.drvlh {
            self.drvls = 2; // High
            if self.drvlc != 0 {
                return Ok(self.drvlh);
            }
            return Err(());
        }

        if val < self.drvll {
            self.drvls = 1; // Low
            if self.drvlc != 0 {
                return Ok(self.drvll);
            }
            return Err(());
        }

        self.drvls = 0; // Normal
        Ok(val)
    }

    /// Send the value to the output, updating SENT/OSENT and timing.
    fn send_value(&mut self, value: f64) {
        self.osent = self.sent;
        self.sent = value;
        self.last_send_time = Some(Instant::now());
        self.sts = 2; // Success
    }

    /// Check if the delay period has elapsed since last send.
    fn delay_elapsed(&self) -> bool {
        if self.dly <= 0.0 {
            return true;
        }
        match self.last_send_time {
            Some(t) => t.elapsed().as_secs_f64() >= self.dly,
            None => true, // Never sent before
        }
    }
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "SENT",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "OSENT",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "WAIT",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DRVLH",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DRVLL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DRVLS",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "DRVLC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "VER",
        dbf_type: DbFieldType::String,
        read_only: true,
    },
    FieldDesc {
        name: "STS",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DPREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLY",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "OUT",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "SINP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "SIV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "SYNC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

impl Record for ThrottleRecord {
    fn record_type(&self) -> &'static str {
        "throttle"
    }

    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        // When SYNC=1, read SINP into VAL BEFORE process() runs.
        // This matches C EPICS where dbGetLink is synchronous/immediate.
        if self.sync == 1 {
            self.sync = 0;
            return vec![ProcessAction::ReadDbLink {
                link_field: "SINP",
                target_field: "VAL",
            }];
        }
        Vec::new()
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let mut actions = Vec::new();

        // If we're being called after a delay to drain a pending value
        if self.delay_active {
            if self.delay_elapsed() {
                // Delay expired — send pending value if any
                self.delay_active = false;
                self.wait = 0;
                if let Some(pv) = self.pending_value.take() {
                    self.send_value(pv);
                    actions.push(ProcessAction::WriteDbLink {
                        link_field: "OUT",
                        value: EpicsValue::Double(self.sent),
                    });
                    // More values may have queued; start a new delay cycle
                    if self.dly > 0.0 {
                        self.delay_active = true;
                        self.wait = 1;
                        let delay = std::time::Duration::from_secs_f64(self.dly);
                        actions.push(ProcessAction::ReprocessAfter(delay));
                        return Ok(ProcessOutcome {
                            result: RecordProcessResult::Complete,
                            actions,
                            device_did_compute: false,
                        });
                    }
                }
                return Ok(ProcessOutcome::complete_with(actions));
            } else {
                // Still waiting — queue the current value, reschedule
                self.pending_value = Some(self.val);
                let remaining = self.dly
                    - self
                        .last_send_time
                        .map(|t| t.elapsed().as_secs_f64())
                        .unwrap_or(0.0);
                let delay = std::time::Duration::from_secs_f64(remaining.max(0.001));
                actions.push(ProcessAction::ReprocessAfter(delay));
                return Ok(ProcessOutcome {
                    result: RecordProcessResult::Complete,
                    actions,
                    device_did_compute: false,
                });
            }
        }

        // Normal processing: check limits
        match self.check_limits(self.val) {
            Ok(clamped) => {
                self.oval = self.val;
                self.val = clamped;
            }
            Err(()) => {
                self.val = self.oval;
                self.sts = 1; // Error
                return Ok(ProcessOutcome::complete_with(actions));
            }
        }

        // Check if we can send immediately
        if self.delay_elapsed() {
            // Send immediately
            self.send_value(self.val);
            actions.push(ProcessAction::WriteDbLink {
                link_field: "OUT",
                value: EpicsValue::Double(self.sent),
            });

            // Start delay period if DLY > 0
            if self.dly > 0.0 {
                self.delay_active = true;
                self.wait = 1;
                // ReprocessAfter: current cycle's OUT write proceeds (SENT is output),
                // then framework schedules re-process after DLY to drain pending values.
                let delay = std::time::Duration::from_secs_f64(self.dly);
                actions.push(ProcessAction::ReprocessAfter(delay));
                return Ok(ProcessOutcome {
                    result: RecordProcessResult::Complete,
                    actions,
                    device_did_compute: false,
                });
            }

            self.wait = 0;
            Ok(ProcessOutcome::complete_with(actions))
        } else {
            // Still in delay from previous send — queue value
            self.pending_value = Some(self.val);
            self.wait = 1;
            self.delay_active = true;
            let remaining = self.dly
                - self
                    .last_send_time
                    .map(|t| t.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
            let delay = std::time::Duration::from_secs_f64(remaining.max(0.001));
            actions.push(ProcessAction::ReprocessAfter(delay));
            Ok(ProcessOutcome {
                result: RecordProcessResult::Complete,
                actions,
                device_did_compute: false,
            })
        }
    }

    fn can_device_write(&self) -> bool {
        true
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        match field {
            "DLY" => {
                if self.dly < 0.0 {
                    self.dly = 0.0;
                }
            }
            "DRVLH" | "DRVLL" => {
                self.limit_flag = self.drvlh > self.drvll;
                if !self.limit_flag {
                    self.drvls = 0; // Normal
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "SENT" => Some(EpicsValue::Double(self.sent)),
            "OSENT" => Some(EpicsValue::Double(self.osent)),
            "WAIT" => Some(EpicsValue::Short(self.wait)),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "DRVLH" => Some(EpicsValue::Double(self.drvlh)),
            "DRVLL" => Some(EpicsValue::Double(self.drvll)),
            "DRVLS" => Some(EpicsValue::Short(self.drvls)),
            "DRVLC" => Some(EpicsValue::Short(self.drvlc)),
            "VER" => Some(EpicsValue::String(self.ver.clone())),
            "STS" => Some(EpicsValue::Short(self.sts)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "DPREC" => Some(EpicsValue::Short(self.dprec)),
            "DLY" => Some(EpicsValue::Double(self.dly)),
            "OUT" => Some(EpicsValue::String(self.out.clone())),
            "OV" => Some(EpicsValue::Short(self.ov)),
            "SINP" => Some(EpicsValue::String(self.sinp.clone())),
            "SIV" => Some(EpicsValue::Short(self.siv)),
            "SYNC" => Some(EpicsValue::Short(self.sync)),
            _ => None,
        }
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
            "HOPR" => match value {
                EpicsValue::Double(v) => {
                    self.hopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LOPR" => match value {
                EpicsValue::Double(v) => {
                    self.lopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVLH" => match value {
                EpicsValue::Double(v) => {
                    self.drvlh = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVLL" => match value {
                EpicsValue::Double(v) => {
                    self.drvll = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DRVLC" => match value {
                EpicsValue::Short(v) => {
                    self.drvlc = v;
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
            "DPREC" => match value {
                EpicsValue::Short(v) => {
                    self.dprec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DLY" => match value {
                EpicsValue::Double(v) => {
                    self.dly = v;
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
            "SINP" => match value {
                EpicsValue::String(v) => {
                    self.sinp = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "SYNC" => match value {
                EpicsValue::Short(v) => {
                    self.sync = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // Read-only fields
            "OVAL" | "SENT" | "OSENT" | "WAIT" | "DRVLS" | "VER" | "STS" | "OV" | "SIV" => {
                Err(CaError::ReadOnlyField(name.into()))
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 {
            self.limit_flag = self.drvlh > self.drvll;
        }
        Ok(())
    }
}
