use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{CaError, CaResult};

use super::DbRecordDef;
use super::parse_db;
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
    let content = expand_includes(path, macros, config)?;
    parse_db(&content, macros)
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

    let mut output = String::with_capacity(content.len());
    for line in content.lines() {
        if let Some(subst_str) = parse_substitute_directive(line) {
            // Apply substitute overrides to local macros
            for pair in subst_str.split(',') {
                if let Some((k, v)) = pair.split_once('=') {
                    let expanded_v = substitute_macros(v.trim(), &local_macros);
                    local_macros.insert(k.trim().to_string(), expanded_v);
                }
            }
        } else if let Some(filename) = parse_include_directive(line) {
            let expanded_filename = substitute_macros(&filename, &local_macros);
            let include_path =
                resolve_include_path(&expanded_filename, parent_dir, &config.include_paths)?;
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
