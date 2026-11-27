//! What `caget` and `caput` print when access security or the record refuses
//! them, measured against C's tools on a C softIoc with the same `.db`/`.acf`.
//!
//! `caput` reports a put refused at issue as `ERROR from put operation:` and
//! a put that failed at completion as `ERROR occurred writing data:`, both
//! with `ca_message` text (`caput.c:552-553`, `:573-574`); those lines match C.
//!
//! Where C also says `Read operation timed out` for a read that was refused,
//! nothing timed out: libca never uncounts a refused get's IO
//! (`getCopy.cpp:44`) and `caget -c` counts only successful reads
//! (`caget.c:126-139`). The port marks the PV on stdout and does not repeat
//! that line.

#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::process::Output;

use epics_base_rs::server::access_security::parse_acf;
use epics_ca_rs::server::CaServer;
use tokio::process::Command;

const DB: &str = r#"
record(ao, "DENY:RO") { field(ASG, "RO") }
record(ao, "DENY:NONE") { field(ASG, "NONE") }
record(mbbo, "DENY:ENUM") { field(ASG, "NONE") field(ZRST, "Zero") field(ONST, "One") }
record(ao, "DENY:OK") {}
record(ai, "DENY:AI") {}
"#;

const ACF: &str = r#"
ASG(DEFAULT) { RULE(1, WRITE) }
ASG(RO) { RULE(1, READ) }
ASG(NONE) { RULE(1, NONE) }
"#;

/// Serve `DB` under `ACF` and return the UDP port the server bound.
async fn serve() -> u16 {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &HashMap::new())
        .expect("load db")
        .acf(parse_acf(ACF).expect("parse acf"))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });
    port
}

async fn run(bin: &str, port: u16, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .output()
        .await
        .expect("run tool")
}

fn text(out: &Output) -> (String, String, Option<i32>) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

/// A connected PV whose read is denied: marked on stdout beside the readable
/// PV, exit 0 as in C, and no timeout claim on stderr.
#[tokio::test(flavor = "multi_thread")]
async fn caget_marks_a_denied_read_without_claiming_a_timeout() {
    let port = serve().await;
    let caget = env!("CARGO_BIN_EXE_caget-rs");

    for mode in [&[][..], &["-c"][..]] {
        let args = [mode, &["DENY:NONE", "DENY:OK"]].concat();
        let (stdout, stderr, code) = text(&run(caget, port, &args).await);
        assert_eq!(
            stdout,
            "DENY:NONE                      *** no read access\n\
             DENY:OK                        0\n",
            "caget {args:?}"
        );
        assert_eq!(stderr, "", "caget {args:?}");
        assert_eq!(code, Some(0), "caget {args:?}");
    }
}

/// A put the client refuses from the channel's access rights: C's line with
/// `ca_message(ECA_NOWTACCESS)` and exit 1, with or without `-c`. stderr is a
/// pipe here, so the ERROR tag carries no colour escapes.
#[tokio::test(flavor = "multi_thread")]
async fn caput_reports_a_refused_put_with_the_status_text() {
    let port = serve().await;
    let caput = env!("CARGO_BIN_EXE_caput-rs");
    let refused = "ERROR from put operation: Write access denied\n";

    for mode in [&[][..], &["-c"][..]] {
        let args = [mode, &["DENY:RO", "1"]].concat();
        let (stdout, stderr, code) = text(&run(caput, port, &args).await);
        assert_eq!(
            stdout, "Old : DENY:RO                        0\n",
            "{args:?}"
        );
        assert_eq!(stderr, refused, "caput {args:?}");
        assert_eq!(code, Some(1), "caput {args:?}");
    }

    // No access at all: the Old read is marked on stdout, then the put is
    // refused.
    let (stdout, stderr, code) = text(&run(caput, port, &["DENY:NONE", "1"]).await);
    assert_eq!(
        stdout,
        "Old : DENY:NONE                      *** no read access\n"
    );
    assert_eq!(stderr, refused);
    assert_eq!(code, Some(1));
}

/// A put the server accepted and then failed: `-c` waits for the completion
/// and C prints its status under the other ERROR line.
#[tokio::test(flavor = "multi_thread")]
async fn caput_reports_a_failed_completion_with_the_status_text() {
    let port = serve().await;
    let caput = env!("CARGO_BIN_EXE_caput-rs");

    let (stdout, stderr, code) = text(&run(caput, port, &["-c", "DENY:AI.PHAS", "32768"]).await);
    assert_eq!(stdout, "Old : DENY:AI.PHAS                   0\n");
    assert_eq!(
        stderr,
        "ERROR occurred writing data: Channel write request failed\n"
    );
    assert_eq!(code, Some(1));
}

/// An ENUM PV whose menu cannot be read: exit 1 before printing or writing
/// anything, as in C, naming the denial rather than a timeout.
#[tokio::test(flavor = "multi_thread")]
async fn caput_enum_menu_read_denied_names_the_denial() {
    let port = serve().await;
    let caput = env!("CARGO_BIN_EXE_caput-rs");

    let (stdout, stderr, code) = text(&run(caput, port, &["DENY:ENUM", "One"]).await);
    assert_eq!(stdout, "");
    assert_eq!(stderr, "error: Read access denied\n");
    assert_eq!(code, Some(1));
}
