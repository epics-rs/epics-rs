//! The Rust snc-rs writes, as a tree, and its printer.
//!
//! C snc prints its output with `gen_code()` calls. snc-rs builds
//! [`Item`]s and [`print`](fn@print)s them in a fixed layout that does not depend on
//! line width: four-space indentation, one item or statement per line,
//! a struct literal one field per line, and any other list (arguments,
//! array elements) one element per line only when an element spans lines
//! or carries a comment. The printer also owns precedence: it inserts the
//! parentheses Rust needs, so a tree cannot print as a different parse.

/// A module-level item, or a line between items.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// `// text`.
    Comment(String),
    /// An empty line.
    Blank,
    /// An attribute line printed as is: `#![allow(..)]`, `#[derive(Clone)]`.
    Attr(String),
    /// `[pub ]use path;`.
    Use {
        public: bool,
        path: String,
    },
    /// `[pub ]const|static name: ty = expr;`.
    Const(Const),
    Fn(Fn),
    Struct(Struct),
    /// `head { items }`, e.g. `impl seqg_rt::ProgramVars for seqg_vars`.
    Impl(String, Vec<Item>),
    /// `mod name { items }`.
    Mod(String, Vec<Item>),
    /// An expression item followed by `;`: a macro call such as
    /// `seqg_escaped!(r#"..."#);`.
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Const {
    pub public: bool,
    /// `static` rather than `const`.
    pub is_static: bool,
    pub name: String,
    pub ty: String,
    pub expr: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fn {
    pub public: bool,
    pub name: String,
    pub params: Vec<Param>,
    /// The type after `->`; `None` for `()`.
    pub ret: Option<String>,
    pub body: Vec<Stmt>,
}

/// `name: ty`.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: String,
}

/// `[pub ]struct name { fields }`; attributes are [`Item::Attr`]s before
/// it.
#[derive(Clone, Debug, PartialEq)]
pub struct Struct {
    pub public: bool,
    pub name: String,
    pub fields: Vec<Field>,
}

/// A struct field, each preceded by its comment lines.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub comments: Vec<String>,
    pub public: bool,
    pub name: String,
    pub ty: String,
}

