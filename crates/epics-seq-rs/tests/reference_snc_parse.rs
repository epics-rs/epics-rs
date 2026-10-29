//! Every upstream SNL program, and a set of malformed ones, through C snc's
//! front end and through `parse_program`: the syntax trees, the diagnostics
//! and the exit status must agree.
//!
//! The C side is snc_dump (tests/snc_dump), linked here from the objects of
//! the sequencer build. Both sides print the tree in snc_dump's format.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use epics_base_rs::reference::{ReferenceTree, reference_path, reference_root};
use epics_seq_rs::snc::driver::{Exit, Snc};
use epics_seq_rs::snc::parser::parse_program;
use epics_seq_rs::snc::types::{Ast, Extra, NodeId, NodeTag};
use epics_seq_rs::snc::var_types::TypeKind;

/// The .st and .stt programs of the module checkouts, relative to the
/// modules tree.
const CORPUS: &[&str] = &[
    "calc/calcApp/src/editSseq.st",
    "sequencer/examples/simple/sncExEntry.st",
    "sequencer/examples/simple/sncExOpt.st",
    "sequencer/examples/simple/sncExample.st",
    "sequencer/examples/small/pvTimeStamp.st",
    "sequencer/examples/small/timer.st",
    "sequencer/examples/small/optt.st",
    "sequencer/examples/small/version.st",
    "sequencer/examples/small/syncq.st",
    "sequencer/examples/small/decl.st",
    "sequencer/examples/small/parameter.st",
    "sequencer/examples/small/delay.st",
    "sequencer/examples/small/entry.st",
    "sequencer/examples/small/entryVar.st",
    "sequencer/examples/johng/johng.st",
    "asyn/makeSupport/app/_NAME_App/src/sncExample.stt",
    "sequencer/examples/cmdButtons/cmdButtons.st",
    "sequencer/examples/parallel/control.st",
    "sequencer/examples/traffic/traffic.st",
    "sequencer/examples/demo/demo.st",
    "sequencer/test/compiler/member.st",
    "sequencer/test/compiler/exOpt_DuplOpt.st",
    "sequencer/test/compiler/sync_not_monitored.st",
    "sequencer/test/compiler/state_not_reachable.st",
    "sequencer/test/compiler/funcdefShadowGlobal.st",
    "sequencer/test/compiler/include_windows_h.st",
    "sequencer/test/compiler/foreignTypes.st",
    "sequencer/test/compiler/sync_not_assigned.st",
    "sequencer/test/compiler/syncq_no_size.st",
    "sequencer/test/compiler/syncq_not_assigned.st",
    "sequencer/test/compiler/varinit.st",
    "sequencer/test/compiler/pvArray.st",
    "sequencer/test/compiler/delay_in_action.st",
    "sequencer/test/compiler/syncq_size_out_of_range.st",
    "sequencer/test/compiler/varinitOptr.st",
    "sequencer/test/compiler/type_expr.st",
    "sequencer/test/compiler/namingConflict.st",
    "sequencer/test/compiler/cast.st",
    "sequencer/test/compiler/tooLong.st",
    "sequencer/test/compiler/change.st",
    "sequencer/test/compiler/scope.st",
    "sequencer/test/compiler/exOpt_UnrecOpt.st",
    "sequencer/test/compiler/efArray.st",
    "sequencer/test/compiler/foreignGlobal.st",
    "sequencer/test/compiler/efNoInit.st",
    "sequencer/test/compiler/cppComment.st",
    "sequencer/test/compiler/simple.st",
    "sequencer/test/compiler/type_not_allowed.st",
    "sequencer/test/compiler/nesting_depth.st",
    "sequencer/test/compiler/efPointer.st",
    "sequencer/test/compiler/syncq_not_monitored.st",
    "sequencer/test/compiler/funcdef.st",
    "sequencer/test/compiler/foreignNoInit.st",
    "sequencer/test/compiler/misplacedExit.st",
    "sequencer/test/compiler/pvNotAssigned.st",
    "sequencer/test/compiler/reservedId.st",
    "sequencer/test/compiler/subscript.st",
    "sequencer/test/compiler/efGlobal.st",
    "motor/modules/motorOms/omsApp/src/MAX_trajectoryScan.st",
    "sscan/sscanApp/src/scanProg.st",
    "sequencer/test/validate/skeleton.st",
    "sequencer/test/validate/pvSync.st",
    "sequencer/test/validate/entryOpte.st",
    "sequencer/test/validate/race.st",
    "sequencer/test/validate/local.st",
    "sequencer/test/validate/void.st",
    "sequencer/test/validate/foreignTypes.st",
    "sequencer/test/validate/userfuncEf.st",
    "sequencer/test/validate/stop.st",
    "sequencer/test/validate/evflag.st",
    "sequencer/test/validate/pvGetComplete.st",
    "sequencer/test/validate/reassign.st",
    "sequencer/test/validate/functionInStruct.st",
    "sequencer/test/validate/pvAssignStress.st",
    "sequencer/test/validate/indirectCall.st",
    "sequencer/test/validate/opttVar.st",
    "sequencer/test/validate/norace.st",
    "sequencer/test/validate/pvGetAsync.st",
    "sequencer/test/validate/change.st",
    "sequencer/test/validate/evflagExt.st",
    "sequencer/test/validate/structdef.st",
    "sequencer/test/validate/pvAssignSubst.st",
    "sequencer/test/validate/exitOptx.st",
    "sequencer/test/validate/commaOperator.st",
    "sequencer/test/validate/pvGetCancel.st",
    "sequencer/test/validate/pvSyncDb.st",
    "sequencer/test/validate/sizeof.st",
    "sequencer/test/validate/array.st",
    "sequencer/test/validate/pvGetSync.st",
    "sequencer/test/validate/assign.st",
    "sequencer/test/validate/pvPutAndMonitor.st",
    "sequencer/test/validate/userfunc.st",
    "sequencer/test/validate/safeModeNotAssigned.st",
    "sequencer/test/validate/raceCommon.st",
    "sequencer/test/validate/safeMonitor.st",
    "sequencer/test/validate/monitorEvflag.st",
    "sequencer/test/validate/bittypes.st",
    "sequencer/test/validate/pvPutAsync.st",
    "sequencer/test/validate/pvGet.st",
    "sequencer/test/validate/pvSyncNoDb.st",
    "std/stdApp/src/delayDo.st",
    "std/stdApp/src/femto.st",
    "motor/modules/motorNewport/newportApp/src/xpsSlave.st",
    "motor/modules/motorNewport/newportApp/src/XPS_trajectoryScan.st",
    "motor/modules/motorNewport/newportApp/src/MM4005_trajectoryScan.st",
    "asyn/testIPServerApp/src/ipSNCServer.st",
    "ip/ipApp/src/Oxford_CS800.st",
    "ip/ipApp/src/Keithley65xxEM.st",
    "ip/ipApp/src/Keithley2kDMM_mf40.st",
    "ip/ipApp/src/Keithley2kDMM.st",
    "ip/ipApp/src/Federal.st",
    "mca/mcaApp/SISSrc/SIS38XX_SNL.st",
    "motor/modules/motorAerotech/aerotechApp/src/EnsembleTrajectoryScan.st",
    "optics/opticsApp/src/kohzuCtl.st",
    "optics/opticsApp/src/filterDrive.st",
    "optics/opticsApp/src/xiahsc.st",
    "optics/opticsApp/src/flexCombinedMotion.st",
    "optics/opticsApp/src/Io.st",
    "optics/opticsApp/src/orient_st.st",
    "optics/opticsApp/src/pf4.st",
    "optics/opticsApp/src/sncqxbpm.st",
    "optics/opticsApp/src/hrCtl.st",
    "optics/opticsApp/src/ml_monoCtl.st",
    "optics/opticsApp/src/xia_slit.st",
    "optics/opticsApp/src/kohzuCtl_soft.st",
    "measComp/measCompApp/src/USBCTR_SNL.st",
];

