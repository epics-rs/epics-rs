//! Macro definitions and expansion, after cpplib's `macro.cc`.

use std::rc::Rc;

use super::engine::Engine;
use super::lex::{self, END_ARG, Kind, Loc, P, PASTE_LEFT, PREV_WHITE, STRINGIFY_ARG, Tok};
use super::print::quote_into;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Builtin {
    File,
    BaseFile,
    FileName,
    Line,
    IncludeLevel,
    Counter,
    /// Defined, so `#ifdef` sees it, but not expandable here.
    Unsupported,
}

pub(super) struct Macro {
    pub fun_like: bool,
    pub variadic: bool,
    pub params: Vec<Rc<[u8]>>,
    pub body: Vec<Tok>,
    pub builtin: Option<Builtin>,
    /// The defining directive's line.
    pub loc: Loc,
}

impl Macro {
    /// `compare_macros`: whether a redefinition is the same definition.
    fn same_as(&self, other: &Macro) -> bool {
        self.fun_like == other.fun_like
            && self.variadic == other.variadic
            && self.params == other.params
            && self.body.len() == other.body.len()
            && self
                .body
                .iter()
                .zip(&other.body)
                .all(|(a, b)| a.kind == b.kind && a.flags == b.flags && a.text == b.text)
    }
}

#[derive(Default)]
struct Arg {
    toks: Vec<Tok>,
    /// A variadic argument left out entirely: GNU `, ## __VA_ARGS__`
    /// then drops the comma.
    omitted: bool,
    expanded: Option<Vec<Tok>>,
    stringified: Option<Tok>,
}

const BUILTINS: [(&str, Builtin); 16] = [
    ("__FILE__", Builtin::File),
    ("__BASE_FILE__", Builtin::BaseFile),
    ("__FILE_NAME__", Builtin::FileName),
    ("__LINE__", Builtin::Line),
    ("__INCLUDE_LEVEL__", Builtin::IncludeLevel),
    ("__COUNTER__", Builtin::Counter),
    ("__DATE__", Builtin::Unsupported),
    ("__TIME__", Builtin::Unsupported),
    ("__TIMESTAMP__", Builtin::Unsupported),
    ("__has_include", Builtin::Unsupported),
    ("__has_include_next", Builtin::Unsupported),
    ("__has_attribute", Builtin::Unsupported),
    ("__has_cpp_attribute", Builtin::Unsupported),
    ("__has_c_attribute", Builtin::Unsupported),
    ("__has_builtin", Builtin::Unsupported),
    ("_Pragma", Builtin::Unsupported),
];

/// The macros `gcc -undef` still defines in gnu17 mode.
const PREDEFINED: [&str; 5] = [
    "__STDC__ 1",
    "__STDC_VERSION__ 201710L",
    "__STDC_HOSTED__ 1",
    "__STDC_UTF_16__ 1",
    "__STDC_UTF_32__ 1",
];

/// Names whose redefinition or `#undef` cpplib always warns about
/// (`NODE_WARN`): the builtins `builtin_array` marks so, and every
/// `__STDC_` macro but three.
fn always_warns(name: &[u8]) -> bool {
    match name {
        b"__LINE__" | b"__INCLUDE_LEVEL__" | b"__COUNTER__" | b"_Pragma" => true,
        _ if name.starts_with(b"__has_") => true,
        b"__STDC_FORMAT_MACROS" | b"__STDC_LIMIT_MACROS" | b"__STDC_CONSTANT_MACROS" => false,
        _ => name.starts_with(b"__STDC_"),
    }
}

impl Engine {
    pub(super) fn define_builtins(&mut self) {
        for (name, b) in BUILTINS {
            self.macros.insert(
                Rc::from(name.as_bytes()),
                Rc::new(Macro {
                    fun_like: false,
                    variadic: false,
                    params: Vec::new(),
                    body: Vec::new(),
                    builtin: Some(b),
                    loc: Loc::default(),
                }),
            );
        }
        let built_in = self.intern(b"<built-in>");
        for def in PREDEFINED {
            self.run_define(built_in, def.as_bytes());
        }
    }

    /// `run_directive` for `#define`: a `-D` or predefine, as a one-line
    /// buffer whose locations, like cpplib's, have no line.
    pub(super) fn run_define(&mut self, name: u32, text: &[u8]) {
        self.push_buffer(text, name, None, Vec::new());
        let top = self.buffers.len() - 1;
        self.buffers[top].lexer.need_line = false;
        self.buffers[top].delta = -1;
        self.in_directive = true;
        self.directive = "define";
        self.directive_loc = Loc {
            name,
            line: 0,
            col: 0,
        };
        if let Some(n) = self.lex_macro_node(true) {
            self.define(n);
        }
        self.end_directive();
        self.pop_buffer();
    }

