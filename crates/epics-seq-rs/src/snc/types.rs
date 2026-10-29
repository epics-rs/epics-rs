//! Common types, enumerations and accessors (C `types.h`).
//!
//! C links the syntax tree, declarations, types and channels with raw
//! pointers, several of them shared (one `Type` can back many declarations,
//! `mk_const_type` mutates it in place). Here every such object lives in an
//! [`Ast`] arena and is named by a copyable id, so sharing and in-place
//! mutation keep their C meaning.

use std::ops::{Index, IndexMut};
use std::sync::Arc;

use crate::snc::sym_table::SymTable;
use crate::snc::var_types::Type;

pub type TypeMask = u64;

macro_rules! id_type {
    ($($(#[$m:meta])* $name:ident),* $(,)?) => {$(
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);

        impl $name {
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    )*};
}

id_type!(
    /// A syntax node (C `Node *`).
    NodeId,
    /// A type (C `Type *`).
    TypeId,
    /// A variable (C `Var *`).
    VarId,
    /// A scope's variable list (C `VarList *`).
    VarListId,
    /// A channel (C `Chan *`).
    ChanId,
    /// A sync queue (C `SyncQ *`).
    SyncQId,
);

/// Run-time and compile-time options (C `struct options`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// do pvGet() asynchronously
    pub async_: bool,
    /// wait for all conns to complete
    pub conn: bool,
    /// run-time debug
    pub debug: bool,
    /// reentrant
    pub reent: bool,
    /// safe (no globals)
    pub safe: bool,
    /// new event flag mode
    pub newef: bool,
    /// generate main program
    pub main: bool,
    /// generate line markers
    pub line: bool,
    /// compiler warnings
    pub warn: bool,
    /// extra compiler warnings
    pub xwarn: bool,
}

/// C `DEFAULT_OPTIONS`.
impl Default for Options {
    fn default() -> Self {
        Options {
            async_: false,
            conn: true,
            debug: false,
            reent: false,
            safe: false,
            newef: true,
            main: false,
            line: true,
            warn: true,
            xwarn: false,
        }
    }
}

/// Run-time state options (C `struct state_options`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateOptions {
    /// reset timers on state entry from self
    pub do_reset_timers: bool,
    /// don't do entry actions if entering from same state
    pub no_entry_from_self: bool,
    /// don't do exit actions if exiting to same state
    pub no_exit_to_self: bool,
}

/// C `DEFAULT_STATE_OPTIONS`.
impl Default for StateOptions {
    fn default() -> Self {
        StateOptions {
            do_reset_timers: true,
            no_entry_from_self: true,
            no_exit_to_self: true,
        }
    }
}

macro_rules! toks {
    ($($v:ident = $name:literal,)*) => {
        /// Token codes, numbered as lemon numbers snl.lem's terminals (C
        /// `TOK_*` in the generated `snl.h`). `Pre` and `Post` are
        /// precedence pseudo tokens the lexer never returns.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(u8)]
        pub enum Tok {
            $($v,)*
        }

        impl Tok {
            /// Every token code, in code order.
            pub const ALL: &'static [Tok] = &[$(Tok::$v,)*];

            /// The terminal's name in snl.lem (`TOK_` stripped), `"$"` for
            /// end of input.
            pub fn grammar_name(self) -> &'static str {
                match self {
                    $(Tok::$v => $name,)*
                }
            }
        }
    };
}

