//! Preprocessing an upstream program, running the port's front end and
//! analysis the way C snc runs them, and the golden format of the analysis
//! fixtures.

#![allow(dead_code)] // each test binary uses its own part

use std::path::{Path, PathBuf};

use epics_snc_rs::analysis::analyse_program;
use epics_snc_rs::driver::Snc;
use epics_snc_rs::parser::parse_program;
use epics_snc_rs::types::{Ast, Program};

/// The command-line option sets every program is analysed under.
pub const OPTION_SETS: [&[&str]; 3] = [&[], &["+W"], &["-w"]];

/// One compiler run up to the end of analysis.
pub struct Run {
    pub snc: Snc,
    pub ast: Ast,
    /// `None` when parsing stopped the compiler before analysis
    pub prg: Option<Program>,
}

/// Parse and analyse `input` as `snc <opts> <input_name>` does.
pub fn analyse(input: &[u8], input_name: &str, opts: &[&str]) -> Run {
    let mut snc = Snc::capturing();
    let args: Vec<String> = std::iter::once("snc")
        .chain(opts.iter().copied())
        .chain([input_name])
        .map(String::from)
        .collect();
    snc.parse_args(&args).expect("snc arguments");
    let mut ast = Ast::new();
    let prg = match parse_program(&mut snc, &mut ast, input, input_name) {
        Ok(Some(exp)) => {
            let options = snc.options;
            Some(analyse_program(&mut snc, &mut ast, exp, options))
        }
        Ok(None) | Err(_) => None,
    };
    Run { snc, ast, prg }
}

pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/analysis")
}

/// The `.st` fixtures, sorted.
pub fn fixtures() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixture_dir())
        .expect("read fixture dir")
        .map(|e| e.expect("fixture entry").file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".st"))
        .collect();
    names.sort();
    names
}

/// The header of the golden section for one option set.
pub fn section_header(opts: &[&str], name: &str) -> String {
    let mut line = String::from("$ snc");
    for o in opts {
        line.push(' ');
        line.push_str(o);
    }
    format!("{line} {name}\n")
}

/// A fixture's golden: the diagnostics under each option set.
pub fn render(name: &str) -> String {
    let input = std::fs::read(fixture_dir().join(name)).expect("read fixture");
    let mut out = String::new();
    for opts in OPTION_SETS {
        let run = analyse(&input, name, opts);
        out.push_str(&section_header(opts, name));
        out.push_str(&String::from_utf8_lossy(run.snc.diagnostics()));
    }
    out
}

/// Preprocess `st` as the EPICS build does, from a fresh `O.*` directory
/// next to (symlinks of) the source directory's entries, so `../x` includes
/// resolve as there. Returns the scratch dir, the `.i` file's directory
/// and its name. Unix only, as are the gcc-comparing tests that call it.
#[cfg(unix)]
pub fn preprocess(st: &Path) -> (tempfile::TempDir, PathBuf, String) {
    use epics_base_rs::reference::{ReferenceTree, reference_root};
    use std::process::Command;

    let base = reference_root(ReferenceTree::Base);
    let seq = reference_root(ReferenceTree::Modules).join("sequencer");
    let src = st.parent().expect("source dir");
    let scratch = tempfile::tempdir().expect("scratch dir");
    for entry in std::fs::read_dir(src).expect("read source dir") {
        let name = entry.expect("source entry").file_name();
        if !name.to_string_lossy().starts_with("._") {
            std::os::unix::fs::symlink(src.join(&name), scratch.path().join(&name))
                .expect("link source entry");
        }
    }
    let odir = scratch.path().join("O.snc-rs");
    std::fs::create_dir(&odir).expect("create O dir");
    let stem = st.file_stem().unwrap().to_string_lossy().into_owned();
    let i_name = format!("{stem}.i");
    let out = Command::new("gcc")
        .args(["-x", "c", "-E"])
        .args(["-D_GNU_SOURCE", "-D_DEFAULT_SOURCE", "-DUSE_TYPED_RSET"])
        .args(["-D_X86_64_", "-DUNIX", "-Dlinux"])
        .args(["-I.", "-I..", "-I../O.Common"])
        .arg(format!("-I{}", seq.join("include").display()))
        .arg(format!("-I{}", base.join("include/compiler/gcc").display()))
        .arg(format!("-I{}", base.join("include/os/Linux").display()))
        .arg(format!("-I{}", base.join("include").display()))
        .arg(format!("../{}", st.file_name().unwrap().to_string_lossy()))
        .current_dir(&odir)
        .output()
        .expect("run gcc -E");
    assert!(
        out.status.success(),
        "preprocessing {} failed:\n{}",
        st.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(odir.join(&i_name), out.stdout).expect("write .i");
    (scratch, odir, i_name)
}
