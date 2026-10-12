//! `EPICS_CAS_MAX_EVENTS_PER_CHAN` is read and clamped at CA server start
//! (`configure_events_per_que`, C `rsrv_init` `caservertask.c:510-530`),
//! never by an attach: `db_add_event` runs under the record's lock, where an
//! env read, a `String` or an errlog line has no place.

use epics_base_rs::server::event_queue::{
    EventUser, LogKind, attach, configure_events_per_que, events_per_que,
};

#[test]
fn only_server_start_applies_the_variable() {
    // This binary is its own process with one test, so nothing races the env.
    unsafe { std::env::set_var("EPICS_CAS_MAX_EVENTS_PER_CHAN", "70000") };
    let _held = attach(&EventUser::new(), 1, LogKind::Val);
    assert_eq!(events_per_que(), 36, "an attach must not read the variable");
    configure_events_per_que();
    assert_eq!(events_per_que(), 65535);
}
