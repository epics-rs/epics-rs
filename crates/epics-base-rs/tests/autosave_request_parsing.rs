use std::path::PathBuf;

use epics_base_rs::server::autosave::error::AutosaveError;
use epics_base_rs::server::autosave::macros::MacroContext;
use epics_base_rs::server::autosave::request::{self, dedup_entries, load_request_file, pv_names};

#[test]
fn test_simple_pvs() {
    let content = "TEMP.VAL\nPRESS.VAL\nSWITCH.VAL\n";
    let ctx = MacroContext::new();
    let entries = request::parse_request_string(content, &ctx, "test.req").unwrap();
    assert_eq!(
        pv_names(&entries),
        vec!["TEMP.VAL", "PRESS.VAL", "SWITCH.VAL"]
    );
}

#[test]
fn test_macros_in_pv() {
    let content = "$(P)temp.VAL\n$(P)press.VAL\n";
    let ctx = MacroContext::from_map([("P".into(), "IOC:".into())].into());
    let entries = request::parse_request_string(content, &ctx, "test.req").unwrap();
    assert_eq!(pv_names(&entries), vec!["IOC:temp.VAL", "IOC:press.VAL"]);
    assert_eq!(entries[0].expanded_from.as_deref(), Some("$(P)temp.VAL"));
}

#[tokio::test]
async fn test_file_include_relative_path() {
    let dir = tempfile::tempdir().unwrap();
    let sub_dir = dir.path().join("sub");
    std::fs::create_dir(&sub_dir).unwrap();

    // Write included file
    std::fs::write(sub_dir.join("inner.req"), "INNER_PV1\nINNER_PV2\n").unwrap();

    // Write main file
    let main_req = dir.path().join("main.req");
    std::fs::write(&main_req, "OUTER_PV\nfile sub/inner.req\nOUTER_PV2\n").unwrap();

    let ctx = MacroContext::new();
    let entries = load_request_file(&main_req, &ctx).await.unwrap();
    let names = pv_names(&entries);
    assert_eq!(
        names,
        vec!["OUTER_PV", "INNER_PV1", "INNER_PV2", "OUTER_PV2"]
    );
}

#[tokio::test]
async fn test_nested_include() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(dir.path().join("level2.req"), "LEVEL2_PV\n").unwrap();
    std::fs::write(
        dir.path().join("level1.req"),
        "LEVEL1_PV\nfile level2.req\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("main.req"), "MAIN_PV\nfile level1.req\n").unwrap();

    let ctx = MacroContext::new();
    let entries = load_request_file(&dir.path().join("main.req"), &ctx)
        .await
        .unwrap();
    let names = pv_names(&entries);
    assert_eq!(names, vec!["MAIN_PV", "LEVEL1_PV", "LEVEL2_PV"]);
}

#[tokio::test]
async fn test_depth_limit() {
    let dir = tempfile::tempdir().unwrap();

    // Create a chain of includes deeper than MAX_INCLUDE_DEPTH
    for i in 0..12 {
        let content = if i < 11 {
            format!("PV_{i}\nfile level{}.req\n", i + 1)
        } else {
            format!("PV_{i}\n")
        };
        std::fs::write(dir.path().join(format!("level{i}.req")), content).unwrap();
    }

    let ctx = MacroContext::new();
    let result = load_request_file(&dir.path().join("level0.req"), &ctx).await;
    assert!(matches!(
        result,
        Err(AutosaveError::IncludeDepthExceeded(_))
    ));
}

#[tokio::test]
async fn test_circular_include() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(dir.path().join("a.req"), "PV_A\nfile b.req\n").unwrap();
    std::fs::write(dir.path().join("b.req"), "PV_B\nfile a.req\n").unwrap();

    let ctx = MacroContext::new();
    let result = load_request_file(&dir.path().join("a.req"), &ctx).await;
    assert!(matches!(result, Err(AutosaveError::IncludeCycle { .. })));
}

#[test]
fn test_comments_and_blanks() {
    let content = "# Comment\n\nPV1\n  # Another comment\n  \nPV2\n";
    let ctx = MacroContext::new();
    let entries = request::parse_request_string(content, &ctx, "test.req").unwrap();
    assert_eq!(pv_names(&entries), vec!["PV1", "PV2"]);
}

#[test]
fn test_duplicate_pv_dedup() {
    let content = "PV1\nPV2\nPV1\nPV3\n";
    let ctx = MacroContext::new();
    let entries = request::parse_request_string(content, &ctx, "test.req").unwrap();
    let deduped = dedup_entries(entries);
    let names = pv_names(&deduped);
    // PV1 last occurrence wins, so it should appear where the last one was but in order
    assert_eq!(names, vec!["PV2", "PV1", "PV3"]);
}

#[test]
fn test_request_entry_source_info() {
    let content = "PV1\nPV2\nPV3\n";
    let ctx = MacroContext::new();
    let entries = request::parse_request_string(content, &ctx, "myfile.req").unwrap();
    assert_eq!(entries[0].source_file, PathBuf::from("myfile.req"));
    assert_eq!(entries[0].line_no, 1);
    assert_eq!(entries[1].line_no, 2);
    assert_eq!(entries[2].line_no, 3);
}

#[tokio::test]
async fn test_include_with_macros() {
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(dir.path().join("inner.req"), "$(P)pv1\n$(P)pv2\n").unwrap();
    std::fs::write(dir.path().join("main.req"), "file inner.req P=TEST:\n").unwrap();

    let ctx = MacroContext::new();
    let entries = load_request_file(&dir.path().join("main.req"), &ctx)
        .await
        .unwrap();
    let names = pv_names(&entries);
    assert_eq!(names, vec!["TEST:pv1", "TEST:pv2"]);
}