toks! {
    Eoi = "$",
    Equal = "EQUAL",
    Addeq = "ADDEQ",
    Subeq = "SUBEQ",
    Andeq = "ANDEQ",
    Oreq = "OREQ",
    Diveq = "DIVEQ",
    Muleq = "MULEQ",
    Modeq = "MODEQ",
    Lsheq = "LSHEQ",
    Rsheq = "RSHEQ",
    Xoreq = "XOREQ",
    Question = "QUESTION",
    Colon = "COLON",
    Oror = "OROR",
    Andand = "ANDAND",
    Vbar = "VBAR",
    Caret = "CARET",
    Ampersand = "AMPERSAND",
    Eq = "EQ",
    Ne = "NE",
    Gt = "GT",
    Ge = "GE",
    Le = "LE",
    Lt = "LT",
    Lshift = "LSHIFT",
    Rshift = "RSHIFT",
    Add = "ADD",
    Sub = "SUB",
    Asterisk = "ASTERISK",
    Slash = "SLASH",
    Mod = "MOD",
    Not = "NOT",
    Tilde = "TILDE",
    Incr = "INCR",
    Decr = "DECR",
    Sizeof = "SIZEOF",
    Pre = "PRE",
    Lbracket = "LBRACKET",
    Lparen = "LPAREN",
    Pointer = "POINTER",
    Period = "PERIOD",
    Post = "POST",
    Program = "PROGRAM",
    Name = "NAME",
    Rparen = "RPAREN",
    Assign = "ASSIGN",
    Semicolon = "SEMICOLON",
    Lbrace = "LBRACE",
    Rbrace = "RBRACE",
    To = "TO",
    Comma = "COMMA",
    Monitor = "MONITOR",
    Sync = "SYNC",
    Syncq = "SYNCQ",
    Intcon = "INTCON",
    Rbracket = "RBRACKET",
    Const = "CONST",
    Foreign = "FOREIGN",
    Char = "CHAR",
    Short = "SHORT",
    Int = "INT",
    Long = "LONG",
    Unsigned = "UNSIGNED",
    Int8t = "INT8T",
    Uint8t = "UINT8T",
    Int16t = "INT16T",
    Uint16t = "UINT16T",
    Int32t = "INT32T",
    Uint32t = "UINT32T",
    Float = "FLOAT",
    Double = "DOUBLE",
    String = "STRING",
    Evflag = "EVFLAG",
    Void = "VOID",
    Enum = "ENUM",
    Struct = "STRUCT",
    Union = "UNION",
    Typename = "TYPENAME",
    Option = "OPTION",
    Ss = "SS",
    State = "STATE",
    Entry = "ENTRY",
    Exit = "EXIT",
    When = "WHEN",
    Break = "BREAK",
    Continue = "CONTINUE",
    Return = "RETURN",
    If = "IF",
    Else = "ELSE",
    While = "WHILE",
    For = "FOR",
    Fpcon = "FPCON",
    Strcon = "STRCON",
    Ccode = "CCODE",
}

/// A token as the lexer hands it to the parser (C `struct token`).
///
/// `str` and `file` are optional because C leaves them NULL in places: an
/// abstract declarator's token is all zero, and a `when` ending in `exit`
/// clears `str`. Messages print a missing string as glibc does, `(null)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub symbol: Tok,
    pub str: Option<Arc<str>>,
    pub file: Option<Arc<str>>,
    pub line: i32,
}

impl Token {
    /// C `Token k = {0,0,0,0}`.
    pub fn zero() -> Self {
        Token {
            symbol: Tok::Eoi,
            str: None,
            file: None,
            line: 0,
        }
    }

    /// The token text, `""` when C has NULL.
    pub fn text(&self) -> &str {
        self.str.as_deref().unwrap_or("")
    }
}

/// Extra data for `when` clauses (C `struct when`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct When {
    /// declaration of target state
    pub next_state: Option<NodeId>,
}

/// Extra data for state clauses (C `struct state`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// index in array of seqState structs
    pub index: u32,
    /// is this state a target state?
    pub is_target: bool,
    /// state options
    pub options: StateOptions,
    /// list of 'local' variables
    pub var_list: Option<VarListId>,
}

/// Extra data for state set clauses (C `struct state_set`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateSet {
    /// number of states
    pub num_states: u32,
    /// list of 'local' variables
    pub var_list: Option<VarListId>,
}

