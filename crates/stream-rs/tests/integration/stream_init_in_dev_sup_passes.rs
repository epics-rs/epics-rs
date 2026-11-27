//! `streamInit` is the stream dsets' `init`, so iocInit runs `streamInit(0)`
//! in `initDevSup` and `streamInit(1)` in `finishDevSup` (iocInit.c:173,
//! :183): an `initHookAfterInitRecSup` hook still reads the shell's
//! `streamError`, an `initHookAfterInitDevSup` one reads the 1 the protocol
//! files are read with, and an `initHookAfterFinishDevSup` one the shell's
//! again.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_app::init_hooks::{InitHookState, init_hook_register};
use epics_base_rs::server::ioc_builder::IocBuilder;
use stream_rs::error::STREAM_ERROR;

#[test]
fn stream_init_runs_in_init_dev_sup_and_finish_dev_sup() {
    epics_base_rs::runtime::task::test_block_on(async {
        stream_rs::error::set_stream_error(0);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let builder = stream_rs::dev::register(IocBuilder::new());
        init_hook_register(Arc::new(move |state| {
            if matches!(
                state,
                InitHookState::AfterInitRecSup
                    | InitHookState::AfterInitDevSup
                    | InitHookState::AfterFinishDevSup
            ) {
                log.lock()
                    .unwrap()
                    .push((state.name(), STREAM_ERROR.load(Ordering::Relaxed)));
            }
        }));
        let _ioc = builder.build_ioc().await.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                ("initHookAfterInitRecSup", 0),
                ("initHookAfterInitDevSup", 1),
                ("initHookAfterFinishDevSup", 0),
            ]
        );
    });
}
