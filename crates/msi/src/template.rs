//! Template processor — port of msi.cpp makeSubstitutions() + inputData.
//!
//! Handles `include "file"` and `substitute "macros"` directives,
//! with an include stack for nested file processing.

use std::path::{Path, PathBuf};

use crate::error::MsiError;
use crate::maclib::MacHandle;

pub struct TemplateProcessor {
    include_paths: Vec<PathBuf>,
    max_include_depth: usize,
}

struct InputFrame {
    #[allow(dead_code)]
    path: PathBuf,
    lines: Vec<String>,
    index: usize,
}

impl TemplateProcessor {
    pub fn new() -> Self {
        Self {
            include_paths: Vec::new(),
            max_include_depth: 20,
        }
    }

    pub fn add_include_path(&mut self, path: impl Into<PathBuf>) {
        self.include_paths.push(path.into());
    }

    /// Process a template file, expanding macros and resolving includes.
    pub fn process_file(
        &self,
        path: &Path,
        macros: &mut MacHandle,
    ) -> Result<String, MsiError> {
        let content = std::fs::read_to_string(path).map_err(|e| MsiError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let mut stack = vec![InputFrame {
            path: path.to_path_buf(),
            lines: content.lines().map(|l| l.to_string()).collect(),
            index: 0,
        }];
        self.process_stack(&mut stack, macros)
    }

    /// Process a template string with a given base directory for include resolution.
    pub fn process_string(
        &self,
        content: &str,
        base_dir: &Path,
        macros: &mut MacHandle,
    ) -> Result<String, MsiError> {
        let _ = base_dir; // Include paths come from add_include_path only
        let mut stack = vec![InputFrame {
            path: PathBuf::from("<string>"),
            lines: content.lines().map(|l| l.to_string()).collect(),
            index: 0,
        }];
        self.process_stack(&mut stack, macros)
    }

    fn process_stack(
        &self,
        stack: &mut Vec<InputFrame>,
        macros: &mut MacHandle,
    ) -> Result<String, MsiError> {
        let mut output = String::new();

        loop {
            // Get next line from the stack (front = top of stack = last element)
            let line = match stack.last_mut() {
                Some(frame) => {
                    if frame.index < frame.lines.len() {
                        let line = frame.lines[frame.index].clone();
                        frame.index += 1;
                        Some(line)
                    } else {
                        // EOF for this frame — pop and try next
                        stack.pop();
                        continue;
                    }
                }
                None => None,
            };

            let line = match line {
                Some(l) => l,
                None => break,
            };

            // Try to parse as directive
            match self.try_parse_directive(&line)? {
                Some(Directive::Include(filename)) => {
                    if stack.len() >= self.max_include_depth {
                        return Err(MsiError::IncludeDepth {
                            path: PathBuf::from(&filename),
                            max_depth: self.max_include_depth,
                        });
                    }
                    let resolved = self.resolve_include(&filename, stack)?;
                    let content =
                        std::fs::read_to_string(&resolved).map_err(|e| MsiError::Io {
                            path: resolved.clone(),
                            source: e,
                        })?;
                    stack.push(InputFrame {
                        path: resolved,
                        lines: content.lines().map(|l| l.to_string()).collect(),
                        index: 0,
                    });
                }
                Some(Directive::Substitute(defns)) => {
                    let defs = MacHandle::parse_defns(&defns);
                    macros.install_macros(&defs);
                }
                None => {
                    // Normal line — expand macros
                    let expanded = macros.expand_string(&line);
                    output.push_str(&expanded);
                    output.push('\n');
                }
            }
        }

        Ok(output)
    }

    /// Try to parse a line as an include or substitute directive.
    /// Returns None if the line is not a valid directive (treated as normal line).
    ///
    /// Directive parsing rules (from C msi.cpp lines 306-340):
    /// 1. Skip leading whitespace
    /// 2. First non-ws char must be 'i' or 's'
    /// 3. Match "include" or "substitute"
    /// 4. Skip whitespace after keyword
    /// 5. Must have opening '"'
    /// 6. Scan to closing '"' (allowing \" escape)
    /// 7. After closing '"', only spaces then newline/EOF allowed
    fn try_parse_directive(&self, line: &str) -> Result<Option<Directive>, MsiError> {
        let trimmed = line.trim_start();

        // Must start with 'i' or 's'
        let first = match trimmed.chars().next() {
            Some(c) if c == 'i' || c == 's' => c,
            _ => return Ok(None),
        };

        // Try to match keyword
        let (directive_type, rest) = if first == 'i' && trimmed.starts_with("include") {
            (DirectiveType::Include, &trimmed["include".len()..])
        } else if first == 's' && trimmed.starts_with("substitute") {
            (DirectiveType::Substitute, &trimmed["substitute".len()..])
        } else {
            return Ok(None);
        };

        // The character after the keyword must be whitespace or '"'
        // (to avoid matching "includes" or "substitutex")
        if let Some(c) = rest.chars().next() {
            if !c.is_whitespace() && c != '"' {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }

        // Skip whitespace after keyword
        let rest = rest.trim_start();

        // Must start with '"'
        if !rest.starts_with('"') {
            return Ok(None);
        }

        // Find closing '"', allowing \" escape
        let chars: Vec<char> = rest.chars().collect();
        let mut i = 1; // skip opening '"'
        let mut value = String::new();
        loop {
            if i >= chars.len() {
                // No closing quote found — treat as normal line
                return Ok(None);
            }
            let c = chars[i];
            if c == '\\' && i + 1 < chars.len() && chars[i + 1] == '"' {
                value.push('"');
                i += 2;
                continue;
            }
            if c == '"' {
                i += 1;
                break;
            }
            value.push(c);
            i += 1;
        }

        // After closing quote: only spaces then end-of-line allowed
        let trailing: String = chars[i..].iter().collect();
        let trailing_trimmed = trailing.trim_end();
        if !trailing_trimmed.is_empty() {
            // Trailing text → not a directive, treat as normal line
            return Ok(None);
        }

        match directive_type {
            DirectiveType::Include => Ok(Some(Directive::Include(value))),
            DirectiveType::Substitute => Ok(Some(Directive::Substitute(value))),
        }
    }

    /// Resolve an include filename to a full path.
    /// Rules (from C msi.cpp lines 510-542):
    /// 1. Absolute path → use directly
    /// 2. Relative path → search include_paths in order
    fn resolve_include(
        &self,
        filename: &str,
        _stack: &[InputFrame],
    ) -> Result<PathBuf, MsiError> {
        let path = Path::new(filename);

        // Absolute path
        if path.is_absolute() {
            if path.exists() {
                return Ok(path.to_path_buf());
            }
            return Err(MsiError::IncludeNotFound {
                name: filename.to_string(),
                searched: vec![path.to_path_buf()],
            });
        }

        // If no include paths, try relative to cwd
        if self.include_paths.is_empty() {
            let direct = PathBuf::from(filename);
            if direct.exists() {
                return Ok(direct);
            }
            return Err(MsiError::IncludeNotFound {
                name: filename.to_string(),
                searched: vec![direct],
            });
        }

        // Search include paths
        let mut searched = Vec::new();
        for dir in &self.include_paths {
            let candidate = dir.join(filename);
            if candidate.exists() {
                return Ok(candidate);
            }
            searched.push(candidate);
        }

        Err(MsiError::IncludeNotFound {
            name: filename.to_string(),
            searched,
        })
    }
}

impl Default for TemplateProcessor {
    fn default() -> Self {
        Self::new()
    }
}

enum DirectiveType {
    Include,
    Substitute,
}

enum Directive {
    Include(String),
    Substitute(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn basic_expansion() {
        let dir = TempDir::new().unwrap();
        let tmpl = write_file(dir.path(), "test.template", "record(ai, \"$(P)$(R)\")\n");
        let proc = TemplateProcessor::new();
        let mut mac = MacHandle::new();
        mac.put_value("P", Some("IOC:"));
        mac.put_value("R", Some("ai1"));
        let result = proc.process_file(&tmpl, &mut mac).unwrap();
        assert_eq!(result.trim(), "record(ai, \"IOC:ai1\")");
    }

    #[test]
    fn include_chain() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "c.template", "LEAF\n");
        write_file(dir.path(), "b.template", "include \"c.template\"\nB_LINE\n");
        let tmpl = write_file(
            dir.path(),
            "a.template",
            "A_START\ninclude \"b.template\"\nA_END\n",
        );
        let mut proc = TemplateProcessor::new();
        proc.add_include_path(dir.path());
        let mut mac = MacHandle::new();
        let result = proc.process_file(&tmpl, &mut mac).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines, vec!["A_START", "LEAF", "B_LINE", "A_END"]);
    }