/// Syntax node kinds (C `enum node_tag`); the comment lists the children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeTag {
    /// assign statement \[subscr,pvs\]
    DAssign,
    /// variable declaration \[init\]
    DDecl,
    /// entry or exit statement \[block\]
    DEntex,
    /// function definition \[decl,block\]
    DFuncdef,
    /// monitor statement \[subscr\]
    DMonitor,
    /// option definition []
    DOption,
    /// whole program \[param,defns,entry,statesets,exit,xdefns\]
    DProg,
    /// state set statement \[defns,states\]
    DSs,
    /// state statement \[defns,entry,whens,exit\]
    DState,
    /// struct definition \[members\]
    DStructdef,
    /// sync statement \[subscr,evflag\]
    DSync,
    /// syncq statement \[subscr,evflag,maxqsize\]
    DSyncq,
    /// when statement \[cond,block\]
    DWhen,
    /// binary operator \[left,right\]
    EBinop,
    /// builtin function []
    EBuiltin,
    /// type cast \[type,operand\]
    ECast,
    /// numeric (inkl. character) constant []
    EConst,
    /// function call \[expr,args\]
    EFunc,
    /// array or struct initializer \[elems\]
    EInit,
    /// struct or union member []
    EMember,
    /// parenthesized expression \[expr\]
    EParen,
    /// unary postfix operator \[operand\]
    EPost,
    /// unary prefix operator \[operand\]
    EPre,
    /// member selection \[left,right\]
    ESelect,
    /// string constant []
    EString,
    /// subscripted expr \[operand,index\]
    ESubscr,
    /// ternary operator \[cond,then,else\]
    ETernop,
    /// variable []
    EVar,
    /// state change statement []
    SChange,
    /// compound statement \[defns,stmts\]
    SCmpnd,
    /// for statement \[init,cond,iter,stmt\]
    SFor,
    /// if statement \[cond,then,else\]
    SIf,
    /// break or continue stmt []
    SJump,
    /// return stmt \[expr\]
    SReturn,
    /// simple statement, i.e. 'expr;'  \[expr\]
    SStmt,
    /// while statement \[cond,stmt\]
    SWhile,
    /// C code or other text to be inserted []
    TText,
}

impl NodeTag {
    /// C `node_info[tag].name`.
    pub fn name(self) -> &'static str {
        NODE_INFO[self as usize].0
    }

    /// C `node_info[tag].num_children`.
    pub fn num_children(self) -> usize {
        NODE_INFO[self as usize].1
    }

    /// C `bit(tag)`.
    pub const fn bit(self) -> TypeMask {
        1 << (self as u32)
    }
}

const NODE_INFO: [(&str, usize); 37] = [
    ("D_ASSIGN", 2),
    ("D_DECL", 1),
    ("D_ENTEX", 1),
    ("D_FUNCDEF", 2),
    ("D_MONITOR", 1),
    ("D_OPTION", 0),
    ("D_PROG", 6),
    ("D_SS", 2),
    ("D_STATE", 4),
    ("D_STRUCTDEF", 1),
    ("D_SYNC", 2),
    ("D_SYNCQ", 3),
    ("D_WHEN", 2),
    ("E_BINOP", 2),
    ("E_BUILTIN", 0),
    ("E_CAST", 2),
    ("E_CONST", 0),
    ("E_FUNC", 2),
    ("E_INIT", 1),
    ("E_MEMBER", 0),
    ("E_PAREN", 1),
    ("E_POST", 1),
    ("E_PRE", 1),
    ("E_SELECT", 2),
    ("E_STRING", 0),
    ("E_SUBSCR", 2),
    ("E_TERNOP", 3),
    ("E_VAR", 0),
    ("S_CHANGE", 0),
    ("S_CMPND", 2),
    ("S_FOR", 4),
    ("S_IF", 3),
    ("S_JUMP", 0),
    ("S_RETURN", 1),
    ("S_STMT", 1),
    ("S_WHILE", 2),
    ("T_TEXT", 0),
];

use NodeTag::*;

/// Node kinds that are scopes, i.e. allow variable declarations as
/// immediate subnodes (C `scope_mask`).
pub const SCOPE_MASK: TypeMask =
    DProg.bit() | DFuncdef.bit() | DSs.bit() | DState.bit() | SCmpnd.bit();

/// Node kinds that may have sub-scopes (C `has_sub_scope_mask`).
pub const HAS_SUB_SCOPE_MASK: TypeMask = DEntex.bit()
    | DFuncdef.bit()
    | DProg.bit()
    | DSs.bit()
    | DState.bit()
    | DWhen.bit()
    | SCmpnd.bit()
    | SFor.bit()
    | SIf.bit()
    | SStmt.bit()
    | SWhile.bit();

/// Node kinds that are expressions, i.e. no definitions or statements
/// (C `expr_mask`).
pub const EXPR_MASK: TypeMask = EBinop.bit()
    | ECast.bit()
    | EConst.bit()
    | EFunc.bit()
    | EInit.bit()
    | EParen.bit()
    | EPost.bit()
    | EPre.bit()
    | ESelect.bit()
    | EString.bit()
    | ESubscr.bit()
    | ETernop.bit()
    | EVar.bit()
    | TText.bit();

