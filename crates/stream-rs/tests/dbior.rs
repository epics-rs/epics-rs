//! `dbior stream <interest>` against C StreamDevice:
//! `tests/oracle/dbior/dbior.out` is what C's `Stream::report` printed for
//! two records on bus "dummy" (see `tests/oracle/dbior/run.sh`). The one
//! difference is the version: C's names its StreamDevice checkout, rs's is
//! `STREAM_VERSION`.

#[path = "dbior/harness.rs"]
mod harness;

const DB: &str = r#"
record(ai, "R1") { field(DTYP, "stream") field(INP, "@p.proto get dummy") }
record(stringout, "R2") { field(DTYP, "stream") field(OUT, "@p.proto put dummy") }
"#;

#[test]
fn dbior_stream_matches_c() {
    epics_base_rs::runtime::task::test_block_on(async {
        let db = harness::boot(DB).await;
        harness::assert_levels(&db, "dbior.out", &["0", "1", "2", "3", "100"]).await;
    });
}
