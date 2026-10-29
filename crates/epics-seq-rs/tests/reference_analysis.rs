//! The port's diagnostics up to the end of analysis against the C snc built
//! in the sequencer checkout: the crafted fixtures, the sequencer's own 96
//! example and test programs, and the 30 programs of the module collection.
//! Every program runs under each option set of `common::OPTION_SETS`; the
//! upstream ones are preprocessed as the EPICS build does.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use epics_base_rs::reference::{ReferenceTree, reference_path, reference_root};

/// Programs on which C snc aborts, where the port reports and goes on (C
/// bugs are not copied). C asserts a named parameter (`noname`) and a
/// function type behind every function definition (`notfuncarr` reads its
/// parameters out of an array type), asserts `sync != M_SINGLE` on entry
/// to `sync_var`/`sync_elem` though `analyse_syncq` never checks it, and
/// recurses forever in `fixup_struct_refs` (`selfref3`).
const C_ABORTS: &[&str] = &[
    "noname.st",
    "notfuncarr.st",
    "selfref3.st",
    "syncq_after_sync.st",
    "syncq_after_sync_array.st",
    "syncq_elem_after_sync.st",
];

/// Programs whose diagnostics C snc's code generation continues.
const CODEGEN_DIAGNOSTICS: &[&str] = &[
    "delay_in_action.st",
    "efNoInit.st",
    "funcdefShadowGlobal.st",
    "pvArray.st",
    "pvNotAssigned.st",
    "typeerr.st",
    "undecl.st",
];

/// The 30 SNL programs of the module collection.
const MODULE_PROGRAMS: &[&str] = &[
    "asyn/testIPServerApp/src/ipSNCServer.st",
    "calc/calcApp/src/editSseq.st",
    "ip/ipApp/src/Federal.st",
    "ip/ipApp/src/Keithley2kDMM_mf40.st",
    "ip/ipApp/src/Keithley2kDMM.st",
    "ip/ipApp/src/Keithley65xxEM.st",
    "ip/ipApp/src/Oxford_CS800.st",
    "mca/mcaApp/SISSrc/SIS38XX_SNL.st",
    "measComp/measCompApp/src/USBCTR_SNL.st",
    "motor/.ci/exampleApp/src/sncProgram.st",
    "motor/modules/motorAerotech/aerotechApp/src/EnsembleTrajectoryScan.st",
    "motor/modules/motorNewport/newportApp/src/MM4005_trajectoryScan.st",
    "motor/modules/motorNewport/newportApp/src/xpsSlave.st",
    "motor/modules/motorNewport/newportApp/src/XPS_trajectoryScan.st",
    "motor/modules/motorOms/omsApp/src/MAX_trajectoryScan.st",
    "optics/opticsApp/src/filterDrive.st",
    "optics/opticsApp/src/flexCombinedMotion.st",
    "optics/opticsApp/src/hrCtl.st",
    "optics/opticsApp/src/Io.st",
    "optics/opticsApp/src/kohzuCtl_soft.st",
    "optics/opticsApp/src/kohzuCtl.st",
    "optics/opticsApp/src/ml_monoCtl.st",
    "optics/opticsApp/src/orient_st.st",
    "optics/opticsApp/src/pf4.st",
    "optics/opticsApp/src/sncqxbpm.st",
    "optics/opticsApp/src/xiahsc.st",
    "optics/opticsApp/src/xia_slit.st",
    "sscan/sscanApp/src/scanProg.st",
    "std/stdApp/src/delayDo.st",
    "std/stdApp/src/femto.st",
];

fn c_snc() -> PathBuf {
    reference_path(ReferenceTree::Modules, "sequencer/bin/linux-x86_64/snc")
}

fn run_c_snc(dir: &Path, input: &str, opts: &[&str], out: &Path) -> Output {
    Command::new(c_snc())
        .args(opts)
        .arg(input)
        .arg("-o")
        .arg(out)
        .current_dir(dir)
        .output()
        .expect("run C snc")
}

/// Compare one program under one option set; `Err` describes a mismatch.
fn check(dir: &Path, input: &str, program: &str, opts: &[&str]) -> Result<(), String> {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let c = run_c_snc(dir, input, opts, &scratch.path().join("out.c"));
    let source = std::fs::read(dir.join(input)).expect("read input");
    let run = common::analyse(&source, input, opts);
    let got = run.snc.diagnostics();
    let want = &c.stderr[..];
    let show = |s: &[u8]| String::from_utf8_lossy(s).into_owned();
    let mismatch = |why: &str| {
        Err(format!(
            "{program} {opts:?}: {why}\n--- port\n{}--- C snc ({})\n{}",
            show(got),
            c.status,
            show(want)
        ))
    };

    if C_ABORTS.contains(&program) {
        // up to the abort, C agrees with the port
        let before_abort = match want.strip_suffix(b"\n").and_then(|w| {
            let start = w.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            w[start..].starts_with(b"snc: ").then_some(&want[..start])
        }) {
            Some(before) => before,
            None if c.status.code().is_none() => want,
            None => return mismatch("C snc no longer aborts"),
        };
        if c.status.success() || !got.starts_with(before_abort) {
            return mismatch("differs before C snc's abort");
        }
        return Ok(());
    }
    let analysed_clean = run.prg.is_some() && run.snc.err_cnt() == 0;
    if analysed_clean && CODEGEN_DIAGNOSTICS.contains(&program) {
        if want.len() > got.len() && want.starts_with(got) {
            return Ok(());
        }
        return mismatch("not followed by code generation's diagnostics");
    }
    if got != want {
        return mismatch("diagnostics differ");
    }
    if analysed_clean != c.status.success() {
        return mismatch("exit status differs");
    }
    Ok(())
}

fn check_all(dir: &Path, input: &str, program: &str, failed: &mut Vec<String>) {
    for opts in common::OPTION_SETS {
        if let Err(e) = check(dir, input, program, opts) {
            failed.push(e);
        }
    }
}

fn check_upstream(st: &Path, failed: &mut Vec<String>) {
    let program = st.file_name().unwrap().to_string_lossy().into_owned();
    let (_scratch, odir, i_name) = common::preprocess(st);
    check_all(&odir, &i_name, &program, failed);
}

#[test]
fn fixtures_match_c_snc() {
    let dir = common::fixture_dir();
    let mut failed = Vec::new();
    for name in common::fixtures() {
        check_all(&dir, &name, &name, &mut failed);
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

fn sequencer_programs(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read sequencer dir") {
        let path = entry.expect("sequencer entry").path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name.starts_with("._") || name.starts_with("O.") {
            continue;
        }
        if path.is_dir() {
            sequencer_programs(&path, found);
        } else if name.ends_with(".st") {
            found.push(path);
        }
    }
}

#[test]
fn sequencer_programs_match_c_snc() {
    let seq = reference_root(ReferenceTree::Modules).join("sequencer");
    let mut programs = Vec::new();
    for dir in ["examples", "test"] {
        sequencer_programs(&seq.join(dir), &mut programs);
    }
    programs.sort();
    assert_eq!(programs.len(), 96, "sequencer R2-2-9 has 96 .st programs");
    let mut failed = Vec::new();
    for st in &programs {
        check_upstream(st, &mut failed);
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn module_programs_match_c_snc() {
    let mut failed = Vec::new();
    for rel in MODULE_PROGRAMS {
        check_upstream(&reference_path(ReferenceTree::Modules, rel), &mut failed);
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