/// Corpus programs C snc rejects: upstream's own failure tests (errors
/// expected in sequencer/test/compiler/snc_test.plt) and fragments that
/// only compile inside the .st that includes them.
const C_REJECTS: &[&str] = &[
    "sequencer/test/compiler/efArray.st",
    "sequencer/test/compiler/efPointer.st",
    "sequencer/test/compiler/foreignNoInit.st",
    "sequencer/test/compiler/misplacedExit.st",
    "sequencer/test/compiler/reservedId.st",
    "sequencer/test/compiler/type_not_allowed.st",
    "sequencer/test/compiler/simple.st",
    "sequencer/test/validate/pvSync.st",
    "sequencer/test/validate/raceCommon.st",
];

/// Inputs C snc reports errors on. With the corpus's failure tests they
/// draw every reachable message of snl.re, snl.lem and var_types.c.
const MALFORMED: &[(&str, &str)] = &[
    ("empty", ""),
    (
        "bad_char",
        "program p ss s { state a { when () { x = 1 @ 2; } exit } }\n",
    ),
    ("cr", "program p\r\nss s { state a { when () {} exit } }\n"),
    ("unterminated_string", "program p int x = \"abc\nss s {}\n"),
    (
        "string_newline_escape",
        "program p string s = \"a\\\nb\";\n",
    ),
    ("unterminated_comment", "program p /* no end\n"),
    ("unterminated_ccode", "program p %{ int x;\n"),
    (
        "reserved_prefix",
        "program p int seqg_x1; ss s { state a { when () {} exit } }\n",
    ),
    (
        "missing_semicolon",
        "program p int x ss s { state a { when () {} exit } }\n",
    ),
    (
        "missing_subscript_semicolon",
        "program p int x[2] ss s { state a { when () {} exit } }\n",
    ),
    (
        "missing_state_name",
        "program p ss s { state { when () {} exit } }\n",
    ),
    (
        "two_errors",
        "program p int x int y; ss s { state a { when (x ==) {} exit } }\n",
    ),
    (
        "bad_array_size",
        "program p int x[99999999999]; ss s { state a { when () {} exit } }\n",
    ),
    (
        "zero_array_size",
        "program p int x[0]; ss s { state a { when () {} exit } }\n",
    ),
    (
        "line_marker",
        "# 10 \"other.st\"\nprogram p\nss s { state a { when () {} exit } } junk\n",
    ),
    (
        "line_marker_flags",
        "# 3 \"x.st\" 1 3\nprogram p ss s { state a { when () { y = ; } exit } }\n",
    ),
    (
        "empty_line_marker",
        "# \nprogram p ss s { state a { when () {} exit } } }\n",
    ),
    ("eof_in_state", "program p ss s { state a { when () {}\n"),
    ("eof_after_program", "program\n"),
    ("monitor_junk", "program p monitor x junk"),
    (
        "stray_ccode",
        "program p ss s { state a { when () {} exit } } %% int x;\nint y;\n",
    ),
    (
        "assign_to",
        "program p int x; assign x to ; ss s { state a { when () {} exit } }\n",
    ),
    (
        "bad_funcdef",
        "program p ss s { state a { when () {} exit } } int f(int) { return 1; } int g(\n",
    ),
    (
        "bad_struct",
        "program p struct s { int a; int } ; ss s { state a { when () {} exit } }\n",
    ),
    (
        "bad_expr",
        "program p ss s { state a { when (a ? b) {} exit } }\n",
    ),
    (
        "bad_for",
        "program p ss s { state a { when () { for (;;;) {} } exit } }\n",
    ),
    (
        "bad_if_else",
        "program p ss s { state a { when () { if (1) {} else else {} } exit } }\n",
    ),
    (
        "bad_cast",
        "program p ss s { state a { when () { x = (int *[) y; } exit } }\n",
    ),
    (
        "bad_entry",
        "program p entry { x = } ss s { state a { when () {} exit } }\n",
    ),
    (
        "bad_sync",
        "program p evflag f; sync x to ; ss s { state a { when () {} exit } }\n",
    ),
    (
        "bad_number",
        "program p int x = 08; double y = 1e; ss s { state a { when () {} exit } }\n",
    ),
    (
        "monitor_recovery",
        "program p int x; monitor x junk;\nss s { state a { when () {} exit } }\n",
    ),
    (
        "monitor_subscript_recovery",
        "program p int x[2]; monitor x[1] junk;\nss s { state a { when () {} exit } }\n",
    ),
    (
        "sync_recovery",
        "program p int x; evflag f; sync x to f junk;\nss s { state a { when () {} exit } }\n",
    ),
    (
        "transition_recovery",
        "program p ss s { state a { when () {} junk }\nstate b { when () {} exit } }\n",
    ),
];