/// C `is_scope(e)`.
pub fn is_scope(tag: NodeTag) -> bool {
    tag.bit() & SCOPE_MASK != 0
}

/// Child positions (C `assign_subscr` ... `while_stmt` accessor macros).
pub mod child {
    pub const ASSIGN_SUBSCR: usize = 0;
    pub const ASSIGN_PVS: usize = 1;
    pub const BINOP_LEFT: usize = 0;
    pub const BINOP_RIGHT: usize = 1;
    pub const CAST_TYPE: usize = 0;
    pub const CAST_OPERAND: usize = 1;
    pub const CMPND_DEFNS: usize = 0;
    pub const CMPND_STMTS: usize = 1;
    pub const DECL_INIT: usize = 0;
    pub const ENTEX_BLOCK: usize = 0;
    pub const FOR_INIT: usize = 0;
    pub const FOR_COND: usize = 1;
    pub const FOR_ITER: usize = 2;
    pub const FOR_STMT: usize = 3;
    pub const FUNC_EXPR: usize = 0;
    pub const FUNC_ARGS: usize = 1;
    pub const FUNCDEF_DECL: usize = 0;
    pub const FUNCDEF_BLOCK: usize = 1;
    pub const IF_COND: usize = 0;
    pub const IF_THEN: usize = 1;
    pub const IF_ELSE: usize = 2;
    pub const INIT_ELEMS: usize = 0;
    pub const MONITOR_SUBSCR: usize = 0;
    pub const PAREN_EXPR: usize = 0;
    pub const POST_OPERAND: usize = 0;
    pub const PRE_OPERAND: usize = 0;
    pub const PROG_PARAM: usize = 0;
    pub const PROG_DEFNS: usize = 1;
    pub const PROG_ENTRY: usize = 2;
    pub const PROG_STATESETS: usize = 3;
    pub const PROG_EXIT: usize = 4;
    pub const PROG_XDEFNS: usize = 5;
    pub const RETURN_EXPR: usize = 0;
    pub const SELECT_LEFT: usize = 0;
    pub const SELECT_RIGHT: usize = 1;
    pub const SS_DEFNS: usize = 0;
    pub const SS_STATES: usize = 1;
    pub const STATE_DEFNS: usize = 0;
    pub const STATE_ENTRY: usize = 1;
    pub const STATE_WHENS: usize = 2;
    pub const STATE_EXIT: usize = 3;
    pub const STMT_EXPR: usize = 0;
    pub const STRUCTDEF_MEMBERS: usize = 0;
    pub const SUBSCR_OPERAND: usize = 0;
    pub const SUBSCR_INDEX: usize = 1;
    pub const SYNC_SUBSCR: usize = 0;
    pub const SYNC_EVFLAG: usize = 1;
    pub const SYNCQ_SUBSCR: usize = 0;
    pub const SYNCQ_EVFLAG: usize = 1;
    pub const SYNCQ_SIZE: usize = 2;
    pub const TERNOP_COND: usize = 0;
    pub const TERNOP_THEN: usize = 1;
    pub const TERNOP_ELSE: usize = 2;
    pub const WHEN_COND: usize = 0;
    pub const WHEN_BLOCK: usize = 1;
    pub const WHILE_COND: usize = 0;
    pub const WHILE_STMT: usize = 1;
}

/// Extra data of a node, by tag (C `union extra`). `None` is C's zeroed
/// union.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Extra {
    #[default]
    None,
    /// variable reference
    Var(VarId),
    /// variable declaration
    Decl(VarId),
    /// option value
    Option(bool),
    /// top-level declarations
    Prog(VarListId),
    /// state set data
    Ss(StateSet),
    /// state data
    State(State),
    /// transition data
    When(When),
    /// declaration of target state
    Change(NodeId),
    /// block local declarations
    Cmpnd(VarListId),
    /// builtin function, by index into builtin's function table
    Builtin(usize),
    /// builtin constant, by index into builtin's constant table
    Const(usize),
    /// parameters
    Funcdef(VarListId),
}

