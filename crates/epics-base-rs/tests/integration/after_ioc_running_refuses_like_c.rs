//! C `afterIocRunningFunc` (`afterIocRunning.c:71-87`) refuses a line once
//! the hook has run (`initEndFlag`) and an empty one before that, each with a
//! plain `printf` that no redirect takes, and no failed status. The hook's
//! loop (`:46-55`) takes lines until the list is empty, so a line queued by a
//! queued line runs in the same pass.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

#[test]
fn refusals_and_a_line_queued_while_the_queue_runs() {
    let dir = tempfile::tempdir().unwrap();
    let out = |name: &str| dir.path().join(name);
    let st_cmd = out("st.cmd");
    std::fs::write(
        &st_cmd,
        format!(
            "on error break\n\
             afterIocRunning \"\" > {}\n\
             afterIocRunning > {}\n\
             afterIocRunning \"afterIocRunning 'epicsEnvSet AIR_NESTED 1'\"\n\
             iocInit > {}\n\
             afterIocRunning \"epicsEnvSet AIR_LATE 1\" > {}\n\
             epicsEnvSet AIR_SCRIPT_ENDED 1\n",
            out("empty.out").display(),
            out("missing.out").display(),
            out("init.out").display(),
            out("late.out").display(),
        ),
    )
    .unwrap();

    // RTEMS-EXEC-MODEL-ALLOW(1): the runtime only captures the shell's bridge; passes in the exec-backend suite.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let path = st_cmd.to_str().unwrap().to_string();
    let console = fd1_during(|| {
        std::thread::spawn(move || {
            IocShell::new(Arc::new(PvDatabase::new()), bridge).execute_script(path)
        })
        .join()
        .unwrap()
        .unwrap();
    });

    let read = |name: &str| std::fs::read_to_string(out(name)).unwrap();
    assert_eq!(read("empty.out"), "");
    assert_eq!(read("missing.out"), "");
    assert_eq!(read("late.out"), "");
    let refusals: Vec<&str> = console
        .lines()
        .filter(|l| l.contains("afterIocRunning: "))
        .collect();
    let empty = "\x1b[31;1mERROR\x1b[0m afterIocRunning: received an empty 'command' argument";
    assert_eq!(
        refusals,
        [
            empty,
            empty,
            "\x1b[35;1mWARNING\x1b[0m afterIocRunning: can only be used before 'iocInit'",
        ],
        "{console:?}"
    );
    assert_eq!(
        std::env::var("AIR_SCRIPT_ENDED").as_deref(),
        Ok("1"),
        "no refusal fails its line"
    );
    assert!(
        console.contains("\x1b[32;1mafterIocRunning:\x1b[0m epicsEnvSet AIR_NESTED 1\n"),
        "{console:?}"
    );
    assert_eq!(std::env::var("AIR_NESTED").as_deref(), Ok("1"));
    assert!(std::env::var("AIR_LATE").is_err());
    assert_eq!(epics_base_rs::server::ioc_app::ioc_shutdown(), 0);
}

/// What `f` wrote to fd 1.
fn fd1_during(f: impl FnOnce()) -> String {
    let sink = tempfile::NamedTempFile::new().unwrap();
    // SAFETY: the shell writes through fd 1 for the length of one script and
    // this test owns the process, so nothing else holds it.
    let saved = unsafe {
        let saved = libc::dup(1);
        libc::dup2(sink.as_file().as_raw_fd(), 1);
        saved
    };
    f();
    let _ = std::io::stdout().flush();
    // SAFETY: restoring the descriptor this call replaced.
    unsafe {
        libc::dup2(saved, 1);
        libc::close(saved);
    }
    std::fs::read_to_string(sink.path()).unwrap()
}
