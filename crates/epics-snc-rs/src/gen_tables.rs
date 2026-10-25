//! The tables the run-time sequencer reads (C `gen_tables.c`): channels,
//! event masks, states, state sets and the program, as the `static`s of
//! epics-seq-rs's `seq_snc.h` types.

use crate::analysis::{AstCx, traverse_syntax_tree};
use crate::driver::Snc;
use crate::node::strtoui;
use crate::rust_out::{Arg, Const, Expr, Item, c_unescape};
use crate::seq_prim_types::PrimType;
use crate::types::{
    Ast, ChanId, Extra, Multiplicity, NodeId, NodeTag, Options, Program, StateOptions, VarChan,
    VarId, child,
};
use crate::var_types::TypeKind;

const NM_VARS: &str = "seqg_vars";
const NM_CHANS: &str = "seqg_chans";
const NM_STATES: &str = "seqg_states";
const NM_STATESETS: &str = "seqg_statesets";
const NM_ENTRY: &str = "seqg_entry";
const NM_EXIT: &str = "seqg_exit";
const NM_INIT: &str = "seqg_init";
const NM_ACTION: &str = "seqg_action";
const NM_EVENT: &str = "seqg_event";
const NM_MASK: &str = "seqg_mask";
/// gen_ss_code's module of a state set's state functions.
const NM_SS: &str = "seqg_ss";
/// The function returning a `-r` program's one variable block.
const NM_STATIC_VARS: &str = "seqg_static_vars";

/// C `MAGIC` (`seq_release.h`, which build.rs writes): the table layout,
/// which epics-seq-rs checks against its `SEQ_MAGIC`.
const MAGIC: &str = env!("EPICS_SNC_MAGIC");

/// C `gen_tables`: all kinds of tables for the program `p`.
pub fn gen_tables(snc: &mut Snc, ast: &Ast, p: &Program) -> Vec<Item> {
    // the blank line before the tables is the caller's
    let mut out = vec![Item::Comment(
        "************************ Tables ************************".into(),
    )];
    gen_channel_table(&mut out, ast, p);
    gen_state_table(&mut out, snc, ast, p);
    gen_ss_table(&mut out, ast, p);
    gen_prog_table(&mut out, ast, p);
    out
}

/// C's `"\n/* text */\n"` before a table.
fn comment(out: &mut Vec<Item>, text: String) {
    out.push(Item::Blank);
    out.push(Item::Comment(text));
}

fn table(name: String, elem: &str, elems: Vec<Arg>) -> Const {
    Const {
        public: false,
        is_static: true,
        name,
        ty: format!("[{elem}; {}]", elems.len()),
        expr: Expr::Array(elems),
    }
}

/// `Some(name)` when the function exists, else `None`.
fn func(exists: bool, name: String) -> Expr {
    if exists {
        Expr::call("Some", vec![Expr::Atom(name)])
    } else {
        Expr::atom("None")
    }
}

/// `ty::A | ty::B`, or `0` without bits.
fn bits(ty: &str, names: &[&str]) -> Expr {
    names
        .iter()
        .map(|name| Expr::Atom(format!("{ty}::{name}")))
        .reduce(|lhs, rhs| Expr::binary(lhs, "|", rhs))
        .unwrap_or_else(|| Expr::atom("0"))
}

/// The Rust literal of a C string token: its bytes up to the first NUL,
/// where the run-time's C string ended. A table field is a `&str`, so
/// bytes that are not UTF-8 go in lossily.
fn c_string(text: &str) -> Expr {
    let mut bytes = c_unescape(text.as_bytes());
    if let Some(nul) = bytes.iter().position(|&c| c == 0) {
        bytes.truncate(nul);
    }
    Expr::str_lit(&String::from_utf8_lossy(&bytes))
}

