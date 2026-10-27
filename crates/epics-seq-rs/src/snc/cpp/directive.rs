//! Directives and the include search, after cpplib's `directives.cc` and
//! `files.cc`.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use epics_libcom_rs::runtime::crt_errno::strerror;

use super::engine::{
    Engine, File, IfFrame, StartDir, dir_prefix, is_absolute_path, join, path_bytes,
};
use super::expr::unescape;
use super::lex::{Kind, Loc, P, Prefix};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Define,
    Include,
    Endif,
    Ifdef,
    If,
    Else,
    Ifndef,
    Undef,
    Line,
    Elif,
    Elifdef,
    Elifndef,
    Error,
    Pragma,
    Warning,
    /// Known to cpplib, not ported: `#include_next`, `#ident`, `#import`,
    /// `#assert`, `#unassert`, `#sccs` and `# 33 "file"` line markers.
    Unsupported,
}

impl Dir {
    fn lookup(name: &[u8]) -> Option<(Dir, &'static str)> {
        Some(match name {
            b"define" => (Dir::Define, "define"),
            b"include" => (Dir::Include, "include"),
            b"endif" => (Dir::Endif, "endif"),
            b"ifdef" => (Dir::Ifdef, "ifdef"),
            b"if" => (Dir::If, "if"),
            b"else" => (Dir::Else, "else"),
            b"ifndef" => (Dir::Ifndef, "ifndef"),
            b"undef" => (Dir::Undef, "undef"),
            b"line" => (Dir::Line, "line"),
            b"elif" => (Dir::Elif, "elif"),
            b"elifdef" => (Dir::Elifdef, "elifdef"),
            b"elifndef" => (Dir::Elifndef, "elifndef"),
            b"error" => (Dir::Error, "error"),
            b"pragma" => (Dir::Pragma, "pragma"),
            b"warning" => (Dir::Warning, "warning"),
            b"include_next" => (Dir::Unsupported, "include_next"),
            b"ident" => (Dir::Unsupported, "ident"),
            b"import" => (Dir::Unsupported, "import"),
            b"assert" => (Dir::Unsupported, "assert"),
            b"unassert" => (Dir::Unsupported, "unassert"),
            b"sccs" => (Dir::Unsupported, "sccs"),
            _ => return None,
        })
    }

    /// cpplib's `COND`: run even in a skipped group.
    fn is_cond(self) -> bool {
        matches!(
            self,
            Dir::Endif
                | Dir::Ifdef
                | Dir::If
                | Dir::Else
                | Dir::Ifndef
                | Dir::Elif
                | Dir::Elifdef
                | Dir::Elifndef
        )
    }
}

impl Engine {
    /// `_cpp_handle_directive`, after the `#` at `hash`.
    pub(super) fn handle_directive(&mut self, hash: Loc) {
        let was_parsing_args = self.parsing_args != 0;
        if was_parsing_args {
            self.parsing_args = 0;
            self.prevent_expansion = 0;
        }
        self.in_directive = true;
        // cpplib's `directive_line`: the line alone.
        self.directive_loc = Loc { col: 0, ..hash };
        let dname = self.lex_token();
        let mut dir = match dname.kind {
            Kind::Name => Dir::lookup(&dname.text),
            Kind::Number => Some((Dir::Unsupported, "line marker")),
            _ => None,
        };
        if let Some((d, name)) = dir {
            self.directive = name;
            if !matches!(d, Dir::Ifdef | Dir::If | Dir::Ifndef) {
                self.mi_valid = false;
            }
            self.angled_headers = matches!(name, "include" | "include_next" | "import");
            if self.skipping && !d.is_cond() {
                dir = None;
            }
        } else if dname.kind != Kind::Eof && !self.skipping {
            self.error(
                dname.loc,
                format!("invalid preprocessing directive #{}", dname.text_lossy()),
            );
        }
        if let Some((d, name)) = dir {
            match d {
                Dir::Define => {
                    if let Some(n) = self.lex_macro_node(true) {
                        self.define(n);
                    }
                }
                Dir::Undef => {
                    if let Some(n) = self.lex_macro_node(true) {
                        self.undef(n);
                    }
                    self.check_eol(false);
                }
                Dir::Include => self.do_include(),
                Dir::Ifdef => self.do_ifdef(false),
                Dir::Ifndef => self.do_ifdef(true),
                Dir::If => self.do_if(),
                Dir::Else => self.do_else(),
                Dir::Elif | Dir::Elifdef | Dir::Elifndef => self.do_elif(d),
                Dir::Endif => self.do_endif(),
                Dir::Line => self.do_line(),
                Dir::Error => self.do_diagnostic(true),
                Dir::Warning => self.do_diagnostic(false),
                Dir::Pragma => self.do_pragma(),
                Dir::Unsupported => {
                    self.error(hash, format!("#{name} is not supported"));
                }
            }
        }
        self.end_directive();
        if was_parsing_args {
            self.parsing_args = 2;
            self.prevent_expansion = 1;
        }
    }

