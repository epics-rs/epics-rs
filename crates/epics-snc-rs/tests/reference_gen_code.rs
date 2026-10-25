//! generate_code against the epics-seq-rs contract: std's delayDo.st must
//! come out as its tests/golden/delayDo.rs, byte for byte, and every
//! upstream program snc-rs accepts must type-check against epics-seq-rs
//! with a stub companion.

#![cfg(unix)]

mod common;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use epics_base_rs::reference::{ReferenceTree, reference_path, reference_root};
use epics_snc_rs::gen_code::{gen_var_decl, generate_code};
use epics_snc_rs::gen_ss_code::{CDecl, escaped_decl, escaped_var_type, variadic_fixed_args};
use epics_snc_rs::types::{Ast, Extra, NodeId, NodeTag, child};
use epics_snc_rs::var_types::TypeKind;

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

/// The 16 programs v0.32.0 moves onto the engine, each of which must
/// type-check.
const IN_SCOPE: [&str; 16] = [
    "measComp/measCompApp/src/USBCTR_SNL.st",
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

/// Programs that do not build, each with the first error it must fail
/// with, so that a fix or a different failure shows. Every other program
/// type-checks or is rejected by analysis as C snc rejects it.
const EXPECTED_FAILURES: &[(&str, &str)] = &[
    // C snc rejects these with the same error
    (
        "sequencer/test/compiler/delay_in_action.st",
        "delay_in_action.st:12: error: calling delay is only allowed inside a when condition",
    ),
    (
        "sequencer/test/compiler/efNoInit.st",
        "efNoInit.st:9: error: event flag 'ef' cannot be initialized",
    ),
    (
        "sequencer/test/compiler/funcdefShadowGlobal.st",
        "funcdefShadowGlobal.st:10: error: parameter 1 to 'pvPut' was not assigned to a pv",
    ),
    (
        "sequencer/test/compiler/pvArray.st",
        "pvArray.st:10: error: passing multi-PV array 'a' to function 'pvAssign' is no longer allowed",
    ),
    (
        "sequencer/test/compiler/pvNotAssigned.st",
        "pvNotAssigned.st:12: error: parameter 1 to 'pvGet' was not assigned to a pv",
    ),
    // C snc and cc accept it, but pvPutCompleteArray is defined nowhere in
    // seq or base, so it does not link
    (
        "sequencer/examples/parallel/control.st",
        "control.st:37: error[E0425]: cannot find function `pvPutCompleteArray` in this scope: not found in this scope",
    ),
    // these wait for C's pointer operations
    (
        "calc/calcApp/src/editSseq.st",
        "editSseq.st:161: error: snc-rs does not support assignment through a pointer",
    ),
    (
        "motor/modules/motorAerotech/aerotechApp/src/EnsembleTrajectoryScan.st",
        "EnsembleTrajectoryScan.st:184: error: snc-rs does not support a pointer that points into a variable",
    ),
    (
        "motor/modules/motorNewport/newportApp/src/MM4005_trajectoryScan.st",
        "MM4005_trajectoryScan.st:316: error: snc-rs does not support a pointer that points into a variable",
    ),
    (
        "motor/modules/motorNewport/newportApp/src/XPS_trajectoryScan.st",
        "XPS_trajectoryScan.st:165: error: snc-rs does not support assignment through a pointer",
    ),
    (
        "motor/modules/motorOms/omsApp/src/MAX_trajectoryScan.st",
        "MAX_trajectoryScan.st:392: error: snc-rs does not support a pointer that points into a variable",
    ),
    (
        "sequencer/test/validate/array.st",
        "array.st:31: error: snc-rs does not support a pointer that points into a variable",
    ),
    (
        "sequencer/test/validate/opttVar.st",
        "opttVar.st:103: error: snc-rs does not support a pointer that points into a variable",
    ),
    (
        "sequencer/test/validate/sizeof.st",
        "sizeof.st:19: error: snc-rs does not support a pointer that points into a variable",
    ),
    // C's void * and a function pointer have no Rust type
    (
        "sequencer/test/compiler/funcdef.st",
        "funcdef.st:8: error: snc-rs does not support void pointer result of function 'f1'",
    ),
    (
        "sequencer/test/compiler/type_expr.st",
        "type_expr.st:13: error: snc-rs does not support void pointer variable 'p'",
    ),
    (
        "sequencer/test/validate/void.st",
        "void.st:12: error: snc-rs does not support void pointer variable 'p'",
    ),
    // these wait for a run-time contract for SNL and foreign structs
    (
        "sequencer/examples/small/decl.st",
        "decl.st:20: error: snc-rs does not support typename TS_STAMP variable 'ts'",
    ),
    (
        "sequencer/test/compiler/cast.st",
        "cast.st:24: error: snc-rs does not support enum enum_test variable 'e'",
    ),
    (
        "sequencer/examples/small/timer.st",
        "timer.st:9: error: snc-rs does not support struct definitions",
    ),
    (
        "sequencer/test/compiler/foreignTypes.st",
        "foreignTypes.st:17: error: snc-rs does not support enum enum_test variable 'e0'",
    ),
    (
        "sequencer/test/compiler/member.st",
        "member.st:6: error: snc-rs does not support struct struct_t variable 's'",
    ),
    (
        "sequencer/test/validate/foreignTypes.st",
        "foreignTypes.st:12: error: snc-rs does not support struct struct_t variable 's'",
    ),
    (
        "sequencer/test/validate/functionInStruct.st",
        "functionInStruct.st:9: error: snc-rs does not support struct definitions",
    ),
    (
        "sequencer/test/validate/indirectCall.st",
        "indirectCall.st:26: error: snc-rs does not support struct s variable 'gx'",
    ),
    (
        "sequencer/test/validate/structdef.st",
        "structdef.st:9: error: snc-rs does not support struct definitions",
    ),
    // these wait for a run-time timestamp with C's members, a foreign
    // global SNL code writes, and a Zeroed pvStat
    (
        "sequencer/examples/small/pvTimeStamp.st",
        "pvTimeStamp.st:10: error: snc-rs does not support a member of a seq run-time value",
    ),
    (
        "sequencer/test/validate/pvGet.st",
        "pvGet.st:26: error: snc-rs does not support assignment to a foreign variable",
    ),
    (
        "sequencer/test/validate/pvGetCancel.st",
        "pvGetCancel.st:25: error: snc-rs does not support typename pvStat variable 'status'",
    ),
];

/// Generated code collected in memory.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One program through snc-rs.
struct Generated {
    code: String,
    diagnostics: String,
    /// errors reported by code generation
    errors: u32,
    foreign: Foreign,
}

/// What snc-rs makes of one program.
enum Outcome {
    /// analysis rejects it, as C snc does (reference_analysis holds the
    /// two to the same diagnostics)
    Rejected,
    /// code generation panicked, with this message
    Panicked(String),
    Generated(Generated),
}

fn generate(st: &Path, opts: &[&str]) -> Outcome {
    let (_scratch, odir, i_name) = common::preprocess(st);
    let source = std::fs::read(odir.join(&i_name)).expect("read .i");
    let mut run = common::analyse(&source, &i_name, opts);
    if run.snc.err_cnt() > 0 {
        return Outcome::Rejected;
    }
    let sink = Sink::default();
    run.snc.set_output(Box::new(sink.clone()));
    let prg = run.prg.as_ref().expect("program");
    if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // C snc builds every program here, so its generator never exits
        generate_code(&mut run.snc, &run.ast, prg).expect("snc bug")
    })) {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|m| m.to_string()))
            .unwrap_or_default();
        return Outcome::Panicked(message);
    }
    let code = String::from_utf8(sink.0.lock().unwrap().clone()).expect("UTF-8 output");
    Outcome::Generated(Generated {
        code,
        diagnostics: String::from_utf8_lossy(run.snc.diagnostics()).into_owned(),
        errors: run.snc.err_cnt(),
        foreign: foreign_names(&run.ast),
    })
}

