use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::{AutosaveError, AutosaveResult};
use crate::format::MAX_INCLUDE_DEPTH;
use crate::macros::MacroContext;

/// A single entry parsed from a .req file.
#[derive(Debug, Clone)]
pub struct RequestEntry {
    pub pv_name: String,
    pub source_file: PathBuf,
    pub line_no: usize,
    /// Original text before macro expansion (if different from pv_name).
    pub expanded_from: Option<String>,
}

/// Load and parse a .req file, resolving includes and macros.
pub async fn load_request_file(
    path: &Path,
    macros: &MacroContext,
) -> AutosaveResult<Vec<RequestEntry>> {
    let canonical = tokio::fs::canonicalize(path).await.map_err(|e| {
        AutosaveError::RequestFile {
            path: path.display().to_string(),
            message: format!("cannot resolve path: {e}"),
        }
    })?;
    let content = tokio::fs::read_to_string(&canonical).await.map_err(|e| {
        AutosaveError::RequestFile {
            path: path.display().to_string(),
            message: e.to_string(),
        }
    })?;
    let base_dir = canonical.parent().unwrap_or(Path::new("."));
    let mut include_stack = vec![canonical.clone()];
    parse_request_inner(&content, base_dir, macros, 0, &mut include_stack, &canonical)
}

/// Load a .req file from a string (for testing without filesystem).
pub fn parse_request_string(
    content: &str,
    macros: &MacroContext,
    source_name: &str,
) -> AutosaveResult<Vec<RequestEntry>> {
    let source = PathBuf::from(source_name);
    let mut entries = Vec::new();
    let source_str = source_name;

    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("file ") || line.starts_with("file\t") {
            // file includes not supported in string mode
            return Err(AutosaveError::RequestFile {
                path: source_name.to_string(),
                message: format!("file include not supported in parse_request_string at line {line_no}"),
            });
        }

        let expanded = macros.expand(line, source_str, line_no)?;
        let expanded_from = if expanded != line {
            Some(line.to_string())
        } else {
            None
        };
        entries.push(RequestEntry {
            pv_name: expanded,
            source_file: source.clone(),
            line_no,
            expanded_from,
        });
    }
    Ok(entries)
}

fn parse_request_inner(
    content: &str,
    base_dir: &Path,
    macros: &MacroContext,
    depth: usize,
    include_stack: &mut Vec<PathBuf>,
    source_path: &Path,
) -> AutosaveResult<Vec<RequestEntry>> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(AutosaveError::IncludeDepthExceeded(MAX_INCLUDE_DEPTH));
    }

    let mut entries = Vec::new();
    let source_str = source_path.display().to_string();

    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with("file ") || line.starts_with("file\t") {
            let rest = line[4..].trim();
            let (file_path, inline_macros) = parse_file_directive(rest);

            // Expand macros in file path
            let expanded_path = macros.expand(&file_path, &source_str, line_no)?;

            // Merge inline macros
            let child_macros = if inline_macros.is_empty() {
                macros.clone()
            } else {
                let overrides = MacroContext::parse_inline(&inline_macros);
                macros.with_overrides(&overrides)
            };

            // Resolve relative to including file's directory
            let include_path = if Path::new(&expanded_path).is_absolute() {
                PathBuf::from(&expanded_path)
            } else {
                base_dir.join(&expanded_path)
            };

            let canonical = std::fs::canonicalize(&include_path).map_err(|e| {
                AutosaveError::RequestFile {
                    path: include_path.display().to_string(),
                    message: format!("cannot resolve include: {e}"),
                }
            })?;

            // Cycle detection
            if include_stack.contains(&canonical) {
                let mut chain: Vec<String> = include_stack
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                chain.push(canonical.display().to_string());
                return Err(AutosaveError::IncludeCycle { chain });
            }

            include_stack.push(canonical.clone());
            let inc_content = std::fs::read_to_string(&canonical).map_err(|e| {
                AutosaveError::RequestFile {
                    path: canonical.display().to_string(),
                    message: e.to_string(),
                }
            })?;
            let inc_dir = canonical.parent().unwrap_or(Path::new("."));
            let sub_entries = parse_request_inner(
                &inc_content,
                inc_dir,
                &child_macros,
                depth + 1,
                include_stack,
                &canonical,
            )?;
            entries.extend(sub_entries);
            include_stack.pop();
            continue;
        }

        // Regular PV line
        let expanded = macros.expand(line, &source_str, line_no)?;
        let expanded_from = if expanded != line {
            Some(line.to_string())
        } else {
            None
        };
        entries.push(RequestEntry {
            pv_name: expanded,
            source_file: source_path.to_path_buf(),
            line_no,
            expanded_from,
        });
    }

    Ok(entries)
}

/// Parse a file directive: `file <path> [macros]`
/// Path can be quoted or unquoted.
fn parse_file_directive(rest: &str) -> (String, String) {
    let rest = rest.trim();
    if rest.starts_with('"') {
        // Quoted path
        if let Some(end_quote) = rest[1..].find('"') {
            let path = rest[1..end_quote + 1].to_string();
            let after = rest[end_quote + 2..].trim();
            let macros = after.to_string();
            (path, macros)
        } else {
            (rest.to_string(), String::new())
        }
    } else {
        // Unquoted: split at first whitespace
        let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
        let path = parts[0].to_string();
        let macros = if parts.len() > 1 {
            parts[1].trim().to_string()
        } else {
            String::new()
        };
        (path, macros)
    }
}

/// Extract just PV names from entries.
pub fn pv_names(entries: &[RequestEntry]) -> Vec<String> {
    entries.iter().map(|e| e.pv_name.clone()).collect()
}

/// Dedup entries, keeping the last occurrence of each PV name.
pub fn dedup_entries(entries: Vec<RequestEntry>) -> Vec<RequestEntry> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();

    // Iterate in reverse to keep last occurrence
    for entry in entries.into_iter().rev() {
        if seen.insert(entry.pv_name.clone()) {
            result.push(entry);
        }
    }
    result.reverse();
    result
}