/// C `gen_channel_table`: one entry for each channel.
fn gen_channel_table(out: &mut Vec<Item>, ast: &Ast, p: &Program) {
    let chans: Vec<Arg> = std::iter::successors(p.chan_list.first, |&cp| ast[cp].next)
        .map(|cp| gen_channel(ast, cp, p.num_event_flags).into())
        .collect();
    let what = if chans.is_empty() {
        "No channel definitions"
    } else {
        "Channel table"
    };
    comment(out, what.into());
    out.push(Item::Const(table(
        NM_CHANS.into(),
        &format!("seqg_rt::SeqChan<{NM_VARS}>"),
        chans,
    )));
}

/// C `gen_var_name`: the place of the variable `vp` of global lifetime in
/// the `seqg_vars` that `base` is.
pub(crate) fn var_place(ast: &Ast, vp: VarId, base: Expr) -> Expr {
    let var = &ast[vp];
    let name = var.name.as_deref().unwrap_or_default();
    let scope = &ast[var.scope.expect("variable has a scope")];
    match (&scope.tag, &scope.extra) {
        (NodeTag::DProg, _) => base.field(name),
        (NodeTag::DSs, _) => base
            .field(format!("{NM_VARS}_{}", scope.token.text()))
            .field(name),
        (NodeTag::DState, Extra::State(state)) => {
            let ss = &ast[state.var_list.expect("state variable list")];
            let ss = &ast[ss.parent_scope.expect("state inside a state set")];
            base.field(format!("{NM_VARS}_{}", ss.token.text()))
                .field(format!("{NM_VARS}_{}", scope.token.text()))
                .field(name)
        }
        (tag, _) => unreachable!("global variable in {}", tag.name()),
    }
}

/// The run-time's `PrimType` variant for a C `prim_type_tag`.
fn prim_type(t: PrimType) -> &'static str {
    match t {
        PrimType::Char => "Char",
        PrimType::UChar => "UChar",
        PrimType::Short => "Short",
        PrimType::UShort => "UShort",
        PrimType::Int => "Int",
        PrimType::UInt => "UInt",
        PrimType::Long => "Long",
        PrimType::ULong => "ULong",
        PrimType::Int8T => "Int8T",
        PrimType::UInt8T => "UInt8T",
        PrimType::Int16T => "Int16T",
        PrimType::UInt16T => "UInt16T",
        PrimType::Int32T => "Int32T",
        PrimType::UInt32T => "UInt32T",
        PrimType::Float => "Float",
        PrimType::Double => "Double",
        PrimType::String => "String",
    }
}

/// C `gen_channel`: a `SeqChan`. C's `#error` for a `long` channel is the
/// run-time's `SeqProgram::validate` refusal here.
fn gen_channel(ast: &Ast, cp: ChanId, num_event_flags: u32) -> Expr {
    let chan = &ast[cp];
    let var = &ast[chan.var];
    let name = var.name.as_deref().unwrap_or_default();
    let (place, elem) = match var.assign {
        Multiplicity::Multi => (
            var_place(ast, chan.var, Expr::atom("v")).index(Expr::Atom(chan.index.to_string())),
            format!("[{}]", chan.index),
        ),
        _ => (var_place(ast, chan.var, Expr::atom("v")), String::new()),
    };
    // the flag's constant, which a flag only sync'd uses nowhere else
    let ef_id = match chan.sync {
        Some(ef) => match &ast[ef].chan {
            VarChan::EvFlag(_) => Expr::atom(ast[ef].name.as_deref().unwrap_or_default()),
            other => unreachable!("sync'd to {other:?}"),
        },
        None => Expr::call("seqg_rt::EfId", vec![Expr::atom("0")]),
    };
    let TypeKind::Prim(prim) = ast[ast.base_type(var.type_)].kind else {
        unreachable!("channel of a non-primitive variable");
    };
    let (queue_size, queue_index) = match chan.syncq {
        None => (Expr::atom("0"), 0),
        Some(q) if ast[q].size == 0 => (Expr::atom("seqg_rt::DEFAULT_QUEUE_SIZE"), ast[q].index),
        Some(q) => (Expr::Atom(ast[q].size.to_string()), ast[q].index),
    };
    let fields = vec![
        (
            "ch_name",
            match &chan.name {
                Some(pv) => Expr::call("Some", vec![c_string(pv)]),
                None => Expr::atom("None"),
            },
        ),
        (
            "var",
            Expr::Closure("v".into(), Box::new(Expr::unary("&", place))),
        ),
        ("var_name", Expr::str_lit(&format!("{name}{elem}"))),
        (
            "var_type",
            Expr::Atom(format!("seqg_rt::PrimType::{}", prim_type(prim))),
        ),
        ("count", Expr::Atom(chan.count.to_string())),
        (
            "event_num",
            Expr::Atom((num_event_flags + var.index + chan.index + 1).to_string()),
        ),
        ("ef_id", ef_id),
        ("monitored", Expr::Atom(chan.monitor.to_string())),
        ("queue_size", queue_size),
        ("queue_index", Expr::Atom(queue_index.to_string())),
    ];
    Expr::Struct(
        "seqg_rt::SeqChan".into(),
        fields.into_iter().map(|(f, e)| (f.into(), e)).collect(),
    )
}

