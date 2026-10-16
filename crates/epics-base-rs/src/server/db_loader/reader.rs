//! C `db_yyinput`, `dbIncludeNew` and `inputFileList`
//! (`dbLexRoutines.c:56-63`, `:368-461`): the reader the `.db` parser pulls
//! its text from, one expanded line at a time.
//!
//! The parser owns `include`, `path` and `addpath`, as C's grammar does
//! (`dbYacc.y:65-81`), and this reader only does what C's actions ask of the
//! file list. So an `include` is opened when the parser reduces it, wherever
//! the line breaks fall: the rest of its line is lexed first, then the
//! included file, then the line after it — C's order, because flex already
//! holds the rest of the line when the file is pushed.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{CaError, CaResult, JsonLexFault};
use crate::runtime::log::ERL_ERROR;

use super::include::{
    DbLoadConfig, DbOpenedFile, db_add_path, db_open_file_located, db_path, parse_macro_defns,
    parse_substitute_directive,
};
use super::json_lex::LexAborts;
use super::{DbFaults, DbIncludeFrame, DbSource, MacroDefs, MacroExpandOptions, MacroTable};

/// One entry of C's `inputFileList`.
struct InputFile {
    /// The canonicalised path, which decides whether two `include`s name
    /// one file. `None` for text handed in as a string.
    identity: Option<PathBuf>,
    /// What `dbIncludePrint` prints for this file, parked at the line its
    /// reader has reached.
    frame: DbIncludeFrame,
    content: Vec<u8>,
    cursor: usize,
}

/// The text the parser has been handed so far, and the reader that hands it
/// more on demand.
pub(super) struct Feed {
    buf: Vec<u8>,
    pub(super) faults: DbFaults,
    /// The lexer's `yyerrorAbort`s, whichever start condition raised them.
    pub(super) lexed: LexAborts,
    /// One table for the whole read, includes and all: C `dbReadCOM` has one
    /// `MAC_HANDLE` and `dbIncludeNew` pushes a file without touching it
    /// (`dbLexRoutines.c:256-300`, `:443-461`).
    table: MacroTable,
    /// `None` for a string, which has no file system behind it: an
    /// `include` there names nothing to open, and `substitute` (an msi
    /// command, not C `.db` grammar) is not recognised.
    config: Option<DbLoadConfig>,
    files: Vec<InputFile>,
    /// The include search path: one for the whole read, as C keeps it on
    /// `savedPdbbase`, set from the load's own at `dbReadCOM`
    /// (`dbLexRoutines.c:245-253`).
    paths: Vec<PathBuf>,
    /// C `yyAbort` as `db_yyinput` sees it (`dbLexRoutines.c:374`): no more
    /// lines are read.
    closed: bool,
}

impl Feed {
    fn new(macros: MacroDefs, config: Option<DbLoadConfig>, mut faults: DbFaults) -> Self {
        faults.bind_source(DbSource::default());
        let paths = config
            .as_ref()
            .map_or_else(Vec::new, |config| config.include_paths.clone());
        Self {
            buf: Vec::new(),
            faults,
            lexed: LexAborts::default(),
            table: MacroTable::new(
                macros,
                MacroExpandOptions {
                    suppress_warnings: super::db_quiet_macro_warnings(),
                    ..MacroExpandOptions::default()
                },
            ),
            config,
            files: Vec::new(),
            paths,
            closed: false,
        }
    }

    /// A string with no file behind it, read as C's "standard input".
    pub(super) fn text(input: &[u8], macros: impl Into<MacroDefs>, faults: DbFaults) -> Self {
        let mut feed = Self::new(macros.into(), None, faults);
        feed.files.push(InputFile {
            identity: None,
            frame: DbIncludeFrame {
                path: None,
                filename: None,
                line: 0,
            },
            content: input.to_vec(),
            cursor: 0,
        });
        feed
    }