/// The names a program uses without declaring them: what its escaped C
/// or a C header defines, and so what a companion supplies.
#[derive(Default)]
struct Foreign {
    /// called, with the argument counts of the calls
    funcs: BTreeMap<String, BTreeSet<usize>>,
    /// the escaped-C declaration of a called function
    decls: BTreeMap<String, CDecl>,
    /// read, with the most subscripts a use applies
    values: BTreeMap<String, usize>,
    /// the Rust element type and dimension count of a read value's
    /// escaped-C declaration
    value_types: BTreeMap<String, (&'static str, usize)>,
}

fn foreign_names(ast: &Ast) -> Foreign {
    let undeclared = |id: NodeId| match (ast[id].tag, &ast[id].extra) {
        (NodeTag::EVar, Extra::Var(vp)) if ast[ast[*vp].type_].kind == TypeKind::None => {
            ast[*vp].name.as_deref().map(String::from)
        }
        _ => None,
    };
    let mut foreign = Foreign::default();
    let mut callees = HashSet::new();
    for node in &ast.nodes {
        if node.tag != NodeTag::EFunc {
            continue;
        }
        let callee = node.child(child::FUNC_EXPR).expect("callee");
        if let Some(name) = undeclared(callee) {
            let argc = ast.list(node.child(child::FUNC_ARGS)).count();
            if let Some(decl) = escaped_decl(ast, &name) {
                foreign.decls.insert(name.clone(), decl);
            }
            foreign.funcs.entry(name).or_default().insert(argc);
            callees.insert(callee);
        }
    }
    // the variable under `id`'s subscripts and parentheses, and the count
    // of subscripts
    let base = |mut id: NodeId| {
        let mut dims = 0;
        loop {
            let (operand, sub) = match ast[id].tag {
                NodeTag::ESubscr => (ast[id].child(child::SUBSCR_OPERAND), 1),
                NodeTag::EParen => (ast[id].child(child::PAREN_EXPR), 0),
                _ => return (id, dims),
            };
            let Some(operand) = operand else {
                return (id, dims);
            };
            (id, dims) = (operand, dims + sub);
        }
    };
    for id in (0..ast.nodes.len() as u32).map(NodeId) {
        if let Some(name) = undeclared(id).filter(|_| !callees.contains(&id)) {
            foreign.values.entry(name).or_default();
        }
        let (var, dims) = base(id);
        if let Some(name) = undeclared(var).filter(|_| dims > 0) {
            let most = foreign.values.entry(name).or_default();
            *most = (*most).max(dims);
        }
    }
    for name in foreign.values.keys() {
        if let Some(decl) = escaped_var_type(ast, name) {
            foreign.value_types.insert(name.clone(), decl);
        }
    }
    foreign
}

/// The names `epics_seq_rs::c` exports, which a companion takes as they
/// are.
fn c_exports() -> HashSet<String> {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../epics-seq-rs/src/c.rs"),
    )
    .expect("read epics-seq-rs c.rs");
    let ident = |s: &str| {
        s.trim_start()
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let mut names: HashSet<String> = src
        .split("pub fn ")
        .skip(1)
        .map(ident)
        .chain(src.split("pub enum ").skip(1).map(ident))
        .chain(src.split("pub trait ").skip(1).map(ident))
        .collect();
    // math1!(fabs => abs, ...)
    let math = src.split("math1!(").nth(1).expect("math1 invocation");
    let math = &math[..math.find(");").expect("end of math1")];
    names.extend(
        math.split(',')
            .filter_map(|m| m.split("=>").next())
            .map(ident),
    );
    names.remove("");
    names
}

/// Functions programs call that C defines nowhere, which a companion
/// cannot supply either.
const UNDEFINED: &[&str] = &["pvPutCompleteArray"];

/// A companion for `foreign`: an arm for every escaped fragment that
/// expands the SNL between them, and a placeholder for every foreign name
/// `epics_seq_rs::c` does not supply. A value is a constant, a subscripted
/// one a static array, and an array passed whole a static reference to a
/// `CDst` and `CSrc`, as the epics-seq-rs contract has escaped-C globals.
fn stub_companion(foreign: &Foreign, c: &HashSet<String>) -> String {
    let mut s = String::from(
        "#![allow(dead_code, unused_imports, unused_macros, non_snake_case, non_upper_case_globals)]\n\
         use epics_seq_rs::c::*;\n\
         macro_rules! seqg_escaped {\n    \
             (@arg $text:literal) => {};\n    \
             (@arg $snl:block) => { $snl };\n    \
             ($env:ident $(, $arg:tt)+) => {{ let _ = &$env; $(seqg_escaped!(@arg $arg);)+ }};\n    \
             ($($text:literal),+) => {};\n\
         }\n",
    );
    for (name, argcs) in &foreign.funcs {
        if c.contains(name) || UNDEFINED.contains(&name.as_str()) {
            continue;
        }
        let argc = *argcs.iter().next().unwrap();
        let decl = foreign.decls.get(name);
        // a parameter its prototype types takes that type, as the call
        // converts to it
        let variadic = decl.filter(|d| d.variadic);
        let typed = decl
            .and_then(|d| d.params.as_ref())
            .filter(|p| p.len() == argc || variadic.is_some());
        let mut params: Vec<String> = (0..argc)
            .map(|i| match typed.and_then(|p| p.get(i).copied().flatten()) {
                Some(t) => format!("_: {t}"),
                None => "_: impl Sized".into(),
            })
            .collect();
        // gen_ss_code passes a variadic function's rest as one slice
        let escaped_fixed = variadic.and_then(|d| d.params.as_ref().map(Vec::len));
        if let Some(fixed) = variadic_fixed_args(name).or(escaped_fixed) {
            params.truncate(fixed);
            params.push(if name.ends_with("scanf") {
                "_: &[&dyn ScanTarget]".into()
            } else {
                "_: &[CArg<'_>]".into()
            });
        }
        let ret = decl.and_then(|d| d.ret).unwrap_or("i32");
        s += &format!(
            "pub fn {name}({}) -> {ret} {{ Default::default() }}\n",
            params.join(", ")
        );
    }
    for (name, &used) in &foreign.values {
        if !c.contains(name) && !foreign.funcs.contains_key(name) {
            let (t, dims) = foreign
                .value_types
                .get(name)
                .copied()
                .unwrap_or(("i32", used));
            let (mut ty, mut value) = (t.to_string(), format!("0 as {t}"));
            for _ in 0..dims {
                (ty, value) = (format!("[{ty}; 256]"), format!("[{value}; 256]"));
            }
            if used < dims {
                // an array SNL code passes whole, which C decays to a
                // pointer the callee may write through
                s += &format!(
                    "pub struct {name}_buf;\n\
                     impl CDst for {name}_buf {{ \
                     fn with_c_dst<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> R {{ f(&mut []) }} }}\n\
                     impl CSrc for {name}_buf {{ \
                     fn with_c_buffer<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {{ f(&[]) }} }}\n\
                     pub static {name}: &{name}_buf = &{name}_buf;\n"
                );
                continue;
            }
            let item = if dims > 0 { "static" } else { "const" };
            s += &format!("pub {item} {name}: {ty} = {value};\n");
        }
    }
    s
}

/// The sequencer's own programs under `dir`, sorted.
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
fn delay_do_is_the_golden() {
    let st = reference_path(ReferenceTree::Modules, "std/stdApp/src/delayDo.st");
    let g = match generate(&st, &[]) {
        Outcome::Generated(g) => g,
        Outcome::Panicked(m) => panic!("generate_code panicked on delayDo: {m}"),
        Outcome::Rejected => panic!("delayDo fails analysis"),
    };
    assert_eq!(g.diagnostics, "");
    let code = g.code;
    let golden = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../epics-seq-rs/tests/golden/delayDo.rs"),
    )
    .expect("read golden");
    if code != golden {
        let line = code
            .lines()
            .zip(golden.lines())
            .position(|(a, b)| a != b)
            .unwrap_or(code.lines().count().min(golden.lines().count()));
        panic!(
            "delayDo differs from the golden at line {}:\n got: {:?}\nwant: {:?}",
            line + 1,
            code.lines().nth(line),
            golden.lines().nth(line)
        );
    }
}