/// A generic syntax node (C `struct syntax_node`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// next in list
    pub next: Option<NodeId>,
    /// last in list; correct only for the first element (see `link_node`)
    pub last: Option<NodeId>,
    /// children [left,right,...], `tag.num_children()` of them
    pub children: Vec<Option<NodeId>>,
    /// what kind of node
    pub tag: NodeTag,
    pub token: Token,
    /// extra data, depends on tag
    pub extra: Extra,
}

impl Node {
    /// The `i`th child (see [`child`]).
    pub fn child(&self, i: usize) -> Option<NodeId> {
        self.children[i]
    }

    /// C `ep->extra.e_decl`.
    pub fn e_decl(&self) -> Option<VarId> {
        match self.extra {
            Extra::Decl(v) => Some(v),
            _ => None,
        }
    }
}

/// C `struct event_flag`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvFlag {
    pub index: u32,
    pub queued: bool,
}

/// For channel assign, monitor, sync, and syncq (C `enum multiplicity`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Multiplicity {
    /// not at all
    #[default]
    None,
    /// whole variable (array or scalar)
    Single,
    /// array, each element treated separately
    Multi,
}

/// A variable's channel data (C `union chan`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum VarChan {
    #[default]
    None,
    /// single channel if assign == M_SINGLE
    Single(ChanId),
    /// multiple channels if assign == M_MULTI
    Multi(Vec<Option<ChanId>>),
    /// event flag data if this is an event flag
    EvFlag(EvFlag),
}

/// Variable definition (C `struct variable`).
///
/// Laws (invariants), as C states them:
/// L1a: monitor == M_MULTI => assign == M_MULTI;
/// L1b: sync == M_MULTI => assign == M_MULTI;
/// L1c: syncq == M_MULTI => assign == M_MULTI;
/// L2a: assign == M_NONE => monitor == M_NONE;
/// L2b: assign == M_NONE => sync == M_NONE;
/// L2c: assign == M_NONE => syncq == M_NONE;
/// L3: assign == M_MULTI => type->tag == T_ARRAY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Var {
    /// link to next variable in list
    pub next: Option<VarId>,
    /// variable name
    pub name: Option<Arc<str>>,
    /// declaration of this variable (or None if not declared)
    pub decl: Option<NodeId>,
    /// scope of this variable
    pub scope: Option<NodeId>,
    /// type of this variable
    pub type_: TypeId,
    /// assigned: one of enum multiplicity
    pub assign: Multiplicity,
    /// monitored: one of enum multiplicity
    pub monitor: Multiplicity,
    /// sync'd: one of enum multiplicity
    pub sync: Multiplicity,
    /// syncq'd: one of enum multiplicity
    pub syncq: Multiplicity,
    pub chan: VarChan,
    /// index (base) in seqChan array
    pub index: u32,
}

/// Channel assignment info (C `struct channel`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chan {
    /// link to next channel in list
    pub next: Option<ChanId>,
    /// channel (pv) name
    pub name: Option<Arc<str>>,
    /// index (offset) if array element
    pub index: u32,
    /// variable definition
    pub var: VarId,
    /// request count for pv access
    pub count: u32,
    /// whether this channel is monitored
    pub monitor: bool,
    /// event flag variable if sync'd
    pub sync: Option<VarId>,
    /// sync queue if syncQ'd
    pub syncq: Option<SyncQId>,
}

/// C `struct sync_queue`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncQ {
    pub next: Option<SyncQId>,
    pub index: u32,
    pub size: u32,
}

/// C `struct chan_list`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChanList {
    pub first: Option<ChanId>,
    pub last: Option<ChanId>,
    /// number of elements in this list
    pub num_elems: u32,
}

/// C `struct sync_queue_list`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncQList {
    pub first: Option<SyncQId>,
    pub last: Option<SyncQId>,
    /// number of elements in this list
    pub num_elems: u32,
}

/// C `struct var_list`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VarList {
    pub first: Option<VarId>,
    pub last: Option<VarId>,
    /// next surrounding scope
    pub parent_scope: Option<NodeId>,
}

/// The second half of a symbol-table key: C passes an object's address as
/// the `void *type`, so a name can be bound once per scope or namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SymScope {
    /// analysis.c's `structdefs` marker
    StructDefs,
    /// a scope's variable list
    VarList(VarListId),
    /// the program (state sets) or a state set (states)
    Node(NodeId),
    /// builtin.c's `const_symbols` marker
    ConstSymbols,
    /// builtin.c's `func_symbols` marker
    FuncSymbols,
}

