//! `caput LO <text>` against a `longout` holding 77, as C caput prints it
//! (softIoc R7.0.10-146-g8f5015b66, `caput` from the same build): the token
//! goes as `DBR_STRING`, so the server's `epicsParse*` decides.
//!
//! ```text
//! caput LO 2147483648   Old : LO 77 / New : LO 77, CA.Client.Exception block
//! caput LO 12abc        Old : LO 77 / New : LO 12
//! caput LO 1e3          Old : LO 77 / New : LO 1
//! ```
//!
//! The block's `ctx=` is rsrv's `RECORD_NAME`: `caput LO.VAL ...` prints
//! `channel=LO.VAL` and `ctx="LO"`.

#![cfg(tokio_backend)]

use std::process::Command;

use epics_base_rs::server::records::longout::LongoutRecord;
use epics_ca_rs::server::CaServer;
use serial_test::serial;

async fn caput(port: u16, args: &[&str]) -> (String, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_caput-rs"))
            .arg("-w")
            .arg(fact_budget::FACT_BUDGET.as_secs().to_string())
            .args(&args)
            .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .output()
            .expect("spawn caput-rs")
    })
    .await
    .expect("caput-rs child joined");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn caput_prints_the_old_and_new_lines_c_caput_prints_for_a_string_put() {
    let server = CaServer::builder()
        .port(0)
        .record("LO", LongoutRecord::new(0))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });

    // (token, New line, whether libca's exception block is printed)
    for (token, new, refused) in [
        ("2147483648", "New : LO 77", true),
        ("-2147483649", "New : LO 77", true),
        ("9223372036854775807", "New : LO 77", true),
        ("12abc", "New : LO 12", false),
        ("1e3", "New : LO 1", false),
        ("0x7fffffff", "New : LO 2147483647", false),
    ] {
        caput(port, &["LO", "77"]).await;
        let (out, err) = caput(port, &["LO", token]).await;
        // The name is padded to a column, as in C; the rows compare with
        // spaces collapsed, the way the softIoc transcript was taken.
        let lines: Vec<String> = out
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(
            lines,
            ["Old : LO 77", new],
            "caput LO {token}: {out:?} {err:?}"
        );
        if refused {
            assert!(
                err.contains("Warning: \"Channel write request failed\""),
                "caput LO {token}: {err:?}"
            );
            assert!(
                err.lines().any(|l| l
                    == "    Context: \"op=1, channel=LO, type=DBR_STRING, count=1, ctx=\"LO\"\""),
                "caput LO {token}: {err:?}"
            );
        } else {
            assert!(
                !err.contains("CA.Client.Exception"),
                "caput LO {token}: {err:?}"
            );
        }
    }
}

/// The channel name is what the client opened; `ctx=` is the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_exception_ctx_is_the_record_name_whatever_the_channel_name() {
    let server = CaServer::builder()
        .port(0)
        .record("LO", LongoutRecord::new(0))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });

    let (_out, err) = caput(port, &["LO.VAL", "2147483648"]).await;
    assert!(
        err.lines()
            .any(|l| l
                == "    Context: \"op=1, channel=LO.VAL, type=DBR_STRING, count=1, ctx=\"LO\"\""),
        "{err:?}"
    );
}