    /// `do_define` after the name: the definition, and the redefinition
    /// warning.
    pub(super) fn define(&mut self, name: Rc<[u8]>) {
        let Some(m) = self.create_definition() else {
            return;
        };
        if let Some(old) = self.macros.get(&name).cloned()
            && (old.builtin.is_some() || always_warns(&name) || !old.same_as(&m))
        {
            let tag = if old.builtin.is_some() && !always_warns(&name) {
                " [-Wbuiltin-macro-redefined]"
            } else {
                ""
            };
            self.warning(
                self.directive_loc,
                format!("\"{}\" redefined{tag}", String::from_utf8_lossy(&name)),
            );
            if old.builtin.is_none() {
                self.note(
                    old.loc,
                    "this is the location of the previous definition".into(),
                );
            }
        }
        self.disabled.remove(&name);
        self.macros.insert(name, Rc::new(m));
    }

    pub(super) fn undef(&mut self, name: Rc<[u8]>) {
        if let Some(old) = self.macros.remove(&name) {
            let name_text = String::from_utf8_lossy(&name);
            if always_warns(&name) {
                self.warning(self.last_loc, format!("undefining \"{name_text}\""));
            } else if old.builtin.is_some() {
                self.warning(
                    self.directive_loc,
                    format!("undefining \"{name_text}\" [-Wbuiltin-macro-redefined]"),
                );
            }
            self.disabled.remove(&name);
        }
    }

    /// `create_iso_definition`.
    fn create_definition(&mut self) -> Option<Macro> {
        let mut m = Macro {
            fun_like: false,
            variadic: false,
            params: Vec::new(),
            body: Vec::new(),
            builtin: None,
            loc: self.directive_loc,
        };
        let first = self.lex_body_token();
        let mut pending = None;
        if first.prev_white() {
            pending = Some(first);
        } else if first.is(P::OpenParen) {
            let params = self.parse_params(&mut m.variadic);
            self.va_args_ok = m.variadic
                && params
                    .as_ref()
                    .is_some_and(|p| p.last().is_some_and(|n| &**n == b"__VA_ARGS__"));
            m.params = params?;
            m.fun_like = true;
        } else {
            if first.kind != Kind::Eof {
                self.warning(
                    self.last_loc,
                    "ISO C99 requires whitespace after the macro name".into(),
                );
            }
            pending = Some(first);
        }
        let ok = self.definition_body(&mut m, pending);
        self.va_args_ok = false;
        ok.then_some(m)
    }

    fn definition_body(&mut self, m: &mut Macro, mut pending: Option<Tok>) -> bool {
        let paste_err = "'##' cannot appear at either end of a macro expansion";
        let mut following_paste = false;
        loop {
            let mut tok = match pending.take() {
                Some(t) => t,
                None => self.lex_body_token(),
            };
            if tok.kind == Kind::Name
                && let Some(i) = m.params.iter().position(|p| *p == tok.text)
            {
                tok.kind = Kind::Param(i as u16);
            }
            if m.fun_like && m.body.last().is_some_and(|t| t.is(P::Hash)) {
                if let Kind::Param(_) = tok.kind {
                    let hash = m.body.pop().expect("#");
                    tok.flags =
                        (tok.flags & !PREV_WHITE) | STRINGIFY_ARG | (hash.flags & PREV_WHITE);
                } else {
                    self.error(
                        self.last_loc,
                        "'#' is not followed by a macro parameter".into(),
                    );
                    return false;
                }
            }
            if tok.kind == Kind::Eof {
                if following_paste {
                    self.error(self.last_loc, paste_err.into());
                    return false;
                }
                break;
            }
            if tok.is(P::Paste) {
                let Some(last) = m.body.last_mut() else {
                    self.error(self.last_loc, paste_err.into());
                    return false;
                };
                // A second `##` in a row is kept by cpplib only for
                // `-dD`; it does not take part in expansion.
                last.flags |= PASTE_LEFT;
                following_paste = true;
                continue;
            }
            following_paste = false;
            m.body.push(tok);
        }
        if let Some(t) = m.body.first_mut() {
            t.flags &= !PREV_WHITE;
        }
        true
    }

    /// A token of a definition's first token or body. cpplib lexes these
    /// into the macro's own storage, so its diagnostics, which point at
    /// the last token lexed, keep pointing at the name or the `)`.
    fn lex_body_token(&mut self) -> Tok {
        let last = self.last_loc;
        let t = self.lex_direct();
        self.last_loc = last;
        t
    }

