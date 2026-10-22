//! cpplib's reader: the buffer stack, macro contexts, and the token loop
//! that feeds the printer.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use super::column::FileLines;
use super::lex::{self, BOL, Kind, LexDiag, LexEnv, Lexer, Loc, NO_EXPAND, P, PASTE_LEFT, Tok};
use super::macros::Macro;
use super::print::Printer;
use super::{Diagnostic, Severity};

/// One open `#if` group.
pub(super) struct IfFrame {
    pub loc: Loc,
    pub skip_elses: bool,
    pub was_skipping: bool,
    pub directive: &'static str,
    pub mi_cmacro: Option<Rc<[u8]>>,
}

/// Where an include search started. cpplib caches a file per (name as
/// written, start directory), so a header reached under two spellings is
/// two files to the include-guard optimisation, and to its line markers.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum StartDir {
    /// The main file, and absolute names.
    None,
    /// The directory of the including file, for `""`.
    Local(Vec<u8>),
    /// The head of the `-I` chain.
    Chain,
}

pub(super) struct File {
    /// The name as gcc spells it, for line markers and `__FILE__`.
    pub path: Vec<u8>,
    /// Where it is read from.
    pub disk: PathBuf,
    pub once_only: bool,
    /// The include guard, once the file has been read through.
    pub cmacro: Option<Rc<[u8]>>,
}

pub(super) struct Buffer {
    pub lexer: Lexer,
    pub name: u32,
    pub delta: i64,
    /// `None` for a command-line `-D`/`-U` buffer.
    pub file: Option<usize>,
    pub ifs: Vec<IfFrame>,
    /// Directory prefix of the file's path, searched first for `""`.
    pub dir: Vec<u8>,
}

pub(super) struct Context {
    pub toks: Vec<Tok>,
    pub pos: usize,
    pub macro_name: Option<Rc<[u8]>>,
}

pub(super) struct Engine {
    pub names: Vec<Vec<u8>>,
    name_ids: HashMap<Vec<u8>, u32>,
    pub buffers: Vec<Buffer>,
    pub contexts: Vec<Context>,
    pub lookahead: Vec<Tok>,
    pub macros: HashMap<Rc<[u8]>, Rc<Macro>>,
    pub disabled: HashSet<Rc<[u8]>>,
    pub include_dirs: Vec<Vec<u8>>,
    pub files: Vec<File>,
    pub file_ids: HashMap<(Vec<u8>, StartDir), usize>,
    /// Each file read, by the name its buffer reports, for the display
    /// columns GCC computes from the file a location names.
    sources: HashMap<u32, FileLines>,
    pub seen_once_only: bool,
    pub main_path: Vec<u8>,
    /// The main file's directory as spelled and on disk.
    pub main_dir: (Vec<u8>, Vec<u8>),
    pub in_directive: bool,
    pub directive: &'static str,
    pub directive_loc: Loc,
    pub angled_headers: bool,
    pub skipping: bool,
    pub prevent_expansion: u32,
    pub parsing_args: u8,
    pub va_args_ok: bool,
    /// cpplib's `SEEN_EOL ()`: the last token lexed ended the line.
    pub seen_eol: bool,
    pub last_loc: Loc,
    pub mi_valid: bool,
    pub mi_cmacro: Option<Rc<[u8]>>,
    pub mi_ind_cmacro: Option<Rc<[u8]>>,
    pub counter: u32,
    main_done: bool,
    pub fatal: bool,
    pub diags: Vec<Diagnostic>,
    pub printer: Printer,
}

