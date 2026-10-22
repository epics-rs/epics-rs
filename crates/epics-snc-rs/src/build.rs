//! The build-script entry: what the EPICS build's rules do to a module's
//! programs (`%.i: %.st` runs cpp, `%.c: %.i` and `%.c: %.stt` run snc),
//! with the generated code going to `$OUT_DIR/<stem>.rs`.
//!
//! ```no_run
//! // build.rs
//! epics_snc_rs::build::compile_dir("src");
//! ```
//!
//! Programs compile with snc's default options; `option` lines inside a
//! program apply as in C. A failing program fails the build with snc's
//! diagnostics on stderr. Warnings of a program that compiles become cargo
//! warnings.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use epics_libcom_rs::runtime::log::StagedStderr;

use crate::cpp;
use crate::driver::{EXIT_FAILURE, Exit, Snc};

/// Compile every `*.st` directly in `dir`, in file-name order, into
/// `$OUT_DIR/<stem>.rs`, and tell cargo to rerun on a change to the
/// directory, to a program, or to a file one includes. Returns the files
/// written.
pub fn compile_dir(dir: impl AsRef<Path>) -> Vec<PathBuf> {
    finish(compile_dir_into(
        dir.as_ref(),
        &out_dir(),
        &mut io::stdout().lock(),
    ))
}

/// Compile one `.st` (or `.stt`, which skips cpp as in the EPICS build)
/// into `$OUT_DIR/<stem>.rs`, telling cargo to rerun on a change to it or
/// to a file it includes. Returns the file written.
pub fn compile(st: impl AsRef<Path>) -> PathBuf {
    finish(compile_into(
        st.as_ref(),
        &out_dir(),
        &mut io::stdout().lock(),
    ))
}

fn out_dir() -> PathBuf {
    std::env::var_os("OUT_DIR")
        .expect("OUT_DIR is unset: epics_snc_rs::build runs from a build script")
        .into()
}

/// A program snc refused.
#[derive(Debug)]
struct Failure {
    st: PathBuf,
    diagnostics: Vec<u8>,
}

fn finish<T>(result: Result<T, Failure>) -> T {
    match result {
        Ok(written) => written,
        Err(failure) => {
            // cargo shows a failed build script's stderr; this is snc's.
            let _ = StagedStderr.write_all(&failure.diagnostics);
            panic!("snc-rs: {} did not compile", failure.st.display());
        }
    }
}

fn compile_dir_into(
    dir: &Path,
    out_dir: &Path,
    cargo: &mut dyn Write,
) -> Result<Vec<PathBuf>, Failure> {
    rerun_if_changed(cargo, dir);
    let mut programs: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("snc-rs: reading {}: {e}", dir.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|e| panic!("snc-rs: reading {}: {e}", dir.display()))
                .path()
        })
        .filter(|path| path.extension() == Some(OsStr::new("st")) && path.is_file())
        .collect();
    programs.sort();
    programs
        .iter()
        .map(|st| compile_into(st, out_dir, cargo))
        .collect()
}

fn compile_into(st: &Path, out_dir: &Path, cargo: &mut dyn Write) -> Result<PathBuf, Failure> {
    let mut snc = Snc::capturing();
    // appended, not with_extension: x.1.st and x.2.st must not both be x.rs
    let mut name = st.file_stem().unwrap_or_default().to_os_string();
    name.push(".rs");
    let output = out_dir.join(name);
    let status = translate_st(&mut snc, st, &output, cargo).unwrap_or_else(|Exit(s)| s);
    if status != 0 {
        return Err(Failure {
            st: st.to_path_buf(),
            diagnostics: snc.diagnostics().to_vec(),
        });
    }
    for line in String::from_utf8_lossy(snc.diagnostics()).lines() {
        writeln!(cargo, "cargo::warning={line}").expect("write to cargo");
    }
    Ok(output)
}

/// cpp, then snc from `parse_program` on, as `make` runs the two.
fn translate_st(
    snc: &mut Snc,
    st: &Path,
    output: &Path,
    cargo: &mut dyn Write,
) -> Result<i32, Exit> {
    let name = vpath_name(st);
    let input = if st.extension() == Some(OsStr::new("stt")) {
        rerun_if_changed(cargo, st);
        snc.read_input(st)?
    } else {
        let pre = preprocess(snc, st, &name)?;
        for file in &pre.files {
            rerun_if_changed(cargo, file);
        }
        pre.text
    };
    snc.translate(&input, &name.to_string_lossy(), output)
}

