//! A CP or CPP link aimed at a record of the same IOC is a CA link
//! (`dbLink.c:118-127`): softIoc and softioc-rs, fed one database and one
//! console script, agree on every holder's processing, its alarm and the
//! `dbcar` count, and the PP/NPP input links stay database links.

#![cfg(tokio_backend)]

use c_ca_tools::{LineCollector, require_tool};

/// `HCPPS` precedes `HCP`: dbCa's subscriptions on `SRC` are posted to its
/// one event queue in link order, and the CPP-or-not decision is taken in
/// that callback, so `HCP` processed means `HCPPS`'s event was decided. The
/// port's pass dispatches `SRC`'s holders in the same order.
const DB: &str = "\
record(ao, \"SRC\") { field(HIGH, \"5\") field(HSV, \"MAJOR\") }
record(calc, \"HCPPS\") { field(INPA, \"SRC CPP\") field(SCAN, \"Event\") field(EVNT, \"77\") field(CALC, \"VAL+1\") }
record(calc, \"HCPP\") { field(INPA, \"SRC CPP\") field(CALC, \"VAL+1\") }
record(calc, \"HCPS\") { field(INPA, \"SRC CP\") field(SCAN, \"Event\") field(EVNT, \"77\") field(CALC, \"VAL+1\") }
record(calc, \"HMS\") { field(INPA, \"SRC CP MS\") field(CALC, \"A\") }
record(calc, \"HCP\") { field(INPA, \"SRC CP\") field(CALC, \"VAL+1\") }
record(calc, \"HPP\") { field(INPA, \"SRC PP\") field(CALC, \"A\") }
record(calc, \"HNPP\") { field(INPA, \"SRC NPP\") field(CALC, \"A\") }
";

struct Ioc {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    out: LineCollector,
    err: LineCollector,
    marks: usize,
}

impl Drop for Ioc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Ioc {
    fn spawn(mut cmd: std::process::Command, db: &std::path::Path) -> Self {
        use std::process::Stdio;
        let mut child = cmd
            .arg("-d")
            .arg(db)
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
            // Nothing answers there: a local link must not need a server.
            .env("EPICS_CA_ADDR_LIST", "192.0.2.1")
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the IOC");
        let stdin = child.stdin.take().unwrap();
        let out = LineCollector::spawn(child.stdout.take().unwrap());
        let err = LineCollector::spawn(child.stderr.take().unwrap());
        Self {
            child,
            stdin,
            out,
            err,
            marks: 0,
        }
    }

    fn console(&self) -> String {
        format!("{}{}", self.out.text(), self.err.text())
    }

    /// What `line` printed: the stdout between it and an `echo` mark.
    fn run(&mut self, line: &str) -> String {
        use std::io::Write as _;
        self.marks += 1;
        let mark = format!("@@mark{}@@", self.marks);
        let start = self.out.text().len();
        writeln!(self.stdin, "{line}\necho {mark}").expect("write to the IOC's stdin");
        assert!(
            self.out
                .wait_for(fact_budget::FACT_BUDGET, |t| t[start..].contains(&mark)),
            "no answer to {line}:\n{}",
            self.console()
        );
        let text = self.out.text()[start..].to_string();
        let body = text.split_once(&mark).unwrap().0;
        body.lines()
            .map(|l| l.rsplit_once("epics> ").map_or(l, |(_, rest)| rest))
            .map(|l| l.trim_start_matches("\u{1b}[0m").trim_end())
            .filter(|l| !l.is_empty() && !l.starts_with("echo "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn dbgf(&mut self, field: &str) -> String {
        let answer = self.run(&format!("dbgf {field}"));
        let (_, value) = answer
            .rsplit_once("DBF_")
            .and_then(|(_, v)| v.split_once(':'))
            .unwrap_or_else(|| panic!("dbgf {field}: {answer}"));
        value.trim().trim_matches('"').to_string()
    }

    fn until(&mut self, field: &str, want: &str) {
        let end = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        loop {
            let got = self.dbgf(field);
            if got == want {
                return;
            }
            assert!(
                std::time::Instant::now() < end,
                "{field} stays {got}, want {want}:\n{}",
                self.console()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Every holder's count, value and alarm, in one string to compare.
    fn state(&mut self) -> String {
        let mut s = String::new();
        for rec in ["HCPPS", "HCPP", "HCPS", "HMS", "HCP", "HPP", "HNPP"] {
            for f in ["VAL", "SEVR", "STAT", "AMSG"] {
                let field = format!("{rec}.{f}");
                s += &format!("{field} {}\n", self.dbgf(&field));
            }
        }
        s
    }
}

/// The script, run on each IOC; what it observes is compared.
fn drive(ioc: &mut Ioc) -> Vec<(&'static str, String)> {
    let mut seen = Vec::new();
    // dbCaRun: every CP holder, and the Passive CPP one, processed once.
    ioc.until("HCP", "1");
    seen.push(("connect", ioc.state()));
    seen.push(("dbcar", ioc.run("dbcar")));
    ioc.run("dbpf SRC 2");
    ioc.until("HCP", "2");
    seen.push(("SRC 2", ioc.state()));
    ioc.run("dbpf SRC 7");
    ioc.until("HCP", "3");
    seen.push(("SRC 7 is HIGH", ioc.state()));
    // An input PP link processes its Passive source first; NPP does not.
    ioc.run("dbpf SRC.VAL 3");
    ioc.until("HCP", "4");
    ioc.run("dbpf HPP.PROC 1");
    ioc.run("dbpf HNPP.PROC 1");
    seen.push(("PP and NPP", ioc.state()));
    seen.push(("dbcar after", ioc.run("dbcar")));
    seen
}

#[test]
#[ignore = "needs softIoc on PATH (EPICS base); runs in --profile interop"]
fn local_cp_and_cpp_links_are_ca_links_as_in_softioc() {
    require_tool("softIoc");
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("local_cp.db");
    std::fs::write(&db, DB).expect("write the db");

    let mut c = named_port::on_a_named_port(|port| {
        let mut cmd = std::process::Command::new("softIoc");
        cmd.env("EPICS_CAS_SERVER_PORT", port.to_string());
        let ioc = Ioc::spawn(cmd, &db);
        let settled = ioc.err.wait_for(fact_budget::FACT_BUDGET, |t| {
            c_ca_tools::softioc_verdict(t) != c_ca_tools::SoftIocVerdict::Silent
        });
        assert!(settled, "softIoc did not boot:\n{}", ioc.console());
        match c_ca_tools::softioc_verdict(&ioc.err.text()) {
            c_ca_tools::SoftIocVerdict::Up => Some(ioc),
            _ => None,
        }
    });
    let mut rs_cmd = std::process::Command::new(env!("CARGO_BIN_EXE_softioc-rs"));
    rs_cmd.args(["--port", "0"]);
    let mut rs = Ioc::spawn(rs_cmd, &db);

    let want = drive(&mut c);
    let got = drive(&mut rs);
    for ((step, w), (_, g)) in want.iter().zip(&got) {
        assert_eq!(g, w, "softioc-rs after {step}, softIoc on the left");
    }
    // The values the comparison rests on, so a pair of equally wrong IOCs
    // does not pass.
    let connect = &want[0].1;
    for line in [
        "HCPPS.VAL 0",
        "HCPP.VAL 1",
        "HCPS.VAL 1",
        "HCP.VAL 1",
        "HCP.SEVR NO_ALARM",
    ] {
        assert!(connect.contains(line), "softIoc at connect: {connect}");
    }
    assert!(
        want[1].1.contains("Total 5 CA links; 5 connected"),
        "softIoc dbcar: {}",
        want[1].1
    );
    let high = &want[3].1;
    assert!(high.contains("HMS.SEVR MAJOR"), "softIoc: {high}");
    assert!(high.contains("HCPPS.VAL 0"), "softIoc: {high}");
}
