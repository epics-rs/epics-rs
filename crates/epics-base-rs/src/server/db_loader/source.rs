//! Where a `.db` diagnostic happened, in C's own terms.
//!
//! C's `.db` reader answers this from lexer state: `pinputFileNow` walks
//! `inputFileList` (`dbLexRoutines.c:56-63`) and `my_buffer` holds the
//! macro-EXPANDED text of the line just read (`db_yyinput`, `:368-410`).
//! `yyerror` reads both (`dbYacc.y:374-381`), which is why every C loader
//! diagnostic can name the file, the line and the source text while the
//! call site that raised it names none of them.
//!
//! This port has no line-at-a-time lexer: `expand_includes` flattens the
//! whole include tree into one string before the parser sees a character,
//! so a line number in that text is NOT a line number in any file the
//! operator wrote. [`DbSource`] is what makes it one again — the flattened
//! text plus, per flattened line, the include stack that was open at it.
//! Without the map the port could still print *a* number, and printing the
//! wrong line is worse for the operator than printing none.

use std::sync::Arc;

/// One frame of C's `inputFileList` (`dbLexRoutines.c:56-63`), as
/// `dbIncludePrint` reads it (`:411-430`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbIncludeFrame {
    /// C `inputFile.path` — the search-path entry the file was found
    /// under, `None` when it was named outright.
    pub path: Option<String>,
    /// C `inputFile.filename`; `None` is C's "standard input".
    pub filename: Option<String>,
    /// C `inputFile.line_num` — 1-based, and for an OUTER frame it is
    /// the line the `include` directive sits on, because that is where
    /// that file's reader is parked.
    pub line: u32,
}

impl DbIncludeFrame {
    /// A file named outright, at a line — the only shape the `.db`
    /// loader builds, since it resolves the path before opening.
    pub fn file(filename: impl Into<String>, line: u32) -> Self {
        Self {
            path: None,
            filename: Some(filename.into()),
            line,
        }
    }
}

/// The flattened `.db` text and, per line of it, what C's lexer would
/// have been holding: the include stack innermost-FIRST, and the line's
/// own expanded text.
///
/// One `Arc` per distinct stack, shared by every line under it, so an
/// N-line file costs N pointers and one frame vector.
#[derive(Clone, Debug, Default)]
pub struct DbSource {
    /// C `my_buffer` per line, trailing newline included exactly as
    /// `fgets` leaves it — `yyerror`'s ` %d | %s\n` relies on it.
    lines: Vec<String>,
    /// The stack open at each line, innermost first.
    frames: Vec<Arc<[DbIncludeFrame]>>,
}

impl DbSource {
    /// Build from the per-line pairs the include expander emits.
    pub fn new(lines: Vec<String>, frames: Vec<Arc<[DbIncludeFrame]>>) -> Self {
        debug_assert_eq!(lines.len(), frames.len());
        Self { lines, frames }
    }

    /// A single un-included file: every line carries the same one frame,
    /// differing only in its own line number.
    pub fn single_file(filename: Option<&str>, text: &str) -> Self {
        let lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
        let frames = (1..=lines.len() as u32)
            .map(|line| {
                Arc::from(vec![DbIncludeFrame {
                    path: None,
                    filename: filename.map(str::to_string),
                    line,
                }])
            })
            .collect();
        Self { lines, frames }
    }

    /// The stack and the source text at a 1-based flattened line, or
    /// `None` when the line is not one this source has — the parser can
    /// point one past the end after a trailing newline.
    pub fn at(&self, line: u32) -> Option<(&[DbIncludeFrame], &str)> {
        let i = (line as usize).checked_sub(1)?;
        Some((self.frames.get(i)?, self.lines.get(i)?.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// C `dbIncludePrint` (`dbLexRoutines.c:411-430`), byte for byte.
///
/// The spacing is load-bearing and not obvious from the shape: the path
/// clause is written `" path \"%s\" "` with a trailing space of its own,
/// so a file found on a search path reads `in path "."  file "x.db"` with
/// TWO spaces, while one named outright reads `in file "x.db"` with one.
/// An operator's eye and any log parser match on that.
pub fn include_print(frames: &[DbIncludeFrame]) -> String {
    let mut out = String::new();
    for frame in frames {
        out.push_str(" in");
        if let Some(path) = &frame.path {
            out.push_str(&format!(" path \"{path}\" "));
        }
        match &frame.filename {
            Some(name) => out.push_str(&format!(" file \"{name}\"")),
            None => out.push_str(" standard input"),
        }
        out.push_str(&format!(" line {}\n", frame.line));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boundary: C writes the path clause with a trailing space, so the
    /// two shapes differ by one space before `file`. Measured against
    /// `softIoc` R7.0.10 on a `.db` with an unknown record type — the
    /// console reads `in path "."  file "badtype.db" line 1`.
    #[test]
    fn include_print_spacing_follows_c() {
        assert_eq!(
            include_print(&[DbIncludeFrame::file("/tmp/x.db", 7)]),
            " in file \"/tmp/x.db\" line 7\n"
        );
        assert_eq!(
            include_print(&[DbIncludeFrame {
                path: Some(".".into()),
                filename: Some("badtype.db".into()),
                line: 1,
            }]),
            " in path \".\"  file \"badtype.db\" line 1\n"
        );
        assert_eq!(
            include_print(&[DbIncludeFrame {
                path: None,
                filename: None,
                line: 3,
            }]),
            " in standard input line 3\n"
        );
    }

    /// The stack prints innermost first and every frame gets a line, so a
    /// two-deep include names both files.
    #[test]
    fn include_print_walks_every_frame() {
        assert_eq!(
            include_print(&[
                DbIncludeFrame::file("inner.db", 2),
                DbIncludeFrame::file("outer.db", 9),
            ]),
            " in file \"inner.db\" line 2\n in file \"outer.db\" line 9\n"
        );
    }

    #[test]
    fn single_file_keeps_the_newline_c_echoes() {
        let src = DbSource::single_file(Some("x.db"), "a\nb\n");
        let (frames, text) = src.at(2).expect("line 2");
        assert_eq!(text, "b\n");
        assert_eq!(frames, [DbIncludeFrame::file("x.db", 2)]);
        assert!(src.at(3).is_none());
    }
}
