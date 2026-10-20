use std::path::{Path, PathBuf};

use super::error::{AutosaveError, AutosaveResult};
use super::format::{BUF_SIZE, MAX_INCLUDE_DEPTH};
use super::macros::MacroContext;
use super::save_set::FileMacros;
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
    /// The list's path PV, name PV and backups its levels' macros set.
    pub file_macros: FileMacros,
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
    read_request_list(filename, search_paths, macros, None, &SrSettings::default()).await
}

/// [`load_request_file_with_search_paths`] tracing as `settings` says;
/// `macrostring` is the text `macros` came from, which the traces print.
pub(crate) async fn read_request_list(
    filename: &str,
    search_paths: &[PathBuf],
    macros: &MacroContext,
    macrostring: Option<&str>,
    settings: &SrSettings,
) -> AutosaveResult<RequestList> {
    let mut walk = IncludeWalk {
        stack: Vec::new(),
        search_paths,
        unopened: false,
        file_macros: FileMacros::default(),
        settings,
    };
    let Some(entries) = read_req_file(filename, macrostring, macros, 0, &mut walk)? else {
        return Err(AutosaveError::ReqFileUnopened(filename.to_string()));
    };
    Ok(RequestList {
        entries,
        include_unopened: walk.unopened,
        file_macros: walk.file_macros,
    })
}

/// State one request file's include tree shares across its levels.
struct IncludeWalk<'a> {
    /// The files being read, outermost first, for cycle detection.
    stack: Vec<PathBuf>,
    search_paths: &'a [PathBuf],
    unopened: bool,
    file_macros: FileMacros,
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
        crate::epics_println!("save_restore:concatenate_paths: dest='{dest}'");
    }
}

/// C `openReqFile(reqFile, fpp)`'s search (save_restore.c:3506-3534): the
/// first of `search_paths` that holds `name`, else `name` from the current
/// directory when there are none, remembered for [`req_file_exists`].
/// `want` is C's non-NULL `fpp`.
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
        crate::epics_println!("save_restore:openReqFile: entry: reqFile='{name}', fpp={fpp}");
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
            Some(_) => crate::epics_println!("openReqFile: found '{name}' by searching"),
            None => crate::epics_println!("openReqFile: didn't find '{name}' by searching"),
        }
    }
    settings
        .recent_req_files
        .lock()
        .unwrap()
        .record(name, opened.is_some());
    opened
}

fn read_req(path: PathBuf) -> Option<(PathBuf, String)> {
    let content = std::fs::read_to_string(&path).ok()?;
    Some((path, content))
}

/// C `openReqFile(reqFile, NULL)`: whether the request file is found, as
/// last found or not when the name is remembered (save_restore.c:3493-3503).
pub(crate) fn req_file_exists(name: &str, search_paths: &[PathBuf], settings: &SrSettings) -> bool {
    let recent = settings.recent_req_files.lock().unwrap().lookup(name);
    if let Some(found) = recent {
        if settings.debug() > 5 {
            let value = if found { "found" } else { "not-found" };
            crate::epics_println!("openReqFile: using cached {value} value for '{name}'");
        }
        return found;
    }
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

/// The lines C `readReqFile` reads with `fgets(line, BUF_SIZE, inp_fd)`
/// (save_restore.c:3576-3590): one that does not end in a newline — too
/// long, or the file's last — has the rest discarded and is cut at its last
/// comma or space, so it does not end inside a macro definition.
struct FgetsLines<'a> {
    rest: &'a [u8],
    debug: i32,
}

impl FgetsLines<'_> {
    fn fgets(&mut self) -> Option<Vec<u8>> {
        if self.rest.is_empty() {
            return None;
        }
        let max = (BUF_SIZE - 1).min(self.rest.len());
        let n = self.rest[..max]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(max, |i| i + 1);
        let (chunk, rest) = self.rest.split_at(n);
        self.rest = rest;
        Some(chunk.to_vec())
    }
}

impl Iterator for FgetsLines<'_> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let mut line = self.fgets()?;
        if line.last() != Some(&b'\n') {
            let mut eline = line.clone();
            while !eline.is_empty() && eline.last() != Some(&b'\n') {
                if self.debug != 0 {
                    crate::epics_println!(
                        "save_restore:readReqFile: didn't reach newline:\n\t'{}'",
                        text(&eline)
                    );
                }
                let Some(next) = self.fgets() else { break };
                eline = next;
                if self.debug != 0 {
                    crate::epics_println!(
                        "save_restore:readReqFile: discard:\n\t'{}'",
                        text(&eline)
                    );
                }
            }
            // C's `isspace`, which takes `\v` too.
            let mut c = line.len() - 1;
            while c > 0 && line[c] != b',' && !matches!(line[c], b' ' | b'\t'..=b'\r') {
                c -= 1;
            }
            line.truncate(c);
            if self.debug != 0 {
                crate::epics_println!("save_restore:readReqFile: line='{}'", text(&line));
            }
        }
        Some(text(&line))
    }
}

