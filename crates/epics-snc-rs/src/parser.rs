//! The parser: lemon's driver (C `lemon/snl.lt`, as generated into
//! `snl.c`) over the `snl` tables, and `parse_program`
//! (C `snl.re`).

use crate::driver::{Exit, Snc};
use crate::lemon::Tables;
use crate::lexer::Scanner;
use crate::snl::{self, Cx, Minor};
use crate::types::{Ast, NodeId, Tok, Token};

/// C `yyStackEntry`.
struct Entry {
    stateno: u32,
    major: usize,
    minor: Minor,
}

/// C `yyParser`.
struct Parser {
    t: &'static Tables,
    stack: Vec<Entry>,
    /// shifts left before error messages resume; negative after accept
    errcnt: i32,
}

impl Parser {
    /// C `snlParserAlloc`.
    fn new() -> Self {
        Parser {
            t: snl::tables(),
            stack: vec![Entry {
                stateno: 0,
                major: 0,
                minor: Minor::None,
            }],
            errcnt: -1,
        }
    }

    fn top(&self) -> &Entry {
        self.stack.last().expect("parser stack bottom")
    }

    /// C `yy_find_shift_action`.
    fn find_shift_action(&self, look_ahead: usize) -> u32 {
        let stateno = self.top().stateno;
        if stateno >= self.t.yy_min_reduce() {
            return stateno;
        }
        self.t.find_action(stateno, look_ahead)
    }

    /// C `yy_shift`.
    fn shift(&mut self, mut new_state: u32, major: usize, minor: Minor) {
        if new_state > self.t.yy_max_shift() {
            new_state += self.t.yy_min_reduce() - self.t.yy_min_shiftreduce();
        }
        self.stack.push(Entry {
            stateno: new_state,
            major,
            minor,
        });
    }

    /// C `yy_reduce`: run the action of rule `ruleno` (table numbering)
    /// and take the goto that follows it.
    fn reduce(&mut self, cx: &mut Cx<'_>, ruleno: usize) -> Result<(), Exit> {
        let t = self.t;
        let base = self.stack.len() - t.rule_nrhs[ruleno];
        let rhs: Vec<Minor> = self.stack.drain(base..).map(|e| e.minor).collect();
        let value = match snl::RULES[t.rule_index[ruleno]].action {
            Some(action) => action(cx, rhs)?,
            None => Minor::None,
        };
        let goto = t.rule_lhs[ruleno];
        let mut act = t.find_action(self.top().stateno, goto);
        if act <= t.yy_max_shiftreduce() {
            if act > t.yy_max_shift() {
                act += t.yy_min_reduce() - t.yy_min_shiftreduce();
            }
            self.stack.push(Entry {
                stateno: act,
                major: goto,
                minor: value,
            });
        } else {
            assert_eq!(act, t.yy_accept_action());
            // yy_accept
            self.errcnt = -1;
        }
        Ok(())
    }

    /// C `snlParser`: feed one token.
    fn parse(&mut self, cx: &mut Cx<'_>, minor: Token) -> Result<(), Exit> {
        let t = self.t;
        let nocode = t.nsymbol + 1;
        let mut major = minor.symbol as usize;
        let mut errorhit = false;
        loop {
            let act = self.find_shift_action(major);
            if act <= t.yy_max_shiftreduce() {
                self.shift(act, major, Minor::Tok(minor.clone()));
                self.errcnt -= 1;
                major = nocode;
            } else if act <= t.yy_max_reduce() {
                self.reduce(cx, (act - t.yy_min_reduce()) as usize)?;
            } else {
                assert_eq!(act, t.yy_error_action());
                if self.errcnt < 0 {
                    syntax_error(cx.snc, &minor);
                }
                if self.top().major == t.errsym || errorhit {
                    // discard the input token
                    major = nocode;
                } else {
                    let mut act = 0;
                    while self.stack.len() > 1 && {
                        act = t.find_action(self.top().stateno, t.errsym);
                        act >= t.yy_min_reduce()
                    } {
                        self.stack.pop();
                    }
                    if self.stack.len() == 1 || major == 0 {
                        // yy_parse_failed
                        self.stack.truncate(1);
                        cx.snc.report(format_args!("parser giving up\n"));
                        return Err(Exit(1));
                    }
                    self.shift(act, t.errsym, Minor::Tok(minor.clone()));
                }
                self.errcnt = 3;
                errorhit = true;
            }
            if major == nocode || self.stack.len() == 1 {
                return Ok(());
            }
        }
    }
}

/// snl.lem's `%syntax_error`. It leaves a location prefix for the next
/// message, as C does.
fn syntax_error(snc: &mut Snc, token: &Token) {
    let text = token.str.as_deref().unwrap_or("(null)");
    snc.error_at(
        token.file.as_deref(),
        token.line,
        format_args!("syntax error near token '{text}'\n"),
    );
    snc.report_loc(token.file.as_deref(), token.line);
}

/// C `parse_program`: parse preprocessed SNL `input`, read from
/// `src_file`, into `ast`. `None` when the parse ends without accepting,
/// which can only follow reported errors (C returns an uninitialized
/// pointer there).
pub fn parse_program(
    snc: &mut Snc,
    ast: &mut Ast,
    input: &[u8],
    src_file: &str,
) -> Result<Option<NodeId>, Exit> {
    let mut scanner = Scanner::new(input, src_file);
    let mut parser = Parser::new();
    let mut cx = Cx {
        snc,
        ast,
        result: None,
    };
    loop {
        let tv = scanner.scan(cx.snc);
        let done = tv.symbol == Tok::Eoi;
        parser.parse(&mut cx, tv)?;
        if done {
            return Ok(cx.result);
        }
    }
}
