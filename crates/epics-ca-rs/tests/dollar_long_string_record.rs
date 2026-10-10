//! `REC.VAL$` on a long-string record, and `$` on a link, served as C's
//! `DBR_CHAR` address of the field (`dbAccess.c:682-701`). Oracle — softIoc
//! R7.0.10, `record(lso,"LSO"){field(SIZV,"20")}`,
//! `record(stringin,"SI"){field(VAL,"hello")}`, `record(ai,"AI"){field(INP,"SI")}`:
//!
//! ```text
//! cainfo LSO.VAL$                     -> DBF_CHAR, Element count: 20
//! caput -S LSO.VAL$ <39 chars>        -> Invalid element count requested
//! caput -S LSO.VAL$ ABCDEFGHIJKLMNOPQRS  -> VAL ABCDEFGHIJKLMNOPQRS, LEN 20
//! caput -a LSO.VAL$ 20 65 .. 84       -> VAL ABCDEFGHIJKLMNOPQRS, LEN 20
//! caput -a LSO.VAL$ 3 120 121 122     -> VAL xyz, LEN 4
//! caput -a LSO.VAL$ 1 65              -> VAL A, LEN 2
//! cainfo LSO                          -> DBF_STRING, Element count: 1
//! caput -S LI.VAL$ <39|40|41|49 chars> -> LEN 40|41|42|50   (lsi, SIZV 50)
//! cainfo AI.INP$                      -> DBF_CHAR, Element count: 1024
//! caput -S AI.INP$ 2.5                -> AI.INP 2.5
//! caget AI.VAL$                       -> not found
//! ```

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

#[path = "common/budget.rs"]
mod budget;

use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::lsi::LsiRecord;
use epics_base_rs::server::records::lso::LsoRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::{DBR_CHAR, DbFieldType};
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient};
use epics_ca_rs::server::CaServer;

async fn connect(client: &CaClient, name: &str) -> CaChannel {
    let ch = client.create_channel(name);
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .unwrap_or_else(|e| panic!("{name} connects: {e:?}"));
    ch
}

async fn read(client: &CaClient, name: &str) -> EpicsValue {
    connect(client, name).await.get().await.expect(name).1
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dollar_channels_serve_the_dbr_char_address() {
    let mut lso = LsoRecord::default();
    lso.sizv = 20;
    let mut li = LsiRecord::default();
    li.sizv = 50;
    let server = CaServer::builder()
        .port(0)
        .record("LSO", lso)
        .record("LI", li)
        .record("SI", StringinRecord::new("hello"))
        .record("AI", AiRecord::new(0.0))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });
    // SAFETY: the only test in this binary; set before `CaClient::new()`.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new().await.expect("client");

    let whole = connect(&client, "LSO").await;
    assert_eq!(whole.native_field_type().unwrap(), DbFieldType::String);
    assert_eq!(whole.element_count().unwrap(), 1, "cvt_dbaddr no_elements");

    let val = connect(&client, "LSO.VAL$").await;
    assert_eq!(val.native_field_type().unwrap(), DbFieldType::Char);
    assert_eq!(val.element_count().unwrap(), 20, "no_elements = SIZV");

    let chars = |s: &[u8]| EpicsValue::CharArray(s.to_vec().into());
    for (written, want_val, want_len) in [
        (&b"ABCDEFGHIJKLMNOPQRS\0"[..], "ABCDEFGHIJKLMNOPQRS", 20.0),
        (&b"ABCDEFGHIJKLMNOPQRST"[..], "ABCDEFGHIJKLMNOPQRS", 20.0),
        (&b"xyz"[..], "xyz", 4.0),
        (&b"A"[..], "A", 2.0),
    ] {
        val.put(&chars(written)).await.expect("put LSO.VAL$");
        assert_eq!(
            read(&client, "LSO.VAL").await,
            EpicsValue::String(want_val.into()),
            "LSO.VAL after a {}-element put",
            written.len()
        );
        assert_eq!(
            read(&client, "LSO.LEN").await,
            EpicsValue::Double(want_len),
            "LSO.LEN after a {}-element put",
            written.len()
        );
    }
    let snap = val.get_with_dbr_type(DBR_CHAR, 20).await.expect("get");
    let mut want = b"A".to_vec();
    want.resize(20, 0);
    assert_eq!(snap.value, EpicsValue::UCharArray(want.into()));

    // A `DBR_STRING`'s 40 does not bound a `$` put; SIZV does.
    let li = connect(&client, "LI.VAL$").await;
    let text: Vec<u8> = (b'a'..=b'z').chain(b'A'..=b'Z').collect();
    for n in [39, 40, 41, 49] {
        let mut written = text[..n].to_vec();
        written.push(0);
        li.put(&chars(&written)).await.expect("put LI.VAL$");
        assert_eq!(
            read(&client, "LI.LEN").await,
            EpicsValue::Double(n as f64 + 1.0),
            "LI.LEN after a {n}-char put"
        );
    }

    connect(&client, "AI.INP")
        .await
        .put_string("SI")
        .await
        .expect("put AI.INP");
    let inp = connect(&client, "AI.INP$").await;
    assert_eq!(inp.element_count().unwrap(), 1024, "PVLINK_STRINGSZ");
    inp.put(&chars(b"2.5\0")).await.expect("put AI.INP$");
    assert_eq!(
        read(&client, "AI.INP").await,
        EpicsValue::String("2.5".into())
    );

    let refused = client.create_channel("AI.VAL$");
    assert!(
        refused
            .wait_connected(std::time::Duration::from_millis(500))
            .await
            .is_err(),
        "`$` on a DBF_DOUBLE is S_dbLib_fieldNotFound"
    );
}
