mod alarm;
mod common_fields;
mod link;
mod record_instance;
mod record_trait;
mod scan;

// Re-export all public types so existing imports continue to work.
pub use crate::server::recgbl::EventMask;
pub use alarm::{AlarmSeverity, AnalogAlarmConfig};
pub use common_fields::CommonFields;
pub use link::{DbLink, LinkAddress, LinkProcessPolicy, ParsedLink, parse_link, parse_link_v2};
pub use record_instance::RecordInstance;
pub use record_trait::{
    CommonFieldPutResult, FieldDesc, ProcessAction, ProcessOutcome, ProcessSnapshot, Record,
    RecordProcessResult, SubroutineFn,
};
pub use scan::ScanType;
