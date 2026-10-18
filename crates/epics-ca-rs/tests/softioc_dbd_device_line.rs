//! A `device()` line in a `.dbd` read by `dbLoadDatabase` (C `dbDevice`,
//! dbLexRoutines.c), on softioc-rs against softIoc R7.0.10 given the same
//! files: an unknown record type or link type is refused as C refuses it,
//! and a new choice joins the DTYP menu, so a record loaded afterwards may
//! name it. At `iocInit` each such choice with no support behind it is
//! named, as C's `initDevSup` names it.

#![cfg(all(unix, tokio_backend))]

use std::process::{Command, Stdio};

const X_DBD: &str = r#"device(ai,INST_IO,devFoo,"Foo")
device(bo,CONSTANT,devBar,"Bar")
device(ai,INST_IO,devOther,"Soft Channel")
"#;

const BAD_DBD: &str = r#"device(nosuch,INST_IO,devN,"N")
device(ai,BAD,devB,"B")
device(longin,INST_IO,devAfter,"After")
"#;

const DB: &str = r#"record(ai, "F") { field(DTYP, "Foo") field(INP, "@x") }
record(bo, "B") { field(DTYP, "Bar") }
"#;

const SCRIPT: &str = "dbLoadDatabase x.dbd
dbLoadDatabase bad.dbd
dbLoadRecords t.db
iocInit
dbgf F.PACT
dbgf F.DTYP
dbgf B.DTYP
";

/// softIoc's stderr, colour stripped, up to `Starting iocInit`.
const C_LOAD_ERRORS: &str = "ERROR: Record type 'nosuch' not found for device 'N'
ERROR:  at or before ')' in path \".\"  file \"bad.dbd\" line 1

 1 | device(nosuch,INST_IO,devN,\"N\")

ERROR: Bad link type 'BAD' for device 'B'
ERROR: \n 2 | device(ai,BAD,devB,\"B\")

Starting iocInit
";

/// softIoc's answer to each `dbgf`, in order.
const C_DBGF: &[&str] = &[
    "DBF_UCHAR:          0 = 0x0",
    "DBF_STRING:         \"Foo\"",
    "DBF_STRING:         \"Bar\"",
];

/// softIoc's stderr after `Starting iocInit`: `initDevSup` names each dset
/// nobody registered before `initDatabase` runs any `init_record`.
const C_INIT_ERRORS: &[&str] = &[
    "device support devFoo not found",
    "device support devBar not found",
    "device support devAfter not found",
    "recGblRecordError: ai: init_record Error (514,3) PV: F",
    "recGblRecordError: bo: init_record Error (514,3) PV: B",
];

fn strip_colour(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        rest = &rest[rest.find('m').map_or(rest.len(), |m| m + 1)..];
    }
    out.push_str(rest);
    out
}

/// softioc-rs run on `st.cmd` in a directory holding `files`: its stdout
/// and its colour-stripped stderr.
fn softioc(files: &[(&str, &str)]) -> (String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
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
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        strip_colour(&String::from_utf8_lossy(&out.stderr)),
    )
}

#[test]
fn a_loaded_device_line_extends_the_dtyp_menu_as_softioc_does() {
    let (stdout, stderr) = softioc(&[
        ("x.dbd", X_DBD),
        ("bad.dbd", BAD_DBD),
        ("t.db", DB),
        ("st.cmd", SCRIPT),
    ]);
    let load = &stderr[..stderr
        .find("Starting iocInit\n")
        .map_or(0, |at| at + "Starting iocInit\n".len())];
    assert_eq!(load, C_LOAD_ERRORS, "{stderr}");
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
        .filter(|l| l.starts_with("recGblRecordError") || l.starts_with("device support "))
        .collect();
    assert_eq!(errors, C_INIT_ERRORS, "{stderr}");
}

/// `initDevSup` walks `recordTypeList`, which softIoc.dbd lists in `strcmp`
/// order, so the lines follow the record types and not the `device()` lines.
#[test]
fn unserved_dsets_are_named_in_record_type_order() {
    const DBD: &str = r#"device(longin,INST_IO,devL,"L")
device(bo,CONSTANT,devBar,"Bar")
device(ai,INST_IO,devFoo,"Foo")
device(longin,INST_IO,devL2,"L2")
device(aSub,CONSTANT,devAS,"AS")
"#;
    let (_, stderr) = softioc(&[
        ("x.dbd", DBD),
        ("st.cmd", "dbLoadDatabase x.dbd\niocInit\n"),
    ]);
    let named: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("device support "))
        .collect();
    assert_eq!(
        named,
        [
            "device support devAS not found",
            "device support devFoo not found",
            "device support devBar not found",
            "device support devL not found",
            "device support devL2 not found",
        ],
        "{stderr}"
    );
}

