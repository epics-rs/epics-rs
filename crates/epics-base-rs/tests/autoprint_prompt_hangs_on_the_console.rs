//! dbBkpt.c's `dbPrint` runs on the lock set's `bkptCont` thread, which
//! takes every record of a lock set holding a breakpoint (`dbBkpt.c:701`),
//! so its blank line, `dbpr` dump and `-> ` all land on the console, and the
//! `-> ` hangs: the next output follows it on the same line. C oracle
//! (`oracleStdIoc`, the same script): `f` holds only `dbpf`'s line.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOPRINT_PROMPT_HANGS_ON_THE_CONSOLE_SCRIPT";

#[epics_macros_rs::epics_test]
async fn child_script() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    // The stdin shell after the script, as C's softIoc runs one.
    IocApplication::new()
        .port(0)
        .startup_script(script.to_str().unwrap())
        .run(|config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            config.run_shell(Vec::new()).await
        })
        .await
        .unwrap();
}

/// The stdin shell's prompt, which races the print; nothing else the
/// shell prints is concurrent with it.
const PROMPT: &str = "epics> ";

#[test]
fn the_autoprint_prompt_hangs_on_the_console() {
    let dir = tempfile::tempdir().unwrap();
    // iocsh reads an unquoted `\` as an escape, as C `split()` does.
    let d = dir.path().display().to_string().replace('\\', "/");
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
             dbpf B.PROC 1 > {d}/f.txt\n"
        ),
    )
    .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_script", "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{d}/st.cmd"))
        .env("IOCSH_PS1", PROMPT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0; 4096];
        while let Ok(n @ 1..) = stdout.read(&mut buf) {
            let _ = tx.send(buf[..n].to_vec());
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut console = Vec::new();
    let mut read_until = |done: &dyn Fn(&str) -> bool| loop {
        let text = String::from_utf8_lossy(&console).replace(PROMPT, "");
        if done(&text) {
            return text;
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(chunk) => console.extend(chunk),
            Err(e) => panic!("{e}: {text}"),
        }
    };
    // `echo MARK` goes in once the print's `-> ` is out, so its output is
    // the next thing printed.
    read_until(&|text| {
        text.find("NAME: B ")
            .is_some_and(|at| text[at..].contains("\n-> "))
    });
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"echo MARK\n").unwrap();
    drop(stdin);
    let console = read_until(&|text| text.contains("MARK\n"));
    assert!(child.wait().unwrap().success(), "{console}");
    assert_eq!(
        std::fs::read_to_string(format!("{d}/f.txt")).unwrap(),
        "DBF_UCHAR:          1 = 0x1   \n"
    );
    let at = console
        .find("\n\nACKS: ")
        .unwrap_or_else(|| panic!("{console}"));
    let print = &console[at..];
    assert!(print.contains("NAME: B "), "{print}");
    assert!(print.contains("\n-> MARK\n"), "{print:?}");
}