    pub(super) fn end_directive(&mut self) {
        self.skip_rest_of_line();
        self.in_directive = false;
        self.angled_headers = false;
    }

    fn skip_rest_of_line(&mut self) {
        while !self.contexts.is_empty() {
            self.pop_context();
        }
        if !self.seen_eol {
            while self.lex_token().kind != Kind::Eof {}
        }
    }

    /// `check_eol`.
    fn check_eol(&mut self, expand: bool) {
        if self.seen_eol {
            return;
        }
        let t = if expand {
            self.get_token()
        } else {
            self.lex_token()
        };
        if t.kind != Kind::Eof {
            let tag = match self.directive {
                "else" | "endif" => " [-Wendif-labels]",
                _ => "",
            };
            self.warning(
                t.loc,
                format!("extra tokens at end of #{} directive{tag}", self.directive),
            );
        }
    }

    pub(super) fn lex_macro_node(&mut self, is_def: bool) -> Option<Rc<[u8]>> {
        let t = self.lex_token();
        match t.kind {
            Kind::Name
                if is_def
                    && matches!(
                        &*t.text,
                        b"defined" | b"__has_include" | b"__has_include_next"
                    ) =>
            {
                self.error(
                    t.loc,
                    format!("\"{}\" cannot be used as a macro name", t.text_lossy()),
                );
                None
            }
            Kind::Name => Some(t.text),
            Kind::Eof => {
                self.error(
                    t.loc,
                    format!("no macro name given in #{} directive", self.directive),
                );
                None
            }
            _ => {
                self.error(t.loc, "macro names must be identifiers".into());
                None
            }
        }
    }

    fn push_conditional(&mut self, skip: bool, directive: &'static str, cmacro: Option<Rc<[u8]>>) {
        let frame = IfFrame {
            loc: self.directive_loc,
            skip_elses: self.skipping || !skip,
            was_skipping: self.skipping,
            directive,
            mi_cmacro: if self.mi_valid && self.mi_cmacro.is_none() {
                cmacro
            } else {
                None
            },
        };
        self.skipping = skip;
        self.buffers.last_mut().expect("buffer").ifs.push(frame);
    }

    fn do_ifdef(&mut self, ifndef: bool) {
        let mut skip = true;
        let mut cmacro = None;
        if !self.skipping
            && let Some(node) = self.lex_macro_node(false)
        {
            let defined = self.macros.contains_key(&node);
            skip = defined == ifndef;
            if ifndef {
                cmacro = Some(node);
            }
            self.check_eol(false);
        }
        self.push_conditional(skip, if ifndef { "ifndef" } else { "ifdef" }, cmacro);
    }

    fn do_if(&mut self) {
        let skip = self.skipping || !self.parse_expr(true);
        let cmacro = self.mi_ind_cmacro.clone();
        self.push_conditional(skip, "if", cmacro);
    }

