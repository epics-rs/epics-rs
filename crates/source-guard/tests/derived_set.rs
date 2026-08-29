//! `sweep` is default-in: a file added to a module is swept on the commit that
//! adds it, and an exemption that no longer names a file is an error rather
//! than a line that quietly removes nothing.

use source_guard::sweep;
use std::fs;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("nested")).expect("create scratch module");
    fs::write(dir.join("a.rs"), "fn a() {}\n").expect("write a.rs");
    fs::write(dir.join("b.rs"), "fn b() {}\n").expect("write b.rs");
    fs::write(dir.join("nested/c.rs"), "fn c() {}\n").expect("write c.rs");
    fs::write(dir.join("notes.txt"), "not rust\n").expect("write notes.txt");
    dir
}

#[test]
fn every_rust_file_in_the_module_is_swept_unless_named() {
    let dir = scratch("swept");
    let labels: Vec<&str> = sweep(&dir, &["b.rs"]).into_iter().map(|(l, _)| l).collect();
    assert_eq!(labels, ["a.rs", "nested/c.rs"]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_exemption_that_names_no_file_is_an_error() {
    let dir = scratch("stale");
    let err = std::panic::catch_unwind(|| sweep(&dir, &["renamed.rs"]))
        .expect_err("a stale exemption must not pass silently");
    let msg = err
        .downcast_ref::<String>()
        .expect("panic carries a message");
    assert!(msg.contains("renamed.rs"), "{msg}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_module_directory_with_no_rust_file_is_an_error() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("hollow");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch module");
    fs::write(dir.join("notes.txt"), "not rust\n").expect("write notes.txt");
    let err = std::panic::catch_unwind(|| sweep(&dir, &[]))
        .expect_err("an empty subject set must not pass silently");
    let msg = err
        .downcast_ref::<String>()
        .expect("panic carries a message");
    assert!(msg.contains("holds no .rs file"), "{msg}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn exempting_the_whole_module_is_an_error() {
    let dir = scratch("all_exempt");
    let err = std::panic::catch_unwind(|| sweep(&dir, &["a.rs", "b.rs", "nested/c.rs"]))
        .expect_err("an exemption list covering everything must not pass silently");
    let msg = err
        .downcast_ref::<String>()
        .expect("panic carries a message");
    assert!(msg.contains("is exempted"), "{msg}");
    let _ = fs::remove_dir_all(&dir);
}
