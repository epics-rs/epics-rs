//! C's `iocsh(NULL)` reads stdin each time it is called; nothing in `iocsh()`
//! (`iocsh.cpp:1335-1338`) keeps a once-per-process flag, and a C `main`
//! calling it twice reads at both. So a second IOC in one process gets its
//! own prompt, and so does a `main`'s own prompt after both. The first IOC
//! skips `iocInit` (its tail prompt), because a second build's `asInit` is
//! refused as C's `asInitCommon` refuses it (`asDbLib.c:129-134`).

#![cfg(unix)]

use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_app::{IocApplication, IocInitDecision, RunOutcome};
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandDef, CommandOutcome,
};

fn probe(into: Arc<Mutex<Vec<String>>>) -> CommandDef {
    CommandDef::new(
        "probePrompt",
        vec![ArgDesc {
            name: "tag",
            arg_type: ArgType::String,
        }],
        "probePrompt tag - record which prompt read this line",
        move |args: &[ArgValue], _ctx: &_| {
            let ArgValue::String(tag) = &args[0] else {
                panic!("one string argument");
            };
            into.lock()
                .unwrap()
                .push(String::from_utf8_lossy(tag.as_ref()).into_owned());
            Ok(CommandOutcome::Continue)
        },
    )
}

#[epics_macros_rs::epics_test]
async fn a_second_ioc_and_a_later_main_each_read_their_lines() {
    // Every prompt's lines, written up front: each prompt reads to its
    // `exit` and leaves the rest in stdin's buffer for the next one.
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let lines = b"probePrompt first\nexit\nprobePrompt second\nexit\nprobePrompt main\nexit\n";
    assert_eq!(
        unsafe { libc::write(fds[1], lines.as_ptr().cast(), lines.len()) },
        lines.len() as isize
    );
    assert_eq!(unsafe { libc::close(fds[1]) }, 0);
    assert_eq!(unsafe { libc::dup2(fds[0], 0) }, 0);

    let seen = Arc::new(Mutex::new(Vec::new()));
    IocApplication::new()
        .port(0)
        .register_startup_command(probe(seen.clone()))
        .before_ioc_init(|| IocInitDecision::skip(true))
        .run(|_config| async move { unreachable!("this IOC never runs iocInit") })
        .await
        .unwrap();
    let second = IocApplication::new()
        .port(0)
        .run_outcome(|config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            config.run_shell(Vec::new()).await
        })
        .await
        .unwrap();
    // An IOC that never initialised reads `second` at its tail prompt too.
    assert_eq!(second, RunOutcome::Served);
    // A `main`'s own `iocsh(NULL)` after both IOCs, on `_main_` as the
    // servers' `run_with_shell` puts it.
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    epics_base_rs::runtime::task::run_on_main_thread(move || {
        epics_base_rs::server::iocsh::IocShell::new(
            Arc::new(epics_base_rs::server::database::PvDatabase::new()),
            bridge,
        )
        .run_main_prompt()
    })
    .unwrap()
    .await
    .unwrap()
    .unwrap();

    assert_eq!(*seen.lock().unwrap(), ["first", "second", "main"]);
}