/// A symbol-table value (C `void *value`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sym {
    Type(TypeId),
    Var(VarId),
    Node(NodeId),
    /// builtin constant, by index into builtin's constant table
    Const(usize),
    /// builtin function, by index into builtin's function table
    Func(usize),
}

/// C `struct program`.
pub struct Program {
    /// the whole syntax tree
    pub prog: NodeId,
    /// program name
    pub name: Option<Arc<str>>,
    /// parameter string for program stmt
    pub param: Option<Arc<str>>,
    /// program options, from source or command line
    pub options: Options,
    /// symbol table
    pub sym_table: SymTable<SymScope, Sym>,
    /// channel list, incl. number of channels
    pub chan_list: ChanList,
    /// syncq list, incl. number of syncqs
    pub syncq_list: SyncQList,
    /// number of state sets
    pub num_ss: u32,
    /// number of event flags
    pub num_event_flags: u32,
}

/// Owner of every node, type, variable and channel (C's `new`/`newArray`
/// allocations).
#[derive(Clone, Debug, Default)]
pub struct Ast {
    pub nodes: Vec<Node>,
    pub types: Vec<Type>,
    pub vars: Vec<Var>,
    pub var_lists: Vec<VarList>,
    pub chans: Vec<Chan>,
    pub syncqs: Vec<SyncQ>,
}

macro_rules! arena_index {
    ($($id:ident => $field:ident: $t:ty),* $(,)?) => {$(
        impl Index<$id> for Ast {
            type Output = $t;
            fn index(&self, id: $id) -> &$t {
                &self.$field[id.index()]
            }
        }
        impl IndexMut<$id> for Ast {
            fn index_mut(&mut self, id: $id) -> &mut $t {
                &mut self.$field[id.index()]
            }
        }
    )*};
}

arena_index!(
    NodeId => nodes: Node,
    TypeId => types: Type,
    VarId => vars: Var,
    VarListId => var_lists: VarList,
    ChanId => chans: Chan,
    SyncQId => syncqs: SyncQ,
);

fn next_id(len: usize) -> u32 {
    u32::try_from(len).expect("arena exceeds u32 ids")
}

impl Ast {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, node: Node) -> NodeId {
        let id = NodeId(next_id(self.nodes.len()));
        self.nodes.push(node);
        id
    }

    pub fn add_type(&mut self, t: Type) -> TypeId {
        let id = TypeId(next_id(self.types.len()));
        self.types.push(t);
        id
    }

    /// C `new(Var)` with name and type filled in.
    pub fn add_var(&mut self, name: Option<Arc<str>>, type_: TypeId) -> VarId {
        let id = VarId(next_id(self.vars.len()));
        self.vars.push(Var {
            next: None,
            name,
            decl: None,
            scope: None,
            type_,
            assign: Multiplicity::None,
            monitor: Multiplicity::None,
            sync: Multiplicity::None,
            syncq: Multiplicity::None,
            chan: VarChan::None,
            index: 0,
        });
        id
    }

    pub fn add_var_list(&mut self, list: VarList) -> VarListId {
        let id = VarListId(next_id(self.var_lists.len()));
        self.var_lists.push(list);
        id
    }

    pub fn add_chan(&mut self, chan: Chan) -> ChanId {
        let id = ChanId(next_id(self.chans.len()));
        self.chans.push(chan);
        id
    }

    pub fn add_syncq(&mut self, syncq: SyncQ) -> SyncQId {
        let id = SyncQId(next_id(self.syncqs.len()));
        self.syncqs.push(syncq);
        id
    }

    /// The nodes of the list starting at `first`, following `next`
    /// (C `foreach(e, l)`).
    pub fn list(&self, first: Option<NodeId>) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::successors(first, |&id| self[id].next)
    }

    /// C `funcdef_params`: the parameter declarations of a function
    /// definition node.
    pub fn funcdef_params(&self, funcdef: NodeId) -> Option<NodeId> {
        let decl = self[funcdef].children[child::FUNCDEF_DECL].expect("funcdef without decl");
        let var = self[decl].e_decl().expect("funcdef decl without var");
        match self[self[var].type_].kind {
            crate::snc::var_types::TypeKind::Function { param_decls, .. } => param_decls,
            _ => panic!("funcdef decl is not a function type"),
        }
    }
}
