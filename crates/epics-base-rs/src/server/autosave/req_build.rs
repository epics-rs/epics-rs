//! dbrestore.c's request-file builders (`dbrestore.c:1455-1745`).

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::runtime::log::errlog_vprintf;
use crate::server::db_loader::{MacroExpandOptions, db_expand_file_name, expand_macros};
use crate::server::iocsh::record_names_type_major;
use crate::server::iocsh::registry::CommandContext;
use crate::server::iocsh::{db_load_records_hook, set_db_load_records_hook};
use crate::types::DbfCode;

use super::AutosaveStartupConfig;
use super::macros::MacroContext;
use super::request::req_file_exists;

/// C `MAX_FIELD_SIZE` (dbrestore.c:1453).
const MAX_FIELD_SIZE: usize = 30;

/// C `makeAutosaveFileFromDbInfo(fileBaseName, info_name)`
/// (dbrestore.c:1460-1554): a request file of `<record>.<field>` for each
/// field the records' `info_name` info item names, records by type and
/// then load order, aliases skipped.
pub fn make_autosave_file_from_db_info(
    ctx: &CommandContext,
    file_base_name: &str,
    info_name: &str,
) {
    let fname = if file_base_name.contains(".req") {
        file_base_name.to_string()
    } else {
        format!("{file_base_name}.req")
    };
    let Ok(mut out) = std::fs::File::create(&fname) else {
        errlog_vprintf(format_args!(
            "../dbrestore.c:makeAutosaveFileFromDbInfo unable to open file '{fname}'\n"
        ));
        return;
    };
    let mut text = Vec::new();
    for name in record_names_type_major(ctx) {
        let Some(rec) = ctx.db().get_record(&name) else {
            continue;
        };
        let rec = rec.read();
        let Some(info) = rec.get_info(info_name) else {
            continue;
        };
        for field in info.split_ascii_whitespace() {
            // C's byte cut can split a character; such a name is no field
            // and is printed with the split bytes.
            let field = &field.as_bytes()[..field.len().min(MAX_FIELD_SIZE - 1)];
            // C sets `long_string` on the first `$` field and never clears
            // it, so every later non-string field of the walk was refused
            // as not found; here it is the field's own `$`.
            let (real, long_string) = match field.strip_suffix(b"$") {
                Some(real) => (real, true),
                None => (field, false),
            };
            let desc = std::str::from_utf8(real)
                .ok()
                .and_then(|real| rec.field_desc(real));
            let found = desc.is_some_and(|desc| {
                !long_string
                    || matches!(
                        desc.declared_dbf,
                        DbfCode::String | DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink
                    )
            });
            let line = [name.as_bytes(), b".", field].concat();
            if found {
                text.extend_from_slice(&line);
                text.push(b'\n');
            } else {
                let msg = [b"makeAutosaveFileFromDbInfo: ", &line[..], b" not found"];
                ctx.println_bytes(&msg.concat());
            }
        }
    }
    let _ = out.write_all(&text);
}

/// C `makeAutosaveFiles()` (dbrestore.c:1739-1743).
pub fn make_autosave_files(ctx: &CommandContext) {
    make_autosave_file_from_db_info(ctx, "info_settings.req", "autosaveFields");
    make_autosave_file_from_db_info(ctx, "info_positions.req", "autosaveFields_pass0");
}

/// C `macEnvExpand(filename)` for `func`, or ERRLOG C's refusal.
fn env_expand(func: &str, filename: &str) -> Option<String> {
    let fname = db_expand_file_name(filename);
    if fname.is_none() {
        errlog_vprintf(format_args!(
            "../dbrestore.c:{func} macEnvExpand('{filename}') returned NULL\n"
        ));
    }
    fname
}

/// C `eraseFile(filename)` (dbrestore.c:1557-1570): truncates the file.
pub fn erase_file(filename: &str) -> i32 {
    let Some(fname) = env_expand("eraseFile", filename) else {
        return -1;
    };
    let _ = std::fs::File::create(fname);
    0
}

/// C `appendToFile(filename, line)` (dbrestore.c:1572-1592).
pub fn append_to_file(filename: &str, line: &str) -> i32 {
    let Some(fname) = env_expand("appendToFile", filename) else {
        return -1;
    };
    let appended = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&fname)
        .map(|mut fd| {
            let _ = writeln!(fd, "{line}");
        });
    if appended.is_err() {
        errlog_vprintf(format_args!(
            "../dbrestore.c:appendToFile Can't open file '{fname}'\n"
        ));
        return -1;
    }
    0
}

/// C `MAXSTRING` (save_restore.h:61).
const MAXSTRING: usize = 300;

/// C `struct buildInfoItem` (dbrestore.c:1599-1604).
#[derive(Debug)]
pub struct BuildItem {
    filename: String,
    suffix: String,
    enabled: bool,
}

