//! DebugInterface.cc against C StreamDevice's run-time logs: each case of
//! `oracle/debug/cases.txt` boots a database with the record "rec" on the
//! bus "debug", processes it once, and compares what the bus, the core and
//! the record layer print, and the record's fields after the run
//! (`oracle/debug/run-debug.sh` made the logs).

mod common;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Once;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_condition_string;
use epics_base_rs::server::record::{
    AlarmSeverity, CommonFields, DbLinkType, DevSup, Dset, Record,
};
use epics_base_rs::types::EpicsValue;
use stream_rs::epics::{
    DO_NOT_CONVERT, ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamIoFunction,
    StreamRecordType,
};

static LONGIN: [DevSup; 1] = [DevSup {
    name: "devlonginStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false),
}];

static AI: [DevSup; 1] = [DevSup {
    name: "devaiStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], false),
}];

static STRINGIN: [DevSup; 1] = [DevSup {
    name: "devstringinStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false),
}];

/// devlonginStream.c's `readData`
fn longin_read(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let mut val = 0;
    if io.scanf_long(format, &mut val) == ERROR {
        return ERROR;
    }
    let _ = record.put_field("VAL", EpicsValue::Long(val as i32));
    OK
}

/// devaiStream.c's `readData` for a `%f` format: the defaults' ASLO 1,
/// AOFF 0 and SMOO 0 leave the value as read.
fn ai_read(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let mut val = 0.0;
    if io.scanf_double(format, &mut val) == ERROR {
        return ERROR;
    }
    let _ = record.put_field("VAL", EpicsValue::Double(val));
    DO_NOT_CONVERT
}

/// devstringinStream.c's `readData`, into the 40 bytes of VAL.
fn stringin_read(
    io: &StreamIo<'_>,
    record: &mut dyn Record,
    _: &mut CommonFields,
    format: &Format<'_>,
) -> i64 {
    let mut val = [0u8; 40];
    if io.scanf_string(format, &mut val) == ERROR as isize {
        return ERROR;
    }
    let len = val.iter().position(|&b| b == 0).unwrap_or(val.len());
    let text = String::from_utf8_lossy(&val[..len]).into_owned();
    let _ = record.put_field("VAL", EpicsValue::String(text.into()));
    OK
}

struct RecordType {
    read: StreamIoFunction,
}

impl StreamRecordType for RecordType {
    fn init_record(&self, _record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        StreamInitArgs {
            link: common.text.inp.to_string(),
            read_data: Some(self.read),
            write_data: None,
            map_error: false,
        }
    }

    fn is_output(&self) -> bool {
        false
    }
}

/// The debug and error output, the debug file's: both go there.
static OUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Out;

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        OUT.lock().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn oracle_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/oracle/debug")
}

/// The kept lines, as run-debug.sh keeps C's: the bus's, the bus
/// registry's, the core's and the record layer's debug lines without the
/// thread name and file:line, and the error lines. A writeRequest line
/// runs on over the line ends of the output it prints.
fn kept(out: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cont = false;
    for line in String::from_utf8_lossy(out).split_inclusive('\n') {
        let line = line.strip_suffix('\n').unwrap_or(line);
        if cont {
            cont = !line.ends_with(')');
            lines.push(line.to_string());
            continue;
        }
        // the thread name, then "file:line: " or nothing for an error
        let Some((_, rest)) = line.split_once(' ') else {
            continue;
        };
        match rest.split_once(": ") {
            Some((at, msg)) if at.contains(".rs:") => {
                let file = at.split(':').next().unwrap_or("");
                if matches!(
                    file,
                    "epics.rs" | "core.rs" | "bus.rs" | "debug_interface.rs"
                ) && !msg.starts_with("StreamCore::attachBus(")
                {
                    cont =
                        msg.starts_with("DebugInterface::writeRequest") && !msg.ends_with("msec)");
                    lines.push(msg.to_string());
                }
            }
            _ => lines.push(rest.to_string()),
        }
    }
    lines
}

struct Case {
    rtype: String,
    link: String,
    intr: bool,
    twice: bool,
}

fn case(name: &str) -> Case {
    let cases = std::fs::read_to_string(oracle_dir().join("cases.txt")).unwrap();
    for line in cases.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f[0] == name {
            return Case {
                rtype: f[1].into(),
                link: f[2].into(),
                intr: f[3].split(',').any(|o| o == "intr"),
                twice: f[3].split(',').any(|o| o == "twice"),
            };
        }
    }
    panic!("no case {name}");
}

/// `drvStreamInit`'s two lines: the driver initializes once a process,
/// so the first case prints them, for each case.
static DRV_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// One case at a time: the output and the debug level are the process's.
static RUN: Mutex<()> = Mutex::new(());

/// The fields the C run's dbgf prints.
fn fields(db: &PvDatabase) -> Vec<String> {
    let rec = db.get_record("rec").unwrap();
    let r = rec.read();
    let c = &r.common;
    // dbgf prints a long in decimal and hex
    let val = match r.record.get_field("VAL") {
        Some(EpicsValue::Long(v)) => format!("{v} = {v:#x}"),
        Some(EpicsValue::Double(v)) => format!("{v}"),
        Some(EpicsValue::String(v)) => format!("\"{v}\""),
        v => panic!("VAL {v:?}"),
    };
    let sevr = match c.sevr {
        AlarmSeverity::NoAlarm => "NO_ALARM",
        AlarmSeverity::Minor => "MINOR",
        AlarmSeverity::Major => "MAJOR",
        AlarmSeverity::Invalid => "INVALID",
    };
    let pact = r.is_processing() as u8;
    vec![
        format!("VAL {val}"),
        format!("SEVR \"{sevr}\""),
        format!("STAT \"{}\"", alarm_condition_string(c.stat)),
        format!("UDF {} = {:#x}", c.udf, c.udf),
        format!("PACT {pact} = {pact:#x}"),
        format!("PROC {} = {:#x}", c.proc_field, c.proc_field),
    ]
}