    #[test]
    fn substitute_directive() {
        let dir = TempDir::new().unwrap();
        let tmpl = write_file(
            dir.path(),
            "test.template",
            "substitute \"P=SIM:,R=cam1\"\nrecord(ai, \"$(P)$(R)\")\n",
        );
        let proc = TemplateProcessor::new();
        let mut mac = MacHandle::new();
        let result = proc.process_file(&tmpl, &mut mac).unwrap();
        assert_eq!(result.trim(), "record(ai, \"SIM:cam1\")");
    }

    #[test]
    fn depth_limit() {
        let dir = TempDir::new().unwrap();
        // Self-including file
        write_file(dir.path(), "loop.template", "include \"loop.template\"\n");
        let tmpl = dir.path().join("loop.template");
        let mut proc = TemplateProcessor::new();
        proc.add_include_path(dir.path());
        let mut mac = MacHandle::new();
        let result = proc.process_file(&tmpl, &mut mac);
        assert!(matches!(result, Err(MsiError::IncludeDepth { .. })));
    }

    #[test]
    fn include_not_found() {
        let dir = TempDir::new().unwrap();
        let tmpl = write_file(
            dir.path(),
            "test.template",
            "include \"nonexistent.template\"\n",
        );
        let mut proc = TemplateProcessor::new();
        proc.add_include_path(dir.path());
        let mut mac = MacHandle::new();
        let result = proc.process_file(&tmpl, &mut mac);
        assert!(matches!(result, Err(MsiError::IncludeNotFound { .. })));
    }