/// Inputs the front end accepts: grammar rules the corpus never reduces,
/// and values only analysis refuses, which the tree carries as C does.
/// With the corpus and [`MALFORMED`] they reduce every rule of snl.lem.
const ACCEPTED: &[(&str, &str)] = &[
    (
        "option_unknown",
        "program p option +q; ss s { state a { when () {} exit } }\n",
    ),
    (
        "state_option_unknown",
        "program p ss s { state a { option -q; when () {} exit } }\n",
    ),
    (
        "struct_after_ss",
        "program p ss s { state a { when () {} exit } }\nstruct pt { int x; int y[2]; };\n",
    ),
    (
        "subscripted_monitor_sync",
        "program p int x[2]; evflag f; monitor x[1]; sync x[0] to f;\nss s { state a { when () {} exit } }\n",
    ),
    (
        "brace_initializers",
        "program p struct pt v = (struct pt){1, {2}}; int w[2] = {};\nss s { state a { when () {} exit } }\n",
    ),
    (
        "abstract_declarators",
        "program p ss s { state a { when () {\nx = sizeof(int[3]) + sizeof(int (int, char *)) + sizeof(int (*)(void));\n} exit } }\n",
    ),
    (
        "operators",
        "program p ss s { state a { when () {\nx = ~a + --b; x = a >> 1 ^ b; x &= 1; x <<= 2; x >>= 3; x ^= 4;\n} exit } }\n",
    ),
    (
        "syncq_zero",
        "program p syncq x 0 ; ss s { state a { when () {} exit } }\n",
    ),
    (
        "number_overflow",
        "program p int x = 0xffffffffffffffffff; ss s { state a { when (delay(1e999)) {} exit } }\n",
    ),
];

