use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::error::{AutosaveError, AutosaveResult};
use super::format::MAX_INCLUDE_DEPTH;
use super::macros::MacroContext;
use super::settings::SrSettings;
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
    read_request_list(filename, search_paths, macros, "", &SrSettings::default()).await
}

/// [`load_request_file_with_search_paths`] tracing as `settings` says;
/// `macrostring` is the text `macros` came from, which the traces print.
pub(crate) async fn read_request_list(
    filename: &str,
    search_paths: &[PathBuf],
    macros: &MacroContext,
    macrostring: &str,
    settings: &SrSettings,
) -> AutosaveResult<RequestList> {
    let mut walk = IncludeWalk {
        stack: Vec::new(),
        search_paths,
        unopened: false,
        settings,
    };
    let Some(entries) = read_req_file(filename, macrostring, macros, 0, &mut walk)? else {
        return Err(AutosaveError::ReqFileUnopened(filename.to_string()));
    };
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
    settings: &'a SrSettings,
}

/// C `concatenate_paths` (save_restore.c:750-768): one `/` between the
/// two, so an absolute `s2` lands UNDER `s1` rather than replacing it.
pub(crate) fn concatenate_paths(s1: &str, s2: &str, settings: &SrSettings) -> String {
    let mut dest = s1.to_string();
    if !s2.is_empty() && !s2.starts_with('/') && !dest.is_empty() && !dest.ends_with('/') {
        dest.push('/');
    }
    match s2.strip_prefix('/') {
        Some(rest) if dest.ends_with('/') => dest.push_str(rest),
        _ => dest.push_str(s2),
    }
    concatenated(&dest, settings);
    dest
}

/// `concatenate_paths`'s trace, for a site whose path rs joined once,
/// before the C call that joins it again.
pub(crate) fn concatenated(dest: &str, settings: &SrSettings) {
    if settings.debug() > 2 {
        println!("save_restore:concatenate_paths: dest='{dest}'");
    }
}

/// C `openReqFile(reqFile, fpp)` (save_restore.c:3481-3534) without its
/// recent-name cache: the first of `search_paths` that holds `name`, else
/// `name` from the current directory when there are none. `want` is
/// C's non-NULL `fpp`.
fn open_req_file(
    name: &str,
    search_paths: &[PathBuf],
    want: bool,
    settings: &SrSettings,
) -> Option<(PathBuf, String)> {
    let debug = settings.debug();
    let mut opened = None;
    if debug > 5 {
        let fpp = if want {
            format!("{:p}", &opened)
        } else {
            "(nil)".to_string()
        };
        println!("save_restore:openReqFile: entry: reqFile='{name}', fpp={fpp}");
    }
    if search_paths.is_empty() {
        opened = read_req(PathBuf::from(name));
    } else {
        for dir in search_paths {
            let path = concatenate_paths(&dir.to_string_lossy(), name, settings);
            opened = read_req(path.into());
            if opened.is_some() {
                break;
            }
        }
    }
    if debug > 5 {
        match opened {
            Some(_) => println!("openReqFile: found '{name}' by searching"),
            None => println!("openReqFile: didn't find '{name}' by searching"),
        }
    }
    opened
}

fn read_req(path: PathBuf) -> Option<(PathBuf, String)> {
    let content = std::fs::read_to_string(&path).ok()?;
    Some((path, content))
}

