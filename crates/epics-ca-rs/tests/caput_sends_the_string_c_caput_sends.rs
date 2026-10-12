//! `CaClient::caput` puts what C `caput` puts (`caput.c:455-552`): the token
//! as a `DBR_STRING`, so the server's `epicsParse*` decides. An ENUM goes by
//! menu name, else as a number. Rows are softIoc R7.0.10-146-g8f5015b66 with
//! `caput` against `record(longout,"LO")`, `int64out`, `mbbo` (ZRST a, ONST b).
//!
//! The client used to parse into the field's width with `as`, so
//! `LO 2147483648` stored -2147483648 and `LO 1e3` was refused, where C
//! refuses the first and stores 1 for the second.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

mod common;

use epics_base_rs::server::records::int64out::Int64outRecord;
use epics_base_rs::server::records::longout::LongoutRecord;
use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn caput_leaves_the_parse_to_the_server() {
    let mut mbbo = MbboRecord::new(0);
    mbbo.zrst = "a".into();
    mbbo.onst = "b".into();
    mbbo.zrvl = 0;
    mbbo.onvl = 1;
    let server = CaServer::builder()
        .port(0)
        .record("LO", LongoutRecord::new(0))
        .record("I64O", Int64outRecord::new(0))
        .record("MBBO", mbbo)
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });
    // SAFETY: `#[serial(epics_env)]`, set before the client snapshots it.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new().await.expect("client");

    let get = |pv: &'static str| {
        let client = &client;
        async move {
            let ch = client.create_channel(pv);
            ch.wait_connected(fact_budget::FACT_BUDGET).await.unwrap();
            ch.get_with_timeout(fact_budget::FACT_BUDGET)
                .await
                .unwrap()
                .1
        }
    };

    // (pv, token, field afterwards); every put is preceded by 77.
    let rows: [(&str, &str, EpicsValue); 12] = [
        ("LO", "9223372036854775807", EpicsValue::Long(77)),
        ("LO", "2147483648", EpicsValue::Long(77)),
        ("LO", "-2147483649", EpicsValue::Long(77)),
        ("LO", "12abc", EpicsValue::Long(12)),
        ("LO", "1e3", EpicsValue::Long(1)),
        ("LO", "0x7fffffff", EpicsValue::Long(i32::MAX)),
        ("LO", "1.5", EpicsValue::Long(1)),
        ("I64O", "9223372036854775808", EpicsValue::Double(77.0)),
        (
            "I64O",
            "9223372036854775807",
            EpicsValue::Double(i64::MAX as f64),
        ),
        ("I64O", "18446744073709551615", EpicsValue::Double(77.0)),
        ("MBBO", "b", EpicsValue::Enum(1)),
        ("MBBO", "0", EpicsValue::Enum(0)),
    ];
    for (pv, token, want) in rows {
        let seed = if pv == "MBBO" { "1" } else { "77" };
        client.caput(pv, seed).await.unwrap();
        // The server may refuse; C `caput` prints the exception and moves on.
        let _ = client.caput_callback(pv, token, 2.0).await;
        assert_eq!(get(pv).await, want, "caput {pv} {token}");
    }
}
