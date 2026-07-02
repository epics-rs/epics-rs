use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{CaError, CaResult};

use super::DbRecordDef;
use super::substitute_macros;

/// Configuration for file-based DB loading with include support.
pub struct DbLoadConfig {
    pub include_paths: Vec<PathBuf>,
    pub max_include_depth: usize,
}

impl Default for DbLoadConfig {
    fn default() -> Self {
        Self {
            include_paths: Vec::new(),
            max_include_depth: 32,
        }
    }
}

/// File-based entry point: expand includes, substitute macros, parse records.
pub fn parse_db_file(
    path: &Path,
    macros: &HashMap<String, String>,
    config: &DbLoadConfig,
) -> CaResult<Vec<DbRecordDef>> {
    parse_db_file_with_breaktables(path, macros, config).map(|(records, _breaktables)| records)
}

/// Like [`parse_db_file`] but also returns the `breaktable(...)` definitions,
/// for the `dbLoadRecords` runtime path to merge into the database's shared
/// breakpoint-table registry.
pub fn parse_db_file_with_breaktables(
    path: &Path,
    macros: &HashMap<String, String>,
    config: &DbLoadConfig,
) -> CaResult<(Vec<DbRecordDef>, Vec<crate::server::cvt_bpt::BrkTable>)> {
    let content = expand_includes(path, macros, config)?;
    super::parse_db_with_breaktables(&content, macros)
}

/// Expand `include "..."` directives recursively.
pub fn expand_includes(
    path: &Path,
    macros: &HashMap<String, String>,
    config: &DbLoadConfig,
) -> CaResult<String> {
    let canonical = path.canonicalize().map_err(|e| CaError::DbParseError {
        line: 0,
        column: 0,
        message: format!("cannot resolve '{}': {}", path.display(), e),
    })?;
    let mut stack = Vec::new();
    expand_includes_inner(&canonical, macros, config, &mut stack)
}

fn expand_includes_inner(
    path: &Path,
    macros: &HashMap<String, String>,
    config: &DbLoadConfig,
    stack: &mut Vec<PathBuf>,
) -> CaResult<String> {
    // Circular include detection
    if stack.iter().any(|p| p == path) {
        let chain: Vec<String> = stack.iter().map(|p| p.display().to_string()).collect();
        return Err(CaError::DbParseError {
            line: 0,
            column: 0,
            message: format!(
                "circular include: {} -> {}",
                chain.join(" -> "),
                path.display()
            ),
        });
    }

    // Depth limit
    if stack.len() >= config.max_include_depth {
        return Err(CaError::DbParseError {
            line: 0,
            column: 0,
            message: format!(
                "include depth limit ({}) exceeded at '{}'",
                config.max_include_depth,
                path.display()
            ),
        });
    }

    let content = std::fs::read_to_string(path).map_err(|e| CaError::DbParseError {
        line: 0,
        column: 0,
        message: format!("cannot read '{}': {}", path.display(), e),
    })?;

    let parent_dir = path.parent().unwrap_or(Path::new("."));
    stack.push(path.to_path_buf());

    // Local macro overrides from `substitute` directives.
    // These override the caller-provided macros for subsequent includes.
    let mut local_macros = macros.clone();

    // Include search path. Starts from the caller-supplied config and
    // is mutated by `path`/`addpath` directives encountered in the
    // file (L-3): C `dbPathCmd` replaces the path, `dbAddPathCmd`
    // appends to it (`dbLexRoutines.c:433-441`).
    let mut local_paths: Vec<PathBuf> = config.include_paths.clone();

    let mut output = String::with_capacity(content.len());
    for line in content.lines() {
        if let Some(subst_str) = parse_substitute_directive(line) {
            // Apply substitute overrides to local macros. Quote- and
            // escape-aware splitting matches C `macParseDefns`
            // (macUtil.c): a `,` or `=` inside `'...'`/`"..."` or after
            // a `\` does not act as a separator.
            for (k, v) in parse_macro_defns(&subst_str) {
                let expanded_v = substitute_macros(&v, &local_macros);
                local_macros.insert(k, expanded_v);
            }
        } else if let Some(dirs) = parse_path_directive(line, "path") {
            // `path "a:b:c"` — replace the search path. C separates
            // entries with the OS path separator.
            let expanded = substitute_macros(&dirs, &local_macros);
            local_paths = split_path_list(&expanded);
        } else if let Some(dirs) = parse_path_directive(line, "addpath") {
            // `addpath "a:b"` — append to the search path.
            let expanded = substitute_macros(&dirs, &local_macros);
            local_paths.extend(split_path_list(&expanded));
        } else if let Some(filename) = parse_include_directive(line) {
            let expanded_filename = substitute_macros(&filename, &local_macros);
            let include_path = resolve_include_path(&expanded_filename, parent_dir, &local_paths)?;
            let canonical = include_path
                .canonicalize()
                .map_err(|e| CaError::DbParseError {
                    line: 0,
                    column: 0,
                    message: format!("cannot resolve '{}': {}", include_path.display(), e),
                })?;
            let included = expand_includes_inner(&canonical, &local_macros, config, stack)?;
            output.push_str(&included);
            output.push('\n');
        } else {
            // Apply current macros (including substitute overrides) to content lines
            let expanded_line = substitute_macros(line, &local_macros);
            output.push_str(&expanded_line);
            output.push('\n');
        }
    }

    stack.pop();
    Ok(output)
}

