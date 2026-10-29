//! The SNL grammar (C `snl.lem`): operator precedence, the rules in
//! lemon syntax, and each rule's action.

use std::sync::{Arc, OnceLock};

use crate::snc::driver::{Exit, Snc};
use crate::snc::lemon::{Assoc, Lalr, RuleSpec, Tables};
use crate::snc::seq_prim_types::PrimType;
use crate::snc::types::NodeTag::*;
use crate::snc::types::{Ast, NodeId, NodeTag, Token, TypeId};
use crate::snc::var_types::{ForeignTag, mk_decl, mk_decls};

/// A grammar symbol's value (C `YYMINORTYPE`). The `error` symbol carries
/// the token that caused the error.
#[derive(Debug)]
pub(crate) enum Minor {
    None,
    Tok(Token),
    Node(Option<NodeId>),
    Type(TypeId),
    Prim(PrimType),
}

impl Minor {
    fn tok(&self) -> Token {
        match self {
            Minor::Tok(t) => t.clone(),
            m => panic!("expected a token, got {m:?}"),
        }
    }

    /// C `x.str` of a `NAME` token.
    fn name(&self) -> Arc<str> {
        self.tok().str.expect("NAME token without text")
    }

    fn node(&self) -> Option<NodeId> {
        match self {
            Minor::Node(n) => *n,
            m => panic!("expected a node, got {m:?}"),
        }
    }

    fn ty(&self) -> TypeId {
        match self {
            Minor::Type(t) => *t,
            m => panic!("expected a type, got {m:?}"),
        }
    }

    fn prim(&self) -> PrimType {
        match self {
            Minor::Prim(p) => *p,
            m => panic!("expected a primitive type, got {m:?}"),
        }
    }
}

/// What rule actions work on: the extra argument (C `presult`) and the
/// state C keeps in globals.
pub(crate) struct Cx<'a> {
    pub snc: &'a mut Snc,
    pub ast: &'a mut Ast,
    pub result: Option<NodeId>,
}

impl Cx<'_> {
    fn node(&mut self, tag: NodeTag, tok: Token, children: &[Option<NodeId>]) -> Option<NodeId> {
        Some(self.ast.node(tag, tok, children))
    }

    fn link(&mut self, ep1: Option<NodeId>, ep2: Option<NodeId>) -> Option<NodeId> {
        self.ast.link_node(ep1, ep2)
    }
}

