//! `DTYP "General Time"` (devGeneralTime.c) on softioc-rs, against softIoc
//! R7.0.10 given the same db and script: the channel names match without
//! regard to case, a miss prints `Bad parm` and leaves the record dead, and
//! the stringin channels name the providers as C's generalTime does.

#![cfg(all(unix, tokio_backend))]

use std::process::{Command, Stdio};

const DB: &str = r#"record(ai, "A:LC") { field(DTYP, "General Time") field(INP, "@time") }
record(ai, "A:BAD") { field(DTYP, "General Time") field(INP, "@XX") }
record(ai, "A:EMPTY") { field(DTYP, "General Time") }
record(bo, "B:BAD") { field(DTYP, "General Time") field(OUT, "@X") }
record(longin, "L:CNT") { field(DTYP, "General Time") field(INP, "@GETERRCNT") }
record(longin, "L:BAD") { field(DTYP, "General Time") field(INP, "@X") }
record(stringin, "S:BEST") { field(DTYP, "General Time") field(INP, "@BESTTCP") }
record(stringin, "S:TOP") { field(DTYP, "General Time") field(INP, "@TOPTCP") }
record(stringin, "S:BAD") { field(DTYP, "General Time") field(INP, "@X") }
"#;

const SCRIPT: &str = "dbLoadRecords t.db
iocInit
dbpf A:LC.PROC 1
dbpf S:BEST.PROC 1
dbpf S:TOP.PROC 1
dbpf L:CNT.PROC 1
dbgf A:LC.UDF
dbgf A:LC.PACT
dbgf S:BEST
dbgf S:BEST.SEVR
dbgf S:TOP
dbgf L:CNT.UDF
dbgf S:BAD.PACT
";

/// softIoc's answer to each `dbgf`, in order.
const C_DBGF: &[&str] = &[
    "DBF_UCHAR:          0 = 0x0",
    "DBF_UCHAR:          0 = 0x0",
    "DBF_STRING:         \"No working providers\"",
    "DBF_STRING:         \"MAJOR\"",
    "DBF_STRING:         \"OS Clock\"",
    "DBF_UCHAR:          0 = 0x0",
    "DBF_UCHAR:          1 = 0x1",
];

const C_INIT_ERRORS: &[&str] = &[
    "recGblRecordError: devAiGeneralTime::init_ai: Bad parm Illegal field value PV: A:BAD",
    "recGblRecordError: devAiGeneralTime::init_ai: Bad parm Illegal field value PV: A:EMPTY",
    "recGblRecordError: devBoGeneralTime::init_bo: Bad parm Illegal field value PV: B:BAD",
    "recGblRecordError: devLiGeneralTime::init_li: Bad parm Illegal field value PV: L:BAD",
    "recGblRecordError: devSiGeneralTime::init_si: Bad parm Illegal field value PV: S:BAD",
];

#[test]
fn general_time_records_read_as_softioc_does() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("t.db"), DB).unwrap();
    std::fs::write(dir.path().join("st.cmd"), SCRIPT).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .current_dir(dir.path())
        .arg("st.cmd")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env(
            "EPICS_CA_SERVER_PORT",
            named_port::free_for_tcp_and_udp().to_string(),
        )
        .env(
            "EPICS_CA_REPEATER_PORT",
            named_port::free_for_tcp_and_udp().to_string(),
        )
        .stdin(Stdio::null())
        .output()
        .expect("softioc-rs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stdout.lines().collect();
    let dbgf: Vec<&str> = lines
        .windows(2)
        .filter(|w| w[0].contains("dbgf "))
        .map(|w| w[1].trim_end())
        .collect();
    assert_eq!(dbgf, C_DBGF, "{stdout}");
    let errors: Vec<&str> = stderr
        .lines()
        .chain(stdout.lines())
        .filter(|l| l.starts_with("recGblRecordError"))
        .collect();
    assert_eq!(errors, C_INIT_ERRORS, "{stderr}");
}
