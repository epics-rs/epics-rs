mod alarm;
mod common_fields;
mod link;
mod process_passive;
mod record_instance;
mod record_trait;
mod scan;

// Re-export all public types so existing imports continue to work.
pub use crate::server::recgbl::EventMask;
pub use alarm::{AlarmSeverity, AnalogAlarmConfig};
pub use common_fields::CommonFields;
pub use link::{
    CaLink, CalcLink, DbLink, HwLink, HwLinkKind, LinkAddress, LinkProcessPolicy, LinkType,
    MonitorSwitch, ParsedLink, PvaJsonLink, link_field_type, parse_link, parse_link_v2,
    parse_output_link_v2,
};
pub use record_instance::{NotifyWaitSet, RecordInstance};
pub use record_trait::{
    CommonFieldPutResult, EPICS_TIME_EVENT_DEVICE_TIME, FieldDesc, ProcessAction, ProcessContext,
    ProcessOutcome, ProcessSnapshot, Record, RecordProcessResult, SubroutineFn,
};
pub use scan::ScanType;