/// `dbDevice` stores the dset NAME and `initDevSup` finds the dset by it, so
/// a new choice naming a dset the IOC already has is that support: a soft
/// one, a builtin one, and only the name nothing registered is reported.
/// `registryDeviceSupportFind` is one registry for every record type, so a
/// dset named for another type is found and `initDevSup` says nothing.
/// softIoc then crashes in a record using the choice; that is not copied.
#[test]
fn a_dset_of_another_record_type_is_found() {
    const DBD: &str = r#"device(ao,CONSTANT,devAiSoft,"X")
device(ai,CONSTANT,devNope,"Y")
device(bo,CONSTANT,devNope,"Z")
device(longin,CONSTANT,devTimestampAI,"T")
"#;
    let (_, stderr) = softioc(&[
        ("x.dbd", DBD),
        ("st.cmd", "dbLoadDatabase x.dbd\niocInit\n"),
    ]);
    let named: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("device support "))
        .collect();
    assert_eq!(
        named,
        [
            "device support devNope not found",
            "device support devNope not found",
        ],
        "{stderr}"
    );
}

#[test]
fn a_new_choice_naming_a_known_dset_binds_it() {
    const DBD: &str = r#"device(ai,CONSTANT,devAiSoft,"MySoft")
device(ai,INST_IO,devTimestampAI,"MyStamp")
device(bo,CONSTANT,devBoSoft,"MyBo")
device(bo,CONSTANT,devNope,"Nope")
"#;
    const DB: &str = r#"record(ai, "M") { field(DTYP, "MySoft") field(INP, "5") field(PINI, "YES") }
record(ai, "T") { field(DTYP, "MyStamp") field(INP, "@%Y") }
record(bo, "B") { field(DTYP, "MyBo") field(PINI, "YES") field(VAL, "1") }
"#;
    const SCRIPT: &str = "dbLoadDatabase x.dbd
dbLoadRecords t.db
iocInit
dbgf M.VAL
dbgf M.PACT
dbgf M.SEVR
dbgf B.PACT
dbgf B.SEVR
dbpf T.PROC 1
dbgf T.SEVR
dbgf T.UDF
";
    let (stdout, stderr) = softioc(&[("x.dbd", DBD), ("t.db", DB), ("st.cmd", SCRIPT)]);
    let init: Vec<&str> = stderr
        .lines()
        .skip_while(|l| *l != "Starting iocInit")
        .filter(|l| !l.starts_with("##"))
        .collect();
    assert_eq!(
        init,
        [
            "Starting iocInit",
            "device support devNope not found",
            "iocRun: All initialization complete",
        ],
        "{stderr}"
    );
    let lines: Vec<&str> = stdout.lines().collect();
    let answers: Vec<&str> = lines
        .windows(2)
        .filter(|w| w[0].contains("dbgf ") || w[0].contains("dbpf "))
        .map(|w| w[1].trim_end())
        .collect();
    assert_eq!(
        answers,
        [
            "DBF_DOUBLE:         5",
            "DBF_UCHAR:          0 = 0x0",
            "DBF_STRING:         \"NO_ALARM\"",
            "DBF_UCHAR:          0 = 0x0",
            "DBF_STRING:         \"NO_ALARM\"",
            "DBF_UCHAR:          1 = 0x1",
            "DBF_STRING:         \"NO_ALARM\"",
            "DBF_UCHAR:          0 = 0x0",
        ],
        "{stdout}"
    );
}

/// The refusal is a `yyerror(NULL)`, so `dbReadDatabase` returns -1
/// (`dbYacc.y:397`) and `on error break` stops the script there.
#[test]
fn a_refused_device_line_fails_the_load() {
    let (stdout, stderr) = softioc(&[
        ("bad.dbd", "device(nosuch,INST_IO,devN,\"N\")\n"),
        (
            "st.cmd",
            "on error break\ndbLoadDatabase bad.dbd\necho after\n",
        ),
    ]);
    assert!(stderr.contains("iocsh Error: Break\n"), "{stderr}");
    assert!(!stdout.lines().any(|l| l == "after"), "{stdout}");
}

