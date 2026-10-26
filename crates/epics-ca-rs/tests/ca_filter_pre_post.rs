//! A CA channel runs its filters in C's `dbChannel` order whatever the JSON
//! order: the pre chain (`dbnd`, `dec`, `sync`) on the post, the post chain
//! (`arr`) as the event is read (`dbEvent.c:901,923`, `:984-985`), and a `$`
//! channel's post chain on its `DBR_CHAR` image before `dbGet`'s cut.
//! softIoc R7.0.10 on the records below:
//!
//! ```text
//! $ caget 'FS.VAL${"arr":{"s":1,"e":3}}' 'FS.VAL${"arr":{"s":1,"e":1}}' \
//!         'FS.VAL${"arr":{"s":0,"e":5}}' 'FS.VAL${"arr":{"s":2}}'
//! FS.VAL${"arr":{"s":1,"e":3}} 3 101 108 0
//! FS.VAL${"arr":{"s":1,"e":1}}   101
//! FS.VAL${"arr":{"s":0,"e":5}} 6 104 101 108 108 111 0
//! FS.VAL${"arr":{"s":2}} 38 108 108 111 0 0 0 ... (38 elements)
//! $ caget -# 2 'FS.VAL${"arr":{"s":1,"e":3}}' 'FS.VAL$'
//! FS.VAL${"arr":{"s":1,"e":3}} 2 101 0
//! FS.VAL$ 2 104 0
//! $ caget -# 1 'FS.VAL${"arr":{"s":1,"e":3}}'
//! FS.VAL${"arr":{"s":1,"e":3}}   1 0
//! ```
//!
//! `camonitor -tn` on each channel, then `caput FS world` and
//! `caput -a W 3 1 2 3`, `3 1 2.2 3`, `3 1.5 2.7 3`:
//!
//! ```text
//! FS.VAL$  6 104 101 108 108 111 0 UDF NO_ALARM
//! FS.VAL$  6 119 111 114 108 100 0
//! FS.VAL${"arr":{"s":1,"e":3}}  3 101 108 0 UDF NO_ALARM
//! FS.VAL${"arr":{"s":1,"e":3}}  3 111 114 0
//! FS.VAL${"arr":{"s":1,"e":1}}    101 UDF NO_ALARM
//! FS.VAL${"arr":{"s":1,"e":1}}    111
//! W                               UDF INVALID
//! W  3 1 2 3
//! W  3 1 2.2 3
//! W  3 1.5 2.7 3
//! W.VAL{"arr":{"s":0,"e":0},"dbnd":{"d":5}}  UDF INVALID
//! W.VAL{"arr":{"s":0,"e":0},"dbnd":{"d":5}}  1
//! W.VAL{"arr":{"s":0,"e":0},"dbnd":{"d":5}}  1
//! W.VAL{"arr":{"s":0,"e":0},"dbnd":{"d":5}}  1.5
//! ```
//!
//! and the same for `W.VAL{"dbnd":{"d":5},"arr":{"s":0,"e":0}}`: `dbnd` runs
//! on the whole array, where it passes everything, never on `arr`'s element.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient, MonitorHandle};
use epics_ca_rs::server::CaServer;

const DB: &str = r#"
record(stringin, "FS") { field(VAL, "hello") }
record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "5") }
"#;

async fn connect(client: &CaClient, name: &str) -> CaChannel {
    let ch = client.create_channel(name);
    ch.wait_connected(fact_budget::FACT_BUDGET)
        .await
        .unwrap_or_else(|e| panic!("{name} connects: {e:?}"));
    ch
}

fn elements(name: &str, value: EpicsValue) -> Vec<f64> {
    match value {
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => {
            b.iter().map(|&c| f64::from(c)).collect()
        }
        EpicsValue::Char(c) | EpicsValue::UChar(c) => vec![f64::from(c)],
        EpicsValue::DoubleArray(v) => v.to_vec(),
        EpicsValue::Double(v) => vec![v],
        other => panic!("{name}: {other:?}"),
    }
}