/// C `gen_state_table`: the event masks and the state table of each state
/// set.
fn gen_state_table(out: &mut Vec<Item>, snc: &mut Snc, ast: &Ast, p: &Program) {
    let num_event_words = nwords(p.num_event_flags + p.chan_list.num_elems);
    let statesets = ast.list(ast[p.prog].child(child::PROG_STATESETS));
    for ssp in statesets {
        let ss_name = ast[ssp].token.text();
        let states = || ast.list(ast[ssp].child(child::SS_STATES));

        comment(out, format!("Event masks for state set \"{ss_name}\""));
        let mut masks = vec![
            Item::Use {
                public: false,
                path: "super::*".into(),
            },
            Item::Blank,
        ];
        for sp in states() {
            let words = state_event_mask(snc, ast, sp, p.num_event_flags, num_event_words);
            let mask = table(
                format!("{NM_MASK}_{}", ast[sp].token.text()),
                "seqg_rt::SeqMask",
                words
                    .iter()
                    .map(|w| Expr::Atom(format!("{w:#010x}")).into())
                    .collect(),
            );
            // public for the state table, which is outside the module
            masks.push(Item::Const(Const {
                public: true,
                ..mask
            }));
        }
        out.push(Item::Mod(format!("{NM_MASK}_{ss_name}"), masks));

        comment(out, format!("State table for state set \"{ss_name}\""));
        out.push(Item::Const(table(
            format!("{NM_STATES}_{ss_name}"),
            &format!("seqg_rt::SeqState<{NM_VARS}>"),
            states()
                .map(|sp| fill_state_struct(ast, sp, ss_name).into())
                .collect(),
        )));
    }
}

/// C `fill_state_struct`: a `SeqState`.
fn fill_state_struct(ast: &Ast, sp: NodeId, ss_name: &str) -> Expr {
    let state = &ast[sp];
    let name = state.token.text();
    let func_name = |nm: &str| format!("{NM_SS}_{ss_name}::{nm}_{name}");
    let Extra::State(extra) = &state.extra else {
        unreachable!("state without state data");
    };
    let fields = vec![
        ("state_name", Expr::str_lit(name)),
        ("action_func", Expr::Atom(func_name(NM_ACTION))),
        ("event_func", Expr::Atom(func_name(NM_EVENT))),
        (
            "entry_func",
            func(
                state.child(child::STATE_ENTRY).is_some(),
                func_name(NM_ENTRY),
            ),
        ),
        (
            "exit_func",
            func(state.child(child::STATE_EXIT).is_some(), func_name(NM_EXIT)),
        ),
        (
            "event_mask",
            Expr::unary(
                "&",
                Expr::Atom(format!("{NM_MASK}_{ss_name}::{NM_MASK}_{name}")),
            ),
        ),
        ("options", encode_state_options(extra.options)),
    ];
    Expr::Struct(
        "seqg_rt::SeqState".into(),
        fields.into_iter().map(|(f, e)| (f.into(), e)).collect(),
    )
}

