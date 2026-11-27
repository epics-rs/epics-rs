//! An `initHookRegister` observer at `initHookAtBeginning` must read the
//! PRE-build IOC state, not `iocBuilding`.
//!
//! C `iocBuild_1` (`iocInit.c:145-148`) announces `initHookAtIocBuild`
//! and `initHookAtBeginning` while `iocState` is still `iocVoid`, calls
//! `coreRelease()`, and only then assigns `iocState = iocBuilding`. An
//! observer registered at either of those two states therefore sees the
//! state the IOC is coming FROM. The port had the assignment first, so
//! the same observer saw `Building` — a difference no stdout A/B can
//! show, because only a hook callback can ask.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_app::init_hooks::{InitHookState, init_hook_register};
use epics_base_rs::server::ioc_app::{IocApplication, IocState, get_ioc_state};

/// Every announced state paired with the lifecycle state visible to a
/// callback at that moment.
type Seen = Arc<Mutex<Vec<(InitHookState, IocState)>>>;

fn state_at(seen: &Seen, state: InitHookState) -> IocState {
    let log = seen.lock().unwrap();
    log.iter()
        .find(|(s, _)| *s == state)
        .unwrap_or_else(|| panic!("{} was never announced", state.name()))
        .1
}

fn position_of(seen: &Seen, state: InitHookState) -> usize {
    let log = seen.lock().unwrap();
    log.iter()
        .position(|(s, _)| *s == state)
        .unwrap_or_else(|| panic!("{} was never announced", state.name()))
}

#[epics_macros_rs::epics_test]
async fn an_at_beginning_observer_reads_the_state_the_ioc_came_from() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    init_hook_register(Arc::new(move |state| {
        recorder.lock().unwrap().push((state, get_ioc_state()));
    }));

    IocApplication::new()
        .port(0)
        .run(|_config| async move { Ok(()) })
        .await
        .unwrap();

    // C `iocInit.c:122` and `:145` — both announced from `iocVoid`.
    assert_eq!(
        state_at(&seen, InitHookState::AtIocBuild),
        IocState::Void,
        "AtIocBuild is announced before the build state is assigned"
    );
    assert_eq!(
        state_at(&seen, InitHookState::AtBeginning),
        IocState::Void,
        "AtBeginning is announced before the build state is assigned"
    );
    // C `iocInit.c:148` — the assignment lands between AtBeginning and
    // the next announce, so the very next observer sees `Building`.
    assert_eq!(
        state_at(&seen, InitHookState::AfterCallbackInit),
        IocState::Building,
        "the build state must be assigned by the time callbackInit is announced"
    );
    assert!(
        position_of(&seen, InitHookState::AtIocBuild)
            < position_of(&seen, InitHookState::AtBeginning),
        "AtIocBuild precedes AtBeginning"
    );
    assert!(
        position_of(&seen, InitHookState::AtBeginning)
            < position_of(&seen, InitHookState::AfterCallbackInit),
        "AtBeginning precedes AfterCallbackInit"
    );
}
