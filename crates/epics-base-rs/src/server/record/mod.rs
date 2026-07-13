mod alarm;
mod common_fields;
mod link;
mod menu_choices;
mod pini;
mod process_passive;
mod record_instance;
mod record_trait;
mod scan;

// Re-export all public types so existing imports continue to work.
pub use crate::server::recgbl::EventMask;
pub use alarm::{AlarmSeverity, AnalogAlarmConfig};
pub use common_fields::CommonFields;
pub use link::{
    CaLink, CalcLink, DbLink, HwLink, HwLinkKind, JlinkValue, LinkAddress, LinkFieldType,
    LinkProcessPolicy, LinkType, LsLoad, MonitorSwitch, PVAJSON_IDENTITY_SEP, ParsedLink,
    PvaJsonLink, link_field_type, load_link_ls, out_link_discards_cp, parse_c_double,
    parse_forward_link_v2, parse_link, parse_link_field, parse_link_v2, parse_output_link_v2,
    pvajson_identity_key,
};
pub use menu_choices::{
    MENU_ALARM_SEVR, MENU_ALARM_STAT, MENU_CONVERT, MENU_FTYPE, MENU_IVOA, MENU_OMSL, MENU_PINI,
    MENU_POST, MENU_PRIORITY, MENU_SCAN, MENU_SIMM, MENU_YES_NO, binary_enum_states,
    multibit_enum_states, resolve_enum_state_string, resolve_menu_field_string,
    resolve_menu_field_string_db_load, shared_menu_choices,
};
pub use pini::PiniMode;
pub(crate) use record_instance::value_as_dbr_string;
pub use record_instance::{AlarmAck, DeferredNotifyPut, NotifyWaitSet, RecordInstance};
pub use record_trait::{
    ArrayMonitorPost, CommonFieldPutResult, ConstantInitLink, CyclePostMask,
    EPICS_TIME_EVENT_DEVICE_TIME, FieldDesc, FieldMetadataOverride, InputFetchPolicy, LinkReadAs,
    OutTarget, ProcessAction, ProcessContext, ProcessOutcome, ProcessSnapshot, Record,
    RecordProcessResult, SubroutineFn, ValuePostGate, coerce_put_value, put_field_internal_default,
    seed_input_links,
};
pub(crate) use record_trait::{AuxPostMask, value_gate};
pub use scan::{ScanType, SimModeScan};
