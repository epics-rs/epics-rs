//! C `readReqFile` (save_restore.c:3562-3640) gives each request file a
//! macLib handle of its own macro string and nothing else: with no string
//! the lines are read as written, an undefined name comes back as
//! `$(NAME,undefined)` and the line still names a channel, and an included
//! file sees only the definitions on its `file` line.

use std::collections::HashMap;

use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::{load_request_file_with_search_paths, pv_names};

async fn members(dir: &std::path::Path, macros: &[(&str, &str)]) -> Vec<String> {
    let map: HashMap<String, String> = macros
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let list = load_request_file_with_search_paths(
        "main.req",
        &[dir.to_path_buf()],
        &MacroContext::from_map(map),
    )
    .await
    .expect("a list with macro faults still builds");
    pv_names(&list.entries)
}

fn write(dir: &std::path::Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

#[epics_macros_rs::epics_test]
async fn each_file_expands_with_its_own_definitions_only() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "main.req",
        "$(P)a\n$(UNDEF)b\nfile plain.req\nfile given.req Q=$(P)\n",
    );
    write(dir.path(), "plain.req", "$(P)c\n");
    write(dir.path(), "given.req", "$(Q)d\n$(P)e\n");
    assert_eq!(
        members(dir.path(), &[("P", "IOC:")]).await,
        [
            "IOC:a",
            "$(UNDEF,undefined)b",
            "$(P)c",
            "IOC:d",
            "$(P,undefined)e",
        ]
    );
}

/// No macro string: not even the environment is read.
#[epics_macros_rs::epics_test]
async fn no_macro_string_reads_every_line_as_written() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "main.req", "$(HOME)x\n$$y\n");
    assert_eq!(members(dir.path(), &[]).await, ["$(HOME)x", "$$y"]);
}
