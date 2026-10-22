//! The C preprocessor the EPICS build runs on a `.st` before snc (`%.i:
//! %.st` in the sequencer's `RULES_SNCSEQ`), ported from GCC 13's cpplib
//! and its `-E` printer so snc sees the text, line markers included, that
//! `gcc -E` writes.
//!
//! Covered: `#include` with `""`/`<>` and `-I` directories, `#define` and
//! `#undef` with `#`, `##` and variadic macros, the conditionals with
//! `defined` and integer expressions, `#line`, `#error`/`#warning`,
//! `#pragma once`, comments, and `__FILE__`, `__LINE__`, `__BASE_FILE__`,
//! `__FILE_NAME__`, `__INCLUDE_LEVEL__`, `__COUNTER__`. The predefined
//! macros are the ones `gcc -undef` keeps; target macros (`linux`,
//! `__x86_64__`, …) come only from `defines`. System include directories
//! are not searched. `__DATE__`, `__TIME__`, `__TIMESTAMP__`, `_Pragma`,
//! `__has_include` and the other `__has_*`, `__VA_OPT__`, raw strings,
//! `#include_next`, `#import`, `#ident`, `#assert` and other pragmas are
//! errors.

mod column;
mod directive;
mod engine;
mod expr;
mod lex;
mod macros;
mod print;

use std::fmt;
use std::path::{Path, PathBuf};

use engine::{Engine, path_bytes};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Note,
    Warning,
    Error,
    /// An error that stopped preprocessing.
    Fatal,
}

/// A cpp diagnostic, located as gcc locates it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: String,
    /// 0 for `<command-line>` and `<built-in>`.
    pub line: u32,
    /// 0 for a diagnostic about a whole directive line.
    pub col: u32,
    pub severity: Severity,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let severity = match self.severity {
            Severity::Note => "note",
            Severity::Warning => "warning",
            Severity::Error => "error",
            Severity::Fatal => "fatal error",
        };
        write!(f, "{}:", self.file)?;
        if self.line != 0 {
            write!(f, "{}:", self.line)?;
            if self.col != 0 {
                write!(f, "{}:", self.col)?;
            }
        }
        write!(f, " {severity}: {}", self.message)
    }
}

/// What `gcc -E` leaves: its standard output, its diagnostics, and the
/// files it read.
#[derive(Debug)]
pub struct Output {
    pub text: Vec<u8>,
    pub diagnostics: Vec<Diagnostic>,
    /// Every file read, the main file first.
    pub files: Vec<PathBuf>,
}

impl Output {
    /// gcc's exit status would be non-zero.
    pub fn failed(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| matches!(d.severity, Severity::Error | Severity::Fatal))
    }
}

/// `gcc -E -D<name>[=<value>]… -I<dir>… <name>` on `text`, the contents
/// of `path`. Line markers, `__FILE__` and header names are spelled from
/// `name` and `include_dirs` as gcc spells them; a spelled path under
/// `name`'s directory is read from `path`'s.
pub fn preprocess(
    path: &Path,
    name: &Path,
    text: &[u8],
    defines: &[(String, Option<String>)],
    include_dirs: &[PathBuf],
) -> Output {
    let mut e = Engine::new(include_dirs.iter().map(|d| path_bytes(d)).collect());
    let cmdline = e.intern(b"<command-line>");
    for (name, value) in defines {
        // cpp_define: `-DX=v` is `#define X v`, `-DX` is `#define X 1`.
        let def = format!("{name} {}", value.as_deref().unwrap_or("1"));
        e.run_define(cmdline, def.as_bytes());
    }
    e.push_main(path_bytes(name), path.to_path_buf(), text);
    if !e.fatal {
        e.run();
    }
    Output {
        files: e.files.iter().map(|f| f.disk.clone()).collect(),
        diagnostics: std::mem::take(&mut e.diags),
        text: e.printer.finish(),
    }
}
