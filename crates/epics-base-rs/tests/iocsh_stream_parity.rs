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
//!
//! Unix only: what is captured is the process console, and the only way to
//! capture it is to point fds 1 and 2 somewhere else and put them back. There
//! is no fd 2 on Windows, and `libc` is a `cfg(unix)` dependency of this crate.

#![cfg(unix)]

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
    // --- a line of separators only ends the body (`iocsh.cpp:1225-1227`) ---
    (
        "k40.cmd",
        "echo one\n,\necho two\n",
        "echo one\none\n,\n",
        "",
    ),
    (
        "k41.cmd",
        "echo one\n()\necho two\n",
        "echo one\none\n()\n",
        "",
    ),
    // --- `exit` inside `iocshCmd` ends only that body ---
    (
        "k42.cmd",
        "iocshCmd exit\necho a1\niocshCmd \"()\"\necho a2\n",
        "iocshCmd exit\necho a1\na1\niocshCmd \"()\"\necho a2\na2\n",
        "",
    ),
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
    // --- a registered command's own failure is printed in its own text,
    // unframed: `showError` frames only what `iocshBody` raises before
    // the call (`iocsh.cpp:1272-1304`), and `onCallFunc` prints its
    // usage itself (`:1529-1530`, `:1571-1573`) ---
    (
        "k31.cmd",
        "on error bogus\necho \"after\"\n",
        "on error bogus\necho \"after\"\nafter\n",
        "Usage: on error [continue | break | halt | wait <delay>]\n",
    ),
    // --- a shell error inside `iocshCmd` is raised by an `iocshBody`
    // whose filename is NULL, so it too wears no frame; that body's
    // implied `on error break` then reports itself ---
    (
        "k32.cmd",
        "iocshCmd \"dbpr A1 notanumber\"\necho \"after\"\n",
        "iocshCmd \"dbpr A1 notanumber\"\necho \"after\"\nafter\n",
        "Invalid integer 'notanumber'.\niocsh Error: Break\n",
    ),
    // --- a redirect with no target: `split` refuses the line, so even
    // `<` alone runs nothing (`iocsh.cpp:351-355`) ---
    (
        "k33.cmd",
        "<\ndbl >\necho after\n",
        "<\ndbl >\necho after\nafter\n",
        "ERROR k33.cmd line 1: Invalid redirection.\nERROR k33.cmd line 2: Invalid redirection.\n",
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

/// `(script name, contents, C's stdout, C's stderr, the redirect target,
/// what C left in it)`.
type RedirectCase = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

/// Measured the same way as [`CASES`], plus the redirect target's bytes.
/// C's `split` lifts the redirect off every line before `argv[0]` is looked
/// at, so the commands the port dispatches itself (`iocshLoad`, `iocshCmd`,
/// `iocshRun`, `on`) are redirected like any other; and what C frames
/// before it calls the function (`cvtArg`, a registry miss) is printed
/// before `startRedirect`, on the console.
const REDIRECT_CASES: &[RedirectCase] = &[
    (
        "r1.cmd",
        "iocshLoad inner.iocsh > r1.txt\n",
        "iocshLoad inner.iocsh > r1.txt\n",
        "",
        "r1.txt",
        "echo \"inner ran\"\ninner ran\n",
    ),
    (
        "r2.cmd",
        "iocshCmd \"echo cmdran\" > r2.txt\n",
        "iocshCmd \"echo cmdran\" > r2.txt\n",
        "",
        "r2.txt",
        "cmdran\n",
    ),
    (
        "r3.cmd",
        "iocshRun \"echo runran\" > r3.txt\n",
        "iocshRun \"echo runran\" > r3.txt\n",
        "",
        "r3.txt",
        "runran\n",
    ),
    (
        "r4.cmd",
        "on error bogus 2> r4.txt\n",
        "on error bogus 2> r4.txt\n",
        "",
        "r4.txt",
        "Usage: on error [continue | break | halt | wait <delay>]\n",
    ),
    (
        "r5.cmd",
        "iocshCmd \"dbpr A1 notanumber\" 2> r5.txt\n",
        "iocshCmd \"dbpr A1 notanumber\" 2> r5.txt\n",
        "",
        "r5.txt",
        "Invalid integer 'notanumber'.\niocsh Error: Break\n",
    ),
    // The inner body frames its own miss on the stderr it was handed.
    (
        "r6.cmd",
        "iocshLoad bad.iocsh 2> r6.txt\n",
        "iocshLoad bad.iocsh 2> r6.txt\nnosuchcmd\n",
        "",
        "r6.txt",
        "ERROR bad.iocsh line 1: Command 'nosuchcmd' not registered.\n",
    ),
    (
        "r8.cmd",
        "dbpr A1 notanumber 2> r8.txt\n",
        "dbpr A1 notanumber 2> r8.txt\n",
        "ERROR r8.cmd line 1: Invalid integer 'notanumber'.\n",
        "r8.txt",
        "",
    ),
    (
        "r9.cmd",
        "nosuchcmd 2> r9.txt\n",
        "nosuchcmd 2> r9.txt\n",
        "ERROR r9.cmd line 1: Command 'nosuchcmd' not registered.\n",
        "r9.txt",
        "",
    ),
    (
        "r10.cmd",
        "on error wait 2> r10.txt\n",
        "on error wait 2> r10.txt\n",
        "",
        "r10.txt",
        "Usage: on error [continue | break | halt | wait <delay>]\n",
    ),
    // --- the loaders write C's raw `stdout`/`stderr` (`dbAccess.c`,
    // `dbLexRoutines.c` and `dbLoadTemplate.y` do not include
    // `epicsStdio.h`), which no redirect swaps: the console has every
    // line and the target is created empty ---
    (
        "r11.cmd",
        "dbLoadRecords nosuch.db 2> r11.txt\n",
        "dbLoadRecords nosuch.db 2> r11.txt\n",
        "ERROR: Can't open file 'nosuch.db'\nERROR: Failed to load 'nosuch.db'\n",
        "r11.txt",
        "",
    ),
    (
        "r12.cmd",
        "dbLoadRecords badfield.db 2> r12.txt\n",
        "dbLoadRecords badfield.db 2> r12.txt\n",
        "ERROR: ai record 'C1' doesn't have a field 'NOSUCHFLD'\n    Did you mean \"DESC\"?  (Descriptor)\nERROR:  at or before ')' in path \".\"  file \"badfield.db\" line 1\n\n 1 | record(ai,\"C1\"){ field(NOSUCHFLD,\"1\") }\n\nERROR: Failed to load 'badfield.db'\n",
        "r12.txt",
        "",
    ),
    (
        "r13.cmd",
        "dbLoadRecords > r13.txt\n",
        "dbLoadRecords > r13.txt\nUsage: dbLoadRecords \"file\", \"subs\"\n",
        "",
        "r13.txt",
        "",
    ),
    (
        "r14.cmd",
        "dbLoadDatabase > r14.txt\n",
        "dbLoadDatabase > r14.txt\nUsage: dbLoadDatabase \"file\", \"path\", \"subs\"\n",
        "",
        "r14.txt",
        "",
    ),
    (
        "r15.cmd",
        "dbLoadTemplate 2> r15.txt\n",
        "dbLoadTemplate 2> r15.txt\n",
        "must specify variable substitution file\n",
        "r15.txt",
        "",
    ),
    (
        "r16.cmd",
        "dbLoadTemplate nosuch.substitutions 2> r16.txt\n",
        "dbLoadTemplate nosuch.substitutions 2> r16.txt\n",
        "dbLoadTemplate: error opening sub file nosuch.substitutions: No such file or directory\n",
        "r16.txt",
        "",
    ),
    (
        "r17.cmd",
        "dbLoadTemplate s1.substitutions 2> r17.txt\n",
        "dbLoadTemplate s1.substitutions 2> r17.txt\n",
        "ERROR: Can't open file 'nosuchtmpl.db'\nERROR: Failed to load 'nosuchtmpl.db'\ndbLoadRecords(\"nosuchtmpl.db\", P=1)\nSubstitution file error: Error while reading included file\nline 1: '}'\n",
        "r17.txt",
        "",
    ),
    // --- `exit` ends the body before `openRedirect` (`iocsh.cpp:1240`),
    // so its target is never created ---
    (
        "r18.cmd",
        "exit > r18.txt\necho notreached\n",
        "exit > r18.txt\n",
        "",
        "r18.txt",
        "<not created>",
    ),
    // --- C `split`: a redirect anywhere, several per line, one word each ---
    (
        "p1.cmd",
        "echo hi > p1a.txt 2> p1b.txt\n",
        "echo hi > p1a.txt 2> p1b.txt\n",
        "",
        "p1a.txt",
        "hi\n",
    ),
    (
        "p1b.cmd",
        "echo hi > p1c.txt 2> p1d.txt\n",
        "echo hi > p1c.txt 2> p1d.txt\n",
        "",
        "p1d.txt",
        "",
    ),
    (
        "p2.cmd",
        "> p2.txt echo hi\n",
        "> p2.txt echo hi\n",
        "",
        "p2.txt",
        "hi\n",
    ),
    (
        "p3.cmd",
        "echo > p3.txt hi\n",
        "echo > p3.txt hi\n",
        "",
        "p3.txt",
        "hi\n",
    ),
    (
        "p4.cmd",
        "echo hi >\n",
        "echo hi >\n",
        "ERROR p4.cmd line 1: Invalid redirection.\n",
        "p4.txt",
        "<not created>",
    ),
    (
        "p5.cmd",
        "echo hi > \"p 5.txt\"\n",
        "echo hi > \"p 5.txt\"\n",
        "",
        "p 5.txt",
        "hi\n",
    ),
    (
        "p6.cmd",
        "> p6.txt exit\necho notreached\n",
        "> p6.txt exit\n",
        "",
        "p6.txt",
        "<not created>",
    ),
    (
        "p7.cmd",
        "< inner.iocsh > p7.txt\n",
        "< inner.iocsh > p7.txt\n",
        "",
        "p7.txt",
        "echo \"inner ran\"\ninner ran\n",
    ),
    (
        "p8.cmd",
        "echo a1>p8.txt\n",
        "echo a1>p8.txt\n",
        "",
        "p8.txt",
        "a\n",
    ),
];

#[epics_macros_rs::epics_test]
#[serial_test::serial(iocsh_stream_parity)]
async fn redirect_targets_hold_the_bytes_c_wrote() {
    let dir = tempfile::tempdir().expect("script root");
    let saved_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(dir.path()).expect("cwd");
    std::fs::write("inner.iocsh", "echo \"inner ran\"\n").expect("script");
    std::fs::write("bad.iocsh", "nosuchcmd\n").expect("script");
    std::fs::write(
        "badfield.db",
        "record(ai,\"C1\"){ field(NOSUCHFLD,\"1\") }\n",
    )
    .expect("db");
    std::fs::write("s1.substitutions", "file nosuchtmpl.db { {P=1} }\n").expect("subs");

    let mut wrong = Vec::new();
    for (name, body, want_out, want_err, target, want_file) in REDIRECT_CASES {
        std::fs::write(name, body).expect("script");
        let (got_out, got_err) = run(name);
        let got_file = std::fs::read_to_string(target)
            .map(|s| strip_ansi(&s))
            .unwrap_or_else(|_| "<not created>".into());
        for (what, want, got) in [
            ("stdout", *want_out, got_out),
            ("stderr", *want_err, got_err),
            (*target, *want_file, got_file),
        ] {
            if got != want {
                wrong.push(format!("{name} {what}\n     C: {want:?}\n  ours: {got:?}"));
            }
        }
    }

    std::env::set_current_dir(saved_cwd).expect("cwd");
    assert!(
        wrong.is_empty(),
        "the two shells wrote different bytes:\n{}",
        wrong.join("\n")
    );
}