/// `dbInitRecordLinks` takes the link type from the row in `devList`, a
/// loaded or registered one as much as base's, so a contributed `INST_IO`
/// choice refuses a CONSTANT and leaves the empty `INST_IO` link.
#[test]
fn a_contributed_row_types_its_device_link() {
    let (stdout, stderr) = softioc(&[
        ("x.dbd", "device(ai,INST_IO,devFoo,\"Foo\")\n"),
        (
            "t.db",
            "record(ai, \"F\") { field(DTYP, \"Foo\") field(INP, \"5\") }\n",
        ),
        (
            "st.cmd",
            "dbLoadDatabase x.dbd\ndbLoadRecords t.db\niocInit\ndbgf F.INP\n",
        ),
    ]);
    let init: Vec<&str> = stderr
        .lines()
        .skip_while(|l| *l != "Starting iocInit")
        .filter(|l| !l.starts_with("##"))
        .collect();
    assert_eq!(
        init,
        [
            "Starting iocInit",
            "device support devFoo not found",
            "ERROR: F.INP: can't initialize link type INST_IO with \"5\" (type CONSTANT)",
            "recGblRecordError: ai: init_record Error (514,3) PV: F",
            "iocRun: All initialization complete",
        ],
        "{stderr}"
    );
    let lines: Vec<&str> = stdout.lines().collect();
    let dbgf: Vec<&str> = lines
        .windows(2)
        .filter(|w| w[0].contains("dbgf "))
        .map(|w| w[1].trim_end())
        .collect();
    assert_eq!(dbgf, ["DBF_STRING:         \"@\""], "{stdout}");
}

/// `MACRO_LINK` and `PN_LINK` are `pamaplinkType` words, so their lines add
/// rows. `dbParseLink` yields neither, so `dbCanSetLink` refuses every text,
/// and the emptied link reads as `dbGetString` renders a zeroed one.
#[test]
fn macro_and_pn_link_rows_join_the_menu() {
    const DB: &str = r#"record(ai, "M1") { field(DTYP, "M") field(INP, "@x") }
record(ai, "M2") { field(DTYP, "M") }
record(ao, "P1") { field(DTYP, "P") field(OUT, "abc") }
record(ao, "P2") { field(DTYP, "P") field(OUT, "") }
"#;
    let (stdout, stderr) = softioc(&[
        (
            "x.dbd",
            "device(ai,MACRO_LINK,devM,\"M\")\ndevice(ao,PN_LINK,devP,\"P\")\n",
        ),
        ("t.db", DB),
        (
            "st.cmd",
            "dbLoadDatabase x.dbd\ndbLoadRecords t.db\niocInit\n\
             dbgf M1.INP\ndbgf M2.INP\ndbgf P1.OUT\ndbgf P2.OUT\ndbgf M1.PACT\n",
        ),
    ]);
    let init: Vec<&str> = stderr
        .lines()
        .skip_while(|l| *l != "Starting iocInit")
        .filter(|l| !l.starts_with("##"))
        .collect();
    assert_eq!(
        init,
        [
            "Starting iocInit",
            "device support devM not found",
            "device support devP not found",
            "ERROR: M1.INP: can't initialize link type MACRO_LINK with \"@x\" (type INST_IO)",
            "ERROR: P1.OUT: can't initialize link type PN_LINK with \"abc\" (type PV_LINK)",
            "ERROR: P2.OUT: can't initialize link type PN_LINK with \"\" (type CONSTANT)",
            "recGblRecordError: ai: init_record Error (514,3) PV: M1",
            "recGblRecordError: ai: init_record Error (514,3) PV: M2",
            "recGblRecordError: ao: init_record Error (514,3) PV: P1",
            "recGblRecordError: ao: init_record Error (514,3) PV: P2",
            "iocRun: All initialization complete",
        ],
        "{stderr}"
    );
    let lines: Vec<&str> = stdout.lines().collect();
    let dbgf: Vec<&str> = lines
        .windows(2)
        .filter(|w| w[0].contains("dbgf "))
        .map(|w| w[1].trim_end())
        .collect();
    assert_eq!(
        dbgf,
        [
            "DBF_STRING:         \"\"",
            "DBF_STRING:         \"\"",
            "DBF_STRING:         \" NMS\"",
            "DBF_STRING:         \" NMS\"",
            "DBF_UCHAR:          0 = 0x0",
        ],
        "{stdout}"
    );
}
