//! gen_tables against C snc's tables: the sequencer's own 96
//! programs, the 30 of the module collection and the crafted fixtures,
//! under each option set. C's text is turned into the Rust the epics-seq-rs
//! contract maps it to (its tests/golden/delayDo.rs) and compared token by
//! token with gen_tables' tree; layout is the printer's and not compared
//! here. The diagnostics of gen_tables must follow analysis' as in C.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use epics_base_rs::reference::{ReferenceTree, reference_path, reference_root};
use epics_snc_rs::gen_tables::gen_tables;
use epics_snc_rs::rust_out::{Expr, Item, print};

/// Option sets covering the three variable models and every program option
/// bit.
const OPTION_SETS: [&[&str]; 4] = [&[], &["+r"], &["+s"], &["+a", "-c", "+d", "-e"]];

/// Programs on which C's gen_ss_code reports between analysis and the
/// tables.
const SS_CODE_DIAGNOSTICS: &[&str] = &[
    "delay_in_action.st",
    "efNoInit.st",
    "funcdefShadowGlobal.st",
    "pvArray.st",
    "pvNotAssigned.st",
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

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tables")
}

// ---- C's tables as Rust ----

/// The bytes the C compiler makes of a string literal's body, up to the
/// first NUL, as a Rust literal.
fn c_literal(body: &str) -> String {
    let mut bytes = Vec::new();
    let mut it = body.bytes().peekable();
    while let Some(c) = it.next() {
        if c != b'\\' {
            bytes.push(c);
            continue;
        }
        let e = it.next().expect("escape");
        let value = match e {
            b'a' => 7,
            b'b' => 8,
            b'f' => 12,
            b'n' => 10,
            b'r' => 13,
            b't' => 9,
            b'v' => 11,
            b'x' => {
                let mut v: u32 = 0;
                while let Some(d) = it.peek().and_then(|&d| (d as char).to_digit(16)) {
                    v = (v * 16 + d) % 256;
                    it.next();
                }
                v
            }
            b'0'..=b'7' => {
                let mut v = u32::from(e - b'0');
                for _ in 0..2 {
                    match it.peek() {
                        Some(&d) if (b'0'..=b'7').contains(&d) => {
                            v = v * 8 + u32::from(d - b'0');
                            it.next();
                        }
                        _ => break,
                    }
                }
                v % 256
            }
            other => u32::from(other),
        };
        bytes.push(value as u8);
    }
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    format!("{:?}", String::from_utf8_lossy(&bytes[..end]))
}

/// A C string literal `"..."` as a Rust literal.
fn literal(c: &str) -> String {
    let body = c
        .strip_prefix('"')
        .and_then(|c| c.strip_suffix('"'))
        .unwrap_or_else(|| panic!("not a string literal: {c}"));
    c_literal(body)
}

