//! StreamEpics.cc against C StreamDevice's run-time logs: each case of
//! `oracle/epics/cases.txt` boots a database with the record "rec" on the
//! mock bus "P", plays the bus answers of the C log in its order, and
//! compares what the record layer and the core print, and the record's
//! fields after the run (`oracle/epics/run-epics.sh` made the logs).

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
use stream_rs::bus::StreamIoStatus;
use stream_rs::epics::{
    DO_NOT_CONVERT, ERROR, Format, OK, Stream, StreamDevice, StreamIo, StreamIoFunction,
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

static LONGOUT: [DevSup; 1] = [DevSup {
    name: "devlongoutStream",
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

/// devlonginStream.c / devlongoutStream.c / devaiStream.c /
/// devstringinStream.c: the record type,
/// its link and its `readData` / `writeData`.
struct RecordType {
    output: bool,
    read: Option<StreamIoFunction>,
}

impl StreamRecordType for RecordType {
    fn init_record(
        &self,
        stream: &Stream,
        record: &mut dyn Record,
        common: &mut CommonFields,
    ) -> i64 {
        let link = if self.output {
            common.text.out.to_string()
        } else {
            common.text.inp.to_string()
        };
        stream.init_record(record, common, &link, self.read, None)
    }

    fn is_output(&self) -> bool {
        self.output
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/oracle/epics")
}

/// The kept lines, as run-epics.sh keeps C's: StreamEpics.cc's and
/// StreamCore.cc's debug lines without the thread name and file:line, and
/// the error lines.
fn kept(out: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    for line in String::from_utf8_lossy(out).lines() {
        // the thread name, then "file:line: " or nothing for an error
        let Some((_, rest)) = line.split_once(' ') else {
            continue;
        };
        match rest.split_once(": ") {
            Some((at, msg)) if at.contains(".rs:") => {
                let file = at.split(':').next().unwrap_or("");
                // attachBus's line prints a pointer
                if (file == "epics.rs" || file == "core.rs")
                    && !msg.starts_with("StreamCore::attachBus(")
                {
                    lines.push(msg.to_string());
                }
            }
            _ => lines.push(rest.to_string()),
        }
    }
    lines
}

/// A logged bus answer the test plays: `StreamCore::lockCallback(rec,
/// StreamIoSuccess)` and the like, or the bus's own error line.
fn play(line: &str) -> Option<Answer> {
    // AsynDriverInterface's "rec connectToAsynPort: ...", printed by the mock
    if let Some((client, _)) = line.split_once(": ")
        && client
            .strip_prefix("rec ")
            .is_some_and(|f| f.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        return Some(Answer::BusError(line.to_string()));
    }
    if let Some(rest) = line.strip_prefix("StreamCore::connectCallback(rec, ") {
        let (status, _) = rest.split_once(") activeCommand: ")?;
        return Some(Answer::Connect(io_status(status)?));
    }
    let rest = line.strip_prefix("StreamCore::")?;
    let (callback, args) = rest.split_once("(rec, ")?;
    let args = args.strip_suffix(')')?;
    let (status, input) = match args.split_once(" input=\"") {
        Some((status, input)) => {
            let input = input.rsplit_once("\", size=")?.0;
            (status, Some(unescape(input)))
        }
        None => (args, None),
    };
    let status = io_status(status)?;
    match (callback, input) {
        ("lockCallback", None) => Some(Answer::Lock(status)),
        ("writeCallback", None) => Some(Answer::Write(status)),
        ("readCallback", Some(input)) => Some(Answer::Read(status, input)),
        _ => None,
    }
}

fn io_status(status: &str) -> Option<StreamIoStatus> {
    Some(match status {
        "StreamIoSuccess" => StreamIoStatus::StreamIoSuccess,
        "StreamIoTimeout" => StreamIoStatus::StreamIoTimeout,
        "StreamIoNoReply" => StreamIoStatus::StreamIoNoReply,
        "StreamIoEnd" => StreamIoStatus::StreamIoEnd,
        "StreamIoFault" => StreamIoStatus::StreamIoFault,
        _ => return None,
    })
}

enum Answer {
    Lock(StreamIoStatus),
    Write(StreamIoStatus),
    Read(StreamIoStatus, Vec<u8>),
    Connect(StreamIoStatus),
    BusError(String),
}

impl Answer {
    fn give(self, client: &dyn stream_rs::bus::Client) {
        match self {
            Answer::Lock(status) => client.lock_callback(status),
            Answer::Write(status) => client.write_callback(status),
            Answer::Read(status, input) => {
                client.read_callback(status, &input);
            }
            Answer::Connect(status) => client.connect_callback(status),
            Answer::BusError(line) => {
                stream_rs::error::stream_error(format!("{line}\n").as_bytes())
            }
        }
    }
}

/// StreamBuffer::expand's `<xx>` back to the byte.
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<'
            && i + 3 < b.len()
            && b[i + 3] == b'>'
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

struct Case {
    rtype: String,
    link: String,
    val: String,
    table: String,
    opts: Vec<String>,
}

fn case(name: &str) -> Case {
    let cases = std::fs::read_to_string(oracle_dir().join("cases.txt")).unwrap();
    for line in cases.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        // run-epics.sh's `read` collapses empty fields; none are empty
        let f: Vec<&str> = line.split('\t').collect();
        if f[0] == name {
            return Case {
                rtype: f[1].into(),
                link: if f[2] == "<empty>" {
                    String::new()
                } else {
                    f[2].into()
                },
                val: f[3].into(),
                table: f[4].into(),
                opts: f[5].split(',').map(str::to_string).collect(),
            };
        }
    }
    panic!("no case {name}");
}

/// `drvStreamInit`'s two lines: the driver initializes once a process,
/// so the first case prints them, for each case.
static DRV_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// One case at a time: the bus, the output and the debug level are the
/// process's.
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
    // C keeps the spaces before the bus name in the protocol name
    // (StreamEpics.cc initRecord) and finds no protocol "p   ": the port
    // ends the name at its last byte, so the link with spaces round its
    // parts runs as the plain one.
    let file = if name == "link_spaces" {
        "link_plain"
    } else {
        name
    };
    let log = std::fs::read_to_string(oracle_dir().join(format!("{file}.log"))).unwrap();
    let dir = log
        .lines()
        .find_map(|l| l.strip_prefix("StreamProtocolParser::path = "))
        .unwrap()
        .to_string();
    log.lines()
        .map(|l| {
            let l = masked(&fixed_convert_text(&l.replace(&dir, "<dir>")));
            if name == "link_spaces" {
                l.replace(
                    "parse link string \"l.proto p P\"",
                    "parse link string \"   l.proto    p    P\"",
                )
            } else {
                l
            }
        })
        .collect()
}

/// C prints "convert" for DO_NOT_CONVERT and "don't convert" for OK
/// (StreamEpics.cc initRecord and process); the port prints them the right
/// way round.
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

/// Run the case as run-epics.sh does: iocInit, the PROC put, and compare
/// the kept lines and the fields after the run.
async fn run_case(name: &str) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        common::setup();
        // SAFETY: set before any thread of this test binary reads it.
        unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", oracle_dir()) };
        stream_rs::error::set_stream_debug_file(Some(Box::new(Out)));
    });
    let case = case(name);
    {
        let mut mock = common::MOCK.lock();
        mock.lock_ok = case.table != "-";
        // asyn's octet ports take an interrupt user
        mock.async_read = true;
    }
    *common::CLIENT.lock() = None;
    OUT.lock().clear();
    stream_rs::error::set_stream_debug(1);

    let mut want = expected(name);
    let has = |o: &str| case.opts.iter().any(|x| x == o);
    let fields_want = want.split_off(want.len() - 6);
    // the driver's lines are printed by the first case only
    let drv = if DRV_LINES.lock().is_empty() { 0 } else { 2 };

    // the bus answers, played in the log's order
    let script = want.clone();
    let player = std::thread::spawn(move || {
        for (i, line) in script.iter().enumerate() {
            // evalIn hands its early input to readCallback itself
            if i > 0 && script[i - 1].starts_with("StreamCore::evalIn(rec): early input: ") {
                continue;
            }
            let Some(play) = play(line) else { continue };
            let have = wait_lines(i - drv);
            if have.len() < i - drv {
                return Err(format!("not reached {line:?}: {have:#?}"));
            }
            let client = common::CLIENT.lock().clone().and_then(|c| c.upgrade());
            let Some(client) = client else {
                return Err(format!("no client to play {line:?}: {have:#?}"));
            };
            play.give(&*client);
        }
        Ok(())
    });

    let output = case.rtype.ends_with("out");
    let field = if output { "OUT" } else { "INP" };
    let link = format!("@{}", case.link);
    let val = if has("noval") {
        String::new()
    } else {
        format!("field(VAL, \"{}\")", case.val)
    };
    let scan = if has("intr") {
        "field(SCAN, \"I/O Intr\")"
    } else {
        ""
    };
    let tpro = if has("tpro") {
        "field(TPRO, \"2\")"
    } else {
        ""
    };
    let db_text = format!(
        "record({}, \"rec\") {{ field(DTYP, \"stream\") field({field}, \"{link}\") {val} {scan} {tpro} }}\n",
        case.rtype
    );
    let ioc = IocBuilder::new()
        .register_dynamic_device_support(
            &[
                ("longin", &LONGIN),
                ("ai", &AI),
                ("stringin", &STRINGIN),
                ("longout", &LONGOUT),
            ],
            |ctx: &DeviceSupportContext| {
                let (output, read) = match (ctx.record_type, ctx.dtyp) {
                    ("longin", "stream") => (false, Some(longin_read as StreamIoFunction)),
                    ("ai", "stream") => (false, Some(ai_read as StreamIoFunction)),
                    ("stringin", "stream") => (false, Some(stringin_read as StreamIoFunction)),
                    ("longout", "stream") => (true, None),
                    _ => return None,
                };
                Some(Box::new(StreamDevice::new(RecordType { output, read })) as _)
            },
        )
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    let put_scan = |scan: &'static str| {
        let db = db.clone();
        async move {
            let _ = db
                .put_record_field_from_ca_no_notify("rec", "SCAN", EpicsValue::String(scan.into()))
                .await;
        }
    };
    if !has("noproc") && !has("intr") {
        let proc = if has("proc2") { 2 } else { 1 };
        // dbpf's put: a dbPutField
        let _ = db
            .put_record_field_from_ca_no_notify("rec", "PROC", EpicsValue::Long(proc))
            .await;
    }
    if has("twice") {
        // run-epics.sh puts again after its sleep, once the first run is done
        let done = want
            .iter()
            .position(|l| l == "recordProcessCallback(rec) processing record done")
            .unwrap();
        wait_lines(done + 1 - drv);
        let _ = db
            .put_record_field_from_ca_no_notify("rec", "PROC", EpicsValue::Long(1))
            .await;
    }
    if has("flip") {
        put_scan("I/O Intr").await;
        epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
        put_scan("Passive").await;
        epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
        put_scan("I/O Intr").await;
        epics_base_rs::runtime::task::sleep(Duration::from_millis(1500)).await;
    }
    if has("pauserun") {
        assert_eq!(epics_base_rs::server::ioc_app::ioc_pause(), 0);
        epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
        assert_eq!(epics_base_rs::server::ioc_app::ioc_run(), 0);
        epics_base_rs::runtime::task::sleep(Duration::from_millis(500)).await;
    }
    player.join().unwrap().unwrap();
    wait_lines(want.len() - drv);
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
    link_plain,
    link_args,
    link_args_space,
    link_args_nested,
    link_addr_hex,
    link_addr_octal,
    link_addr_none,
    link_addr_neg,
    link_no_protocol,
    link_no_bus,
    link_empty,
    link_bad_protocol,
    link_bad_file,
    link_bad_bus,
    init_ok,
    init_fail,
    init_proc2,
    link_spaces,
    ai_ok,
    ai_mismatch,
    reply_timeout,
    wait_timer,
    twice,
    tpro,
    si_ok,
    connect_fault,
    intr_ok,
    intr_flip,
    intr_pause,
    init_rerun,
    intr_init_rerun,
    asyncconnect,
);