/// A statement of a function or block body.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// `// text`.
    Comment(String),
    /// `let pat[: ty][ = init];`.
    Let {
        pat: String,
        ty: Option<String>,
        init: Option<Expr>,
    },
    /// An expression statement: `expr;`, or without the `;` when the
    /// expression is block-like (`if`, `while`, `loop`, `match`, a block).
    Expr(Expr),
    /// The value of the block: the last statement, without `;`.
    Tail(Expr),
    /// An item inside a body, e.g. a `static`.
    Item(Item),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// One token printed as is: a path, a literal, `true`.
    Atom(String),
    /// `callee(args)`.
    Call(Box<Expr>, Vec<Arg>),
    /// `recv.name(args)`.
    Method(Box<Expr>, String, Vec<Arg>),
    /// `base.name`.
    Field(Box<Expr>, String),
    /// `base[index]`.
    Index(Box<Expr>, Box<Expr>),
    /// `name!(args)`, written without a trailing comma.
    Macro(String, Vec<Arg>),
    /// `[elems]`.
    Array(Vec<Arg>),
    /// `op expr`, `op` one of `&`, `*`, `!`, `-`.
    Unary(&'static str, Box<Expr>),
    /// `lhs op rhs` for a binary operator (not an assignment).
    Binary(Box<Expr>, &'static str, Box<Expr>),
    /// `lhs op rhs` for `=` and the compound assignments.
    Assign(Box<Expr>, &'static str, Box<Expr>),
    /// `expr as ty`.
    Cast(Box<Expr>, String),
    /// `(expr)`, for parentheses precedence does not need.
    Paren(Box<Expr>),
    /// `|params| body`.
    Closure(String, Box<Expr>),
    /// `{ stmts }`.
    Block(Vec<Stmt>),
    /// `if cond { then }[ else els]`; `els` is an [`Expr::Block`] or an
    /// [`Expr::If`].
    If(Box<Expr>, Vec<Stmt>, Option<Box<Expr>>),
    /// `while cond { body }`.
    While(Box<Expr>, Vec<Stmt>),
    /// `loop { body }`.
    Loop(Vec<Stmt>),
    /// `match scrutinee { arms }`.
    Match(Box<Expr>, Vec<Arm>),
    /// `path { name: expr, .. }`.
    Struct(String, Vec<(String, Expr)>),
    /// `return[ expr]`.
    Return(Option<Box<Expr>>),
    Break,
    Continue,
}

/// An argument or array element, with an optional `// comment` after it.
#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub expr: Expr,
    pub comment: Option<String>,
}

/// `pat => { body }`.
#[derive(Clone, Debug, PartialEq)]
pub struct Arm {
    pub pat: String,
    pub body: Vec<Stmt>,
}

impl From<Expr> for Arg {
    fn from(expr: Expr) -> Self {
        Arg {
            expr,
            comment: None,
        }
    }
}

impl Arg {
    /// `expr, // comment`.
    pub fn commented(expr: Expr, comment: impl Into<String>) -> Self {
        Arg {
            expr,
            comment: Some(comment.into()),
        }
    }
}

impl Expr {
    pub fn atom(s: impl Into<String>) -> Self {
        Expr::Atom(s.into())
    }

    /// A Rust string literal holding `s`.
    pub fn str_lit(s: &str) -> Self {
        Expr::Atom(format!("{s:?}"))
    }

    /// The literal for a C string token's text (between the quotes,
    /// escapes as written): a `&str` literal, or `&b"..."[..]` when the
    /// bytes are not UTF-8.
    pub fn c_str(token: &str) -> Self {
        let bytes = c_unescape(token.as_bytes());
        match String::from_utf8(bytes) {
            Ok(s) => Expr::str_lit(&s),
            Err(e) => {
                let mut lit = String::from("b\"");
                for &b in e.as_bytes() {
                    match b {
                        b'"' => lit.push_str("\\\""),
                        b'\\' => lit.push_str("\\\\"),
                        b' '..=b'~' => lit.push(b as char),
                        _ => lit.push_str(&format!("\\x{b:02x}")),
                    }
                }
                lit.push('"');
                Expr::unary(
                    "&",
                    Expr::Index(Box::new(Expr::Atom(lit)), Box::new(Expr::atom(".."))),
                )
            }
        }
    }

    /// `callee(args)` for a path `callee`.
    pub fn call(callee: impl Into<String>, args: Vec<Expr>) -> Self {
        Expr::Call(
            Box::new(Expr::atom(callee)),
            args.into_iter().map(Arg::from).collect(),
        )
    }

    /// `self.name(args)`.
    pub fn method(self, name: impl Into<String>, args: Vec<Expr>) -> Self {
        Expr::Method(
            Box::new(self),
            name.into(),
            args.into_iter().map(Arg::from).collect(),
        )
    }

    /// `self.name`.
    pub fn field(self, name: impl Into<String>) -> Self {
        Expr::Field(Box::new(self), name.into())
    }

    /// `self[index]`.
    pub fn index(self, index: Expr) -> Self {
        Expr::Index(Box::new(self), Box::new(index))
    }

    pub fn unary(op: &'static str, e: Expr) -> Self {
        Expr::Unary(op, Box::new(e))
    }

    pub fn binary(lhs: Expr, op: &'static str, rhs: Expr) -> Self {
        Expr::Binary(Box::new(lhs), op, Box::new(rhs))
    }

    pub fn assign(lhs: Expr, op: &'static str, rhs: Expr) -> Self {
        Expr::Assign(Box::new(lhs), op, Box::new(rhs))
    }

    pub fn cast(self, ty: impl Into<String>) -> Self {
        Expr::Cast(Box::new(self), ty.into())
    }
}

/// The bytes a C string or character literal's text stands for (C99
/// 6.4.4.4): simple, octal and hex escapes decoded, anything else kept.
pub fn c_unescape(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let c = text[i];
        i += 1;
        if c != b'\\' || i == text.len() {
            out.push(c);
            continue;
        }
        let e = text[i];
        i += 1;
        out.push(match e {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'v' => 0x0b,
            b'0'..=b'7' => {
                let mut v = u32::from(e - b'0');
                for _ in 0..2 {
                    match text.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(d - b'0');
                            i += 1;
                        }
                        _ => break,
                    }
                }
                v as u8
            }
            b'x' => {
                let mut v = 0u32;
                while let Some(d) = text.get(i).and_then(|&d| (d as char).to_digit(16)) {
                    v = v.wrapping_mul(16) + d;
                    i += 1;
                }
                v as u8
            }
            // `\\`, `\'`, `\"`, `\?`, and unknown escapes, which C
            // compilers take as the character itself
            _ => e,
        });
    }
    out
}

/// The text of a file holding `items` at module level.
pub fn print(items: &[Item]) -> String {
    let mut out = String::new();
    for item in items {
        line(&mut out, 0, &item_text(item, 0));
    }
    out
}

const TAB: usize = 4;