/// Split a C initializer list at its top-level commas.
fn split_top(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let (mut depth, mut in_str, mut esc) = (0, false, false);
    for ch in s.chars() {
        if in_str {
            cur.push(ch);
            match (esc, ch) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    parts.push(cur.trim().to_string());
    parts
}

fn prim_type(tag: &str) -> &'static str {
    match tag {
        "P_CHAR" => "Char",
        "P_UCHAR" => "UChar",
        "P_SHORT" => "Short",
        "P_USHORT" => "UShort",
        "P_INT" => "Int",
        "P_UINT" => "UInt",
        "P_LONG" => "Long",
        "P_ULONG" => "ULong",
        "P_INT8T" => "Int8T",
        "P_UINT8T" => "UInt8T",
        "P_INT16T" => "Int16T",
        "P_UINT16T" => "UInt16T",
        "P_INT32T" => "Int32T",
        "P_UINT32T" => "UInt32T",
        "P_FLOAT" => "Float",
        "P_DOUBLE" => "Double",
        "P_STRING" => "String",
        other => panic!("prim type {other}"),
    }
}

/// `{"pv", offsetof(struct seqg_vars, x), "x", P_INT, 1, 4, 0, 1, 0, 0}`,
/// its event flag by the name in `flags`.
fn channel(row: &str, flags: &BTreeMap<String, String>) -> String {
    let body = row
        .trim()
        .strip_prefix('{')
        .and_then(|r| r.strip_suffix("},"))
        .unwrap_or_else(|| panic!("channel row {row:?}"));
    let f = split_top(body);
    assert_eq!(f.len(), 10, "channel row {row:?}");
    let ch_name = match f[0].as_str() {
        "0" => "None".to_string(),
        pv => format!("Some({})", literal(pv)),
    };
    let place = f[1]
        .strip_prefix("offsetof(struct seqg_vars, ")
        .and_then(|p| p.strip_suffix(')'))
        .or_else(|| f[1].strip_prefix("(size_t)&"))
        .unwrap_or_else(|| panic!("channel offset {}", f[1]));
    let monitored = match f[7].as_str() {
        "0" => "false",
        "1" => "true",
        m => panic!("monitor flag {m}"),
    };
    let queue_size = match f[8].as_str() {
        "DEFAULT_QUEUE_SIZE" => "seqg_rt::DEFAULT_QUEUE_SIZE",
        n => n,
    };
    let ef_id = match f[6].as_str() {
        "0" => "seqg_rt::EfId(0)".to_string(),
        n => flags
            .get(n)
            .unwrap_or_else(|| panic!("event flag {n}"))
            .clone(),
    };
    format!(
        "seqg_rt::SeqChan {{ ch_name: {ch_name}, var: |v| &v.{place}, var_name: {}, \
         var_type: seqg_rt::PrimType::{}, count: {}, event_num: {}, ef_id: {ef_id}, \
         monitored: {monitored}, queue_size: {queue_size}, queue_index: {} }},\n",
        literal(&f[2]),
        prim_type(&f[3]),
        f[4],
        f[5],
        f[9]
    )
}

/// `(0 | OPT_A | OPT_B)` as `ty::A | ty::B`, `0` without bits, leaving out
/// `dropped`.
fn bits(c: &str, ty: &str, dropped: &[&str]) -> String {
    let inner = c
        .strip_prefix("(0")
        .and_then(|c| c.strip_suffix(')'))
        .unwrap_or_else(|| panic!("option bits {c}"));
    let set: Vec<String> = inner
        .split(" | ")
        .filter(|b| !b.is_empty())
        .map(|b| b.strip_prefix("OPT_").expect("OPT_ bit"))
        .filter(|b| !dropped.contains(b))
        .map(|b| format!("{ty}::{b}"))
        .collect();
    if set.is_empty() {
        "0".into()
    } else {
        set.join(" | ")
    }
}

fn func(c: &str) -> String {
    match c {
        "0" => "None".into(),
        f => format!("Some({f})"),
    }
}

/// `\t/* label */   value,` as (label, value).
fn labelled(line: &str) -> (String, String) {
    let rest = line
        .trim()
        .strip_prefix("/* ")
        .unwrap_or_else(|| panic!("labelled field {line:?}"));
    let (label, value) = rest.split_once(" */").expect("label end");
    let value = value.trim();
    let value = value.strip_suffix(',').unwrap_or(value);
    (label.to_string(), value.to_string())
}

/// The labelled fields of one `{ ... }` entry, up to `\t},` (or `};` for
/// the program table).
fn entry<'a>(lines: &mut impl Iterator<Item = &'a str>, end: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    for line in lines.by_ref() {
        if line == end {
            return fields;
        }
        fields.push(labelled(line));
    }
    panic!("entry without {end:?}")
}

fn get<'a>(fields: &'a [(String, String)], label: &str) -> &'a str {
    &fields
        .iter()
        .find(|(l, _)| l == label)
        .unwrap_or_else(|| panic!("no field {label:?} in {fields:?}"))
        .1
}

