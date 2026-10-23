//! The `-E` writer, after GCC 13's `c-ppoutput.cc`: tokens with the
//! spacing it inserts, line markers, and the blank lines it keeps instead
//! of a marker for a gap under eight lines.

use super::lex::{Kind, Loc, P, Tok, avoid_paste};

pub(super) struct Printer {
    pub out: Vec<u8>,
    src_line: u32,
    src_name: u32,
    printed: bool,
    prev: Option<Tok>,
    /// `print.source`: the padding that decides the space before the
    /// next token; `Some(prev_white)`, or `None` for a null source.
    source: Option<bool>,
    avoid_paste: bool,
}

impl Printer {
    pub(super) fn new() -> Self {
        Printer {
            out: Vec::new(),
            src_line: 1,
            src_name: u32::MAX,
            printed: false,
            prev: None,
            source: None,
            avoid_paste: false,
        }
    }

    /// `token_streamer::stream`.
    pub(super) fn stream(&mut self, tok: &Tok, names: &[Vec<u8>]) {
        match tok.kind {
            Kind::Padding { src } => {
                self.avoid_paste = true;
                if self.source.is_none() || (self.source == Some(false) && src.is_none()) {
                    self.source = src;
                }
                return;
            }
            Kind::Eof => return,
            _ => {}
        }
        if self.avoid_paste {
            if self.source.is_none() {
                self.source = Some(tok.prev_white());
            }
            if tok.loc.line != self.src_line {
                self.line_change(tok.loc, names);
                self.out.push(b' ');
                self.printed = true;
            } else if self.source == Some(true)
                || self.prev.as_ref().is_some_and(|p| avoid_paste(p, tok))
                || (self.prev.is_none() && tok.is(P::Hash))
            {
                self.out.push(b' ');
                self.printed = true;
            }
        } else if tok.prev_white() {
            if tok.loc.line != self.src_line {
                self.line_change(tok.loc, names);
                self.printed = true;
            }
            self.out.push(b' ');
        }
        self.avoid_paste = false;
        self.source = None;
        self.out.extend_from_slice(&tok.text);
        self.printed = true;
        self.prev = Some(tok.clone());
    }

    /// `cb_line_change` / `do_line_change`: a new logical line starts at
    /// `loc`; indent to its column, less the space `stream` adds.
    pub(super) fn line_change(&mut self, loc: Loc, names: &[Vec<u8>]) {
        self.maybe_print_line(loc, names);
        self.prev = None;
        self.source = None;
        self.printed = true;
        for _ in 2..loc.col {
            self.out.push(b' ');
        }
    }

    /// Line arithmetic wraps as gcc's unsigned `linenum_type` does past
    /// `#line 4294967295`.
    pub(super) fn maybe_print_line(&mut self, loc: Loc, names: &[Vec<u8>]) {
        if self.printed {
            self.out.push(b'\n');
            self.src_line = self.src_line.wrapping_add(1);
            self.printed = false;
        }
        if loc.name == self.src_name
            && loc.line >= self.src_line
            && loc.line < self.src_line.wrapping_add(8)
        {
            while loc.line > self.src_line {
                self.out.push(b'\n');
                self.src_line = self.src_line.wrapping_add(1);
            }
        } else {
            self.print_line(loc, "", names);
        }
    }

    pub(super) fn print_line(&mut self, loc: Loc, flags: &str, names: &[Vec<u8>]) {
        if self.printed {
            self.out.push(b'\n');
        }
        self.printed = false;
        self.src_line = loc.line;
        self.src_name = loc.name;
        self.out
            .extend_from_slice(format!("# {} \"", loc.line).as_bytes());
        quote_into(&mut self.out, &names[loc.name as usize]);
        self.out.push(b'"');
        self.out.extend_from_slice(flags.as_bytes());
        self.out.push(b'\n');
    }

    pub(super) fn finish(mut self) -> Vec<u8> {
        if self.printed {
            self.out.push(b'\n');
        }
        self.out
    }
}

/// cpplib's `cpp_quote_string`: `s` escaped for the inside of a C string
/// literal.
pub(super) fn quote_into(out: &mut Vec<u8>, s: &[u8]) {
    for &c in s {
        match c {
            b'\\' | b'"' => out.extend_from_slice(&[b'\\', c]),
            b'\n' => out.extend_from_slice(b"\\n"),
            _ => out.push(c),
        }
    }
}
