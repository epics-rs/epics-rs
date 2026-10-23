//! A command that reads stdin in a shell fed from a pipe gets the script's
//! next line, which the shell then does not run: C's `epicsReadline` and
//! the command read one `FILE` (`seq_qry.c` `userInput`'s `fgets(stdin)`
//! under `softIoc < st.cmd`). It must not wait on the shell's own read.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandDef, CommandOutcome};

const CHILD: &str = "A_PIPED_SHELL_COMMAND_READS_THE_NEXT_SCRIPT_LINE";

/// The prompt the shell prints before every read, taken out of its output.
const PROMPT: &str = "epics> ";

#[test]
fn child_shell() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    // RTEMS-EXEC-MODEL-ALLOW(1): the runtime only captures the shell's bridge.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
    shell.register(CommandDef::new(
        "readAnswer",
        Vec::new(),
        "readAnswer - print the next line of stdin",
        |_: &[ArgValue], _ctx: &_| {
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).unwrap();
            println!("answer: {}", answer.trim_end());
            Ok(CommandOutcome::Continue)
        },
    ));
    shell.run_repl().unwrap();
}

#[test]
fn a_piped_shell_command_reads_the_next_script_line() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_shell", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("IOCSH_PS1", PROMPT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"readAnswer\nnotACommand\necho after\n")
        .unwrap();
    drop(stdin);

    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("the shell hangs on a command that reads stdin");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (mut out, mut err) = (String::new(), String::new());
    child.stdout.unwrap().read_to_string(&mut out).unwrap();
    child.stderr.unwrap().read_to_string(&mut err).unwrap();
    let out = out.replace(PROMPT, "");
    assert!(out.contains("answer: notACommand\nafter\n"), "{out}");
    assert!(!err.contains("notACommand"), "{err}");
}
