//! C's `afterIocRunningHook` is an init hook (`afterIocRunning.c:38-56`), so
//! every `iocRun` runs the queue — a `CaServerBuilder` binary's bare
//! `PvDatabase` shell as much as `IocApplication`'s.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandDef, CommandOutcome,
};

fn probe(into: Arc<Mutex<Vec<String>>>) -> CommandDef {
    CommandDef::new(
        "probeOrder",
        vec![ArgDesc {
            name: "tag",
            arg_type: ArgType::String,
        }],
        "probeOrder tag - record the order lines run in for this test",
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

#[test]
fn a_bare_shell_ioc_init_runs_the_queue_before_the_next_line() {
    let dir = tempfile::tempdir().unwrap();
    let st_cmd = dir.path().join("st.cmd");
    std::fs::write(
        &st_cmd,
        "afterIocRunning \"probeOrder queued\"\niocInit\nprobeOrder next\n",
    )
    .unwrap();

    // RTEMS-EXEC-MODEL-ALLOW(1): the runtime only captures the shell's bridge; passes in the exec-backend suite.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let probe = probe(seen.clone());
    let path = st_cmd.to_str().unwrap().to_string();
    std::thread::spawn(move || {
        let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
        shell.register(probe);
        shell.execute_script(path)
    })
    .join()
    .unwrap()
    .unwrap();

    assert_eq!(*seen.lock().unwrap(), ["queued", "next"]);
    assert_eq!(epics_base_rs::server::ioc_app::ioc_shutdown(), 0);
}