    /// A `dbLoadRecords` read, with no file open yet: [`Self::open`] gives
    /// it the top-level one.
    pub(super) fn files(
        macros: impl Into<MacroDefs>,
        config: &DbLoadConfig,
        faults: DbFaults,
    ) -> Self {
        Self::new(macros.into(), Some(config.clone()), faults)
    }

    /// Push `opened` onto the file list; it is read next.
    pub(super) fn open(&mut self, opened: &DbOpenedFile) -> CaResult<()> {
        self.push_file(opened)
    }

    /// The byte at `i`, reading lines until there is one: C's lexer asking
    /// `db_yyinput` for more. `None` is end of input.
    pub(super) fn get(&mut self, i: usize) -> Option<u8> {
        while i >= self.buf.len() {
            if !self.read_line() {
                return None;
            }
        }
        Some(self.buf[i])
    }

    /// Text the parser has already read.
    pub(super) fn bytes(&self, range: std::ops::Range<usize>) -> &[u8] {
        &self.buf[range]
    }

    /// A lexer `yyerrorAbort` (`dbLex.l:129-153`) raised with the lexer on
    /// `line`, its `yytext` beginning at `start`: the text ends with the
    /// line the lexer holds, and nothing more is read.
    pub(super) fn refuse(&mut self, start: usize, line: usize, fault: JsonLexFault) {
        if self.lexed.eof.is_none() {
            let read = self.buf.len();
            let eof = self.buf[start..read]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(read, |at| start + at + 1);
            self.lexed.line = line;
            self.lexed.eof = Some(eof);
            self.buf.truncate(eof);
            self.closed = true;
        }
        self.lexed.faults.push(fault);
    }

    /// A `yyerrorAbort` has been raised: the line the lexer holds is the
    /// last.
    pub(super) fn aborted(&self) -> bool {
        self.lexed.eof.is_some()
    }

    pub(super) fn into_parts(self) -> (Vec<u8>, DbFaults, LexAborts) {
        (self.buf, self.faults, self.lexed)
    }

    /// C `db_yyinput`: the next line of the innermost open file, expanded,
    /// or of its parent once it is done.
    fn read_line(&mut self) -> bool {
        if self.closed {
            return false;
        }
        loop {
            let Some(file) = self.files.last_mut() else {
                return false;
            };
            let rest = &file.content[file.cursor..];
            if rest.is_empty() {
                self.files.pop();
                continue;
            }
            let len = rest
                .iter()
                .position(|&b| b == b'\n')
                .map_or(rest.len(), |at| at + 1);
            let raw = rest[..len].to_vec();
            file.cursor += len;
            file.frame.line += 1;
            let line_num = file.frame.line;
            let filename = file.frame.filename.clone();
            let frames: Arc<[DbIncludeFrame]> =
                self.files.iter().rev().map(|f| f.frame.clone()).collect();

            let line = raw.strip_suffix(b"\n").unwrap_or(&raw);
            let substitute = self
                .config
                .is_some()
                .then(|| parse_substitute_directive(line))
                .flatten();
            let expanded = match substitute {
                // msi is the reference for this one: C's `.db` grammar has no
                // `substitute`. `makeSubstitutions` matches it on the RAW line
                // and never expands a command line (`msi.cpp:305-358`), and
                // `addMacroReplacements` installs the RAW value
                // (`:256-275`), so a reference in it stays live.
                Some(defns) => {
                    for (name, value) in parse_macro_defns(defns) {
                        match value {
                            Some(value) => self.table.define(&name, value),
                            // `macPutValue(handle, name, NULL)`: no `=` deletes.
                            None => self.table.undefine(&name),
                        }
                    }
                    b"\n".to_vec()
                }
                None => {
                    let at = self.faults.source_len() + 1;
                    super::db_expand_line_in(
                        &mut self.table,
                        &raw,
                        filename.as_deref(),
                        line_num,
                        at,
                        &mut self.faults,
                    )
                }
            };
            self.faults
                .read_line(String::from_utf8_lossy(&expanded).into_owned(), frames);
            self.buf.extend_from_slice(&expanded);
            return true;
        }
    }