/// snc_dump, built once per test process from the sequencer build's
/// objects. Each process builds in a directory of its own and renames the
/// binary into place, so no process runs a half-written one.
fn snc_dump() -> PathBuf {
    static BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        let seq = reference_root(ReferenceTree::Modules).join("sequencer");
        let base = reference_root(ReferenceTree::Base);
        let obj = seq.join("src/snc/O.linux-x86_64");
        let dir = scratch_dir("snc_dump");
        let main_o = dir.join("main.o");
        run_ok(
            Command::new("objcopy")
                .arg("--redefine-sym")
                .arg("main=snc_main")
                .arg(obj.join("main.o"))
                .arg(&main_o),
        );
        let bin = dir.join("snc_dump");
        let mut cc = Command::new("gcc");
        cc.args(["-O0", "-g", "-Wall", "-Werror"]);
        for inc in [
            seq.join("src/snc"),
            seq.join("src/common"),
            obj.clone(),
            seq.join("include"),
            base.join("include"),
            base.join("include/os/Linux"),
            base.join("include/compiler/gcc"),
        ] {
            cc.arg("-I").arg(inc);
        }
        cc.arg("-o")
            .arg(&bin)
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/snc_dump/snc_dump.c"
            ))
            .arg(&main_o);
        for o in ["lexer.o", "snl.o", "node.o", "var_types.o"] {
            cc.arg(obj.join(o));
        }
        run_ok(&mut cc);
        let installed = dir.with_file_name("snc_dump");
        std::fs::rename(&bin, &installed).expect("install snc_dump");
        std::fs::remove_dir_all(&dir).expect("remove snc_dump build dir");
        installed
    })
    .clone()
}

fn run_ok(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// What one front end made of one input.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// `None`: C crashed (it does when the parse ends without accepting).
    status: Option<i32>,
    tree: String,
    diagnostics: String,
}

fn c_outcome(input: &Path) -> Outcome {
    let out = Command::new(snc_dump())
        .arg("-o")
        .arg("/dev/null")
        .arg(input)
        .output()
        .expect("run snc_dump");
    Outcome {
        status: out.status.code(),
        tree: String::from_utf8(out.stdout).expect("utf-8 dump"),
        diagnostics: String::from_utf8(out.stderr).expect("utf-8 diagnostics"),
    }
}

