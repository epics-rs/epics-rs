//! Substitution file parser — port of msi.cpp tokenizer and substituteGet*() functions.
//!
//! Supports three block formats:
//! - Regular: `{ A=x, B=y }`
//! - Pattern: `pattern { A, B } { x, y }`
//! - File: `file "template.db" { pattern { A, B } { x, y } }`
//! - Global: `global { VAR=val }`

use std::path::Path;

use crate::error::MsiError;

/// A single substitution set — a template filename (optional) plus macro replacements.
#[derive(Debug, Clone)]
pub struct SubstSet {
    pub filename: Option<String>,
    pub replacements: String, // "A=x,B=y" format
}

/// Parse a substitution file into a sequence of SubstSets.
pub fn parse_subst_file(path: &Path) -> Result<Vec<SubstSet>, MsiError> {
    let content = std::fs::read_to_string(path).map_err(|e| MsiError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    parse_subst_string(&content, &path.display().to_string())
}

/// Parse a substitution string into SubstSets.
pub fn parse_subst_string(content: &str, source_name: &str) -> Result<Vec<SubstSet>, MsiError> {
    let mut tokenizer = Tokenizer::new(content, source_name);
    let mut sets = Vec::new();
    let mut global_macros = String::new();

    loop {
        tokenizer.skip_separators();
        let tok = tokenizer.peek();

        match tok {
            Token::Eof => break,
            Token::String(ref s) if s == "global" => {
                tokenizer.next(); // consume "global"
                let global = parse_global_block(&mut tokenizer)?;
                if !global_macros.is_empty() && !global.is_empty() {
                    global_macros.push(',');
                }
                global_macros.push_str(&global);
            }
            Token::String(ref s) if s == "file" => {
                tokenizer.next(); // consume "file"
                let file_sets = parse_file_block(&mut tokenizer, &global_macros)?;
                sets.extend(file_sets);
            }
            Token::String(ref s) if s == "pattern" => {
                // Bare pattern (no file block)
                let pattern_sets = parse_pattern_block(&mut tokenizer, &global_macros, None)?;
                sets.extend(pattern_sets);
            }
            Token::LBrace => {
                // Bare regular set
                let regular_sets = parse_regular_sets(&mut tokenizer, &global_macros, None)?;
                sets.extend(regular_sets);
            }
            _ => {
                return Err(MsiError::SubstParse {
                    file: source_name.to_string(),
                    line: tokenizer.line,
                    message: format!("unexpected token: {:?}", tok),
                });
            }
        }
    }

    Ok(sets)
}

/// Parse `{ VAR=val, VAR2=val2 }`
fn parse_global_block(tokenizer: &mut Tokenizer) -> Result<String, MsiError> {
    tokenizer.skip_separators();
    tokenizer.expect_lbrace()?;
    let macros = read_regular_body(tokenizer)?;
    tokenizer.expect_rbrace()?;
    Ok(macros)
}

/// Parse `"filename" { ... }` (after "file" keyword consumed)
fn parse_file_block(
    tokenizer: &mut Tokenizer,
    global_macros: &str,
) -> Result<Vec<SubstSet>, MsiError> {
    tokenizer.skip_separators();
    let filename = tokenizer.expect_string("filename")?;
    let filename = strip_outer_quotes(&filename);

    tokenizer.skip_separators();
    tokenizer.expect_lbrace()?;

    let mut sets = Vec::new();

    loop {
        tokenizer.skip_separators();
        let tok = tokenizer.peek();

        match tok {
            Token::RBrace => {
                tokenizer.next(); // consume closing '}'
                break;
            }
            Token::Eof => break,
            Token::String(ref s) if s == "pattern" => {
                let pattern_sets =
                    parse_pattern_block(tokenizer, global_macros, Some(&filename))?;
                sets.extend(pattern_sets);
            }
            Token::LBrace => {
                let regular_sets =
                    parse_regular_sets(tokenizer, global_macros, Some(&filename))?;
                sets.extend(regular_sets);
            }
            _ => {
                return Err(MsiError::SubstParse {
                    file: tokenizer.source.clone(),
                    line: tokenizer.line,
                    message: format!("unexpected token in file block: {:?}", tok),
                });
            }
        }
    }

    Ok(sets)
}

/// Parse `pattern { A, B } { x, y } { x2, y2 } ...`
fn parse_pattern_block(
    tokenizer: &mut Tokenizer,
    global_macros: &str,
    filename: Option<&str>,
) -> Result<Vec<SubstSet>, MsiError> {
    tokenizer.next(); // consume "pattern"
    tokenizer.skip_separators();

    // Read pattern names: { A, B, C }
    tokenizer.expect_lbrace()?;
    let mut names = Vec::new();
    loop {
        tokenizer.skip_separators();
        match tokenizer.peek() {
            Token::RBrace => {
                tokenizer.next();
                break;
            }
            Token::String(s) => {
                tokenizer.next();
                names.push(strip_outer_quotes(&s));
            }
            other => {
                return Err(MsiError::SubstParse {
                    file: tokenizer.source.clone(),
                    line: tokenizer.line,
                    message: format!("expected pattern name or }}, got {:?}", other),
                });
            }
        }
    }

    // Read value sets: { x, y } { x2, y2 } ...
    let mut sets = Vec::new();
    loop {
        tokenizer.skip_separators();
        match tokenizer.peek() {
            Token::LBrace => {
                tokenizer.next(); // consume '{'
                let mut values = Vec::new();
                loop {
                    tokenizer.skip_separators();
                    match tokenizer.peek() {
                        Token::RBrace => {
                            tokenizer.next();
                            break;
                        }
                        Token::String(s) => {
                            tokenizer.next();
                            values.push(strip_outer_quotes(&s));
                        }
                        other => {
                            return Err(MsiError::SubstParse {
                                file: tokenizer.source.clone(),
                                line: tokenizer.line,
                                message: format!(
                                    "expected pattern value or }}, got {:?}",
                                    other
                                ),
                            });
                        }
                    }
                }

                let mut replacements = String::new();
                if !global_macros.is_empty() {
                    replacements.push_str(global_macros);
                }
                for (i, name) in names.iter().enumerate() {
                    if !replacements.is_empty() {
                        replacements.push(',');
                    }
                    replacements.push_str(name);
                    replacements.push('=');
                    if let Some(val) = values.get(i) {
                        replacements.push_str(val);
                    }
                }

                sets.push(SubstSet {
                    filename: filename.map(|s| s.to_string()),
                    replacements,
                });
            }
            _ => break, // No more value sets
        }
    }

    Ok(sets)
}

/// Parse one or more `{ A=x, B=y }` regular sets.
fn parse_regular_sets(
    tokenizer: &mut Tokenizer,
    global_macros: &str,
    filename: Option<&str>,
) -> Result<Vec<SubstSet>, MsiError> {
    let mut sets = Vec::new();

    loop {
        tokenizer.skip_separators();
        match tokenizer.peek() {
            Token::LBrace => {
                tokenizer.next(); // consume '{'
                let body = read_regular_body(tokenizer)?;
                tokenizer.expect_rbrace()?;

                let mut replacements = String::new();
                if !global_macros.is_empty() {
                    replacements.push_str(global_macros);
                    if !body.is_empty() {
                        replacements.push(',');
                    }
                }
                replacements.push_str(&body);

                sets.push(SubstSet {
                    filename: filename.map(|s| s.to_string()),
                    replacements,
                });
            }
            _ => break,
        }
    }

    Ok(sets)
}

/// Read the body of a regular block (between { and }), returning "A=x,B=y".
/// Strings are concatenated directly; separator tokens become commas.
fn read_regular_body(tokenizer: &mut Tokenizer) -> Result<String, MsiError> {
    let mut result = String::new();
    let mut need_sep = false;

    loop {
        match tokenizer.peek() {
            Token::RBrace | Token::Eof => break,
            Token::Separator => {
                tokenizer.next();
                if !result.is_empty() {
                    need_sep = true;
                }
            }
            Token::String(s) => {
                tokenizer.next();
                if need_sep {
                    result.push(',');
                    need_sep = false;
                }
                result.push_str(&strip_outer_quotes(&s));
            }
            other => {
                return Err(MsiError::SubstParse {
                    file: tokenizer.source.clone(),
                    line: tokenizer.line,
                    message: format!("unexpected token in block body: {:?}", other),
                });
            }
        }
    }

    Ok(result)
}

/// Strip surrounding double quotes from a token value, if present.
fn strip_outer_quotes(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        // Also unescape \" inside
        inner.replace("\\\"", "\"")
    } else {
        s.to_string()
    }
}