/// A rule action: the right-hand side values in, the left-hand side value
/// out.
pub(crate) type Action = fn(&mut Cx<'_>, Vec<Minor>) -> Result<Minor, Exit>;

pub(crate) struct SnlRule {
    pub text: &'static str,
    pub action: Option<Action>,
}

fn arr<const N: usize>(m: Vec<Minor>) -> [Minor; N] {
    m.try_into().expect("rule arity")
}

fn ok(n: Option<NodeId>) -> Result<Minor, Exit> {
    Ok(Minor::Node(n))
}

fn nil() -> Result<Minor, Exit> {
    Ok(Minor::Node(None))
}

fn ty(t: TypeId) -> Result<Minor, Exit> {
    Ok(Minor::Type(t))
}

fn prim(p: PrimType) -> Result<Minor, Exit> {
    Ok(Minor::Prim(p))
}

macro_rules! rule {
    ($text:literal) => {
        SnlRule {
            text: $text,
            action: None,
        }
    };
    ($text:literal, |$cx:pat_param, [$($m:pat),* $(,)?]| $body:expr) => {
        SnlRule {
            text: $text,
            action: Some(|$cx, m| {
                let [$($m),*] = arr(m);
                $body
            }),
        }
    };
}

/// The `%left`/`%right` lines, lowest precedence first.
pub(crate) const PRECEDENCE: [(Assoc, &str); 14] = [
    (
        Assoc::Right,
        "EQUAL ADDEQ SUBEQ ANDEQ OREQ DIVEQ MULEQ MODEQ LSHEQ RSHEQ XOREQ",
    ),
    (Assoc::Right, "QUESTION COLON"),
    (Assoc::Left, "OROR"),
    (Assoc::Left, "ANDAND"),
    (Assoc::Left, "VBAR"),
    (Assoc::Left, "CARET"),
    (Assoc::Left, "AMPERSAND"),
    (Assoc::Left, "EQ NE"),
    (Assoc::Left, "GT GE LE LT"),
    (Assoc::Left, "LSHIFT RSHIFT"),
    (Assoc::Left, "ADD SUB"),
    (Assoc::Left, "ASTERISK SLASH MOD"),
    (Assoc::Right, "NOT TILDE INCR DECR SIZEOF PRE"),
    (Assoc::Left, "LBRACKET LPAREN POINTER PERIOD POST"),
];

/// The rules, in snl.lem order.
pub(crate) static RULES: [SnlRule; 224] = [
    rule!(
        "program ::= PROGRAM NAME(n) program_param(pp) initial_defns(ds) entry(en) state_sets(ss) exit(ex) final_defns(fs).",
        |cx, [_, n, pp, ds, en, ss, ex, fs]| {
            cx.result = cx.node(
                DProg,
                n.tok(),
                &[
                    pp.node(),
                    ds.node(),
                    en.node(),
                    ss.node(),
                    ex.node(),
                    fs.node(),
                ],
            );
            Ok(Minor::None)
        }
    ),
    rule!("program_param(r) ::= LPAREN string(x) RPAREN.", |_,
                                                            [
        _,
        x,
        _,
    ]| Ok(
        x
    )),
    rule!("program_param(r) ::= .", |_, []| nil()),
    rule!(
        "initial_defns(r) ::= initial_defns(xs) initial_defn(x).",
        |cx, [xs, x]| ok(cx.link(xs.node(), x.node()))
    ),
    rule!("initial_defns(r) ::= .", |_, []| nil()),
    rule!("initial_defn(r) ::= assign(x).", |_, [x]| Ok(x)),
    rule!("initial_defn(r) ::= monitor(x).", |_, [x]| Ok(x)),
    rule!("initial_defn(r) ::= sync(x).", |_, [x]| Ok(x)),
    rule!("initial_defn(r) ::= syncq(x).", |_, [x]| Ok(x)),
    rule!("initial_defn(r) ::= declaration(x).", |_, [x]| Ok(x)),
    rule!("initial_defn(r) ::= option(x).", |_, [x]| Ok(x)),
    rule!("initial_defn(r) ::= c_code(x).", |_, [x]| Ok(x)),
    rule!("initial_defn(r) ::= funcdef(x).", |_, [x]| Ok(x)),
    rule!("initial_defn(r) ::= structdef(x).", |_, [x]| Ok(x)),
    rule!("final_defns(r) ::= final_defns(xs) final_defn(x).", |cx,
                                                                [
        xs,
        x,
    ]| {
        ok(cx.link(xs.node(), x.node()))
    }),
    rule!("final_defns(r) ::= .", |_, []| nil()),
    rule!("final_defn(r) ::= c_code(x).", |_, [x]| Ok(x)),
    rule!("final_defn(r) ::= funcdef(x).", |_, [x]| Ok(x)),
    rule!("final_defn(r) ::= structdef(x).", |_, [x]| Ok(x)),
    rule!(
        "assign(r) ::= ASSIGN variable(v) to string(t) SEMICOLON.",
        |cx, [_, v, _, t, _]| ok(cx.node(DAssign, v.tok(), &[None, t.node()]))
    ),
    rule!(
        "assign(r) ::= ASSIGN variable(v) subscript(s) to string(t) SEMICOLON.",
        |cx, [_, v, s, _, t, _]| {
            let n1 = cx.node(EConst, s.tok(), &[]);
            ok(cx.node(DAssign, v.tok(), &[n1, t.node()]))
        }
    ),
    rule!(
        "assign(r) ::= ASSIGN variable(v) to LBRACE(t) strings(ss) RBRACE SEMICOLON.",
        |cx, [_, v, _, t, ss, _, _]| {
            let n1 = cx.node(EInit, t.tok(), &[ss.node()]);
            ok(cx.node(DAssign, v.tok(), &[None, n1]))
        }
    ),
    rule!("assign(r) ::= ASSIGN variable(v) SEMICOLON.", |cx,
                                                          [
        _,
        v,
        _,
    ]| ok(
        cx.node(DAssign, v.tok(), &[None, None])
    )),
    rule!("to ::= TO."),
    rule!("to ::= ."),
    rule!("strings(r) ::= strings(xs) COMMA string(x).", |cx,
                                                          [
        xs,
        _,
        x,
    ]| ok(
        cx.link(xs.node(), x.node())
    )),
    rule!("strings(r) ::= string(x).", |_, [x]| Ok(x)),
    rule!("strings(r) ::= .", |_, []| nil()),
    rule!(
        "monitor(r) ::= MONITOR variable(v) opt_subscript(s) SEMICOLON.",
        |cx, [_, v, s, _]| ok(cx.node(DMonitor, v.tok(), &[s.node()]))
    ),
    rule!(
        "monitor(r) ::= MONITOR variable(v) opt_subscript(sub) error SEMICOLON.",
        |cx, [_, v, sub, _, _]| {
            let r = cx.node(DMonitor, v.tok(), &[sub.node()]);
            let what = if sub.node().is_some() {
                "subscript or "
            } else {
                ""
            };
            cx.snc.report(format_args!("expected {what}';'\n"));
            ok(r)
        }
    ),
    rule!(
        "sync(r) ::= SYNC variable(v) opt_subscript(s) to event_flag(f) SEMICOLON.",
        |cx, [_, v, s, _, f, _]| {
            let n1 = cx.node(EVar, f.tok(), &[]);
            ok(cx.node(DSync, v.tok(), &[s.node(), n1]))
        }
    ),
    rule!(
        "sync(r) ::= SYNC variable(v) opt_subscript(s) to event_flag(f) error SEMICOLON.",
        |cx, [_, v, s, _, f, _, _]| {
            let n1 = cx.node(EVar, f.tok(), &[]);
            let r = cx.node(DSync, v.tok(), &[s.node(), n1]);
            cx.snc.report(format_args!("expected ';'\n"));
            ok(r)
        }
    ),
    rule!(
        "syncq(r) ::= SYNCQ variable(v) opt_subscript(s) to event_flag(f) syncq_size(n) SEMICOLON.",
        |cx, [_, v, s, _, f, n, _]| {
            let n1 = cx.node(EVar, f.tok(), &[]);
            ok(cx.node(DSyncq, v.tok(), &[s.node(), n1, n.node()]))
        }
    ),
    rule!(
        "syncq(r) ::= SYNCQ variable(v) opt_subscript(s) syncq_size(n) SEMICOLON.",
        |cx, [_, v, s, n, _]| ok(cx.node(DSyncq, v.tok(), &[s.node(), None, n.node()]))
    ),
    rule!("event_flag(r) ::= NAME(x).", |_, [x]| Ok(x)),
    rule!("variable(r) ::= NAME(x).", |_, [x]| Ok(x)),
    rule!("syncq_size(r) ::= INTCON(n).", |cx, [n]| ok(cx.node(
        EConst,
        n.tok(),
        &[]
    ))),
    rule!("syncq_size(r) ::= .", |_, []| nil()),
    rule!("opt_subscript(r) ::= subscript(s).", |cx, [s]| ok(cx.node(
        EConst,
        s.tok(),
        &[]
    ))),
    rule!("opt_subscript(r) ::= .", |_, []| nil()),
    rule!("subscript(r) ::= LBRACKET INTCON(n) RBRACKET.", |_,
                                                            [
        _,
        n,
        _,
    ]| Ok(
        n
    )),
    rule!(
        "declaration(r) ::= basetype(t) init_declarators(ds) SEMICOLON.",
        |cx, [t, ds, _]| ok(mk_decls(cx.snc, cx.ast, ds.node(), t.ty())?)
    ),
    rule!("init_declarators(r) ::= init_declarator(x).", |_, [x]| Ok(
        x
    )),
    rule!(
        "init_declarators(r) ::= init_declarators(xs) COMMA init_declarator(x).",
        |cx, [xs, _, x]| ok(cx.link(xs.node(), x.node()))
    ),
    rule!("init_declarator(r) ::= declarator(x).", |_, [x]| Ok(x)),
    rule!(
        "init_declarator(r) ::= declarator(x) EQUAL(t) init_expr(i).",
        |cx, [x, t, i]| ok(cx.node(EBinop, t.tok(), &[x.node(), i.node()]))
    ),
    rule!("declarator(r) ::= variable(n).", |cx, [n]| ok(cx.node(
        EVar,
        n.tok(),
        &[]
    ))),
    rule!(
        "declarator(r) ::= declarator(x) subscript(s). [POST]",
        |cx, [x, s]| {
            let n1 = cx.node(EConst, s.tok(), &[]);
            ok(cx.node(ESubscr, s.tok(), &[x.node(), n1]))
        }
    ),
    rule!(
        "declarator(r) ::= declarator(x) LPAREN(t) param_decls(ps) RPAREN. [POST]",
        |cx, [x, t, ps, _]| ok(cx.node(EFunc, t.tok(), &[x.node(), ps.node()]))
    ),
    rule!(
        "declarator(r) ::= LPAREN declarator(x) RPAREN. [PRE]",
        |_, [_, x, _]| Ok(x)
    ),
    rule!(
        "declarator(r) ::= ASTERISK(t) declarator(x). [PRE]",
        |cx, [t, x]| ok(cx.node(EPre, t.tok(), &[x.node()]))
    ),
    rule!("declarator(r) ::= CONST(t) declarator(x). [PRE]", |cx,
                                                              [
        t,
        x,
    ]| ok(
        cx.node(EPre, t.tok(), &[x.node()])
    )),
    rule!("param_decls(r) ::= .", |_, []| nil()),
    rule!("param_decls(r) ::= param_decl(x).", |_, [x]| Ok(x)),
    rule!(
        "param_decls(r) ::= param_decls(xs) COMMA param_decl(x).",
        |cx, [xs, _, x]| ok(cx.link(xs.node(), x.node()))
    ),
    rule!(
        "param_decl(r) ::= basetype(t) declarator(d).",
        |cx, [t, d]| ok(Some(mk_decl(cx.snc, cx.ast, d.node(), t.ty())?))
    ),
    rule!("param_decl(r) ::= type_expr(x).", |_, [x]| Ok(x)),
    rule!(
        "declaration(r) ::= FOREIGN variables(ds) SEMICOLON.",
        |cx, [_, ds, _]| {
            let t = cx.ast.mk_no_type();
            ok(mk_decls(cx.snc, cx.ast, ds.node(), t)?)
        }
    ),
    rule!("variables(r) ::= variable(x).", |cx, [x]| ok(cx.node(
        EVar,
        x.tok(),
        &[]
    ))),
    rule!("variables(r) ::= variables(xs) COMMA variable(x).", |cx,
                                                                [
        xs,
        _,
        x,
    ]| {
        let n1 = cx.node(EVar, x.tok(), &[]);
        ok(cx.link(xs.node(), n1))
    }),
    rule!(
        "init_expr(r) ::= LPAREN(tc) type_expr(c) RPAREN LBRACE(tx) init_exprs(x) RBRACE.",
        |cx, [tc, c, _, tx, x, _]| {
            let n1 = cx.node(EInit, tx.tok(), &[x.node()]);
            ok(cx.node(ECast, tc.tok(), &[c.node(), n1]))
        }
    ),
    rule!("init_expr(r) ::= LBRACE(t) init_exprs(x) RBRACE.", |cx,
                                                               [
        t,
        x,
        _,
    ]| {
        ok(cx.node(EInit, t.tok(), &[x.node()]))
    }),
    rule!("init_expr(r) ::= expr(x).", |_, [x]| Ok(x)),
    rule!(
        "init_exprs(r) ::= init_exprs(xs) COMMA init_expr(x).",
        |cx, [xs, _, x]| ok(cx.link(xs.node(), x.node()))
    ),
    rule!("init_exprs(r) ::= init_expr(x).", |_, [x]| Ok(x)),
    rule!("init_exprs(r) ::= .", |_, []| nil()),
    rule!("prim_type(r) ::= CHAR.", |_, [_]| prim(PrimType::Char)),
    rule!("prim_type(r) ::= SHORT.", |_, [_]| prim(PrimType::Short)),
    rule!("prim_type(r) ::= INT.", |_, [_]| prim(PrimType::Int)),
    rule!("prim_type(r) ::= LONG.", |_, [_]| prim(PrimType::Long)),
    rule!("prim_type(r) ::= UNSIGNED CHAR.", |_, [_, _]| prim(
        PrimType::UChar
    )),
    rule!("prim_type(r) ::= UNSIGNED SHORT.", |_, [_, _]| prim(
        PrimType::UShort
    )),
    rule!("prim_type(r) ::= UNSIGNED INT.", |_, [_, _]| prim(
        PrimType::UInt
    )),
    rule!("prim_type(r) ::= UNSIGNED LONG.", |_, [_, _]| prim(
        PrimType::ULong
    )),
    rule!("prim_type(r) ::= INT8T.", |_, [_]| prim(PrimType::Int8T)),
    rule!("prim_type(r) ::= UINT8T.", |_, [_]| prim(PrimType::UInt8T)),
    rule!("prim_type(r) ::= INT16T.", |_, [_]| prim(PrimType::Int16T)),
    rule!("prim_type(r) ::= UINT16T.", |_, [_]| prim(
        PrimType::UInt16T
    )),
    rule!("prim_type(r) ::= INT32T.", |_, [_]| prim(PrimType::Int32T)),
    rule!("prim_type(r) ::= UINT32T.", |_, [_]| prim(
        PrimType::UInt32T
    )),
    rule!("prim_type(r) ::= FLOAT.", |_, [_]| prim(PrimType::Float)),
    rule!("prim_type(r) ::= DOUBLE.", |_, [_]| prim(PrimType::Double)),
    rule!("prim_type(r) ::= STRING.", |_, [_]| prim(PrimType::String)),
    rule!("basetype(r) ::= prim_type(x).", |cx, [x]| ty(cx
        .ast
        .mk_prim_type(x.prim()))),
    rule!("basetype(r) ::= EVFLAG.", |cx, [_]| ty(cx.ast.mk_ef_type())),
    rule!("basetype(r) ::= VOID.", |cx, [_]| ty(cx.ast.mk_void_type())),
    rule!("basetype(r) ::= ENUM NAME(x).", |cx, [_, x]| ty(cx
        .ast
        .mk_foreign_type(ForeignTag::Enum, x.name()))),
    rule!("basetype(r) ::= STRUCT NAME(x).", |cx, [_, x]| ty(cx
        .ast
        .mk_foreign_type(ForeignTag::Struct, x.name()))),
    rule!("basetype(r) ::= UNION NAME(x).", |cx, [_, x]| ty(cx
        .ast
        .mk_foreign_type(ForeignTag::Union, x.name()))),
    rule!("basetype(r) ::= TYPENAME NAME(x).", |cx, [_, x]| ty(cx
        .ast
        .mk_foreign_type(ForeignTag::Typename, x.name()))),
    rule!("type_expr(r) ::= basetype(t).", |cx, [t]| ok(Some(
        mk_decl(cx.snc, cx.ast, None, t.ty())?
    ))),
    rule!(
        "type_expr(r) ::= basetype(t) abs_decl(d).",
        |cx, [t, d]| ok(Some(mk_decl(cx.snc, cx.ast, d.node(), t.ty())?))
    ),
    rule!(
        "abs_decl(r) ::= LPAREN abs_decl(x) RPAREN.",
        |_, [_, x, _]| Ok(x)
    ),
    rule!("abs_decl(r) ::= ASTERISK(t). [PRE]", |cx, [t]| ok(cx.node(
        EPre,
        t.tok(),
        &[None]
    ))),
    rule!("abs_decl(r) ::= ASTERISK(t) abs_decl(d). [PRE]", |cx,
                                                             [
        t,
        d,
    ]| ok(
        cx.node(EPre, t.tok(), &[d.node()])
    )),
    rule!("abs_decl(r) ::= CONST(t). [PRE]", |cx, [t]| ok(cx.node(
        EPre,
        t.tok(),
        &[None]
    ))),
    rule!(
        "abs_decl(r) ::= CONST(t) abs_decl(d). [PRE]",
        |cx, [t, d]| ok(cx.node(EPre, t.tok(), &[d.node()]))
    ),
    rule!("abs_decl(r) ::= subscript(s). [POST]", |cx, [s]| {
        let n1 = cx.node(EConst, s.tok(), &[]);
        ok(cx.node(ESubscr, s.tok(), &[None, n1]))
    }),
    rule!("abs_decl(r) ::= abs_decl(d) subscript(s). [POST]", |cx,
                                                               [
        d,
        s,
    ]| {
        let n1 = cx.node(EConst, s.tok(), &[]);
        ok(cx.node(ESubscr, s.tok(), &[d.node(), n1]))
    }),
    rule!(
        "abs_decl(r) ::= LPAREN(t) param_decls(ps) RPAREN. [POST]",
        |cx, [t, ps, _]| ok(cx.node(EFunc, t.tok(), &[None, ps.node()]))
    ),
    rule!(
        "abs_decl(r) ::= abs_decl(d) LPAREN(t) param_decls(ps) RPAREN. [POST]",
        |cx, [d, t, ps, _]| ok(cx.node(EFunc, t.tok(), &[d.node(), ps.node()]))
    ),
    rule!(
        "option(r) ::= OPTION option_value(v) NAME(n) SEMICOLON.",
        |cx, [_, v, n, _]| ok(Some(cx.ast.opt_defn(n.tok(), &v.tok())))
    ),
    rule!("option_value(r) ::= ADD(t).", |_, [t]| Ok(t)),
    rule!("option_value(r) ::= SUB(t).", |_, [t]| Ok(t)),
    rule!("state_sets(r) ::= state_sets(xs) state_set(x).", |cx,
                                                             [
        xs,
        x,
    ]| ok(
        cx.link(xs.node(), x.node())
    )),
    rule!("state_sets(r) ::= state_set(x).", |_, [x]| Ok(x)),
    rule!(
        "state_set(r) ::= SS NAME(n) LBRACE ss_defns(ds) states(xs) RBRACE.",
        |cx, [_, n, _, ds, xs, _]| ok(cx.node(DSs, n.tok(), &[ds.node(), xs.node()]))
    ),
    rule!(
        "ss_defns(r) ::= ss_defns(xs) ss_defn(x).",
        |cx, [xs, x]| ok(cx.link(xs.node(), x.node()))
    ),
    rule!("ss_defns(r) ::= .", |_, []| nil()),
    rule!("ss_defn(r) ::= assign(x).", |_, [x]| Ok(x)),
    rule!("ss_defn(r) ::= monitor(x).", |_, [x]| Ok(x)),
    rule!("ss_defn(r) ::= sync(x).", |_, [x]| Ok(x)),
    rule!("ss_defn(r) ::= syncq(x).", |_, [x]| Ok(x)),
    rule!("ss_defn(r) ::= declaration(x).", |_, [x]| Ok(x)),
    rule!("states(r) ::= states(xs) state(x).", |cx, [xs, x]| ok(
        cx.link(xs.node(), x.node())
    )),
    rule!("states(r) ::= state(x).", |_, [x]| Ok(x)),
    rule!(
        "state(r) ::= STATE NAME(n) LBRACE state_defns(ds) entry(en) transitions(ws) exit(ex) RBRACE.",
        |cx, [_, n, _, ds, en, ws, ex, _]| ok(cx.node(
            DState,
            n.tok(),
            &[ds.node(), en.node(), ws.node(), ex.node()]
        ))
    ),
    rule!("state_defns(r) ::= state_defns(xs) state_defn(x).", |cx,
                                                                [
        xs,
        x,
    ]| {
        ok(cx.link(xs.node(), x.node()))
    }),
    rule!("state_defns(r) ::= .", |_, []| nil()),
    rule!("state_defn(r) ::= assign(x).", |_, [x]| Ok(x)),
    rule!("state_defn(r) ::= monitor(x).", |_, [x]| Ok(x)),
    rule!("state_defn(r) ::= sync(x).", |_, [x]| Ok(x)),
    rule!("state_defn(r) ::= syncq(x).", |_, [x]| Ok(x)),
    rule!("state_defn(r) ::= declaration(x).", |_, [x]| Ok(x)),
    rule!("state_defn(r) ::= option(x).", |_, [x]| Ok(x)),
    rule!("entry(r) ::= ENTRY(t) block(b).", |cx, [t, b]| ok(cx.node(
        DEntex,
        t.tok(),
        &[b.node()]
    ))),
    rule!("entry(r) ::= .", |_, []| nil()),
    rule!("exit(r) ::= EXIT(t) block(b).", |cx, [t, b]| ok(cx.node(
        DEntex,
        t.tok(),
        &[b.node()]
    ))),
    rule!("exit(r) ::= .", |_, []| nil()),
    rule!("transitions(r) ::= transitions(xs) transition(x).", |cx,
                                                                [
        xs,
        x,
    ]| {
        ok(cx.link(xs.node(), x.node()))
    }),
    rule!("transitions(r) ::= transition(x).", |_, [x]| Ok(x)),
    rule!(
        "transition(r) ::= WHEN(t) LPAREN condition(c) RPAREN block(b) STATE NAME(n).",
        |cx, [t, _, c, _, b, _, n]| {
            let mut t = t.tok();
            t.str = n.tok().str;
            ok(cx.node(DWhen, t, &[c.node(), b.node()]))
        }
    ),
    rule!(
        "transition(r) ::= WHEN(t) LPAREN condition(c) RPAREN block(b) EXIT.",
        |cx, [t, _, c, _, b, _]| {
            let mut t = t.tok();
            t.str = None;
            ok(cx.node(DWhen, t, &[c.node(), b.node()]))
        }
    ),
    rule!(
        "transition(r) ::= WHEN(t) LPAREN condition(c) RPAREN block(b) error.",
        |cx, [t, _, c, _, b, _]| {
            let mut t = t.tok();
            t.str = None;
            let r = cx.node(DWhen, t, &[c.node(), b.node()]);
            cx.snc.report(format_args!("expected 'state' or 'exit'\n"));
            ok(r)
        }
    ),
    rule!("condition(r) ::= opt_expr(x).", |_, [x]| Ok(x)),
    rule!(
        "block(r) ::= LBRACE(t) block_defns(ds) statements(xs) RBRACE.",
        |cx, [t, ds, xs, _]| ok(cx.node(SCmpnd, t.tok(), &[ds.node(), xs.node()]))
    ),
    rule!("block_defns(r) ::= block_defns(ds) block_defn(d).", |cx,
                                                                [
        ds,
        d,
    ]| {
        ok(cx.link(ds.node(), d.node()))
    }),
    rule!("block_defns(r) ::= .", |_, []| nil()),
    rule!("block_defn(r) ::= declaration(x).", |_, [x]| Ok(x)),
    rule!("block_defn(r) ::= c_code(x).", |_, [x]| Ok(x)),
    rule!("statements(r) ::= statements(xs) statement(x).", |cx,
                                                             [
        xs,
        x,
    ]| ok(
        cx.link(xs.node(), x.node())
    )),
    rule!("statements(r) ::= .", |_, []| nil()),
    rule!("statement(r) ::= BREAK(t) SEMICOLON.", |cx, [t, _]| ok(
        cx.node(SJump, t.tok(), &[])
    )),
    rule!("statement(r) ::= CONTINUE(t) SEMICOLON.", |cx, [t, _]| ok(
        cx.node(SJump, t.tok(), &[])
    )),
    rule!("statement(r) ::= RETURN(t) opt_expr(x) SEMICOLON.", |cx,
                                                                [
        t,
        x,
        _,
    ]| {
        ok(cx.node(SReturn, t.tok(), &[x.node()]))
    }),
    rule!(
        "statement(r) ::= STATE NAME(t) SEMICOLON.",
        |cx, [_, t, _]| ok(cx.node(SChange, t.tok(), &[]))
    ),
    rule!("statement(r) ::= c_code(x).", |_, [x]| Ok(x)),
    rule!("statement(r) ::= block(x).", |_, [x]| Ok(x)),
    rule!(
        "statement(r) ::= IF(t) LPAREN comma_expr(c) RPAREN statement(th).",
        |cx, [t, _, c, _, th]| ok(cx.node(SIf, t.tok(), &[c.node(), th.node(), None]))
    ),
    rule!(
        "statement(r) ::= IF(t) LPAREN comma_expr(c) RPAREN statement(th) ELSE statement(el).",
        |cx, [t, _, c, _, th, _, el]| ok(cx.node(SIf, t.tok(), &[c.node(), th.node(), el.node()]))
    ),
    rule!(
        "statement(r) ::= WHILE(t) LPAREN comma_expr(c) RPAREN statement(x).",
        |cx, [t, _, c, _, x]| ok(cx.node(SWhile, t.tok(), &[c.node(), x.node()]))
    ),
    rule!("statement(r) ::= for_statement(x).", |_, [x]| Ok(x)),
    rule!(
        "statement(r) ::= opt_expr(x) SEMICOLON(t).",
        |cx, [x, t]| ok(cx.node(SStmt, t.tok(), &[x.node()]))
    ),
    rule!(
        "for_statement(r) ::= FOR(for) LPAREN opt_expr(init) SEMICOLON opt_expr(cond) SEMICOLON opt_expr(iter) RPAREN statement(st).",
        |cx, [for_, _, init, _, cond, _, iter, _, st]| ok(cx.node(
            SFor,
            for_.tok(),
            &[init.node(), cond.node(), iter.node(), st.node()]
        ))
    ),
    rule!("expr(r) ::= INTCON(x).", |cx, [x]| ok(cx.node(
        EConst,
        x.tok(),
        &[]
    ))),
    rule!("expr(r) ::= FPCON(x).", |cx, [x]| ok(cx.node(
        EConst,
        x.tok(),
        &[]
    ))),
    rule!("expr(r) ::= string(x).", |_, [x]| Ok(x)),
    rule!("expr(r) ::= variable(v).", |cx, [v]| ok(cx.node(
        EVar,
        v.tok(),
        &[]
    ))),
    rule!("expr(r) ::= LPAREN(t) comma_expr(x) RPAREN.", |cx,
                                                          [
        t,
        x,
        _,
    ]| ok(
        cx.node(EParen, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= expr(x) LPAREN(t) args(y) RPAREN.", |cx,
                                                            [
        x,
        t,
        y,
        _,
    ]| ok(
        cx.node(EFunc, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= EXIT(n) LPAREN(t) args(y) RPAREN.", |cx,
                                                            [
        n,
        t,
        y,
        _,
    ]| {
        let n1 = cx.node(EVar, n.tok(), &[]);
        ok(cx.node(EFunc, t.tok(), &[n1, y.node()]))
    }),
    rule!(
        "expr(r) ::= SIZEOF(n) LPAREN(t) type_expr(y) RPAREN.",
        |cx, [n, t, y, _]| {
            let n1 = cx.node(EVar, n.tok(), &[]);
            ok(cx.node(EFunc, t.tok(), &[n1, y.node()]))
        }
    ),
    rule!("expr(r) ::= expr(x) LBRACKET(t) expr(y) RBRACKET.", |cx,
                                                                [
        x,
        t,
        y,
        _,
    ]| {
        ok(cx.node(ESubscr, t.tok(), &[x.node(), y.node()]))
    }),
    rule!(
        "expr(r) ::= expr(x) PERIOD(t) member(y).",
        |cx, [x, t, y]| ok(cx.node(ESelect, t.tok(), &[x.node(), y.node()]))
    ),
    rule!(
        "expr(r) ::= expr(x) POINTER(t) member(y).",
        |cx, [x, t, y]| ok(cx.node(ESelect, t.tok(), &[x.node(), y.node()]))
    ),
    rule!("expr(r) ::= expr(x) INCR(t). [POST]", |cx, [x, t]| ok(
        cx.node(EPost, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= expr(x) DECR(t). [POST]", |cx, [x, t]| ok(
        cx.node(EPost, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= ADD(t) expr(x). [PRE]", |cx, [t, x]| ok(
        cx.node(EPre, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= SUB(t) expr(x). [PRE]", |cx, [t, x]| ok(
        cx.node(EPre, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= ASTERISK(t) expr(x). [PRE]", |cx, [t, x]| ok(
        cx.node(EPre, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= AMPERSAND(t)expr(x). [PRE]", |cx, [t, x]| ok(
        cx.node(EPre, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= NOT(t) expr(x). [PRE]", |cx, [t, x]| ok(
        cx.node(EPre, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= TILDE(t) expr(x). [PRE]", |cx, [t, x]| ok(
        cx.node(EPre, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= INCR(t) expr(x). [PRE]", |cx, [t, x]| ok(
        cx.node(EPre, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= DECR(t) expr(x). [PRE]", |cx, [t, x]| ok(
        cx.node(EPre, t.tok(), &[x.node()])
    )),
    rule!("expr(r) ::= SIZEOF(t) expr(x). [PRE]", |cx, [t, x]| {
        let n1 = cx.node(EVar, t.tok(), &[]);
        ok(cx.node(EFunc, t.tok(), &[n1, x.node()]))
    }),
    rule!(
        "expr(r) ::= LPAREN(t) type_expr(c) RPAREN expr(x). [PRE]",
        |cx, [t, c, _, x]| ok(cx.node(ECast, t.tok(), &[c.node(), x.node()]))
    ),
    rule!("expr(r) ::= expr(x) SUB(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) ADD(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!(
        "expr(r) ::= expr(x) ASTERISK(t) expr(y).",
        |cx, [x, t, y]| ok(cx.node(EBinop, t.tok(), &[x.node(), y.node()]))
    ),
    rule!("expr(r) ::= expr(x) SLASH(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) GT(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) GE(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) EQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) NE(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) LE(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) LT(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) OROR(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!(
        "expr(r) ::= expr(x) ANDAND(t) expr(y).",
        |cx, [x, t, y]| ok(cx.node(EBinop, t.tok(), &[x.node(), y.node()]))
    ),
    rule!(
        "expr(r) ::= expr(x) LSHIFT(t) expr(y).",
        |cx, [x, t, y]| ok(cx.node(EBinop, t.tok(), &[x.node(), y.node()]))
    ),
    rule!(
        "expr(r) ::= expr(x) RSHIFT(t) expr(y).",
        |cx, [x, t, y]| ok(cx.node(EBinop, t.tok(), &[x.node(), y.node()]))
    ),
    rule!("expr(r) ::= expr(x) VBAR(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) CARET(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!(
        "expr(r) ::= expr(x) AMPERSAND(t) expr(y).",
        |cx, [x, t, y]| ok(cx.node(EBinop, t.tok(), &[x.node(), y.node()]))
    ),
    rule!("expr(r) ::= expr(x) MOD(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!(
        "expr(r) ::= expr(c) QUESTION(t) expr(th) COLON expr(el).",
        |cx, [c, t, th, _, el]| ok(cx.node(ETernop, t.tok(), &[c.node(), th.node(), el.node()]))
    ),
    rule!("expr(r) ::= expr(x) EQUAL(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) ADDEQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) SUBEQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) ANDEQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) OREQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) DIVEQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) MULEQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) MODEQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) LSHEQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) RSHEQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!("expr(r) ::= expr(x) XOREQ(t) expr(y).", |cx, [x, t, y]| ok(
        cx.node(EBinop, t.tok(), &[x.node(), y.node()])
    )),
    rule!(
        "comma_expr(r) ::= comma_expr(xs) COMMA(t) expr(x).",
        |cx, [xs, t, x]| ok(cx.node(EBinop, t.tok(), &[xs.node(), x.node()]))
    ),
    rule!("comma_expr(r) ::= expr(x).", |_, [x]| Ok(x)),
    rule!("opt_expr(r) ::= comma_expr(x).", |_, [x]| Ok(x)),
    rule!("opt_expr(r) ::= .", |_, []| nil()),
    rule!("args(r) ::= args(xs) COMMA expr(x).", |cx, [xs, _, x]| ok(
        cx.link(xs.node(), x.node())
    )),
    rule!("args(r) ::= expr(x).", |_, [x]| Ok(x)),
    rule!("args(r) ::= .", |_, []| nil()),
    rule!("string(r) ::= STRCON(t).", |cx, [t]| ok(cx.node(
        EString,
        t.tok(),
        &[]
    ))),
    rule!("member(r) ::= NAME(t).", |cx, [t]| ok(cx.node(
        EMember,
        t.tok(),
        &[]
    ))),
    rule!(
        "funcdef(r) ::= basetype(t) declarator(d) block(b).",
        |cx, [t, d, b]| {
            let d = d.node();
            let decl = mk_decl(cx.snc, cx.ast, d, t.ty())?;
            let k = cx.ast[d.expect("funcdef without declarator")].token.clone();
            ok(cx.node(DFuncdef, k, &[Some(decl), b.node()]))
        }
    ),
    rule!(
        "structdef(r) ::= STRUCT NAME(t) members(ds) SEMICOLON.",
        |cx, [_, t, ds, _]| ok(cx.node(DStructdef, t.tok(), &[ds.node()]))
    ),
    rule!("members(r) ::= LBRACE member_decls(xs) RBRACE.", |_,
                                                             [
        _,
        xs,
        _,
    ]| Ok(
        xs
    )),
    rule!("member_decls(r) ::= member_decl(x).", |_, [x]| Ok(x)),
    rule!(
        "member_decls(r) ::= member_decls(xs) member_decl(x).",
        |cx, [xs, x]| ok(cx.link(xs.node(), x.node()))
    ),
    rule!(
        "member_decl(r) ::= basetype(t) declarator(d) SEMICOLON.",
        |cx, [t, d, _]| ok(Some(mk_decl(cx.snc, cx.ast, d.node(), t.ty())?))
    ),
    rule!("member_decl(r) ::= c_code(x).", |_, [x]| Ok(x)),
    rule!("c_code(r) ::= CCODE(t).", |cx, [t]| ok(cx.node(
        TText,
        t.tok(),
        &[]
    ))),
];

/// Run lemon on the grammar.
pub(crate) fn lalr() -> Lalr {
    let specs: Vec<RuleSpec> = RULES
        .iter()
        .map(|r| RuleSpec {
            text: r.text,
            code: r.action.is_some(),
        })
        .collect();
    Lalr::new(&PRECEDENCE, &specs)
}

/// The parser tables, built on first use.
pub(crate) fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| lalr().tables())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snc::types::Tok;

    #[test]
    fn token_codes_are_lemon_terminal_numbers() {
        let g = lalr();
        assert_eq!(g.nterminal, Tok::ALL.len());
        for &t in Tok::ALL {
            assert_eq!(g.syms[t as usize].name, t.grammar_name());
        }
    }

    /// The sizes the C build's snl.c defines.
    #[test]
    fn tables_match_c_sizes() {
        let g = lalr();
        // snl.out flags the same two: CCODE after block_defns, and ELSE
        assert_eq!(g.nconflict, 2);
        let t = g.tables();
        assert_eq!(t.nsymbol + 1, 160, "YYNOCODE");
        assert_eq!(t.errsym, 95, "YYERRORSYMBOL");
        assert_eq!(t.nxstate, 254, "YYNSTATE");
        assert_eq!(t.nrule, 224, "YYNRULE");
        assert_eq!(t.yy_max_shift(), 253);
        assert_eq!(t.yy_min_shiftreduce(), 402);
        assert_eq!(t.yy_max_shiftreduce(), 625);
        assert_eq!(t.yy_min_reduce(), 626);
        assert_eq!(t.yy_max_reduce(), 849);
        assert_eq!(t.yy_error_action(), 850);
        assert_eq!(t.yy_accept_action(), 851);
    }
}