/// Append `text` as a line at `ind`, or an empty line.
fn line(out: &mut String, ind: usize, text: &str) {
    if !text.is_empty() {
        out.extend(std::iter::repeat_n(' ', ind));
        out.push_str(text);
    }
    out.push('\n');
}

fn nl(ind: usize) -> String {
    format!("\n{}", " ".repeat(ind))
}

fn vis(public: bool) -> &'static str {
    if public { "pub " } else { "" }
}

fn comment(text: &str) -> String {
    if text.is_empty() {
        "//".into()
    } else {
        format!("// {text}")
    }
}

/// An item starting at column `ind`; later lines carry their own indent.
fn item_text(item: &Item, ind: usize) -> String {
    match item {
        Item::Comment(c) => comment(c),
        Item::Blank => String::new(),
        Item::Attr(a) => a.clone(),
        Item::Use { public, path } => format!("{}use {path};", vis(*public)),
        Item::Const(c) => format!(
            "{}{} {}: {} = {};",
            vis(c.public),
            if c.is_static { "static" } else { "const" },
            c.name,
            c.ty,
            expr(&c.expr, ind)
        ),
        Item::Fn(f) => {
            let params: Vec<_> = f
                .params
                .iter()
                .map(|p| format!("{}: {}", p.name, p.ty))
                .collect();
            let ret = f
                .ret
                .as_ref()
                .map(|r| format!(" -> {r}"))
                .unwrap_or_default();
            format!(
                "{}fn {}({}){ret} {}",
                vis(f.public),
                f.name,
                params.join(", "),
                block(&f.body, ind)
            )
        }
        Item::Struct(s) => {
            let mut t = format!("{}struct {} {{", vis(s.public), s.name);
            if s.fields.is_empty() {
                t.push('}');
                return t;
            }
            for f in &s.fields {
                for c in &f.comments {
                    t += &nl(ind + TAB);
                    t += &comment(c);
                }
                t += &nl(ind + TAB);
                t += &format!("{}{}: {},", vis(f.public), f.name, f.ty);
            }
            t += &nl(ind);
            t.push('}');
            t
        }
        Item::Impl(head, items) => braced_items(head, items, ind),
        Item::Mod(name, items) => braced_items(&format!("mod {name}"), items, ind),
        Item::Expr(e) => format!("{};", expr(e, ind)),
    }
}

fn braced_items(head: &str, items: &[Item], ind: usize) -> String {
    if items.is_empty() {
        return format!("{head} {{}}");
    }
    let mut t = format!("{head} {{\n");
    for item in items {
        line(&mut t, ind + TAB, &item_text(item, ind + TAB));
    }
    t.extend(std::iter::repeat_n(' ', ind));
    t.push('}');
    t
}

/// `{}` or `{ stmts }` with the statements at `ind + 4`.
fn block(stmts: &[Stmt], ind: usize) -> String {
    if stmts.is_empty() {
        return "{}".into();
    }
    let mut t = String::from("{\n");
    for st in stmts {
        line(&mut t, ind + TAB, &stmt_text(st, ind + TAB));
    }
    t.extend(std::iter::repeat_n(' ', ind));
    t.push('}');
    t
}

fn stmt_text(st: &Stmt, ind: usize) -> String {
    match st {
        Stmt::Comment(c) => comment(c),
        Stmt::Let { pat, ty, init } => {
            let mut t = format!("let {pat}");
            if let Some(ty) = ty {
                t += &format!(": {ty}");
            }
            if let Some(init) = init {
                t += &format!(" = {}", expr(init, ind));
            }
            t.push(';');
            t
        }
        Stmt::Expr(e) if is_block_like(e) => expr(e, ind),
        Stmt::Expr(e) => format!("{};", expr(e, ind)),
        Stmt::Tail(e) => expr(e, ind),
        Stmt::Item(item) => item_text(item, ind),
    }
}

fn is_block_like(e: &Expr) -> bool {
    matches!(
        e,
        Expr::If(..) | Expr::While(..) | Expr::Loop(..) | Expr::Match(..) | Expr::Block(..)
    )
}

/// Binding strength: postfix and primary 15, unary 14, `as` 13, the
/// binary operators 3..=11, and assignment, closures and `return` 1.
fn prec(e: &Expr) -> u8 {
    match e {
        Expr::Atom(a) if a.starts_with('-') => 14,
        Expr::Unary(..) => 14,
        Expr::Cast(..) => 13,
        Expr::Binary(_, op, _) => binop_prec(op),
        Expr::Assign(..) | Expr::Closure(..) | Expr::Return(..) | Expr::Break | Expr::Continue => 1,
        _ => 15,
    }
}