    fn do_else(&mut self) {
        let loc = self.last_loc;
        let Some(f) = self.buffers.last_mut().expect("buffer").ifs.last_mut() else {
            self.error(loc, "#else without #if".into());
            return;
        };
        let after_else = f.directive == "else";
        f.directive = "else";
        self.skipping = f.skip_elses;
        f.skip_elses = true;
        f.mi_cmacro = None;
        let was_skipping = f.was_skipping;
        let began = f.loc;
        if after_else {
            self.error(loc, "#else after #else".into());
            self.error(began, "the conditional began here".into());
        }
        if !was_skipping {
            self.check_eol(false);
        }
    }

    fn do_elif(&mut self, d: Dir) {
        let loc = self.last_loc;
        let name = self.directive;
        let Some(f) = self.buffers.last_mut().expect("buffer").ifs.last_mut() else {
            self.error(loc, format!("#{name} without #if"));
            return;
        };
        let after_else = f.directive == "else";
        f.directive = "elif";
        let skip_elses = f.skip_elses;
        let began = f.loc;
        if after_else {
            self.error(loc, format!("#{name} after #else"));
            self.error(began, "the conditional began here".into());
        }
        // DR#412: once a group is taken, later controlling expressions
        // are not evaluated.
        if skip_elses {
            self.skipping = true;
        } else {
            self.skipping = match d {
                Dir::Elif => !self.parse_expr(false),
                _ => match self.lex_macro_node(false) {
                    Some(node) => self.macros.contains_key(&node) == (d == Dir::Elifndef),
                    None => true,
                },
            };
            let skipping = self.skipping;
            let f = self.buffers.last_mut().expect("buffer").ifs.last_mut();
            f.expect("frame").skip_elses = !skipping;
        }
        let f = self.buffers.last_mut().expect("buffer").ifs.last_mut();
        f.expect("frame").mi_cmacro = None;
    }

    fn do_endif(&mut self) {
        let loc = self.last_loc;
        let buf = self.buffers.last_mut().expect("buffer");
        let Some(f) = buf.ifs.pop() else {
            self.error(loc, "#endif without #if".into());
            return;
        };
        let outermost = buf.ifs.is_empty();
        if !f.was_skipping {
            self.check_eol(false);
        }
        if outermost && f.mi_cmacro.is_some() {
            self.mi_valid = true;
            self.mi_cmacro = f.mi_cmacro;
        }
        self.skipping = f.was_skipping;
    }

    fn do_include(&mut self) {
        let header = self.get_token_no_padding();
        self.angled_headers = false;
        let (fname, angled) = match header.kind {
            Kind::Str(Prefix::None) => (header.text[1..header.text.len() - 1].to_vec(), false),
            Kind::HeaderName => (header.text[1..header.text.len() - 1].to_vec(), true),
            Kind::Punct(P::Less) => (self.glue_header_name(), true),
            _ => {
                self.error(
                    header.loc,
                    format!("#{} expects \"FILENAME\" or <FILENAME>", self.directive),
                );
                return;
            }
        };
        self.check_eol(true);
        if fname.is_empty() {
            self.error(header.loc, format!("empty filename in #{}", self.directive));
            return;
        }
        if self.buffers.len() >= 200 {
            self.error(
                header.loc,
                format!(
                    "#include nested depth {} exceeds maximum of 200",
                    self.buffers.len()
                ),
            );
            return;
        }
        self.skip_rest_of_line();
        self.stack_include(&fname, angled, header.loc);
    }

    fn glue_header_name(&mut self) -> Vec<u8> {
        let mut name = Vec::new();
        loop {
            let t = self.get_token_no_padding();
            if t.is(P::Greater) {
                break;
            }
            if t.kind == Kind::Eof {
                self.error(t.loc, "missing terminating > character".into());
                break;
            }
            if t.prev_white() {
                name.push(b' ');
            }
            name.extend_from_slice(&t.text);
        }
        name
    }

