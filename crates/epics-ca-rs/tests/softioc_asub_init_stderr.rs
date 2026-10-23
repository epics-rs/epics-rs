//! `iocInit 2> f`: aSubRecord.c's INAM/SNAM misses land in `f`, subRecord.c's
//! stay on the console. aSubRecord.c includes epicsStdio.h and subRecord.c
//! does not. Measured on softIoc R7.0.10.

#![cfg(all(unix, tokio_backend))]

use std::process::{Command, Stdio};

const DB: &str = r#"record(aSub, "A") { field(INAM, "noInit") }
record(aSub, "B") { field(SNAM, "noSnam") }
record(sub, "C") { field(INAM, "noInitC") }
"#;

#[test]
fn asub_misses_go_to_the_thread_stderr() {
    // A port taken before the IOC bound it is drawn again; the verdict reads
    // both streams, as the redirect splits iocInit's stderr from errlog's.
    let (file, console) = named_port::on_a_named_port(|port| {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("t.db"), DB).unwrap();
        std::fs::write(
            dir.path().join("st.cmd"),
            "dbLoadRecords t.db\niocInit 2> e.txt\n",
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
            .current_dir(dir.path())
            .arg("st.cmd")
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .env(
                "EPICS_CA_REPEATER_PORT",
                named_port::free_for_tcp_and_udp().to_string(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("softioc-rs");
        let file = std::fs::read_to_string(dir.path().join("e.txt")).unwrap_or_default();
        let console = String::from_utf8_lossy(&out.stderr).into_owned();
        let taken = named_port::verdict::rs_ioc_port_was_taken(&format!("{file}\n{console}"), port);
        (!taken).then_some((file, console))
    });
    let misses = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.contains("not found"))
            .map(|l| l.split(' ').next().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(misses(&file), ["A.INAM", "B.SNAM"], "{file:?}");
    assert_eq!(misses(&console), ["C.INAM"], "{console:?}");
}
