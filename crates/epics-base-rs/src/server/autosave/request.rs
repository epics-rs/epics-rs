use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::error::{AutosaveError, AutosaveResult};
use super::format::MAX_INCLUDE_DEPTH;
use super::macros::MacroContext;
use super::startup::is_valid_first_pv_char;

/// A single entry parsed from a .req file.
#[derive(Debug, Clone)]
pub struct RequestEntry {
    pub pv_name: String,
    pub source_file: PathBuf,
    pub line_no: usize,
    /// Original text before macro expansion (if different from pv_name).
    pub expanded_from: Option<String>,
}

/// A parsed request file: its entries, and whether a `file` include in it
/// could not be opened.
///
/// C `readReqFile` (save_restore.c:3553-3559) skips such an include, the
/// list keeping every other PV, and leaves the list's status FAIL "Can't
/// open .req file" until its first write.
#[derive(Debug, Clone)]
pub struct RequestList {
    pub entries: Vec<RequestEntry>,
    pub include_unopened: bool,
}

/// Load and parse a .req file, resolving includes and macros.
pub async fn load_request_file(
    path: &Path,
    macros: &MacroContext,
) -> AutosaveResult<Vec<RequestEntry>> {
    Ok(
        load_request_file_with_search_paths(&path.to_string_lossy(), &[], macros)
            .await?
            .entries,
    )
}

/// Load a .req file by searching multiple directories.
///
/// The top-level file and every `file` include are found as C
/// `openReqFile` (save_restore.c:3481-3534) finds them: in each of
/// `search_paths` in order when there is one, else as named from the
/// current directory. The including file's directory is not searched.
pub async fn load_request_file_with_search_paths(
    filename: &str,
    search_paths: &[PathBuf],
    macros: &MacroContext,
) -> AutosaveResult<RequestList> {
    let mut opened = None;
    for path in req_file_candidates(filename, search_paths) {
        if let Ok(content) = crate::runtime::fs::read_to_string(&path).await {
            opened = Some((path, content));
            break;
        }
    }
    let Some((path, content)) = opened else {
        println!("save_restore:readReqFile: unable to open file {filename}. Exiting.");
        return Err(AutosaveError::ReqFileUnopened(filename.to_string()));
    };
    let canonical = crate::runtime::fs::canonicalize(&path)
        .await
        .unwrap_or(path);
    let mut walk = IncludeWalk {
        stack: vec![canonical.clone()],
        search_paths,
        unopened: false,
    };
    let entries = parse_request_inner(&content, macros, 0, &canonical, &mut walk)?;
    Ok(RequestList {
        entries,
        include_unopened: walk.unopened,
    })
}

/// State one request file's include tree shares across its levels.
struct IncludeWalk<'a> {
    /// The files being read, outermost first, for cycle detection.
    stack: Vec<PathBuf>,
    search_paths: &'a [PathBuf],
    unopened: bool,
}

/// The paths C `openReqFile` tries for `name`, in order.
fn req_file_candidates(name: &str, search_paths: &[PathBuf]) -> Vec<PathBuf> {
    if search_paths.is_empty() {
        return vec![PathBuf::from(name)];
    }
    search_paths
        .iter()
        .map(|dir| concatenate_paths(&dir.to_string_lossy(), name).into())
        .collect()
}

/// C `concatenate_paths` (save_restore.c:750-768): one `/` between the
/// two, so an absolute `s2` lands UNDER `s1` rather than replacing it.
fn concatenate_paths(s1: &str, s2: &str) -> String {
    let mut dest = s1.to_string();
    if !s2.is_empty() && !s2.starts_with('/') && !dest.is_empty() && !dest.ends_with('/') {
        dest.push('/');
    }
    match s2.strip_prefix('/') {
        Some(rest) if dest.ends_with('/') => dest.push_str(rest),
        _ => dest.push_str(s2),
    }
    dest
}

/// [`load_request_file_with_search_paths`]'s include opener.
fn open_include(name: &str, search_paths: &[PathBuf]) -> Option<(PathBuf, String)> {
    req_file_candidates(name, search_paths)
        .into_iter()
        .find_map(|path| {
            let content = std::fs::read_to_string(&path).ok()?;
            Some((std::fs::canonicalize(&path).unwrap_or(path), content))
        })
}

