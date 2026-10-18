//! A plain lsi VAL is the SIZV-wide `DBF_STRING` `cvt_dbaddr` makes it
//! (`lsiRecord.c:122-145`), read to its NUL; only `$` is `LEN` elements. A
//! device read that failed after scanning into VAL (devlsiStream.c) leaves
//! the NUL past `LEN`. streamApp R7.0.10.1-DEV, `lsi` SIZV 41 read "o" then
//! `%!9s` failing on "abcdef", and SIZV 100 the same on 50 `x`:
//!
//! ```text
//! caget A.VAL      A.VAL      abcdef
//! caget -S A.VAL$  A.VAL$ ab
//! caget B.VAL      B.VAL      xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
//! ```

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use epics_base_rs::server::records::lsi::LsiRecord;
use epics_base_rs::types::{DBR_CHAR, DBR_STRING};
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient, EnumReadback};
use epics_ca_rs::server::CaServer;

const DB: &str = r#"
record(lsi, "A") { field(SIZV, "41") }
record(lsi, "B") { field(SIZV, "100") }
"#;

async fn connect(client: &CaClient, name: &str) -> CaChannel {
    let ch = client.create_channel(name);
    ch.wait_connected(fact_budget::FACT_BUDGET)
        .await
        .unwrap_or_else(|e| panic!("{name} connects: {e:?}"));
    ch
}

fn text(name: &str, value: EpicsValue) -> String {
    match value {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{name}: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_get_and_monitor_read_val_past_len_to_its_nul() {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &Default::default())
        .expect("load db")
        .build()
        .await
        .expect("build CA server");
    let db = server.database().clone();
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });
    for (name, read) in [("A", b"abcdef".to_vec()), ("B", vec![b'x'; 50])] {
        let cell = db.get_record(name).unwrap();
        let mut inst = cell.write();
        let lsi = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<LsiRecord>())
            .unwrap();
        lsi.val.set_text(b"o");
        let sizv = lsi.sizv as usize;
        lsi.val.scan_in_place(sizv, |buf| {
            buf[..read.len()].copy_from_slice(&read);
            buf[read.len()] = 0;
            None
        });
    }
    // SAFETY: the only test in this binary; set before `CaClient::new()`.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new().await.expect("client");

    for (name, want) in [("A.VAL", "abcdef".to_string()), ("B.VAL", "x".repeat(39))] {
        let ch = connect(&client, name).await;
        let got = ch.get_with_dbr_type(DBR_STRING, 0).await.expect("get");
        assert_eq!(text(name, got.value), want, "caget {name}");
        let mut mon = ch
            .subscribe_with_mask_readback_count(0.0, 0x5, EnumReadback::Native, false, None)
            .await
            .expect("subscribe");
        let event = tokio::time::timeout(fact_budget::FACT_BUDGET, mon.recv())
            .await
            .expect("an event")
            .expect("monitor open")
            .expect("event");
        assert_eq!(text(name, event.value), want, "camonitor {name}");
    }

    let chars = connect(&client, "A.VAL$")
        .await
        .get_with_dbr_type(DBR_CHAR, 0)
        .await
        .expect("get A.VAL$");
    let (EpicsValue::CharArray(chars) | EpicsValue::UCharArray(chars)) = chars.value else {
        panic!("A.VAL$: {:?}", chars.value);
    };
    // caget A.VAL$ -> 41 97 98 0 0 ...
    let mut want = [0u8; 41];
    want[..2].copy_from_slice(b"ab");
    assert_eq!(&chars[..], &want[..], "caget A.VAL$");
}
