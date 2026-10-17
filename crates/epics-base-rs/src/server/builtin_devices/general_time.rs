//! `General Time` device support — EPICS base `devGeneralTime.c`
//! (`devSoft.dbd`): four dsets on one DTYP, each a table of INST_IO channel
//! names matched case-insensitively at `init_record`.
//!
//! - `ai` (`devAiGeneralTime`): `TIME` — the current time as seconds past
//!   the EPICS epoch.
//! - `bo` (`devBoGeneralTime`): `RSTERRCNT` — reset the generalTime error
//!   count.
//! - `longin` (`devLiGeneralTime`): `GETERRCNT` — the error count.
//! - `stringin` (`devSiGeneralTime`): `BESTTCP` / `TOPTCP` / `BESTTEP` — the
//!   last current-time provider, the highest-priority one, and the last
//!   event provider.
//!
//! Each `init_*` also refuses an `INP`/`OUT` that is not INST_IO
//! (`devGeneralTime.c:59-64`). The DTYP's `device()` line declares INST_IO,
//! so dbStatic has already turned any other text into an empty INST_IO link
//! and that arm cannot fire; the empty name then misses the table.

use crate::error::{CaError, CaResult};
use crate::runtime::general_time;
use crate::server::builtin_devices::timestamp::epics_time_parts;
use crate::server::device_support::{
    DeviceAlarm, DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use crate::server::recgbl::{alarm_status, rec_gbl_record_error};
use crate::server::record::{AlarmSeverity, Record, ScanType};
use crate::types::EpicsValue;

/// One entry of the four C channel tables.
#[derive(Clone, Copy)]
enum Channel {
    /// `ai_channels`: `{"TIME", getCurrentTime}`.
    Time,
    /// `bo_channels`: `{"RSTERRCNT", resetErrors}`.
    ResetErrors,
    /// `li_channels`: `{"GETERRCNT", errorCount}`.
    ErrorCount,
    /// `si_channels`: `{"BESTTCP", timeProvider}`.
    BestCurrent,
    /// `si_channels`: `{"TOPTCP", highestProvider}`.
    TopCurrent,
    /// `si_channels`: `{"BESTTEP", eventProvider}`.
    BestEvent,
}

/// The record type's channel table and the prefix its `Bad parm` line names.
fn table(record_type: &str) -> Option<(&'static str, &'static [(&'static str, Channel)])> {
    Some(match record_type {
        "ai" => ("devAiGeneralTime::init_ai", &[("TIME", Channel::Time)]),
        "bo" => (
            "devBoGeneralTime::init_bo",
            &[("RSTERRCNT", Channel::ResetErrors)],
        ),
        "longin" => (
            "devLiGeneralTime::init_li",
            &[("GETERRCNT", Channel::ErrorCount)],
        ),
        "stringin" => (
            "devSiGeneralTime::init_si",
            &[
                ("BESTTCP", Channel::BestCurrent),
                ("TOPTCP", Channel::TopCurrent),
                ("BESTTEP", Channel::BestEvent),
            ],
        ),
        _ => return None,
    })
}

/// Whether `devSoft.dbd` declares a `General Time` dset for `record_type`.
pub fn serves(record_type: &str) -> bool {
    table(record_type).is_some()
}

/// `General Time` device support for `ai`, `bo`, `longin` and `stringin`.
pub struct GeneralTimeDeviceSupport {
    /// The INST_IO string of `INP` (`OUT` for bo), `@` stripped.
    parm: String,
    record_name: String,
    /// C `prec->dpvt`: the matched channel, `None` after `Bad parm`.
    channel: Option<Channel>,
    /// The stringin read found no provider (`MAJOR`/`READ`).
    no_provider: bool,
}

impl GeneralTimeDeviceSupport {
    /// Construct from the record's link text: `OUT` for bo, `INP` otherwise.
    pub fn new(link: &str) -> Self {
        Self {
            parm: link.strip_prefix('@').unwrap_or("").to_string(),
            record_name: String::new(),
            channel: None,
            no_provider: false,
        }
    }
}

impl DeviceSupport for GeneralTimeDeviceSupport {
    fn dtyp(&self) -> &str {
        "General Time"
    }

    fn set_record_info(&mut self, name: &str, _scan: ScanType) {
        self.record_name = name.to_string();
    }

    fn holds_dpvt(&self) -> bool {
        self.channel.is_some()
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        let rt = record.record_type();
        let Some((who, channels)) = table(rt) else {
            return Err(CaError::InvalidValue(format!(
                "DTYP='General Time': no dset for record type '{rt}'"
            )));
        };
        self.channel = channels
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&self.parm))
            .map(|&(_, channel)| channel);
        if self.channel.is_none() {
            // `recGblRecordError(S_db_badField, ...); prec->pact = TRUE`.
            rec_gbl_record_error(
                "Illegal field value",
                &self.record_name,
                &format!("{who}: Bad parm"),
            );
            return Ok(DeviceInitOutcome::dead());
        }
        // `init_bo` returns 2 (with `mask = 0`, already its default).
        Ok(if rt == "bo" {
            DeviceInitOutcome::DoNotConvert(DeviceUdf::Untouched)
        } else {
            DeviceInitOutcome::ok()
        })
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let Some(channel) = self.channel else {
            return Ok(DeviceReadOutcome::failed());
        };
        match channel {
            Channel::Time => {
                // `getCurrentTime` cannot fail here: the OS clock provider is
                // always registered, so `read_ai`'s READ/INVALID arm is dead.
                let (sec, nsec) = epics_time_parts(general_time::get_current());
                let val = sec as f64 + nsec as f64 * 1e-9;
                record.put_field("VAL", EpicsValue::Double(val))?;
                Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
            }
            Channel::ErrorCount => {
                // C `int` from `generalTimeGetErrorCounts`.
                let count = general_time::error_counts() as i32;
                record.put_field("VAL", EpicsValue::Long(count))?;
                Ok(DeviceReadOutcome::ok())
            }
            Channel::BestCurrent | Channel::TopCurrent | Channel::BestEvent => {
                let name = match channel {
                    Channel::BestCurrent => general_time::current_provider_name(),
                    Channel::TopCurrent => general_time::highest_current_name(),
                    _ => general_time::event_provider_name(),
                };
                self.no_provider = name.is_none();
                let mut val = name.unwrap_or_else(|| "No working providers".to_string());
                // `strncpy` into `char val[40]` with the last byte NUL.
                if val.len() > 39 {
                    let mut end = 39;
                    while !val.is_char_boundary(end) {
                        end -= 1;
                    }
                    val.truncate(end);
                }
                record.put_field("VAL", EpicsValue::String(val.into()))?;
                Ok(DeviceReadOutcome::converted(DeviceUdf::Defined))
            }
            Channel::ResetErrors => Ok(DeviceReadOutcome::failed()),
        }
    }

    fn last_alarm(&self) -> Option<DeviceAlarm<'_>> {
        self.no_provider.then_some(DeviceAlarm {
            stat: alarm_status::READ_ALARM,
            sevr: AlarmSeverity::Major as u16,
            msg: "",
        })
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        if let Some(Channel::ResetErrors) = self.channel {
            general_time::reset_error_counts();
        }
        Ok(())
    }
}
