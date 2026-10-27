//! The state set code (C `gen_ss_code.c`): the program's init, entry and
//! exit functions, each state's entry, exit, event and action functions,
//! and the functions an SNL `funcdef` defines.
//!
//! C prints each expression as the C text it is and leaves the conversions
//! to the C compiler. Rust converts nothing implicitly, so every translated
//! expression carries its `Ty`, and C's integer promotions, usual
//! arithmetic conversions, truth values and conversions on assignment are
//! written out.

use std::collections::BTreeSet;

use crate::builtin::{CONST_SYMBOLS, ConstType, FUNC_SYMBOLS, Param as BuiltinParam, ParamType};
use crate::driver::{Exit, Snc};
use crate::gen_code::{
    CClass, c_bracket_balance, c_classes, escaped_key, escaped_lit, gen_var_decl, line_comment,
    rt_typename, value_type, vars,
};
use crate::gen_tables::var_place;
use crate::rust_out::{Arg, Arm, Expr, Fn, Item, Param, Stmt, c_unescape};
use crate::seq_prim_types::PrimType;
use crate::type_check::type_is_function;
use crate::types::{Ast, Extra, Multiplicity, NodeId, NodeTag, Program, Tok, TypeId, VarId, child};
use crate::var_types::{ForeignTag, TypeKind};

const NM_ENTRY: &str = "seqg_entry";
const NM_EXIT: &str = "seqg_exit";
const NM_INIT: &str = "seqg_init";
const NM_ACTION: &str = "seqg_action";
const NM_EVENT: &str = "seqg_event";
const NM_SS: &str = "seqg_ss";
const ENV_TYPE: &str = "&seqg_rt::StateSet<seqg_vars>";
const C_CHAR: &str = "std::ffi::c_char";
const C_VOID: &str = "std::ffi::c_void";

/// C's truth of a value of unknown type (`holds`) or its negation. The
/// value may be a pointer, which has no integer 0, so `seqg_rt::c::truth`
/// tests it against its `Default`; a bare `== Default::default()` is
/// ambiguous where serde_json adds `impl PartialEq<Value> for i32`.
fn foreign_truth(e: Expr, holds: bool) -> Expr {
    let t = Expr::call("seqg_rt::c::truth", vec![e]);
    if holds { t } else { Expr::unary("!", t) }
}

/// C `gen_ss_code`: the program init function, the program entry
/// function, the functions of every state of every state set and the
/// program exit function, in that order.
///
/// C's `seqg_event_<ss>_<ssnum>_<state>` collides for ss `a` state `b_1_c`
/// and ss `a_0_b` state `c`. Here a state's functions are module
/// `seqg_ss_<ss>`'s `seqg_event_<state>` and the like, each name built
/// from one SNL name unique in its scope.
pub fn gen_ss_code(snc: &mut Snc, ast: &Ast, prg: &Program) -> Result<Vec<Item>, Exit> {
    let prog = &ast[prg.prog];
    let mut out = Vec::new();
    doc(&mut out, "Program init func".into());
    out.push(Item::Fn(gen_prog_init(snc, ast, prg.prog)?));
    if let Some(entry) = prog.child(child::PROG_ENTRY) {
        doc(&mut out, "Program entry func".into());
        out.push(Item::Fn(gen_entex_func(snc, ast, NM_ENTRY.into(), entry)));
    }
    for ssp in ast.list(prog.child(child::PROG_STATESETS)) {
        let ss_name = ast[ssp].token.text();
        let mut items = vec![Item::Use {
            public: false,
            path: "super::*".into(),
        }];
        for sp in ast.list(ast[ssp].child(child::SS_STATES)) {
            let state = &ast[sp];
            let state_name = state.token.text();
            doc(
                &mut items,
                format!("****** Code for state \"{state_name}\" in state set \"{ss_name}\" ******"),
            );
            // public for the state table, which is outside the module
            let mut push = |kind: &str, f: Fn| {
                doc(
                    &mut items,
                    format!(
                        "{kind} function for state \"{state_name}\" in state set \"{ss_name}\""
                    ),
                );
                items.push(Item::Fn(Fn { public: true, ..f }));
            };
            if let Some(entry) = state.child(child::STATE_ENTRY) {
                let name = format!("{NM_ENTRY}_{state_name}");
                push("Entry", gen_entex_func(snc, ast, name, entry));
            }
            if let Some(exit) = state.child(child::STATE_EXIT) {
                let name = format!("{NM_EXIT}_{state_name}");
                push("Exit", gen_entex_func(snc, ast, name, exit));
            }
            let whens = state.child(child::STATE_WHENS);
            let name = format!("{NM_EVENT}_{state_name}");
            push("Event", gen_event_func(snc, ast, name, whens));
            let name = format!("{NM_ACTION}_{state_name}");
            push("Action", gen_action_func(snc, ast, name, whens));
        }
        out.push(Item::Blank);
        out.push(Item::Mod(format!("{NM_SS}_{ss_name}"), items));
    }
    if let Some(exit) = prog.child(child::PROG_EXIT) {
        doc(&mut out, "Program exit func".into());
        out.push(Item::Fn(gen_entex_func(snc, ast, NM_EXIT.into(), exit)));
    }
    // generate_code puts the empty line before the section
    out.remove(0);
    Ok(out)
}

/// C `gen_funcdef`: a function whose first parameter is the calling state
/// set's `seqg_env`, C's implicit `SS_ID`. A parameter is a `SeqVar`
/// local, as every SNL variable is, so the body reads it the same way.
pub fn gen_funcdef(snc: &mut Snc, ast: &Ast, _prg: &Program, fp: NodeId) -> Vec<Item> {
    let decl = ast[fp].child(child::FUNCDEF_DECL).expect("funcdef decl");
    let vp = ast[decl].e_decl().expect("funcdef decl var");
    let name = ast[vp].name.as_deref().unwrap_or_default();
    let TypeKind::Function {
        return_type,
        param_decls,
    } = ast[ast[vp].type_].kind
    else {
        unreachable!("analysis reports a funcdef that is not a function");
    };
    let mut g = Gen::new(snc, ast, Context::Func);
    let mut params = Vec::new();
    let mut body = Vec::new();
    // the first is the implicit `seqg_env`, the env parameter
    for pd in ast.list(param_decls).skip(1) {
        let pv = ast[pd].e_decl().expect("parameter decl var");
        let pname = ast[pv].name.as_deref().unwrap_or_default();
        let ty = param_type(ast, ast[pv].type_).unwrap_or_else(|unsupported| {
            g.snc.error_at_node(
                &ast[pd],
                format_args!("snc-rs does not support {unsupported} parameter '{pname}'\n"),
            );
            "()".into()
        });
        params.push(Param {
            name: pname.into(),
            ty,
        });
        body.push(Stmt::Let {
            pat: pname.into(),
            ty: None,
            init: Some(Expr::call("seqg_rt::SeqVar::new", vec![Expr::atom(pname)])),
        });
    }
    let ret = match ast[return_type].kind {
        TypeKind::Void => None,
        _ => Some(value_type(ast, return_type).unwrap_or_else(|unsupported| {
            g.snc.error_at_node(
                &ast[decl],
                format_args!("snc-rs does not support {unsupported} result of function '{name}'\n"),
            );
            "()".into()
        })),
    };
    g.ret = ret.is_some().then_some(return_type);
    if let Some(block) = ast[fp].child(child::FUNCDEF_BLOCK) {
        body.extend(g.block_body(block, &None));
    }
    let body = g.finish(body);
    params.insert(0, g.env_param());
    let mut out = Vec::new();
    out.push(Item::Blank);
    out.extend(line_comment(g.snc, &ast[decl]).map(Item::Comment));
    out.push(Item::Fn(Fn {
        public: false,
        name: name.into(),
        params,
        ret,
        body,
    }));
    out
}

/// The Rust type of a function parameter of SNL type `t`. C passes an
/// array or a `string` as a pointer the callee may write through, which a
/// Rust copy would not reach, so those have none.
fn param_type(ast: &Ast, t: TypeId) -> Result<String, String> {
    match ast[t].kind {
        TypeKind::Array { .. } => Err("array".into()),
        TypeKind::Prim(PrimType::String) => Err("string".into()),
        _ => value_type(ast, t),
    }
}

/// The number of fixed parameters of a variadic C library function that
/// SNL code calls, whose remaining arguments snc-rs passes as one slice.
pub fn variadic_fixed_args(name: &str) -> Option<usize> {
    variadic(name).map(|(fixed, _)| fixed)
}

/// `(fixed parameters, scans)`: a `printf`-like function takes its
/// variadic arguments as `CArg`s, a `scanf`-like one as places.
fn variadic(name: &str) -> Option<(usize, bool)> {
    Some(match name {
        "printf"
        | "errlogPrintf"
        | "epicsPrintf"
        | "errlogPrintfNoConsole"
        | "epicsStdoutPrintf"
        | "cantProceed"
        | "testPass"
        | "testFail"
        | "testDiag"
        | "testAbort" => (1, false),
        "sprintf" | "fprintf" | "errlogSevPrintf" | "testOk" => (2, false),
        "snprintf" | "epicsSnprintf" => (3, false),
        "errPrintf" => (4, false),
        "scanf" => (1, true),
        "sscanf" | "fscanf" => (2, true),
        _ => return None,
    })
}

/// A parameter of a C function's prototype.
#[derive(Clone, Copy)]
enum CParam {
    /// a string read, passed as it is
    Src,
    /// a character buffer written, passed as it is
    Dst,
    /// a number, converted to this type
    Num(&'static str),
    /// a `char *`, converted as C converts to a pointer type: `0` is null
    CharPtr,
    /// a pointer the callee writes through, passed as the place
    Place,
}

/// The prototype in scope where SNL code calls a C function, by which gcc
/// converts the arguments and types the call.
#[derive(Default)]
struct CProto {
    /// `None` for a parameter its declaration does not spell in keywords
    params: Vec<Option<CParam>>,
    /// `None` where its declaration does not spell it in keywords
    ret: Option<Ty>,
    /// [`variadic`]'s, for a variadic function
    variadic: Option<(usize, bool)>,
}

/// [`CProto`] of C function `name`: the library's for one `epics_seq_rs::c`
/// supplies, else the program's escaped-C declaration.
fn c_prototype(ast: &Ast, name: &str) -> CProto {
    let lib = variadic(name);
    if let Some((params, ret)) = prototype(name) {
        return CProto {
            params: params.iter().copied().map(Some).collect(),
            ret: Some(ret),
            variadic: lib,
        };
    }
    let Some(decl) = escaped_decl(ast, name) else {
        return CProto {
            variadic: lib,
            ..CProto::default()
        };
    };
    let params = decl.params.unwrap_or_default();
    let variadic = lib.or(decl.variadic.then_some((params.len(), false)));
    let ret = decl.ret.map(|t| {
        if t == "()" {
            Ty::Unit
        } else {
            Ty::Num(t.into())
        }
    });
    CProto {
        params: params.into_iter().map(|p| p.map(CParam::Num)).collect(),
        ret,
        variadic,
    }
}

/// The prototype of a C library function `epics_seq_rs::c` supplies, by
/// which C converts the arguments: its fixed parameters and its result.
fn prototype(name: &str) -> Option<(&'static [CParam], Ty)> {
    use CParam::{CharPtr, Dst, Num, Place, Src};
    let num = |t: &str| Ty::Num(t.into());
    let char_ptr = || Ty::Ptr(C_CHAR.into());
    Some(match name {
        "strcpy" | "strcat" => (&[Dst, Src], Ty::Unit),
        "strncpy" => (&[Dst, Src, Num("usize")], Ty::Unit),
        "strcmp" => (&[Src, Src], num("i32")),
        "strlen" => (&[Src], num("usize")),
        "atof" => (&[Src], num("f64")),
        "epicsThreadSleep" => (&[Num("f64")], Ty::Unit),
        "fabs" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "sqrt" | "floor" | "ceil"
        | "exp" | "log" | "log10" => (&[Num("f64")], num("f64")),
        "atan2" | "pow" => (&[Num("f64"), Num("f64")], num("f64")),
        "printf" => (&[Src], num("i32")),
        "sprintf" => (&[Dst, Src], num("i32")),
        "epicsSnprintf" => (&[Dst, Num("usize"), Src], num("i32")),
        "sscanf" => (&[Src, Src], num("i32")),
        "strchr" => (&[CharPtr, Num("i32")], char_ptr()),
        "strstr" => (&[CharPtr, Src], char_ptr()),
        "epicsStrtok_r" => (&[CharPtr, Src, Place], char_ptr()),
        _ => return None,
    })
}

/// C's `\n/* doc */\n` before a function.
fn doc(out: &mut Vec<Item>, text: String) {
    out.push(Item::Blank);
    out.push(Item::Comment(text));
}

/// C `gen_prog_func(prog, "init", NM_INIT, gen_prog_init_body)`: the
/// initializers of the variables with program lifetime (C
/// `gen_user_var_init`).
fn gen_prog_init(snc: &mut Snc, ast: &Ast, prog: NodeId) -> Result<Fn, Exit> {
    let mut g = Gen::new(snc, ast, Context::Global);
    g.has_env = false;
    let mut body = Vec::new();
    for vp in vars(ast, Some(ast.var_list_from_scope(prog))) {
        let Some(init) = decl_init(ast, vp) else {
            continue;
        };
        if ast[ast[vp].type_].kind == TypeKind::EvFlag {
            g.snc.error_at_node(
                &ast[init],
                format_args!(
                    "event flag '{}' cannot be initialized\n",
                    ast[vp].name.as_deref().unwrap_or_default()
                ),
            );
        } else {
            // C gen_builtin_func asserts before it writes the call
            if let Some((call, name)) = first_builtin_call(ast, init) {
                g.snc.assert_at_node(
                    false,
                    &ast[call],
                    format_args!("calling built-in function {name} not allowed here\n"),
                )?;
            }
            g.var_init(vp, init, &mut body);
        }
    }
    g.cx = Context::Ss;
    for ssp in ast.list(ast[prog].child(child::PROG_STATESETS)) {
        let scopes = std::iter::once(ssp).chain(ast.list(ast[ssp].child(child::SS_STATES)));
        for scope in scopes {
            for vp in vars(ast, Some(ast.var_list_from_scope(scope))) {
                if let Some(init) = decl_init(ast, vp) {
                    g.var_init(vp, init, &mut body);
                }
            }
        }
    }
    Ok(Fn {
        public: false,
        name: NM_INIT.into(),
        params: vec![param(g.uses.var, "seqg_var", "&seqg_vars")],
        ret: None,
        body,
    })
}

fn decl_init(ast: &Ast, vp: VarId) -> Option<NodeId> {
    ast[vp].decl.and_then(|d| ast[d].child(child::DECL_INIT))
}

/// The first built-in function call in `ep` in the order C writes code,
/// and the function's name.
fn first_builtin_call(ast: &Ast, ep: NodeId) -> Option<(NodeId, &'static str)> {
    let node = &ast[ep];
    if node.tag == NodeTag::EFunc
        && let Some(callee) = node.child(child::FUNC_EXPR)
        && let Extra::Builtin(fi) = ast[callee].extra
    {
        return Some((ep, FUNC_SYMBOLS[fi].name));
    }
    node.children
        .iter()
        .flat_map(|&c| ast.list(c))
        .find_map(|c| first_builtin_call(ast, c))
}

/// `name: ty`, the name `_`-prefixed when the body does not use it.
fn param(used: bool, name: &str, ty: &str) -> Param {
    Param {
        name: if used {
            name.into()
        } else {
            format!("_{name}")
        },
        ty: ty.into(),
    }
}

/// C `gen_entex_body` in a function of its own: a state's or the
/// program's entry or exit function.
fn gen_entex_func(snc: &mut Snc, ast: &Ast, name: String, xp: NodeId) -> Fn {
    let mut g = Gen::new(snc, ast, Context::Ss);
    let block = ast[xp].child(child::ENTEX_BLOCK).expect("entry/exit block");
    let body = g.block_body(block, &None);
    let body = g.finish(body);
    Fn {
        public: false,
        name,
        params: vec![g.env_param()],
        ret: None,
        body,
    }
}

/// C `gen_event_body`: for each transition of a state, the test of its
/// condition, which on success names the transition and its next state.
fn gen_event_func(snc: &mut Snc, ast: &Ast, name: String, whens: Option<NodeId>) -> Fn {
    let mut g = Gen::new(snc, ast, Context::Cond);
    let mut body = Vec::new();
    let mut ptrn = false;
    for (trans_num, tp) in ast.list(whens).enumerate() {
        ptrn = true;
        let cond = ast[tp].child(child::WHEN_COND);
        g.comment(cond.unwrap_or(tp), &None, &mut body);
        let test = match cond {
            Some(c) => g.truth(c),
            None => Expr::atom("true"),
        };
        let Extra::When(when) = &ast[tp].extra else {
            unreachable!("when without transition data");
        };
        let mut then = Vec::new();
        match when.next_state {
            // "when(...) {...} exit" -> exit from program
            None => then.push(Stmt::Expr(g.env().method("exit", Vec::new()))),
            Some(sp) => {
                g.uses.pnst = true;
                then.push(Stmt::Expr(Expr::assign(
                    Expr::unary("*", Expr::atom("seqg_pnst")),
                    "=",
                    Expr::Atom(state_index(ast, sp).to_string()),
                )));
            }
        }
        then.push(Stmt::Expr(Expr::assign(
            Expr::unary("*", Expr::atom("seqg_ptrn")),
            "=",
            Expr::Atom(trans_num.to_string()),
        )));
        then.push(Stmt::Expr(Expr::Return(Some(Box::new(Expr::atom("true"))))));
        body.push(Stmt::Expr(Expr::If(Box::new(test), then, None)));
    }
    body.push(Stmt::Tail(Expr::atom("false")));
    let body = g.finish(body);
    Fn {
        public: false,
        name,
        params: vec![
            g.env_param(),
            param(ptrn, "seqg_ptrn", "&mut usize"),
            param(g.uses.pnst, "seqg_pnst", "&mut usize"),
        ],
        ret: Some("bool".into()),
        body,
    }
}

/// C `gen_action_body`: the actions of each transition of a state, by
/// transition number.
fn gen_action_func(snc: &mut Snc, ast: &Ast, name: String, whens: Option<NodeId>) -> Fn {
    let mut g = Gen::new(snc, ast, Context::Trans);
    let mut arms = Vec::new();
    for (trans_num, tp) in ast.list(whens).enumerate() {
        let block = ast[tp].child(child::WHEN_BLOCK).expect("when block");
        arms.push(Arm {
            pat: trans_num.to_string(),
            body: g.block_body(block, &None),
        });
    }
    arms.push(Arm {
        pat: "_".into(),
        body: Vec::new(),
    });
    let body = g.finish(vec![Stmt::Expr(Expr::Match(
        Box::new(Expr::atom("seqg_trn")),
        arms,
    ))]);
    Fn {
        public: false,
        name,
        params: vec![
            g.env_param(),
            param(true, "seqg_trn", "usize"),
            param(g.uses.pnst, "seqg_pnst", "&mut usize"),
        ],
        ret: None,
        body,
    }
}

fn state_index(ast: &Ast, sp: NodeId) -> u32 {
    match &ast[sp].extra {
        Extra::State(state) => state.index,
        _ => unreachable!("state without state data"),
    }
}

/// Expression context (C `enum context`). Certain nodes of the syntax
/// tree are interpreted differently depending on the context in which
/// they appear.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Context {
    /// when() condition
    Cond,
    /// state transition actions
    Trans,
    /// otherwise inside a state set
    Ss,
    /// inside a function definition
    Func,
    /// outside state sets and functions
    Global,
}

/// The type of a translated expression, as far as its Rust depends on it.
#[derive(Clone, Debug, PartialEq)]
enum Ty {
    /// an integer constant expression, typed by its context as a Rust
    /// integer literal is
    Int,
    /// a floating constant expression (`f64`, C's `double`)
    Float,
    /// a value of this numeric Rust type
    Num(String),
    Bool,
    /// a string literal
    Str,
    /// a `seqg_rt::Ptr` to this Rust type, C's `T *`
    Ptr(String),
    /// `&place`: a string, an array, a multi-PV array, or C's `&x`
    Ref,
    /// a value of this `seqg_rt` type
    Rt(&'static str),
    /// a value of a type snc-rs does not know: a foreign name, a foreign
    /// call's result
    Foreign,
    Unit,
}

/// A translated expression.
struct Val {
    e: Expr,
    ty: Ty,
}

/// What a place holds.
#[derive(Clone, Copy)]
enum PlaceTy {
    Snl(TypeId),
    /// a `char` that a `string`'s subscript, or a `char *` that is not a
    /// variable's, names
    Char,
    /// what a `void *` points to, which C does not read
    Void,
    /// a foreign global SNL code assigns, the companion's
    /// `LazyLock<SeqVar<T>>`
    Global(VarId),
    /// a member of a foreign struct or union, whose type the companion
    /// knows and snc-rs does not
    Member,
}

/// How C's `.` reaches a member of a struct or union.
#[derive(Clone, Copy, PartialEq)]
enum Members {
    /// by a reference into the struct, `field(|v| &mut v.a)`
    Struct,
    /// by the companion's accessors, `view(|v| v.a(), |v, x| v.set_a(x))`,
    /// as a union's members share its bytes
    Union,
}

/// A variable or an element of one: the expression naming its `SeqVar`
/// (or `At`).
struct Place {
    expr: Expr,
    ty: PlaceTy,
    /// a multi-PV array not subscripted, `[SeqVar<T>; N]`
    multi: bool,
}

#[derive(Default)]
struct Uses {
    var: bool,
    env: bool,
    pnst: bool,
}

/// The translation of one generated function.
struct Gen<'a> {
    snc: &'a mut Snc,
    ast: &'a Ast,
    cx: Context,
    /// whether the function has a `seqg_env`; `seqg_init` has none
    has_env: bool,
    uses: Uses,
    /// what a `break` or `continue` binds to, innermost last
    loops: Vec<Frame>,
    /// the result type of the function definition being translated, if
    /// it has one
    ret: Option<TypeId>,
    /// [`assigned_globals`]
    globals: BTreeSet<String>,
}

/// What a `break` or `continue` binds to.
#[derive(Clone)]
enum Frame {
    /// an SNL loop, with what a `continue` runs first: a `for` loop's
    /// iteration expression
    Loop(Vec<Stmt>),
    /// a loop in escaped C, which the companion expands around the SNL
    /// block; a Rust `continue` there skips what runs between the body and
    /// the next test, so only a loop with nothing there `continues`
    EscapedLoop { continues: bool },
    /// a `switch` in escaped C, which a Rust `break` cannot leave
    EscapedSwitch,
}