fn binop_prec(op: &str) -> u8 {
    match op {
        "*" | "/" | "%" => 11,
        "+" | "-" => 10,
        "<<" | ">>" => 9,
        "&" => 8,
        "^" => 7,
        "|" => 6,
        "==" | "!=" | "<" | ">" | "<=" | ">=" => 5,
        "&&" => 4,
        "||" => 3,
        _ => panic!("not a binary operator: {op}"),
    }
}

/// Whether `e` printed bare ends in an `as` cast, which would take a
/// following `<` or `<<` as the start of generic arguments.
fn ends_with_cast(e: &Expr) -> bool {
    match e {
        Expr::Cast(..) => true,
        Expr::Binary(_, op, rhs) => binop_prec(op) < prec(rhs) && ends_with_cast(rhs),
        _ => false,
    }
}

fn operand(e: &Expr, paren: bool, ind: usize) -> String {
    if paren {
        format!("({})", expr(e, ind))
    } else {
        expr(e, ind)
    }
}

/// The receiver of a method call or field access, a callee, an indexed
/// base.
fn postfix_base(e: &Expr, ind: usize) -> String {
    operand(e, prec(e) < 15 || is_block_like(e), ind)
}

/// An expression starting mid-line on a line indented `ind`.
fn expr(e: &Expr, ind: usize) -> String {
    match e {
        Expr::Atom(a) => a.clone(),
        Expr::Call(callee, args) => {
            format!(
                "{}{}",
                postfix_base(callee, ind),
                list("(", ")", args, true, ind)
            )
        }
        Expr::Method(recv, name, args) => {
            format!(
                "{}.{name}{}",
                postfix_base(recv, ind),
                list("(", ")", args, true, ind)
            )
        }
        Expr::Field(base, name) => format!("{}.{name}", postfix_base(base, ind)),
        Expr::Index(base, index) => format!("{}[{}]", postfix_base(base, ind), expr(index, ind)),
        Expr::Macro(name, args) => format!("{name}!{}", list("(", ")", args, false, ind)),
        Expr::Array(elems) => list("[", "]", elems, true, ind),
        Expr::Unary(op, x) => {
            // `--x` would lint as a double negation
            let double_neg = *op == "-"
                && match &**x {
                    Expr::Unary("-", _) => true,
                    Expr::Atom(s) => s.starts_with('-'),
                    _ => false,
                };
            format!("{op}{}", operand(x, prec(x) < 14 || double_neg, ind))
        }
        Expr::Binary(lhs, op, rhs) => {
            let p = binop_prec(op);
            let lhs_paren = prec(lhs) < p
                || (p == 5 && prec(lhs) == 5)
                || (matches!(*op, "<" | "<<") && ends_with_cast(lhs));
            format!(
                "{} {op} {}",
                operand(lhs, lhs_paren, ind),
                operand(rhs, prec(rhs) <= p, ind)
            )
        }
        Expr::Assign(lhs, op, rhs) => format!("{} {op} {}", expr(lhs, ind), expr(rhs, ind)),
        Expr::Cast(x, ty) => format!("{} as {ty}", operand(x, prec(x) < 14, ind)),
        Expr::Paren(x) => format!("({})", expr(x, ind)),
        Expr::Closure(params, body) => format!("|{params}| {}", expr(body, ind)),
        Expr::Block(stmts) => block(stmts, ind),
        Expr::If(cond, then, els) => {
            let mut t = format!("if {} {}", expr(cond, ind), block(then, ind));
            if let Some(els) = els {
                t += &format!(" else {}", expr(els, ind));
            }
            t
        }
        Expr::While(cond, body) => format!("while {} {}", expr(cond, ind), block(body, ind)),
        Expr::Loop(body) => format!("loop {}", block(body, ind)),
        Expr::Match(scrutinee, arms) => {
            let mut t = format!("match {} {{", expr(scrutinee, ind));
            for arm in arms {
                t += &nl(ind + TAB);
                t += &format!("{} => {}", arm.pat, block(&arm.body, ind + TAB));
            }
            t += &nl(ind);
            t.push('}');
            t
        }
        Expr::Struct(path, fields) => {
            if fields.is_empty() {
                return format!("{path} {{}}");
            }
            let mut t = format!("{path} {{");
            for (name, value) in fields {
                t += &nl(ind + TAB);
                t += &format!("{name}: {},", expr(value, ind + TAB));
            }
            t += &nl(ind);
            t.push('}');
            t
        }
        Expr::Return(None) => "return".into(),
        Expr::Return(Some(x)) => format!("return {}", expr(x, ind)),
        Expr::Break => "break".into(),
        Expr::Continue => "continue".into(),
    }
}