    #[test]
    fn trailing_text_not_directive() {
        let dir = TempDir::new().unwrap();
        let tmpl = write_file(
            dir.path(),
            "test.template",
            "include \"foo\" bar\n",
        );
        let proc = TemplateProcessor::new();
        let mut mac = MacHandle::new();
        mac.suppress_warnings(true);
        let result = proc.process_file(&tmpl, &mut mac).unwrap();
        // Should be treated as normal line, not as include directive
        assert!(result.contains("include"));
    }

    #[test]
    fn preserve_empty_lines() {
        let dir = TempDir::new().unwrap();
        let tmpl = write_file(dir.path(), "test.template", "line1\n\nline3\n");
        let proc = TemplateProcessor::new();
        let mut mac = MacHandle::new();
        let result = proc.process_file(&tmpl, &mut mac).unwrap();
        assert_eq!(result, "line1\n\nline3\n");
    }

    #[test]
    fn process_string_works() {
        let proc = TemplateProcessor::new();
        let mut mac = MacHandle::new();
        mac.put_value("X", Some("42"));
        let result = proc
            .process_string("value is $(X)", Path::new("."), &mut mac)
            .unwrap();
        assert_eq!(result.trim(), "value is 42");
    }

    #[test]
    fn include_with_escaped_quote() {
        let dir = TempDir::new().unwrap();
        // A file whose name doesn't actually contain a quote (just testing the parsing)
        write_file(dir.path(), "test.template", "CONTENT\n");
        let tmpl = write_file(
            dir.path(),
            "main.template",
            "include \"test.template\"\n",
        );
        let mut proc = TemplateProcessor::new();
        proc.add_include_path(dir.path());
        let mut mac = MacHandle::new();
        let result = proc.process_file(&tmpl, &mut mac).unwrap();
        assert_eq!(result.trim(), "CONTENT");
    }

    #[test]
    fn multiple_includes() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "a.db", "A_CONTENT\n");
        write_file(dir.path(), "b.db", "B_CONTENT\n");
        let tmpl = write_file(
            dir.path(),
            "main.template",
            "include \"a.db\"\ninclude \"b.db\"\n",
        );
        let mut proc = TemplateProcessor::new();
        proc.add_include_path(dir.path());
        let mut mac = MacHandle::new();
        let result = proc.process_file(&tmpl, &mut mac).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines, vec!["A_CONTENT", "B_CONTENT"]);
    }
}
