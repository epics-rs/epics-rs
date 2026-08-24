mod alarm;
pub mod bpt_generated;
mod common_fields;
mod constant_link;
pub mod dbd_generated;
mod device_menu_registry;
mod link;
mod menu_choices;
mod pini;
mod process_passive;
pub(crate) mod record_instance;
mod record_trait;
mod scan;

// Re-export all public types so existing imports continue to work.
pub use crate::server::recgbl::EventMask;
pub use alarm::{AlarmLimit, AlarmSeverity, AnalogAlarmConfig};
pub use common_fields::CommonFields;
pub use constant_link::{rec_gbl_init_constant_link, reseed_constant_input_link};
pub use device_menu_registry::register_device_menu;
pub(crate) use device_menu_registry::{contributed_device_menu, merged_device_menu};
pub use link::{
    CaLink, CalcArg, CalcLink, DbLink, DbLinkType, HwLink, HwLinkKind, JlinkValue, JsonLinkParse,
    LinkAddress, LinkFieldType, LinkProcessPolicy, LinkType, LsLoad, MonitorSwitch,
    PVAJSON_IDENTITY_SEP, ParsedLink, PvaJsonLink, check_json_link_text, check_link_assignment,
    declared_link_type, link_field_type, load_link_ls, out_link_discards_cp, parse_c_double,
    parse_forward_link_v2, parse_link, parse_link_field, parse_link_v2, parse_output_link_v2,
    pvajson_identity_key,
};
pub use menu_choices::{
    Ftype, MENU_ALARM_SEVR, MENU_ALARM_STAT, MENU_CONVERT, MENU_FTYPE, MENU_IVOA, MENU_OMSL,
    MENU_PINI, MENU_POST, MENU_PRIORITY, MENU_SCAN, MENU_SIMM, MENU_YES_NO, binary_enum_states,
    binary_enum_string_form, multibit_enum_states, multibit_enum_string_form,
    multibit_state_string_index, resolve_enum_state_string, resolve_menu_field_string,
    resolve_menu_field_string_db_load, shared_menu_choices,
};
pub use pini::PiniMode;
pub use record_instance::{
    AlarmAck, AmbientWriteOriginScope, DeferredNotify, DeferredNotifyPut, NotifyWaitSet, PactExit,
    ProcessCompletion, RecordInstance, ambient_write_origin_scope,
};
pub(crate) use record_instance::{ambient_write_origin, value_as_dbr_string};
pub use record_trait::{
    ArrayMonitorPost, Asl, CommonFieldPutResult, ConstantInitLink, CyclePostMask,
    DelayedCallbackOutcome, EPICS_TIME_EVENT_DEVICE_TIME, FieldDeclaration, FieldDesc,
    FieldMetadataOverride, InputFetchPolicy, LinkReadAs, OutTarget, ProcessAction, ProcessContext,
    ProcessOutcome, ProcessSnapshot, RawSoftEntry, Record, RecordProcessResult, Special,
    SubroutineFn, ValuePostGate, arg_letter_offset, arg_link_field,
    calc_class_link_backed_metadata_field, coerce_put_value, put_field_internal_default,
    seed_input_links,
};
pub(crate) use record_trait::{AuxPostMask, value_gate};
pub use scan::{ScanList, ScanType, SimModeScan};