    /// `parse_params`, after the `(`.
    fn parse_params(&mut self, variadic: &mut bool) -> Option<Vec<Rc<[u8]>>> {
        let mut params: Vec<Rc<[u8]>> = Vec::new();
        let mut prev_ident = false;
        loop {
            let tok = self.lex_token();
            match tok.kind {
                Kind::Name if !prev_ident && !*variadic => {
                    if params.contains(&tok.text) {
                        self.error(
                            tok.loc,
                            format!("duplicate macro parameter \"{}\"", tok.text_lossy()),
                        );
                        return None;
                    }
                    params.push(tok.text.clone());
                    prev_ident = true;
                }
                Kind::Punct(P::CloseParen) if prev_ident || params.is_empty() || *variadic => {
                    return Some(params);
                }
                Kind::Punct(P::Comma) | Kind::Punct(P::CloseParen) if prev_ident && !*variadic => {
                    prev_ident = false;
                }
                Kind::Punct(P::Ellipsis) if !*variadic => {
                    if !prev_ident {
                        params.push(Rc::from(&b"__VA_ARGS__"[..]));
                    }
                    *variadic = true;
                }
                _ => {
                    let msg = if *variadic {
                        "expected ')' after \"...\"".to_string()
                    } else if tok.kind == Kind::Eof {
                        if prev_ident {
                            "expected ')' before end of line".into()
                        } else {
                            "expected parameter name before end of line".into()
                        }
                    } else if prev_ident {
                        format!("expected ',' or ')', found \"{}\"", tok.text_lossy())
                    } else {
                        format!("expected parameter name, found \"{}\"", tok.text_lossy())
                    };
                    self.error(tok.loc, msg);
                    return None;
                }
            }
        }
    }

    /// `enter_macro_context`: push the expansion of `name`, or return
    /// false when a function-like macro is not invoked.
    pub(super) fn enter_macro_context(&mut self, m: &Rc<Macro>, name: &Tok) -> bool {
        self.mi_valid = false;
        self.angled_headers = false;
        if let Some(b) = m.builtin {
            return self.builtin_macro(b, name);
        }
        if m.fun_like {
            self.prevent_expansion += 1;
            self.parsing_args = 1;
            let args = self.funlike_invocation_p(m, name);
            self.parsing_args = 0;
            self.prevent_expansion -= 1;
            let Some(args) = args else {
                return false;
            };
            if !m.params.is_empty() {
                self.replace_args(m, name, args);
            }
        }
        self.disabled.insert(name.text.clone());
        if m.params.is_empty() {
            let toks = m
                .body
                .iter()
                .map(|t| Tok {
                    loc: name.loc,
                    ..t.clone()
                })
                .collect();
            self.push_context(toks, Some(name.text.clone()));
        }
        true
    }

    fn funlike_invocation_p(&mut self, m: &Macro, name: &Tok) -> Option<Vec<Arg>> {
        let mut padding: Option<Option<bool>> = None;
        let tok = loop {
            let t = self.get_token();
            let Kind::Padding { src } = t.kind else {
                break t;
            };
            let replace = match padding {
                None => true,
                Some(p) => p.is_none() || (p == Some(false) && src.is_none()),
            };
            if replace {
                padding = Some(src);
            }
        };
        if tok.is(P::OpenParen) {
            self.parsing_args = 2;
            return self.collect_args(m, name);
        }
        if tok.kind != Kind::Eof || tok.flags & END_ARG != 0 {
            self.backup(tok);
            if let Some(src) = padding {
                self.push_context(vec![Tok::padding(src)], None);
            }
        }
        None
    }