/// Parse an include directive line. Returns the filename if the line is an include directive.
pub(crate) fn parse_include_directive(line: &str) -> Option<String> {
    let trimmed = line.trim();
    // Comment lines are not include directives
    if trimmed.starts_with('#') {
        return None;
    }
    if !trimmed.starts_with("include") {
        return None;
    }
    // Must have whitespace or quote after "include"
    let rest = &trimmed["include".len()..];
    if rest.is_empty() {
        return None;
    }
    // SAFETY: `rest` is non-empty (checked by `is_empty()` above)
    let first = rest.chars().next().unwrap();
    if !first.is_whitespace() && first != '"' {
        return None;
    }
    // Extract quoted filename
    let quote_start = rest.find('"')?;
    let after_quote = &rest[quote_start + 1..];
    let quote_end = after_quote.find('"')?;
    Some(after_quote[..quote_end].to_string())
}

/// Parse a `substitute` directive line.
///
/// EPICS DB files use `substitute "NAME=VALUE"` to override macros for subsequent
/// `include` directives. Returns the quoted content if the line is a substitute directive.
pub(crate) fn parse_substitute_directive(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return None;
    }
    if !trimmed.starts_with("substitute") {
        return None;
    }
    let rest = &trimmed["substitute".len()..];
    if rest.is_empty() {
        return None;
    }
    // SAFETY: `rest` is non-empty (checked by `is_empty()` above)
    let first = rest.chars().next().unwrap();
    if !first.is_whitespace() && first != '"' {
        return None;
    }
    // Extract quoted content
    let quote_start = rest.find('"')?;
    let after_quote = &rest[quote_start + 1..];
    let quote_end = after_quote.find('"')?;
    Some(after_quote[..quote_end].to_string())
}

/// Parse a `path "..."` / `addpath "..."` directive line. `keyword`
/// is `"path"` or `"addpath"`. Returns the quoted directory list if
/// the line is that directive.
///
/// Mirrors C dbStatic (`dbYacc.y:71-81`): `tokenPATH`/`tokenADDPATH`
/// followed by a quoted string.
pub(crate) fn parse_path_directive(line: &str, keyword: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return None;
    }
    let rest = trimmed.strip_prefix(keyword)?;
    // The keyword must be followed by whitespace or a quote, so
    // `pathological` is not mistaken for `path`.
    let first = rest.chars().next()?;
    if !first.is_whitespace() && first != '"' {
        return None;
    }
    let quote_start = rest.find('"')?;
    let after_quote = &rest[quote_start + 1..];
    let quote_end = after_quote.find('"')?;
    Some(after_quote[..quote_end].to_string())
}

