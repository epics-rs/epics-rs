//! C `configMenuSub.c`: the aSub routines behind `configMenu.db`, and the
//! `save_restore.c` calls they make (`findConfigFiles`, `fdbrestoreX`,
//! `manual_save`, `getMacroString`).
//!
//! A load or save runs on a background task, as on C's save_restore task;
//! its completion is C `configMenuCallback`: `D = status` under the
//! record's lock, then `scanOnce` so the second process reports it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::CaResult;
use crate::runtime::task::{CallbackPriority, spawn_background};
use crate::server::database::{PvDatabase, SubroutineContext};
use crate::server::record::{ContextSubroutineFn, Record};
use crate::types::EpicsValue;

use super::request::concatenate_paths;
use super::save_set::ManualSource;
use super::settings::SrSettings;
use super::startup::{
    AutosaveStartupConfig, manual_restore_request_ok, manual_save_manager, run_manual_restore,
};

/// C `NUM_ITEMS`: menu entries per page.
const NUM_ITEMS: usize = 10;

/// One `<config>_<name>.cfg` file: C `struct configFileListItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    pub name: String,
    /// The text after `Menu:currDesc`, `None` when the file has no such line.
    pub description: Option<String>,
}

/// C `findConfigFiles`: the `<config>_*.cfg` files in `dir`, in directory
/// order, or `Err(-1)` when `dir` cannot be read.
pub fn find_config_files(
    dir: Option<&Path>,
    config: &str,
    settings: &SrSettings,
) -> Result<Vec<ConfigFile>, i32> {
    let debug = settings.debug();
    let prefix = format!("{config}_");
    if debug != 0 {
        println!("findConfigFiles: config='{config}', config_underscore={prefix}");
    }
    let shown = dir.map(|d| d.display().to_string()).unwrap_or_default();
    let Some(entries) = dir.and_then(|d| std::fs::read_dir(d).ok()) else {
        if debug != 0 {
            println!("findConfigFiles: opendir('{shown}') failed.");
        }
        return Err(-1);
    };
    if debug != 0 {
        println!("findConfigFiles: opendir('{shown}') succeeded.");
    }
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if debug > 1 {
            println!("findConfigFiles: checking '{file_name}'.");
        }
        let Some(rest) = file_name.strip_prefix(&prefix) else {
            continue;
        };
        if debug != 0 {
            println!("findConfigFiles: found '{file_name}'");
            println!("findConfigFiles: searching '{rest}' for .cfg");
        }
        let Some(name) = rest.strip_suffix(".cfg") else {
            continue;
        };
        if debug != 0 {
            println!("findConfigFiles: found config file '{name}'");
        }
        let fullpath = concatenate_paths(&shown, file_name, settings);
        let description = match std::fs::read_to_string(&fullpath) {
            Ok(text) => {
                if debug != 0 {
                    println!("findConfigFiles: searching '{fullpath}' for description");
                }
                text.lines().find_map(|line| {
                    let at = line.find("Menu:currDesc")?;
                    // C skips the one separator character after the name.
                    let rest = &line[at + "Menu:currDesc".len()..];
                    let mut chars = rest.chars();
                    chars.next();
                    Some(chars.as_str().trim_end_matches('\r').to_string())
                })
            }
            Err(_) => {
                if debug != 0 {
                    println!("findConfigFiles: can't open '{file_name}'");
                }
                None
            }
        };
        found.push(ConfigFile {
            name: name.to_string(),
            description,
        });
    }
    if debug != 0 {
        println!("findConfigFiles: ");
        for f in &found {
            println!(
                "\tname='{}'; desc='{}'",
                f.name,
                f.description.as_deref().unwrap_or("(null)")
            );
        }
    }
    Ok(found)
}