    fn collect_args(&mut self, m: &Macro, name: &Tok) -> Option<Vec<Arg>> {
        let paramc = m.params.len();
        let slots = paramc.max(1);
        let mut args: Vec<Arg> = (0..slots).map(|_| Arg::default()).collect();
        let mut argc = 0;
        let end = loop {
            argc += 1;
            let mut toks: Vec<Tok> = Vec::new();
            let mut depth = 0u32;
            let end = loop {
                let t = self.get_token();
                match t.kind {
                    Kind::Padding { .. } if toks.is_empty() => continue,
                    Kind::Punct(P::OpenParen) => depth += 1,
                    Kind::Punct(P::CloseParen) => {
                        if depth == 0 {
                            break t;
                        }
                        depth -= 1;
                    }
                    Kind::Punct(P::Comma) if depth == 0 && !(m.variadic && argc == paramc) => {
                        break t;
                    }
                    Kind::Eof => break t,
                    _ => {}
                }
                toks.push(t);
            };
            while toks.last().is_some_and(Tok::is_padding) {
                toks.pop();
            }
            args[(argc - 1).min(slots - 1)].toks = toks;
            if !end.is(P::Comma) {
                break end;
            }
        };
        if end.kind == Kind::Eof {
            let loc = end.loc;
            if !self.contexts.is_empty() || self.in_directive {
                self.backup(end);
            }
            self.error(
                loc,
                format!(
                    "unterminated argument list invoking macro \"{}\"",
                    name.text_lossy()
                ),
            );
            return None;
        }
        if argc == 1 && paramc == 0 && args[0].toks.is_empty() {
            argc = 0;
        }
        if argc != paramc && !(argc + 1 == paramc && m.variadic) {
            let msg = if argc < paramc {
                format!(
                    "macro \"{}\" requires {paramc} arguments, but only {argc} given",
                    name.text_lossy()
                )
            } else {
                format!(
                    "macro \"{}\" passed {argc} arguments, but takes just {paramc}",
                    name.text_lossy()
                )
            };
            self.error(self.last_loc, msg);
            self.note(
                m.loc,
                format!("macro \"{}\" defined here", name.text_lossy()),
            );
            return None;
        }
        if m.variadic && (argc < paramc || (argc == 1 && args[0].toks.is_empty())) {
            args[paramc - 1].omitted = true;
        }
        Some(args)
    }

    /// `replace_args`: the body with each parameter replaced, padding
    /// around each argument so the printer spaces it as gcc does.
    fn replace_args(&mut self, m: &Macro, name: &Tok, mut args: Vec<Arg>) {
        for (i, src) in m.body.iter().enumerate() {
            let Kind::Param(n) = src.kind else {
                continue;
            };
            let arg = &mut args[n as usize];
            if src.flags & STRINGIFY_ARG != 0 {
                if arg.stringified.is_none() {
                    arg.stringified = Some(self.stringify_arg(&arg.toks, name.loc));
                }
            } else if src.flags & PASTE_LEFT != 0
                || (i > 0 && m.body[i - 1].flags & PASTE_LEFT != 0)
            {
            } else if arg.expanded.is_none() {
                arg.expanded = Some(self.expand_arg(&arg.toks));
            }
        }

        let mut out: Vec<Tok> = Vec::new();
        for (i, src) in m.body.iter().enumerate() {
            let Kind::Param(n) = src.kind else {
                out.push(Tok {
                    loc: name.loc,
                    ..src.clone()
                });
                continue;
            };
            let arg = &args[n as usize];
            let prev_paste = i > 0 && m.body[i - 1].flags & PASTE_LEFT != 0;
            let src_paste = src.flags & PASTE_LEFT != 0;
            let mut paste_flag = None;
            let from: &[Tok] = if src.flags & STRINGIFY_ARG != 0 {
                std::slice::from_ref(arg.stringified.as_ref().expect("stringified"))
            } else if src_paste {
                &arg.toks
            } else if prev_paste {
                if let Some(last) = out.len().checked_sub(1) {
                    if out[last].is(P::Comma) && m.variadic && n as usize == m.params.len() - 1 {
                        if arg.omitted {
                            out.pop();
                        } else {
                            paste_flag = Some(last);
                        }
                    } else if arg.toks.is_empty() {
                        paste_flag = Some(last);
                    }
                }
                &arg.toks
            } else {
                arg.expanded.as_deref().unwrap_or(&[])
            };
            if !self.in_directive && i > 0 && !prev_paste {
                out.push(Tok::padding(Some(src.prev_white())));
            }
            if !from.is_empty() {
                out.extend(from.iter().map(|t| Tok {
                    loc: name.loc,
                    ..t.clone()
                }));
                if src_paste {
                    paste_flag = Some(out.len() - 1);
                }
            }
            if !self.in_directive && !src_paste {
                out.push(Tok::padding(None));
            }
            if let Some(p) = paste_flag {
                if src_paste {
                    out[p].flags |= PASTE_LEFT;
                } else {
                    out[p].flags &= !PASTE_LEFT;
                }
            }
        }
        self.push_context(out, Some(name.text.clone()));
    }

    /// `expand_arg`: the argument fully macro-expanded on its own, before
    /// substitution.
    fn expand_arg(&mut self, toks: &[Tok]) -> Vec<Tok> {
        if toks.is_empty() {
            return Vec::new();
        }
        let mut ctx = toks.to_vec();
        let mut end = Tok::eof(Loc::default());
        end.flags = END_ARG;
        ctx.push(end);
        self.push_context(ctx, None);
        let mut out = Vec::new();
        loop {
            let t = self.get_token();
            if t.kind == Kind::Eof {
                break;
            }
            out.push(t);
        }
        self.pop_context();
        out
    }