/// The frames escaped C `text` leaves open, outermost first: an `if`,
/// `else` or bare block binds neither jump in C, so it has none.
fn escaped_frames(text: &str) -> Vec<Frame> {
    let code = c_code(text);
    let mut open = Vec::new();
    let mut parens = Vec::new();
    // the last `(`...`)` closed, by index
    let mut closed = None;
    for (i, &b) in code.iter().enumerate() {
        match b {
            b'(' => parens.push(i),
            b')' => closed = parens.pop().map(|o| (o, i)),
            b'{' => open.push(escaped_frame(&code[..i], closed)),
            b'}' => {
                open.pop();
            }
            _ => {}
        }
    }
    open.into_iter().flatten().collect()
}

/// C text with its literals and comments blanked.
fn c_code(text: &str) -> Vec<u8> {
    let classes = c_classes(text.as_bytes());
    text.bytes()
        .zip(classes)
        .map(|(b, c)| if c == CClass::Code { b } else { b' ' })
        .collect()
}

/// Every `name (...)` in the program's escaped C: the code of its
/// statement before `name`, and the code between the parentheses.
fn escaped_fn_uses(ast: &Ast, name: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let name = name.as_bytes();
    let mut uses = Vec::new();
    for n in ast.nodes.iter().filter(|n| n.tag == NodeTag::TText) {
        let code = c_code(n.token.text());
        for i in 0..code.len() {
            if !code[i..].starts_with(name)
                || i > 0 && is_ident(code[i - 1])
                || code.get(i + name.len()).is_some_and(|&b| is_ident(b))
            {
                continue;
            }
            let rest = code[i + name.len()..].trim_ascii_start();
            if rest.first() != Some(&b'(') {
                continue;
            }
            let mut depth = 0;
            let close = rest.iter().position(|&b| {
                depth += match b {
                    b'(' => 1,
                    b')' => -1,
                    _ => 0,
                };
                depth == 0
            });
            if let Some(close) = close {
                let start = code[..i].iter().rposition(|b| b";{}".contains(b));
                let start = start.map_or(0, |s| s + 1);
                uses.push((code[start..i].to_vec(), rest[1..close].to_vec()));
            }
        }
    }
    uses
}

/// A function's declaration in the program's escaped C, in the Rust types
/// its keywords spell, each `None` where they spell none: the types the
/// companion's function takes and returns.
pub struct CDecl {
    pub ret: Option<&'static str>,
    /// `None` for `()`, which declares no prototype
    pub params: Option<Vec<Option<&'static str>>>,
    /// whether the prototype ends in `...`, whose arguments the call
    /// passes after `params` as one `&[CArg]` slice
    pub variadic: bool,
}

/// `name`'s declaration in the program's escaped C, by which gcc types and
/// converts a call: the result of its first declaration, the parameters of
/// its first prototype.
pub fn escaped_decl(ast: &Ast, name: &str) -> Option<CDecl> {
    let uses = escaped_fn_uses(ast, name);
    let mut decls = uses
        .iter()
        .filter_map(|(spec, params)| Some((decl_specs(spec)?, params)))
        .peekable();
    let ret = c_spec_type(&decls.peek()?.0);
    let (params, variadic) = decls
        .find_map(|(_, params)| c_params(params))
        .map_or((None, false), |(p, v)| (Some(p), v));
    Some(CDecl {
        ret,
        params,
        variadic,
    })
}

/// The foreign variables SNL code assigns (`x = e`, `x += e`, `x++`):
/// globals the escaped C defines, which the companion holds in a cell as
/// a `static x: LazyLock<SeqVar<T>>` rather than as a constant.
pub fn assigned_globals(ast: &Ast) -> BTreeSet<String> {
    let target = |n: &crate::types::Node| match n.tag {
        NodeTag::EBinop if is_assign(n.token.symbol) => n.child(child::BINOP_LEFT),
        NodeTag::EPre | NodeTag::EPost if matches!(n.token.symbol, Tok::Incr | Tok::Decr) => {
            n.child(0)
        }
        _ => None,
    };
    let mut globals = BTreeSet::new();
    for mut ep in ast.nodes.iter().filter_map(target) {
        while ast[ep].tag == NodeTag::EParen {
            ep = ast[ep].child(child::PAREN_EXPR).expect("operand");
        }
        if let (NodeTag::EVar, Extra::Var(vp)) = (ast[ep].tag, &ast[ep].extra)
            && ast[ast[*vp].type_].kind == TypeKind::None
        {
            globals.extend(ast[*vp].name.as_deref().map(String::from));
        }
    }
    globals
}

/// The Rust type of variable `name` by its first declaration in the
/// program's escaped C, when keywords spell an arithmetic one, and the
/// count of its array dimensions.
pub fn escaped_var_type(ast: &Ast, name: &str) -> Option<(&'static str, usize)> {
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let n = name.as_bytes();
    let texts = ast.nodes.iter().filter(|n| n.tag == NodeTag::TText);
    let decls = texts.flat_map(|node| {
        let code = c_code(node.token.text());
        (0..code.len())
            .filter(|&i| {
                code[i..].starts_with(n)
                    && (i == 0 || !is_ident(code[i - 1]))
                    && code.get(i + n.len()).is_none_or(|&b| !is_ident(b))
            })
            .filter_map(|i| var_decl(&code, i, n.len()))
            .collect::<Vec<_>>()
    });
    decls.into_iter().next().flatten()
}

/// The type of `ep`, a variable or an element of one, by the variable's
/// escaped-C declaration, applying `subscripts` more subscripts.
fn escaped_value_ty(ast: &Ast, ep: NodeId, subscripts: usize) -> Ty {
    let node = &ast[ep];
    match (node.tag, &node.extra) {
        (NodeTag::EVar, Extra::Var(vp)) if ast[ast[*vp].type_].kind == TypeKind::None => {
            match escaped_var_type(ast, ast[*vp].name.as_deref().unwrap_or_default()) {
                Some((t, dims)) if dims == subscripts => Ty::Num(t.into()),
                _ => Ty::Foreign,
            }
        }
        (NodeTag::ESubscr, _) => match node.child(child::SUBSCR_OPERAND) {
            Some(operand) => escaped_value_ty(ast, operand, subscripts + 1),
            None => Ty::Foreign,
        },
        (NodeTag::EParen, _) => match node.child(child::PAREN_EXPR) {
            Some(e) => escaped_value_ty(ast, e, subscripts),
            None => Ty::Foreign,
        },
        _ => Ty::Foreign,
    }
}

/// What `code` declares the name at `code[at..at + len]` as, if a variable
/// at its top level: its Rust type when keywords spell an arithmetic one,
/// and its array dimensions.
fn var_decl(code: &[u8], at: usize, len: usize) -> Option<Option<(&'static str, usize)>> {
    let depth = |s: &[u8], open, close| {
        s.iter()
            .map(|&b| (b == open) as i32 - (b == close) as i32)
            .sum::<i32>()
    };
    let start = code[..at]
        .iter()
        .rposition(|b| b";{}".contains(b))
        .map_or(0, |s| s + 1);
    let prefix = std::str::from_utf8(&code[start..at]).ok()?;
    if depth(&code[..at], b'{', b'}') != 0 || depth(prefix.as_bytes(), b'(', b')') != 0 {
        return None;
    }
    let mut rest = code[at + len..].trim_ascii_start();
    let mut dims = 0;
    while rest.first() == Some(&b'[') {
        rest = rest[rest.iter().position(|&b| b == b']')? + 1..].trim_ascii_start();
        dims += 1;
    }
    if !matches!(rest.first(), Some(b'=' | b',' | b';')) || rest.starts_with(b"==") {
        return None;
    }
    // the specifiers, then the declarators before this one
    let (spec, own) = match prefix.rsplit_once(',') {
        Some((before, own)) => {
            let head = before.split(',').next().unwrap_or_default();
            let words: Vec<&str> = head
                .split_whitespace()
                .take_while(|w| C_QUALIFIERS.contains(w) || C_TYPE_WORDS.contains(w))
                .collect();
            (Some(words).filter(|w| !w.is_empty())?, own)
        }
        None => (decl_specs(prefix.as_bytes())?, ""),
    };
    let pointer = own.contains('*') || spec.iter().any(|w| w.contains('*'));
    let ty = c_spec_type(&spec).filter(|&t| t != "()" && !pointer);
    Some(ty.map(|t| (t, dims)))
}

/// The words of the declaration specifiers before a declarator, or `None`
/// when `spec`, the code of its statement before the name, is not that: a
/// call's has an operator, a parenthesis or a statement keyword.
fn decl_specs(spec: &[u8]) -> Option<Vec<&str>> {
    let spec = std::str::from_utf8(spec).ok()?;
    let words: Vec<&str> = spec
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(str::split_whitespace)
        .collect();
    let spec_word = |w: &&str| {
        w.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_*".contains(&b))
            && !matches!(*w, "return" | "else" | "do" | "goto" | "sizeof" | "typedef")
    };
    (!words.is_empty() && words.iter().all(spec_word)).then_some(words)
}

/// The Rust type of each parameter a parameter list declares before any
/// `...`, and whether it ends in one; `None` for `()`, which declares no
/// prototype.
fn c_params(params: &[u8]) -> Option<(Vec<Option<&'static str>>, bool)> {
    let params = std::str::from_utf8(params).ok()?.trim();
    let mut depth = 0;
    let top_comma = |c: char| {
        depth += match c {
            '(' => 1,
            ')' => -1,
            _ => 0,
        };
        c == ',' && depth == 0
    };
    match params {
        "" => None,
        "void" => Some((Vec::new(), false)),
        _ => {
            let mut params: Vec<&str> = params.split(top_comma).collect();
            let variadic = params.last().is_some_and(|p| p.trim() == "...");
            if variadic {
                params.pop();
            }
            Some((params.into_iter().map(c_param_type).collect(), variadic))
        }
    }
}

/// The Rust type of a parameter declaration of an arithmetic type: a
/// pointer, an array or a function is passed as it is.
fn c_param_type(param: &str) -> Option<&'static str> {
    if param.contains(['*', '[', '(']) {
        return None;
    }
    let mut words: Vec<&str> = param.split_whitespace().collect();
    // the parameter's name
    if words
        .last()
        .is_some_and(|w| !C_QUALIFIERS.contains(w) && !C_TYPE_WORDS.contains(w))
    {
        words.pop();
    }
    c_spec_type(&words).filter(|&t| t != "()")
}

const C_QUALIFIERS: [&str; 6] = [
    "static", "extern", "inline", "register", "const", "volatile",
];

const C_TYPE_WORDS: [&str; 9] = [
    "signed", "unsigned", "int", "short", "long", "char", "float", "double", "void",
];

/// The Rust type C declaration specifiers name, if they are keywords of an
/// arithmetic type or `void`.
fn c_spec_type(words: &[&str]) -> Option<&'static str> {
    let mut words: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| !C_QUALIFIERS.contains(w))
        .collect();
    if words.is_empty() {
        return None;
    }
    let (signed, unsigned) = (words.contains(&"signed"), words.contains(&"unsigned"));
    words.retain(|w| !matches!(*w, "signed" | "unsigned" | "int"));
    Some(match (words.as_slice(), unsigned) {
        ([], false) => "i32",
        ([], true) => "u32",
        (["short"], false) => "i16",
        (["short"], true) => "u16",
        (["long"] | ["long", "long"], false) => "i64",
        (["long"] | ["long", "long"], true) => "u64",
        (["char"], true) => "u8",
        (["char"], false) if signed => "i8",
        (["char"], false) => "std::ffi::c_char",
        (["float"], false) => "f32",
        (["double"], false) => "f64",
        (["void"], false) => "()",
        _ => return None,
    })
}

