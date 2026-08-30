//! What the two shells put on stdout and stderr for the same malformed
//! and edge-case input.
//!
//! Every expectation here is MEASURED, not derived: each script was run
//! through `~/work/epics-base/bin/linux-x86_64/softIoc`
//! (R7.0.10-146-g8f5015b663d764ad75df) with stdin on `/dev/null`, and the
//! bytes below are that run's two streams with the ANSI colour stripped
//! and `softMain`'s trailing interactive prompt removed — `softMain.cpp`
//! enters `iocsh(NULL)` after every script (`:247-253`), so C's stdout
//! always ends with one `epics> ` that belongs to the shell C is about to
//! start rather than to the script it just ran.
//!
//! The port paints the same escapes only when `use_ansi_color` says so,
//! a deliberate deviation from C painting unconditionally, so this
//! compares the stripped streams on both sides.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = match rest[at..].find('m') {
            Some(end) => &rest[at + end + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Run one script through a fresh shell with fds 1 and 2 pointed at
/// files, because the echo C compares against is written with `printf`
/// and never passes through the command context.
fn run(script: &str) -> (String, String) {
    let out_file = tempfile::NamedTempFile::new().expect("stdout capture");
    let err_file = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the shell writes through fds 1 and 2 for the length of one
    // script and this test is `serial`, so no other thread is holding
    // either while they are swapped.
    let (saved_out, saved_err) = unsafe {
        let saved = (libc::dup(1), libc::dup(2));
        libc::dup2(out_file.as_file().as_raw_fd(), 1);
        libc::dup2(err_file.as_file().as_raw_fd(), 2);
        saved
    };

    let db = Arc::new(PvDatabase::new());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let path = script.to_string();
    // The shell blocks, so it runs off the reactor thread the way an IOC
    // startup script does.
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_script(&path)).join();

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: restoring the two descriptors this call replaced.
    unsafe {
        libc::dup2(saved_out, 1);
        libc::dup2(saved_err, 2);
        libc::close(saved_out);
        libc::close(saved_err);
    }
    assert!(ran.is_ok(), "the shell panicked running {script}");

    (
        strip_ansi(&std::fs::read_to_string(out_file.path()).expect("stdout capture")),
        strip_ansi(&std::fs::read_to_string(err_file.path()).expect("stderr capture")),
    )
}

/// `(script name, contents, C's stdout, C's stderr)`.
///
/// A case whose scripts include a second file writes it as its own row
/// with no expectation of its own — the row that includes it carries the
/// measurement.
type Case = (&'static str, &'static str, &'static str, &'static str);