/// The name make hands cpp and snc: RULES_SNCSEQ runs both in O.<arch>
/// on vpath's `../<file>`, so `__FILE__`, line markers and diagnostics do
/// not depend on the checkout.
fn vpath_name(st: &Path) -> PathBuf {
    let mut name = OsString::from("../");
    name.push(st.file_name().unwrap_or_default());
    name.into()
}

/// A program after cpp.
struct Preprocessed {
    text: Vec<u8>,
    /// Every file cpp read, the program first.
    files: Vec<PathBuf>,
}

/// cpp, with its diagnostics in snc's stream; an error stops the program
/// before snc as a failing `%.i: %.st` stops make.
fn preprocess(snc: &mut Snc, st: &Path, name: &Path) -> Result<Preprocessed, Exit> {
    let text = snc.read_input(st)?;
    // -I.. : the program's own directory, from O.<arch>.
    let out = cpp::preprocess(st, name, &text, &[], &["..".into()]);
    for d in &out.diagnostics {
        snc.report(format_args!("{d}\n"));
    }
    if out.failed() {
        return Err(Exit(EXIT_FAILURE));
    }
    Ok(Preprocessed {
        text: out.text,
        files: out.files,
    })
}

fn rerun_if_changed(cargo: &mut dyn Write, path: &Path) {
    writeln!(cargo, "cargo::rerun-if-changed={}", path.display()).expect("write to cargo");
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = "program p ss s { state a { when () {} exit } }\n";

    fn cargo_lines(cargo: &[u8]) -> Vec<String> {
        String::from_utf8(cargo.to_vec())
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn compile_dir_takes_each_st_in_name_order() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        for name in ["b.st", "a.st", "c.stt", "notes.txt"] {
            std::fs::write(src.path().join(name), OK).unwrap();
        }
        std::fs::create_dir(src.path().join("sub.st")).unwrap();

        let mut cargo = Vec::new();
        let written = compile_dir_into(src.path(), out.path(), &mut cargo).unwrap();

        assert_eq!(written, [out.path().join("a.rs"), out.path().join("b.rs")]);
        assert!(written.iter().all(|w| w.is_file()));
        let rerun = |p: &Path| format!("cargo::rerun-if-changed={}", p.display());
        assert_eq!(
            cargo_lines(&cargo),
            [
                rerun(src.path()),
                rerun(&src.path().join("a.st")),
                rerun(&src.path().join("b.st")),
            ]
        );
    }

    #[test]
    fn dotted_stems_keep_their_own_output() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        for name in ["x.1.st", "x.2.st"] {
            std::fs::write(src.path().join(name), OK).unwrap();
        }

        let written = compile_dir_into(src.path(), out.path(), &mut Vec::new()).unwrap();

        assert_eq!(
            written,
            [out.path().join("x.1.rs"), out.path().join("x.2.rs")]
        );
        assert!(written.iter().all(|w| w.is_file()));
    }

    #[test]
    fn failure_carries_snc_diagnostics_and_writes_nothing() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let st = src.path().join("bad.st");
        std::fs::write(&st, "program p ss s { state { } }\n").unwrap();

        let failure = compile_into(&st, out.path(), &mut Vec::new()).unwrap_err();

        assert_eq!(failure.st, st);
        assert_eq!(
            String::from_utf8(failure.diagnostics).unwrap(),
            "../bad.st:1: syntax error near token '{'\n../bad.st:1: parser giving up\n"
        );
        assert!(!out.path().join("bad.rs").exists());
    }

    #[test]
    fn missing_program_fails_with_main_c_message() {
        let out = tempfile::tempdir().unwrap();
        let st = out.path().join("gone.st");
        let failure = compile_into(&st, out.path(), &mut Vec::new()).unwrap_err();
        assert_eq!(
            String::from_utf8(failure.diagnostics).unwrap(),
            format!(
                "error opening input file: {}: No such file or directory\n",
                st.display()
            )
        );
    }

    #[test]
    fn warnings_of_a_compiled_program_become_cargo_warnings() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        for name in ["w.st", "w.stt"] {
            let st = src.path().join(name);
            std::fs::write(
                &st,
                "program p\nint const const cc_i;\nss s { state a { when () {} exit } }\n",
            )
            .unwrap();
            let mut cargo = Vec::new();
            compile_into(&st, out.path(), &mut cargo).unwrap();
            assert_eq!(
                cargo_lines(&cargo)[1..],
                [format!(
                    "cargo::warning=../{name}:2: warning: discarding repeated const"
                )]
            );
        }
    }

    #[test]
    #[should_panic(expected = "snc-rs: x.st did not compile")]
    fn a_failure_fails_the_build_script() {
        finish::<()>(Err(Failure {
            st: PathBuf::from("x.st"),
            diagnostics: b"x.st:1: parser giving up\n".to_vec(),
        }));
    }

    #[test]
    fn st_goes_through_cpp_and_reruns_on_its_includes() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let st = src.path().join("p.st");
        let h = src.path().join("defs.h");
        std::fs::write(&h, "#define BODY { state a { when () {} exit } }\n").unwrap();
        std::fs::write(&st, "#include \"defs.h\"\nprogram p ss s BODY\n").unwrap();
        let mut cargo = Vec::new();
        assert_eq!(
            compile_into(&st, out.path(), &mut cargo).unwrap(),
            out.path().join("p.rs")
        );
        let rerun = |p: &Path| format!("cargo::rerun-if-changed={}", p.display());
        assert_eq!(cargo_lines(&cargo), [rerun(&st), rerun(&h)]);
    }

    #[test]
    fn cpp_spells_names_as_rules_sncseq_from_any_checkout() {
        // RULES_SNCSEQ runs cpp in O.<arch> on ../p.st with -I..; a
        // header in the directory above the program's is not one of its.
        let program = "program p\n#include \"h.h\"\n#include <g.h>\n\
                       ss s { state a { when () { printf(\"%s\\n\", __FILE__); } exit } }\n";
        let checkout = |top: &Path| {
            let src = top.join("src");
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(src.join("p.st"), program).unwrap();
            std::fs::write(src.join("h.h"), "%% static const char *hfile = __FILE__;\n").unwrap();
            std::fs::write(src.join("g.h"), "%% static const char *gfile = __FILE__;\n").unwrap();
            std::fs::write(top.join("h.h"), "%% decoy\n").unwrap();
            std::fs::write(top.join("g.h"), "%% decoy\n").unwrap();
            let st = src.join("p.st");
            let pre = preprocess(&mut Snc::capturing(), &st, &vpath_name(&st)).unwrap();
            let out = tempfile::tempdir().unwrap();
            let rs = compile_into(&st, out.path(), &mut Vec::new()).unwrap();
            (pre.text, std::fs::read(rs).unwrap())
        };
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (checkout(a.path()), checkout(&b.path().join("x")));
        assert_eq!(
            String::from_utf8(a.0.clone()).unwrap(),
            "# 1 \"../p.st\"\nprogram p\n\
             # 1 \"../h.h\" 1\n%% static const char *hfile = \"../h.h\";\n# 3 \"../p.st\" 2\n\
             # 1 \"../g.h\" 1\n%% static const char *gfile = \"../g.h\";\n# 4 \"../p.st\" 2\n\
             ss s { state a { when () { printf(\"%s\\n\", \"../p.st\"); } exit } }\n"
        );
        assert_eq!(a, b);
    }

    #[test]
    fn a_cpp_error_stops_the_program_before_snc() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let st = src.path().join("e.st");
        std::fs::write(&st, format!("#warning w\n#error e\n{OK}")).unwrap();
        let failure = compile_into(&st, out.path(), &mut Vec::new()).unwrap_err();
        assert_eq!(
            String::from_utf8(failure.diagnostics).unwrap(),
            "../e.st:1:2: warning: #warning w [-Wcpp]\n../e.st:2:2: error: #error e\n"
        );
        assert!(!out.path().join("e.rs").exists());
    }

    #[test]
    fn stt_skips_cpp() {
        let src = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let stt = src.path().join("tmpl.stt");
        std::fs::write(&stt, OK).unwrap();
        let mut cargo = Vec::new();
        assert_eq!(
            compile_into(&stt, out.path(), &mut cargo).unwrap(),
            out.path().join("tmpl.rs")
        );
        assert_eq!(
            cargo_lines(&cargo),
            [format!("cargo::rerun-if-changed={}", stt.display())]
        );
    }
}
