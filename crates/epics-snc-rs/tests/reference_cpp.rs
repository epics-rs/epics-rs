//! `cpp::preprocess` against the system `gcc -E` (GCC 13), the preprocessor
//! the EPICS build runs on a `.st`: output and diagnostics for every `.st`
//! and `.stt` of the modules tree, and the display column of every
//! codepoint.

use std::path::{Path, PathBuf};
use std::process::Command;

use epics_base_rs::reference::{ReferenceTree, reference_root};
use epics_snc_rs::cpp;

/// The `-D`s `$(CPP)` gets on linux-x86_64 for RULES_SNCSEQ's `%.i: %.st`.
const DEFINES: &[&str] = &[
    "_GNU_SOURCE",
    "_DEFAULT_SOURCE",
    "USE_TYPED_RSET",
    "_X86_64_",
    "UNIX",
    "linux",
];

fn programs(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).expect("read dir") {
        let e = e.expect("dir entry");
        let name = e.file_name().to_string_lossy().into_owned();
        // `.git`, and the AppleDouble `._x.st` a macOS copy leaves.
        if name.starts_with('.') {
            continue;
        }
        if e.file_type().expect("file type").is_dir() {
            if !name.starts_with("O.") {
                programs(&e.path(), out);
            }
        } else if name.ends_with(".st") || name.ends_with(".stt") {
            out.push(e.path());
        }
    }
}

/// gcc on `args`: standard output and the diagnostic lines.
fn gcc(args: &[String]) -> (Vec<u8>, Vec<String>) {
    let out = Command::new("gcc")
        .args(["-x", "c", "-E", "-fno-diagnostics-show-caret"])
        .args(args)
        .output()
        .expect("run gcc");
    let diags = String::from_utf8_lossy(&out.stderr)
        .lines()
        .map(str::to_owned)
        .collect();
    (out.stdout, diags)
}

/// Output from the main file's first line marker on: gcc's `<built-in>`,
/// `<command-line>` and `stdc-predef.h` part comes before it.
fn from_main<'a>(text: &'a [u8], path: &Path) -> &'a [u8] {
    let marker = format!("# 1 \"{}\"\n", path.display());
    let at = text
        .windows(marker.len())
        .position(|w| w == marker.as_bytes())
        .unwrap_or_else(|| panic!("no {marker:?} in gcc output"));
    &text[at..]
}

#[test]
fn modules_programs_preprocess_as_gcc() {
    let modules = reference_root(ReferenceTree::Modules);
    let base = reference_root(ReferenceTree::Base);
    let mut files = Vec::new();
    programs(&modules, &mut files);
    files.sort();
    assert!(files.len() > 100, "{} programs", files.len());
    let mut failures = Vec::new();
    for f in &files {
        // The build runs cpp in O.<arch> with -I. -I../O.Common -I..,
        // then the module's install include and the RELEASE and base ones.
        let src = f.parent().expect("source dir");
        let top = src
            .ancestors()
            .find(|d| d.join("configure").is_dir())
            .unwrap_or(src);
        let include_dirs = [
            src.join("O.linux-x86_64"),
            src.join("O.Common"),
            src.to_path_buf(),
            top.join("include"),
            modules.join("sequencer/include"),
            base.join("include/compiler/gcc"),
            base.join("include/os/Linux"),
            base.join("include"),
        ];
        let mut args: Vec<String> = DEFINES.iter().map(|d| format!("-D{d}")).collect();
        args.extend(include_dirs.iter().map(|d| format!("-I{}", d.display())));
        args.push(f.display().to_string());
        let (gcc_text, gcc_diags) = gcc(&args);
        let defines: Vec<_> = DEFINES.iter().map(|d| (d.to_string(), None)).collect();
        let text = std::fs::read(f).expect("read program");
        let out = cpp::preprocess(f, f, &text, &defines, &include_dirs);
        let diags: Vec<_> = out.diagnostics.iter().map(|d| d.to_string()).collect();
        if out.text != from_main(&gcc_text, f) {
            failures.push(format!("{}: output differs", f.display()));
        }
        if diags != gcc_diags {
            failures.push(format!("{}: {diags:?} vs gcc {gcc_diags:?}", f.display()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The column of an unterminated `"` after each codepoint in a comment,
/// which is 6 plus the codepoint's display width.
#[test]
fn every_codepoint_has_gcc_display_width() {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cpp-wcwidth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let path = dir.join("w.c");
    let mut text = Vec::new();
    for c in (1..=0x10ffff).filter_map(char::from_u32) {
        if !matches!(c, '\n' | '\r' | '*' | '/' | '\\') {
            text.extend_from_slice(format!("/*{c}*/ \"\n").as_bytes());
        }
    }
    std::fs::write(&path, &text).expect("write probe");
    let (_, gcc_diags) = gcc(&[path.display().to_string()]);
    let unterminated = |d: &String| d.contains("missing terminating");
    let gcc_diags: Vec<_> = gcc_diags.into_iter().filter(unterminated).collect();
    let out = cpp::preprocess(&path, &path, &text, &[], &[]);
    let diags: Vec<_> = out
        .diagnostics
        .iter()
        .map(|d| d.to_string())
        .filter(unterminated)
        .collect();
    assert_eq!(diags.len(), gcc_diags.len());
    let first = diags.iter().zip(&gcc_diags).find(|(a, b)| a != b);
    assert!(first.is_none(), "{first:?}");
    std::fs::remove_dir_all(&dir).expect("remove scratch dir");
}