const CASES: &[Case] = &[
    // --- an unknown command ---
    (
        "k01.cmd",
        "nosuchcommand\n",
        "nosuchcommand\n",
        "ERROR k01.cmd line 1: Command 'nosuchcommand' not registered.\n",
    ),
    // --- too few arguments, both shapes: the shell runs the line and
    // the body refuses, unframed (`libComRegister.c:142-151`) ---
    (
        "k02.cmd",
        "epicsEnvSet ONLYONE\n",
        "epicsEnvSet ONLYONE\n",
        "Missing environment variable value argument.\n",
    ),
    (
        "k03.cmd",
        "epicsEnvSet\n",
        "epicsEnvSet\n",
        "Missing environment variable name argument.\n",
    ),
    // --- too many arguments: C reads the ones it declared and drops the
    // rest without a word (`iocsh.cpp:1288-1296`) ---
    (
        "k04.cmd",
        "dbl a b c d\n",
        "dbl a b c d\nNo record type\n",
        "",
    ),
    // --- a bad numeric argument, all three of C's sentences ---
    (
        "k06.cmd",
        "dbpr A1 notanumber\n",
        "dbpr A1 notanumber\n",
        "ERROR k06.cmd line 1: Invalid integer 'notanumber'.\n",
    ),
    (
        "k07.cmd",
        "dbpr A1 99999999999999999999\n",
        "dbpr A1 99999999999999999999\n",
        "ERROR k07.cmd line 1: Integer '99999999999999999999' out of range.\n",
    ),
    (
        "k08.cmd",
        "epicsThreadSleep notanumber\n",
        "epicsThreadSleep notanumber\n",
        "ERROR k08.cmd line 1: Invalid double 'notanumber'.\n",
    ),
    // --- an unterminated quote (`iocsh.cpp:362-366`) ---
    (
        "k09.cmd",
        "dbl \"abc\n",
        "dbl \"abc\n",
        "ERROR k09.cmd line 1: Unbalanced quote.\n",
    ),
    // --- unmatched parens and stray commas: C's separator set is
    // uniform, so all three of these run `dbl` and say nothing ---
    ("k10.cmd", "dbl(\n", "dbl(\n", ""),
    ("k11.cmd", "dbl)\n", "dbl)\n", ""),
    // `zzz` and not a real record type on purpose: the point is that
    // the comma separated and the word behind it reached `dbl` as its
    // argument, which "No record type" proves and an empty listing would
    // not.
    ("k12.cmd", "dbl , zzz\n", "dbl , zzz\nNo record type\n", ""),
    ("k13.cmd", "echo(hello)\n", "echo(hello)\nhello\n", ""),
    // A quote is removed wherever it sits, not stripped off the ends.
    ("k14.cmd", "echo(a\"b\"c)\n", "echo(a\"b\"c)\nabc\n", ""),
    // --- comments, empty and blank lines (`iocsh.cpp:1196-1204`) ---
    ("k15.cmd", "# a comment\n", "# a comment\n", ""),
    ("k16.cmd", "#- silent\n", "", ""),
    ("k17.cmd", "\n", "", ""),
    // The blank line IS echoed, with its whitespace intact.
    ("k18.cmd", "   \t  \n", "   \t  \n", ""),
    // --- `<` to a file that will not open: reported by `iocshBody`
    // itself (`iocsh.cpp:1053-1058`), so unframed — there is no line
    // number yet ---
    (
        "k19.cmd",
        "< nosuch.cmd\n",
        "< nosuch.cmd\n",
        "Can't open nosuch.cmd: No such file or directory\n",
    ),
    // --- a malformed line inside a redirect: framed with the INNER
    // file's name, and the outer script runs on ---
    ("kq.cmd", "echo \"unterminated\n", "", ""),
    (
        "k20.cmd",
        "< kq.cmd\necho \"outer continues\"\n",
        "< kq.cmd\necho \"unterminated\necho \"outer continues\"\nouter continues\n",
        "ERROR kq.cmd line 1: Unbalanced quote.\n",
    ),
    // --- `exit` inside a redirect ends only the inner body
    // (`iocsh.cpp:1240`) ---
    (
        "ksub.cmd",
        "echo \"in sub\"\nexit\necho \"never\"\n",
        "",
        "",
    ),
    (
        "k21.cmd",
        "< ksub.cmd\necho \"outer after exit\"\n",
        "< ksub.cmd\necho \"in sub\"\nin sub\nexit\necho \"outer after exit\"\nouter after exit\n",
        "",
    ),
    // --- a failure inside a redirect: the inner file's name again, and
    // the outer script still runs on ---
    ("kbad.cmd", "nosuchcmd\n", "", ""),
    (
        "k27.cmd",
        "< kbad.cmd\necho \"outer after failure\"\n",
        "< kbad.cmd\nnosuchcmd\necho \"outer after failure\"\nouter after failure\n",
        "ERROR kbad.cmd line 1: Command 'nosuchcmd' not registered.\n",
    ),
    // --- an undefined macro: `macDefExpand` returns NULL, so the line is
    // never echoed and never looked up, and macLib has raised the only
    // diagnostic (`iocsh.cpp:1184-1187`) ---
    (
        "k22.cmd",
        "dbl $(UNDEF)\n",
        "",
        "macLib: macro UNDEF is undefined (expanding string dbl $(UNDEF))\n",
    ),
    // --- a variable set then read back through `var` ---
    (
        "k24.cmd",
        "var dbTemplateMaxVars 200\nvar dbTemplateMaxVars\n",
        "var dbTemplateMaxVars 200\nvar dbTemplateMaxVars\nint dbTemplateMaxVars = 200\n",
        "",
    ),
    // --- a name no variable matches: written by `varCallFunc` straight
    // to stderr and failed with a bare `iocshSetError(1)`
    // (`iocsh.cpp:1460-1464`), so it wears no frame ---
    (
        "k25.cmd",
        "var nosuchvar\n",
        "var nosuchvar\n",
        "No known vars match 'nosuchvar'.\n",
    ),
    // --- `help` for a name nothing registers: silent on both streams ---
    (
        "k26.cmd",
        "help nosuchcommand\n",
        "help nosuchcommand\n",
        "",
    ),
    // --- `cd` to a directory that is not there: SIX WORDS on stderr and
    // nothing else. C's `chdirCallFunc` (`libComRegister.c:108-116`) hands
    // `chdir`'s return to `iocshSetError`, so the line IS errored, but the
    // sentence it prints is its own and unframed — no file, no line
    // number, no directory name and no errno. The script runs on because
    // `onerr` defaults to Continue (`iocsh.cpp:1001`, `:1127-1130`).
    (
        "k28.cmd",
        "cd topbin\necho \"after\"\n",
        "cd topbin\necho \"after\"\nafter\n",
        "Invalid directory path, ignored\n",
    ),
    // The SAME sentence with no argument at all — and here C's `||`
    // short-circuits before `iocshSetError`, so this one does not even
    // error the line.
    (
        "k29.cmd",
        "cd\necho \"after\"\n",
        "cd\necho \"after\"\nafter\n",
        "Invalid directory path, ignored\n",
    ),
    // --- `iocshLoad`'s pathname up against the separator set: the `,` is
    // a separator like any other (`iocsh.cpp:271`), so the file opened is
    // `inner.iocsh` and not `inner.iocsh,`. The inner script's line is
    // echoed by the inner body, between the outer echo and the output.
    ("inner.iocsh", "echo \"inner ran\"\n", "", ""),
    (
        "k30.cmd",
        "iocshLoad inner.iocsh, \"P=X\"\n",
        "iocshLoad inner.iocsh, \"P=X\"\necho \"inner ran\"\ninner ran\n",
        "",
    ),
];

