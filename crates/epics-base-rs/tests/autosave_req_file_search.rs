//! C `openReqFile` (save_restore.c:3481-3534) looks for a request file,
//! the top-level one and every `file` include alike, in each
//! `set_requestfile_path` directory when there is one, else as named from
//! the current directory. It never looks beside the including file.

use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::load_request_file_with_search_paths;

#[epics_macros_rs::epics_test]
async fn an_include_beside_its_includer_but_off_the_path_does_not_open() {
    let path = tempfile::tempdir().unwrap();
    let sub = path.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(path.path().join("top.req"), "TOP\nfile sub/mid.req\n").unwrap();
    std::fs::write(sub.join("mid.req"), "MID\nfile inner.req\n").unwrap();
    // Beside `sub/mid.req`, but `<path>/inner.req` does not exist.
    std::fs::write(sub.join("inner.req"), "BESIDE\n").unwrap();
    let list = load_request_file_with_search_paths(
        "top.req",
        &[path.path().to_path_buf()],
        &MacroContext::new(),
    )
    .await
    .unwrap();
    let names: Vec<_> = list.entries.iter().map(|e| e.pv_name.as_str()).collect();
    assert_eq!(names, ["TOP", "MID"]);
    assert!(list.include_unopened);
}

#[epics_macros_rs::epics_test]
async fn includes_search_the_path_in_order() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(second.path().join("top.req"), "file inner.req\n").unwrap();
    std::fs::write(first.path().join("inner.req"), "FIRST\n").unwrap();
    std::fs::write(second.path().join("inner.req"), "SECOND\n").unwrap();
    let paths = [first.path().to_path_buf(), second.path().to_path_buf()];
    let list = load_request_file_with_search_paths("top.req", &paths, &MacroContext::new())
        .await
        .unwrap();
    assert_eq!(list.entries[0].pv_name, "FIRST");
}

/// `concatenate_paths` puts an absolute name UNDER each path directory.
#[epics_macros_rs::epics_test]
async fn an_absolute_name_is_joined_under_each_path_directory() {
    // In the target dir, on the drive the test runs from: Windows opens the
    // drive-less name there.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let abs = dir.path().join("abs.req");
    std::fs::write(&abs, "ABS\n").unwrap();
    let path = tempfile::tempdir().unwrap();
    let ctx = MacroContext::new();
    let name = rooted(&abs);
    assert!(
        load_request_file_with_search_paths(&name, &[path.path().to_path_buf()], &ctx)
            .await
            .is_err()
    );
    let under = path.path().join(&name[1..]);
    std::fs::create_dir_all(under.parent().unwrap()).unwrap();
    std::fs::write(&under, "UNDER\n").unwrap();
    let list = load_request_file_with_search_paths(&name, &[path.path().to_path_buf()], &ctx)
        .await
        .unwrap();
    assert_eq!(list.entries[0].pv_name, "UNDER");
    // With no path list it is opened as named.
    let list = load_request_file_with_search_paths(&name, &[], &ctx)
        .await
        .unwrap();
    assert_eq!(list.entries[0].pv_name, "ABS");
}

/// `p` as a `/`-led name with no drive, absolute to C `isAbsolute` on
/// every OS, where a Windows `C:\` name would not join under a directory.
fn rooted(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    match s.as_bytes() {
        [_, b':', ..] => s[2..].to_string(),
        _ => s,
    }
}
