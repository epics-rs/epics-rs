//! dbBkpt.c's `dbPrint` runs on the lock set's `bkptCont` thread, which
//! takes every record of a lock set holding a breakpoint (`dbBkpt.c:701`),
//! so its blank line, `dbpr` dump and `-> ` all land on the console, and the
//! `-> ` hangs: the next output follows it on the same line. C oracle
//! (`oracleStdIoc`, the same script): `f` holds only `dbpf`'s line.

use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOPRINT_PROMPT_HANGS_ON_THE_CONSOLE_SCRIPT";

#[epics_macros_rs::epics_test]
async fn child_script() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .startup_script(script.to_str().unwrap())
        .run(|_| async { Ok(()) })
        .await
        .unwrap();
}

#[test]
fn the_autoprint_prompt_hangs_on_the_console() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display().to_string();
    std::fs::write(
        format!("{d}/a.db"),
        "record(ai, \"A\") {}\nrecord(ai, \"B\") { field(INP, \"A NPP\") }\n",
    )
    .unwrap();
    std::fs::write(
        format!("{d}/st.cmd"),
        format!(
            "dbLoadRecords(\"{d}/a.db\")\n\
             iocInit\n\
             dbb A\n\
             dbap B\n\
             dbpf B.PROC 1 > {d}/f.txt\n\
             epicsThreadSleep 1\n\
             echo MARK\n"
        ),
    )
    .unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_script", "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{d}/st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        std::fs::read_to_string(format!("{d}/f.txt")).unwrap(),
        "DBF_UCHAR:          1 = 0x1   \n"
    );
    let console = String::from_utf8_lossy(&out.stdout);
    // The script echoes each line it runs, so the next line's echo is what
    // follows the `-> `, as C's `epics> ` prompt does.
    let at = console
        .find("\n\nACKS: ")
        .unwrap_or_else(|| panic!("{console}"));
    let print = &console[at..];
    assert!(print.contains("NAME: B "), "{print}");
    // The print races the shell, so the echo after it is either the sleep's
    // or the next line's; either one starts in bold.
    let prompt = print.find("\n-> ").unwrap_or_else(|| panic!("{print:?}"));
    let rest = print[prompt + 4..].lines().next().unwrap();
    assert!(rest.starts_with("\x1b[1m"), "{print:?}");
}