/// `open items close`: on one line, or one item per line when an item
/// spans lines or has a comment. A macro's list gets no trailing comma.
fn list(open: &str, close: &str, items: &[Arg], trailing_comma: bool, ind: usize) -> String {
    let one_line: Vec<_> = items.iter().map(|a| expr(&a.expr, ind)).collect();
    if items.iter().all(|a| a.comment.is_none()) && one_line.iter().all(|s| !s.contains('\n')) {
        return format!("{open}{}{close}", one_line.join(", "));
    }
    let mut t = String::from(open);
    for (i, a) in items.iter().enumerate() {
        t += &nl(ind + TAB);
        t += &expr(&a.expr, ind + TAB);
        if trailing_comma || i + 1 < items.len() {
            t.push(',');
        }
        if let Some(c) = &a.comment {
            t.push(' ');
            t += &comment(c);
        }
    }
    t += &nl(ind);
    t += close;
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Expr {
        Expr::atom(s)
    }

    #[test]
    fn precedence_inserts_parentheses() {
        let sum = Expr::binary(a("a"), "+", a("b"));
        assert_eq!(
            expr(&Expr::binary(sum.clone(), "*", a("c")), 0),
            "(a + b) * c"
        );
        assert_eq!(
            expr(&Expr::binary(a("c"), "-", sum.clone()), 0),
            "c - (a + b)"
        );
        assert_eq!(
            expr(&Expr::binary(sum.clone(), "-", a("c")), 0),
            "a + b - c"
        );
        let cmp = Expr::binary(a("a"), "==", a("b"));
        assert_eq!(expr(&Expr::binary(cmp, "==", a("c")), 0), "(a == b) == c");
        let cast = a("x").cast("i32");
        assert_eq!(
            expr(&Expr::binary(cast.clone(), "<", a("y")), 0),
            "(x as i32) < y"
        );
        assert_eq!(
            expr(&Expr::binary(cast.clone(), "==", a("y")), 0),
            "x as i32 == y"
        );
        let tail_cast = Expr::binary(a("a"), "+", cast.clone());
        assert_eq!(
            expr(&Expr::binary(tail_cast, "<<", a("2")), 0),
            "(a + x as i32) << 2"
        );
        assert_eq!(expr(&Expr::unary("-", cast), 0), "-(x as i32)");
        assert_eq!(
            expr(&Expr::unary("-", Expr::unary("-", a("x"))), 0),
            "-(-x)"
        );
        assert_eq!(expr(&Expr::unary("-", a("-1")), 0), "-(-1)");
        assert_eq!(expr(&sum.method("abs", vec![]), 0), "(a + b).abs()");
        assert_eq!(expr(&Expr::unary("!", a("x")).cast("u8"), 0), "!x as u8");
    }

    #[test]
    fn lists_go_vertical_only_for_comments_and_multiline_items() {
        let call = Expr::Method(
            Box::new(a("seqg_env")),
            "pv_put_tmo".into(),
            vec![
                Arg::commented(Expr::call("seqg_rt::ChId", vec![a("6")]), "seqState"),
                a("seqg_rt::CompType::Sync").into(),
            ],
        );
        assert_eq!(
            expr(&call, 4),
            "seqg_env.pv_put_tmo(\n        seqg_rt::ChId(6), // seqState\n        \
             seqg_rt::CompType::Sync,\n    )"
        );
        let mac = Expr::Macro(
            "seqg_escaped".into(),
            vec![
                a("seqg_env").into(),
                Expr::Block(vec![]).into(),
                a("r#\"}\"#").into(),
            ],
        );
        assert_eq!(expr(&mac, 0), "seqg_escaped!(seqg_env, {}, r#\"}\"#)");
        let mac = Expr::Macro(
            "m".into(),
            vec![Expr::Block(vec![Stmt::Expr(a("x"))]).into()],
        );
        assert_eq!(expr(&mac, 0), "m!(\n    {\n        x;\n    }\n)");
    }

    #[test]
    fn c_strings_decode_escapes() {
        assert_eq!(Expr::c_str(r#"a\"b\n\101\x41"#), a(r#""a\"b\nAA""#));
        assert_eq!(
            Expr::c_str(r"\377"),
            Expr::unary(
                "&",
                Expr::Index(Box::new(a(r#"b"\xff""#)), Box::new(a("..")))
            )
        );
        assert_eq!(expr(&Expr::c_str(r"\377"), 0), r#"&b"\xff"[..]"#);
    }
}