/// C `encode_state_options`: the state option bits.
fn encode_state_options(options: StateOptions) -> Expr {
    let mut set = Vec::new();
    if !options.do_reset_timers {
        set.push("NORESETTIMERS");
    }
    if !options.no_entry_from_self {
        set.push("DOENTRYFROMSELF");
    }
    if !options.no_exit_to_self {
        set.push("DOEXITTOSELF");
    }
    Expr::call(
        "seqg_rt::StateOptions::new",
        vec![bits("seqg_rt::StateOptions", &set)],
    )
}

/// C `gen_ss_table`: one entry for each state set.
fn gen_ss_table(out: &mut Vec<Item>, ast: &Ast, p: &Program) {
    comment(out, "State set table".into());
    let statesets = ast.list(ast[p.prog].child(child::PROG_STATESETS));
    out.push(Item::Const(table(
        NM_STATESETS.into(),
        &format!("seqg_rt::SeqSs<{NM_VARS}>"),
        statesets
            .map(|ssp| {
                let ss_name = ast[ssp].token.text();
                Expr::Struct(
                    "seqg_rt::SeqSs".into(),
                    vec![
                        ("ss_name".into(), Expr::str_lit(ss_name)),
                        (
                            "states".into(),
                            Expr::unary("&", Expr::Atom(format!("{NM_STATES}_{ss_name}"))),
                        ),
                    ],
                )
                .into()
            })
            .collect(),
    )));
}

/// C `gen_prog_table`: the program's `SeqProgram`. C's `OPT_REENT` and
/// `OPT_SAFE` are its `VarModel`.
fn gen_prog_table(out: &mut Vec<Item>, ast: &Ast, p: &Program) {
    comment(out, "Program table (global)".into());
    let name = p.name.as_deref().unwrap_or_default();
    let prog = &ast[p.prog];
    let vars = if p.options.safe {
        Expr::atom("seqg_rt::VarModel::Safe")
    } else if p.options.reent {
        Expr::atom("seqg_rt::VarModel::Reentrant")
    } else {
        Expr::call(
            "seqg_rt::VarModel::Static",
            vec![Expr::atom(NM_STATIC_VARS)],
        )
    };
    let fields = vec![
        ("magic", Expr::atom(MAGIC)),
        ("prog_name", Expr::str_lit(name)),
        ("chan", Expr::unary("&", Expr::atom(NM_CHANS))),
        ("ss", Expr::unary("&", Expr::atom(NM_STATESETS))),
        ("vars", vars),
        ("params", c_string(p.param.as_deref().unwrap_or_default())),
        ("num_ev_flags", Expr::Atom(p.num_event_flags.to_string())),
        ("options", encode_options(p.options)),
        ("init_func", Expr::atom(NM_INIT)),
        (
            "entry_func",
            func(prog.child(child::PROG_ENTRY).is_some(), NM_ENTRY.into()),
        ),
        (
            "exit_func",
            func(prog.child(child::PROG_EXIT).is_some(), NM_EXIT.into()),
        ),
        ("num_queues", Expr::Atom(p.syncq_list.num_elems.to_string())),
    ];
    out.push(Item::Const(Const {
        public: true,
        is_static: true,
        name: name.into(),
        ty: format!("seqg_rt::SeqProgram<{NM_VARS}>"),
        expr: Expr::Struct(
            "seqg_rt::SeqProgram".into(),
            fields.into_iter().map(|(f, e)| (f.into(), e)).collect(),
        ),
    }));
}