fn rs_outcome(input: &Path) -> Outcome {
    let bytes = std::fs::read(input).expect("read input");
    let name = input.to_str().expect("utf-8 path");
    let mut snc = Snc::capturing();
    let mut ast = Ast::new();
    let (status, tree) = match parse_program(&mut snc, &mut ast, &bytes, name) {
        Ok(Some(root)) => {
            let mut tree = String::new();
            dump_list(&ast, Some(root), 0, &mut tree);
            (Some(i32::from(snc.err_cnt() > 0)), tree)
        }
        Ok(None) => (None, String::new()),
        Err(Exit(status)) => (Some(status), String::new()),
    };
    Outcome {
        status,
        tree,
        diagnostics: String::from_utf8(snc.diagnostics().to_vec()).expect("utf-8 diagnostics"),
    }
}

fn nz(s: &Option<Arc<str>>) -> &str {
    s.as_deref().unwrap_or("(null)")
}

/// snc_dump's `show_type`; function parameter lists go to `pending`.
fn show_type(
    ast: &Ast,
    t: epics_seq_rs::snc::types::TypeId,
    out: &mut String,
    pending: &mut Vec<Option<NodeId>>,
) {
    let t = &ast[t];
    if t.is_const {
        out.push_str("const ");
    }
    match &t.kind {
        TypeKind::None => out.push_str("none"),
        TypeKind::EvFlag => out.push_str("evflag"),
        TypeKind::Void => out.push_str("void"),
        TypeKind::Prim(p) => out.push_str(&format!("prim:{}", *p as i32)),
        TypeKind::Foreign { tag, name } => out.push_str(&format!("foreign:{}:{name}", *tag as i32)),
        TypeKind::Pointer { value_type } => {
            out.push_str("ptr(");
            show_type(ast, *value_type, out, pending);
            out.push(')');
        }
        TypeKind::Array {
            num_elems,
            elem_type,
        } => {
            out.push_str(&format!("arr{num_elems}("));
            show_type(ast, *elem_type, out, pending);
            out.push(')');
        }
        TypeKind::Function {
            return_type,
            param_decls,
        } => {
            out.push_str("fn(");
            show_type(ast, *return_type, out, pending);
            out.push(')');
            pending.push(*param_decls);
        }
        TypeKind::Struct { name, .. } => out.push_str(&format!("struct:{name}")),
    }
}

fn dump_node(ast: &Ast, id: NodeId, level: usize, out: &mut String) {
    let e = &ast[id];
    out.push_str(&format!(
        "{:indent$}{} sym={} str={} file={} line={}",
        "",
        e.tag.name(),
        e.token.symbol as u8,
        nz(&e.token.str),
        nz(&e.token.file),
        e.token.line,
        indent = level * 2
    ));
    match e.last {
        None => out.push_str(" last=-"),
        Some(last) if last == id => out.push_str(" last=self"),
        Some(last) => match ast.list(Some(id)).position(|n| n == last) {
            Some(k) => out.push_str(&format!(" last=+{k}")),
            None => out.push_str(" last=?"),
        },
    }
    let mut pending = Vec::new();
    if e.tag == NodeTag::DDecl {
        let v = &ast[e.e_decl().expect("D_DECL without var")];
        let decl = if v.decl == Some(id) { "self" } else { "other" };
        out.push_str(&format!(" var={} decl={decl} type=", nz(&v.name)));
        show_type(ast, v.type_, out, &mut pending);
    }
    if e.tag == NodeTag::DOption {
        let Extra::Option(value) = e.extra else {
            panic!("D_OPTION without value")
        };
        out.push_str(&format!(" value={}", u32::from(value)));
    }
    out.push('\n');
    for params in pending {
        out.push_str(&format!("{:indent$}params\n", "", indent = level * 2 + 2));
        dump_list(ast, params, level + 2, out);
    }
    for (i, c) in e.children.iter().enumerate() {
        if c.is_some() {
            out.push_str(&format!("{:indent$}[{i}]\n", "", indent = level * 2 + 2));
            dump_list(ast, *c, level + 2, out);
        }
    }
}

fn dump_list(ast: &Ast, first: Option<NodeId>, level: usize, out: &mut String) {
    for id in ast.list(first) {
        dump_node(ast, id, level, out);
    }
}

/// The first line where two texts differ, for a readable failure.
fn first_diff(what: &str, c: &str, rs: &str) -> String {
    let line = c
        .lines()
        .zip(rs.lines())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| c.lines().count().min(rs.lines().count()));
    format!(
        "{what} differs at line {}:\n    C:  {:?}\n    rs: {:?}",
        line + 1,
        c.lines().nth(line),
        rs.lines().nth(line)
    )
}