/// Split an OS path list (`a:b:c` on Unix, `a;b;c` on Windows) into
/// individual directory paths.
fn split_path_list(list: &str) -> Vec<PathBuf> {
    std::env::split_paths(list)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// Parse a `name=value,name2=value2,...` macro-definition string into
/// `(name, value)` pairs, quote- and escape-aware.
///
/// Mirrors C `macParseDefns` (`macUtil.c`): a `,` or `=` is only a
/// separator when it is outside `'...'`/`"..."` and not immediately
/// preceded by a backslash. Whitespace around names/values is
/// trimmed; surrounding quotes are NOT stripped (macLib keeps them so
/// the value can carry literal separators). Definitions with no `=`
/// are dropped.
pub(crate) fn parse_macro_defns(defns: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = defns.chars().collect();
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut name = String::new();
    let mut value = String::new();
    let mut quote: Option<char> = None;
    // false = collecting name, true = collecting value
    let mut in_value = false;
    let mut has_eq = false;
    let mut i = 0;

    let flush = |name: &mut String,
                 value: &mut String,
                 has_eq: &mut bool,
                 pairs: &mut Vec<(String, String)>| {
        let k = name.trim();
        if *has_eq && !k.is_empty() {
            pairs.push((k.to_string(), value.trim().to_string()));
        }
        name.clear();
        value.clear();
        *has_eq = false;
    };

    while i < chars.len() {
        let c = chars[i];
        // Escape: backslash + next char are a literal 2-char unit.
        if c == '\\' && i + 1 < chars.len() {
            if in_value {
                value.push(c);
                value.push(chars[i + 1]);
            } else {
                name.push(c);
                name.push(chars[i + 1]);
            }
            i += 2;
            continue;
        }
        // Quote state.
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            if in_value {
                value.push(c);
            } else {
                name.push(c);
            }
            i += 1;
            continue;
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            if in_value {
                value.push(c);
            } else {
                name.push(c);
            }
            i += 1;
            continue;
        }
        // Unquoted separators.
        match c {
            '=' if !in_value => {
                in_value = true;
                has_eq = true;
            }
            ',' => {
                flush(&mut name, &mut value, &mut has_eq, &mut pairs);
                in_value = false;
            }
            _ if in_value => value.push(c),
            _ => name.push(c),
        }
        i += 1;
    }
    flush(&mut name, &mut value, &mut has_eq, &mut pairs);
    pairs
}

/// Resolve an include filename to a path.
/// Search order: current file's directory → config.include_paths.
pub(crate) fn resolve_include_path(
    filename: &str,
    current_dir: &Path,
    include_paths: &[PathBuf],
) -> CaResult<PathBuf> {
    let file_path = Path::new(filename);

    // Absolute path: use directly
    if file_path.is_absolute() {
        if file_path.exists() {
            return Ok(file_path.to_path_buf());
        }
        return Err(CaError::DbParseError {
            line: 0,
            column: 0,
            message: format!("include file not found: '{filename}'"),
        });
    }

    // Relative: search current dir first
    let candidate = current_dir.join(file_path);
    if candidate.exists() {
        return Ok(candidate);
    }

    // Then search include paths
    for dir in include_paths {
        let candidate = dir.join(file_path);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(CaError::DbParseError {
        line: 0,
        column: 0,
        message: format!(
            "include file not found: '{filename}' (searched: {}, {})",
            current_dir.display(),
            include_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

/// Override DTYP fields on records that already have a DTYP field.
pub fn override_dtyp(records: &mut [DbRecordDef], dtyp: &str) {
    for rec in records.iter_mut() {
        for (name, value) in rec.fields.iter_mut() {
            if name == "DTYP" {
                *value = dtyp.to_string();
            }
        }
    }
}

#[cfg(test)]
mod macro_defns_tests {
    use super::*;

    #[test]
    fn simple_pairs() {
        assert_eq!(
            parse_macro_defns("A=1,B=2"),
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(
            parse_macro_defns(" A = 1 , B = 2 "),
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }

    /// a comma inside a quoted value does NOT split the pair.
    #[test]
    fn quoted_comma_not_split() {
        assert_eq!(
            parse_macro_defns(r#"MSG="a,b",N=1"#),
            vec![("MSG".into(), r#""a,b""#.into()), ("N".into(), "1".into())]
        );
    }

    /// an `=` inside a quoted value does not start a new value.
    #[test]
    fn quoted_equals_not_split() {
        assert_eq!(
            parse_macro_defns(r#"EXPR="x=y""#),
            vec![("EXPR".into(), r#""x=y""#.into())]
        );
    }

    /// An unquoted comma still splits (C parity for `MSG=a,b`).
    #[test]
    fn unquoted_comma_splits() {
        assert_eq!(
            parse_macro_defns("MSG=a,b"),
            // `b` has no `=` so it is dropped (not a definition).
            vec![("MSG".into(), "a".into())]
        );
    }

    /// An escaped separator is literal, not a split point.
    #[test]
    fn escaped_separator_is_literal() {
        assert_eq!(
            parse_macro_defns(r"K=a\,b"),
            vec![("K".into(), r"a\,b".into())]
        );
    }

    #[test]
    fn empty_input() {
        assert!(parse_macro_defns("").is_empty());
    }
}
