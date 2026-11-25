//! C `iocInit` ends by announcing `initHookAfterIocRunning`, whose
//! `afterIocRunningHook` runs the queued lines then and there
//! (`iocInit.c:273-277`, `afterIocRunning.c:38-56`), so they come before the
//! script's next line. And C softMain's `iocsh(NULL)` is a later statement on
//! the same `_main_` thread (`softMain.cpp:231-250`), so while the script
//! runs, `epicsThreadShowAll` lists one `_main_`, at its priority 0 and never
//! rescheduled (`osdThread.c:406-412`) — even with a protocol runner whose
//! prompt was asked for at that `iocInit`.

#![cfg(unix)]

use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_app::IocApplication;
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

#[epics_macros_rs::epics_test]
async fn queued_lines_run_inside_ioc_init_on_the_one_main_thread() {
    // The runner's prompt reads this process's stdin; give it an EOF.
    let null = std::fs::File::open("/dev/null").unwrap();
    assert_eq!(
        unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&null), 0) },
        0
    );

    let seen = Arc::new(Mutex::new(Vec::new()));
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    let threads = dir.join("after_ioc_running_threads.txt");
    let script = dir.join("after_ioc_running_order.cmd");
    std::fs::write(
        &script,
        format!(
            "afterIocRunning \"probeOrder queued\"\n\
             iocInit\n\
             probeOrder next\n\
             epicsThreadSleep 0.3\n\
             epicsThreadShowAll > {}\n",
            threads.display()
        ),
    )
    .unwrap();
    IocApplication::new()
        .port(0)
        .register_startup_command(probe(seen.clone()))
        .startup_script(script.to_str().unwrap())
        .run(|config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            config.run_shell(Vec::new()).await
        })
        .await
        .unwrap();

    assert_eq!(*seen.lock().unwrap(), ["queued", "next"]);

    // The sleep gives a runner that wrongly started a thread for its prompt
    // the time to list it.
    let listing = std::fs::read_to_string(&threads).unwrap();
    let main_rows: Vec<Vec<&str>> = listing
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|t| t.first() == Some(&"_main_"))
        .collect();
    assert_eq!(main_rows.len(), 1, "{listing}");
    // NAME, EPICS ID, LWP ID, OSIPRI: C's `_main_` is EPICS priority 0.
    assert_eq!(main_rows[0][3], "0", "{listing}");
    // OSSPRI: and C never reschedules it, so it is at the process's own
    // policy, as this test thread is.
    let mut policy = 0;
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::pthread_getschedparam(libc::pthread_self(), &mut policy, &mut param) },
        0
    );
    assert_eq!(
        main_rows[0][4],
        param.sched_priority.to_string(),
        "{listing}"
    );
}