impl Engine {
    pub(super) fn new(include_dirs: Vec<Vec<u8>>) -> Self {
        let mut e = Engine {
            names: Vec::new(),
            name_ids: HashMap::new(),
            buffers: Vec::new(),
            contexts: Vec::new(),
            lookahead: Vec::new(),
            macros: HashMap::new(),
            disabled: HashSet::new(),
            include_dirs,
            files: Vec::new(),
            file_ids: HashMap::new(),
            sources: HashMap::new(),
            seen_once_only: false,
            main_path: Vec::new(),
            main_dir: (Vec::new(), Vec::new()),
            in_directive: false,
            directive: "",
            directive_loc: Loc::default(),
            angled_headers: false,
            skipping: false,
            prevent_expansion: 0,
            parsing_args: 0,
            va_args_ok: false,
            seen_eol: false,
            last_loc: Loc::default(),
            mi_valid: false,
            mi_cmacro: None,
            mi_ind_cmacro: None,
            counter: 0,
            main_done: false,
            fatal: false,
            diags: Vec::new(),
            printer: Printer::new(),
        };
        e.define_builtins();
        e
    }

    pub(super) fn intern(&mut self, name: &[u8]) -> u32 {
        if let Some(&id) = self.name_ids.get(name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_vec());
        self.name_ids.insert(name.to_vec(), id);
        id
    }

    /// Where a spelled path is on disk: one under the main file's spelled
    /// directory is under its real one, so `../h.h` from `../p.st` never
    /// reaches the process's parent directory.
    pub(super) fn disk(&self, spelled: &[u8]) -> PathBuf {
        let (dir, real) = &self.main_dir;
        match spelled.strip_prefix(dir.as_slice()) {
            Some(rest) if !(dir.is_empty() && to_path(spelled).is_absolute()) => {
                to_path(&[real, rest].concat())
            }
            _ => to_path(spelled),
        }
    }

    fn diag(&mut self, loc: Loc, severity: Severity, message: String) {
        let file = self
            .names
            .get(loc.name as usize)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        // location_compute_display_column: the byte column for a name no
        // file was read under or a line past its end.
        let col = match self.sources.get_mut(&loc.name) {
            Some(f) if loc.line != 0 && loc.col != 0 => f.display_col(loc.line, loc.col),
            _ => loc.col,
        };
        self.diags.push(Diagnostic {
            file,
            line: loc.line,
            col,
            severity,
            message,
        });
    }

    pub(super) fn error(&mut self, loc: Loc, message: String) {
        self.diag(loc, Severity::Error, message);
    }

    pub(super) fn warning(&mut self, loc: Loc, message: String) {
        self.diag(loc, Severity::Warning, message);
    }

    pub(super) fn note(&mut self, loc: Loc, message: String) {
        self.diag(loc, Severity::Note, message);
    }

    /// An error that stops preprocessing, as cpplib's `CPP_DL_FATAL`.
    pub(super) fn fatal(&mut self, loc: Loc, message: String) {
        self.diag(loc, Severity::Fatal, message);
        self.fatal = true;
    }

    fn lex_diags(&mut self, diags: Vec<LexDiag>) {
        for d in diags {
            let buf = self.buffers.last().expect("buffer");
            let loc = Loc {
                name: buf.name,
                line: (d.line as i64 + buf.delta) as u32,
                col: d.col,
            };
            let severity = if d.error {
                Severity::Error
            } else {
                Severity::Warning
            };
            self.diag(loc, severity, d.msg);
        }
    }

