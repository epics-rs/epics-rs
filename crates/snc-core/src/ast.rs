use crate::error::Span;

/// Top-level AST for an SNL program.
#[derive(Debug)]
pub struct Program {
    pub name: String,
    pub options: Vec<ProgramOption>,
    pub definitions: Vec<Definition>,
    pub entry: Option<Block>,
    pub state_sets: Vec<StateSet>,
    pub exit: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ProgramOption {
    Safe,      // +s
    Reentrant, // +r
    Main,      // +m
}

/// Top-level definitions (variables, assigns, monitors, etc.)
#[derive(Debug)]
pub enum Definition {
    VarDecl(VarDecl),
    Assign(Assign),
    Monitor(Monitor),
    Sync(Sync),
    EvFlag(EvFlagDecl),
    Option(ProgramOption),
    CPreprocessor(String), // #define, #include, etc. — skip
    EmbeddedCode(String),  // %%, %{ }%
}

#[derive(Debug)]
pub struct VarDecl {
    pub type_spec: TypeSpec,
    pub name: String,
    /// Array dimensions, e.g. `int arr[10]` → `[IntLit(10)]`.
    pub dimensions: Vec<Expr>,
    pub init: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeSpec {
    Int,
    Short,
    Long,
    Float,
    Double,
    String,
    Char,
    Unsigned(Box<TypeSpec>), // unsigned int, etc.
}

#[derive(Debug)]
pub struct Assign {
    pub var_name: String,
    pub pv_name: Option<String>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Monitor {
    pub var_name: String,
    pub span: Span,
}

#[derive(Debug)]
pub struct Sync {
    pub var_name: String,
    pub ef_name: String,
    pub span: Span,
}

#[derive(Debug)]
pub struct EvFlagDecl {
    pub name: String,
    pub span: Span,
}

#[derive(Debug)]
pub struct StateSet {
    pub name: String,
    pub local_vars: Vec<VarDecl>,
    pub states: Vec<State>,
    pub span: Span,
}

#[derive(Debug)]
pub struct State {
    pub name: String,
    pub entry: Option<Block>,
    pub transitions: Vec<Transition>,
    pub exit: Option<Block>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Transition {
    pub condition: Option<Expr>,
    pub body: Block,
    /// Target state name, or None for `exit`.
    pub target: TransitionTarget,
    pub span: Span,
}

#[derive(Debug)]
pub enum TransitionTarget {
    State(String),
    Exit,
}

/// Expression AST.
#[derive(Debug)]
pub enum Expr {
    IntLit(i64, Span),
    FloatLit(f64, Span),
    StringLit(String, Span),
    Ident(String, Span),
    BinaryOp(Box<Expr>, BinOp, Box<Expr>, Span),
    UnaryOp(UnaryOp, Box<Expr>, Span),
    Call(String, Vec<Expr>, Span),
    Assign(Box<Expr>, Box<Expr>, Span),
    CompoundAssign(Box<Expr>, BinOp, Box<Expr>, Span),
    Field(Box<Expr>, String, Span),
    Index(Box<Expr>, Box<Expr>, Span),
    Paren(Box<Expr>, Span),
    PostIncr(Box<Expr>, Span),
    PostDecr(Box<Expr>, Span),
    PreIncr(Box<Expr>, Span),
    PreDecr(Box<Expr>, Span),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>, Span),
    Cast(TypeSpec, Box<Expr>, Span),
    ArrayInit(Vec<Expr>, Span),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

#[derive(Debug, Clone, Copy)]
pub enum UnaryOp {
    Neg,
    Not,
    BitNot,
}

/// Statement within a block.
#[derive(Debug)]
pub enum Stmt {
    Expr(Expr),
    VarDecl(VarDecl),
    If(Expr, Block, Option<Block>),
    While(Expr, Block),
    For(Option<Box<Expr>>, Option<Expr>, Option<Box<Expr>>, Block),
    Break,
    Return(Option<Expr>),
    Block(Block),
    EmbeddedCode(String),
}

#[derive(Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}
