//! A `{pva:{...}}` member pvxs's callback for that value kind does not take
//! prints pvxs's `pvxs.ioc.link.parse` warning, once per link, and the link
//! keeps the key's earlier value (softIocPVX 1.5.3, `ioc/pvalink_jlif.cpp`).
//! Exactly once is the contract: softIocPVX's second copy is its load-time
//! validation parse running the same callbacks, not a rule about warnings.
//!
//! Tokio only: the pvalink runtime is.
#![cfg(all(feature = "pvalink", tokio_backend, unix))]

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::runtime::log::errlog_flush;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_bridge_rs::pvalink::install_pvalink_resolver;

const DB: &str = r#"
record(ai, "R:PV3") { field(INP, {pva:{pv:"T:A",pv:3}}) }
record(ai, "R:PROC") { field(INP, {pva:{pv:"T:B",proc:"CP",proc:"XX"}}) }
record(ai, "R:Q") { field(INP, {pva:{pv:"T:C",Q:3,Q:"a"}}) }
record(ai, "R:PVI") { field(INP, {pva:{pv:3}}) }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn ignored_pva_link_member_prints_pvxs_warning() {
    let db: Arc<PvDatabase> = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: fd 2 is process-global; this binary holds one test.
    let saved_err = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let _resolver = install_pvalink_resolver(&db).await;
    errlog_flush();
    // SAFETY: restoring what this test replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
    }
    let err = std::fs::read_to_string(err_sink.path()).expect("stderr capture");
    // `YYYY-MM-DDTHH:MM:SS.nnnnnnnnn WARN …` → `<ts> WARN …`.
    let mut lines: Vec<String> = err
        .lines()
        .map(|l| match l.split_once(' ') {
            Some((ts, rest)) if ts.len() == 29 && ts.as_bytes()[10] == b'T' => {
                format!("<ts> {rest}")
            }
            _ => l.to_string(),
        })
        .collect();
    let mut want: Vec<String> = [
        r#"unknown integer depth=1 key="pv" value=3"#,
        r#"unknown integer depth=1 key="pv" value=3"#,
        r#"unknown proc depth=1 key="proc" value="XX""#,
        r#"unknown string depth=1 key="Q" value="a""#,
    ]
    .iter()
    .flat_map(|m| {
        [
            "<ts> WARN pvxs.ioc.link.parse".to_string(),
            format!("     pva link parsing {m}"),
        ]
    })
    .collect();
    lines.sort_unstable();
    want.sort_unstable();
    assert_eq!(lines, want, "{err:?}");
}