/// The frame of the construct whose `{` follows `code`.
fn escaped_frame(code: &[u8], closed: Option<(usize, usize)>) -> Option<Frame> {
    let trim = |s: &[u8]| s.trim_ascii_end().len();
    let word = |s: &[u8]| {
        let s = s.trim_ascii_end();
        let start = s
            .iter()
            .rposition(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
            .map_or(0, |p| p + 1);
        s[start..].to_vec()
    };
    match closed {
        Some((open, close)) if close + 1 == trim(code) => match &word(&code[..open])[..] {
            b"while" => Some(Frame::EscapedLoop { continues: true }),
            b"for" => {
                let iter = code[open + 1..close].rsplit(|&b| b == b';').next();
                let continues = iter.is_some_and(|s| s.trim_ascii().is_empty());
                Some(Frame::EscapedLoop { continues })
            }
            b"switch" => Some(Frame::EscapedSwitch),
            _ => None,
        },
        _ if word(code) == b"do" => Some(Frame::EscapedLoop { continues: false }),
        _ => None,
    }
}

impl<'a> Gen<'a> {
    fn new(snc: &'a mut Snc, ast: &'a Ast, cx: Context) -> Self {
        Gen {
            snc,
            ast,
            cx,
            has_env: true,
            uses: Uses::default(),
            loops: Vec::new(),
            ret: None,
            globals: assigned_globals(ast),
        }
    }

    fn env(&mut self) -> Expr {
        self.uses.env = true;
        Expr::atom("seqg_env")
    }

    /// `body` after `let seqg_var = seqg_env.var();` if it uses it.
    fn finish(&mut self, mut body: Vec<Stmt>) -> Vec<Stmt> {
        if self.uses.var {
            let var = self.env().method("var", Vec::new());
            body.insert(
                0,
                Stmt::Let {
                    pat: "seqg_var".into(),
                    ty: None,
                    init: Some(var),
                },
            );
        }
        body
    }

    fn env_param(&self) -> Param {
        param(self.uses.env, "seqg_env", ENV_TYPE)
    }

    /// The `// file:line` comment of `ep`, unless the code around it
    /// carries that line already (`ctx`).
    fn comment(&self, ep: NodeId, ctx: &Option<String>, out: &mut Vec<Stmt>) {
        let line = line_comment(self.snc, &self.ast[ep]);
        if let Some(line) = line.filter(|l| Some(l) != ctx.as_ref()) {
            out.push(Stmt::Comment(line));
        }
    }

    /// C `gen_var_init`: the initializer of `vp`, after its line comment.
    fn var_init(&mut self, vp: VarId, init: NodeId, out: &mut Vec<Stmt>) {
        self.comment(init, &None, out);
        let place = self.var_place(vp);
        self.init_place(place, init, out);
    }

    /// Store initializer `init` in `place`, an `E_INIT` element by element.
    fn init_place(&mut self, place: Place, init: NodeId, out: &mut Vec<Stmt>) {
        let ast = self.ast;
        if ast[init].tag == NodeTag::EInit
            && let PlaceTy::Snl(t) = place.ty
            && self.members(place.ty).is_some()
        {
            // the companion's `From` takes C's braces as nested tuples
            let ty = value_type(ast, t).expect("a foreign type");
            let v = Expr::call(format!("{ty}::from"), vec![self.init_tuple(init)]);
            out.push(Stmt::Expr(place.expr.method("set", vec![v])));
            return;
        }
        if ast[init].tag == NodeTag::EInit {
            let elem = self.elem_kind(place.ty, place.multi);
            for (i, ep) in ast.list(ast[init].child(child::INIT_ELEMS)).enumerate() {
                let Some(elem) = elem else {
                    // C's compiler rejects braces around a scalar's value
                    let v = self.expr(ep);
                    out.push(Stmt::Expr(place.expr.clone().method("set", vec![v.e])));
                    continue;
                };
                let index = Val {
                    e: Expr::Atom(i.to_string()),
                    ty: Ty::Int,
                };
                let sub = index_place(&place, elem, index);
                self.init_place(sub, ep, out);
            }
            return;
        }
        let is_text = match place.ty {
            PlaceTy::Snl(t) => match &ast[t].kind {
                TypeKind::Prim(PrimType::String) => true,
                TypeKind::Array { elem_type, .. } => {
                    ast[*elem_type].kind == TypeKind::Prim(PrimType::Char)
                }
                _ => false,
            },
            PlaceTy::Char | PlaceTy::Void | PlaceTy::Global(_) | PlaceTy::Member => false,
        };
        let store = if is_text && !place.multi {
            let v = self.expr(init);
            Expr::call(
                "seqg_rt::c::strcpy",
                vec![Expr::unary("&", place.expr), v.e],
            )
        } else {
            let ty = self.place_value_ty(&place);
            let v = self.stored(init, &ty);
            place.expr.method("set", vec![v])
        };
        out.push(Stmt::Expr(store));
    }

    /// A foreign struct or union initializer `ep`, a brace a tuple.
    fn init_tuple(&mut self, ep: NodeId) -> Expr {
        let ast = self.ast;
        if ast[ep].tag != NodeTag::EInit {
            return self.arg_value(ep);
        }
        let elems = ast.list(ast[ep].child(child::INIT_ELEMS));
        Expr::Tuple(elems.map(|e| self.init_tuple(e).into()).collect())
    }

    /// C `gen_block` without the braces: the block's local variables, its
    /// escaped C definitions and its statements. In a function body C's
    /// definitions run as statements do, so they are the head of the
    /// statement list, with the env and joined to a later fragment that
    /// closes their brackets.
    fn block_body(&mut self, cmpnd: NodeId, ctx: &Option<String>) -> Vec<Stmt> {
        let ast = self.ast;
        let mut out = Vec::new();
        self.local_decls(cmpnd, ctx, &mut out);
        let mut list: Vec<NodeId> = ast
            .list(ast.defn_list_from_scope(cmpnd))
            .filter(|&ep| ast[ep].tag == NodeTag::TText)
            .collect();
        if !list.is_empty() {
            out.push(Stmt::Comment("C code definitions".into()));
        }
        list.extend(ast.list(ast[cmpnd].child(child::CMPND_STMTS)));
        out.extend(self.stmts(&list, ctx));
        out
    }

    /// C `gen_local_var_decls`: a `let` for each variable of block `scope`,
    /// initialized.
    fn local_decls(&mut self, scope: NodeId, ctx: &Option<String>, out: &mut Vec<Stmt>) {
        let ast = self.ast;
        for vp in vars(ast, Some(ast.var_list_from_scope(scope))) {
            let var = &ast[vp];
            let decl = var.decl.expect("local variables are declared");
            self.comment(decl, ctx, out);
            let ty = Some(gen_var_decl(self.snc, ast, vp));
            let pat = var.name.as_deref().unwrap_or_default().to_owned();
            let zeroed = Expr::call("seqg_rt::Zeroed::zeroed", Vec::new());
            let init = ast[decl].child(child::DECL_INIT);
            let scalar = match &ast[var.type_].kind {
                TypeKind::Prim(p) => *p != PrimType::String,
                TypeKind::Pointer { .. } => true,
                _ => false,
            };
            match init {
                Some(init) if scalar && ast[init].tag != NodeTag::EInit => {
                    let value = self.stored(init, &self.value_ty(var.type_));
                    out.push(Stmt::Let {
                        pat,
                        ty,
                        init: Some(Expr::call("seqg_rt::SeqVar::new", vec![value])),
                    });
                }
                _ => {
                    out.push(Stmt::Let {
                        pat,
                        ty,
                        init: Some(zeroed),
                    });
                    if let Some(init) = init {
                        let place = self.var_place(vp);
                        self.init_place(place, init, out);
                    }
                }
            }
        }
    }

    /// The statements `list`, each run of escaped C one `seqg_escaped!`
    /// call: fragments whose brackets balance only together are one call,
    /// with the SNL statements between them as block arguments.
    fn stmts(&mut self, list: &[NodeId], ctx: &Option<String>) -> Vec<Stmt> {
        let ast = self.ast;
        let mut out = Vec::new();
        let mut i = 0;
        while i < list.len() {
            if ast[list[i]].tag != NodeTag::TText {
                self.stmt(list[i], ctx, &mut out);
                i += 1;
                continue;
            }
            self.comment(list[i], ctx, &mut out);
            let run_ctx = line_comment(self.snc, &ast[list[i]]);
            let mut args = vec![Arg::from(self.env())];
            let mut balance = 0;
            let mut run_text = String::new();
            while i < list.len() {
                if ast[list[i]].tag == NodeTag::TText {
                    let text = ast[list[i]].token.text();
                    balance += c_bracket_balance(text);
                    args.push(escaped_lit(&escaped_key(text)).into());
                    run_text.push_str(text);
                    run_text.push('\n');
                    i += 1;
                    if balance == 0 {
                        break;
                    }
                } else {
                    let depth = self.loops.len();
                    self.loops.extend(escaped_frames(&run_text));
                    let mut block = Vec::new();
                    while i < list.len() && ast[list[i]].tag != NodeTag::TText {
                        self.stmt(list[i], &run_ctx, &mut block);
                        i += 1;
                    }
                    self.loops.truncate(depth);
                    args.push(Expr::Block(block).into());
                }
            }
            out.push(Stmt::Expr(Expr::Macro("seqg_escaped".into(), args)));
        }
        out
    }

    /// C `gen_expr` for a statement: its Rust after its line comment.
    fn stmt(&mut self, sp: NodeId, ctx: &Option<String>, out: &mut Vec<Stmt>) {
        let ast = self.ast;
        let node = &ast[sp];
        let c = |i| node.child(i).expect("statement part");
        match node.tag {
            NodeTag::SStmt => {
                // an empty statement is dropped
                if let Some(e) = node.child(child::STMT_EXPR) {
                    self.comment(sp, ctx, out);
                    self.expr_stmt(e, out);
                }
            }
            NodeTag::SCmpnd => {
                self.comment(sp, ctx, out);
                let line = line_comment(self.snc, node);
                let body = self.block_body(sp, &line);
                out.push(Stmt::Expr(Expr::Block(body)));
            }
            NodeTag::SIf => {
                self.comment(sp, ctx, out);
                let e = self.if_expr(sp);
                out.push(Stmt::Expr(e));
            }
            NodeTag::SWhile => {
                self.comment(sp, ctx, out);
                let line = line_comment(self.snc, node);
                let cond = self.truth(c(child::WHILE_COND));
                self.loops.push(Frame::Loop(Vec::new()));
                let body = self.branch(c(child::WHILE_STMT), &line);
                self.loops.pop();
                out.push(Stmt::Expr(if is_true(&cond) {
                    Expr::Loop(body)
                } else {
                    Expr::While(Box::new(cond), body)
                }));
            }
            NodeTag::SFor => {
                self.comment(sp, ctx, out);
                let line = line_comment(self.snc, node);
                if let Some(init) = node.child(child::FOR_INIT) {
                    self.expr_stmt(init, out);
                }
                let cond = node.child(child::FOR_COND).map(|cond| self.truth(cond));
                let cond = cond.filter(|c| !is_true(c));
                let mut iter = Vec::new();
                if let Some(it) = node.child(child::FOR_ITER) {
                    self.expr_stmt(it, &mut iter);
                }
                self.loops.push(Frame::Loop(iter.clone()));
                let mut body = self.branch(c(child::FOR_STMT), &line);
                self.loops.pop();
                body.extend(iter);
                out.push(Stmt::Expr(match cond {
                    Some(cond) => Expr::While(Box::new(cond), body),
                    None => Expr::Loop(body),
                }));
            }
            NodeTag::SJump => {
                let is_continue = node.token.symbol == Tok::Continue;
                let frame = if is_continue {
                    // C's continue passes through a switch
                    let mut loops = self.loops.iter().rev();
                    loops.find(|f| !matches!(f, Frame::EscapedSwitch)).cloned()
                } else {
                    self.loops.last().cloned()
                };
                self.comment(sp, ctx, out);
                let jump = match frame {
                    Some(Frame::Loop(iter)) if is_continue => {
                        out.extend(iter);
                        Expr::Continue
                    }
                    Some(Frame::EscapedLoop { continues: false }) if is_continue => {
                        self.refuse(
                            sp,
                            "a continue of an escaped C loop that runs code before its next test",
                        );
                        return;
                    }
                    Some(Frame::Loop(_) | Frame::EscapedLoop { .. }) if is_continue => {
                        Expr::Continue
                    }
                    Some(Frame::EscapedSwitch) => {
                        self.refuse(sp, "a break out of an escaped C switch");
                        return;
                    }
                    Some(_) => Expr::Break,
                    // C's action function runs a when block as a case of
                    // switch (seqg_trn), which break leaves
                    None if !is_continue && self.cx == Context::Trans => Expr::Return(None),
                    None => {
                        let what = if is_continue {
                            "continue statement not within a loop"
                        } else {
                            "break statement not within loop or switch"
                        };
                        self.snc.error_at_node(node, format_args!("{what}\n"));
                        return;
                    }
                };
                out.push(Stmt::Expr(jump));
            }
            NodeTag::SChange => {
                if self.cx != Context::Trans {
                    self.snc.error_at_node(
                        node,
                        format_args!("state change statement not allowed here\n"),
                    );
                    return;
                }
                self.comment(sp, ctx, out);
                let Extra::Change(target) = node.extra else {
                    unreachable!("state change without target");
                };
                self.uses.pnst = true;
                out.push(Stmt::Expr(Expr::assign(
                    Expr::unary("*", Expr::atom("seqg_pnst")),
                    "=",
                    Expr::Atom(state_index(ast, target).to_string()),
                )));
                out.push(Stmt::Expr(Expr::Return(None)));
            }
            NodeTag::SReturn => {
                if self.cx != Context::Func {
                    self.snc
                        .error_at_node(node, format_args!("return statement not allowed here\n"));
                    return;
                }
                self.comment(sp, ctx, out);
                let value = node.child(child::RETURN_EXPR).map(|e| {
                    Box::new(match self.ret {
                        Some(t) => self.stored(e, &self.value_ty(t)),
                        None => self.expr(e).e,
                    })
                });
                out.push(Stmt::Expr(Expr::Return(value)));
            }
            NodeTag::TText => out.extend(self.stmts(&[sp], ctx)),
            tag => unreachable!("statement {}", tag.name()),
        }
    }

    /// An `if` statement; an `else if` chains.
    fn if_expr(&mut self, sp: NodeId) -> Expr {
        let ast = self.ast;
        let node = &ast[sp];
        let line = line_comment(self.snc, node);
        let cond = self.truth(node.child(child::IF_COND).expect("if condition"));
        let then = self.branch(node.child(child::IF_THEN).expect("if body"), &line);
        let els = match node.child(child::IF_ELSE) {
            Some(e) if ast[e].tag == NodeTag::SIf => Some(Box::new(self.if_expr(e))),
            Some(e) => Some(Box::new(Expr::Block(self.branch(e, &line)))),
            None => None,
        };
        Expr::If(Box::new(cond), then, els)
    }

    /// The body of an `if`, `while` or `for`: a block's body, or the one
    /// statement.
    fn branch(&mut self, sp: NodeId, ctx: &Option<String>) -> Vec<Stmt> {
        if self.ast[sp].tag == NodeTag::SCmpnd {
            self.block_body(sp, ctx)
        } else {
            self.stmts(&[sp], ctx)
        }
    }

    /// An expression evaluated for its effect.
    fn expr_stmt(&mut self, ep: NodeId, out: &mut Vec<Stmt>) {
        let ast = self.ast;
        let node = &ast[ep];
        let c = |i| node.child(i).expect("expression operand");
        let sym = node.token.symbol;
        match node.tag {
            NodeTag::EParen => return self.expr_stmt(c(child::PAREN_EXPR), out),
            NodeTag::EBinop if sym == Tok::Comma => {
                self.expr_stmt(c(child::BINOP_LEFT), out);
                self.expr_stmt(c(child::BINOP_RIGHT), out);
                return;
            }
            NodeTag::EBinop if is_assign(sym) => return self.assign_stmt(ep, out),
            NodeTag::EPre | NodeTag::EPost if matches!(sym, Tok::Incr | Tok::Decr) => {
                return self.incdec_stmt(c(0), sym, out);
            }
            NodeTag::ETernop => {
                let cond = self.truth(c(child::TERNOP_COND));
                let mut then = Vec::new();
                self.expr_stmt(c(child::TERNOP_THEN), &mut then);
                let mut els = Vec::new();
                self.expr_stmt(c(child::TERNOP_ELSE), &mut els);
                out.push(Stmt::Expr(Expr::If(
                    Box::new(cond),
                    then,
                    Some(Box::new(Expr::Block(els))),
                )));
                return;
            }
            _ => {}
        }
        let v = self.expr(ep);
        out.push(match v.e {
            e @ (Expr::Call(..)
            | Expr::Method(..)
            | Expr::Macro(..)
            | Expr::Block(..)
            | Expr::If(..)) => Stmt::Expr(e),
            e => Stmt::Let {
                pat: "_".into(),
                ty: None,
                init: Some(e),
            },
        });
    }

    /// An assignment statement: `set` for `=`, `modify` for a compound
    /// assignment, its operand evaluated before the lock is taken.
    fn assign_stmt(&mut self, ep: NodeId, out: &mut Vec<Stmt>) {
        let ast = self.ast;
        let node = &ast[ep];
        let op = node.token.symbol;
        let lhs = node.child(child::BINOP_LEFT).expect("assignment target");
        let rhs = node.child(child::BINOP_RIGHT).expect("assigned value");
        let Some(place) = self.store_place(lhs, op) else {
            self.expr(rhs);
            return;
        };
        let ty = self.place_value_ty(&place);
        if op == Tok::Equal {
            let v = self.stored(rhs, &ty);
            out.push(Stmt::Expr(place.expr.method("set", vec![v])));
            return;
        }
        let v = self.hoisted(rhs, out);
        let body = compound(op, &ty, v);
        out.push(Stmt::Expr(modify(place.expr, body)));
    }

    /// The place a store `op` (`=`, a compound assignment, `++` or `--`) to
    /// `target` writes: an SNL variable, an element of one, or what a
    /// pointer points to; `None` (refused) for anything else. The
    /// companion's globals are not written from SNL code.
    fn store_place(&mut self, target: NodeId, op: Tok) -> Option<Place> {
        if self.place_kind(target).is_none() {
            let mut ep = target;
            while self.ast[ep].tag == NodeTag::EParen {
                ep = self.ast[ep].child(child::PAREN_EXPR).expect("operand");
            }
            let node = &self.ast[ep];
            let through_pointer = match node.tag {
                NodeTag::EPre => node.token.symbol == Tok::Asterisk,
                NodeTag::ESelect => node.token.symbol == Tok::Pointer,
                _ => false,
            };
            if through_pointer {
                self.refuse(target, "assignment through a pointer");
            } else {
                self.refuse(target, "assignment to a foreign variable");
            }
            return None;
        }
        let place = self.place(target);
        let pointer_op = matches!(
            op,
            Tok::Equal | Tok::Addeq | Tok::Subeq | Tok::Incr | Tok::Decr
        );
        let ty = self.place_value_ty(&place);
        if !pointer_op && matches!(ty, Ty::Ptr(_)) {
            self.refuse(target, "pointer arithmetic");
        } else if op != Tok::Equal && ty == Ty::Ptr(C_VOID.into()) {
            self.refuse(target, "arithmetic on a void pointer");
        } else if op != Tok::Equal && matches!(ty, Ty::Rt(_)) {
            self.refuse(target, "arithmetic on an enum");
        }
        Some(place)
    }

    /// `ep` translated as a value stored into, or passed as, a `ty`: C's
    /// conversion on assignment.
    fn stored(&mut self, ep: NodeId, ty: &Ty) -> Expr {
        let v = match ty {
            Ty::Ptr(_) => self.pointer_val(ep).or_else(|| self.member_ptr(ep, ty)),
            _ => None,
        };
        let v = match v {
            Some(v) => v,
            None => self.expr(ep),
        };
        if matches!(ty, Ty::Rt(_)) && v.ty != *ty {
            // C's enum holds any int, a `seqg_rt` enum only its variants
            self.refuse(ep, "a conversion to an enum");
        }
        conv(v, ty)
    }

    /// `&s.a` of a member snc-rs does not know the type of, as the pointer
    /// `ty` its target is; `None` for anything else.
    fn member_ptr(&mut self, mut ep: NodeId, ty: &Ty) -> Option<Val> {
        let ast = self.ast;
        while ast[ep].tag == NodeTag::EParen {
            ep = ast[ep].child(child::PAREN_EXPR)?;
        }
        if ast[ep].tag != NodeTag::EPre || ast[ep].token.symbol != Tok::Ampersand {
            return None;
        }
        let mut operand = ast[ep].child(child::PRE_OPERAND)?;
        while ast[operand].tag == NodeTag::EParen {
            operand = ast[operand].child(child::PAREN_EXPR)?;
        }
        let (PlaceTy::Member, _) = self.place_kind(operand)? else {
            return None;
        };
        if self.holder(operand).and_then(|h| self.members(h)) == Some(Members::Union) {
            // a union member is a copy with no address
            self.refuse(ep, "a pointer to a union member");
        }
        let place = self.place(operand);
        Some(Val {
            e: place.expr.method("ptr", Vec::new()),
            ty: ty.clone(),
        })
    }

    /// What `ep` points to where C reads it as a pointer: a pointer, an
    /// array or `string` that decays, `&place`, a string literal, or
    /// arithmetic on one of them; `None` for anything else. Nothing is
    /// translated.
    fn pointee(&self, ep: NodeId) -> Option<PlaceTy> {
        let ast = self.ast;
        let node = &ast[ep];
        let c = |i| node.child(i);
        let sym = node.token.symbol;
        match node.tag {
            NodeTag::EParen => self.pointee(c(child::PAREN_EXPR)?),
            NodeTag::EString => Some(PlaceTy::Char),
            // `&g` of a foreign global is the place, as a foreign call's is
            NodeTag::EPre if sym == Tok::Ampersand => {
                match self.place_kind(c(child::PRE_OPERAND)?)? {
                    (PlaceTy::Global(_) | PlaceTy::Member, _) => None,
                    (t, _) => Some(t),
                }
            }
            NodeTag::EPre | NodeTag::EPost if matches!(sym, Tok::Incr | Tok::Decr) => {
                self.held_pointee(c(0)?)
            }
            NodeTag::EBinop if is_assign(sym) => self.held_pointee(c(child::BINOP_LEFT)?),
            NodeTag::EBinop if matches!(sym, Tok::Add | Tok::Sub) => {
                let l = self.pointee(c(child::BINOP_LEFT)?);
                let r = self.pointee(c(child::BINOP_RIGHT)?);
                match (l, r) {
                    (Some(t), None) => Some(t),
                    (None, Some(t)) if sym == Tok::Add => Some(t),
                    _ => None,
                }
            }
            NodeTag::ETernop => composite(
                self.pointee(c(child::TERNOP_THEN)?),
                self.pointee(c(child::TERNOP_ELSE)?),
            ),
            NodeTag::ECast => {
                let decl = c(child::CAST_TYPE)?;
                match ast[ast[ast[decl].e_decl()?].type_].kind {
                    TypeKind::Pointer { value_type } => self.snl_pointee(value_type),
                    _ => None,
                }
            }
            NodeTag::EFunc => self.result_pointee(c(child::FUNC_EXPR)?),
            _ => {
                let (ty, multi) = self.place_kind(ep)?;
                if let PlaceTy::Snl(t) = ty
                    && let TypeKind::Pointer { value_type } = ast[t].kind
                {
                    return self.snl_pointee(value_type);
                }
                self.elem_kind(ty, multi).map(|(t, _)| t)
            }
        }
    }

    /// What the pointer the place `ep` holds points to.
    fn held_pointee(&self, ep: NodeId) -> Option<PlaceTy> {
        match self.place_kind(ep)? {
            (PlaceTy::Snl(t), _) => match self.ast[t].kind {
                TypeKind::Pointer { value_type } => self.snl_pointee(value_type),
                _ => None,
            },
            (PlaceTy::Char | PlaceTy::Void | PlaceTy::Global(_) | PlaceTy::Member, _) => None,
        }
    }

    /// A pointee of SNL type `t`, if a `seqg_rt::Ptr` can point to it.
    fn snl_pointee(&self, t: TypeId) -> Option<PlaceTy> {
        match self.ast[t].kind {
            TypeKind::Void => Some(PlaceTy::Void),
            _ => value_type(self.ast, t).ok().map(|_| PlaceTy::Snl(t)),
        }
    }

    /// What the result of a call to `callee` points to.
    fn result_pointee(&self, callee: NodeId) -> Option<PlaceTy> {
        let ast = self.ast;
        let char_ptr = |ty: Ty| (ty == Ty::Ptr(C_CHAR.into())).then_some(PlaceTy::Char);
        match (ast[callee].tag, &ast[callee].extra) {
            (NodeTag::EBuiltin, Extra::Builtin(fi)) => {
                char_ptr(builtin_type(FUNC_SYMBOLS[*fi].name))
            }
            (NodeTag::EVar, Extra::Var(vp)) => match ast[ast[*vp].type_].kind {
                TypeKind::Function { return_type, .. } => match ast[return_type].kind {
                    TypeKind::Pointer { value_type } => self.snl_pointee(value_type),
                    _ => None,
                },
                TypeKind::None => char_ptr(prototype(ast[callee].token.text())?.1),
                _ => None,
            },
            _ => None,
        }
    }

    /// The Rust type of what a pointer to `t` points to.
    fn pointee_type(&self, t: PlaceTy) -> String {
        match t {
            PlaceTy::Snl(t) => value_type(self.ast, t).expect("a pointee has a Rust type"),
            PlaceTy::Char => C_CHAR.into(),
            PlaceTy::Void => C_VOID.into(),
            PlaceTy::Global(_) | PlaceTy::Member => {
                unreachable!("a foreign global or member is no pointee")
            }
        }
    }

    /// [`Gen::pointer`] of `ep` and its type, if [`Gen::pointee`] reads it
    /// as a pointer.
    fn pointer_val(&mut self, ep: NodeId) -> Option<Val> {
        let ty = Ty::Ptr(self.pointee_type(self.pointee(ep)?));
        Some(Val {
            e: self.pointer(ep),
            ty,
        })
    }

    /// `ep`, which [`Gen::pointee`] reads as a pointer, as a `seqg_rt::Ptr`.
    /// `&x` is its place's `ptr()`, and an array or a `string` decays with
    /// `Ptr::from` to its first element: both point into the variable's
    /// cell and see its later writes, as C's do. `p + n` is `p.offset(n)`.
    fn pointer(&mut self, ep: NodeId) -> Expr {
        let ast = self.ast;
        let node = &ast[ep];
        let c = |i| node.child(i).expect("expression operand");
        let sym = node.token.symbol;
        let ty = Ty::Ptr(self.pointee_type(self.pointee(ep).expect("a pointer")));
        match node.tag {
            NodeTag::EParen => self.pointer(c(child::PAREN_EXPR)),
            NodeTag::EPre if sym == Tok::Ampersand => {
                let operand = c(child::PRE_OPERAND);
                if let Some((array, index)) = self.multi_elem(operand) {
                    // C lays the cells out as one array: `&v[i]` is `v + i`
                    let v = self.pointer(array);
                    let n = self.expr(index);
                    return v.method("offset", vec![offset(Tok::Add, n)]);
                }
                let place = self.place(operand);
                if place.multi {
                    // a `T (*)[N]`, and no `[T; N]` holds the cells
                    self.refuse(ep, "a pointer to a multi-PV array");
                }
                place.expr.method("ptr", Vec::new())
            }
            NodeTag::EBinop if matches!(sym, Tok::Add | Tok::Sub) => {
                let (l, r) = (c(child::BINOP_LEFT), c(child::BINOP_RIGHT));
                let (p, n) = if self.pointee(l).is_some() {
                    (l, r)
                } else {
                    (r, l)
                };
                if let Some(PlaceTy::Void) = self.pointee(p) {
                    self.refuse(ep, "arithmetic on a void pointer");
                }
                let p = self.pointer(p);
                let n = self.expr(n);
                p.method("offset", vec![offset(sym, n)])
            }
            NodeTag::ETernop => {
                let cond = self.truth(c(child::TERNOP_COND));
                let a = self.stored(c(child::TERNOP_THEN), &ty);
                let b = self.stored(c(child::TERNOP_ELSE), &ty);
                Expr::If(
                    Box::new(cond),
                    vec![Stmt::Tail(a)],
                    Some(Box::new(Expr::Block(vec![Stmt::Tail(b)]))),
                )
            }
            NodeTag::ECast => {
                let operand = c(child::CAST_OPERAND);
                let v = match self.pointer_val(operand) {
                    Some(v) => v,
                    None => self.expr(operand),
                };
                let void = Ty::Ptr(C_VOID.into());
                match &v.ty {
                    // C converts any pointer to and from `void *`
                    Ty::Ptr(_) if v.ty != ty && v.ty != void && ty != void => {
                        self.refuse(ep, "a cast between pointer types");
                    }
                    _ if is_integer(&v.ty) && !is_null(&v) => {
                        self.refuse(ep, "a cast of an integer to a pointer");
                    }
                    _ => {}
                }
                conv(v, &ty)
            }
            _ if self.held_pointee(ep).is_none() && self.place_kind(ep).is_some() => {
                let place = self.place(ep);
                Expr::call("seqg_rt::Ptr::from", vec![Expr::unary("&", place.expr)])
            }
            _ => {
                let v = self.expr(ep);
                conv(v, &ty)
            }
        }
    }

    /// The multi-PV array and the index of `ep`, if it subscripts one.
    fn multi_elem(&self, ep: NodeId) -> Option<(NodeId, NodeId)> {
        let node = &self.ast[ep];
        match node.tag {
            NodeTag::EParen => self.multi_elem(node.child(child::PAREN_EXPR)?),
            NodeTag::ESubscr => {
                let array = node.child(child::SUBSCR_OPERAND)?;
                let (_, multi) = self.place_kind(array)?;
                multi.then_some((array, node.child(child::SUBSCR_INDEX)?))
            }
            _ => None,
        }
    }

    /// The `+` or `-` `ep` as C's pointer arithmetic: `p + n` and `p - n`
    /// a pointer, `p - q` the elements between; `None` when no operand is
    /// a pointer.
    fn pointer_arith(&mut self, ep: NodeId) -> Option<Val> {
        let node = &self.ast[ep];
        let l = node.child(child::BINOP_LEFT).expect("operand");
        let r = node.child(child::BINOP_RIGHT).expect("operand");
        if let Some(t) = self.pointee(ep) {
            let ty = Ty::Ptr(self.pointee_type(t));
            let e = self.pointer(ep);
            return Some(Val { e, ty });
        }
        if node.token.symbol != Tok::Sub || self.pointee(l).is_none() || self.pointee(r).is_none() {
            return None;
        }
        if let Some(PlaceTy::Void) = self.pointee(l) {
            self.refuse(ep, "arithmetic on a void pointer");
        }
        let p = self.pointer(l);
        let q = self.pointer(r);
        Some(Val {
            e: p.method("diff", vec![Expr::unary("&", q)]),
            ty: Ty::Num("isize".into()),
        })
    }

    fn refuse(&mut self, ep: NodeId, what: &str) {
        self.snc.error_at_node(
            &self.ast[ep],
            format_args!("snc-rs does not support {what}\n"),
        );
    }

    /// `rhs`, moved into `let seqg_rhs` first when evaluating it takes a
    /// lock, so no lock is taken under the target's.
    fn hoisted(&mut self, rhs: NodeId, out: &mut Vec<Stmt>) -> Val {
        let hoist = self.takes_locks(rhs);
        let v = self.expr(rhs);
        if !hoist {
            return v;
        }
        out.push(Stmt::Let {
            pat: "seqg_rhs".into(),
            ty: None,
            init: Some(v.e),
        });
        Val {
            e: Expr::atom("seqg_rhs"),
            ty: v.ty,
        }
    }

    /// Whether evaluating `ep` can lock a variable: it reads one, calls a
    /// function, or holds escaped C.
    fn takes_locks(&self, ep: NodeId) -> bool {
        let node = &self.ast[ep];
        match node.tag {
            NodeTag::EFunc | NodeTag::TText => true,
            NodeTag::EVar => {
                matches!(node.extra, Extra::Var(vp) if self.is_storage(vp) || self.is_global(vp))
            }
            _ => node.children.iter().flatten().any(|&c| self.takes_locks(c)),
        }
    }

    /// `x++` or `--x` as a statement.
    fn incdec_stmt(&mut self, operand: NodeId, op: Tok, out: &mut Vec<Stmt>) {
        let Some(place) = self.store_place(operand, op) else {
            return;
        };
        let ty = self.place_value_ty(&place);
        if !matches!(ty, Ty::Num(_) | Ty::Ptr(_) | Ty::Foreign) {
            out.push(Stmt::Expr(Expr::assign(
                place.expr,
                op_str(op),
                Expr::atom("1"),
            )));
            return;
        }
        out.push(Stmt::Expr(modify(place.expr, compound(op, &ty, int_one()))));
    }

    /// `x++`, `++x`, `x--` or `--x` as a value.
    fn incdec_value(&mut self, operand: NodeId, op: Tok, pre: bool) -> Val {
        let Some(place) = self.store_place(operand, op) else {
            return Val {
                e: Expr::atom("()"),
                ty: Ty::Foreign,
            };
        };
        let ty = self.place_value_ty(&place);
        if !matches!(ty, Ty::Num(_) | Ty::Ptr(_) | Ty::Foreign) {
            return raw_incdec(place.expr, op);
        }
        let step = Stmt::Expr(compound(op, &ty, int_one()));
        let body = if pre {
            vec![step, Stmt::Tail(current(&ty))]
        } else {
            vec![
                Stmt::Let {
                    pat: "seqg_old".into(),
                    ty: None,
                    init: Some(current(&ty)),
                },
                step,
                Stmt::Tail(Expr::atom("seqg_old")),
            ]
        };
        Val {
            e: modify(place.expr, Expr::Block(body)),
            ty,
        }
    }

    /// An assignment as a value: the value assigned, converted to the
    /// target's type.
    fn assign_value(&mut self, ep: NodeId) -> Val {
        let ast = self.ast;
        let node = &ast[ep];
        let op = node.token.symbol;
        let lhs = node.child(child::BINOP_LEFT).expect("assignment target");
        let rhs = node.child(child::BINOP_RIGHT).expect("assigned value");
        let Some(place) = self.store_place(lhs, op) else {
            return self.expr(rhs);
        };
        let ty = self.place_value_ty(&place);
        let mut stmts = Vec::new();
        if op == Tok::Equal {
            let stored = Expr::atom("seqg_rhs");
            let stored = if matches!(ty, Ty::Ptr(_)) {
                stored.method("clone", Vec::new())
            } else {
                stored
            };
            let v = self.stored(rhs, &ty);
            stmts.push(Stmt::Let {
                pat: "seqg_rhs".into(),
                ty: None,
                init: Some(v),
            });
            stmts.push(Stmt::Expr(place.expr.method("set", vec![stored])));
            stmts.push(Stmt::Tail(Expr::atom("seqg_rhs")));
        } else {
            let v = self.hoisted(rhs, &mut stmts);
            let body = Expr::Block(vec![
                Stmt::Expr(compound(op, &ty, v)),
                Stmt::Tail(current(&ty)),
            ]);
            stmts.push(Stmt::Tail(modify(place.expr, body)));
        }
        let e = match stmts.as_slice() {
            [Stmt::Tail(_)] => match stmts.pop() {
                Some(Stmt::Tail(e)) => e,
                _ => unreachable!(),
            },
            _ => Expr::Block(stmts),
        };
        Val { e, ty }
    }

    /// C `gen_expr` for an expression: its Rust and type.
    fn expr(&mut self, ep: NodeId) -> Val {
        let ast = self.ast;
        let node = &ast[ep];
        let c = |i| node.child(i).expect("expression operand");
        let sym = node.token.symbol;
        match node.tag {
            NodeTag::EVar | NodeTag::ESubscr | NodeTag::EPre | NodeTag::ESelect
                if self.place_kind(ep).is_some() =>
            {
                let place = self.place(ep);
                self.read(place)
            }
            NodeTag::EVar => {
                let Extra::Var(vp) = node.extra else {
                    unreachable!("unconnected E_VAR");
                };
                let name = ast[vp].name.as_deref().unwrap_or_default();
                let ty = match ast[ast[vp].type_].kind {
                    TypeKind::EvFlag => Ty::Rt("EfId"),
                    // seqCom.h:132-133 `#define ssId seqg_env`, `pVar seqg_var`
                    TypeKind::None if name == "ssId" => return self.implicit("seqg_env"),
                    TypeKind::None if name == "pVar" => return self.implicit("seqg_var"),
                    _ => escaped_value_ty(ast, ep, 0),
                };
                Val {
                    e: Expr::atom(name),
                    ty,
                }
            }
            NodeTag::ESubscr => {
                // an element of something that is neither an SNL array nor
                // a pointer
                let base = self.expr(c(child::SUBSCR_OPERAND));
                let index = self.expr(c(child::SUBSCR_INDEX));
                Val {
                    e: base.e.index(usize_index(index)),
                    ty: escaped_value_ty(ast, ep, 0),
                }
            }
            NodeTag::EConst => match node.extra {
                Extra::Const(i) => self.builtin_const(i),
                // C's `1.1f` is a float, which as a double is not 1.1
                _ if sym == Tok::Fpcon && node.token.text().ends_with(['f', 'F']) => Val {
                    e: Expr::Atom(format!("{}f32", float_lit(node.token.text()))),
                    ty: Ty::Num("f32".into()),
                },
                _ if sym == Tok::Fpcon => Val {
                    e: Expr::Atom(float_lit(node.token.text())),
                    ty: Ty::Float,
                },
                _ => int_lit(node.token.text()),
            },
            NodeTag::EString => Val {
                e: Expr::c_str(node.token.text()),
                ty: Ty::Str,
            },
            NodeTag::EParen => self.expr(c(child::PAREN_EXPR)),
            NodeTag::EFunc => self.call(ep),
            NodeTag::EBinop => {
                let (lhs, rhs) = (c(child::BINOP_LEFT), c(child::BINOP_RIGHT));
                match sym {
                    _ if is_assign(sym) => self.assign_value(ep),
                    Tok::Comma => {
                        let mut stmts = Vec::new();
                        self.expr_stmt(lhs, &mut stmts);
                        let r = self.expr(rhs);
                        stmts.push(Stmt::Tail(r.e));
                        Val {
                            e: Expr::Block(stmts),
                            ty: r.ty,
                        }
                    }
                    Tok::Andand | Tok::Oror => {
                        let l = self.truth(lhs);
                        let r = self.truth(rhs);
                        Val {
                            e: Expr::binary(l, op_str(sym), r),
                            ty: Ty::Bool,
                        }
                    }
                    Tok::Add | Tok::Sub if let Some(v) = self.pointer_arith(ep) => v,
                    // C compares the addresses an array or a `string` decays to
                    Tok::Eq | Tok::Ne
                        if let Some(t) = composite(self.pointee(lhs), self.pointee(rhs)) =>
                    {
                        let ty = Ty::Ptr(self.pointee_type(t));
                        let mut side = |ep| match self.pointer_val(ep) {
                            Some(v) => Val {
                                e: conv(v, &ty),
                                ty: ty.clone(),
                            },
                            None => self.expr(ep),
                        };
                        let l = side(lhs);
                        let r = side(rhs);
                        binop(sym, l, r)
                    }
                    _ => {
                        let l = self.expr(lhs);
                        let r = self.expr(rhs);
                        self.arith(ep, sym, l, r)
                    }
                }
            }
            NodeTag::EPre => {
                let operand = c(child::PRE_OPERAND);
                match sym {
                    Tok::Incr | Tok::Decr => self.incdec_value(operand, sym, true),
                    Tok::Ampersand => {
                        let e = if self.place_kind(operand).is_some() {
                            self.place(operand).expr
                        } else {
                            self.expr(operand).e
                        };
                        Val {
                            e: Expr::unary("&", e),
                            ty: Ty::Ref,
                        }
                    }
                    Tok::Asterisk => Val {
                        e: Expr::unary("*", self.expr(operand).e),
                        ty: Ty::Foreign,
                    },
                    Tok::Not => Val {
                        e: self.not(operand),
                        ty: Ty::Bool,
                    },
                    Tok::Tilde => {
                        let v = promoted(self.expr(operand));
                        Val {
                            e: Expr::unary("!", v.e),
                            ty: v.ty,
                        }
                    }
                    Tok::Sub => neg(self.expr(operand)),
                    Tok::Add => promoted(self.expr(operand)),
                    tok => unreachable!("prefix operator {tok:?}"),
                }
            }
            NodeTag::EPost => self.incdec_value(c(child::POST_OPERAND), sym, false),
            NodeTag::ETernop => {
                let cond = self.truth(c(child::TERNOP_COND));
                let a = self.expr(c(child::TERNOP_THEN));
                let b = self.expr(c(child::TERNOP_ELSE));
                let void = Ty::Ptr(C_VOID.into());
                let ptr = [&a.ty, &b.ty]
                    .into_iter()
                    .find(|t| **t == void)
                    .or_else(|| [&a.ty, &b.ty].into_iter().find(|t| matches!(t, Ty::Ptr(_))))
                    .cloned();
                // seqg_rt::arith::select takes two foreign operands of one
                // type, such as two structs, as they are
                let arith = |t: &Ty| is_numeric(t) || *t == Ty::Foreign;
                let (a, b, ty) = if arith(&a.ty) && arith(&b.ty) {
                    match operands(a, b) {
                        Operands::Known(a, b, ty) => (a, b, ty),
                        Operands::Unknown(a, b) => {
                            let lazy = |e| Expr::Closure(String::new(), Box::new(e));
                            return Val {
                                e: Expr::call(
                                    "seqg_rt::arith::select",
                                    vec![cond, lazy(a), lazy(b)],
                                ),
                                ty: Ty::Foreign,
                            };
                        }
                    }
                } else if let Some(ty) = ptr {
                    (conv(a, &ty), conv(b, &ty), ty)
                } else {
                    (a.e, b.e, a.ty)
                };
                Val {
                    e: Expr::If(
                        Box::new(cond),
                        vec![Stmt::Tail(a)],
                        Some(Box::new(Expr::Block(vec![Stmt::Tail(b)]))),
                    ),
                    ty,
                }
            }
            NodeTag::ECast => {
                let decl = c(child::CAST_TYPE);
                let t = ast[ast[decl].e_decl().expect("cast type")].type_;
                let v = self.expr(c(child::CAST_OPERAND));
                match &ast[t].kind {
                    TypeKind::Prim(p) if *p != PrimType::String => {
                        let tn = value_type(ast, t).expect("numeric type");
                        let e = cast_num(arith_operand(v), &tn);
                        Val { e, ty: Ty::Num(tn) }
                    }
                    TypeKind::Void => Val {
                        e: v.e,
                        ty: Ty::Unit,
                    },
                    _ => v,
                }
            }
            NodeTag::ESelect => {
                let l = self.expr(c(child::SELECT_LEFT));
                if matches!(l.ty, Ty::Rt(_)) {
                    self.refuse(ep, "a member of a seq run-time value");
                }
                let member = ast[c(child::SELECT_RIGHT)].token.text();
                Val {
                    e: l.e.field(member),
                    ty: Ty::Foreign,
                }
            }
            NodeTag::TText => {
                let env = self.env();
                let text = escaped_lit(&escaped_key(node.token.text()));
                Val {
                    e: Expr::Macro("seqg_escaped".into(), vec![env.into(), text.into()]),
                    ty: Ty::Foreign,
                }
            }
            NodeTag::EInit => {
                let elems = ast
                    .list(node.child(child::INIT_ELEMS))
                    .map(|e| self.expr(e).e.into())
                    .collect();
                Val {
                    e: Expr::Array(elems),
                    ty: Ty::Foreign,
                }
            }
            tag => unreachable!(
                "unhandled expression ({}:{})",
                tag.name(),
                node.token.text()
            ),
        }
    }

    /// A builtin constant.
    fn builtin_const(&mut self, i: usize) -> Val {
        let name = CONST_SYMBOLS[i].name;
        let rt = |path: String, ty| Val {
            e: Expr::Atom(path),
            ty: Ty::Rt(ty),
        };
        match name {
            "TRUE" | "FALSE" => Val {
                e: Expr::atom(if name == "TRUE" { "1" } else { "0" }),
                ty: Ty::Int,
            },
            "SYNC" => rt("seqg_rt::CompType::Sync".into(), "CompType"),
            "ASYNC" => rt("seqg_rt::CompType::Async".into(), "CompType"),
            "NOEVFLAG" => rt("seqg_rt::NOEVFLAG".into(), "EfId"),
            "pvSevrOK" => rt("seqg_rt::PvSevr::OK".into(), "PvSevr"),
            "seqg_var" | "seqg_env" => self.implicit(name),
            _ => match (name.strip_prefix("pvStat"), name.strip_prefix("pvSevr")) {
                (Some(s), _) => rt(format!("seqg_rt::PvStat::{}", camel(s)), "PvStat"),
                (_, Some(s)) => rt(format!("seqg_rt::PvSevr::{}", camel(s)), "PvSevr"),
                _ => unreachable!("builtin constant {name}"),
            },
        }
    }

    /// `seqg_var` or `seqg_env`, the variables and the state set C code
    /// reaches.
    fn implicit(&mut self, name: &str) -> Val {
        let e = if name == "seqg_env" {
            self.env()
        } else {
            self.uses.var = true;
            Expr::atom(name)
        };
        Val { e, ty: Ty::Foreign }
    }

    /// A function call: a builtin, `sizeof`, an SNL function, or a C
    /// function.
    fn call(&mut self, ep: NodeId) -> Val {
        let ast = self.ast;
        let node = &ast[ep];
        let callee = node.child(child::FUNC_EXPR).expect("callee");
        if ast[callee].tag == NodeTag::EBuiltin {
            return self.builtin_call(ep);
        }
        if ast[callee].tag == NodeTag::EVar && ast[callee].token.symbol == Tok::Sizeof {
            return self.size_of(ep);
        }
        let f = self.expr(callee);
        let args: Vec<NodeId> = ast.list(node.child(child::FUNC_ARGS)).collect();
        let mut out: Vec<Arg> = Vec::new();
        if type_is_function(self.snc, ast, callee) {
            // add arguments for implicit parameters
            out.push(self.env().into());
        }
        // an SNL function converts its arguments to its parameters' types,
        // as its C prototype does
        if let (NodeTag::EVar, Extra::Var(vp)) = (ast[callee].tag, &ast[callee].extra)
            && let TypeKind::Function {
                return_type,
                param_decls,
            } = ast[ast[*vp].type_].kind
        {
            // the first is the implicit `seqg_env`, passed above
            let params: Vec<TypeId> = ast
                .list(param_decls)
                .skip(1)
                .map(|pd| ast[ast[pd].e_decl().expect("parameter decl var")].type_)
                .collect();
            for (i, a) in args.into_iter().enumerate() {
                let e = match params.get(i) {
                    Some(&t) => self.stored(a, &self.value_ty(t)),
                    None => self.expr(a).e,
                };
                out.push(e.into());
            }
            let ty = match ast[return_type].kind {
                TypeKind::Void => Ty::Unit,
                _ => self.value_ty(return_type),
            };
            return Val {
                e: Expr::Call(Box::new(f.e), out),
                ty,
            };
        }
        let c_name = match (ast[callee].tag, &ast[callee].extra) {
            (NodeTag::EVar, Extra::Var(vp)) if ast[ast[*vp].type_].kind == TypeKind::None => {
                Some(ast[callee].token.text())
            }
            _ => None,
        };
        let CProto {
            params,
            ret,
            variadic,
        } = c_name.map_or_else(CProto::default, |name| c_prototype(ast, name));
        match variadic {
            Some((fixed, scans)) if args.len() >= fixed => {
                for (i, &a) in args[..fixed].iter().enumerate() {
                    out.push(self.c_arg(a, params.get(i).copied().flatten()).into());
                }
                let rest = args[fixed..]
                    .iter()
                    .map(|&a| {
                        if scans {
                            self.expr(a).e.into()
                        } else {
                            self.variadic_arg(a).into()
                        }
                    })
                    .collect();
                out.push(Expr::unary("&", Expr::Array(rest)).into());
            }
            _ => {
                for (i, a) in args.into_iter().enumerate() {
                    out.push(self.c_arg(a, params.get(i).copied().flatten()).into());
                }
            }
        }
        Val {
            e: Expr::Call(Box::new(f.e), out),
            ty: ret.unwrap_or(Ty::Foreign),
        }
    }

    /// An argument to a C function, by its prototype's parameter when
    /// snc-rs knows it.
    fn c_arg(&mut self, ap: NodeId, param: Option<CParam>) -> Expr {
        match param {
            Some(CParam::Num(t)) => {
                let v = self.expr(ap);
                conv(v, &Ty::Num(t.into()))
            }
            // C converts a `void *` to the `char *` parameter
            Some(CParam::Src | CParam::Dst) if matches!(self.pointee(ap), Some(PlaceTy::Void)) => {
                self.stored(ap, &Ty::Ptr(C_CHAR.into()))
            }
            // a `char *` value is a `CDst` as a place is; C's write into a
            // literal is undefined
            Some(CParam::Dst) => {
                let v = self.expr(ap);
                match &v.ty {
                    Ty::Str => self.refuse(ap, "a write into a string literal"),
                    Ty::Ptr(t) if t != C_CHAR => self.refuse(ap, "a cast between pointer types"),
                    _ => {}
                }
                v.e
            }
            Some(CParam::CharPtr) => self.stored(ap, &Ty::Ptr(C_CHAR.into())),
            Some(CParam::Src | CParam::Place) | None => self.arg_value(ap),
        }
    }

    /// A `printf`-like function's variadic argument as a `CArg`: a `char *`
    /// the string it points to, another pointer its address, which is all
    /// `%p` reads of it.
    fn variadic_arg(&mut self, ap: NodeId) -> Expr {
        let v = match self.pointee(ap) {
            Some(t) if self.pointee_type(t) != C_CHAR => {
                self.pointer(ap).method("addr", Vec::new())
            }
            _ => self.arg_value(ap),
        };
        Expr::call("seqg_rt::c::CArg::from", vec![v])
    }

    /// A C function's argument, passed by C's decay rule: a string or an
    /// array as its place, a truth value or a run-time value as C's `int`.
    fn arg_value(&mut self, ap: NodeId) -> Expr {
        let v = self.expr(ap);
        match v.ty {
            Ty::Bool => v.e.cast("i32"),
            Ty::Rt(r) => rt_int(v.e, r),
            _ => v.e,
        }
    }

    /// `sizeof`, which C leaves to its compiler: the size on the targets
    /// EPICS supports, whose `long` and pointers are 8 bytes as snc-rs's
    /// `i64` is.
    fn size_of(&mut self, ep: NodeId) -> Val {
        let ast = self.ast;
        let size = ast[ep]
            .child(child::FUNC_ARGS)
            .and_then(|a| self.operand_size(a));
        let size = size.unwrap_or_else(|| {
            self.snc.error_at_node(
                &ast[ep],
                format_args!("snc-rs cannot compute this sizeof\n"),
            );
            0
        });
        Val {
            e: Expr::Atom(size.to_string()),
            ty: Ty::Int,
        }
    }

    /// The size of `sizeof`'s operand: a type, or the type of an
    /// expression, which C does not evaluate.
    fn operand_size(&self, a: NodeId) -> Option<u64> {
        let ast = self.ast;
        if let Some(t) = sizeof_type(ast, a) {
            return type_size(ast, t);
        }
        match ast[a].tag {
            NodeTag::EParen => self.operand_size(ast[a].child(child::PAREN_EXPR)?),
            // a `char` array, not the pointer it decays to
            NodeTag::EString => None,
            _ => match self.place_kind(a) {
                Some((PlaceTy::Snl(t), _)) => type_size(ast, t),
                Some((PlaceTy::Char, _)) => Some(1),
                Some((PlaceTy::Void | PlaceTy::Global(_) | PlaceTy::Member, _)) => None,
                None => self.pointee(a).map(|_| 8),
            },
        }
    }

    /// C `gen_builtin_call`: `seqg_env.<name>(...)` with every parameter
    /// snc defaults filled in.
    fn builtin_call(&mut self, ep: NodeId) -> Val {
        let ast = self.ast;
        let node = &ast[ep];
        let callee = node.child(child::FUNC_EXPR).expect("callee");
        let Extra::Builtin(fi) = ast[callee].extra else {
            unreachable!("builtin without function");
        };
        let fsym = &FUNC_SYMBOLS[fi];
        let method = snake_case(fsym.c_name.unwrap_or(fsym.name));
        let mut args = Vec::new();
        let done = |g: &mut Gen<'_>, args| Val {
            e: Expr::Method(Box::new(g.env()), method.clone(), args),
            ty: builtin_type(fsym.name),
        };
        if !self.has_env {
            self.snc.error_at_node(
                node,
                format_args!(
                    "snc-rs cannot call built-in function {} in an initializer\n",
                    fsym.name
                ),
            );
            return done(self, args);
        }
        if fsym.cond_only && self.cx != Context::Cond {
            self.snc.error_at_node(
                node,
                format_args!(
                    "calling {} is only allowed inside a when condition\n",
                    fsym.name
                ),
            );
            return done(self, args);
        }
        if fsym.action_only && self.cx == Context::Cond {
            self.snc.error_at_node(
                node,
                format_args!(
                    "calling {} is not allowed inside a when condition\n",
                    fsym.name
                ),
            );
            return done(self, args);
        }
        let mut ap = node.child(child::FUNC_ARGS);
        let mut after_pv_array = false;
        for (i, pp) in fsym.params.iter().enumerate() {
            let n = i + 1;
            match ap {
                None => match pp.default_value {
                    Some(d) => args.push(default_arg(d).into()),
                    None => {
                        self.snc.error_at_node(
                            node,
                            format_args!(
                                "not enough arguments to built-in function '{}'\n",
                                fsym.name
                            ),
                        );
                        return done(self, args);
                    }
                },
                Some(a) => match pp.type_ {
                    // a `FALSE`-defaulted flag is tested
                    ParamType::Other if pp.default_value == Some("FALSE") => {
                        args.push(self.truth(a).into());
                    }
                    ParamType::Other => {
                        let v = self.expr(a);
                        args.push(other_arg(pp, v, fsym.name, after_pv_array).into());
                    }
                    ParamType::Ef => args.push(self.ef_arg(fsym.name, a, n).into()),
                    ParamType::Pv => args.push(self.pv_arg(fsym.name, a, n, false)),
                    ParamType::PvArray => args.push(self.pv_arg(fsym.name, a, n, true)),
                },
            }
            after_pv_array = pp.type_ == ParamType::PvArray;
            ap = ap.and_then(|a| ast[a].next);
        }
        if ap.is_some() {
            self.snc.error_at_node(
                node,
                format_args!("too many arguments to built-in function '{}'\n", fsym.name),
            );
        }
        done(self, args)
    }

    /// C `gen_ef_arg`: an event flag argument.
    fn ef_arg(&mut self, func: &str, ap: NodeId, index: usize) -> Expr {
        let ast = self.ast;
        let node = &ast[ap];
        match (node.tag, &node.extra) {
            (NodeTag::EConst, Extra::Const(i)) if CONST_SYMBOLS[*i].type_ == ConstType::EvFlag => {
                self.builtin_const(*i).e
            }
            (NodeTag::EVar, Extra::Var(vp)) if ast[ast[*vp].type_].kind == TypeKind::EvFlag => {
                Expr::atom(ast[*vp].name.as_deref().unwrap_or_default())
            }
            // C reads the null e_const of a literal and crashes on efSet(0);
            // that defect is not copied
            _ => {
                self.snc.error_at_node(
                    node,
                    format_args!(
                        "argument {index} to built-in function {func} must be an event flag\n"
                    ),
                );
                Expr::atom("seqg_rt::NOEVFLAG")
            }
        }
    }

    /// C `gen_pv_arg`: the channel a pv argument names, `ChId(index)`
    /// commented with the variable's name.
    fn pv_arg(&mut self, func: &str, ap: NodeId, index: usize, pv_array: bool) -> Arg {
        let ast = self.ast;
        let node = &ast[ap];
        let var_of = |e: NodeId| match ast[e].extra {
            Extra::Var(vp) => vp,
            _ => unreachable!("unconnected E_VAR"),
        };
        let unnamed = Arg::from(Expr::call("seqg_rt::ChId", vec![Expr::atom("0")]));
        let (vp, subscr) = match node.tag {
            NodeTag::EVar => {
                let vp = var_of(ap);
                let var = &ast[vp];
                let name = var.name.as_deref().unwrap_or_default();
                if var.assign == Multiplicity::Multi && !pv_array {
                    self.snc.error_at_node(
                        node,
                        format_args!(
                            "passing multi-PV array '{name}' to function '{func}' is no longer allowed\n"
                        ),
                    );
                    self.snc.report_at_node(
                        node,
                        format_args!(
                            "Perhaps you meant to pass '{name}[0]' or call the pvArray... variant?\n"
                        ),
                    );
                    return unnamed;
                }
                if var.assign == Multiplicity::Single && pv_array {
                    self.snc.error_at_node(
                        node,
                        format_args!(
                            "passing single-PV variable '{name}' to function '{func}' is not allowed\n"
                        ),
                    );
                    return unnamed;
                }
                (vp, None)
            }
            NodeTag::ESubscr
                if node
                    .child(child::SUBSCR_OPERAND)
                    .is_some_and(|o| ast[o].tag == NodeTag::EVar) =>
            {
                let operand = node.child(child::SUBSCR_OPERAND).expect("operand");
                (var_of(operand), node.child(child::SUBSCR_INDEX))
            }
            _ => {
                self.snc.error_at_node(
                    node,
                    format_args!(
                        "parameter {index} to '{func}' must be a variable or subscripted variable\n"
                    ),
                );
                return unnamed;
            }
        };
        let var = &ast[vp];
        if var.assign == Multiplicity::None {
            self.snc.error_at_node(
                node,
                format_args!("parameter {index} to '{func}' was not assigned to a pv\n"),
            );
        } else if subscr.is_some() && var.assign != Multiplicity::Multi {
            self.snc.error_at_node(
                node,
                format_args!(
                    "parameter {index} to '{func}' is subscripted but the variable it refers to has not been assigned to multiple pvs\n"
                ),
            );
        }
        let mut ch = Expr::call("seqg_rt::ChId", vec![Expr::Atom(var.index.to_string())]);
        if let Some(s) = subscr {
            let i = self.expr(s);
            let offset = match i.ty {
                Ty::Int => i.e,
                Ty::Rt(r) => rt_int(i.e, r).cast("i64"),
                _ => i.e.cast("i64"),
            };
            ch = ch.method("offset", vec![offset]);
        }
        Arg::commented(ch, var.name.as_deref().unwrap_or_default())
    }

    fn is_global(&self, vp: VarId) -> bool {
        let var = &self.ast[vp];
        self.ast[var.type_].kind == TypeKind::None
            && var
                .name
                .as_deref()
                .is_some_and(|n| self.globals.contains(n))
    }

    fn is_storage(&self, vp: VarId) -> bool {
        !matches!(
            self.ast[self.ast[vp].type_].kind,
            TypeKind::None | TypeKind::EvFlag | TypeKind::Function { .. }
        )
    }

    /// The place `vp` is: a field of `seqg_var` for a variable with
    /// program lifetime, a local `let` otherwise.
    fn var_place(&mut self, vp: VarId) -> Place {
        let ast = self.ast;
        let var = &ast[vp];
        let scope = &ast[var.scope.expect("variable has a scope")];
        let expr = match scope.tag {
            NodeTag::DProg | NodeTag::DSs | NodeTag::DState => {
                self.uses.var = true;
                var_place(ast, vp, Expr::atom("seqg_var"))
            }
            // compound statement or function parameter
            _ => Expr::atom(var.name.as_deref().unwrap_or_default()),
        };
        Place {
            expr,
            ty: PlaceTy::Snl(var.type_),
            multi: var.assign == Multiplicity::Multi,
        }
    }

    /// What the place `ep` names holds, if it names an SNL variable or an
    /// element of one; nothing is translated.
    fn place_kind(&self, ep: NodeId) -> Option<(PlaceTy, bool)> {
        let node = &self.ast[ep];
        match node.tag {
            NodeTag::EVar => match node.extra {
                Extra::Var(vp) if self.is_storage(vp) => {
                    let var = &self.ast[vp];
                    Some((PlaceTy::Snl(var.type_), var.assign == Multiplicity::Multi))
                }
                Extra::Var(vp) if self.is_global(vp) => Some((PlaceTy::Global(vp), false)),
                _ => None,
            },
            NodeTag::EParen => self.place_kind(node.child(child::PAREN_EXPR)?),
            // `*a` is `a[0]`, and `p[i]` is `*(p + i)`
            NodeTag::ESubscr => self.deref_kind(node.child(child::SUBSCR_OPERAND)?),
            NodeTag::EPre if node.token.symbol == Tok::Asterisk => {
                self.deref_kind(node.child(child::PRE_OPERAND)?)
            }
            NodeTag::ESelect => {
                self.members(self.holder(ep)?)?;
                Some((PlaceTy::Member, false))
            }
            _ => None,
        }
    }

    /// What the struct or union member selection `ep` selects from holds:
    /// the place left of `.`, or what the pointer left of `->` points to,
    /// a foreign value's of unknown type a [`PlaceTy::Member`].
    fn holder(&self, ep: NodeId) -> Option<PlaceTy> {
        let node = &self.ast[ep];
        let left = node.child(child::SELECT_LEFT)?;
        match node.token.symbol {
            Tok::Period => self.place_kind(left).map(|(t, _)| t),
            _ => Some(self.pointee(left).unwrap_or(PlaceTy::Member)),
        }
    }

    /// How a place holding `t` reaches its members, if it holds a struct
    /// or a union. snc-rs knows a union only by its SNL declaration, so a
    /// member it does not know the type of is taken for a struct.
    fn members(&self, t: PlaceTy) -> Option<Members> {
        match t {
            PlaceTy::Member => Some(Members::Struct),
            PlaceTy::Snl(t) => match &self.ast[t].kind {
                TypeKind::Foreign {
                    tag: ForeignTag::Union,
                    ..
                } => Some(Members::Union),
                TypeKind::Foreign {
                    tag: ForeignTag::Struct,
                    ..
                } => Some(Members::Struct),
                TypeKind::Foreign {
                    tag: ForeignTag::Typename,
                    name,
                } if rt_typename(name).is_none() => Some(Members::Struct),
                _ => None,
            },
            PlaceTy::Char | PlaceTy::Void | PlaceTy::Global(_) => None,
        }
    }

    /// What `*base` and `base[i]` name: an element of an array or `string`
    /// place, else what a pointer `base` points to.
    fn deref_kind(&self, base: NodeId) -> Option<(PlaceTy, bool)> {
        match self.array_elem(base) {
            Some(elem) => Some((elem.0, false)),
            None => self.pointee(base).map(|t| (t, false)),
        }
    }

    /// [`Gen::elem_kind`] of `ep`'s place, if it is an array or `string`.
    fn array_elem(&self, ep: NodeId) -> Option<(PlaceTy, bool)> {
        let (ty, multi) = self.place_kind(ep)?;
        self.elem_kind(ty, multi)
    }

    /// What an element of a place holding `ty` holds, and whether it is
    /// reached by `[i]` (a multi-PV array) rather than `.at(i)`.
    fn elem_kind(&self, ty: PlaceTy, multi: bool) -> Option<(PlaceTy, bool)> {
        let PlaceTy::Snl(t) = ty else {
            return None;
        };
        match &self.ast[t].kind {
            TypeKind::Array { elem_type, .. } => Some((PlaceTy::Snl(*elem_type), multi)),
            TypeKind::Prim(PrimType::String) => Some((PlaceTy::Char, false)),
            _ => None,
        }
    }

    /// The place `ep` names; `place_kind(ep)` is `Some`.
    fn place(&mut self, ep: NodeId) -> Place {
        let ast = self.ast;
        let node = &ast[ep];
        match node.tag {
            NodeTag::EVar => match node.extra {
                Extra::Var(vp) if self.is_global(vp) => Place {
                    expr: Expr::atom(ast[vp].name.as_deref().unwrap_or_default()),
                    ty: PlaceTy::Global(vp),
                    multi: false,
                },
                Extra::Var(vp) => self.var_place(vp),
                _ => unreachable!("unconnected E_VAR"),
            },
            NodeTag::EParen => self.place(node.child(child::PAREN_EXPR).expect("operand")),
            NodeTag::ESubscr => {
                let index = node.child(child::SUBSCR_INDEX).expect("subscript");
                self.deref(
                    node.child(child::SUBSCR_OPERAND).expect("operand"),
                    Some(index),
                )
            }
            NodeTag::EPre => self.deref(node.child(child::PRE_OPERAND).expect("operand"), None),
            NodeTag::ESelect => {
                let left = node.child(child::SELECT_LEFT).expect("operand");
                let name = ast[node.child(child::SELECT_RIGHT).expect("member")]
                    .token
                    .text();
                let (holder, base) = match node.token.symbol {
                    Tok::Period => {
                        let p = self.place(left);
                        (p.ty, p.expr)
                    }
                    _ => match self.pointee(left) {
                        Some(t) => (t, self.pointer(left)),
                        None => (PlaceTy::Member, self.expr(left).e),
                    },
                };
                let kind = self.members(holder).expect("a struct or union");
                Place {
                    expr: member(base, name, kind),
                    ty: PlaceTy::Member,
                    multi: false,
                }
            }
            tag => unreachable!("place {}", tag.name()),
        }
    }

    /// `base[index]`, or `*base` for no index: `at` of an array or
    /// `string` place, else the `seqg_rt::Ptr` `base` is, moved by `index`.
    fn deref(&mut self, base: NodeId, index: Option<NodeId>) -> Place {
        if let Some(elem) = self.array_elem(base) {
            let place = self.place(base);
            let i = match index {
                Some(i) => self.expr(i),
                None => Val {
                    e: Expr::atom("0"),
                    ty: Ty::Int,
                },
            };
            return index_place(&place, elem, i);
        }
        let ty = self.pointee(base).expect("a dereferenced pointer");
        if let PlaceTy::Void = ty {
            self.refuse(base, "a dereference of a void pointer");
        }
        let mut expr = self.pointer(base);
        if let Some(i) = index {
            let i = self.expr(i);
            expr = expr.method("offset", vec![offset(Tok::Add, i)]);
        }
        Place {
            expr,
            ty,
            multi: false,
        }
    }

    /// The value type of a variable of SNL type `t`.
    fn value_ty(&self, t: TypeId) -> Ty {
        let ast = self.ast;
        match &ast[t].kind {
            TypeKind::Prim(PrimType::String) | TypeKind::Array { .. } => Ty::Ref,
            TypeKind::Prim(_) => Ty::Num(value_type(ast, t).expect("numeric type")),
            TypeKind::Pointer { value_type: p } => self
                .snl_pointee(*p)
                .map_or(Ty::Foreign, |t| Ty::Ptr(self.pointee_type(t))),
            TypeKind::Foreign {
                tag: ForeignTag::Typename,
                name,
            } => rt_typename(name).map_or(Ty::Foreign, Ty::Rt),
            _ => Ty::Foreign,
        }
    }

    /// A binary arithmetic operator; pointer arithmetic
    /// [`Gen::pointer_arith`] does not read (`p + q`) is refused.
    fn arith(&mut self, ep: NodeId, op: Tok, l: Val, r: Val) -> Val {
        let pointer = |ty: &Ty| matches!(ty, Ty::Ref | Ty::Ptr(_) | Ty::Str);
        if matches!(op, Tok::Add | Tok::Sub) && (pointer(&l.ty) || pointer(&r.ty)) {
            self.refuse(ep, "pointer arithmetic");
        }
        binop(op, l, r)
    }

    /// `ep` as a Rust `bool`, as C tests it against zero. A value of unknown
    /// type is tested against its type's `Default`, which is C's zero for a
    /// number and the null pointer for a pointer.
    fn truth(&mut self, ep: NodeId) -> Expr {
        let v = self.expr(ep);
        let zero = |float: bool| Expr::atom(if float { "0.0" } else { "0" });
        match v.ty {
            Ty::Bool => v.e,
            Ty::Int | Ty::Float => match literal_is_zero(&v.e) {
                Some(z) => Expr::atom(if z { "false" } else { "true" }),
                None => Expr::binary(v.e, "!=", zero(v.ty == Ty::Float)),
            },
            Ty::Num(t) => Expr::binary(v.e, "!=", zero(is_float(&t))),
            Ty::Rt(r) => Expr::binary(rt_int(v.e, r), "!=", zero(false)),
            Ty::Ptr(_) => Expr::unary("!", v.e.method("is_null", Vec::new())),
            Ty::Str | Ty::Ref => Expr::atom("true"),
            Ty::Foreign => foreign_truth(v.e, true),
            Ty::Unit => Expr::binary(v.e, "!=", zero(false)),
        }
    }

    /// C's `!ep`, as [`Gen::truth`] tests it.
    fn not(&mut self, ep: NodeId) -> Expr {
        let v = self.expr(ep);
        let zero = |float: bool| Expr::atom(if float { "0.0" } else { "0" });
        match v.ty {
            Ty::Bool => Expr::unary("!", v.e),
            Ty::Int | Ty::Float => match literal_is_zero(&v.e) {
                Some(z) => Expr::atom(if z { "true" } else { "false" }),
                None => Expr::binary(v.e, "==", zero(v.ty == Ty::Float)),
            },
            Ty::Num(t) => Expr::binary(v.e, "==", zero(is_float(&t))),
            Ty::Rt(r) => Expr::binary(rt_int(v.e, r), "==", zero(false)),
            Ty::Ptr(_) => v.e.method("is_null", Vec::new()),
            Ty::Str | Ty::Ref => Expr::atom("false"),
            Ty::Foreign => foreign_truth(v.e, false),
            Ty::Unit => Expr::binary(v.e, "==", zero(false)),
        }
    }

    fn place_value_ty(&self, place: &Place) -> Ty {
        match place.ty {
            _ if place.multi => Ty::Ref,
            PlaceTy::Char => Ty::Num(C_CHAR.into()),
            PlaceTy::Void => Ty::Unit,
            PlaceTy::Member => Ty::Foreign,
            PlaceTy::Global(vp) => {
                let name = self.ast[vp].name.as_deref().unwrap_or_default();
                match escaped_var_type(self.ast, name) {
                    Some((t, 0)) => Ty::Num(t.into()),
                    _ => Ty::Foreign,
                }
            }
            PlaceTy::Snl(t) => self.value_ty(t),
        }
    }

    /// A read of `place`: `get()` for a scalar, the place itself for a
    /// string or an array.
    fn read(&self, place: Place) -> Val {
        match self.place_value_ty(&place) {
            Ty::Ref => Val {
                e: Expr::unary("&", place.expr),
                ty: Ty::Ref,
            },
            ty => Val {
                e: place.expr.method("get", Vec::new()),
                ty,
            },
        }
    }
}