    /// `_cpp_stack_include`.
    fn stack_include(&mut self, fname: &[u8], angled: bool, loc: Loc) {
        let Some(id) = self.find_file(fname, angled, loc) else {
            return;
        };
        let file = &self.files[id];
        if file.once_only
            || file
                .cmacro
                .as_ref()
                .is_some_and(|c| self.macros.contains_key(c))
        {
            return;
        }
        let data = match std::fs::read(&file.disk) {
            Ok(d) => d,
            Err(e) => {
                let msg = format!("{}: {}", String::from_utf8_lossy(&file.path), strerror(&e));
                self.fatal(loc, msg);
                return;
            }
        };
        if self.seen_once_only && self.seen_under_other_name(id, &data) {
            return;
        }
        let from = self.last_line_loc();
        let path = self.files[id].path.clone();
        let name = self.intern(&path);
        self.push_buffer(&data, name, Some(id), dir_prefix(&path));
        self.mi_valid = true;
        self.mi_cmacro = None;
        self.printer.maybe_print_line(from, &self.names);
        let to = Loc {
            name,
            line: 1,
            col: 1,
        };
        self.printer.print_line(to, " 1", &self.names);
    }

    /// `has_unique_contents`: a once-only file read under another name.
    fn seen_under_other_name(&self, id: usize, data: &[u8]) -> bool {
        let meta = |p: &Path| {
            std::fs::metadata(p)
                .ok()
                .map(|m| (m.modified().ok(), m.len()))
        };
        let mine = meta(&self.files[id].disk);
        self.files.iter().enumerate().any(|(i, f)| {
            i != id
                && f.once_only
                && meta(&f.disk) == mine
                && std::fs::read(&f.disk).is_ok_and(|d| d == data)
        })
    }

    /// `_cpp_find_file` with cpplib's cache: a file is one entry per
    /// (name as written, start directory), shared with the `-I` chain
    /// head once the search reaches it.
    fn find_file(&mut self, fname: &[u8], angled: bool, loc: Loc) -> Option<usize> {
        let start = if is_absolute_path(fname) {
            StartDir::None
        } else if angled {
            if self.include_dirs.is_empty() {
                self.error(
                    loc,
                    format!(
                        "no include path in which to search for {}",
                        String::from_utf8_lossy(fname)
                    ),
                );
                return None;
            }
            StartDir::Chain
        } else {
            StartDir::Local(self.buffers.last().expect("buffer").dir.clone())
        };
        let key = (fname.to_vec(), start.clone());
        if let Some(&id) = self.file_ids.get(&key) {
            return Some(id);
        }
        let on_disk = |e: &Engine, p: Vec<u8>| {
            let disk = e.disk(&p).filter(|d| d.is_file())?;
            Some((p, disk))
        };
        let mut found = None;
        let mut via_chain = false;
        match &start {
            StartDir::None => found = on_disk(self, fname.to_vec()),
            StartDir::Local(dir) => {
                found = on_disk(self, join(dir, fname));
                if found.is_none() && !self.include_dirs.is_empty() {
                    let chain = (fname.to_vec(), StartDir::Chain);
                    if let Some(&id) = self.file_ids.get(&chain) {
                        self.file_ids.insert(key, id);
                        return Some(id);
                    }
                    via_chain = true;
                }
            }
            StartDir::Chain => via_chain = true,
        }
        if via_chain {
            found = self
                .include_dirs
                .iter()
                .find_map(|d| on_disk(self, join(d, fname)));
        }
        let Some((path, disk)) = found else {
            self.fatal(
                loc,
                format!(
                    "{}: No such file or directory",
                    String::from_utf8_lossy(fname)
                ),
            );
            return None;
        };
        let id = self.files.len();
        self.files.push(File {
            path,
            disk,
            once_only: false,
            cmacro: None,
        });
        self.file_ids.insert(key, id);
        if via_chain {
            self.file_ids.insert((fname.to_vec(), StartDir::Chain), id);
        }
        Some(id)
    }