/// Every SNL primitive type (C snl.lem's `prim_type`) in prim_types.st has
/// the Rust type of its C type.
#[test]
fn every_prim_type_has_its_rust_type() {
    let (_scratch, odir, i_name) = common::preprocess(&fixture_dir().join("prim_types.st"));
    let source = std::fs::read(odir.join(&i_name)).expect("read .i");
    let mut run = common::analyse(&source, &i_name, &[]);
    let prog = run.prg.as_ref().expect("program").prog;
    let mut decls = Vec::new();
    let mut vp = run.ast[run.ast.var_list_from_scope(prog)].first;
    while let Some(v) = vp {
        if run.ast[v].decl.is_some() {
            let name = run.ast[v].name.as_deref().unwrap_or_default().to_owned();
            decls.push((name, gen_var_decl(&mut run.snc, &run.ast, v)));
        }
        vp = run.ast[v].next;
    }
    assert_eq!(String::from_utf8_lossy(run.snc.diagnostics()), "");
    let want = [
        ("p_char", "std::ffi::c_char"),
        ("p_uchar", "u8"),
        ("p_short", "i16"),
        ("p_ushort", "u16"),
        ("p_int", "i32"),
        ("p_uint", "u32"),
        ("p_long", "i64"),
        ("p_ulong", "u64"),
        ("p_int8t", "i8"),
        ("p_uint8t", "u8"),
        ("p_int16t", "i16"),
        ("p_uint16t", "u16"),
        ("p_int32t", "i32"),
        ("p_uint32t", "u32"),
        ("p_float", "f32"),
        ("p_double", "f64"),
        ("p_string", "seqg_rt::SeqString"),
    ]
    .map(|(n, t)| (n.to_owned(), format!("seqg_rt::SeqVar<{t}>")));
    assert_eq!(decls, want);
}

