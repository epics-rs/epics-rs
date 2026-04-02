use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::error::{AutosaveError, AutosaveResult};
use super::format::MAX_INCLUDE_DEPTH;
use super::macros::MacroContext;

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
    load_request_file_with_search_paths(&path.to_string_lossy(), &[], macros).await
}

/// Load a .req file by searching multiple directories.
///
/// Search order for the top-level file:
/// 1. Absolute path (if the filename is absolute)
/// 2. Each directory in `search_paths`
/// 3. Current directory
///
/// For `file` includes within .req files, the search order is:
/// 1. Directory of the including file
/// 2. Each directory in `search_paths`
pub async fn load_request_file_with_search_paths(
    filename: &str,
    search_paths: &[PathBuf],
    macros: &MacroContext,
) -> AutosaveResult<Vec<RequestEntry>> {
    let path = PathBuf::from(filename);

    // Resolve the file location
    let resolved = if path.is_absolute() && path.exists() {
        path
    } else if path.is_absolute() {
        return Err(AutosaveError::RequestFile {
            path: filename.to_string(),
            message: "file not found".to_string(),
        });
    } else {
        resolve_in_search_paths(filename, search_paths).ok_or_else(|| {
            AutosaveError::RequestFile {
                path: filename.to_string(),
                message: format!(
                    "file not found in search paths: {}",
                    search_paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        })?
    };

    let canonical =
        tokio::fs::canonicalize(&resolved)
            .await
            .map_err(|e| AutosaveError::RequestFile {
                path: resolved.display().to_string(),
                message: format!("cannot resolve path: {e}"),
            })?;
    let content =
        tokio::fs::read_to_string(&canonical)
            .await
            .map_err(|e| AutosaveError::RequestFile {
                path: resolved.display().to_string(),
                message: e.to_string(),
            })?;
    let base_dir = canonical.parent().unwrap_or(Path::new("."));
    let mut include_stack = vec![canonical.clone()];
    parse_request_inner(
        &content,
        base_dir,
        macros,
        0,
        &mut include_stack,
        &canonical,
        search_paths,
    )
}

/// Resolve a filename by searching directories, then current directory.
fn resolve_in_search_paths(filename: &str, search_paths: &[PathBuf]) -> Option<PathBuf> {
    for dir in search_paths {
        let candidate = dir.join(filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let path = PathBuf::from(filename);
    if path.exists() {
        return Some(path);
    }
    None
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
                message: format!(
                    "file include not supported in parse_request_string at line {line_no}"
                ),
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
    search_paths: &[PathBuf],
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

            // Merge inline macros — expand through parent context first so that
            // `file "foo.req", P=$(P)` resolves $(P) to its current value.
            let child_macros = if inline_macros.is_empty() {
                macros.clone()
            } else {
                let expanded_macros = macros.expand(&inline_macros, &source_str, line_no)?;
                let overrides = MacroContext::parse_inline(&expanded_macros);
                macros.with_overrides(&overrides)
            };

            // Resolve: first try relative to including file's directory,
            // then search paths (matching C autosave behavior)
            let include_path = if Path::new(&expanded_path).is_absolute() {
                PathBuf::from(&expanded_path)
            } else {
                let relative = base_dir.join(&expanded_path);
                if relative.exists() {
                    relative
                } else {
                    // Fall back to search paths
                    resolve_in_search_paths(&expanded_path, search_paths).unwrap_or(relative) // use original for error message
                }
            };

            let canonical =
                std::fs::canonicalize(&include_path).map_err(|e| AutosaveError::RequestFile {
                    path: include_path.display().to_string(),
                    message: format!("cannot resolve include: {e}"),
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
            let inc_content =
                std::fs::read_to_string(&canonical).map_err(|e| AutosaveError::RequestFile {
                    path: canonical.display().to_string(),
                    message: e.to_string(),
                })?;
            let inc_dir = canonical.parent().unwrap_or(Path::new("."));
            let sub_entries = parse_request_inner(
                &inc_content,
                inc_dir,
                &child_macros,
                depth + 1,
                include_stack,
                &canonical,
                search_paths,
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

/// Parse a file directive: `file <path>[,] [macros]`
/// Path can be quoted or unquoted. Handles both:
///   file "name.req", P=$(P)
///   file name.req,   P=$(P)
fn parse_file_directive(rest: &str) -> (String, String) {
    let rest = rest.trim();
    if rest.starts_with('"') {
        // Quoted path
        if let Some(end_quote) = rest[1..].find('"') {
            let path = rest[1..end_quote + 1].to_string();
            let after = rest[end_quote + 2..].trim();
            // Strip leading comma between path and macros
            let macros = after.strip_prefix(',').unwrap_or(after).trim().to_string();
            (path, macros)
        } else {
            (rest.to_string(), String::new())
        }
    } else {
        // Unquoted: filename ends at first comma or whitespace
        let end = rest
            .find(|c: char| c == ',' || c.is_whitespace())
            .unwrap_or(rest.len());
        let path = rest[..end].to_string();
        let after = rest[end..].trim();
        // Strip leading comma between path and macros
        let macros = after.strip_prefix(',').unwrap_or(after).trim().to_string();
        (path, macros)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_file_directive_quoted_with_comma() {
        let (path, macros) = parse_file_directive("\"ADBase_settings.req\", P=$(P), R=$(R)");
        assert_eq!(path, "ADBase_settings.req");
        assert_eq!(macros, "P=$(P), R=$(R)");
    }

    #[test]
    fn test_parse_file_directive_unquoted_with_comma() {
        let (path, macros) = parse_file_directive("NDFile_settings.req,      P=$(P), R=netCDF1:");
        assert_eq!(path, "NDFile_settings.req");
        assert_eq!(macros, "P=$(P), R=netCDF1:");
    }

    #[test]
    fn test_parse_file_directive_quoted_no_macros() {
        let (path, macros) = parse_file_directive("\"foo.req\"");
        assert_eq!(path, "foo.req");
        assert_eq!(macros, "");
    }

    #[test]
    fn test_parse_file_directive_unquoted_space_separated() {
        let (path, macros) = parse_file_directive("foo.req P=X");
        assert_eq!(path, "foo.req");
        assert_eq!(macros, "P=X");
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