    /// C `dbIncludeNew` (`dbLexRoutines.c:443-461`) for `include` reduced
    /// on flattened `line`, its token `yytext` ending at `end`.
    pub(super) fn include(
        &mut self,
        name: &[u8],
        yytext: &str,
        end: usize,
        line: u32,
    ) -> CaResult<()> {
        if self.config.is_none() {
            return Ok(());
        }
        let name = String::from_utf8_lossy(name).into_owned();
        // C opens the file while this line is read, and the open's own
        // `macEnvExpand` prints at once.
        self.faults.read_through(line);
        let Some(included) = db_open_file_located(&name, &self.paths) else {
            // `yyerror(NULL)`: the include is skipped and the load goes on.
            self.faults.seek(line, yytext);
            self.faults
                .recoverable(format!("{ERL_ERROR}: Can't open include file '{name}'"));
            self.faults.seek_end();
            return Ok(());
        };
        self.open(&included)?;
        // `pinputFileNow` is the new file from here, at its line 0, while
        // `my_buffer` still holds this line: the rest of it is a line of its
        // own, so a diagnostic there names what C's does.
        self.buf.insert(end, b'\n');
        let frames: Arc<[DbIncludeFrame]> =
            self.files.iter().rev().map(|f| f.frame.clone()).collect();
        let text = self.faults.last_line_text();
        self.faults.read_line(text, frames);
        Ok(())
    }

    /// C `dbPathCmd` / `dbAddPathCmd` (`dbLexRoutines.c:433-441`).
    pub(super) fn path(&mut self, dirs: &[u8], add: bool) {
        // SAFETY: `dirs` is a whole token of the file's bytes. On Unix an
        // `OsStr` is any bytes; elsewhere the platform encoding is a
        // superset of UTF-8 and this text came from a file, so a non-UTF-8
        // byte cannot reach here on a platform that would mind — see
        // `db_add_path`.
        let dirs = unsafe { OsStr::from_encoded_bytes_unchecked(dirs) };
        if add {
            self.paths.extend(db_add_path(dirs));
        } else {
            self.paths = db_path(dirs);
        }
    }

    fn push_file(&mut self, opened: &DbOpenedFile) -> CaResult<()> {
        let config = self.config.as_ref().expect("files are read with a config");
        let identity = opened
            .resolved
            .canonicalize()
            .unwrap_or_else(|_| opened.resolved.clone());
        let named = &opened.named;
        if self
            .files
            .iter()
            .any(|f| f.identity.as_ref() == Some(&identity))
        {
            let chain: Vec<&str> = self
                .files
                .iter()
                .filter_map(|f| f.frame.filename.as_deref())
                .collect();
            return Err(CaError::DbParseError {
                line: 0,
                token: String::new(),
                message: format!("circular include: {} -> {named}", chain.join(" -> ")),
            });
        }
        if self.files.len() >= config.max_include_depth {
            return Err(CaError::DbParseError {
                line: 0,
                token: String::new(),
                message: format!(
                    "include depth limit ({}) exceeded at '{named}'",
                    config.max_include_depth,
                ),
            });
        }
        // Bytes, not text: C `fgets` hands its lexer whatever the file
        // holds, and `dbLoadRecords` loads a record whose name is not UTF-8.
        let content = match std::fs::read(&identity) {
            Ok(content) => content,
            // C `fopen`s a directory and its first `fgets` fails, which
            // `db_yyinput` takes for the end of an empty file.
            Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => Vec::new(),
            Err(e) => {
                return Err(CaError::DbParseError {
                    line: 0,
                    token: String::new(),
                    message: format!("cannot read '{named}': {e}"),
                });
            }
        };
        self.files.push(InputFile {
            identity: Some(identity),
            frame: DbIncludeFrame {
                path: opened.found_under.clone(),
                filename: Some(named.clone()),
                line: 0,
            },
            content,
            cursor: 0,
        });
        Ok(())
    }
}