/// Member `name` of what `base`, a place, holds.
fn member(base: Expr, name: &str, kind: Members) -> Expr {
    let v = || Expr::atom("v");
    match kind {
        Members::Struct => base.method(
            "field",
            vec![Expr::Closure(
                "v".into(),
                Box::new(Expr::unary("&mut ", v().field(name))),
            )],
        ),
        Members::Union => base.method(
            "view",
            vec![
                Expr::Closure("v".into(), Box::new(v().method(name, Vec::new()))),
                Expr::Closure(
                    "v, x".into(),
                    Box::new(v().method(format!("set_{name}"), vec![Expr::atom("x")])),
                ),
            ],
        ),
    }
}

/// The element of `place` at `index`.
fn index_place(place: &Place, (ty, by_index): (PlaceTy, bool), index: Val) -> Place {
    let i = usize_index(index);
    let expr = if by_index {
        place.expr.clone().index(i)
    } else {
        place.expr.clone().method("at", vec![i])
    };
    Place {
        expr,
        ty,
        multi: false,
    }
}

fn usize_index(v: Val) -> Expr {
    match v.ty {
        Ty::Int => v.e,
        Ty::Num(t) if t == "usize" => v.e,
        Ty::Rt(r) => rt_int(v.e, r).cast("usize"),
        _ => v.e.cast("usize"),
    }
}