/// C snc's tables, from `/* Tables */` through the program table, as Rust
/// text.
fn c_tables_as_rust(c: &str) -> String {
    // `static const EF_ID name = n;` by n
    let flags: BTreeMap<String, String> = c
        .lines()
        .filter_map(|l| l.strip_prefix("static const EF_ID ")?.strip_suffix(';'))
        .filter_map(|d| d.split_once(" = "))
        .map(|(name, n)| (n.to_string(), name.to_string()))
        .collect();
    let start = c
        .find("/************************ Tables")
        .expect("C output has tables");
    let mut lines = c[start..].lines();
    let mut out = String::new();
    // C names a state's items `<prefix>_<ss>_<ssnum>_<state>`; the port
    // names them `<prefix>_<state>` in a module of the state set
    let mut ss_num = 0;
    let mut mask_ss = None;
    let state_item = |c: &str, prefix: &str, module: &str, ss: &str, ss_num, state: &str| {
        assert_eq!(
            c,
            format!("{prefix}_{ss}_{ss_num}_{state}"),
            "C state item name"
        );
        format!("{module}_{ss}::{prefix}_{state}")
    };
    while let Some(line) = lines.next() {
        if let Some(stars) = line.strip_prefix('/').and_then(|l| l.strip_suffix('/'))
            && stars.contains(" Tables ")
        {
            out += &format!("// {stars}\n");
        } else if line.is_empty() {
        } else if let Some(text) = line.strip_prefix("/* ").and_then(|l| l.strip_suffix(" */")) {
            if mask_ss.take().is_some() {
                out += "}\n";
            }
            out += &format!("// {text}\n");
            if let Some(ss) = text
                .strip_prefix("Event masks for state set \"")
                .and_then(|t| t.strip_suffix('"'))
            {
                out += &format!("mod seqg_mask_{ss} {{\nuse super::*;\n");
                mask_ss = Some(ss.to_string());
            }
        } else if line == "#define seqg_chans 0" {
            out += "static seqg_chans: [seqg_rt::SeqChan<seqg_vars>; 0] = [];\n";
        } else if line == "static seqChan seqg_chans[] = {" {
            let mut rows = Vec::new();
            loop {
                let line = lines.next().expect("channel table end");
                if line == "};" {
                    break;
                } else if line.starts_with("\t/* chName,") {
                } else if line == "#if LONG_MAX > 0x7fffffffL" {
                    // the run-time's validate refuses a long channel
                    for skipped in lines.by_ref() {
                        if skipped == "#endif" {
                            break;
                        }
                    }
                } else {
                    rows.push(channel(line, &flags));
                }
            }
            out += &format!(
                "static seqg_chans: [seqg_rt::SeqChan<seqg_vars>; {}] = [\n{}];\n",
                rows.len(),
                rows.concat()
            );
        } else if let Some(name) = line
            .strip_prefix("static const seqMask ")
            .and_then(|l| l.strip_suffix("[] = {"))
        {
            let mut words = Vec::new();
            loop {
                let line = lines.next().expect("mask end");
                if line == "};" {
                    break;
                }
                words.push(line.trim().to_string());
            }
            let ss = mask_ss.as_deref().expect("mask after its comment");
            let state = name
                .strip_prefix(&format!("seqg_mask_{ss}_{ss_num}_"))
                .expect("C mask name");
            out += &format!(
                "pub static seqg_mask_{state}: [seqg_rt::SeqMask; {}] = [{}];\n",
                words.len(),
                words.concat()
            );
        } else if let Some(name) = line
            .strip_prefix("static seqState ")
            .and_then(|l| l.strip_suffix("[] = {"))
        {
            let ss = name
                .strip_prefix("seqg_states_")
                .expect("C state table name");
            let mut states = Vec::new();
            loop {
                match lines.next().expect("state table end") {
                    "};" => break,
                    "\t{" => {
                        let f = entry(&mut lines, "\t},");
                        let state_lit = get(&f, "state name");
                        let state = &state_lit[1..state_lit.len() - 1];
                        let item = |label, prefix, module| match get(&f, label) {
                            "0" => "0".to_string(),
                            c => state_item(c, prefix, module, ss, ss_num, state),
                        };
                        states.push(format!(
                            "seqg_rt::SeqState {{ state_name: {}, action_func: {}, \
                             event_func: {}, entry_func: {}, exit_func: {}, event_mask: &{}, \
                             options: seqg_rt::StateOptions::new({}) }},\n",
                            literal(state_lit),
                            item("action function", "seqg_action", "seqg_ss"),
                            item("event function", "seqg_event", "seqg_ss"),
                            func(&item("entry function", "seqg_entry", "seqg_ss")),
                            func(&item("exit function", "seqg_exit", "seqg_ss")),
                            item("event mask array", "seqg_mask", "seqg_mask"),
                            bits(get(&f, "state options"), "seqg_rt::StateOptions", &[]),
                        ));
                    }
                    other => panic!("state table line {other:?}"),
                }
            }
            ss_num += 1;
            out += &format!(
                "static {name}: [seqg_rt::SeqState<seqg_vars>; {}] = [\n{}];\n",
                states.len(),
                states.concat()
            );
        } else if line == "static seqSS seqg_statesets[] = {" {
            let mut sets = Vec::new();
            loop {
                match lines.next().expect("state set table end") {
                    "};" => break,
                    "" => {}
                    "\t{" => {
                        let f = entry(&mut lines, "\t},");
                        sets.push(format!(
                            "seqg_rt::SeqSs {{ ss_name: {}, states: &{} }},\n",
                            literal(get(&f, "state set name")),
                            get(&f, "states"),
                        ));
                    }
                    other => panic!("state set table line {other:?}"),
                }
            }
            out += &format!(
                "static seqg_statesets: [seqg_rt::SeqSs<seqg_vars>; {}] = [\n{}];\n",
                sets.len(),
                sets.concat()
            );
        } else if let Some(name) = line
            .strip_prefix("seqProgram ")
            .and_then(|l| l.strip_suffix(" = {"))
        {
            let f = entry(&mut lines, "};");
            let options = get(&f, "encoded options");
            let vars = match get(&f, "user var size") {
                "0" => "seqg_rt::VarModel::Static(seqg_static_vars)",
                "sizeof(struct seqg_vars)" if options.contains("OPT_SAFE") => {
                    "seqg_rt::VarModel::Safe"
                }
                "sizeof(struct seqg_vars)" => "seqg_rt::VarModel::Reentrant",
                other => panic!("user var size {other}"),
            };
            out += &format!(
                "pub static {name}: seqg_rt::SeqProgram<seqg_vars> = seqg_rt::SeqProgram {{ \
                 magic: {}, prog_name: {}, chan: &{}, ss: &{}, vars: {vars}, params: {}, \
                 num_ev_flags: {}, options: seqg_rt::ProgOptions::new({}), init_func: {}, \
                 entry_func: {}, exit_func: {}, num_queues: {} }};\n",
                get(&f, "magic number"),
                literal(get(&f, "program name")),
                get(&f, "channels"),
                get(&f, "state sets"),
                literal(get(&f, "param")),
                get(&f, "num. event flags"),
                bits(options, "seqg_rt::ProgOptions", &["REENT", "SAFE"]),
                get(&f, "init func"),
                func(get(&f, "entry func")),
                func(get(&f, "exit func")),
                get(&f, "num. queues"),
            );
            return out;
        } else {
            panic!("C table line {line:?}");
        }
    }
    panic!("C output ends before the program table")
}

