//! **A name registered once MUST be callable from every shell in the process.**
//!
//! C has one command table — `iocshCommandHead`, the single list
//! `iocshRegister` writes (`iocsh.cpp:684-700`) and every `iocshBody` walks
//! (`:1290-1302`) — so a registrar's command is callable from `st.cmd` and from
//! the `epics>` prompt with nothing for the registrar to decide.
//!
//! This port builds up to four shells per `IocApplication::run` — the startup
//! script's, the `afterIocRunning` queue's, the interactive tail's, and the
//! protocol runner's — and each used to construct a `CommandRegistry` of its
//! own. A command therefore existed in whichever shells its owner remembered to
//! hand it to: `register_shell_command` reached every shell EXCEPT the script's,
//! and a command returned by a link-set installer could reach the script's by no
//! route at all, because that shell is constructed before `iocInit` runs the
//! installer. `casr`, `pvxsr`, `dbsr` and the qsrv runtime commands were four
//! samples of that one split.
//!
//! The two cases below are the two directions of the split, and neither needs a
//! second registration to pass.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandDef, CommandOutcome};

/// A command that does nothing but record that it was reached, so "callable"
/// means the handler ran and not merely that a lookup succeeded.
fn recorder(name: &'static str, into: Arc<Mutex<Vec<String>>>) -> CommandDef {
    CommandDef::new(
        name,
        vec![],
        "record that this command was reached",
        move |_args: &[ArgValue], _ctx: &_| {
            into.lock().unwrap().push(name.to_string());
            Ok(CommandOutcome::Continue)
        },
    )
}

/// A real `st.cmd` on disk: `on error break` is a script directive, and an
/// unknown command has to reach the shell as a script line for it to act on.
fn st_cmd(name: &str, lines: &[&str]) -> String {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write st.cmd");
    path.to_string_lossy().into_owned()
}

/// One boot covering both directions and both call sites.
///
/// `parityShellCmd` is registered through `register_shell_command`, the name
/// that used to mean "every shell but the script's"; `parityInstallerCmd` is
/// returned by a link-set installer, which C reaches through a registrar at
/// `initHookAfterCaLinkInit` and which runs here on the script's own `iocInit`
/// line — after the startup shell was built. Under `on error break` an unknown
/// command ends the script, so `run` returning `Ok` is itself part of the
/// proof; the recorded calls say which lines actually reached a handler.
///
/// The prompt half is the interactive tail's own construction —
/// `IocShell::new` on a thread of its own, as `run_uninitialized_tail` does —
/// against a database this run never saw, so the names can only have come from
/// the process's table.
#[epics_macros_rs::epics_test]
async fn one_registration_reaches_the_script_and_the_prompt() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let script = st_cmd(
        "one_command_table.cmd",
        &[
            "on error break",
            "parityShellCmd",
            "iocInit",
            "parityInstallerCmd",
        ],
    );

    let installer_calls = calls.clone();
    IocApplication::new()
        .port(0)
        .register_shell_command(recorder("parityShellCmd", calls.clone()))
        .register_link_set_installer(move |_db| async move {
            vec![recorder("parityInstallerCmd", installer_calls)]
        })
        .startup_script(&script)
        .run(|_config| async move { Ok(()) })
        .await
        .expect("no line of the script is an unknown command");

    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["parityShellCmd", "parityInstallerCmd"],
        "both script lines must reach their handler: a shell command before \
         `iocInit`, an installer's command after it"
    );

    // The prompt. `BlockingBridge::capture()` has to happen on the runtime;
    // the shell itself runs off it, as the interactive tail's does.
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let prompt_calls = calls.clone();
    std::thread::spawn(move || {
        let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
        for name in ["parityShellCmd", "parityInstallerCmd"] {
            shell
                .execute_line(name)
                .unwrap_or_else(|e| panic!("`{name}` at the prompt: {e}"));
        }
        drop(prompt_calls);
    })
    .join()
    .expect("the prompt shell's thread");

    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [
            "parityShellCmd",
            "parityInstallerCmd",
            "parityShellCmd",
            "parityInstallerCmd"
        ],
        "the same two names, registered once each, must also be callable from \
         a shell built the way the interactive tail builds its own"
    );
}