/// `n` as the `isize` `Ptr::offset` takes for C's `p op n`.
fn offset(op: Tok, n: Val) -> Expr {
    let n = conv(n, &Ty::Num("isize".into()));
    if op == Tok::Sub {
        Expr::unary("-", n)
    } else {
        n
    }
}

fn modify(place: Expr, body: Expr) -> Expr {
    place.method("modify", vec![Expr::Closure("v".into(), Box::new(body))])
}

fn deref_v() -> Expr {
    Expr::unary("*", Expr::atom("v"))
}

/// `*v` as a value of type `ty`, which a `Ptr` is only by a clone.
fn current(ty: &Ty) -> Expr {
    match ty {
        Ty::Ptr(_) => Expr::atom("v").method("clone", Vec::new()),
        _ => deref_v(),
    }
}

fn int_one() -> Val {
    Val {
        e: Expr::atom("1"),
        ty: Ty::Int,
    }
}

/// `x++` of a variable that is not a number, which Rust has no operator
/// for.
fn raw_incdec(e: Expr, op: Tok) -> Val {
    Val {
        e: Expr::Block(vec![
            Stmt::Expr(Expr::assign(e.clone(), op_str(op), Expr::atom("1"))),
            Stmt::Tail(e),
        ]),
        ty: Ty::Foreign,
    }
}

/// The closure body of a compound assignment, `++` or `--` to `*v` of
/// type `ty`: as C, `*v = (ty)(*v op v)`.
fn compound(op: Tok, ty: &Ty, v: Val) -> Expr {
    match ty {
        Ty::Num(_) => {}
        Ty::Ptr(_) => {
            let step = offset(compound_binop(op), v);
            return Expr::assign(deref_v(), "=", Expr::atom("v").method("offset", vec![step]));
        }
        // converted back to the place's type, which only rustc knows
        Ty::Foreign if !matches!(op, Tok::Lsheq | Tok::Rsheq) => {
            let lhs = Val {
                e: deref_v(),
                ty: Ty::Foreign,
            };
            let e = binop(compound_binop(op), lhs, v).e;
            return Expr::assign(deref_v(), "=", e.cast("_"));
        }
        _ => return Expr::assign(deref_v(), op_str(op), v.e),
    }
    let lhs = Val {
        e: deref_v(),
        ty: ty.clone(),
    };
    Expr::assign(deref_v(), "=", conv(binop(compound_binop(op), lhs, v), ty))
}

/// A binary operator other than an assignment, `,`, `&&` and `||`.
fn binop(op: Tok, l: Val, r: Val) -> Val {
    let sym = op_str(op);
    match op {
        Tok::Eq | Tok::Ne | Tok::Gt | Tok::Ge | Tok::Le | Tok::Lt => {
            let direct = matches!(op, Tok::Eq | Tok::Ne)
                && match (&l.ty, &r.ty) {
                    (Ty::Bool, Ty::Bool) => true,
                    (Ty::Rt(a), Ty::Rt(b)) => a == b,
                    (Ty::Ptr(a), Ty::Ptr(b)) => a == b,
                    _ => false,
                };
            if direct {
                return Val {
                    e: Expr::binary(l.e, sym, r.e),
                    ty: Ty::Bool,
                };
            }
            if matches!(op, Tok::Eq | Tok::Ne) {
                let null_test = |p: Expr| {
                    let e = p.method("is_null", Vec::new());
                    Val {
                        e: if op == Tok::Eq {
                            e
                        } else {
                            Expr::unary("!", e)
                        },
                        ty: Ty::Bool,
                    }
                };
                if matches!(l.ty, Ty::Ptr(_)) && is_null(&r) {
                    return null_test(l.e);
                }
                if matches!(r.ty, Ty::Ptr(_)) && is_null(&l) {
                    return null_test(r.e);
                }
                if l.ty == Ty::Foreign && is_null(&r) {
                    return Val {
                        e: foreign_truth(l.e, op == Tok::Ne),
                        ty: Ty::Bool,
                    };
                }
                if r.ty == Ty::Foreign && is_null(&l) {
                    return Val {
                        e: foreign_truth(r.e, op == Tok::Ne),
                        ty: Ty::Bool,
                    };
                }
            }
            let e = match operands(l, r) {
                Operands::Known(a, b, _) => Expr::binary(a, sym, b),
                Operands::Unknown(a, b) => arith_call(op, a, b),
            };
            Val { e, ty: Ty::Bool }
        }
        Tok::Lshift | Tok::Rshift => {
            let l = promoted(l);
            let r = arith_operand(r);
            Val {
                e: Expr::binary(l.e, sym, r.e),
                ty: l.ty,
            }
        }
        _ => {
            let (a, b, ty) = match operands(l, r) {
                Operands::Known(a, b, ty) => (a, b, ty),
                Operands::Unknown(a, b) => {
                    return Val {
                        e: arith_call(op, a, b),
                        ty: Ty::Foreign,
                    };
                }
            };
            let wrapping = match op {
                Tok::Add => "wrapping_add",
                Tok::Sub => "wrapping_sub",
                Tok::Asterisk => "wrapping_mul",
                _ => "",
            };
            let e = match &ty {
                // C's unsigned arithmetic is modulo 2^n; Rust's panics in a
                // dev build
                Ty::Num(t) if t.starts_with('u') && !wrapping.is_empty() => {
                    Expr::call(format!("{t}::{wrapping}"), vec![a, b])
                }
                _ => Expr::binary(a, sym, b),
            };
            Val { e, ty }
        }
    }
}

/// `a` and `b` by C's usual arithmetic conversions.
enum Operands {
    /// brought to their common type, which is the third
    Known(Expr, Expr, Ty),
    /// a foreign operand, whose C type snc-rs does not know, beside an
    /// arithmetic one that is not a `double` or beside another foreign one:
    /// `seqg_rt::arith` converts them by their Rust types
    Unknown(Expr, Expr),
}

fn operands(a: Val, b: Val) -> Operands {
    let a = arith_operand(a);
    let b = arith_operand(b);
    let double = |t: &Ty| matches!(t, Ty::Float) || matches!(t, Ty::Num(n) if n == "f64");
    let (a, b, ty) = match (&a.ty, &b.ty) {
        (Ty::Num(x), Ty::Num(y)) => {
            let c = usual(x, y);
            (cast_num(a, &c), cast_num(b, &c), Ty::Num(c))
        }
        (Ty::Num(x), Ty::Int) => {
            let c = promote(x);
            (cast_num(a, &c), adapt_int(b.e, &c), Ty::Num(c))
        }
        (Ty::Int, Ty::Num(y)) => {
            let c = promote(y);
            (adapt_int(a.e, &c), cast_num(b, &c), Ty::Num(c))
        }
        (Ty::Num(x), Ty::Float) => {
            let c = usual(x, "f64");
            (cast_num(a, &c), b.e, Ty::Num(c))
        }
        (Ty::Float, Ty::Num(y)) => {
            let c = usual("f64", y);
            (a.e, cast_num(b, &c), Ty::Num(c))
        }
        (Ty::Int, Ty::Float) => (adapt_int(a.e, "f64"), b.e, Ty::Float),
        (Ty::Float, Ty::Int) => (a.e, adapt_int(b.e, "f64"), Ty::Float),
        (Ty::Int, Ty::Int) => (a.e, b.e, Ty::Int),
        (Ty::Float, Ty::Float) => (a.e, b.e, Ty::Float),
        // anything with a double is a double
        (Ty::Foreign, t) if double(t) => (a.e.cast("f64"), b.e, Ty::Num("f64".into())),
        (t, Ty::Foreign) if double(t) => (a.e, b.e.cast("f64"), Ty::Num("f64".into())),
        (Ty::Foreign, t) | (t, Ty::Foreign) if is_numeric(t) || *t == Ty::Foreign => {
            return Operands::Unknown(a.e, b.e);
        }
        // as C copies them, such as a pointer beside a foreign operand
        _ => (a.e, b.e, Ty::Foreign),
    };
    Operands::Known(a, b, ty)
}