// ---- gen_tables' tree as text ----

fn rs_expr(e: &Expr) -> String {
    let list = |args: &[epics_snc_rs::rust_out::Arg]| {
        args.iter()
            .map(|a| rs_expr(&a.expr))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match e {
        Expr::Atom(s) => s.clone(),
        Expr::Call(callee, args) => format!("{}({})", rs_expr(callee), list(args)),
        Expr::Array(elems) => format!("[{}]", list(elems)),
        Expr::Struct(path, fields) => format!(
            "{path} {{ {} }}",
            fields
                .iter()
                .map(|(name, e)| format!("{name}: {}", rs_expr(e)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Field(base, name) => format!("{}.{name}", rs_expr(base)),
        Expr::Index(base, index) => format!("{}[{}]", rs_expr(base), rs_expr(index)),
        Expr::Unary(op, e) => format!("{op}{}", rs_expr(e)),
        Expr::Binary(lhs, op, rhs) => format!("{} {op} {}", rs_expr(lhs), rs_expr(rhs)),
        Expr::Closure(params, body) => format!("|{params}| {}", rs_expr(body)),
        other => panic!("gen_tables built {other:?}"),
    }
}

fn rs_items(items: &[Item]) -> String {
    let mut out = String::new();
    for item in items {
        match item {
            Item::Blank => {}
            Item::Comment(text) => out += &format!("// {text}\n"),
            Item::Use {
                public: false,
                path,
            } => out += &format!("use {path};\n"),
            Item::Mod(name, inner) => out += &format!("mod {name} {{\n{}}}\n", rs_items(inner)),
            Item::Const(c) => {
                out += &format!(
                    "{}{} {}: {} = {};\n",
                    if c.public { "pub " } else { "" },
                    if c.is_static { "static" } else { "const" },
                    c.name,
                    c.ty,
                    rs_expr(&c.expr)
                )
            }
            other => panic!("gen_tables built {other:?}"),
        }
    }
    out
}

/// Rust tokens, a trailing comma before a closing bracket dropped.
fn tokens(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let start = i;
        if c[i].is_whitespace() {
            i += 1;
            continue;
        } else if c[i] == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if c[i] == '"' {
            i += 1;
            while c[i] != '"' {
                i += if c[i] == '\\' { 2 } else { 1 };
            }
            i += 1;
        } else if c[i].is_alphanumeric() || c[i] == '_' {
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') {
                i += 1;
            }
        } else {
            i += 1;
        }
        toks.push(c[start..i].iter().collect::<String>());
    }
    let mut out: Vec<String> = Vec::with_capacity(toks.len());
    for (n, t) in toks.iter().enumerate() {
        let closes = matches!(toks.get(n + 1).map(String::as_str), Some(")" | "]" | "}"));
        if !(t == "," && closes) {
            out.push(t.clone());
        }
    }
    out
}

fn first_token_diff(c: &[String], rs: &[String]) -> String {
    let n = c.iter().zip(rs).take_while(|(a, b)| a == b).count();
    let around = |t: &[String]| t[n.saturating_sub(8)..(n + 8).min(t.len())].join(" ");
    format!("token {n}\n  C:  {}\n  rs: {}", around(c), around(rs))
}

// ---- running both ----

/// Compare one program under one option set; `Err` describes a mismatch.
fn check(dir: &Path, input: &str, program: &str, opts: &[&str]) -> Result<(), String> {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let out_c = scratch.path().join("out.c");
    let c = Command::new(c_snc())
        .args(opts)
        .arg(input)
        .arg("-o")
        .arg(&out_c)
        .current_dir(dir)
        .output()
        .expect("run C snc");
    let source = std::fs::read(dir.join(input)).expect("read input");
    let mut run = common::analyse(&source, input, opts);
    let fail = |why: String| Err(format!("{program} {opts:?}: {why}"));

    let Some(prg) = run.prg.as_ref().filter(|_| run.snc.err_cnt() == 0) else {
        // no code generation: reference_analysis compares this far
        if c.status.success() {
            return fail("C snc compiles what the port's analysis refuses".into());
        }
        return Ok(());
    };
    let analysed = run.snc.diagnostics().len();
    let items = gen_tables(&mut run.snc, &run.ast, prg);
    let diagnostics = run.snc.diagnostics();
    let (got_a, got_t) = diagnostics.split_at(analysed);
    let want = &c.stderr[..];
    let show = |s: &[u8]| String::from_utf8_lossy(s).into_owned();
    let diagnostics_agree = if SS_CODE_DIAGNOSTICS.contains(&program) {
        want.len() >= diagnostics.len() && want.starts_with(got_a) && want.ends_with(got_t)
    } else {
        want == diagnostics
    };
    if !diagnostics_agree {
        return fail(format!(
            "diagnostics differ\n--- port\n{}--- C snc\n{}",
            show(diagnostics),
            show(want)
        ));
    }
    if !SS_CODE_DIAGNOSTICS.contains(&program) && c.status.success() != (run.snc.err_cnt() == 0) {
        return fail(format!(
            "C snc exit {} with {} port errors",
            c.status,
            run.snc.err_cnt()
        ));
    }
    if !c.status.success() {
        return Ok(());
    }

    let want = c_tables_as_rust(&std::fs::read_to_string(&out_c).expect("read C output"));
    let got = rs_items(&items);
    let (want_t, got_t) = (tokens(&want), tokens(&got));
    if want_t != got_t {
        return fail(format!(
            "tables differ at {}\n--- port\n{got}--- C snc as Rust\n{want}",
            first_token_diff(&want_t, &got_t)
        ));
    }
    Ok(())
}

fn check_all(dir: &Path, input: &str, program: &str, failed: &mut Vec<String>) {
    for opts in OPTION_SETS {
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
fn fixtures_match_c_snc() {
    let dir = fixture_dir();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("read fixture dir")
        .map(|e| e.expect("fixture entry").file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".st"))
        .collect();
    names.sort();
    assert!(!names.is_empty());
    let mut failed = Vec::new();
    for name in &names {
        check_all(&dir, name, name, &mut failed);
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
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

/// The contract's own golden: delayDo's tables, from the Tables comment
/// through the program table, printed inside `mod seqg` as generate_code
/// places them, byte for byte.
#[test]
fn delay_do_matches_golden() {
    let st = reference_path(ReferenceTree::Modules, "std/stdApp/src/delayDo.st");
    let (_scratch, odir, i_name) = common::preprocess(&st);
    let source = std::fs::read(odir.join(&i_name)).expect("read .i");
    let mut run = common::analyse(&source, &i_name, &[]);
    assert_eq!(run.snc.err_cnt(), 0, "delayDo analysis");
    let prg = run.prg.as_ref().expect("delayDo program");
    let items = gen_tables(&mut run.snc, &run.ast, prg);

    let golden = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../epics-seq-rs/tests/golden/delayDo.rs"),
    )
    .expect("read golden");
    let lines: Vec<&str> = golden.lines().collect();
    let first = lines
        .iter()
        .position(|l| l.trim() == "// ************************ Tables ************************")
        .expect("golden Tables comment");
    let program = lines
        .iter()
        .position(|l| {
            l.trim() == "pub static delayDo: seqg_rt::SeqProgram<seqg_vars> = seqg_rt::SeqProgram {"
        })
        .expect("golden program table");
    let last = program
        + lines[program..]
            .iter()
            .position(|l| l.trim() == "};")
            .unwrap();
    let want = &lines[first..=last];
    let printed = print(&[Item::Mod("seqg".into(), items)]);
    let printed: Vec<&str> = printed.lines().collect();
    let got = &printed[1..printed.len() - 1];
    if let Some(n) = (0..want.len().max(got.len())).find(|&n| want.get(n) != got.get(n)) {
        panic!(
            "delayDo tables differ from the golden at its line {}\n  golden: {:?}\n  port:   {:?}",
            first + n + 1,
            want.get(n),
            got.get(n)
        );
    }
}