    /// Stack the main file, `path` as gcc is handed it and `disk` where it
    /// is: cpplib's `cpp_read_main_file`.
    pub(super) fn push_main(&mut self, path: Vec<u8>, disk: PathBuf, data: &[u8]) {
        self.main_dir = (dir_prefix(&path), dir_prefix(&path_bytes(&disk)));
        let id = self.files.len();
        self.files.push(File {
            path: path.clone(),
            disk,
            once_only: false,
            cmacro: None,
        });
        self.file_ids.insert((path.clone(), StartDir::None), id);
        let name = self.intern(&path);
        self.push_buffer(data, name, Some(id), dir_prefix(&path));
        self.main_path = path;
        self.mi_valid = true;
        self.mi_cmacro = None;
        let at = Loc {
            name,
            line: 1,
            col: 1,
        };
        self.printer.print_line(at, "", &self.names);
    }

    fn do_line(&mut self) {
        let t = self.get_token();
        let line = match t.kind {
            Kind::Number if t.text.iter().all(u8::is_ascii_digit) => {
                let mut n = 0u32;
                let mut wrapped = false;
                for &c in t.text.iter() {
                    let next = n.wrapping_mul(10).wrapping_add((c - b'0') as u32);
                    wrapped |= next / 10 != n;
                    n = next;
                }
                if wrapped {
                    self.warning(t.loc, "line number out of range".into());
                }
                n
            }
            Kind::Eof => {
                self.error(t.loc, "unexpected end of file after #line".into());
                return;
            }
            _ => {
                self.error(
                    t.loc,
                    format!(
                        "\"{}\" after #line is not a positive integer",
                        t.text_lossy()
                    ),
                );
                return;
            }
        };
        let t = self.get_token();
        let mut new_name = None;
        match t.kind {
            Kind::Str(Prefix::None) => {
                new_name = Some(unescape(&t.text[1..t.text.len() - 1]));
                self.check_eol(true);
            }
            Kind::Eof => {}
            _ => {
                self.error(t.loc, format!("invalid filename \"{}\"", t.text_lossy()));
                return;
            }
        }
        self.skip_rest_of_line();
        let name = match new_name {
            Some(n) => self.intern(&n),
            None => self.buffers.last().expect("buffer").name,
        };
        let buf = self.buffers.last_mut().expect("buffer");
        buf.name = name;
        buf.delta = line as i64 - (buf.lexer.last_newline_line() as i64 + 1);
        let at = Loc { name, line, col: 1 };
        self.printer.print_line(at, "", &self.names);
    }

    /// `#error` / `#warning`: the directive's line, unexpanded.
    fn do_diagnostic(&mut self, error: bool) {
        let loc = self.last_loc;
        let mut msg = format!("#{} ", self.directive);
        let mut first = true;
        self.prevent_expansion += 1;
        loop {
            let t = self.get_token();
            match t.kind {
                Kind::Eof => break,
                Kind::Padding { .. } => continue,
                _ => {}
            }
            if !first && t.prev_white() {
                msg.push(' ');
            }
            first = false;
            msg.push_str(&t.text_lossy());
        }
        self.prevent_expansion -= 1;
        if error {
            self.error(loc, msg);
        } else {
            self.warning(loc, msg + " [-Wcpp]");
        }
    }

    /// `#pragma once` is the one pragma kept; gcc would copy any other to
    /// its output.
    fn do_pragma(&mut self) {
        self.prevent_expansion += 1;
        let t = self.get_token_no_padding();
        self.prevent_expansion -= 1;
        if t.kind == Kind::Name && &*t.text == b"once" {
            // gcc -E starts an output line at the pragma name, as for a
            // token, and leaves it blank.
            if self.parsing_args == 0 {
                self.printer.line_change(t.loc, &self.names);
            }
            if self.buffers.len() == 1 {
                self.warning(self.last_loc, "#pragma once in main file".into());
            }
            self.check_eol(false);
            if let Some(id) = self.buffers.last().expect("buffer").file {
                self.files[id].once_only = true;
            }
            self.seen_once_only = true;
        } else {
            self.error(
                t.loc,
                "#pragma other than #pragma once is not supported".into(),
            );
        }
    }
}
