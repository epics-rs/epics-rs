mod alarm;
pub mod bpt_generated;
mod common_fields;
mod constant_link;
mod contributed_dbd;
pub mod dbd_generated;
mod declared_field_registry;
mod device_menu_registry;
mod dsets;
mod link;
mod menu_choices;
pub mod menu_scan;
mod pini;
mod prefetch;
pub(crate) use prefetch::{Hint, prefetch_span, prefetch_val};
mod process_passive;
pub(crate) mod record_instance;
mod record_trait;
mod scan;
mod sparse_text;

// Re-export all public types so existing imports continue to work.
pub use crate::server::recgbl::EventMask;
pub use alarm::{AlarmLimit, AlarmLimitField, AlarmSeverity, AnalogAlarmConfig};
pub(crate) use common_fields::StampBegun;
pub use common_fields::{BkptFlag, CommonFields, CommonText, StampMode, TimeLast};
pub(crate) use constant_link::{constant_load_as, constant_load_scalar};
pub use constant_link::{rec_gbl_init_constant_link, reseed_constant_input_link};
pub use contributed_dbd::{ContributedDbd, register_contributed_dbd};
pub(crate) use contributed_dbd::{
    menus, record_declaration_order, record_fields, record_no_prompt, record_types,
};
pub use declared_field_registry::{
    declared_field, declared_field_table, declared_fields, register_declared_fields,
};
pub use device_menu_registry::{
    DevSup, Dset, register_dbd_devices, register_device_menu, register_device_object_row,
    register_device_row,
};
pub(crate) use device_menu_registry::{
    contributed_device_rows, dev_sup_of, device_menu_choices, has_contributed_device_menu,
    has_device_object, has_no_device_support, merged_device_menu,
};
pub use link::{
    CaLink, CalcArg, CalcLink, CalcVal, ConstLoadError, ConstantLink, DbLink, DbLinkType,
    ExternalForward, ExternalRef, ForwardSlot, HwLink, HwLinkKind, JlinkValue, JsonLinkParse,
    LinkFieldType, LinkProcessPolicy, LinkType, LsLoad, MonitorSwitch, OpenLatch,
    PVAJSON_IDENTITY_SEP, ParsedLink, PvaChannelLink, PvaJsonLink, StateLink, StatePut, VxiAddr,
    ca_forward_flag, check_json_link_text, check_link_assignment, check_link_text,
    declared_link_type, link_field_type, link_type_refusal, load_link_ls, out_link_discards_cp,
    parse_c_double, parse_forward_link_v2, parse_link_field, parse_link_v2, parse_output_link_v2,
    pvajson_identity_key, pvajson_identity_parts,
};
pub(crate) use link::{
    DbLinkAnswer, LinkList, LinkLookup, check_json_link_text_in, implemented_jlif,
    json_const_value, trim_link_text, warn_discarded_cp,
};
pub(crate) use menu_choices::{EnumChoice, enum_state_choice};
pub use menu_choices::{
    Ftype, MENU_ALARM_SEVR, MENU_ALARM_STAT, MENU_CONVERT, MENU_FTYPE, MENU_IVOA, MENU_OMSL,
    MENU_PINI, MENU_POST, MENU_PRIORITY, MENU_SCAN, MENU_SIMM, MENU_YES_NO, binary_enum_states,
    binary_enum_str_state, binary_enum_string_form, multibit_enum_states, multibit_enum_str_state,
    multibit_enum_string_form, multibit_state_string_index, resolve_enum_state_string,
    resolve_menu_field_string, resolve_menu_field_string_db_load, resolve_put_string_enum,
    shared_menu_choices,
};
pub use menu_scan::{MenuScan, SCAN_1ST_PERIODIC, menu_scan};
pub use pini::PiniMode;
pub use record_instance::{
    AlarmAck, AmbientWriteOriginScope, DeferredNotify, DeferredNotifyPut, FieldIx, NotifyWaitSet,
    PactExit, ProcessCompletion, RecordCell, RecordInstance, RecordMut, RecordRef,
    ambient_write_origin_scope,
};
pub(crate) use record_instance::{
    CycleLink, PreparedCommonLink, PreparedLink, PreparedLinks, SimLink, ambient_write_origin,
    input_dset_skips_convert, value_as_dbr_string,
};
pub use record_trait::{
    AfterGate, AnalogAlarmInput, ArrayMonitorPost, Asl, Base, CommonFieldPutResult,
    ConstantInitLink, ContextSubroutineFn, CycleCell, CycleHooks, CycleList, CycleMarks,
    CyclePostMask, CycleRead, CycleValue, DeadbandCells, DeclaredFields, DelayedCallbackOutcome,
    DolStatus, EPICS_TIME_EVENT_DEVICE_TIME, EnumSlots, FORCE_GENERAL, FieldDeclaration, FieldDesc,
    FieldMetadataOverride, FieldSlot, InitLinkReads, InputFetchPolicy, InputLinkRequest, LinkRead,
    LinkReadAs, MonitorDeadbandCells, MonitorValueClasses, MonitorValueGate, OutTarget, OutView,
    Pact, PostName, PostStage, PostTable, PostedValue, ProcessAction, ProcessActions,
    ProcessContext, ProcessOutcome, ProcessSnapshot, ProcessStatusRule, RawSoftEntry, Record,
    RecordProcessResult, ResolvedInputLinks, SecondaryTake, SharedAfterWork, SimmCells, SlotMut,
    SlotRef, SoftStatus, Special, Subroutine, SubroutineFn, ValuePostGate, ValueView,
    arg_letter_offset, arg_link_field, calc_class_link_backed_metadata_field, coerce_put_value,
    dbput_coerce_value, declared_field_ix, declared_post_table, declared_set, field_at,
    input_link_slots_of, link_value_in_field_shape, monitor_deadband_classes,
    put_field_internal_default, put_value_in_field_shape, seed_input_links, uniform_post_table,
};
pub(crate) use record_trait::{InitGetFailure, InitRead};
pub(crate) use record_trait::{
    SoftRead, SoftReading, StoredShape, dbput_coerce_value_in, put_string_precision,
    seed_trackers_from_val, served_element_size,
};
pub use scan::{ScanList, ScanType, SimModeScan};
pub use sparse_text::{AlarmText, SparseText};