/// C `readReqFile(reqFile, plist, macrostring)` (save_restore.c:3538-3706)
/// for one file and its includes, `None` when the file did not open.
fn read_req_file(
    req_file: &str,
    macrostring: Option<&str>,
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
        crate::epics_println!(
            "save_restore:readReqFile: entry: reqFile='{req_file}', plist={list}, macrostring='{}'",
            macrostring.unwrap_or("NULL")
        );
    }
    let macrostring = macrostring.unwrap_or_default();
    let Some((path, content)) = open_req_file(req_file, walk.search_paths, true, walk.settings)
    else {
        crate::epics_println!("save_restore:readReqFile: unable to open file {req_file}. Exiting.");
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
        crate::epics_println!("save_restore:readReqFile: Current macro definitions:");
        macros.report();
        crate::epics_println!("save_restore:readReqFile: --------------------------");
    }

    let mut entries = Vec::new();

    let lines = FgetsLines {
        rest: content.as_bytes(),
        debug,
    };
    for (idx, raw_line) in lines.enumerate() {
        let raw_line = raw_line.as_str();
        let line_no = idx + 1;
        let line = raw_line.trim();

        // C expands only with definitions (`handle && pairs`).
        if !macrostring.is_empty() && debug > 5 {
            crate::epics_println!("save_restore:readReqFile:handle={:p}", macros);
            match pairs.first() {
                Some((name, value)) => {
                    crate::epics_println!("save_restore:readReqFile:pairs[0]='{name}'");
                    if let Some(value) = value {
                        crate::epics_println!("save_restore:readReqFile:pairs[1]='{value}'");
                    }
                }
                // No definition: C's `pairs[0]` is NULL, printed by glibc.
                None => crate::epics_println!("save_restore:readReqFile:pairs[0]='(null)'"),
            }
        }
        // Once per line, the line as read, so macLib names it in a notice.
        let eline = macros.expand(raw_line);
        if debug >= 2 {
            let name = eline.split_ascii_whitespace().next().unwrap_or("");
            crate::epics_println!(
                "save_restore:readReqFile: line='{raw_line}', eline='{eline}', name='{name}'"
            );
        }

        if line.is_empty() {
            continue;
        }

        if line.starts_with("file ") || line.starts_with("file\t") {
            let rest = line[4..].trim();
            let (file_path, inline_macros) = parse_file_directive(rest);

            let expanded_path = macros.expand_part(&file_path);

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
            let (template_file, new_macro) = c_include_args(&eline);
            let child_macros = MacroContext::from_defs(
                MacroContext::defn_pairs(&inline_macros)
                    .map(|(name, raw)| (name, macros.expand_part(&raw)))
                    .collect(),
                !new_macro.is_empty(),
            );

            if debug >= 2 {
                crate::epics_println!(
                    "save_restore:readReqFile: preparing to include file: eline='{eline}'"
                );
                crate::epics_println!(
                    "save_restore:readReqFile: calling readReqFile('{template_file}', {list},'{new_macro}')"
                );
            }
            match read_req_file(
                &expanded_path,
                Some(&new_macro),
                &child_macros,
                depth + 1,
                walk,
            )? {
                Some(sub_entries) => entries.extend(sub_entries),
                None => walk.unopened = true,
            }
            continue;
        }

        // Regular PV line
        let Some(name) = request_pv_name(&eline) else {
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
    walk.file_macros.take_from(macros);
    if debug > 1 {
        crate::epics_println!("save_restore:readReqFile: exit: reqFile='{req_file}'.");
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

    /// C answers an existence check from the names it last searched for,
    /// so a file made after a failed search stays not found.
    #[test]
    fn an_existence_check_answers_from_the_last_search() {
        let dir = tempfile::tempdir().unwrap();
        let paths = [dir.path().to_path_buf()];
        let settings = SrSettings::default();
        assert!(!req_file_exists("a.req", &paths, &settings));
        std::fs::write(dir.path().join("a.req"), "A\n").unwrap();
        assert!(!req_file_exists("a.req", &paths, &settings));
        std::fs::write(dir.path().join("b.req"), "B\n").unwrap();
        assert!(req_file_exists("b.req", &paths, &settings));
        std::fs::remove_file(dir.path().join("b.req")).unwrap();
        assert!(req_file_exists("b.req", &paths, &settings));
        // Five newer misses push `a.req` out.
        for i in 0..5 {
            assert!(!req_file_exists(&format!("x{i}.req"), &paths, &settings));
        }
        assert!(req_file_exists("a.req", &paths, &settings));
    }

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