/// The fixtures snc-rs's own code generation tests use.
fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gen_code")
}

/// Every upstream program and the fixtures of tests/fixtures/gen_code,
/// generated into a crate each with its stub companion, and `cargo check`ed
/// against epics-seq-rs in one workspace, so one program's errors cannot
/// hide another's.
#[test]
fn upstream_programs_type_check() {
    let seq = reference_root(ReferenceTree::Modules).join("sequencer");
    let mut programs = Vec::new();
    for dir in ["examples", "test"] {
        sequencer_programs(&seq.join(dir), &mut programs);
    }
    programs.sort();
    assert_eq!(programs.len(), 96, "sequencer R2-2-9 has 96 .st programs");
    programs.extend(
        MODULE_PROGRAMS
            .iter()
            .map(|rel| reference_path(ReferenceTree::Modules, rel)),
    );
    let mut own: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .expect("read gen_code fixtures")
        .map(|e| e.expect("fixture entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "st"))
        .collect();
    own.sort();
    programs.extend(own);

    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("snc-corpus");
    let _ = std::fs::remove_dir_all(root.join("programs"));
    let c = c_exports();
    let seq_rs = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../epics-seq-rs")
        .canonicalize()
        .expect("epics-seq-rs dir");
    let modules = reference_root(ReferenceTree::Modules);
    let rel = |st: &Path| {
        st.strip_prefix(&modules)
            .unwrap_or(st)
            .display()
            .to_string()
    };
    let (mut members, mut crates) = (Vec::new(), BTreeMap::new());
    let (mut rejected, mut refused, mut panicked) = (Vec::new(), Vec::new(), Vec::new());
    // the first error of every program that does not type-check
    let mut first_error = BTreeMap::new();
    for (i, st) in programs.iter().enumerate() {
        let stem = st.file_stem().unwrap().to_string_lossy();
        let g = match generate(st, &[]) {
            Outcome::Generated(g) => g,
            Outcome::Panicked(m) => {
                panicked.push(format!("{}: {m}", st.display()));
                first_error.insert(rel(st), format!("panicked: {m}"));
                continue;
            }
            Outcome::Rejected => {
                rejected.push(st.display().to_string());
                first_error.insert(rel(st), "rejected by analysis".to_owned());
                continue;
            }
        };
        if g.errors > 0 {
            refused.push(format!("{}:\n{}", st.display(), g.diagnostics));
            let first = g.diagnostics.lines().find(|l| l.contains(": error"));
            let first = first.unwrap_or_default();
            first_error.insert(rel(st), first.trim_start_matches("../").to_owned());
            continue;
        }
        let name = format!("p{i:03}_{stem}");
        let dir = root.join("programs").join(&name);
        std::fs::create_dir_all(dir.join("src")).expect("create program crate");
        std::fs::write(dir.join("generated.rs"), &g.code).expect("write generated code");
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
                 publish = false\n\n[dependencies]\nepics-seq-rs = {{ path = {seq_rs:?} }}\n"
            ),
        )
        .expect("write Cargo.toml");
        let companion = format!(
            "// {}\n{}include!(\"../generated.rs\");\n",
            st.display(),
            stub_companion(&g.foreign, &c)
        );
        std::fs::write(dir.join("src/lib.rs"), companion).expect("write companion");
        members.push(format!("\"programs/{name}\""));
        crates.insert(name, st);
    }
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\nresolver = \"3\"\nmembers = [{}]\n",
            members.join(", ")
        ),
    )
    .expect("write Cargo.toml");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock"),
        root.join("Cargo.lock"),
    )
    .expect("copy Cargo.lock");

    let out = Command::new(env!("CARGO"))
        .args(["check", "--offline", "--workspace", "--keep-going"])
        .arg("--message-format=json-diagnostic-short")
        .env("CARGO_TARGET_DIR", root.join("target"))
        .current_dir(&root)
        .output()
        .expect("run cargo check");
    // a crate type-checks only on cargo's compiler-artifact for it, never
    // on the absence of errors
    let (mut checked, mut errors) = (HashSet::new(), BTreeMap::<String, Vec<String>>::new());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let target = msg["target"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        match msg["reason"].as_str() {
            Some("compiler-artifact") => {
                checked.insert(target);
            }
            Some("compiler-message") if msg["message"]["level"] == "error" => {
                let rendered = msg["message"]["rendered"].as_str().unwrap_or_default();
                errors
                    .entry(target)
                    .or_default()
                    .push(rendered.trim_end().to_owned());
            }
            _ => {}
        }
    }
    let mut failed = Vec::new();
    for (name, st) in &crates {
        let all = errors.get(name).map(Vec::as_slice).unwrap_or_default();
        if checked.contains(name) && all.is_empty() {
            continue;
        }
        let located = all
            .iter()
            .find(|e| e.starts_with(&format!("programs/{name}/")));
        let first = match (located, all.first()) {
            (Some(e), _) => st_error(&root.join("programs").join(name), e),
            (None, Some(e)) => e.clone(),
            (None, None) => "not checked by cargo".to_owned(),
        };
        first_error.insert(rel(st), first.clone());
        let detail = if all.is_empty() {
            first
        } else {
            all.join("\n  ")
        };
        failed.push(format!("{}:\n  {detail}", st.display()));
    }

    let mut gate = Vec::new();
    if !out.status.success() && errors.keys().all(|t| !crates.contains_key(t)) {
        gate.push(format!(
            "cargo check failed ({}) with no error in a program crate:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim_end()
        ));
    }
    for st in IN_SCOPE {
        if let Some(e) = first_error.get(st) {
            gate.push(format!("{st} is in scope and fails: {e}"));
        }
        if !programs.iter().any(|p| rel(p) == st) {
            gate.push(format!("{st} is in scope and not in the corpus"));
        }
    }
    // reference_analysis holds an analysis rejection to C snc's own
    for (st, e) in &first_error {
        if e != "rejected by analysis" && !EXPECTED_FAILURES.iter().any(|&(p, _)| p == st) {
            gate.push(format!("{st} fails and is not pinned: {e}"));
        }
    }
    for &(st, want) in EXPECTED_FAILURES {
        match first_error.get(st) {
            Some(e) if e == want => {}
            got => gate.push(format!("{st} should fail with {want:?}, got {got:?}")),
        }
    }
    gate.extend(panicked.iter().map(|p| format!("panicked: {p}")));
    let message = format!(
        "gate:\n{}\n\n{} of {} generated programs type-check\n\nfailed:\n{}\n\n\
         panicked:\n{}\n\nrefused:\n{}\n\nrejected by analysis: {rejected:?}",
        gate.join("\n"),
        crates.len() - failed.len(),
        crates.len(),
        failed.join("\n"),
        panicked.join("\n"),
        refused.join("\n"),
    );
    std::fs::write(root.join("report.txt"), &message).expect("write report");
    assert!(gate.is_empty(), "{message}");
}

/// A `cargo check --message-format=short` error line of the program crate
/// in `dir`, located by the `// <file>.st:<line>` comment gen_ss_code wrote
/// above it: `pvGet.st:26: error[E0070]: ...`.
fn st_error(dir: &Path, error: &str) -> String {
    let crate_dir = format!("programs/{}/", dir.file_name().unwrap().to_string_lossy());
    let rest = error
        .strip_prefix(&crate_dir)
        .expect("an error of this crate");
    let mut parts = rest.splitn(4, ':');
    let (file, line, _column, message) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    let line: usize = line.parse().expect("error line number");
    let code = std::fs::read_to_string(dir.join(file)).unwrap_or_default();
    let mark = code
        .lines()
        .take(line)
        .filter_map(|l| {
            let mark = l.trim().strip_prefix("// ")?;
            let (_, n) = mark.rsplit_once(".st:")?;
            n.parse::<u32>().is_ok().then_some(mark)
        })
        .last();
    match mark {
        Some(mark) => format!("{mark}:{message}"),
        None => format!("{file}:{line}:{message}"),
    }
}