    /// `stringify_arg`: the `#param` string.
    fn stringify_arg(&mut self, toks: &[Tok], loc: Loc) -> Tok {
        let mut s = vec![b'"'];
        let mut source: Option<bool> = None;
        let mut backslashes = 0u32;
        for t in toks {
            if let Kind::Padding { src } = t.kind {
                if source.is_none() || (source == Some(false) && src.is_none()) {
                    source = src;
                }
                continue;
            }
            if s.len() > 1 && source.unwrap_or(t.prev_white()) {
                s.push(b' ');
            }
            source = None;
            if matches!(t.kind, Kind::Str(_) | Kind::Char(_)) {
                quote_into(&mut s, &t.text);
            } else {
                s.extend_from_slice(&t.text);
            }
            if t.kind == Kind::Other && t.text.first() == Some(&b'\\') {
                backslashes += 1;
            } else {
                backslashes = 0;
            }
        }
        if backslashes % 2 == 1 {
            self.warning(
                self.last_loc,
                "invalid string literal, ignoring final '\\'".into(),
            );
            s.pop();
        }
        s.push(b'"');
        Tok {
            kind: Kind::Str(lex::Prefix::None),
            flags: 0,
            text: Rc::from(s),
            loc,
        }
    }

    /// `paste_all_tokens`: `lhs` has `PASTE_LEFT`; paste the tokens that
    /// follow it in the current context and push the result.
    pub(super) fn paste_all_tokens(&mut self, mut lhs: Tok) {
        let macro_name = self.contexts.last().and_then(|c| c.macro_name.clone());
        loop {
            let ctx = self.contexts.last_mut().expect("paste context");
            let rhs = ctx.toks[ctx.pos].clone();
            ctx.pos += 1;
            if rhs.is_padding() {
                if rhs.flags & PASTE_LEFT != 0 {
                    continue;
                }
                break;
            }
            match self.paste_tokens(&lhs, &rhs) {
                Some(t) => lhs = t,
                None => {
                    lhs.flags &= !PASTE_LEFT;
                    self.backup(rhs);
                    break;
                }
            }
            if rhs.flags & PASTE_LEFT == 0 {
                break;
            }
        }
        self.push_context(vec![lhs], macro_name);
    }

    fn paste_tokens(&mut self, lhs: &Tok, rhs: &Tok) -> Option<Tok> {
        let mut buf = lhs.text.to_vec();
        let lhs_len = buf.len();
        // cpplib's guard against forming a comment.
        if lhs.is(P::Div) && !rhs.is(P::Eq) {
            buf.push(b' ');
        }
        buf.extend_from_slice(&rhs.text);
        match lex::lex_one(&buf, lhs.loc) {
            Some(mut t) => {
                t.flags |= lhs.flags & PREV_WHITE;
                Some(t)
            }
            None => {
                self.error(
                    lhs.loc,
                    format!(
                        "pasting \"{}\" and \"{}\" does not give a valid preprocessing token",
                        String::from_utf8_lossy(&buf[..lhs_len]),
                        rhs.text_lossy()
                    ),
                );
                None
            }
        }
    }

    /// `builtin_macro`: one token, at the location of the name.
    fn builtin_macro(&mut self, b: Builtin, name: &Tok) -> bool {
        let quoted = |s: &[u8]| {
            let mut q = vec![b'"'];
            quote_into(&mut q, s);
            q.push(b'"');
            q
        };
        let text = match b {
            Builtin::File => quoted(&self.names[name.loc.name as usize]),
            Builtin::FileName => {
                let n = &self.names[name.loc.name as usize];
                let base = n.iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1);
                quoted(&n[base..])
            }
            Builtin::BaseFile => quoted(&self.main_path),
            Builtin::Line => name.loc.line.to_string().into_bytes(),
            Builtin::IncludeLevel => (self.buffers.len() - 1).to_string().into_bytes(),
            Builtin::Counter => {
                self.counter += 1;
                (self.counter - 1).to_string().into_bytes()
            }
            Builtin::Unsupported => {
                self.error(
                    name.loc,
                    format!("\"{}\" is not supported", name.text_lossy()),
                );
                return false;
            }
        };
        let mut tok = lex::lex_one(&text, name.loc).expect("builtin token");
        tok.flags = 0;
        self.push_context(vec![tok], None);
        true
    }
}
