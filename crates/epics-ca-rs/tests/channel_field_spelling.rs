//! A channel's field is found by its declared spelling only.
//!
//! C `dbFindFieldPart` (dbStaticLib.c:1817-1844) binary-searches the declared
//! names with `strncmp`, so softIoc R7.0.10 answers `caget -w 1 -t` with
//! `Channel connect timed out: 'P.val' not found.` for every name below, `$`
//! form included, while `P.VAL` and `S.VAL$` connect. The port's field
//! lookups are case-blind, so the search had to refuse the spelling itself.

#![cfg(tokio_backend)]

use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_ca_rs::server::CaServer;
use tokio::process::Command;

const CAGET: &str = env!("CARGO_BIN_EXE_caget-rs");

async fn caget(port: u16, wait: &str, name: &str) -> std::process::Output {
    Command::new(CAGET)
        .args(["-w", wait, "-t", name])
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .output()
        .await
        .expect("run caget-rs")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_field_spelled_in_lower_case_is_not_found() {
    let server = CaServer::builder()
        .port(0)
        .record("P", AiRecord::new(3.0))
        .record("S", StringinRecord::new("abc"))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });

    let bound = fact_budget::FACT_BUDGET.as_secs().to_string();
    let ok = caget(port, &bound, "P.VAL").await;
    assert_eq!(String::from_utf8_lossy(&ok.stdout), "3\n");

    let refused = ["P.val", "P.Val", "P.desc", "P.rtyp", "S.val$"];
    let outs =
        futures_util::future::join_all(refused.iter().map(|name| caget(port, "0.5", name))).await;
    for (name, out) in refused.iter().zip(outs) {
        assert_eq!(String::from_utf8_lossy(&out.stdout), "", "{name}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!("Channel connect timed out: '{name}' not found.\n"),
            "{name}"
        );
        assert_eq!(out.status.code(), Some(1), "{name}");
    }
}
