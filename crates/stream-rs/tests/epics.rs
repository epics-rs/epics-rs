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
    ERROR, Format, OK, Stream, StreamDevice, StreamIo, StreamIoFunction, StreamRecordType,
};

static LONGIN: [DevSup; 1] = [DevSup {
    name: "devlonginStream",
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

/// devlonginStream.c / devlongoutStream.c: the record type, its link and
/// its `readData` / `writeData`.
struct Longx {
    output: bool,
    read: Option<StreamIoFunction>,
}

impl StreamRecordType for Longx {
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
/// StreamIoSuccess)` and the like.
fn play(line: &str) -> Option<Answer> {
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
    let status = match status {
        "StreamIoSuccess" => StreamIoStatus::StreamIoSuccess,
        "StreamIoTimeout" => StreamIoStatus::StreamIoTimeout,
        "StreamIoNoReply" => StreamIoStatus::StreamIoNoReply,
        "StreamIoEnd" => StreamIoStatus::StreamIoEnd,
        "StreamIoFault" => StreamIoStatus::StreamIoFault,
        _ => return None,
    };
    match (callback, input) {
        ("lockCallback", None) => Some(Answer::Lock(status)),
        ("writeCallback", None) => Some(Answer::Write(status)),
        ("readCallback", Some(input)) => Some(Answer::Read(status, input)),
        _ => None,
    }
}

enum Answer {
    Lock(StreamIoStatus),
    Write(StreamIoStatus),
    Read(StreamIoStatus, Vec<u8>),
}

impl Answer {
    fn give(self, client: &dyn stream_rs::bus::Client) {
        match self {
            Answer::Lock(status) => client.lock_callback(status),
            Answer::Write(status) => client.write_callback(status),
            Answer::Read(status, input) => {
                client.read_callback(status, &input);
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
    let val = match r.record.get_field("VAL") {
        Some(EpicsValue::Long(v)) => v,
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
        format!("VAL {val} = {val:#x}"),
        format!("SEVR \"{sevr}\""),
        format!("STAT \"{}\"", alarm_condition_string(c.stat)),
        format!("UDF {} = {:#x}", c.udf, c.udf),
        format!("PACT {pact} = {pact:#x}"),
        format!("PROC {} = {:#x}", c.proc_field, c.proc_field),
    ]
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
            let l = fixed_convert_text(&l.replace(&dir, "<dir>"));
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

/// Run the case up to the end of the record's initialization, and, for a
/// case that processes nothing, compare the fields too.
async fn run_init(name: &str) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        common::setup();
        // SAFETY: set before any thread of this test binary reads it.
        unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", oracle_dir()) };
        stream_rs::error::set_stream_debug_file(Some(Box::new(Out)));
    });
    let case = case(name);
    common::MOCK.lock().lock_ok = case.table != "-";
    *common::CLIENT.lock() = None;
    OUT.lock().clear();
    stream_rs::error::set_stream_debug(1);

    let mut want = expected(name);
    let done = want
        .iter()
        .position(|l| l.starts_with("streamInitRecord(rec) done") || l.starts_with("rec: Empty"))
        .unwrap();
    let noproc = case.opts.iter().any(|o| o == "noproc");
    let tail = want.split_off(done + 1);
    let fields_want: Vec<String> = tail[tail.len() - 6..].to_vec();

    // the bus answers of the init run, played in the log's order
    let script = want.clone();
    let player = std::thread::spawn(move || {
        for (i, line) in script.iter().enumerate() {
            let Some(play) = play(line) else { continue };
            // the driver's lines are printed by the first case only
            let have = wait_lines(i.saturating_sub(2));
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
    let db_text = format!(
        "record({}, \"rec\") {{ field(DTYP, \"stream\") field({field}, \"{link}\") field(VAL, \"{}\") }}\n",
        case.rtype, case.val
    );
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(
            &[("longin", &LONGIN), ("longout", &LONGOUT)],
            |ctx: &DeviceSupportContext| {
                let output = match (ctx.record_type, ctx.dtyp) {
                    ("longin", "stream") => false,
                    ("longout", "stream") => true,
                    _ => return None,
                };
                let read = (!output).then_some(longin_read as StreamIoFunction);
                Some(Box::new(StreamDevice::new(Longx { output, read })) as _)
            },
        )
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    player.join().unwrap().unwrap();
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
        *l = l.replace(&dir, "<dir>");
    }
    if noproc {
        want.extend(fields_want);
        got.extend(fields(&db));
    }
    assert_eq!(got, want, "{name}");
}

macro_rules! cases {
    ($($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() {
            let _run = RUN.lock();
            epics_base_rs::runtime::task::test_block_on(run_init(stringify!($name)));
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
);