    /// Push a buffer over `raw`; the caller does the line-marker
    /// bookkeeping.
    pub(super) fn push_buffer(&mut self, raw: &[u8], name: u32, file: Option<usize>, dir: Vec<u8>) {
        if file.is_some() {
            self.sources.entry(name).or_insert_with(|| {
                FileLines::new(raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw))
            });
        }
        self.buffers.push(Buffer {
            lexer: Lexer::new(lex::clean(raw)),
            name,
            delta: 0,
            file,
            ifs: Vec::new(),
            dir,
        });
    }

    /// `_cpp_pop_buffer` up to the file change: diagnose open groups and
    /// record the include guard of a file read through.
    fn end_buffer(&mut self) {
        let buf = self.buffers.last_mut().expect("buffer");
        let ifs = std::mem::take(&mut buf.ifs);
        let file = buf.file;
        for f in ifs.iter().rev() {
            self.error(f.loc, format!("unterminated #{}", f.directive));
        }
        self.skipping = false;
        if let Some(id) = file {
            if self.mi_valid && self.files[id].cmacro.is_none() {
                self.files[id].cmacro = self.mi_cmacro.clone();
            }
            self.mi_valid = false;
        }
    }

    pub(super) fn pop_buffer(&mut self) {
        self.end_buffer();
        let buf = self.buffers.pop().expect("buffer");
        if buf.file.is_some()
            && let Some(parent) = self.buffers.last()
        {
            let line = parent.lexer.last_newline_line() as i64 + parent.delta + 1;
            let loc = Loc {
                name: parent.name,
                line: line as u32,
                col: 1,
            };
            self.printer.print_line(loc, " 2", &self.names);
        }
    }

    /// The presumed location of the line the top buffer last ended.
    pub(super) fn last_line_loc(&self) -> Loc {
        let buf = self.buffers.last().expect("buffer");
        Loc {
            name: buf.name,
            line: (buf.lexer.last_newline_line() as i64 + buf.delta) as u32,
            col: 1,
        }
    }

    /// `_cpp_lex_direct`: the next token of the top buffer.
    pub(super) fn lex_direct(&mut self) -> Tok {
        loop {
            if self.fatal {
                return Tok::eof(self.last_loc);
            }
            let top = self.buffers.len() - 1;
            let mut flags = 0;
            if self.buffers[top].lexer.need_line {
                if self.in_directive {
                    return self.eol_eof();
                }
                if self.buffers[top].lexer.at_end() {
                    if self.parsing_args != 0 {
                        return self.eol_eof();
                    }
                    if top == 0 {
                        if !self.main_done {
                            self.end_buffer();
                            self.main_done = true;
                        }
                        let mut t = Tok::eof(self.last_loc);
                        t.flags = BOL;
                        return t;
                    }
                    self.pop_buffer();
                    continue;
                }
                self.buffers[top].lexer.need_line = false;
                flags = BOL;
                if self.parsing_args == 2 {
                    flags |= lex::PREV_WHITE;
                }
            }
            let buf = &mut self.buffers[top];
            let env = LexEnv {
                name: buf.name,
                delta: buf.delta,
                angled_headers: self.angled_headers,
                skipping: self.skipping,
            };
            let mut diags = Vec::new();
            let tok = buf.lexer.lex(flags, &env, &mut diags);
            self.lex_diags(diags);
            if let Some(t) = tok {
                self.last_loc = t.loc;
                if t.kind == Kind::Name && !self.skipping {
                    if &*t.text == b"__VA_ARGS__" && !self.va_args_ok {
                        self.warning(
                            t.loc,
                            "__VA_ARGS__ can only appear in the expansion of a C99 variadic macro"
                                .into(),
                        );
                    } else if &*t.text == b"__VA_OPT__" {
                        self.error(t.loc, "__VA_OPT__ is not supported".into());
                    }
                }
                return t;
            }
        }
    }

    /// The EOF that ends a directive, or an argument list at the end of
    /// a buffer: cpplib places it at the newline it stopped on.
    fn eol_eof(&mut self) -> Tok {
        let buf = self.buffers.last().expect("buffer");
        let env = LexEnv {
            name: buf.name,
            delta: buf.delta,
            angled_headers: false,
            skipping: false,
        };
        self.last_loc = buf.lexer.loc(buf.lexer.pos.saturating_sub(1), &env);
        Tok::eof(self.last_loc)
    }

    /// `_cpp_lex_token`: runs directives and tells the printer of new
    /// lines; outside a directive, drops the tokens of a skipped group.
    pub(super) fn lex_token(&mut self) -> Tok {
        loop {
            let tok = match self.lookahead.pop() {
                Some(t) => t,
                None => self.lex_direct(),
            };
            self.seen_eol = tok.kind == Kind::Eof;
            if tok.flags & BOL != 0 {
                if tok.is(P::Hash) && self.parsing_args != 1 {
                    self.handle_directive(tok.loc);
                    continue;
                }
                if !self.skipping && self.parsing_args == 0 && tok.kind != Kind::Eof {
                    self.printer.line_change(tok.loc, &self.names);
                }
            }
            if self.in_directive {
                return tok;
            }
            self.mi_valid = false;
            if !self.skipping || tok.kind == Kind::Eof {
                return tok;
            }
        }
    }

    /// `cpp_get_token`: the next token after macro expansion. A padding
    /// token marks where an expansion began or ended, for the printer's
    /// spacing.
    pub(super) fn get_token(&mut self) -> Tok {
        loop {
            let mut tok = match self.contexts.last_mut() {
                Some(ctx) if ctx.pos < ctx.toks.len() => {
                    let t = ctx.toks[ctx.pos].clone();
                    ctx.pos += 1;
                    if t.flags & PASTE_LEFT != 0 {
                        let src = t.prev_white();
                        self.paste_all_tokens(t);
                        if self.in_directive {
                            continue;
                        }
                        return Tok::padding(Some(src));
                    }
                    t
                }
                Some(_) => {
                    self.pop_context();
                    if self.in_directive {
                        continue;
                    }
                    return Tok::padding(None);
                }
                None => self.lex_token(),
            };
            if tok.kind != Kind::Name || tok.flags & NO_EXPAND != 0 {
                return tok;
            }
            let Some(m) = self.macros.get(&tok.text).cloned() else {
                return tok;
            };
            if self.disabled.contains(&tok.text) {
                tok.flags |= NO_EXPAND;
                if let Some(ctx) = self.contexts.last_mut()
                    && ctx.pos > 0
                {
                    ctx.toks[ctx.pos - 1].flags |= NO_EXPAND;
                }
                return tok;
            }
            if self.prevent_expansion > 0 {
                return tok;
            }
            if self.enter_macro_context(&m, &tok) {
                if self.in_directive {
                    continue;
                }
                return Tok::padding(Some(tok.prev_white()));
            }
            return tok;
        }
    }

    pub(super) fn get_token_no_padding(&mut self) -> Tok {
        loop {
            let t = self.get_token();
            if !t.is_padding() {
                return t;
            }
        }
    }

    pub(super) fn push_context(&mut self, toks: Vec<Tok>, macro_name: Option<Rc<[u8]>>) {
        self.contexts.push(Context {
            toks,
            pos: 0,
            macro_name,
        });
    }

    /// `_cpp_pop_context`: a macro is enabled again once the last context
    /// of its expansion is gone.
    pub(super) fn pop_context(&mut self) {
        let ctx = self.contexts.pop().expect("context");
        if let Some(name) = ctx.macro_name {
            let below = self.contexts.last().and_then(|c| c.macro_name.as_ref());
            if below != Some(&name) {
                self.disabled.remove(&name);
            }
        }
    }

    /// `_cpp_backup_tokens (pfile, 1)`.
    pub(super) fn backup(&mut self, tok: Tok) {
        match self.contexts.last_mut() {
            Some(ctx) => {
                ctx.pos -= 1;
                ctx.toks[ctx.pos] = tok;
            }
            None => {
                self.lookahead.push(tok);
                self.seen_eol = false;
            }
        }
    }

    pub(super) fn run(&mut self) {
        loop {
            let tok = self.get_token();
            self.printer.stream(&tok, &self.names);
            if tok.kind == Kind::Eof {
                break;
            }
        }
    }
}

/// `lbasename`'s directory prefix, trailing `/` kept.
pub(super) fn dir_prefix(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&c| c == b'/') {
        Some(i) => path[..=i].to_vec(),
        None => Vec::new(),
    }
}

/// `append_file_to_dir`.
pub(super) fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = dir.to_vec();
    if !p.is_empty() && p.last() != Some(&b'/') {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

pub(super) fn to_path(b: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(b))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}

pub(super) fn path_bytes(p: &std::path::Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().into_owned().into_bytes()
    }
}