/// C `makeLegal`: every non-alphanumeric character becomes `_`.
pub fn make_legal(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// The four routines C's `configMenuRegistrar` adds, for the IOC's registry.
pub fn subroutines(
    holder: Arc<Mutex<AutosaveStartupConfig>>,
) -> Vec<(&'static str, ContextSubroutineFn)> {
    let list_holder = holder.clone();
    vec![
        (
            "configMenuList_init",
            Box::new(|_rec, ctx| {
                ctx.set_dpvt(Vec::<ConfigFile>::new());
                Ok(0)
            }),
        ),
        (
            "configMenuList_do",
            Box::new(move |rec, ctx| list_do(&list_holder, rec, ctx)),
        ),
        ("configMenu_init", Box::new(|_rec, _ctx| Ok(0))),
        (
            "configMenu_do",
            Box::new(move |rec, ctx| menu_do(&holder, rec, ctx)),
        ),
    ]
}

fn text(rec: &dyn Record, field: &str) -> String {
    match rec.get_field(field) {
        Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
        _ => String::new(),
    }
}

fn number(rec: &dyn Record, field: &str) -> f64 {
    rec.get_field(field).and_then(|v| v.to_f64()).unwrap_or(0.0)
}

/// C `strncpy(dst, src, 39)` into a 40-character string field.
fn item(s: &str) -> EpicsValue {
    let mut end = s.len().min(39);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    EpicsValue::String(s[..end].into())
}

const NAME_FIELDS: [&str; NUM_ITEMS] = [
    "VALA", "VALB", "VALC", "VALD", "VALE", "VALF", "VALG", "VALH", "VALI", "VALJ",
];
const DESC_FIELDS: [&str; NUM_ITEMS] = [
    "VALK", "VALL", "VALM", "VALN", "VALO", "VALP", "VALQ", "VALR", "VALS", "VALT",
];

/// C `configMenuList_do`.
fn list_do(
    holder: &Mutex<AutosaveStartupConfig>,
    rec: &mut dyn Record,
    ctx: &mut SubroutineContext<'_>,
) -> CaResult<i64> {
    let config = text(rec, "A");
    let page = number(rec, "B") as i64;
    let find = number(rec, "C") != 0.0;
    let (save_dir, settings) = {
        let cfg = holder.lock().unwrap();
        (cfg.save_file_path.clone(), cfg.settings.clone())
    };
    if ctx.dpvt::<Vec<ConfigFile>>().is_none() {
        ctx.set_dpvt(Vec::<ConfigFile>::new());
    }
    let list = ctx.dpvt::<Vec<ConfigFile>>().expect("set above");

    let mut status = 0;
    if find || list.first().is_none_or(|f| f.name.is_empty()) {
        match find_config_files(save_dir.as_deref(), &config, &settings) {
            Ok(found) => *list = found,
            Err(s) => {
                list.clear();
                status = s;
            }
        }
        if status != 0 {
            println!("configMenuList_do({config}): findConfigFiles returned {status}");
        }
        rec.put_field("C", EpicsValue::Short(0))?;
    }
    if status == 0 {
        let start = (page * NUM_ITEMS as i64).max(0) as usize;
        let mut items = list.iter().skip(start);
        for i in 0..NUM_ITEMS {
            let (name, desc) = match items.next() {
                Some(f) => (
                    item(&f.name),
                    item(f.description.as_deref().unwrap_or("no description")),
                ),
                None => (item(""), item("")),
            };
            rec.put_field(NAME_FIELDS[i], name)?;
            rec.put_field(DESC_FIELDS[i], desc)?;
        }
    }
    Ok(0)
}

/// C `configMenu_do`.
fn menu_do(
    holder: &Arc<Mutex<AutosaveStartupConfig>>,
    rec: &mut dyn Record,
    ctx: &mut SubroutineContext<'_>,
) -> CaResult<i64> {
    if text(rec, "C") != "Done" {
        // The scanOnce from the completion.
        let failed = number(rec, "D") != 0.0;
        rec.put_field("VALC", EpicsValue::Long(i32::from(failed)))?;
        rec.put_field("VALA", EpicsValue::Long(0))?;
        rec.put_field("VALB", EpicsValue::Long(0))?;
        return Ok(0);
    }
    let a = text(rec, "A");
    if a.is_empty() {
        rec.put_field("D", EpicsValue::Long(1))?;
        rec.put_field("VALC", EpicsValue::Long(1))?;
        return Ok(0);
    }
    // C rewrites the input in place.
    let a = make_legal(&a);
    rec.put_field("A", EpicsValue::String(a.as_str().into()))?;
    let mut filename = format!("{}_{a}.cfg", text(rec, "G"));
    // epicsSnprintf(filename, 99, ...)
    if filename.len() > 98 {
        let mut end = 98;
        while !filename.is_char_boundary(end) {
            end -= 1;
        }
        filename.truncate(end);
    }
    let req_file = text(rec, "F");
    let save = number(rec, "E") != 0.0;
    let b = match ctx.db() {
        Some(db) => {
            let done = Completion {
                db: db.clone(),
                record: ctx.name().to_string(),
            };
            if save {
                manual_save(holder, db, &req_file, filename, done)
            } else {
                fdbrestore_x(holder, db, &req_file, filename, done)
            }
        }
        None => -1,
    };
    rec.put_field("B", EpicsValue::Long(b))?;
    rec.put_field("VALA", EpicsValue::Long(1))?;
    rec.put_field("VALB", EpicsValue::Long(1))?;
    Ok(0)
}

/// C `configMenuCallback` for one record.
struct Completion {
    db: PvDatabase,
    record: String,
}

impl Completion {
    fn finish(self, status: i32) {
        {
            let _lock = self.db.lock_record(&self.record);
            if let Some(rec) = self.db.get_record(&self.record) {
                let _ = rec.write().record.put_field("D", EpicsValue::Long(status));
            }
        }
        self.db.scan_once(self.record.as_bytes());
    }
}

/// C `getMacroString(reqFile)`: the macro string of the set built from it.
fn set_macros(holder: &Mutex<AutosaveStartupConfig>, req_file: &str) -> String {
    let manager = holder.lock().unwrap().manager.clone();
    manager
        .and_then(|m| m.find_set_by_request_file(req_file))
        .map(|(set, _)| set.config().macro_string.clone())
        .unwrap_or_default()
}

fn save_dir_path(holder: &Mutex<AutosaveStartupConfig>, filename: &str) -> PathBuf {
    holder.lock().unwrap().save_dir_file(filename)
}

/// C `fdbrestoreX(filename, getMacroString(reqFile), ...)`: queue an
/// ASCII restore, then `asVerify` the file; the completion gets the count
/// of failed puts, or of differences when every put landed.
fn fdbrestore_x(
    holder: &Arc<Mutex<AutosaveStartupConfig>>,
    db: PvDatabase,
    req_file: &str,
    filename: String,
    done: Completion,
) -> i32 {
    let settings = holder.lock().unwrap().settings.clone();
    if !manual_restore_request_ok(&filename, "", &settings) {
        return -1;
    }
    let macros = set_macros(holder, req_file);
    let path = save_dir_path(holder, &filename);
    let (manager, settings) = {
        let cfg = holder.lock().unwrap();
        (cfg.manager.clone(), cfg.settings.clone())
    };
    spawn_background(CallbackPriority::Low, async move {
        let status = run_manual_restore(
            manager,
            &settings,
            &db,
            &path,
            &filename,
            ManualSource::AsciiFile,
            &macros,
        )
        .await;
        done.finish(status);
    });
    0
}

/// C `manual_save(reqFile, filename, ...)`: write the set built from
/// `req_file` to `filename` in the save directory, then `asVerify` it.
fn manual_save(
    holder: &Arc<Mutex<AutosaveStartupConfig>>,
    db: PvDatabase,
    req_file: &str,
    filename: String,
    done: Completion,
) -> i32 {
    // C passes configMenuCallback and the record as its user pointer.
    let callback = format!("{:p}", Completion::finish as fn(Completion, i32));
    let puser = format!("{:p}", &done);
    let Some(manager) = manual_save_manager(holder, req_file, &filename, &callback, &puser) else {
        return -1;
    };
    let path = save_dir_path(holder, &filename);
    let req_file = req_file.to_string();
    spawn_background(CallbackPriority::Low, async move {
        let status = manager.save_file_op(&db, &req_file, &filename, &path).await;
        done.finish(status);
    });
    0
}