/// streamGetIointInfo's pointers, which differ run to run.
fn masked(l: &str) -> String {
    if !l.starts_with("streamGetIointInfo(") {
        return l.to_string();
    }
    let mut out = String::new();
    let mut rest = l;
    while let Some(at) = rest.find("=0x") {
        out.push_str(&rest[..at + 1]);
        rest = rest[at + 3..].trim_start_matches(|c: char| c.is_ascii_hexdigit());
        out.push_str("<ptr>");
    }
    out + rest
}

/// The C log, made comparable: the protocol path is the oracle directory.
fn expected(name: &str) -> Vec<String> {
    let log = std::fs::read_to_string(oracle_dir().join(format!("{name}.log"))).unwrap();
    let dir = log
        .lines()
        .find_map(|l| l.strip_prefix("StreamProtocolParser::path = "))
        .unwrap()
        .to_string();
    log.split_inclusive('\n')
        .map(|l| {
            let l = l.strip_suffix('\n').unwrap_or(l).replace(&dir, "<dir>");
            masked(&fixed_convert_text(&l))
        })
        .collect()
}

/// C prints "convert" for DO_NOT_CONVERT and "don't convert" for OK
/// (StreamEpics.cc:830-832); the port prints them the right way round.
fn fixed_convert_text(l: &str) -> String {
    for at in ["initialized. ", "ready. "] {
        if let Some((head, what)) = l.split_once(at) {
            let what = match what {
                "convert" => "don't convert",
                "don't convert" => "convert",
                other => other,
            };
            return format!("{head}{at}{what}");
        }
    }
    l.to_string()
}

/// Wait until `out` holds `n` kept lines, or the time is up.
fn wait_lines(n: usize) -> Vec<String> {
    let start = Instant::now();
    loop {
        let lines = kept(&OUT.lock());
        if lines.len() >= n || start.elapsed() > Duration::from_secs(5) {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Run the case as run-debug.sh does: iocInit, the PROC put, and compare
/// the kept lines and the fields after the run.
async fn run_case(name: &str) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // first, as the C build's registrar list starts with it
        stream_rs::debug_interface::register();
        stream_rs::epics::register_driver();
        common::setup();
        // SAFETY: set before any thread of this test binary reads it.
        unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", oracle_dir()) };
        stream_rs::error::set_stream_debug_file(Some(Box::new(Out)));
    });
    let case = case(name);
    OUT.lock().clear();
    stream_rs::error::set_stream_debug(1);

    let mut want = expected(name);
    let fields_want = want.split_off(want.len() - 6);
    let drv = if DRV_LINES.lock().is_empty() { 0 } else { 2 };

    let scan = if case.intr {
        "field(SCAN, \"I/O Intr\")"
    } else {
        ""
    };
    let db_text = format!(
        "record({}, \"rec\") {{ field(DTYP, \"stream\") field(INP, \"@{}\") {scan} }}\n",
        case.rtype, case.link
    );
    let ioc = IocBuilder::new()
        .register_dynamic_device_support(
            &[("longin", &LONGIN), ("ai", &AI), ("stringin", &STRINGIN)],
            |ctx: &DeviceSupportContext| {
                let read = match (ctx.record_type, ctx.dtyp) {
                    ("longin", "stream") => longin_read as StreamIoFunction,
                    ("ai", "stream") => ai_read as StreamIoFunction,
                    ("stringin", "stream") => stringin_read as StreamIoFunction,
                    _ => return None,
                };
                Some(Box::new(StreamDevice::new(RecordType { read })) as _)
            },
        )
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    if !case.intr {
        // dbpf's put: a dbPutField
        let _ = db
            .put_record_field_from_ca_no_notify("rec", "PROC", EpicsValue::Long(1))
            .await;
    }
    if case.twice {
        // run-debug.sh's sleep, then the second dbpf
        epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
        let _ = db
            .put_record_field_from_ca_no_notify("rec", "PROC", EpicsValue::Long(1))
            .await;
    }
    wait_lines(want.len() - drv);
    // run-debug.sh's sleep: nothing more may come
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    stream_rs::error::set_stream_debug(0);

    let mut got = kept(&OUT.lock());
    {
        let mut drv = DRV_LINES.lock();
        if drv.is_empty() {
            *drv = got.iter().take(2).cloned().collect();
        } else {
            got.splice(0..0, drv.iter().cloned());
        }
    }
    let dir = oracle_dir().display().to_string();
    for l in &mut got {
        *l = masked(&l.replace(&dir, "<dir>"));
    }
    want.extend(fields_want);
    got.extend(fields(&db));
    assert_eq!(got, want, "{name}");
    assert_eq!(epics_base_rs::server::ioc_app::ioc_shutdown(), 0);
}

macro_rules! cases {
    ($($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() {
            let _run = RUN.lock();
            epics_base_rs::runtime::task::test_block_on(run_case(stringify!($name)));
        }
    )*};
}

cases!(
    read_ok,
    mismatch,
    nul_output,
    timeouts,
    in_only,
    init,
    no_terminator,
    intr,
    read_twice,
);
