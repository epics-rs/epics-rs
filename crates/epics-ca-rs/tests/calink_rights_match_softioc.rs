//! A CA link's CP holder, processed by the port as softIoc processes it
//! across a rights change, measured side by side on one C server.

#![cfg(all(feature = "client-core", tokio_backend))]

use std::time::Duration;

use c_ca_tools::{require_tool, spawn_softioc_with_acf};

/// An IOC whose `H` holds a CP link to the server's `UP` and forward-links
/// the counter `CNT`, so every processing of `H` counts, a failed link read
/// included. Driven over stdin with `dbgf`.
struct LinkHolder {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    out: c_ca_tools::LineCollector,
    err: c_ca_tools::LineCollector,
}

impl Drop for LinkHolder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl LinkHolder {
    const DB: &str = "record(ai, \"H\") { field(INP, \"UP CP\") field(FLNK, \"CNT\") }\n\
                      record(calc, \"CNT\") { field(INPA, \"CNT NPP\") field(CALC, \"A+1\") }\n";

    fn spawn(mut cmd: std::process::Command, db: &std::path::Path, server_port: u16) -> Self {
        use std::process::Stdio;
        let mut child = cmd
            .arg("-d")
            .arg(db)
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CA_SERVER_PORT", server_port.to_string())
            .env("EPICS_CA_REPEATER_PORT", "5165")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the link-holder IOC");
        let stdin = child.stdin.take().unwrap();
        let out = c_ca_tools::LineCollector::spawn(child.stdout.take().unwrap());
        let err = c_ca_tools::LineCollector::spawn(child.stderr.take().unwrap());
        Self {
            child,
            stdin,
            out,
            err,
        }
    }

    /// `dbgf field`'s value, as the IOC printed it.
    fn dbgf(&mut self, field: &str) -> String {
        use std::io::Write as _;
        let answers = |t: &str| t.matches("DBF_").count();
        let before = answers(&self.out.text());
        writeln!(self.stdin, "dbgf {field}").expect("write to the IOC's stdin");
        assert!(
            self.out
                .wait_for(fact_budget::FACT_BUDGET, |t| answers(t) > before),
            "no answer to dbgf {field}:\n{}",
            self.out.text()
        );
        let text = self.out.text();
        let line = text.rsplit("DBF_").next().unwrap();
        let value = line.lines().next().unwrap().split_once(':').unwrap().1;
        value.trim().trim_matches('"').to_string()
    }

    fn console(&self) -> String {
        format!("{}{}", self.out.text(), self.err.text())
    }

    fn until(&mut self, field: &str, want: &str) -> String {
        let end = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        loop {
            let got = self.dbgf(field);
            if got == want || std::time::Instant::now() >= end {
                return got;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// rsrv sends ACCESS_RIGHTS before the CREATE_CHAN reply
/// (`camessage.c:1180`, `:1201`), so dbCa's `accessRightsCallback` finds
/// the channel unconnected and a read-only connect processes the CP
/// holder for its first value only; a later rights change does process
/// it. Both counts are softIoc's own, read off a C link-holder beside the
/// port's on the same server.
#[test]
#[ignore = "needs softIoc on PATH (EPICS base); runs in --profile interop"]
fn a_read_only_connect_processes_the_cp_holder_as_softioc_does() {
    require_tool("softIoc");
    let server = spawn_softioc_with_acf(
        "record(ao, \"SRC\") { field(VAL, \"5\") field(PINI, \"YES\") field(ASG, \"OPEN\") }\n\
         record(calc, \"UP\") { field(INPA, \"SRC CP\") field(CALC, \"A\") }\n\
         record(bo, \"GW\") { field(ASG, \"OPEN\") field(UDF, \"0\") }\n\
         record(bo, \"GR\") { field(ASG, \"OPEN\") field(UDF, \"0\") }\n",
        "ASG(DEFAULT) { INPA(\"GW\") INPB(\"GR\") \
         RULE(1, READ) { CALC(\"B=0\") } RULE(1, WRITE) { CALC(\"A=1\") } }\n\
         ASG(OPEN) { RULE(1, WRITE) }\n",
    );
    let put = |pv, value| {
        assert!(
            c_ca_tools::run_caput(&server.ca_addr_list(), server.udp_port, pv, value),
            "caput {pv} {value}:\n{}",
            server.console()
        );
    };
    // asCa's eventCallback updates the ACF inputs outside asLock, so an
    // initial event landing inside asComputeAllAsg has its inpChanged bit
    // cleared (asLibRoutines.c:966, :979) and READ stays denied until the
    // input posts again. Post each input after boot, when nothing else
    // evaluates the rules, and hold the holders until UP is readable. A bo
    // posts on a VAL change, so each goes 1 then back to 0: a repeated 0
    // posts only the first time, when STAT leaves UDF.
    for pv in ["GW", "GR"] {
        put(pv, "1");
        put(pv, "0");
    }
    let readable = || {
        c_ca_tools::run_caget(&server.ca_addr_list(), server.udp_port, "UP")
            .is_some_and(|v| !v.contains("no read access"))
    };
    let end = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while !readable() && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        readable(),
        "softIoc grants READ on UP:\n{}",
        server.console()
    );
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("holder.db");
    std::fs::write(&db, LinkHolder::DB).expect("write the holder db");

    // The holder's own RSRV takes a candidate number too, so a steal of it
    // is retried as the server's is, instead of suspending its iocInit.
    let c_holder = named_port::on_a_named_port(|port| {
        let mut cmd = std::process::Command::new("softIoc");
        cmd.env("EPICS_CAS_SERVER_PORT", port.to_string());
        let h = LinkHolder::spawn(cmd, &db, server.udp_port);
        let settled = h.err.wait_for(fact_budget::FACT_BUDGET, |t| {
            c_ca_tools::softioc_verdict(t) != c_ca_tools::SoftIocVerdict::Silent
        });
        assert!(settled, "the C link-holder did not boot:\n{}", h.console());
        match c_ca_tools::softioc_verdict(&h.err.text()) {
            c_ca_tools::SoftIocVerdict::Up => Some(h),
            _ => None,
        }
    });
    let mut rs_cmd = std::process::Command::new(env!("CARGO_BIN_EXE_softioc-rs"));
    rs_cmd.args(["--port", "0"]);
    let mut holders = [
        ("softIoc", c_holder),
        (
            "softioc-rs",
            LinkHolder::spawn(rs_cmd, &db, server.udp_port),
        ),
    ];

    // Connected read-only with the first value, then one more value: the
    // count is the connect's processings plus one.
    for (who, h) in &mut holders {
        let got = h.until("H", "5");
        assert_eq!(
            got,
            "5",
            "{who} connects:\n{}\nserver:\n{}",
            h.console(),
            server.console()
        );
    }
    put("SRC", "6");
    let counts = holders.each_mut().map(|(who, h)| {
        let got = h.until("H", "6");
        assert_eq!(got, "6", "{who} reads the second value:\n{}", h.console());
        h.dbgf("CNT")
    });
    assert_eq!(
        counts[1], counts[0],
        "softioc-rs processed H as often as softIoc over a read-only connect"
    );

    // Read lost while connected: the holder is processed and its link read
    // fails.
    put("GR", "1");
    for (who, h) in &mut holders {
        let got = h.until("H.SEVR", "INVALID");
        assert_eq!(got, "INVALID", "{who}:\n{}", h.console());
    }
}
