//! Inline `file` macros are split before they are expanded, not after.
//!
//! C `macParseDefns` (`macUtil.c:71-193`) splits the RAW definition string
//! into `name`/`value` pairs and keeps the values verbatim — its own
//! comment says quotes and escapes are removed from names only because
//! "unlike values, they will not be re-parsed" — and `macGetValue` expands
//! a value when the macro is looked up. So a comma that appears only
//! AFTER expansion can never terminate a definition.
//!
//! `load_request_file_with_search_paths` had the two steps the other way
//! round: it ran the whole inline string through the parent context and
//! then split the result on every comma. A parent `SUB=a,b` turned
//! `file "sub.req", P=$(P), S=$(SUB)` into `P=IOC:,S=a,b`, which split
//! into `P=IOC:` and `S=a` — the bare `b` matched no `=` and was dropped
//! with no message, so the included set silently saved the wrong PVs.
//!
//! The grammar now comes from `iocsh::macro_defn_pairs`, the crate's one
//! `macParseDefns` port, so a quoted comma survives here for the same
//! reason it survives in `dbLoadRecords`.
//!
//! No C reference for autosave itself: synApps `save_restore.c` is not on
//! this machine. The macLib citations above are epics-base, which is.

use std::collections::HashMap;

use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::{load_request_file, pv_names};

/// Build a two-file request set: `main.req` includes `sub.req`, passing
/// `defns` through as its inline macro text.
fn write_set(dir: &std::path::Path, defns: &str, sub_body: &str) -> std::path::PathBuf {
    let main = dir.join("main.req");
    std::fs::write(&main, format!("file \"sub.req\", {defns}\n")).unwrap();
    std::fs::write(dir.join("sub.req"), sub_body).unwrap();
    main
}

async fn members(main: &std::path::Path, parent: &[(&str, &str)]) -> Vec<String> {
    let map: HashMap<String, String> = parent
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let entries = load_request_file(main, &MacroContext::from_map(map))
        .await
        .expect("request file must load");
    pv_names(&entries)
}

#[epics_macros_rs::epics_test]
async fn a_comma_inside_an_expanded_value_does_not_end_the_definition() {
    let dir = tempfile::tempdir().unwrap();
    let main = write_set(dir.path(), "P=$(P), S=$(SUB)", "$(P)$(S)\n");

    assert_eq!(
        members(&main, &[("P", "IOC:"), ("SUB", "a,b")]).await,
        vec!["IOC:a,b".to_string()],
        "the whole `a,b` is one macro value; splitting it dropped the `b`"
    );
}

/// The definition AFTER the one carrying the comma is the one that
/// vanished entirely: pre-fix `S=a,b` consumed `b` as a nameless fragment
/// and `M` was never seen.
#[epics_macros_rs::epics_test]
async fn a_definition_after_a_comma_bearing_value_still_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let main = write_set(dir.path(), "S=$(SUB), M=m1", "$(S)/$(M)\n");

    assert_eq!(
        members(&main, &[("SUB", "a,b")]).await,
        vec!["a,b/m1".to_string()]
    );
}

/// The same rule from the other direction: a comma written literally in
/// the request file, quoted as `macParseDefns` requires, is one value.
#[epics_macros_rs::epics_test]
async fn a_quoted_comma_is_one_value() {
    let dir = tempfile::tempdir().unwrap();
    let main = write_set(dir.path(), "S='x,y', M=m1", "$(S)/$(M)\n");

    assert_eq!(members(&main, &[]).await, vec!["x,y/m1".to_string()]);
}

/// Negative control: the ordinary case keeps working, and a parent macro
/// referenced by an inline value still resolves to the parent's value.
#[epics_macros_rs::epics_test]
async fn plain_inline_macros_still_resolve_through_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let main = write_set(dir.path(), "P=$(P), M=m1", "$(P)$(M)\n");

    assert_eq!(
        members(&main, &[("P", "IOC:")]).await,
        vec!["IOC:m1".to_string()]
    );
}