/// `seqg_rt::arith`'s C binary `op` on operands of types it converts.
fn arith_call(op: Tok, a: Expr, b: Expr) -> Expr {
    let f = match op {
        Tok::Add => "add",
        Tok::Sub => "sub",
        Tok::Asterisk => "mul",
        Tok::Slash => "div",
        Tok::Mod => "rem",
        Tok::Eq => "eq",
        Tok::Ne => "ne",
        Tok::Lt => "lt",
        Tok::Le => "le",
        Tok::Gt => "gt",
        Tok::Ge => "ge",
        Tok::Ampersand => "and",
        Tok::Vbar => "or",
        Tok::Caret => "xor",
        op => unreachable!("binary operator {op:?}"),
    };
    Expr::call(format!("seqg_rt::arith::{f}"), vec![a, b])
}

/// `v` as C's cast to `t` converts it, typed by itself.
fn cast_num(v: Val, t: &str) -> Expr {
    match &v.ty {
        Ty::Num(s) if s == t => v.e,
        Ty::Int => match adapt_int(v.e, t) {
            e @ Expr::Cast(..) => e,
            e => e.cast(t),
        },
        _ => v.e.cast(t),
    }
}

/// An arithmetic operand: a truth value or a run-time value as C's `int`.
fn arith_operand(v: Val) -> Val {
    match v.ty {
        Ty::Bool => Val {
            e: v.e.cast("i32"),
            ty: Ty::Num("i32".into()),
        },
        Ty::Rt(r @ ("ChId" | "EfId")) => Val {
            e: rt_int(v.e, r),
            ty: Ty::Num("u32".into()),
        },
        Ty::Rt(r) if r != "WallTime" => Val {
            e: rt_int(v.e, r),
            ty: Ty::Num("i32".into()),
        },
        _ => v,
    }
}

/// C's integer promotion of an operand, which `seqg_rt::arith` gives one
/// of unknown type by its Rust type.
fn promoted(v: Val) -> Val {
    let v = arith_operand(v);
    match &v.ty {
        Ty::Num(t) if promote(t) != *t => Val {
            e: v.e.cast("i32"),
            ty: Ty::Num("i32".into()),
        },
        Ty::Foreign => Val {
            e: Expr::call("seqg_rt::arith::promote", vec![v.e]),
            ty: Ty::Foreign,
        },
        _ => v,
    }
}

fn neg(v: Val) -> Val {
    if v.ty == Ty::Foreign {
        return Val {
            e: Expr::call("seqg_rt::arith::neg", vec![v.e]),
            ty: Ty::Foreign,
        };
    }
    let v = promoted(v);
    match &v.ty {
        Ty::Num(t) if t.starts_with('u') => Val {
            e: v.e.method("wrapping_neg", Vec::new()),
            ty: v.ty,
        },
        _ => Val {
            e: Expr::unary("-", v.e),
            ty: v.ty,
        },
    }
}

/// A run-time value as the integer C has.
fn rt_int(e: Expr, r: &str) -> Expr {
    match r {
        "ChId" | "EfId" => e.field("0"),
        "WallTime" => e,
        _ => e.cast("i32"),
    }
}

/// A C arithmetic type snc-rs knows; a foreign value may be a struct or a
/// pointer.
fn is_numeric(ty: &Ty) -> bool {
    matches!(ty, Ty::Int | Ty::Float | Ty::Num(_) | Ty::Bool | Ty::Rt(_))
}

fn is_float(t: &str) -> bool {
    matches!(t, "f32" | "f64")
}

fn is_integer(ty: &Ty) -> bool {
    match ty {
        Ty::Int | Ty::Bool => true,
        Ty::Num(t) => !is_float(t),
        _ => false,
    }
}

/// C's conversion rank: `double`, `float`, then the integers by size.
fn rank(t: &str) -> u8 {
    match t {
        "f64" => 6,
        "f32" => 5,
        "i64" | "u64" | "usize" => 4,
        "i32" | "u32" => 3,
        "i16" | "u16" => 2,
        _ => 1,
    }
}

fn promote(t: &str) -> String {
    if rank(t) < 3 { "i32" } else { t }.into()
}

/// The common type of the usual arithmetic conversions.
fn usual(x: &str, y: &str) -> String {
    let (x, y) = (promote(x), promote(y));
    match rank(&x).cmp(&rank(&y)) {
        std::cmp::Ordering::Greater => x,
        std::cmp::Ordering::Less => y,
        std::cmp::Ordering::Equal if y.starts_with('u') => y,
        std::cmp::Ordering::Equal => x,
    }
}

/// An integer constant expression where a `t` is needed: a literal of a
/// floating type gets its `.0`, anything else is cast.
fn adapt_int(e: Expr, t: &str) -> Expr {
    if !is_float(t) {
        // C converts modulo 2^n. A literal cast to `t` takes `t` as its own
        // type, so a constant `t` may not hold goes by way of `i64`.
        let holds = match int_value(&e) {
            Some(v) => int_fits(v, t),
            None => matches!(t, "i32" | "i64" | "isize"),
        };
        return if holds { e } else { e.cast("i64").cast(t) };
    }
    match e {
        Expr::Atom(s)
            if s.trim_start_matches('-')
                .bytes()
                .all(|b| b.is_ascii_digit()) =>
        {
            Expr::Atom(format!("{s}.0"))
        }
        Expr::Unary("-", x) => Expr::unary("-", adapt_int(*x, t)),
        e => e.cast(t),
    }
}

/// The value of an integer literal, or of a negated one.
fn int_value(e: &Expr) -> Option<i128> {
    match e {
        Expr::Atom(s) => {
            // a Rust type suffix starts with a letter no digit has
            let s = s.split(['i', 'u']).next().unwrap_or(s);
            match (s.strip_prefix("0x"), s.strip_prefix("0o")) {
                (Some(hex), _) => i128::from_str_radix(hex, 16).ok(),
                (_, Some(oct)) => i128::from_str_radix(oct, 8).ok(),
                _ => s.parse().ok(),
            }
        }
        Expr::Unary("-", x) => int_value(x).map(|v| -v),
        Expr::Paren(x) => int_value(x),
        _ => None,
    }
}

/// Whether `v` is C's null pointer constant, an integer constant 0 of any
/// type.
fn is_null(v: &Val) -> bool {
    is_integer(&v.ty) && int_value(&v.e) == Some(0)
}

/// Whether integer type `t` holds `v`; `c_char` only where its sign does
/// not matter.
fn int_fits(v: i128, t: &str) -> bool {
    match t {
        "i8" => i8::try_from(v).is_ok(),
        "u8" => u8::try_from(v).is_ok(),
        "std::ffi::c_char" => (0..=127).contains(&v),
        "i16" => i16::try_from(v).is_ok(),
        "u16" => u16::try_from(v).is_ok(),
        "i32" => i32::try_from(v).is_ok(),
        "u32" => u32::try_from(v).is_ok(),
        "i64" | "isize" => i64::try_from(v).is_ok(),
        "u64" | "usize" => u64::try_from(v).is_ok(),
        _ => false,
    }
}

/// `v` converted as C converts a value assigned to, or passed as, a
/// `target`.
fn conv(v: Val, target: &Ty) -> Expr {
    match target {
        // named, so that a companion function taking another type does not
        // build; `_` where the value's Rust type may already be `t`
        Ty::Num(t) => match v.ty {
            Ty::Int => adapt_int(v.e, t),
            Ty::Float if is_float(t) => v.e,
            Ty::Num(s) if s == *t => v.e,
            Ty::Num(_) | Ty::Float => v.e.cast(t),
            Ty::Bool if is_float(t) => v.e.cast("i32").cast(t),
            Ty::Bool => v.e.cast(t),
            Ty::Rt(r) => rt_int(v.e, r).cast("_"),
            Ty::Ref | Ty::Str | Ty::Ptr(_) | Ty::Unit => v.e,
            Ty::Foreign => v.e.cast("_"),
        },
        Ty::Ptr(t) => match v.ty {
            Ty::Str if t == C_CHAR => {
                let bytes = match v.e {
                    e @ Expr::Atom(_) => e.method("as_bytes", Vec::new()),
                    e => e,
                };
                Expr::call("seqg_rt::CharPtr::from_bytes", vec![bytes])
            }
            _ if is_null(&v) => null_ptr(t),
            // C converts any pointer to and from `void *`
            Ty::Ptr(s) if s != *t && t == C_VOID => v.e.method("erase", Vec::new()),
            Ty::Ptr(s) if s == C_VOID && t != C_VOID => {
                v.e.method(format!("cast::<{t}>"), Vec::new())
            }
            _ => v.e,
        },
        // a foreign global of a type snc-rs does not know: C converts a
        // number to it
        Ty::Foreign => match v.ty {
            Ty::Num(_) | Ty::Float => v.e.cast("_"),
            Ty::Bool => v.e.cast("i32").cast("_"),
            _ => v.e,
        },
        _ => v.e,
    }
}

/// C's pointer type of a conditional or comparison of pointers to `a` and
/// `b`, one of which may be a null constant: `void *` if either is one.
fn composite(a: Option<PlaceTy>, b: Option<PlaceTy>) -> Option<PlaceTy> {
    match (a, b) {
        (Some(PlaceTy::Void), _) | (_, Some(PlaceTy::Void)) => Some(PlaceTy::Void),
        (a, b) => a.or(b),
    }
}

/// C's null pointer to a `t`.
fn null_ptr(t: &str) -> Expr {
    if t == C_CHAR {
        Expr::call("seqg_rt::CharPtr::default", Vec::new())
    } else {
        Expr::call(format!("seqg_rt::Ptr::<{t}>::default"), Vec::new())
    }
}

fn is_true(e: &Expr) -> bool {
    *e == Expr::atom("true")
}

/// Whether a numeric literal is zero, `None` for anything else.
fn literal_is_zero(e: &Expr) -> Option<bool> {
    let Expr::Atom(s) = e else {
        return None;
    };
    let s = s.trim_start_matches('-');
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u128::from_str_radix(hex, 16).ok().map(|v| v == 0);
    }
    if let Some(oct) = s.strip_prefix("0o") {
        return u128::from_str_radix(oct, 8).ok().map(|v| v == 0);
    }
    s.parse::<f64>().ok().map(|v| v == 0.0)
}

/// A C integer constant as a Rust literal of the type C gives it, `long`
/// 64 bits: an `int` stays untyped, any other type is the literal's
/// suffix. Octal is marked `0o`, a character constant is its value as a
/// signed `char`.
fn int_lit(text: &str) -> Val {
    let int = |lit: String| Val {
        e: Expr::Atom(lit),
        ty: Ty::Int,
    };
    if let Some(inner) = text.strip_prefix('\'') {
        let inner = inner.strip_suffix('\'').unwrap_or(inner);
        let value = c_unescape(inner.as_bytes())
            .first()
            .map_or(0, |&b| i32::from(b as i8));
        return int(value.to_string());
    }
    let digits = text.trim_end_matches(['u', 'U', 'l', 'L']);
    let suffix = text[digits.len()..].to_ascii_lowercase();
    let (lit, body, radix) = match digits.strip_prefix('0') {
        Some(hex) if hex.starts_with(['x', 'X']) => (format!("0x{}", &hex[1..]), &hex[1..], 16),
        Some(oct) if !oct.is_empty() && oct.bytes().all(|b| (b'0'..=b'7').contains(&b)) => {
            (format!("0o{oct}"), oct, 8)
        }
        _ => (digits.to_owned(), digits, 10),
    };
    // C11 6.4.4.1: the first of these that holds the value; past them all,
    // gcc's `unsigned long`
    let types: &[&str] = match (suffix.contains('u'), suffix.contains('l'), radix) {
        (false, false, 10) => &["i32", "i64"],
        (false, false, _) => &["i32", "u32", "i64", "u64"],
        (false, true, 10) => &["i64"],
        (false, true, _) => &["i64", "u64"],
        (true, false, _) => &["u32", "u64"],
        (true, true, _) => &["u64"],
    };
    let value = i128::from_str_radix(body, radix).ok();
    let t = types
        .iter()
        .copied()
        .find(|t| value.is_some_and(|v| int_fits(v, t)))
        .unwrap_or("u64");
    if t == "i32" {
        return int(lit);
    }
    Val {
        e: Expr::Atom(format!("{lit}{t}")),
        ty: Ty::Num(t.into()),
    }
}

/// A C floating constant as a Rust literal: suffix dropped, digits on
/// both sides of the point.
fn float_lit(text: &str) -> String {
    let t = text.trim_end_matches(['f', 'F', 'l', 'L']);
    let (mantissa, exponent) = t.split_at(t.find(['e', 'E']).unwrap_or(t.len()));
    let mut s = String::new();
    if mantissa.starts_with('.') {
        s.push('0');
    }
    s.push_str(mantissa);
    if mantissa.ends_with('.') {
        s.push('0');
    }
    s.push_str(exponent);
    s
}