/// Load a .req file from a string (for testing without filesystem).
pub fn parse_request_string(
    content: &str,
    macros: &MacroContext,
    source_name: &str,
) -> AutosaveResult<Vec<RequestEntry>> {
    let source = PathBuf::from(source_name);
    let mut entries = Vec::new();

    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        if line.is_empty() {
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

        let expanded = macros.expand(line);
        let Some(name) = request_pv_name(&expanded) else {
            continue;
        };
        entries.push(RequestEntry {
            pv_name: name.to_string(),
            expanded_from: (name != line).then(|| line.to_string()),
            source_file: source.clone(),
            line_no,
        });
    }
    Ok(entries)
}

fn parse_request_inner(
    content: &str,
    macros: &MacroContext,
    depth: usize,
    source_path: &Path,
    walk: &mut IncludeWalk,
) -> AutosaveResult<Vec<RequestEntry>> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(AutosaveError::IncludeDepthExceeded(MAX_INCLUDE_DEPTH));
    }

    let mut entries = Vec::new();

    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();

        if line.is_empty() {
            continue;
        }

        if line.starts_with("file ") || line.starts_with("file\t") {
            let rest = line[4..].trim();
            let (file_path, inline_macros) = parse_file_directive(rest);

            let expanded_path = macros.expand(&file_path);

            // Merge inline macros in macLib's order: split the RAW text into
            // definitions, THEN expand each value through the parent context
            // (C `macParseDefns` keeps values verbatim, `macUtil.c:74-196`,
            // and `macGetValue` expands them on lookup). Expanding the whole
            // string first let a comma that arrived from INSIDE a value act
            // as a separator: with a parent `SUB=a,b`, `file "sub.req",
            // P=$(P), S=$(SUB)` became `P=IOC:,S=a,b` and then `S=a`, with
            // the bare `b` dropped and no message.
            // The included file sees only its own line's definitions, as C
            // hands `readReqFile` the include's macro string and nothing
            // else (save_restore.c:3640). Each value is expanded through
            // this level after the split, so a comma from inside a value
            // does not end a definition the way C's whole-line expansion
            // lets it.
            let child_macros = MacroContext::from_map(
                MacroContext::parse_inline(&inline_macros)
                    .into_iter()
                    .map(|(name, raw)| (name, macros.expand(&raw)))
                    .collect(),
            );

            let Some((canonical, inc_content)) = open_include(&expanded_path, walk.search_paths)
            else {
                println!("save_restore:readReqFile: unable to open file {expanded_path}. Exiting.");
                walk.unopened = true;
                continue;
            };

            // Cycle detection
            if walk.stack.contains(&canonical) {
                let mut chain: Vec<String> =
                    walk.stack.iter().map(|p| p.display().to_string()).collect();
                chain.push(canonical.display().to_string());
                return Err(AutosaveError::IncludeCycle { chain });
            }

            walk.stack.push(canonical.clone());
            let sub_entries =
                parse_request_inner(&inc_content, &child_macros, depth + 1, &canonical, walk)?;
            entries.extend(sub_entries);
            walk.stack.pop();
            continue;
        }

        // Regular PV line
        let expanded = macros.expand(line);
        let Some(name) = request_pv_name(&expanded) else {
            continue;
        };
        entries.push(RequestEntry {
            pv_name: name.to_string(),
            expanded_from: (name != line).then(|| line.to_string()),
            source_file: source_path.to_path_buf(),
            line_no,
        });
    }

    Ok(entries)
}

/// C `readReqFile`'s channel name (save_restore.c:3605-3650): the expanded
/// line's first token, kept only when it starts with a PV character or `$`,
/// so a `#` token is a comment and anything after the name is ignored.
fn request_pv_name(expanded: &str) -> Option<&str> {
    let name = expanded
        .split([' ', '\t', '\n', '\x0B', '\x0C', '\r'])
        .find(|t| !t.is_empty())?;
    (is_valid_first_pv_char(name) || name.starts_with('$')).then_some(name)
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
    fn a_line_names_its_first_token_if_c_would_take_it() {
        let macros = MacroContext::from_map(
            [("P", "IOC:"), ("C", "#")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .into(),
        );
        let entries =
            parse_request_string("$(P)a 0.5\n*bad\n$(C) note\n$(UNDEF)x\n", &macros, "t.req")
                .unwrap();
        assert_eq!(pv_names(&entries), ["IOC:a", "$(UNDEF,undefined)x"]);
        assert_eq!(entries[0].expanded_from.as_deref(), Some("$(P)a 0.5"));
    }

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
