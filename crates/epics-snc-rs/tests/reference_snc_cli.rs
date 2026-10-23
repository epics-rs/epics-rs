//! The snc-rs command line against C snc's: the same arguments, run in
//! directories of their own, give the same messages, exit status and
//! output files. The cases stop before analysis or pass it without a
//! message, so they hold while the port has no analysis yet.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use epics_base_rs::reference::{ReferenceTree, reference_path};

const OK: &str = "program p ss s { state a { when () {} exit } }\n";
const GIVE_UP: &str = "program p ss s { state { } }\n";

struct Case {
    name: &'static str,
    args: &'static [&'static str],
    /// Files the run leaves besides the inputs.
    outputs: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        name: "no_arguments",
        args: &[],
        outputs: &[],
    },
    Case {
        name: "o_without_name",
        args: &["ok.st", "-o"],
        outputs: &[],
    },
    Case {
        name: "no_input",
        args: &["+a", "-c"],
        outputs: &[],
    },
    Case {
        name: "unknown_options",
        args: &["-x", "+", "-", "-i", "nope.st"],
        outputs: &[],
    },
    Case {
        name: "empty_input_name",
        args: &[""],
        outputs: &[],
    },
    Case {
        name: "input_is_directory",
        args: &["."],
        outputs: &[],
    },
    Case {
        name: "output_dir_missing",
        args: &["ok.st", "-o", "/nonexistent/x.c"],
        outputs: &[],
    },
    Case {
        name: "output_is_directory",
        args: &["ok.st", "-o", "."],
        outputs: &[],
    },
    Case {
        name: "parser_gives_up",
        args: &["give_up.st", "-o", "g.c"],
        outputs: &[],
    },
    Case {
        name: "accepted",
        args: &["+r", "-l", "ok.st", "-o", "out.c"],
        outputs: &["out.c"],
    },
    Case {
        name: "last_input_and_o_win",
        args: &["nope.st", "ok.st", "-o", "a.c", "-o", "b.c"],
        outputs: &["b.c"],
    },
];

fn c_snc() -> PathBuf {
    reference_path(ReferenceTree::Modules, "sequencer/bin/linux-x86_64/snc")
}

fn rs_snc() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_snc-rs"))
}

/// What one run left behind.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    status: Option<i32>,
    stderr: String,
    outputs: BTreeSet<String>,
}

fn run(bin: &Path, dir: &Path, args: &[&str]) -> Run {
    std::fs::create_dir_all(dir).expect("create run dir");
    std::fs::write(dir.join("ok.st"), OK).expect("write ok.st");
    std::fs::write(dir.join("give_up.st"), GIVE_UP).expect("write give_up.st");
    let out = Command::new(bin)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("{}: {e}", bin.display()));
    let outputs = std::fs::read_dir(dir)
        .expect("list run dir")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|n| n != "ok.st" && n != "give_up.st")
        .collect();
    Run {
        status: out.status.code(),
        stderr: release_line_masked(&String::from_utf8(out.stderr).expect("utf-8 stderr")),
        outputs,
    }
}

/// `Sequencer release <ver>, compiled <ctime>` with the version masked:
/// C has the sequencer's, the port its own. The date must have Perl's
/// scalar `localtime` shape, `Www Mmm dd hh:mm:ss yyyy`.
fn release_line_masked(stderr: &str) -> String {
    stderr
        .lines()
        .map(|line| match line.strip_prefix("Sequencer release ") {
            Some(rest) => {
                let (_, date) = rest
                    .split_once(", compiled ")
                    .unwrap_or_else(|| panic!("release line without date: {line:?}"));
                assert!(is_perl_localtime(date), "release date shape: {line:?}");
                "Sequencer release <ver>, compiled <date>\n".to_string()
            }
            None => format!("{line}\n"),
        })
        .collect()
}

fn is_perl_localtime(s: &str) -> bool {
    let b = s.as_bytes();
    let digit = |i: usize| b[i].is_ascii_digit();
    b.len() == 24
        && b[..3].iter().all(u8::is_ascii_alphabetic)
        && b[4..7].iter().all(u8::is_ascii_alphabetic)
        && (b[8] == b' ' || digit(8))
        && digit(9)
        && [11, 12, 14, 15, 17, 18, 20, 21, 22, 23]
            .iter()
            .all(|&i| digit(i))
        && [3, 7, 10, 19].iter().all(|&i| b[i] == b' ')
        && b[13] == b':'
        && b[16] == b':'
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn command_line_runs_as_c_snc() {
    let dir = scratch_dir("snc_cli");
    let mut failures = Vec::new();
    for case in CASES {
        let c = run(&c_snc(), &dir.join("c").join(case.name), case.args);
        let rs = run(&rs_snc(), &dir.join("rs").join(case.name), case.args);
        let want: BTreeSet<String> = case.outputs.iter().map(|s| s.to_string()).collect();
        if c.outputs != want {
            failures.push(format!(
                "{}: C left {:?}, case says {want:?}",
                case.name, c.outputs
            ));
        }
        if c != rs {
            failures.push(format!("{}:\n  C:  {c:?}\n  rs: {rs:?}", case.name));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    std::fs::remove_dir_all(&dir).expect("remove scratch dir");
}

/// Without `-o`, C names the output `<stem>.c`; the port generates Rust and
/// names it `<stem>.rs`.
#[test]
fn default_output_name_is_rs_for_c_dot_c() {
    let dir = scratch_dir("snc_cli_default");
    let c = run(&c_snc(), &dir.join("c"), &["ok.st"]);
    let rs = run(&rs_snc(), &dir.join("rs"), &["ok.st"]);
    assert_eq!((c.status, c.stderr.as_str()), (Some(0), ""));
    assert_eq!((rs.status, rs.stderr.as_str()), (Some(0), ""));
    assert_eq!(c.outputs, BTreeSet::from(["ok.c".to_string()]));
    assert_eq!(rs.outputs, BTreeSet::from(["ok.rs".to_string()]));
    std::fs::remove_dir_all(&dir).expect("remove scratch dir");
}