/// C `openReqFile(reqFile, NULL)`: whether the request file is found.
pub(crate) fn req_file_exists(name: &str, search_paths: &[PathBuf], settings: &SrSettings) -> bool {
    open_req_file(name, search_paths, false, settings).is_some()
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

/// C `readReqFile(reqFile, plist, macrostring)` (save_restore.c:3538-3706)
/// for one file and its includes, `None` when the file did not open.
fn read_req_file(
    req_file: &str,
    macrostring: &str,
    macros: &MacroContext,
    depth: usize,
    walk: &mut IncludeWalk,
) -> AutosaveResult<Option<Vec<RequestEntry>>> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(AutosaveError::IncludeDepthExceeded(MAX_INCLUDE_DEPTH));
    }
    let debug = walk.settings.debug();
    // C's `plist`: one list for the whole include tree.
    let list = format!("{:p}", walk.search_paths.as_ptr());
    if debug > 1 {
        println!(
            "save_restore:readReqFile: entry: reqFile='{req_file}', plist={list}, macrostring='{macrostring}'"
        );
    }
    let Some((path, content)) = open_req_file(req_file, walk.search_paths, true, walk.settings)
    else {
        println!("save_restore:readReqFile: unable to open file {req_file}. Exiting.");
        return Ok(None);
    };
    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
    if walk.stack.contains(&canonical) {
        let mut chain: Vec<String> = walk.stack.iter().map(|p| p.display().to_string()).collect();
        chain.push(canonical.display().to_string());
        return Err(AutosaveError::IncludeCycle { chain });
    }
    walk.stack.push(canonical.clone());

    let pairs = crate::server::iocsh::macro_defn_pairs(macrostring);
    if !macrostring.is_empty() && debug >= 5 {
        println!("save_restore:readReqFile: Current macro definitions:");
        super::save_set::report_macros(macrostring);
        println!("save_restore:readReqFile: --------------------------");
    }

    let mut entries = Vec::new();

    for (idx, raw_line) in content.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();

        if debug >= 2 {
            // C expands only with definitions (`handle && pairs`).
            if !macrostring.is_empty() && debug > 5 {
                println!("save_restore:readReqFile:handle={:p}", macros);
                if let Some((name, value)) = pairs.first() {
                    println!("save_restore:readReqFile:pairs[0]='{name}'");
                    if let Some(value) = value {
                        println!("save_restore:readReqFile:pairs[1]='{value}'");
                    }
                }
            }
            let eline = macros.expand_quiet(raw_line);
            let name = eline.split_ascii_whitespace().next().unwrap_or("");
            println!("save_restore:readReqFile: line='{raw_line}', eline='{eline}', name='{name}'");
        }

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

            let eline = macros.expand_quiet(raw_line);
            let (template_file, new_macro) = c_include_args(&eline);
            if debug >= 2 {
                println!("save_restore:readReqFile: preparing to include file: eline='{eline}'");
                println!(
                    "save_restore:readReqFile: calling readReqFile('{template_file}', {list},'{new_macro}')"
                );
            }
            match read_req_file(&expanded_path, &new_macro, &child_macros, depth + 1, walk)? {
                Some(sub_entries) => entries.extend(sub_entries),
                None => walk.unopened = true,
            }
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
            source_file: canonical.clone(),
            line_no,
        });
    }
    walk.stack.pop();
    if debug > 1 {
        println!("save_restore:readReqFile: exit: reqFile='{req_file}'.");
    }

    Ok(Some(entries))
}

/// The template file and macro string C's `readReqFile` takes from an
/// expanded `file` line (save_restore.c:3611-3644), for its trace.
fn c_include_args(eline: &str) -> (String, String) {
    let t = eline.get(4..).unwrap_or("").trim_start();
    let t = t.strip_prefix('"').unwrap_or(t).trim_start();
    let end = t
        .find(|c: char| c.is_ascii_whitespace() || c == '"' || c == '#')
        .unwrap_or(t.len());
    let mut new_macro = String::new();
    for c in t[end..].chars() {
        if c.is_ascii_whitespace() || c == ',' {
            if new_macro.len() >= 3 && !new_macro.ends_with(',') {
                new_macro.push(',');
            }
        } else if c != '"' {
            new_macro.push(c);
        }
    }
    if new_macro.ends_with(',') {
        new_macro.pop();
    }
    if new_macro.len() < 3 {
        new_macro.clear();
    }
    (t[..end].to_string(), new_macro)
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