/// `HW_LIMIT` as `HwLimit`.
fn camel(s: &str) -> String {
    s.split('_')
        .map(|w| {
            let w = w.to_ascii_lowercase();
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

/// `pvGetTmo` as `pv_get_tmo`.
fn snake_case(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The run-time's return type of a builtin.
fn builtin_type(name: &str) -> Ty {
    match name {
        "delay" | "efTest" | "efClear" | "efTestAndClear" | "pvGetQ" | "pvGetComplete"
        | "pvPutComplete" | "pvArrayGetComplete" | "pvArrayPutComplete" | "pvAssigned"
        | "pvConnected" | "pvArrayConnected" | "optGet" => Ty::Bool,
        "efSet" | "pvFlushQ" | "pvFreeQ" | "pvFlush" | "pvGetCancel" | "pvPutCancel"
        | "pvArrayGetCancel" | "pvArrayPutCancel" | "pvSync" | "pvArraySync" => Ty::Unit,
        "pvSeverity" => Ty::Rt("PvSevr"),
        "pvCount" | "pvChannelCount" | "pvConnectCount" | "pvAssignCount" => Ty::Num("u32".into()),
        "pvName" | "pvMessage" | "macValueGet" => Ty::Ptr(C_CHAR.into()),
        "pvIndex" => Ty::Rt("ChId"),
        "pvTimeStamp" => Ty::Rt("WallTime"),
        _ => Ty::Rt("PvStat"),
    }
}

/// The run-time value of an snc default argument.
fn default_arg(d: &str) -> Expr {
    Expr::atom(match d {
        "DEFAULT" => "seqg_rt::CompType::Default",
        "DEFAULT_TIMEOUT" => "seqg_rt::DEFAULT_TIMEOUT",
        "FALSE" => "false",
        "NULL" => "None",
        "1" => "1",
        d => unreachable!("builtin default {d}"),
    })
}

/// A builtin's `PT_OTHER` argument as its run-time parameter takes it.
fn other_arg(pp: &BuiltinParam, v: Val, func: &str, after_pv_array: bool) -> Expr {
    let num = |t: &str| Ty::Num(t.into());
    match pp.default_value {
        Some("DEFAULT_TIMEOUT") => conv(v, &num("f64")),
        // the completion flags: an array variable, or NULL
        Some("NULL") => match v.ty {
            _ if is_null(&v) => Expr::atom("None"),
            Ty::Ref => Expr::call("Some", vec![v.e]),
            _ => Expr::call("Some", vec![Expr::unary("&", v.e)]),
        },
        Some("1") => conv(v, &num("u32")),
        Some(_) => v.e,
        None if func == "delay" => conv(v, &num("f64")),
        // the length of a pv array
        None if after_pv_array => conv(v, &num("u32")),
        // a string, as a C function takes it
        None => v.e,
    }
}

/// The type `sizeof` measures: of a type name or of a variable.
fn sizeof_type(ast: &Ast, a: NodeId) -> Option<TypeId> {
    let node = &ast[a];
    match (node.tag, &node.extra) {
        (NodeTag::DDecl, Extra::Decl(vp)) | (NodeTag::EVar, Extra::Var(vp)) => Some(ast[*vp].type_),
        (NodeTag::EParen, _) => sizeof_type(ast, node.child(child::PAREN_EXPR)?),
        _ => None,
    }
}

fn type_size(ast: &Ast, t: TypeId) -> Option<u64> {
    Some(match &ast[t].kind {
        TypeKind::Prim(p) => match p {
            PrimType::Char | PrimType::UChar | PrimType::Int8T | PrimType::UInt8T => 1,
            PrimType::Short | PrimType::UShort | PrimType::Int16T | PrimType::UInt16T => 2,
            PrimType::Int
            | PrimType::UInt
            | PrimType::Int32T
            | PrimType::UInt32T
            | PrimType::Float => 4,
            PrimType::Long | PrimType::ULong | PrimType::Double => 8,
            PrimType::String => 40,
        },
        TypeKind::Pointer { .. } => 8,
        TypeKind::Array {
            num_elems,
            elem_type,
        } => u64::from(*num_elems) * type_size(ast, *elem_type)?,
        _ => return None,
    })
}

fn is_assign(t: Tok) -> bool {
    matches!(
        t,
        Tok::Equal
            | Tok::Addeq
            | Tok::Subeq
            | Tok::Andeq
            | Tok::Oreq
            | Tok::Diveq
            | Tok::Muleq
            | Tok::Modeq
            | Tok::Lsheq
            | Tok::Rsheq
            | Tok::Xoreq
    )
}

/// The binary operator of a compound assignment.
fn compound_binop(t: Tok) -> Tok {
    match t {
        Tok::Addeq => Tok::Add,
        Tok::Subeq => Tok::Sub,
        Tok::Andeq => Tok::Ampersand,
        Tok::Oreq => Tok::Vbar,
        Tok::Diveq => Tok::Slash,
        Tok::Muleq => Tok::Asterisk,
        Tok::Modeq => Tok::Mod,
        Tok::Lsheq => Tok::Lshift,
        Tok::Rsheq => Tok::Rshift,
        Tok::Xoreq => Tok::Caret,
        Tok::Incr => Tok::Add,
        Tok::Decr => Tok::Sub,
        t => unreachable!("compound assignment {t:?}"),
    }
}

/// The Rust spelling of an operator; `++` and `--` are `+= 1` and `-= 1`.
fn op_str(t: Tok) -> &'static str {
    match t {
        Tok::Equal => "=",
        Tok::Addeq | Tok::Incr => "+=",
        Tok::Subeq | Tok::Decr => "-=",
        Tok::Andeq => "&=",
        Tok::Oreq => "|=",
        Tok::Diveq => "/=",
        Tok::Muleq => "*=",
        Tok::Modeq => "%=",
        Tok::Lsheq => "<<=",
        Tok::Rsheq => ">>=",
        Tok::Xoreq => "^=",
        Tok::Oror => "||",
        Tok::Andand => "&&",
        Tok::Vbar => "|",
        Tok::Caret => "^",
        Tok::Ampersand => "&",
        Tok::Eq => "==",
        Tok::Ne => "!=",
        Tok::Gt => ">",
        Tok::Ge => ">=",
        Tok::Le => "<=",
        Tok::Lt => "<",
        Tok::Lshift => "<<",
        Tok::Rshift => ">>",
        Tok::Add => "+",
        Tok::Sub => "-",
        Tok::Asterisk => "*",
        Tok::Slash => "/",
        Tok::Mod => "%",
        t => unreachable!("operator {t:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use crate::analysis::analyse_program;
    use crate::parser::parse_program;
    use crate::rust_out;

    /// `src` parsed and analysed without diagnostics.
    fn analysed(src: &str) -> (Snc, Ast, Program) {
        let mut snc = Snc::capturing();
        let mut ast = Ast::new();
        let exp = parse_program(&mut snc, &mut ast, src.as_bytes(), "t.st")
            .expect("parse")
            .expect("program");
        let options = snc.options;
        let prg = analyse_program(&mut snc, &mut ast, exp, options);
        assert_eq!(String::from_utf8_lossy(snc.diagnostics()), "");
        (snc, ast, prg)
    }

    /// `src`'s state set code and the diagnostics generating it gave.
    fn translate(src: &str) -> (String, String) {
        let (mut snc, ast, prg) = analysed(src);
        let items = gen_ss_code(&mut snc, &ast, &prg).expect("no snc bug");
        let code = rust_out::print(&items);
        (
            code,
            String::from_utf8_lossy(snc.diagnostics()).into_owned(),
        )
    }

    /// `code` with every run of whitespace one space.
    fn words(code: &str) -> String {
        code.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn a_builtin_call_in_a_global_initializer_stops_as_c_asserts() {
        let (mut snc, ast, prg) = analysed(
            "program p\nevflag ef = 1;\nint y;\nassign y to \"y\";\nint x = pvIndex(y);\n\
             int z = pvIndex(y) + pvConnected(y);\nss s {\nstate a {\nwhen () {} state a\n}\n}\n",
        );
        assert_eq!(gen_ss_code(&mut snc, &ast, &prg).err(), Some(Exit(1)));
        assert_eq!(
            String::from_utf8_lossy(snc.diagnostics()),
            "t.st:2: error: event flag 'ef' cannot be initialized\n\
             t.st:5: snc bug (assertion failed): calling built-in function pvIndex not allowed here\n"
        );
    }

    #[test]
    fn a_literal_event_flag_argument_is_an_error_where_c_crashes() {
        let (_, diag) =
            translate("program p\nss s {\nstate a {\nwhen () {\nefSet(0);\n} state a\n}\n}\n");
        assert_eq!(
            diag,
            "t.st:5: error: argument 1 to built-in function efSet must be an event flag\n"
        );
    }

    #[test]
    fn a_negative_constant_converts_to_an_unsigned_type_modulo() {
        let (code, diag) = translate(
            "program p\nunsigned int u;\nss s {\nstate z {\nwhen (u != -1) {\nu = -1;\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(
            code.contains("seqg_var.u.get() != (-1 as i64) as u32"),
            "{code}"
        );
        assert!(
            code.contains("seqg_var.u.set((-1 as i64) as u32)"),
            "{code}"
        );
    }

    #[test]
    fn a_constant_converts_modulo_where_its_type_cannot_hold_it() {
        let (code, diag) = translate(
            "program p\nunsigned int u;\nunsigned char c;\nshort h;\ndouble d;\nss s {\nstate z {\n\
             when () {\nu = (unsigned int)-1;\nc = (unsigned char)300;\nh = (short)70000;\n\
             c = 301;\nh = -70000;\nh = 5;\nd = (short)70001 + d;\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        for line in [
            "seqg_var.u.set((-1 as i64) as u32)",
            "seqg_var.c.set((300 as i64) as u8)",
            "seqg_var.h.set((70000 as i64) as i16)",
            "seqg_var.c.set((301 as i64) as u8)",
            "seqg_var.h.set((-70000 as i64) as i16)",
            "seqg_var.h.set(5)",
            "seqg_var.d.set(((70001 as i64) as i16) as f64 + seqg_var.d.get())",
        ] {
            assert!(code.contains(line), "{line}\n{code}");
        }
    }

    #[test]
    fn an_integer_constant_has_the_type_c_gives_it() {
        let (code, diag) = translate(
            "program p\nint i;\nunsigned int u;\nchar *p;\ndouble d;\nss s {\nstate z {\n\
             when (i < 10u && i == 0xFFFFFFFF && p != 0L) {\nu = 0xFFFFFFFF;\nd = 3000000000;\n\
             p = 0u;\nd = 10l;\nd = 0x7FFFFFFFFFFFFFFF;\nd = 18446744073709551615u;\nu = 7;\n\
             } state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        for line in [
            "(seqg_var.i.get() as u32) < 10u32",
            "seqg_var.i.get() as u32 == 0xFFFFFFFFu32",
            "!seqg_var.p.get().is_null()",
            "seqg_var.u.set(0xFFFFFFFFu32)",
            "seqg_var.d.set(3000000000i64 as f64)",
            "seqg_var.p.set(seqg_rt::CharPtr::default())",
            "seqg_var.d.set(10i64 as f64)",
            "seqg_var.d.set(0x7FFFFFFFFFFFFFFFi64 as f64)",
            "seqg_var.d.set(18446744073709551615u64 as f64)",
            "seqg_var.u.set(7)",
        ] {
            assert!(code.contains(line), "{line}\n{code}");
        }
    }

    #[test]
    fn an_upper_case_hex_prefix_is_written_as_rust_takes_it() {
        let (code, diag) = translate(
            "program p\nint i;\nss s {\nstate z {\nwhen () {\ni = 0XfF;\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(code.contains("seqg_var.i.set(0xfF)"), "{code}");
    }

    #[test]
    fn dereferencing_a_string_reaches_its_first_character() {
        let (code, diag) = translate(
            "program p\nstring s;\nss s1 {\nstate z {\nwhen (*s) {\n*s = 'x';\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(code.contains("if seqg_var.s.at(0).get() != 0 {"), "{code}");
        assert!(code.contains("seqg_var.s.at(0).set(120)"), "{code}");
    }

    #[test]
    fn a_conditional_of_a_pointer_and_a_string_is_a_pointer() {
        let (code, diag) = translate(
            "program p\nchar *m;\nss s {\nstate z {\nwhen () {\nprintf(\"%s\", m ? m : \"\");\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(code.contains("seqg_rt::CharPtr::from_bytes("), "{code}");
    }

    #[test]
    fn a_store_through_a_pointer_writes_what_it_points_to() {
        // editSseq.st:161 `*s = '\0'`, opttVar.st:115 `xp++`
        let (code, diag) = translate(
            "program p\nchar *p;\nstring s;\nint k;\nint n;\n%%int *fp;\nss s1 {\nstate z {\n\
             when () {\n*p = 0;\np++;\np -= k;\nn = p - s;\nn = *p++;\n*fp = 1;\n} state z\n}\n}\n",
        );
        assert_eq!(
            diag,
            "t.st:15: error: snc-rs does not support assignment through a pointer\n"
        );
        for e in [
            "seqg_var.p.get().set(0);",
            "seqg_var.p.modify(|v| *v = v.offset(1));",
            "seqg_var.p.modify(|v| *v = v.offset(-(seqg_rhs as isize)));",
            "seqg_var.p.get().diff(&seqg_rt::Ptr::from(&seqg_var.s)) as i32",
            "let seqg_old = v.clone();",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn a_pointer_is_tested_and_compared_as_c_s() {
        // decl.st:69's `!complicated`
        let (code, diag) = translate(
            "program p\nint *p;\nint *q = 0;\ndouble **pd;\nint n;\nss s {\nstate z {\n\
             when (!p && q != 0 && p == q) {\npd = 0;\np = n ? q : 0;\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        for e in [
            "seqg_var.q.set(seqg_rt::Ptr::<i32>::default())",
            "if seqg_var.p.get().is_null() && !seqg_var.q.get().is_null() \
             && seqg_var.p.get() == seqg_var.q.get() {",
            "seqg_var.pd.set(seqg_rt::Ptr::<seqg_rt::Ptr<f64>>::default())",
            "if seqg_var.n.get() != 0 { seqg_var.q.get() } else { seqg_rt::Ptr::<i32>::default() }",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn c_library_arguments_convert_by_the_prototype() {
        let (code, diag) = translate(
            "program p\nint i;\ndouble d;\nss s {\nstate z {\nwhen () {\n\
             d = 2 * sqrt(i);\nepicsThreadSleep(1);\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(
            code.contains("seqg_var.d.set(2.0 * sqrt(seqg_var.i.get() as f64))"),
            "{code}"
        );
        assert!(code.contains("epicsThreadSleep(1.0);"), "{code}");
    }

    #[test]
    fn a_char_pointer_is_a_c_library_destination() {
        // XPS_trajectoryScan.st:165 sprintf(axisName[i], ...)
        let (code, diag) = translate(
            "program p\nchar *p;\nchar *cp[2];\nint *ip;\nss s {\nstate z {\nwhen () {\n\
             sprintf(p, \"x\");\nstrcpy(cp[1], \"y\");\nsprintf(\"lit\", \"x\");\nsprintf(ip, \"x\");\n\
             } state z\n}\n}\n",
        );
        assert_eq!(
            diag,
            "t.st:10: error: snc-rs does not support a write into a string literal\n\
             t.st:11: error: snc-rs does not support a cast between pointer types\n"
        );
        for e in [
            "sprintf(seqg_var.p.get(), \"x\"",
            "strcpy(seqg_var.cp.at(1).get(), \"y\")",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn sizeof_an_expression_is_the_size_of_its_type() {
        // sizeof.st:21 sizeof(*p)
        let (code, diag) = translate(
            "program p\nchar *p;\nstring s;\ndouble d[3];\nint *ip;\nint n;\nss a {\nstate z {\n\
             when () {\nn = sizeof(*p) + sizeof(s[1]) + sizeof(d[1]) + sizeof(*ip) + sizeof(ip+1);\n\
             n = sizeof(\"ab\");\n} state z\n}\n}\n",
        );
        assert_eq!(
            diag, "t.st:11: error: snc-rs cannot compute this sizeof\n",
            "{code}"
        );
        assert!(
            code.contains("seqg_var.n.set(1 + 1 + 8 + 4 + 8);"),
            "{code}"
        );
    }

    #[test]
    fn size_t_ranks_with_the_64_bit_integers() {
        let (code, diag) = translate(
            "program p\nstring s;\nint a[4];\nint n;\nss s1 {\nstate z {\nwhen () {\n\
             n = strlen(s) + 1;\nn = a[strlen(s)];\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(
            code.contains("seqg_var.n.set(usize::wrapping_add(strlen(&seqg_var.s), 1) as i32)"),
            "{code}"
        );
        assert!(
            code.contains("seqg_var.n.set(seqg_var.a.at(strlen(&seqg_var.s)).get())"),
            "{code}"
        );
    }

    #[test]
    fn epics_unit_test_reports_take_their_arguments_as_a_slice() {
        let (code, diag) = translate(
            "program p\nint i;\nss s {\nstate z {\nwhen () {\n\
             testOk(i == 1, \"i=%d\", i);\ntestDiag(\"done\");\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(
            code.contains(
                "testOk((seqg_var.i.get() == 1) as i32, \"i=%d\", &[seqg_rt::c::CArg::from(seqg_var.i.get())]);"
            ),
            "{code}"
        );
        assert!(code.contains("testDiag(\"done\", &[]);"), "{code}");
    }

    /// Each module holds each name once in its type namespace (structs,
    /// modules) and in its value namespace (functions, statics).
    fn assert_names_unique(items: &[Item]) {
        let (mut types, mut values) = (HashSet::new(), HashSet::new());
        for item in items {
            let (seen, name) = match item {
                Item::Struct(s) => (&mut types, &s.name),
                Item::Mod(name, inner) => {
                    assert_names_unique(inner);
                    (&mut types, name)
                }
                Item::Fn(f) => (&mut values, &f.name),
                Item::Const(c) => (&mut values, &c.name),
                _ => continue,
            };
            assert!(seen.insert(name), "{name} is declared twice");
        }
    }

    #[test]
    fn state_functions_and_masks_cannot_collide_across_state_sets() {
        // ss `a` (#0)'s state `b_1_c` and ss `a_0_b` (#1)'s state `c` both
        // get C's seqg_event_a_0_b_1_c and seqg_mask_a_0_b_1_c
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/gen_code/state_coll.st"
        ))
        .expect("read state_coll.st");
        let (mut snc, ast, prg) = analysed(&src);
        let mut items = gen_ss_code(&mut snc, &ast, &prg).expect("no snc bug");
        items.extend(crate::gen_tables::gen_tables(&mut snc, &ast, &prg));
        assert_eq!(String::from_utf8_lossy(snc.diagnostics()), "");
        assert_names_unique(&items);
    }

    #[test]
    fn unsigned_arithmetic_and_narrowing_stores_wrap_as_c() {
        // reviewer4's uwrap.st: each of these panics in a dev build when
        // emitted as `*v -= 1`
        let (code, diag) = translate(
            "program uwrap\nunsigned int u;\nshort h = 32767;\nunsigned char c;\n\
             ss a {\nstate s {\nwhen () {\nu--;\nh += 1;\nc -= 1;\nu = u * 2 + 1;\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        for want in [
            "seqg_var.u.modify(|v| *v = u32::wrapping_sub(*v, 1));",
            "seqg_var.h.modify(|v| *v = (*v as i32 + 1) as i16);",
            "seqg_var.c.modify(|v| *v = (*v as i32 - 1) as u8);",
            "seqg_var.u.set(u32::wrapping_add(u32::wrapping_mul(seqg_var.u.get(), 2), 1));",
        ] {
            assert!(code.contains(want), "{want}\n{code}");
        }
    }

    #[test]
    fn a_float_suffixed_constant_is_a_float() {
        // reviewer4's fsuf.st: as a double, 1.1 != (float)1.1
        let (code, diag) = translate(
            "program fsuf\nfloat f;\ndouble d;\nint r;\nss a {\nstate s {\nwhen () {\n\
             f = 1.1f;\nif (f == 1.1f) r = 1; else r = 2;\nd = .5F;\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        for want in [
            "seqg_var.f.set(1.1f32);",
            "if seqg_var.f.get() == 1.1f32 {",
            "seqg_var.d.set(0.5f32 as f64);",
        ] {
            assert!(code.contains(want), "{want}\n{code}");
        }
    }

    #[test]
    fn break_outside_a_loop_leaves_the_action_as_c_leaves_its_switch() {
        // reviewer4's brk.st: C's action function runs a when block as a
        // case of switch (seqg_trn), so break returns from it
        let (code, diag) = translate(
            "program brk\nint x;\nss a {\nstate s {\nwhen (x == 0) {\n\
             if (x == 0) break;\nwhile (x) { break; }\nx = 1;\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let words = code.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            words.contains("if seqg_var.x.get() == 0 { return; }"),
            "{code}"
        );
        assert!(
            words.contains("while seqg_var.x.get() != 0 { break; }"),
            "{code}"
        );

        // where C has no switch, gcc refuses them
        let (_, diag) = translate(
            "program brk\nint x;\nss a {\nstate s {\nentry {\nbreak;\n}\n\
             when () {\ncontinue;\n} state s\n}\n}\n",
        );
        assert_eq!(
            diag,
            "t.st:6: error: break statement not within loop or switch\n\
             t.st:9: error: continue statement not within a loop\n"
        );

        // inside escaped C, the escaped construct is what they leave
        let (code, diag) = translate(
            "program brk\nint x;\nss a {\nstate s {\nwhen () {\n\
             x = 0;\n%{ for (;;) { }%\nbreak;\n%{ } }%\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(
            code.contains("break;") && !code.contains("return;"),
            "{code}"
        );
    }

    #[test]
    fn an_assigned_foreign_global_is_a_cell() {
        // pvGet.st:26 and :116, and reviewer4's postf.st, whose `cnt++` once
        // came out as the new value
        let (code, diag) = translate(
            "program p\n%%int shared = 0;\n%%epicsMutexId mutex;\n%%int cnt;\n%%int limit;\n\
             int y;\nss a {\nstate s {\nwhen () {\nmutex = epicsMutexMustCreate();\n\
             epicsMutexMustLock(mutex);\nshared = y;\ny = shared;\ny = cnt++;\nshared += 2;\n\
             mutex = y;\ny = limit;\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        for e in [
            "mutex.set(epicsMutexMustCreate());",
            "epicsMutexMustLock(mutex.get());",
            "shared.set(seqg_var.y.get());",
            "seqg_var.y.set(shared.get());",
            "seqg_var.y.set( cnt.modify( |v| { let seqg_old = *v; *v = *v + 1; seqg_old }, ), );",
            "shared.modify(|v| *v = *v + 2);",
            "mutex.set(seqg_var.y.get() as _);",
            // never assigned, so a constant
            "seqg_var.y.set(limit);",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    const MEMBERS: &str = "program p\n\
        %%struct struct_t { int i; struct { double d; } s; };\n\
        %%union union_t { int i; struct struct_t *p; };\n\
        struct struct_t s = {1, {0.1}};\nunion union_t u = {13};\n\
        ss a {\nstruct struct_t *ps = &s;\nunion union_t *pu = &u;\nint *q;\n\
        state s {\nwhen () {\n";

    #[test]
    fn a_foreign_struct_member_is_a_field_and_a_union_member_a_view() {
        // member.st, foreignTypes.st
        let (code, diag) = translate(&format!(
            "{MEMBERS}if (s.i == 1)\ns.i++;\ns.s.d /= 3.0;\nps->i++;\nps->i = 2;\n\
             (*ps).i = 2;\npu->p = ps;\npu->p->s.d *= 3;\nu.i = pu->i;\nq = &s.i;\n\
             }} state s\n}}\n}}\n"
        ));
        assert_eq!(diag, "");
        let code = words(&code);
        for e in [
            "seqg_var.s.set(struct_t::from((1, (0.1,))));",
            "seqg_var.u.set(union_t::from((13,)));",
            "seqg_rt::arith::eq(seqg_var.s.field(|v| &mut v.i).get(), 1)",
            "seqg_var.s.field(|v| &mut v.i).modify(|v| *v = seqg_rt::arith::add(*v, 1) as _);",
            "seqg_var.s.field(|v| &mut v.s).field(|v| &mut v.d).modify(|v| *v = (*v as f64 / 3.0) as _);",
            "seqg_var.seqg_vars_a.ps.get().field(|v| &mut v.i).modify(|v| *v = seqg_rt::arith::add(*v, 1) as _);",
            "seqg_var.seqg_vars_a.pu.get().view(|v| v.p(), |v, x| v.set_p(x)).set(seqg_var.seqg_vars_a.ps.get());",
            "seqg_var.seqg_vars_a.pu.get().view(|v| v.p(), |v, x| v.set_p(x)).get().field(|v| &mut v.s)\
             .field(|v| &mut v.d).modify(|v| *v = seqg_rt::arith::mul(*v, 3) as _);",
            "seqg_var.u.view(|v| v.i(), |v, x| v.set_i(x)).set(seqg_var.seqg_vars_a.pu.get()\
             .view(|v| v.i(), |v, x| v.set_i(x)).get());",
            "seqg_var.seqg_vars_a.q.set(seqg_var.s.field(|v| &mut v.i).ptr());",
        ] {
            assert!(code.contains(&words(e)), "{e}\n{code}");
        }
        // `p->a` and `(*p).a` are one place
        let set = "seqg_var.seqg_vars_a.ps.get().field(|v| &mut v.i).set(2);";
        assert_eq!(code.matches(set).count(), 2, "{code}");
    }

    #[test]
    fn arithmetic_on_a_member_of_unknown_type_converts_in_rust() {
        // a guess that s.s.d is an int truncated it to a long here
        let (code, diag) = translate(
            "program p\n%%struct struct_t { int i; struct { double d; } s; };\n\
             struct struct_t s;\nlong l;\ndouble x;\nint i;\nss a {\nstate s {\nwhen () {\n\
             x = s.s.d * l;\ni = s.s.d < l;\ns.s.d *= 3;\ns.i++;\nx = i ? s.s.d : l;\n\
             } state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        let d = "seqg_var.s.field(|v| &mut v.s).field(|v| &mut v.d)";
        for e in [
            format!("seqg_var.x.set(seqg_rt::arith::mul({d}.get(), seqg_var.l.get()) as _);"),
            format!("seqg_rt::arith::lt({d}.get(), seqg_var.l.get())"),
            format!("{d}.modify(|v| *v = seqg_rt::arith::mul(*v, 3) as _);"),
            "seqg_var.s.field(|v| &mut v.i).modify(|v| *v = seqg_rt::arith::add(*v, 1) as _);"
                .into(),
            format!(
                "seqg_rt::arith::select(seqg_var.i.get() != 0, || {d}.get(), || seqg_var.l.get())"
            ),
        ] {
            assert!(code.contains(&words(&e)), "{e}\n{code}");
        }
    }

    #[test]
    fn two_operands_of_unknown_type_go_by_their_rust_types() {
        // seqg_rt::arith takes two structs or pointers as they are, and
        // converts a double member beside an int one
        let (code, diag) = translate(
            "program p\n%%struct struct_t { int i; struct { double d; } s; };\n\
             %%union union_t { int i; struct struct_t *p; };\n\
             struct struct_t s, s2, s3;\nunion union_t u, u2;\nint c;\ndouble x;\nss a {\n\
             state s {\nwhen () {\ns3 = c ? s : s2;\nc = u.p == u2.p;\ns.i *= 3;\n\
             x = s.s.d * s.i;\ns.s.d *= s.i;\nc = s.s.d > s.i;\nx = c ? s.s.d : s.i;\n\
             } state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        let p = |u| format!("seqg_var.{u}.view(|v| v.p(), |v, x| v.set_p(x)).get()");
        let i = "seqg_var.s.field(|v| &mut v.i)";
        let d = "seqg_var.s.field(|v| &mut v.s).field(|v| &mut v.d)";
        for e in [
            "seqg_var.s3.set(seqg_rt::arith::select(seqg_var.c.get() != 0, \
             || seqg_var.s.get(), || seqg_var.s2.get()));"
                .into(),
            format!(
                "seqg_var.c.set(seqg_rt::arith::eq({}, {}) as i32);",
                p("u"),
                p("u2")
            ),
            format!("{i}.modify(|v| *v = seqg_rt::arith::mul(*v, 3) as _);"),
            format!("seqg_var.x.set(seqg_rt::arith::mul({d}.get(), {i}.get()) as _);"),
            format!(
                "let seqg_rhs = {i}.get(); \
                 {d}.modify(|v| *v = seqg_rt::arith::mul(*v, seqg_rhs) as _);"
            ),
            format!("seqg_var.c.set(seqg_rt::arith::gt({d}.get(), {i}.get()) as i32);"),
            format!(
                "seqg_var.x.set(seqg_rt::arith::select(seqg_var.c.get() != 0, \
                 || {d}.get(), || {i}.get()) as _);"
            ),
        ] {
            assert!(code.contains(&words(&e)), "{e}\n{code}");
        }
    }

    #[test]
    fn a_foreign_operand_promotes_as_c_does() {
        // C's `(c ? s.uc : u) << 4` is 2048 for uc 0x80, `~` of it -129
        let (code, diag) = translate(
            "program p\n%%struct uc_t { unsigned char uc; short sh; };\n\
             struct uc_t s;\nunsigned char u;\nint c;\nint x;\nss a {\n\
             state s {\nwhen () {\nx = (c ? s.uc : u) << 4;\nx = -(c ? s.uc : u);\n\
             x = ~(c ? s.uc : u);\nx = (c ? s.uc : 0) << 4;\nx = -s.uc;\nx = ~s.uc;\n\
             x = +s.uc;\nx = s.uc << s.sh;\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        let uc = "seqg_var.s.field(|v| &mut v.uc).get()";
        let sel =
            |b: &str| format!("seqg_rt::arith::select(seqg_var.c.get() != 0, || {uc}, || {b})");
        let u = sel("seqg_var.u.get()");
        for e in [
            format!("seqg_var.x.set((seqg_rt::arith::promote({u}) << 4) as _);"),
            format!("seqg_var.x.set(seqg_rt::arith::neg({u}) as _);"),
            format!("seqg_var.x.set(!seqg_rt::arith::promote({u}) as _);"),
            format!(
                "seqg_var.x.set((seqg_rt::arith::promote({}) << 4) as _);",
                sel("0")
            ),
            format!("seqg_var.x.set(seqg_rt::arith::neg({uc}) as _);"),
            format!("seqg_var.x.set(!seqg_rt::arith::promote({uc}) as _);"),
            format!("seqg_var.x.set(seqg_rt::arith::promote({uc}) as _);"),
            format!(
                "seqg_var.x.set((seqg_rt::arith::promote({uc}) \
                 << seqg_var.s.field(|v| &mut v.sh).get()) as _);"
            ),
        ] {
            assert!(code.contains(&words(&e)), "{e}\n{code}");
        }
    }

    #[test]
    fn a_pointer_to_a_union_member_is_refused() {
        let (_, diag) = translate(&format!("{MEMBERS}q = &u.i;\n}} state s\n}}\n}}\n"));
        assert_eq!(
            diag,
            "t.st:12: error: snc-rs does not support a pointer to a union member\n"
        );
    }

    #[test]
    fn a_global_of_unknown_type_steps_in_its_cell() {
        let (code, diag) = translate(
            "program p\n%%foo_t g;\nint y;\nss a {\nstate s {\nwhen () {\ng++;\ny = --g;\n\
             } state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        for e in [
            "g.modify(|v| *v = seqg_rt::arith::add(*v, 1) as _);",
            "seqg_var.y.set( g.modify( |v| { *v = seqg_rt::arith::sub(*v, 1) as _; *v }, ) as _, );",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn stores_into_a_foreign_array_are_refused() {
        let (_, diag) = translate(
            "program p\n%%int arr[2];\nss a {\nstate s {\nwhen () {\narr[1] += 2;\n\
             } state s\n}\n}\n",
        );
        assert_eq!(
            diag,
            "t.st:6: error: snc-rs does not support assignment to a foreign variable\n"
        );
    }

    #[test]
    fn a_member_of_a_run_time_value_is_refused() {
        // pvTimeStamp.st:10; the runtime's WallTime has no secPastEpoch
        let (_, diag) = translate(
            "program p\nint t;\nassign t to \"x\";\nss a {\nstate s {\nwhen () {\n\
             printf(\"%d\\n\", pvTimeStamp(t).secPastEpoch);\n} state s\n}\n}\n",
        );
        assert_eq!(
            diag,
            "t.st:7: error: snc-rs does not support a member of a seq run-time value\n"
        );
    }

    #[test]
    fn a_pointer_stored_from_a_variable_points_into_its_cell() {
        // C's pointer sees later writes to the variable, as Ptr does
        let (code, diag) = translate(
            "program p\nstring s;\nchar buf[8];\nchar c;\nchar *g = s;\nchar *p;\nchar *q;\n\
             int m[2];\nassign m to {\"a\",\"b\"};\nint *mp;\nss a {\nstate z {\nwhen () {\n\
             char *l = buf;\np = s;\np = (c ? buf : \"x\");\nc = strlen(q = &s[1]);\np = &c;\n\
             mp = &m[1];\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        for e in [
            "seqg_var.g.set(seqg_rt::Ptr::from(&seqg_var.s));",
            "seqg_rt::SeqVar::new(seqg_rt::Ptr::from(&seqg_var.buf));",
            "seqg_var.p.set(seqg_rt::Ptr::from(&seqg_var.s));",
            "seqg_rt::Ptr::from(&seqg_var.buf)\n",
            "seqg_rt::CharPtr::from_bytes(\"x\".as_bytes())\n",
            "let seqg_rhs = seqg_var.s.at(1).ptr();",
            "seqg_var.p.set(seqg_var.c.ptr());",
            "seqg_var.mp.set(seqg_rt::Ptr::from(&seqg_var.m).offset(1));",
        ] {
            assert!(code.contains(e), "{e} in {code}");
        }
    }

    #[test]
    fn a_pointer_into_a_multi_pv_array_spans_its_cells() {
        // reviewer2's mpv.st: &m[1] is m + 1, as C lays the cells out as
        // one array; &m, a pointer to the whole array, has nothing to point at
        let (code, diag) = translate(
            "program p\nint m[2];\nassign m to {\"a\",\"b\"};\nint *mp;\nint (*ap)[2];\nint n;\n\
             ss a {\nstate z {\nwhen () {\nmp = m;\nmp = &m[n];\nn = &m[1] - mp;\nap = &m;\n\
             } state z\n}\n}\n",
        );
        assert_eq!(
            diag,
            "t.st:13: error: snc-rs does not support a pointer to a multi-PV array\n"
        );
        for e in [
            "seqg_var.mp.set(seqg_rt::Ptr::from(&seqg_var.m));",
            "seqg_var.mp.set(seqg_rt::Ptr::from(&seqg_var.m).offset(seqg_var.n.get() as isize));",
            "seqg_rt::Ptr::from(&seqg_var.m).offset(1).diff(&seqg_var.mp.get())",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn a_void_pointer_converts_to_and_from_a_typed_one() {
        // void.st: void *p = &x; testOk1(*(int *)p == x)
        let (code, diag) = translate(
            "program p\nint x = 42;\nvoid *p = &x;\nint *q;\nint n;\nss a {\nstate z {\nwhen (p) {\n\
             q = (int *)p;\nn = *(int *)p;\np = q;\np = 0;\np = (void *)&x;\nn = p == &x;\n\
             printf(\"%p\", p);\nstrcpy(p, \"a\");\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        for e in [
            "seqg_var.p.set(seqg_var.x.ptr().erase());",
            "!seqg_var.p.get().is_null()",
            "seqg_var.q.set(seqg_var.p.get().cast::<i32>());",
            "seqg_var.n.set(seqg_var.p.get().cast::<i32>().get());",
            "seqg_var.p.set(seqg_var.q.get().erase());",
            "seqg_var.p.set(seqg_rt::Ptr::<std::ffi::c_void>::default());",
            "seqg_var.p.get() == seqg_var.x.ptr().erase()",
            "seqg_rt::c::CArg::from(seqg_var.p.get().addr())",
            "strcpy(seqg_var.p.get().cast::<std::ffi::c_char>(), \"a\")",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn a_void_pointer_neither_moves_nor_dereferences() {
        // C has no `void` to read and no size to step by
        let (_, diag) = translate(
            "program p\nvoid *p;\nlong n;\nss a {\nstate z {\nwhen () {\n\
             p = p + 1;\np++;\nn = *p;\nn = p - p;\np = (void *)n;\n} state z\n}\n}\n",
        );
        assert_eq!(
            diag,
            "t.st:7: error: snc-rs does not support arithmetic on a void pointer\n\
             t.st:8: error: snc-rs does not support arithmetic on a void pointer\n\
             t.st:9: error: snc-rs does not support a dereference of a void pointer\n\
             t.st:10: error: snc-rs does not support arithmetic on a void pointer\n\
             t.st:11: error: snc-rs does not support a cast of an integer to a pointer\n"
        );
    }

    #[test]
    fn a_pv_stat_variable_holds_a_pv_stat() {
        // pvGetCancel.st:25
        let (code, diag) = translate(
            "program p\nint x;\nassign x to \"x\";\ntypename pvStat status;\nint n;\n\
             ss a {\nstate z {\nwhen () {\nstatus = pvGet(x);\nn = status == pvStatOK;\n\
             n = status;\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        for e in [
            "seqg_var.status.set( seqg_env.pv_get_tmo(",
            "seqg_var.status.get() == seqg_rt::PvStat::Ok",
            "seqg_var.n.set((seqg_var.status.get() as i32) as _);",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn a_pv_stat_variable_takes_no_number() {
        // C's pvStat is an enum, which holds any int
        let (_, diag) = translate(
            "program p\nint x;\nassign x to \"x\";\ntypename pvStat status;\n\
             ss a {\nstate z {\nwhen () {\nstatus = 1;\nstatus++;\nstatus = pvSeverity(x);\n\
             } state z\n}\n}\n",
        );
        assert_eq!(
            diag,
            "t.st:8: error: snc-rs does not support a conversion to an enum\n\
             t.st:9: error: snc-rs does not support arithmetic on an enum\n\
             t.st:10: error: snc-rs does not support a conversion to an enum\n"
        );
    }

    #[test]
    fn a_pointer_plus_n_is_its_offset() {
        // sncqxbpm.st:110
        let (code, diag) = translate(
            "program p\nstring r;\nstring s;\nchar *p;\nchar buf[8];\nint n;\nss a {\nstate z {\n\
             when () {\nstrcpy(r, s+1);\nstrcpy(r, n+p);\nstrcpy(r, buf+n);\nstrcpy(r, \"abc\"+2);\n\
             strcpy(r, s-1);\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        for call in [
            "seqg_rt::Ptr::from(&seqg_var.s).offset(1)",
            "seqg_var.p.get().offset(seqg_var.n.get() as isize)",
            "seqg_rt::Ptr::from(&seqg_var.buf).offset(seqg_var.n.get() as isize)",
            "seqg_rt::CharPtr::from_bytes(\"abc\".as_bytes()).offset(2)",
            "seqg_rt::Ptr::from(&seqg_var.s).offset(-1)",
        ] {
            assert!(
                code.contains(&format!("strcpy(&seqg_var.r, {call})")),
                "{code}"
            );
        }
    }

    #[test]
    fn epics_snprintf_takes_its_size_as_size_t() {
        // EnsembleTrajectoryScan.st:418
        let (code, diag) = translate(
            "program p\nchar m[40];\nint n;\nss a {\nstate z {\nwhen () {\n\
             epicsSnprintf(m, n, \"%d\", n);\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(
            code.contains(
                "epicsSnprintf(&seqg_var.m, seqg_var.n.get() as usize, \"%d\", \
                 &[seqg_rt::c::CArg::from(seqg_var.n.get())]);"
            ),
            "{code}"
        );
    }

    #[test]
    fn a_char_pointer_parameter_converts_its_argument_as_c_does() {
        // MM4005_trajectoryScan.st:499 epicsStrtok_r(0, ",", &tok_save)
        let (code, diag) = translate(
            "program p\nstring s;\nchar *p;\nchar *tok;\nss a {\nstate z {\nwhen () {\n\
             p = epicsStrtok_r(0, \",\", &tok);\np = strchr(&s[2], ',');\n\
             p = strstr(s, \"ab\");\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        for stmt in [
            "seqg_var.p.set(epicsStrtok_r(seqg_rt::CharPtr::default(), \",\", &seqg_var.tok));",
            "seqg_var.p.set(strchr(seqg_var.s.at(2).ptr(), 44));",
            "seqg_var.p.set(strstr(seqg_rt::Ptr::from(&seqg_var.s), \"ab\"));",
        ] {
            assert!(code.contains(stmt), "{stmt}\n{code}");
        }
    }

    #[test]
    fn a_variadic_pointer_is_its_address_unless_it_points_to_char() {
        // array.st:46 testDiag("a4ps[%d]=%p", i, a4ps[i])
        let (code, diag) = translate(
            "program p\nstring s;\nint a[3];\nint *ip;\nchar *cp;\nint x;\nss a {\nstate z {\n\
             when () {\nprintf(\"%p %p %p %p %s %s\", ip, a, &x, &a[1], s, cp);\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        let args = [
            "seqg_var.ip.get().addr()",
            "seqg_rt::Ptr::from(&seqg_var.a).addr()",
            "seqg_var.x.ptr().addr()",
            "seqg_var.a.at(1).ptr().addr()",
            "&seqg_var.s",
            "seqg_var.cp.get()",
        ]
        .map(|a| format!("seqg_rt::c::CArg::from({a})"))
        .join(", ");
        assert!(code.contains(&format!("&[{args}]")), "{code}");
    }

    #[test]
    fn a_pointer_plus_offsets_reads_on_past_a_terminator() {
        // reviewer4's tail.st: C's s+1+2 reads on as s+3 does, past a
        // terminator at s+1
        let (code, diag) = translate(
            "program p\nstring r;\nstring s;\nstring sa[3];\nchar cc[2][8];\nchar c;\nchar *p;\n\
             int k;\nss a {\nstate z {\nwhen () {\nstrcpy(r, s+1+2);\nstrcpy(r, sa[1]+2);\n\
             strcpy(r, cc[1]+2);\nc = *(s+1);\nc = (s+1)[k];\nc = *(p+1);\nc = p[k];\n\
             strcpy(r, 1+(s+k));\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        for e in [
            "strcpy(&seqg_var.r, seqg_rt::Ptr::from(&seqg_var.s).offset(1).offset(2))",
            "strcpy(&seqg_var.r, seqg_rt::Ptr::from(&seqg_var.sa.at(1)).offset(2))",
            "strcpy(&seqg_var.r, seqg_rt::Ptr::from(&seqg_var.cc.at(1)).offset(2))",
            "seqg_var.c.set(seqg_rt::Ptr::from(&seqg_var.s).offset(1).get())",
            "seqg_var.c.set(seqg_rt::Ptr::from(&seqg_var.s).offset(1).offset(seqg_var.k.get() as isize).get())",
            "seqg_var.c.set(seqg_var.p.get().offset(1).get())",
            "seqg_var.c.set(seqg_var.p.get().offset(seqg_var.k.get() as isize).get())",
            "strcpy(&seqg_var.r, seqg_rt::Ptr::from(&seqg_var.s).offset(seqg_var.k.get() as isize).offset(1))",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn an_integer_constant_beside_a_value_of_unknown_type_stays_a_literal() {
        // change.st:32's `rand() % 10`: seqg_rt::arith converts the literal,
        // an int, and h's result by the companion's type; a use of h in
        // escaped C declares no type, nor a typedef name x's; a null pointer
        // constant is the value's default
        let (code, diag) = translate(
            "program p\n%%static int g(void) { return h(); }\n%%static char *c(void);\n\
             %%foo_t x;\nint y;\ndouble d;\nss a {\nstate s {\nwhen () {\ny = h() * 2;\n\
             y = h() == 0;\ny = 1 << h();\ny = y ? 1 : h();\ny = x + 1;\ny = 0 != c();\n\
             d = h() * 2.0;\ny = g() + 1;\ny = h() < 3;\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        for e in [
            "seqg_var.y.set(seqg_rt::arith::mul(h(), 2) as _)",
            "seqg_var.y.set(!seqg_rt::c::truth(h()) as i32)",
            "seqg_var.y.set(1 << h())",
            "seqg_var.y.set(seqg_rt::arith::select(seqg_var.y.get() != 0, || 1, || h()) as _)",
            "seqg_var.y.set(seqg_rt::arith::add(x, 1) as _)",
            "seqg_var.y.set(seqg_rt::c::truth(c()) as i32)",
            "seqg_var.d.set(h() as f64 * 2.0)",
            "seqg_var.y.set(g() + 1)",
            "seqg_var.y.set(seqg_rt::arith::lt(h(), 3) as i32)",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn an_snl_function_call_converts_to_the_parameter_types() {
        // funcdef.st:26's pvSetX(1.0) passes an int
        let (code, diag) = translate(
            "program p\nint x;\nvoid pvSetX(int val);\nss a {\nstate s {\nwhen () {\n\
             pvSetX(1.5);\nx = twice(2.5) + 1;\n} state s\n}\n}\n\
             int twice(double d) {\nreturn d;\n}\nvoid pvSetX(int val) {\nx = val;\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        for e in [
            "pvSetX(seqg_env, 1.5 as i32);",
            "seqg_var.x.set(twice(seqg_env, 2.5) + 1);",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn ssid_and_pvar_name_the_env_and_the_variables() {
        // userfunc.st:31; seqCom.h:132-133 define them for the generated C
        let (code, diag) = translate(
            "program p\nint i;\nss a {\nstate s {\nwhen () {\n\
             incr(ssId, &i);\nset_i(pVar, 1);\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        for e in [
            "let seqg_var = seqg_env.var();",
            "incr(seqg_env, &seqg_var.i);",
            "set_i(seqg_var, 1);",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn a_value_of_unknown_type_is_tested_against_its_default() {
        // control.st:37; the companion's x may be a number or a pointer
        let (code, diag) = translate(
            "program p\n%%foo_t x;\nint y;\nassign y to \"y\";\nss a {\nstate s {\nwhen (h()) {\n\
             if (x) y = 1;\nwhile (x) y = 2;\ny = !x;\ny = y && x;\ny = x ? 1 : 2;\n\
             x ? h() : h();\ny = pvPutComplete(y, 1, x);\nif (y) y = 3;\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        for e in [
            "if seqg_rt::c::truth(h()) {",
            "if seqg_rt::c::truth(x) {",
            "while seqg_rt::c::truth(x) {",
            "seqg_var.y.set(!seqg_rt::c::truth(x) as i32)",
            "seqg_var.y.set((seqg_var.y.get() != 0 && seqg_rt::c::truth(x)) as i32)",
            "seqg_var.y.set( if seqg_rt::c::truth(x) { 1 } else { 2 }, )",
            "if seqg_rt::c::truth(x) { h(); } else { h(); }",
            "seqg_rt::ChId(0), // y 1, seqg_rt::c::truth(x), None,",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn a_for_condition_of_unknown_type_is_tested_against_its_default() {
        // reviewer4: `for (;x;)` came out as `while x != 0`, which builds
        // only for an integer x
        let (code, diag) = translate(
            "program p\n%%foo_t x;\nint i;\nss a {\nstate s {\nwhen () {\n\
             for (;x;) i = 1;\nfor (i = 0; h(); i++) i = 2;\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        for e in [
            "while seqg_rt::c::truth(x) {",
            "while seqg_rt::c::truth(h()) {",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }

    #[test]
    fn other_pointer_arithmetic_stays_refused() {
        let (_, diag) = translate(
            "program p\nstring r;\nstring s;\nchar *p;\nss b {\nstate z {\nwhen () {\n\
             strcpy(r, s+s);\np *= 2;\n} state z\n}\n}\n",
        );
        assert_eq!(
            diag,
            "t.st:8: error: snc-rs does not support pointer arithmetic\n\
             t.st:9: error: snc-rs does not support pointer arithmetic\n"
        );
    }

    #[test]
    fn escaped_c_at_a_block_start_runs_with_the_statements() {
        // ml_monoCtl.st:610-611: in a function body C's block definitions
        // are statements that reach the program's variables
        let (code, diag) = translate(
            "program p\nint x;\nss a {\nstate s {\nwhen () {\n\
             %%printf(\"%d\\n\",\n%%  x);\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(
            code.contains(r###"seqg_escaped!(seqg_env, r#"printf("%d\n","#, r#"x);"#);"###),
            "{code}"
        );

        // and one that opens a bracket closes it in a later statement
        let (code, diag) = translate(
            "program p\nint x;\nss a {\nstate s {\nwhen () {\n\
             %%for (;;) {\nx = 1;\n%%}\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let words = code.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            words.contains(r###"seqg_escaped!( seqg_env, r#"for (;;) {"#, { // t.st:7 seqg_var.x.set(1); }, r#"}"# );"###),
            "{code}"
        );
    }

    #[test]
    fn break_and_continue_bind_through_escaped_c_as_in_c() {
        let words = |src: &str| {
            let (code, diag) = translate(src);
            assert_eq!(diag, "", "{code}");
            code.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        // reviewer4's escont.st and esdefn.st: an escaped if is not a loop,
        // so continue runs the SNL for loop's i++ (hrCtl.st:572's shape)
        for (head, line) in [("x = 0;\n", 10), ("", 9)] {
            let code = words(&format!(
                "program p\nint i;\nint x;\nss a {{\nstate s {{\nwhen () {{\n\
                 for (i = 0; i < 3; i++) {{\n{head}%%if (1) {{\ncontinue;\n%%}}\nx = 1;\n}}\n\
                 }} state s\n}}\n}}\n"
            ));
            assert!(
                code.contains(&format!(
                    "// t.st:{line} seqg_var.i.modify(|v| *v = *v + 1); continue; }}, r#\"}}\"#"
                )),
                "{code}"
            );
        }
        // esbrk.st: break leaves C's switch (seqg_trn), not the escaped if
        let code = words(
            "program p\nint x;\nss a {\nstate s {\nwhen () {\nx = 0;\n%%if (1) {\nbreak;\n%%}\n\
             } state s\n}\n}\n",
        );
        assert!(code.contains("{ // t.st:8 return; }"), "{code}");
        // an escaped loop binds both; an if inside it is transparent
        let code = words(
            "program p\nint x;\nss a {\nstate s {\nwhen () {\nx = 0;\n\
             %%while (x) { if (x > 1) {\nbreak;\ncontinue;\n%%} }\n} state s\n}\n}\n",
        );
        assert!(
            code.contains("{ // t.st:8 break; // t.st:9 continue; }"),
            "{code}"
        );
    }

    #[test]
    fn jumps_rust_cannot_make_through_escaped_c_are_refused() {
        let (_, diag) = translate(
            "program p\nint x;\nss a {\nstate s {\nwhen () {\nx = 0;\n\
             %%switch (x) { case 1:\nbreak;\n%%}\n\
             %%for (x = 0; x < 2; x++) {\ncontinue;\n%%}\n\
             %%do {\ncontinue;\n%%} while (x);\n\
             %%for (;;) {\ncontinue;\n%%}\n} state s\n}\n}\n",
        );
        assert_eq!(
            diag,
            "t.st:8: error: snc-rs does not support a break out of an escaped C switch\n\
             t.st:11: error: snc-rs does not support a continue of an escaped C loop \
             that runs code before its next test\n\
             t.st:14: error: snc-rs does not support a continue of an escaped C loop \
             that runs code before its next test\n"
        );
    }

    #[test]
    fn a_call_of_an_escaped_c_variadic_function_passes_the_rest_as_cargs() {
        // XPS_trajectoryScan.st:143's printMessage; the look-alikes in a
        // comment and a literal declare nothing
        let (code, diag) = translate(
            "program p\n%%static void printMessage(const char *pformat, ...);\n\
             %%static int twice(int x); /* twice(int, ...) */\n\
             %%static void f(double d, ...);\n\
             %%static const char *s = \"g(a, ...)\";\nint y;\nss a {\nstate s {\nwhen () {\n\
             printMessage(\"%d\\n\", y);\ny = twice(y);\nf(y);\nf(y, y);\ng(y);\nprintf(\"%d\", y);\n\
             } state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        let code = words(&code);
        for line in [
            "printMessage(\"%d\\n\", &[seqg_rt::c::CArg::from(seqg_var.y.get())]);",
            "seqg_var.y.set(twice(seqg_var.y.get()));",
            "f(seqg_var.y.get() as f64, &[]);",
            "f(seqg_var.y.get() as f64, &[seqg_rt::c::CArg::from(seqg_var.y.get())]);",
            "g(seqg_var.y.get());",
            "printf(\"%d\", &[seqg_rt::c::CArg::from(seqg_var.y.get())]);",
        ] {
            assert!(code.contains(line), "{line} in {code}");
        }
    }

    #[test]
    fn a_call_has_the_type_its_escaped_c_prototype_gives_it() {
        // pf4.st:135's fillShowArrays; gcc types a call by the prototype
        let (code, diag) = translate(
            "program p\n%%static double h(void);\n%{\n#include <x.h>\nstatic unsigned int\n\
             u(int n);\n}%\nint y;\ndouble d;\nss a {\nstate s {\n\
             when () {\nd = h() * 2;\ny = u(y) == -1;\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(code.contains("h() * 2.0"), "{code}");
        assert!(
            code.contains("u(seqg_var.y.get()) == (-1 as i64) as u32"),
            "{code}"
        );
    }

    #[test]
    fn an_escaped_c_call_converts_its_arguments_by_the_prototype() {
        // pf4.st:144's thickZ; gcc converts each argument to its parameter's
        // type, so 70000 reaches a short as 4464
        let (code, diag) = translate(
            "program p\n%%static int f(short a, void (*cb)(int, int), double, char *s, unsigned);\n\
             %%static int g();\nint y;\nshort h;\nstring s;\nss a {\nstate z {\nwhen () {\n\
             y = f(70000, 0, y, s, h);\ny = g(h);\n} state z\n}\n}\n",
        );
        assert_eq!(diag, "");
        assert!(
            code.contains(
                "f((70000 as i64) as i16, 0, seqg_var.y.get() as f64, &seqg_var.s, \
                 seqg_var.h.get() as u32)"
            ),
            "{code}"
        );
        // `()` declares no prototype
        assert!(code.contains("g(seqg_var.h.get())"), "{code}");
    }

    #[test]
    fn a_variable_declared_in_escaped_c_has_its_declared_type() {
        // femto.st:52's powers
        let (code, diag) = translate(
            "program p\n%%static int x;\n%%static double a = f(1, 2), b;\n\
             %%static int powers[] = {5,6};\n%{\nshort m[2][3];\n}%\nint y;\ndouble d;\nss z {\n\
             state s {\nwhen () {\ny = x + 1;\nd = b * 2;\nd = pow(10, powers[y]);\n\
             y = m[1][y] + 1;\n} state s\n}\n}\n",
        );
        assert_eq!(diag, "");
        for e in [
            "seqg_var.y.set(x + 1)",
            "seqg_var.d.set(b * 2.0)",
            "seqg_var.d.set(pow(10.0, powers[seqg_var.y.get() as usize] as f64))",
            "seqg_var.y.set(m[1][seqg_var.y.get() as usize] as i32 + 1)",
        ] {
            assert!(code.contains(e), "{e}\n{code}");
        }
    }
}