// --- Tokenizer ---

#[derive(Debug, Clone, PartialEq)]
enum Token {
    LBrace,
    RBrace,
    Separator,
    String(String),
    Eof,
}

struct Tokenizer {
    lines: Vec<String>,
    line_idx: usize,
    col: usize,
    line: usize, // 1-based line number for error reporting
    source: String,
    peeked: Option<Token>,
}

impl Tokenizer {
    fn new(content: &str, source: &str) -> Self {
        let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        let mut t = Self {
            lines,
            line_idx: 0,
            col: 0,
            line: 1,
            source: source.to_string(),
            peeked: None,
        };
        // Prime: skip initial comment lines
        t.skip_comment_lines();
        t
    }

    fn skip_comment_lines(&mut self) {
        while self.line_idx < self.lines.len() {
            let trimmed = self.lines[self.line_idx].trim_start();
            if trimmed.starts_with('#') {
                self.line_idx += 1;
                self.line += 1;
                self.col = 0;
            } else {
                break;
            }
        }
    }

    fn current_chars(&self) -> Option<&str> {
        if self.line_idx < self.lines.len() {
            Some(&self.lines[self.line_idx])
        } else {
            None
        }
    }

    fn advance_line(&mut self) -> bool {
        self.line_idx += 1;
        self.line += 1;
        self.col = 0;
        self.skip_comment_lines();
        self.line_idx < self.lines.len()
    }

