//! The analysis of the crafted programs in `tests/fixtures/analysis`.
//!
//! Each `<name>.out` holds the diagnostics under every option set, as C snc
//! prints them up to the end of analysis; `reference_analysis` checks them
//! against the live C snc.

mod common;

use epics_seq_rs::snc::analysis::traverse_syntax_tree;
use epics_seq_rs::snc::type_check::type_is_function;
use epics_seq_rs::snc::types::{
    Ast, Extra, Multiplicity, NodeTag, Program, State, StateOptions, VarChan, VarId, child,
};
use epics_seq_rs::snc::var_types::TypeKind;

#[test]
fn fixtures_match_goldens() {
    let mut failed = Vec::new();
    for name in common::fixtures() {
        let golden = common::fixture_dir().join(name.replace(".st", ".out"));
        let want = std::fs::read_to_string(&golden).unwrap_or_default();
        let got = common::render(&name);
        if got != want {
            failed.push(format!("{name}\n--- got\n{got}--- want\n{want}"));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The snc-rs command line analyses what it parsed, and an analysis error
/// fails the run before an output file is opened.
#[test]
fn command_line_runs_the_analysis() {
    let out = tempfile::tempdir().unwrap();
    let c_file = out.path().join("reach.c");
    let run = std::process::Command::new(env!("CARGO_BIN_EXE_snc-rs"))
        .arg("reach.st")
        .arg("-o")
        .arg(&c_file)
        .current_dir(common::fixture_dir())
        .output()
        .unwrap();
    let golden = std::fs::read_to_string(common::fixture_dir().join("reach.out")).unwrap();
    let default_section = golden
        .strip_prefix(&common::section_header(&[], "reach.st"))
        .and_then(|rest| rest.split("$ snc").next())
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stderr), default_section);
    assert_eq!(run.status.code(), Some(1));
    assert!(!c_file.exists());
}

fn analysed(name: &str) -> (common::Run, Program) {
    let input = std::fs::read(common::fixture_dir().join(name)).unwrap();
    let mut run = common::analyse(&input, name, &[]);
    let prg = run.prg.take().expect("program analysed");
    (run, prg)
}

fn top_level_var(ast: &Ast, prg: &Program, name: &str) -> VarId {
    let list = ast.var_list_from_scope(prg.prog);
    std::iter::successors(ast[list].first, |&v| ast[v].next)
        .find(|&v| ast[v].name.as_deref() == Some(name))
        .unwrap_or_else(|| panic!("no top-level variable {name}"))
}

/// Code generation asks `type_is_function` of every call that is not a
/// builtin; a subscript of a non-array, non-pointer is a type error.
#[test]
fn type_of_function_calls() {
    let (mut run, prg) = analysed("typeerr.st");
    assert_eq!(run.snc.diagnostics(), b"");
    let mut calls = Vec::new();
    traverse_syntax_tree(
        &mut run.ast,
        Some(prg.prog),
        NodeTag::EFunc.bit(),
        0,
        None,
        &mut |ast: &mut Ast, ep, _| {
            calls.push(ast[ep].child(child::FUNC_EXPR).unwrap());
            true
        },
    );
    let is_function: Vec<bool> = calls
        .into_iter()
        .map(|f| type_is_function(&mut run.snc, &run.ast, f))
        .collect();
    // x[0](), s.fp(), ps->fp(), (*ps).fp(), x[1][2]()
    assert_eq!(is_function, [false, true, true, true, false]);
    assert_eq!(
        String::from_utf8_lossy(run.snc.diagnostics()),
        "typeerr.st:9: error: type error\n\
         typeerr.st:13: error: type error\n\
         typeerr.st:13: error: type error\n"
    );
}

/// A foreign `struct T` becomes the SNL struct once `struct T` is defined,
/// and a self-referencing struct shared by several declarators terminates.
#[test]
fn struct_refs_resolve() {
    let (run, prg) = analysed("structuse.st");
    let ast = &run.ast;
    let kind = |name| &ast[ast[top_level_var(ast, &prg, name)].type_].kind;
    assert!(matches!(kind("s0"), TypeKind::Foreign { .. }));
    assert!(matches!(kind("s1"), TypeKind::Struct { .. }));

    let (run, prg) = analysed("selfref3.st");
    let ast = &run.ast;
    for name in ["a", "b", "c"] {
        let t = ast[top_level_var(ast, &prg, name)].type_;
        let TypeKind::Struct { member_decls, .. } = ast[t].kind else {
            panic!("{name} is not a struct");
        };
        let next = ast[member_decls.unwrap()].e_decl().unwrap();
        let TypeKind::Pointer { value_type } = ast[ast[next].type_].kind else {
            panic!("{name}.next is not a pointer");
        };
        assert!(matches!(ast[value_type].kind, TypeKind::Struct { .. }));
    }
}

/// Channels, event flags, sync queues, states and options as C snc's
/// generated tables for `tables.st` show them.
#[test]
fn program_tables() {
    let (run, prg) = analysed("tables.st");
    let ast = &run.ast;

    assert_eq!((prg.num_ss, prg.num_event_flags), (2, 2));
    assert!(!prg.options.conn && prg.options.safe && prg.options.reent && prg.options.newef);

    let ef_index = |v: VarId| match &ast[v].chan {
        VarChan::EvFlag(ef) => (ef.index, ef.queued),
        other => panic!("not an event flag: {other:?}"),
    };
    assert_eq!(ef_index(top_level_var(ast, &prg, "e1")), (1, true));
    assert_eq!(ef_index(top_level_var(ast, &prg, "e2")), (2, false));

    // chName, count, index, monitored, efId, queue (index, size)
    let chans: Vec<_> = std::iter::successors(prg.chan_list.first, |&c| ast[c].next)
        .map(|c| {
            let c = &ast[c];
            (
                c.name.as_deref().unwrap().to_owned(),
                c.count,
                c.index,
                c.monitor,
                c.sync.map(|ef| ef_index(ef).0),
                c.syncq.map(|q| (ast[q].index, ast[q].size)),
            )
        })
        .collect();
    let s = String::from;
    assert_eq!(
        chans,
        [
            (s("pv:a"), 1, 0, true, Some(1), Some((0, 7))),
            (s("pv:v0"), 2, 0, false, Some(2), None),
            (s(""), 2, 1, false, None, None),
            (s("pv:v2"), 2, 2, true, None, Some((1, 4))),
            (s(""), 1, 0, false, None, None),
        ]
    );
    assert_eq!(prg.chan_list.num_elems, 5);
    assert_eq!(prg.syncq_list.num_elems, 2);

    let var = |name| &ast[top_level_var(ast, &prg, name)];
    use Multiplicity as M;
    let (a, v, d) = (var("a"), var("v"), var("d"));
    assert_eq!((a.index, v.index, d.index), (0, 1, 4));
    assert_eq!(
        (a.assign, a.monitor, a.sync, a.syncq),
        (M::Single, M::Single, M::Single, M::Single)
    );
    // monitor_elem never marks the variable M_MULTI
    assert_eq!(
        (v.assign, v.monitor, v.sync, v.syncq),
        (M::Multi, M::None, M::Multi, M::Multi)
    );
    assert_eq!((d.assign, d.monitor), (M::Single, M::None));

    let states: Vec<Vec<(u32, bool, StateOptions)>> = ast
        .list(ast[prg.prog].child(child::PROG_STATESETS))
        .map(|ss| {
            let Extra::Ss(set) = &ast[ss].extra else {
                panic!("state set data")
            };
            let states: Vec<_> = ast
                .list(ast[ss].child(child::SS_STATES))
                .map(|sp| match &ast[sp].extra {
                    Extra::State(State {
                        index,
                        is_target,
                        options,
                        ..
                    }) => (*index, *is_target, *options),
                    _ => panic!("state data"),
                })
                .collect();
            assert_eq!(set.num_states as usize, states.len());
            states
        })
        .collect();
    let first = StateOptions {
        do_reset_timers: false,
        ..StateOptions::default()
    };
    let def = StateOptions::default();
    assert_eq!(
        states,
        [
            vec![(0, true, first), (1, true, def), (2, false, def)],
            vec![(0, true, def)],
        ]
    );
}