/// Compare both front ends on `input`; `Err` describes the difference.
fn compare(input: &Path) -> Result<Outcome, String> {
    let c = c_outcome(input);
    let rs = rs_outcome(input);
    let mut diffs = Vec::new();
    if c.status != rs.status {
        diffs.push(format!("status: C {:?}, rs {:?}", c.status, rs.status));
    }
    // A C crash leaves no tree to compare.
    if c.status.is_some() && c.tree != rs.tree {
        diffs.push(first_diff("tree", &c.tree, &rs.tree));
    }
    if c.diagnostics != rs.diagnostics {
        diffs.push(first_diff("diagnostics", &c.diagnostics, &rs.diagnostics));
    }
    if diffs.is_empty() {
        Ok(c)
    } else {
        Err(format!("{}:\n  {}", input.display(), diffs.join("\n  ")))
    }
}

/// A directory of this process's own in the target's test scratch area. A
/// test removes it once it passes; a failing one leaves it for the paths
/// its message names.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// The upstream programs, after the system C preprocessor as the EPICS
/// build runs it on a .st (a .stt goes to snc as is), parse alike; only
/// [`C_REJECTS`] fail.
#[test]
fn corpus_parses_as_c_snc() {
    let seq_include = reference_path(ReferenceTree::Modules, "sequencer/include");
    let dir = scratch_dir("snc_corpus");
    let mut failures = Vec::new();
    for (i, rel) in CORPUS.iter().enumerate() {
        let src = reference_path(ReferenceTree::Modules, rel);
        let input = if rel.ends_with(".st") {
            // The EPICS build runs cpp in O.<arch> with -I. -I$(COMMON_DIR)
            // -I.., which is how `#include "../simple.st"` finds its file.
            let src_dir = src.parent().expect("source dir");
            let pre = dir.join(format!("{i}.i"));
            let mut cpp = Command::new("gcc");
            cpp.args(["-x", "c", "-E"]);
            for inc in [
                src_dir.join("O.linux-x86_64"),
                src_dir.join("O.Common"),
                src_dir.to_path_buf(),
                seq_include.clone(),
            ] {
                cpp.arg("-I").arg(inc);
            }
            let text = run_ok(cpp.arg(&src));
            std::fs::write(&pre, text).expect("write preprocessed input");
            pre
        } else {
            src
        };
        match compare(&input) {
            Ok(c) if (c.status != Some(0)) == C_REJECTS.contains(rel) => {}
            Ok(c) => failures.push(format!(
                "{rel}: C snc exit {:?}, expected {}:\n{}",
                c.status,
                if C_REJECTS.contains(rel) {
                    "failure"
                } else {
                    "success"
                },
                c.diagnostics
            )),
            Err(e) => failures.push(format!("{rel}: {e}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} programs:\n{}",
        failures.len(),
        CORPUS.len(),
        failures.join("\n")
    );
    std::fs::remove_dir_all(&dir).expect("remove scratch dir");
}

/// The accepted inputs parse into the same tree, without diagnostics.
#[test]
fn accepted_inputs_parse_as_c_snc() {
    let dir = scratch_dir("snc_accepted");
    let mut failures = Vec::new();
    for (name, text) in ACCEPTED {
        let input = dir.join(format!("{name}.i"));
        std::fs::write(&input, text).expect("write input");
        match compare(&input) {
            Ok(c) if c.status == Some(0) && c.diagnostics.is_empty() => {}
            Ok(c) => failures.push(format!("{name}: C snc rejects it:\n{}", c.diagnostics)),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    std::fs::remove_dir_all(&dir).expect("remove scratch dir");
}

/// Malformed programs draw the same messages, at the same lines, with the
/// same recovery and exit status; where C crashes, the port returns no
/// tree.
#[test]
fn malformed_inputs_report_as_c_snc() {
    let dir = scratch_dir("snc_malformed");
    let mut failures = Vec::new();
    for (name, text) in MALFORMED {
        let input = dir.join(format!("{name}.i"));
        std::fs::write(&input, text).expect("write input");
        match compare(&input) {
            Ok(c) if c.status != Some(0) => {}
            Ok(_) => failures.push(format!("{name}: C snc accepts it")),
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    std::fs::remove_dir_all(&dir).expect("remove scratch dir");
}