    fn peek(&mut self) -> Token {
        if let Some(ref tok) = self.peeked {
            return tok.clone();
        }
        let tok = self.read_token();
        self.peeked = Some(tok.clone());
        tok
    }

    fn next(&mut self) -> Token {
        if let Some(tok) = self.peeked.take() {
            return tok;
        }
        self.read_token()
    }

    fn skip_separators(&mut self) {
        loop {
            let tok = self.peek();
            if tok == Token::Separator {
                self.next();
            } else {
                break;
            }
        }
    }

    fn expect_lbrace(&mut self) -> Result<(), MsiError> {
        self.skip_separators();
        match self.next() {
            Token::LBrace => Ok(()),
            other => Err(MsiError::SubstParse {
                file: self.source.clone(),
                line: self.line,
                message: format!("expected '{{', got {:?}", other),
            }),
        }
    }

    fn expect_rbrace(&mut self) -> Result<(), MsiError> {
        self.skip_separators();
        match self.next() {
            Token::RBrace => Ok(()),
            other => Err(MsiError::SubstParse {
                file: self.source.clone(),
                line: self.line,
                message: format!("expected '}}', got {:?}", other),
            }),
        }
    }

    fn expect_string(&mut self, context: &str) -> Result<String, MsiError> {
        self.skip_separators();
        match self.next() {
            Token::String(s) => Ok(s),
            other => Err(MsiError::SubstParse {
                file: self.source.clone(),
                line: self.line,
                message: format!("expected {} string, got {:?}", context, other),
            }),
        }
    }

    fn read_token(&mut self) -> Token {
        let line_str = match self.current_chars() {
            Some(s) => s.to_string(),
            None => return Token::Eof,
        };

        let chars: Vec<char> = line_str.chars().collect();

        // If we're past end of line, advance
        if self.col >= chars.len() {
            if !self.advance_line() {
                return Token::Eof;
            }
            return Token::Separator; // line boundary = separator
        }

        let c = chars[self.col];

        // Comment character — treat rest of line as consumed
        if c == '#' {
            if !self.advance_line() {
                return Token::Eof;
            }
            return Token::Separator;
        }

        // Skip whitespace and commas → separator
        if c.is_whitespace() || c == ',' {
            while self.col < chars.len()
                && (chars[self.col].is_whitespace() || chars[self.col] == ',')
            {
                self.col += 1;
            }
            return Token::Separator;
        }

        // Braces
        if c == '{' {
            self.col += 1;
            return Token::LBrace;
        }
        if c == '}' {
            self.col += 1;
            return Token::RBrace;
        }

        // Quoted string
        if c == '"' {
            let mut s = String::new();
            s.push('"');
            self.col += 1;
            while self.col < chars.len() {
                let ch = chars[self.col];
                if ch == '\\' && self.col + 1 < chars.len() && chars[self.col + 1] == '"' {
                    s.push('\\');
                    s.push('"');
                    self.col += 2;
                    continue;
                }
                if ch == '"' {
                    s.push('"');
                    self.col += 1;
                    return Token::String(s);
                }
                s.push(ch);
                self.col += 1;
            }
            // Unterminated quote — return what we have
            return Token::String(s);
        }

        // Bareword
        let mut s = String::new();
        while self.col < chars.len() {
            let ch = chars[self.col];
            if ch.is_whitespace() || ch == ',' || ch == '{' || ch == '}' || ch == '"' {
                break;
            }
            s.push(ch);
            self.col += 1;
        }
        Token::String(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn regular_block() {
        let sets = parse_subst_string("{ A=x, B=y }", "test").unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].replacements, "A=x,B=y");
        assert!(sets[0].filename.is_none());
    }

