//! Regression tests (R13-26): the getopt loop runs in command-line order, and
//! `-h`/`-V` end it where they stand.
//!
//! C processes options STRICTLY in argv order (`caget.c:398`, and the identical
//! loops in the other three tools), and `case 'h'` / `case 'V'` `return` from
//! `main` the moment the loop reaches them (`caget.c:399-405`). Two consequences
//! fall straight out of that single loop:
//!
//! ```text
//! caget -w abc -h    C: warns about 'abc', THEN prints the usage block
//! caget -h -w abc    C: prints the usage block; 'abc' is never scanned, never warns
//! caget -w abc -p xyz    C: the timeout warning, then the priority warning
//! caget -p xyz -w abc    C: the priority warning, then the timeout warning
//! ```
//!
//! clap has no loop: it parses all of argv at once, and its own Help/Version
//! actions terminate the process before any `copt` resolver runs — so `-h`
//! SWALLOWED every warning that C prints first, no matter where it sat. `-h` and
//! `-V` are now ordinary `Count` options, and `copt::Scan` buffers each warning
//! with the argv position getopt would have raised it at, replaying them in that
//! order up to the first terminal option (`Scan::finish`).

use std::process::Command;

fn stderr_of(bin: &str, args: &[&str]) -> String {
    let out = Command::new(bin)
        .args(args)
        .output()
        .expect("spawn the CA tool");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The four tools and the warning each one's `-w` raises on an unscannable
/// timeout (`tool_lib.c`'s `epicsScanDouble` arm — same text in all four).
const TIMEOUT_WARNING: &str = "'abc' is not a valid timeout value";

fn tools() -> [(&'static str, &'static str); 4] {
    [
        (env!("CARGO_BIN_EXE_caget-rs"), "caget"),
        (env!("CARGO_BIN_EXE_camonitor-rs"), "camonitor"),
        (env!("CARGO_BIN_EXE_caput-rs"), "caput"),
        (env!("CARGO_BIN_EXE_cainfo-rs"), "cainfo"),
    ]
}

/// A warning raised BEFORE the loop reaches `-h` is already on stderr when the
/// usage block prints. clap's Help action used to exit first and lose it.
#[test]
fn a_warning_before_help_survives_it() {
    for (bin, tool) in tools() {
        let stderr = stderr_of(bin, &["-w", "abc", "-h"]);
        assert!(
            stderr.contains(TIMEOUT_WARNING),
            "{tool} -w abc -h: C scans '-w' before it reaches `case 'h'`, so the warning is \
             already printed; stderr was:\n{stderr}"
        );
    }
}

/// `case 'h'` returns from `main`, so an option AFTER it is never scanned and
/// cannot warn — the same fact seen from the other side. (The usage block
/// itself shares this stream: C's `usage()` is an `fprintf(stderr, ...)`, so
/// what must be absent here is the WARNING, not all output — R14-16.)
#[test]
fn an_option_after_help_is_never_scanned() {
    for (bin, tool) in tools() {
        let stderr = stderr_of(bin, &["-h", "-w", "abc"]);
        assert!(
            !stderr.contains(TIMEOUT_WARNING),
            "{tool} -h -w abc: `case 'h'` returns before the loop ever reaches '-w'; \
             stderr was:\n{stderr}"
        );
    }
}

/// `case 'V'` returns from `main` exactly as `case 'h'` does (`caget.c:403-405`).
#[test]
fn version_ends_the_loop_where_it_stands() {
    for (bin, tool) in tools() {
        let before = stderr_of(bin, &["-w", "abc", "-V"]);
        assert!(
            before.contains(TIMEOUT_WARNING),
            "{tool} -w abc -V: the warning precedes the version banner; stderr was:\n{before}"
        );
        let after = stderr_of(bin, &["-V", "-w", "abc"]);
        assert!(
            after.is_empty(),
            "{tool} -V -w abc: '-w' is never scanned; stderr was:\n{after}"
        );
    }
}

/// Two bad options warn in the order the command line gave them — not in the
/// order the binary happens to resolve its fields in. Swapping the two swaps the
/// stderr lines, which is the property that separates a real getopt loop from a
/// fixed resolver sequence.
#[test]
fn warnings_come_out_in_command_line_order() {
    let bin = env!("CARGO_BIN_EXE_caget-rs");
    let timeout = "'abc' is not a valid timeout value";
    let priority = "'xyz' is not a valid CA priority";

    let stderr = stderr_of(bin, &["-w", "abc", "-p", "xyz"]);
    let lines: Vec<&str> = stderr.lines().collect();
    assert!(
        lines[0].contains(timeout) && lines[1].contains(priority),
        "caget -w abc -p xyz: '-w' is scanned first; stderr was:\n{stderr}"
    );

    let stderr = stderr_of(bin, &["-p", "xyz", "-w", "abc"]);
    let lines: Vec<&str> = stderr.lines().collect();
    assert!(
        lines[0].contains(priority) && lines[1].contains(timeout),
        "caget -p xyz -w abc: '-p' is scanned first; stderr was:\n{stderr}"
    );
}

/// The loop stops AT the terminal option: a warning before `-h` prints, one
/// after it does not, in a single command line.
#[test]
fn help_cuts_the_loop_in_the_middle() {
    let bin = env!("CARGO_BIN_EXE_caget-rs");
    let stderr = stderr_of(bin, &["-w", "abc", "-h", "-p", "xyz"]);
    assert!(
        stderr.contains("'abc' is not a valid timeout value"),
        "the '-w' before '-h' warns; stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains("'xyz' is not a valid CA priority"),
        "the '-p' after '-h' is never reached; stderr was:\n{stderr}"
    );
}