/// C `encode_options`: the program option bits, less `+r` and `+s`.
fn encode_options(options: Options) -> Expr {
    let mut set = Vec::new();
    if options.async_ {
        set.push("ASYNC");
    }
    if options.conn {
        set.push("CONN");
    }
    if options.debug {
        set.push("DEBUG");
    }
    if options.newef {
        set.push("NEWEF");
    }
    Expr::call(
        "seqg_rt::ProgOptions::new",
        vec![bits("seqg_rt::ProgOptions", &set)],
    )
}

/// C `seqMask`.
type SeqMask = u32;

/// C `NBITS`.
const NBITS: u32 = SeqMask::BITS;

/// C `NWORDS(maxBitNum)`.
fn nwords(max_bit_num: u32) -> usize {
    (1 + max_bit_num / NBITS) as usize
}

/// C `bitnum`: the event bit of channel `ch_ix` of a variable whose
/// channels start at `var_ix`.
fn bitnum(var_ix: u32, ch_ix: u32, num_efs: u32) -> u32 {
    var_ix + ch_ix + num_efs + 1
}

/// C `event_mask_args`, with the tree and the reporter the iteratees need.
struct EventMask<'a> {
    ast: &'a Ast,
    snc: &'a mut Snc,
    words: Vec<SeqMask>,
    num_event_flags: u32,
}

impl AstCx for EventMask<'_> {
    fn ast(&self) -> &Ast {
        self.ast
    }
}

impl EventMask<'_> {
    /// C `bitSet`.
    fn bit_set(&mut self, bitnum: u32) {
        self.words[(bitnum / NBITS) as usize] |= 1 << (bitnum % NBITS);
    }

    fn var(&self, ep: NodeId) -> VarId {
        match self.ast[ep].extra {
            Extra::Var(vp) => vp,
            ref other => unreachable!("E_VAR with extra {other:?}"),
        }
    }

    /// C `iter_event_mask_scalar`: scalar variables and event flags.
    fn scalar(&mut self, ep: NodeId) -> bool {
        let ast = self.ast;
        assert_eq!(ast[ep].tag, NodeTag::EVar);
        let var = &ast[self.var(ep)];

        match (&ast[var.type_].kind, &var.chan) {
            (TypeKind::EvFlag, VarChan::EvFlag(ef)) => {
                self.bit_set(ef.index);
                return false; // no children anyway
            }
            (TypeKind::EvFlag, chan) => unreachable!("event flag with channel {chan:?}"),
            (TypeKind::Prim(_), _) => {}
            _ => return false, // no children anyway
        }

        // if not associated with channel, return
        if var.assign == Multiplicity::None {
            return false;
        }
        let VarChan::Single(cp) = var.chan else {
            unreachable!("scalar with assign {:?}", var.assign); // by L3
        };
        self.bit_set(bitnum(var.index, ast[cp].index, self.num_event_flags));
        false // no children anyway
    }

    /// C `iter_event_mask_array`: array variables, whole or subscripted.
    fn array(&mut self, ep: NodeId) -> bool {
        let ast = self.ast;
        let node = &ast[ep];
        let (e_var, e_ix) = match node.tag {
            NodeTag::ESubscr => {
                let e_var = node
                    .child(child::SUBSCR_OPERAND)
                    .expect("subscript operand");
                let e_ix = node.child(child::SUBSCR_INDEX).expect("subscript index");
                if ast[e_var].tag != NodeTag::EVar {
                    return true;
                }
                (e_var, Some(e_ix))
            }
            NodeTag::EVar => (ep, None),
            tag => unreachable!("{} is neither E_SUBSCR nor E_VAR", tag.name()),
        };
        let var = &ast[self.var(e_var)];

        // this subroutine handles only the array variables
        if !matches!(ast[var.type_].kind, TypeKind::Array { .. }) {
            return true;
        }

        match (var.assign, &var.chan) {
            (Multiplicity::None, _) => false,
            (Multiplicity::Single, &VarChan::Single(cp)) => {
                self.bit_set(bitnum(var.index, ast[cp].index, self.num_event_flags));
                true
            }
            (Multiplicity::Multi, VarChan::Multi(_)) => {
                let length1 = ast.type_array_length1(var.type_);
                match e_ix {
                    // an array variable subscripted with a constant
                    Some(e_ix) if ast[e_ix].tag == NodeTag::EConst => {
                        let subscr = ast[e_ix].token.text();
                        let Some(ix) = strtoui(subscr, length1) else {
                            let name = var.name.as_deref().unwrap_or_default();
                            self.snc.error_at_node(
                                &ast[e_ix],
                                format_args!("subscript in '{name}[{subscr}]' out of range\n"),
                            );
                            return false;
                        };
                        self.bit_set(bitnum(var.index, ix, self.num_event_flags));
                        // do NOT descend further, or the array variable
                        // below sets all the bits
                        false
                    }
                    // must descend for the array variable (see below) and
                    // possible array vars inside the subscript expression
                    Some(_) => true,
                    // no subscript: set all event bits for this variable
                    None => {
                        for ix in 0..length1 {
                            self.bit_set(bitnum(var.index, ix, self.num_event_flags));
                        }
                        false // no children anyway
                    }
                }
            }
            (assign, chan) => unreachable!("assign {assign:?} with channel {chan:?}"),
        }
    }
}