    #[test]
    fn multiple_regular_blocks() {
        let sets = parse_subst_string("{ A=1 }\n{ A=2 }", "test").unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].replacements, "A=1");
        assert_eq!(sets[1].replacements, "A=2");
    }

    #[test]
    fn pattern_block() {
        let sets = parse_subst_string("pattern { A, B }\n{ x, y }\n{ a, b }", "test").unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].replacements, "A=x,B=y");
        assert_eq!(sets[1].replacements, "A=a,B=b");
    }

    #[test]
    fn file_block_regular() {
        let sets = parse_subst_string(
            "file \"test.template\" {\n  { A=1, B=2 }\n  { A=3, B=4 }\n}",
            "test",
        )
        .unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].filename.as_deref(), Some("test.template"));
        assert_eq!(sets[0].replacements, "A=1,B=2");
        assert_eq!(sets[1].replacements, "A=3,B=4");
    }

    #[test]
    fn file_block_pattern() {
        let sets = parse_subst_string(
            "file \"test.template\" {\n  pattern { P, R }\n  { \"SIM1:\", \"cam1:\" }\n  { \"SIM2:\", \"cam2:\" }\n}",
            "test",
        )
        .unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].filename.as_deref(), Some("test.template"));
        assert_eq!(sets[0].replacements, "P=SIM1:,R=cam1:");
        assert_eq!(sets[1].replacements, "P=SIM2:,R=cam2:");
    }

    #[test]
    fn global_block() {
        let sets = parse_subst_string("global { G=global_val }\n{ A=1 }", "test").unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].replacements, "G=global_val,A=1");
    }

    #[test]
    fn comment_lines() {
        let sets = parse_subst_string("# This is a comment\n{ A=1 }\n# Another comment", "test")
            .unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].replacements, "A=1");
    }

    #[test]
    fn quoted_values() {
        let sets = parse_subst_string("{ A=\"hello world\", B=\"foo\" }", "test").unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].replacements, "A=hello world,B=foo");
    }

    #[test]
    fn escaped_quotes_in_values() {
        let sets = parse_subst_string(r#"{ A="he\"llo" }"#, "test").unwrap();
        assert_eq!(sets.len(), 1);
        assert!(sets[0].replacements.contains("he\"llo"));
    }

    #[test]
    fn file_from_disk() {
        let dir = TempDir::new().unwrap();
        let path = write_file(
            dir.path(),
            "test.substitutions",
            "file \"tmpl.template\" {\n  pattern { P, R }\n  { \"IOC:\", \"ai1\" }\n}\n",
        );
        let sets = parse_subst_file(&path).unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].filename.as_deref(), Some("tmpl.template"));
        assert_eq!(sets[0].replacements, "P=IOC:,R=ai1");
    }

    #[test]
    fn multiple_globals_interleaved() {
        let sets = parse_subst_string(
            "global { X=1 }\n{ A=a }\nglobal { Y=2 }\n{ B=b }",
            "test",
        )
        .unwrap();
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].replacements, "X=1,A=a");
        assert_eq!(sets[1].replacements, "X=1,Y=2,B=b");
    }

    #[test]
    fn empty_file() {
        let sets = parse_subst_string("", "test").unwrap();
        assert!(sets.is_empty());
    }

    #[test]
    fn pattern_with_global() {
        let sets = parse_subst_string(
            "global { G=gval }\npattern { A, B }\n{ x, y }",
            "test",
        )
        .unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].replacements, "G=gval,A=x,B=y");
    }
}
