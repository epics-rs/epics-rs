//! C `readReqFile` reads a request file with `fgets(line, BUF_SIZE, ..)`
//! (save_restore.c:3576-3590): a line that does not end in a newline has
//! the rest discarded and is cut at its last comma or space. The C oracle
//! IOC's list for this `s.req` was `A`, `$(P)` and `D`: the include lost
//! its `P=B` past byte 199, and the last line `C`, with no newline, was cut
//! to nothing.

use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::{load_request_file_with_search_paths, pv_names};

#[epics_macros_rs::epics_test]
async fn a_line_without_a_newline_is_cut_at_its_last_separator() {
    let dir = tempfile::tempdir().unwrap();
    let z = "z".repeat(190);
    std::fs::write(
        dir.path().join("s.req"),
        format!("A\nfile t.req Z={z},P=B\nD E\nC"),
    )
    .unwrap();
    std::fs::write(dir.path().join("t.req"), "$(P)\n").unwrap();
    let list = load_request_file_with_search_paths(
        "s.req",
        &[dir.path().to_path_buf()],
        &MacroContext::default(),
    )
    .await
    .unwrap();
    assert_eq!(pv_names(&list.entries), ["A", "$(P)", "D"]);
}