#[epics_macros_rs::epics_test]
#[serial_test::serial(iocsh_stream_parity)]
async fn our_streams_match_the_bytes_c_wrote() {
    let dir = tempfile::tempdir().expect("script root");
    let saved_cwd = std::env::current_dir().expect("cwd");
    // A `<` inside a script resolves against the process cwd, as C's
    // does, so the scripts have to be reached by the names they use for
    // each other.
    std::env::set_current_dir(dir.path()).expect("cwd");

    for (name, body, _, _) in CASES {
        std::fs::write(dir.path().join(name), body).expect("script");
    }

    let mut wrong = Vec::new();
    for (name, _, want_out, want_err) in CASES {
        // The rows that exist only to be included by another carry no
        // expectation and are not run on their own.
        if want_out.is_empty() && want_err.is_empty() {
            continue;
        }
        let (got_out, got_err) = run(name);
        if got_out != *want_out {
            wrong.push(format!(
                "{name} stdout\n     C: {want_out:?}\n  ours: {got_out:?}"
            ));
        }
        if got_err != *want_err {
            wrong.push(format!(
                "{name} stderr\n     C: {want_err:?}\n  ours: {got_err:?}"
            ));
        }
    }

    std::env::set_current_dir(saved_cwd).expect("cwd");
    assert!(
        wrong.is_empty(),
        "the two shells wrote different bytes:\n{}",
        wrong.join("\n")
    );
}