/// C `autosaveBuild(filename, reqFileSuffix, on)` (dbrestore.c:1681-1734):
/// from now on each loaded `.db` whose `<base><suffix>` request file is on
/// the request path adds `file <request file> <macros>` to `filename`.
/// A `None` suffix is C's NULL.
pub fn autosave_build(
    holder: &Arc<Mutex<AutosaveStartupConfig>>,
    filename: &str,
    req_file_suffix: Option<&str>,
    on: bool,
) -> i32 {
    let mut h = holder.lock().unwrap();
    if !h.build_hooked {
        h.build_hooked = true;
        let previous = db_load_records_hook();
        let holder = holder.clone();
        set_db_load_records_hook(Some(Arc::new(move |fname: &str, macro_: &str| {
            my_db_load_records_hook(&holder, previous.as_deref(), fname, macro_);
        })));
    }
    if filename.is_empty() {
        crate::epics_println!("autosaveBuild: bad filename");
        return -1;
    }
    let debug = h.settings.debug();
    let state = if on { "enabled" } else { "disabled" };
    let mut file_found = false;
    let mut item_found = false;
    for item in h.build_items.iter_mut().filter(|i| i.filename == filename) {
        file_found = true;
        if req_file_suffix.is_none_or(|s| s.starts_with('*') || s == item.suffix) {
            if debug != 0 {
                crate::epics_println!(
                    "autosaveBuild: {state} filename '{filename}' and suffix '{}'.",
                    item.suffix
                );
            }
            item.enabled = on;
            item_found = true;
        }
    }
    // C returns after the first item it toggles, so a `*` suffix reached
    // only one of the file's suffixes; here it reaches each.
    if item_found {
        return 0;
    }
    let Some(suffix) = req_file_suffix.filter(|s| !s.is_empty()) else {
        crate::epics_println!("autosaveBuild: bad suffix");
        return -1;
    };
    if !file_found {
        erase_file(filename);
    }
    h.build_items.push(BuildItem {
        filename: filename.to_string(),
        suffix: suffix.to_string(),
        enabled: on,
    });
    if debug != 0 {
        crate::epics_println!(
            "autosaveBuild: initialized and {state} filename '{filename}' and suffix '{suffix}'."
        );
    }
    0
}

/// `s` cut to at most `max` bytes on a char boundary, as a C buffer of
/// `max + 1` holds it.
fn c_buf(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// C `myDbLoadRecordsHook(fname, macro)` (dbrestore.c:1612-1679).
fn my_db_load_records_hook(
    holder: &Mutex<AutosaveStartupConfig>,
    previous: Option<&(dyn Fn(&str, &str) + Send + Sync)>,
    fname: &str,
    macro_: &str,
) {
    // C would go on with a NULL name and crash.
    let Some(db_file_name) = db_expand_file_name(fname) else {
        return;
    };
    let settings = holder.lock().unwrap().settings.clone();
    let debug = settings.debug();
    if debug >= 5 {
        // C prints its static `macroString`, the previous load's expanded
        // subs; this load's own are printed here.
        crate::epics_println!("myDbLoadRecordsHook: dbFileName='{db_file_name}'; subs='{macro_}'");
    }
    if let Some(previous) = previous {
        previous(&db_file_name, macro_);
    }
    let base = match db_file_name.rfind('/') {
        Some(at) => &db_file_name[at + 1..],
        None => db_file_name
            .rfind('\\')
            .map_or(db_file_name.as_str(), |at| &db_file_name[at + 1..]),
    };
    // C sizes this copy by the previous base's length, so a base after a
    // long one could be cut short; here every base gets the whole buffer.
    let base = c_buf(base, MAXSTRING - 1);
    let Some(end) = [".db", ".vdb", ".template"]
        .iter()
        .find_map(|ext| base.find(ext))
    else {
        crate::epics_println!(
            "myDbLoadRecordsHook: Can't make request-file name from '{db_file_name}'"
        );
        return;
    };
    let base = &base[..end];
    let (items, search_paths): (Vec<(String, String)>, Vec<PathBuf>) = {
        let h = holder.lock().unwrap();
        (
            h.build_items
                .iter()
                .filter(|i| i.enabled)
                .map(|i| (i.filename.clone(), i.suffix.clone()))
                .collect(),
            h.request_file_paths.clone(),
        )
    };
    for (filename, suffix) in items {
        let request_file_name = format!("{base}{suffix}");
        if request_file_name.len() >= MAXSTRING
            || !req_file_exists(&request_file_name, &search_paths, &settings)
        {
            continue;
        }
        if debug >= 5 {
            crate::epics_println!("myDbLoadRecordsHook: found '{request_file_name}'");
        }
        // The row's own definitions expanded among themselves, e.g.
        // "N=1,M=m$(N)" (dbrestore.c:1656-1667).
        let macro_string = c_buf(macro_, MAXSTRING - 1);
        let expanded = expand_macros(
            macro_string,
            &MacroContext::parse_inline(macro_string),
            MacroExpandOptions {
                env_fallback: false,
                dollar_escape: false,
                suppress_warnings: true,
            },
        )
        .text;
        let macro_string = c_buf(&expanded, MAXSTRING - 1);
        let cmd = format!("file {request_file_name} {macro_string}");
        if cmd.len() < MAXSTRING {
            append_to_file(&filename, &cmd);
        } else {
            crate::epics_println!(
                "myDbLoadRecordsHook: Can't include {request_file_name}; requestFileCmd is too long (n = {}, MAXSTRING = {MAXSTRING})",
                cmd.len()
            );
        }
    }
}