/// C `gen_state_event_mask`: a bit for each event flag and each process
/// variable (assigned var) used in one of the state's `when` conditions.
/// Bits 1 to `num_event_flags` are the event flags, the channels follow;
/// bit 0 is not used.
fn state_event_mask(
    snc: &mut Snc,
    ast: &Ast,
    sp: NodeId,
    num_event_flags: u32,
    num_event_words: usize,
) -> Vec<SeqMask> {
    let mut cx = EventMask {
        ast,
        snc,
        words: vec![0; num_event_words],
        num_event_flags,
    };
    // Database variables might have a subscript, which could be a constant
    // (set a single event bit) or an expression (set a group of bits for
    // the possible range of the evaluated expression).
    for tp in ast.list(ast[sp].child(child::STATE_WHENS)) {
        let cond = ast[tp].child(child::WHEN_COND);
        // look for scalar variables and event flags
        traverse_syntax_tree(
            &mut cx,
            cond,
            NodeTag::EVar.bit(),
            0,
            None,
            &mut |cx, ep, _| cx.scalar(ep),
        );
        // look for arrays and subscripted array elements
        traverse_syntax_tree(
            &mut cx,
            cond,
            NodeTag::EVar.bit() | NodeTag::ESubscr.bit(),
            0,
            None,
            &mut |cx, ep, _| cx.array(ep),
        );
    }
    cx.words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(text: &str) -> String {
        match c_string(text) {
            Expr::Atom(s) => s,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn simple_escapes_become_their_bytes() {
        assert_eq!(lit(r#"{P}\tx\"\\\?\'"#), r#""{P}\tx\"\\?'""#);
        assert_eq!(lit(r"\a\b\f\n\r\v"), r#""\u{7}\u{8}\u{c}\n\r\u{b}""#);
    }

    #[test]
    fn octal_takes_three_digits_at_most() {
        assert_eq!(lit(r"\101\1012\7"), r#""AA2\u{7}""#);
    }

    #[test]
    fn hex_takes_every_digit_and_truncates() {
        assert_eq!(lit(r"\x41g\x0041"), r#""AgA""#);
    }

    #[test]
    fn the_string_ends_at_a_nul() {
        assert_eq!(lit(r"c\0d"), r#""c""#);
        assert_eq!(lit(r"\x100"), r#""""#);
    }

    #[test]
    fn bytes_that_are_not_utf8_go_in_lossily() {
        assert_eq!(lit(r"\377é"), "\"\u{fffd}é\"");
    }
}