async fn read(client: &CaClient, name: &str, count: u32) -> Vec<f64> {
    let (_, value) = connect(client, name)
        .await
        .get_with_timeout_count(fact_budget::FACT_BUDGET, count)
        .await
        .unwrap_or_else(|e| panic!("read {name}: {e:?}"));
    elements(name, value)
}

async fn monitor(client: &CaClient, name: &str) -> MonitorHandle {
    connect(client, name)
        .await
        .subscribe()
        .await
        .unwrap_or_else(|e| panic!("{name} subscribes: {e:?}"))
}

async fn next(name: &str, mon: &mut MonitorHandle) -> Vec<f64> {
    let snap = tokio::time::timeout(fact_budget::FACT_BUDGET, mon.recv())
        .await
        .unwrap_or_else(|_| panic!("{name}: no event"))
        .expect("monitor open")
        .expect("event");
    elements(name, snap.value)
}

fn chars(text: &[u8]) -> Vec<f64> {
    text.iter().map(|&c| f64::from(c)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filters_run_pre_then_post_on_every_ca_path() {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &Default::default())
        .expect("load db")
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

    const S1E3: &str = r#"FS.VAL${"arr":{"s":1,"e":3}}"#;
    const S1E1: &str = r#"FS.VAL${"arr":{"s":1,"e":1}}"#;
    assert_eq!(read(&client, S1E3, 0).await, chars(b"el\0"));
    assert_eq!(read(&client, S1E1, 0).await, chars(b"e"));
    assert_eq!(
        read(&client, r#"FS.VAL${"arr":{"s":0,"e":5}}"#, 0).await,
        chars(b"hello\0")
    );
    let mut tail = chars(b"llo");
    tail.resize(38, 0.0);
    assert_eq!(read(&client, r#"FS.VAL${"arr":{"s":2}}"#, 0).await, tail);
    assert_eq!(read(&client, S1E3, 2).await, chars(b"e\0"));
    assert_eq!(read(&client, "FS.VAL$", 2).await, chars(b"h\0"));
    assert_eq!(read(&client, S1E3, 1).await, chars(b"\0"));

    const W_ARR_DBND: &str = r#"W.VAL{"arr":{"s":0,"e":0},"dbnd":{"d":5}}"#;
    const W_DBND_ARR: &str = r#"W.VAL{"dbnd":{"d":5},"arr":{"s":0,"e":0}}"#;
    let mut mons = Vec::new();
    for name in ["FS.VAL$", S1E3, S1E1, "W", W_ARR_DBND, W_DBND_ARR] {
        let mut mon = monitor(&client, name).await;
        let first = next(name, &mut mon).await;
        mons.push((name, mon, first));
    }
    let first: Vec<_> = mons.iter().map(|(_, _, f)| f.clone()).collect();
    assert_eq!(
        first,
        [
            chars(b"hello\0"),
            chars(b"el\0"),
            chars(b"e"),
            vec![],
            vec![],
            vec![],
        ]
    );

    connect(&client, "FS")
        .await
        .put_with_timeout(
            &EpicsValue::String("world".into()),
            fact_budget::FACT_BUDGET,
        )
        .await
        .expect("put FS");
    for (name, mon, _) in &mut mons[..3] {
        let got = next(name, mon).await;
        let want = match *name {
            "FS.VAL$" => chars(b"world\0"),
            S1E3 => chars(b"or\0"),
            _ => chars(b"o"),
        };
        assert_eq!(got, want, "{name}");
    }

    let w = connect(&client, "W").await;
    for (wave, first) in [
        ([1.0, 2.0, 3.0], 1.0),
        ([1.0, 2.2, 3.0], 1.0),
        ([1.5, 2.7, 3.0], 1.5),
    ] {
        w.put_with_timeout(
            &EpicsValue::DoubleArray(wave.to_vec().into()),
            fact_budget::FACT_BUDGET,
        )
        .await
        .expect("put W");
        for (name, mon, _) in &mut mons[3..] {
            let want = if *name == "W" {
                wave.to_vec()
            } else {
                vec![first]
            };
            assert_eq!(next(name, mon).await, want, "{name}");
        }
    }
}
