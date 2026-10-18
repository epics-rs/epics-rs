//! StreamEpics.cc against C StreamDevice's run-time logs: each case of
//! `oracle/epics/cases.txt` boots a database with the record "rec" on the
//! mock bus "P", plays the bus answers of the C log in its order, and
//! compares what the record layer and the core print, and the record's
//! fields after the run (`oracle/epics/run-epics.sh` made the logs).

mod common;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::recgbl::alarm_condition_string;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::{EpicsValue, PvString};
use stream_rs::bus::{OwnedReadInput, StreamIoStatus};

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
            let (input, size) = input.rsplit_once("\", size=")?;
            // C's size_t: a take-back of 1 prints as 18446744073709551615
            let size = size.parse::<u64>().ok()? as i64;
            let input = if size < 0 {
                OwnedReadInput::TakeBack(size.unsigned_abs() as usize)
            } else {
                OwnedReadInput::Bytes(unescape(input))
            };
            (status, Some(input))
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
    Read(StreamIoStatus, OwnedReadInput),
    Connect(StreamIoStatus),
    BusError(String),
}

impl Answer {
    fn give(self, client: &dyn stream_rs::bus::Client) {
        match self {
            Answer::Lock(status) => client.lock_callback(status),
            Answer::Write(status) => client.write_callback(status),
            Answer::Read(status, input) => {
                client.read_callback(status, input.as_input());
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
    /// The record's other fields, `(name, value)`.
    more: Vec<(String, String)>,
    /// The fields printed after PROC.
    dump: Vec<String>,
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
                more: list(f.get(6))
                    .into_iter()
                    .map(|fv| {
                        let (f, v) = fv.split_once('=').unwrap();
                        (f.to_string(), v.to_string())
                    })
                    .collect(),
                dump: list(f.get(7)),
            };
        }
    }
    panic!("no case {name}");
}

/// An optional column's `;` list, `-` or absent for none.
fn list(column: Option<&&str>) -> Vec<String> {
    match column {
        None | Some(&"-") => Vec::new(),
        Some(c) => c.split(';').map(str::to_string).collect(),
    }
}

/// The errlog lines, which run-epics.sh keeps after the others: C's
/// errlog thread prints them when it runs.
static ERRLOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `drvStreamInit`'s two lines: the driver initializes once a process,
/// so the first case prints them, for each case.
static DRV_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// One case at a time: the bus, the output and the debug level are the
/// process's.
static RUN: Mutex<()> = Mutex::new(());

/// dbgf's value: a long in decimal and hex, a double `%.12g`, a string in
/// quotes; an array as run-epics.sh keeps it, "[NORD] a b".
fn dbgf(v: Option<EpicsValue>) -> String {
    use EpicsValue as V;
    let g = |v: f64, p| epics_base_rs::calc::cvt::fmt_g(v, p, false, false);
    // dbTest.c printBuffer, each element
    let elements: Vec<String> = match v {
        Some(V::Long(v)) => return format!("{v} = {v:#x}"),
        Some(V::ULong(v)) => return format!("{v} = {v:#x}"),
        Some(V::Short(v)) => return format!("{v} = {:#x}", v as u16),
        Some(V::UShort(v)) => return format!("{v} = {v:#x}"),
        Some(V::Char(v) | V::UChar(v)) => return format!("{v} = {v:#x}"),
        // cvtInt64ToHexString: a sign, then the magnitude
        Some(V::Int64(v)) => {
            let sign = if v < 0 { "-" } else { "" };
            return format!("{v} = {sign}{:#x}", v.unsigned_abs());
        }
        Some(V::Double(v)) => return g(v, 12),
        Some(V::String(v)) => return format!("\"{v}\""),
        Some(V::DoubleArray(a)) => a.iter().map(|&v| g(v, 12)).collect(),
        Some(V::FloatArray(a)) => a.iter().map(|&v| g(v as f64, 6)).collect(),
        Some(V::StringArray(a)) => a
            .iter()
            .map(|v| format!("\"{}\"", escaped(v.as_bytes())))
            .collect(),
        Some(V::ShortArray(a)) => a
            .iter()
            .map(|&v| format!("{v} = {:#x}", v as u16))
            .collect(),
        Some(V::UShortArray(a)) => a.iter().map(|&v| format!("{v} = {v:#x}")).collect(),
        Some(V::LongArray(a)) => a
            .iter()
            .map(|&v| format!("{v} = {:#x}", v as u32))
            .collect(),
        Some(V::ULongArray(a)) => a.iter().map(|&v| format!("{v} = {v:#x}")).collect(),
        // cvtInt64ToHexString: a sign, then the magnitude
        Some(V::Int64Array(a)) => a
            .iter()
            .map(|&v| {
                let sign = if v < 0 { "-" } else { "" };
                format!("{v} = {sign}{:#x}", v.unsigned_abs())
            })
            .collect(),
        Some(V::UInt64Array(a)) => a.iter().map(|&v| format!("{v} = {v:#x}")).collect(),
        Some(V::UCharArray(a)) => a.iter().map(|&v| format!("{v} = {v:#x}")).collect(),
        // one element as a number, more as the string to the first 0x00
        Some(V::CharArray(a)) if a.len() == 1 => {
            let v = a[0] as i8 as i32;
            let c = if (0x20..0x7f).contains(&v) {
                format!(" = '{}'", v as u8 as char)
            } else {
                String::new()
            };
            vec![format!("{v} = {:#x}{c}", v & 0xff)]
        }
        Some(V::CharArray(a)) if a.is_empty() => Vec::new(),
        Some(V::CharArray(a)) => {
            let len = a.iter().position(|&b| b == 0).unwrap_or(a.len());
            return format!("[{}] \"{}\"", a.len(), escaped(&a[..len]));
        }
        // dbgf reads an ENUM field as DBR_STRING, which an array's record
        // support cannot give
        Some(V::EnumArray(a)) => {
            return if a.len() == 1 {
                "failed.".into()
            } else {
                format!("[{}] failed.", a.len())
            };
        }
        v => panic!("dbgf {v:?}"),
    };
    match elements.len() {
        0 => "[0] (empty)".into(),
        1 => elements.into_iter().next().unwrap(),
        n => format!("[{n}] {}", elements.join(" ")),
    }
}

/// epicsStrnEscapedFromRaw
fn escaped(raw: &[u8]) -> String {
    let mut out = String::new();
    for &b in raw {
        match b {
            b'\x07' => out.push_str("\\a"),
            b'\x08' => out.push_str("\\b"),
            b'\x0c' => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'\x0b' => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'"' => out.push_str("\\\""),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

fn sevr_string(sevr: AlarmSeverity) -> &'static str {
    match sevr {
        AlarmSeverity::NoAlarm => "NO_ALARM",
        AlarmSeverity::Minor => "MINOR",
        AlarmSeverity::Major => "MAJOR",
        AlarmSeverity::Invalid => "INVALID",
    }
}

/// The fields the C run's dbgf prints.
fn fields(db: &PvDatabase, dump: &[String]) -> Vec<String> {
    let rec = db.get_record("rec").unwrap();
    let r = rec.read();
    let c = &r.common;
    // a DBF_ENUM field prints as its state string, an lsi's or lso's
    // VAL and OVAL as the DBF_STRING cvt_dbaddr makes them, to the NUL
    let long_string = matches!(r.record.record_type(), "lsi" | "lso");
    let show = |f: &str| match (f, r.resolve_field(f)) {
        ("NSEV", _) => format!("\"{}\"", sevr_string(c.nsev)),
        ("NSTA", _) => format!("\"{}\"", alarm_condition_string(c.nsta)),
        // the record's `get_enum_str`
        (_, Some(EpicsValue::Enum(v))) => {
            let snap = r.snapshot_for_field(f).unwrap();
            format!("\"{}\"", snap.enums.unwrap().string_form.render(v))
        }
        (_, Some(EpicsValue::CharArray(v))) if long_string => {
            let v = epics_base_rs::server::database::char_view::long_string_field_bytes(&v);
            format!("\"{}\"", String::from_utf8_lossy(v))
        }
        (_, v) => dbgf(v),
    };
    let val = show("VAL");
    let pact = r.is_processing() as u8;
    let mut shown = vec![
        format!("VAL {val}"),
        format!("SEVR \"{}\"", sevr_string(c.sevr)),
        format!("STAT \"{}\"", alarm_condition_string(c.stat)),
        format!("UDF {} = {:#x}", c.udf, c.udf),
        format!("PACT {pact} = {pact:#x}"),
        format!("PROC {} = {:#x}", c.proc_field, c.proc_field),
    ];
    for f in dump {
        shown.push(format!("{f} {}", show(f)));
    }
    shown
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

/// A line of the C log: the C thread that printed it, and the line.
struct Logged {
    thread: String,
    line: String,
}

/// The C log, made comparable: the protocol path is the oracle directory.
/// The dbgf lines after the run have no thread.
fn expected(name: &str) -> Vec<Logged> {
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
        .find_map(|l| l.strip_prefix("-\tStreamProtocolParser::path = "))
        .unwrap()
        .to_string();
    log.lines()
        .map(|l| match l.split_once('\t') {
            Some((thread, line)) => (thread, line),
            None => ("", l),
        })
        .flat_map(|(thread, line)| run_on_split(thread, line))
        .map(|Logged { thread, line }| {
            // C leaves OVAL behind VAL once MPST and APST are both Always
            // (devstringoutStream.c:39-40); the port does not.
            if name == "so_init_always" && line == "OVAL \"one\"" {
                let line = "OVAL \"two\"".to_string();
                return Logged { thread, line };
            }
            let line = masked(&fixed_convert_text(&line.replace(&dir, "<dir>")));
            let line = if name == "link_spaces" {
                line.replace(
                    "parse link string \"l.proto p P\"",
                    "parse link string \"   l.proto    p    P\"",
                )
            } else {
                line
            };
            Logged { thread, line }
        })
        .collect()
}

/// C's `scanValue(long&)` debug format ends `failed\\n` (StreamCore.cc), so
/// the line runs on into the next debug line, with that line's thread name
/// and file:line (run-epics.sh cuts it when it is neither StreamCore.cc's
/// nor StreamEpics.cc's); the port ends the line.
fn run_on_split(thread: &str, line: &str) -> Vec<Logged> {
    let logged = |thread: &str, line: &str| Logged {
        thread: thread.to_string(),
        line: line.to_string(),
    };
    match line.split_once(" failed\\n") {
        Some((head, next)) if head.starts_with("StreamCore::scanValue(") => {
            let mut out = vec![logged(thread, &format!("{head} failed"))];
            if let Some((next_thread, next)) = next.split_once(' ') {
                let next = next.split_once(": ").map_or(next, |(_, rest)| rest);
                out.push(logged(next_thread, next));
            }
            out
        }
        _ => vec![logged(thread, line)],
    }
}

/// Whether `got` is the C threads' lines in some interleaving: each
/// thread's lines in their order, with no order across threads, which C's
/// scheduler decides run to run (the iocInit thread's "waiting for
/// initDone" against the port thread's callbacks, the errlog thread's
/// lines). On a mismatch, the number of `got` lines matched the furthest.
fn interleaves(got: &[String], want: &[Logged]) -> Result<(), usize> {
    let mut threads: Vec<(&str, Vec<&str>)> = Vec::new();
    for l in want {
        match threads.iter_mut().find(|(t, _)| *t == l.thread) {
            Some((_, lines)) => lines.push(&l.line),
            None => threads.push((&l.thread, vec![&l.line])),
        }
    }
    let seqs: Vec<Vec<&str>> = threads.into_iter().map(|(_, lines)| lines).collect();
    // two threads can print the same line next, so search: the state is
    // each thread's count of matched lines
    fn search(
        got: &[String],
        seqs: &[Vec<&str>],
        at: &mut Vec<usize>,
        seen: &mut std::collections::HashSet<Vec<usize>>,
        furthest: &mut usize,
    ) -> bool {
        let n: usize = at.iter().sum();
        *furthest = (*furthest).max(n);
        if n == got.len() {
            return at.iter().zip(seqs).all(|(&i, s)| i == s.len());
        }
        if !seen.insert(at.clone()) {
            return false;
        }
        for t in 0..seqs.len() {
            if seqs[t].get(at[t]) == Some(&got[n].as_str()) {
                at[t] += 1;
                let found = search(got, seqs, at, seen, furthest);
                at[t] -= 1;
                if found {
                    return true;
                }
            }
        }
        false
    }
    let mut furthest = 0;
    let mut at = vec![0; seqs.len()];
    if search(got, &seqs, &mut at, &mut Default::default(), &mut furthest) {
        Ok(())
    } else {
        Err(furthest)
    }
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

/// The field lines where the port fixes a C defect, C's line first. The
/// `.log` stays C's.
fn fixed_fields(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        // devmbboStream.c:88 sets LALM before :89 compares it with VAL.
        "mbbo_init_cos" | "mbbo_init_cos_rerun" => &[
            ("NSEV \"NO_ALARM\"", "NSEV \"MINOR\""),
            ("NSTA \"NO_ALARM\"", "NSTA \"COS\""),
        ],
        _ => &[],
    }
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
        epics_base_rs::runtime::log::errlog_add_listener(|m| {
            // as run-epics.sh keeps C's: the severity's lines last, the
            // others in place after the first debug line, iocInit's dropped
            for line in m.split_inclusive('\n') {
                if line.starts_with("sevr=") {
                    ERRLOG.lock().push(line.trim_end_matches('\n').to_string());
                    continue;
                }
                let dropped = ["iocsh Error: ", "iocRun: ", "iocPause: ", "iocInit: "]
                    .iter()
                    .any(|p| line.starts_with(p))
                    || line.starts_with("Starting iocInit")
                    || line.starts_with('#');
                let mut out = OUT.lock();
                if !out.is_empty() && !dropped {
                    out.extend_from_slice(line.as_bytes());
                }
            }
        });
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
    epics_base_rs::runtime::log::errlog_flush();
    ERRLOG.lock().clear();
    stream_rs::error::set_stream_debug(1);

    let mut logged = expected(name);
    let has = |o: &str| case.opts.iter().any(|x| x == o);
    let mut fields_want: Vec<String> = logged
        .split_off(logged.len() - 6 - case.dump.len())
        .into_iter()
        .map(|l| l.line)
        .collect();
    let want: Vec<String> = logged.iter().map(|l| l.line.clone()).collect();
    let errlog_want = want.iter().filter(|l| l.starts_with("sevr=")).count();
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

    // run-epics.sh's `*o | *out | *oDirect`
    let output = ["o", "out", "oDirect"]
        .iter()
        .any(|s| case.rtype.ends_with(s));
    let field = if output { "OUT" } else { "INP" };
    let link = format!("@{}", case.link);
    let val = if has("noval") || has("putval") {
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
    let more: String = case
        .more
        .iter()
        .map(|(f, v)| format!("field({f}, \"{v}\") "))
        .collect();
    let db_text = format!(
        "record({}, \"rec\") {{ field(DTYP, \"stream\") field({field}, \"{link}\") {val} {scan} {tpro} {more}}}\n",
        case.rtype
    );
    // The calc module's record type, which streamApp links for
    // stream-scalcout.dbd.
    let builder = IocBuilder::new().register_record_type("scalcout", || {
        Box::new(epics_base_rs::server::records::scalcout::ScalcoutRecord::default())
    });
    let ioc = stream_rs::dev::register(builder)
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        // run-epics.sh loads from a startup script, not softIoc -d
        .boot_past_failed_loads()
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
    if has("putval") {
        let ftvl = case.more.iter().find(|(f, _)| f == "FTVL").unwrap();
        // dbpf's put of VAL, a pp field: a dbPutField that processes
        let _ = db
            .put_record_field_from_ca_no_notify("rec", "VAL", put_value(&ftvl.1, &case.val))
            .await;
    } else if !has("noproc") && !has("intr") {
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
    // the shell's stdout: streamReload's and streamReportRecord's lines
    let mut stdout = Vec::new();
    if has("reload") {
        let at = if has("quiet") {
            want.iter().rposition(|l| !l.starts_with("rec: ")).unwrap() + 1
        } else {
            want.iter().position(|l| l == "streamReload(rec)").unwrap()
        };
        wait_lines(at - drv);
        if has("quiet") {
            // the reload prints its error lines only
            stream_rs::error::set_stream_debug(0);
        }
        stdout = shell(&db, "streamReload rec").await;
        epics_base_rs::runtime::task::sleep(Duration::from_millis(500)).await;
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
    epics_base_rs::runtime::log::errlog_flush();
    wait_lines(want.len() - errlog_want - drv);
    stream_rs::error::set_stream_debug(0);
    epics_base_rs::runtime::log::errlog_flush();

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
    got.extend(ERRLOG.lock().drain(..));
    for (c, port) in fixed_fields(name) {
        let l = fields_want.iter_mut().find(|l| l == c);
        *l.unwrap_or_else(|| panic!("{name}: C no longer shows its defect")) = port.to_string();
    }
    let fields_got = fields(&db, &case.dump);
    if has("report") {
        stdout.extend(shell(&db, "streamReportRecord rec").await);
    }
    if let Err(at) = interleaves(&got, &logged) {
        let threads: Vec<String> = logged
            .iter()
            .map(|l| format!("{}\t{}", l.thread, l.line))
            .collect();
        panic!(
            "{name}: line {at} is no C thread's next line\n got: {:#?}\n C: {threads:#?}",
            &got[at.min(got.len())..]
        );
    }
    assert_eq!(fields_got, fields_want, "{name}");
    let mut stdout_want = Vec::new();
    for side in ["reload", "report"] {
        if has(side) {
            stdout_want.extend(std::fs::read(oracle_dir().join(format!("{name}.{side}"))).unwrap());
        }
    }
    let stdout = String::from_utf8_lossy(&stdout);
    assert_eq!(
        stdout,
        String::from_utf8_lossy(&stdout_want),
        "{name} stdout"
    );
}

/// Run an iocsh line on a thread of its own, as the shell's thread runs
/// it, with stream-rs's commands registered; give its stdout.
async fn shell(db: &Arc<PvDatabase>, line: &str) -> Vec<u8> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "stream-rs-epics-{}-{}.out",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let line = format!("{line} > {}", path.display());
    let db = db.clone();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let thread = std::thread::spawn(move || {
        let shell = IocShell::new(db, bridge);
        for command in stream_rs::epics::iocsh_commands() {
            shell.register(command);
        }
        shell.execute_line(line).unwrap()
    });
    while !thread.is_finished() {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    thread.join().unwrap();
    let out = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    out
}

/// dbpf's VAL: a JSON array of the FTVL's elements, or for CHAR and UCHAR
/// a string, its bytes and the 0 after them.
fn put_value(ftvl: &str, val: &str) -> EpicsValue {
    let Some(list) = val.strip_prefix('[').and_then(|v| v.strip_suffix(']')) else {
        let mut bytes = val.as_bytes().to_vec();
        bytes.push(0);
        return match ftvl {
            "CHAR" => EpicsValue::CharArray(bytes.into()),
            "UCHAR" => EpicsValue::UCharArray(bytes.into()),
            _ => panic!("FTVL {ftvl} takes an array"),
        };
    };
    let items: Vec<&str> = if list.is_empty() {
        Vec::new()
    } else {
        list.split(',').collect()
    };
    fn parsed<T: std::str::FromStr>(items: &[&str]) -> Vec<T>
    where
        T::Err: std::fmt::Debug,
    {
        items.iter().map(|s| s.parse().unwrap()).collect()
    }
    match ftvl {
        "DOUBLE" => EpicsValue::DoubleArray(parsed(&items).into()),
        "FLOAT" => EpicsValue::FloatArray(parsed(&items).into()),
        "LONG" => EpicsValue::LongArray(parsed(&items).into()),
        "ULONG" => EpicsValue::ULongArray(parsed(&items).into()),
        "SHORT" => EpicsValue::ShortArray(parsed(&items).into()),
        "USHORT" => EpicsValue::UShortArray(parsed(&items).into()),
        "INT64" => EpicsValue::Int64Array(parsed(&items).into()),
        "UINT64" => EpicsValue::UInt64Array(parsed(&items).into()),
        "STRING" => EpicsValue::StringArray(
            items
                .iter()
                .map(|s| PvString::from_bytes(s.trim_matches('"').as_bytes()))
                .collect::<Vec<_>>()
                .into(),
        ),
        _ => panic!("FTVL {ftvl}"),
    }
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
    intr_out,
    bin_in,
    bin_in_mismatch,
    si_split,
    si_init_split,
);

// streamReload, streamReportRecord
cases!(
    reload_ok,
    reload_init,
    reload_intr,
    report_ok,
    report_intr,
    report_reconnect,
    report_badbus,
);

// devaiStream.c
cases!(
    ai_scale,
    ai_aslo_zero,
    ai_smoo,
    ai_long,
    ai_ulong,
    ai_ulong_big,
    ai_linr,
    ai_wlong,
    ai_wlinr,
    ai_wulong_neg,
    ai_wurval_neg,
    ai_string,
    ai_smoo_init,
    ai_smoo_udf,
    ai_smoo_nan,
    ai_smoo_inf,
    ai_bad_link,
);

// devlongoutStream.c
cases!(
    lo_init,
    lo_init_mdel,
    lo_init_ulong,
    lo_write,
    lo_wulong_neg,
    lo_wdouble,
    lo_wstring,
    lo_rdouble,
    lo_enum,
    lo_enum_neg,
    lo_wenum,
    lo_wenum_neg,
);

// devaaiStream.c
cases!(
    aai_double,
    aai_full,
    aai_float_long,
    aai_short,
    aai_ushort,
    aai_enum,
    aai_char,
    aai_uchar,
    aai_long,
    aai_ulong,
    aai_int64,
    aai_uint64,
    aai_long_double,
    aai_none,
    aai_d2long,
    aai_l2string,
    aai_s2double,
    aai_strings,
    aai_chars,
    aai_chars_full,
    aai_wdouble,
    aai_wenum,
    aai_wchar,
    aai_wuint64,
    aai_wstring_double,
    aai_wdouble_long,
    aai_wstrings,
    aai_wchars,
    aai_wchars_full,
    aai_wchars_nelm,
    aai_wenum_range,
    aai_chars_shorter,
    aai_nelm1,
);

/// `help` of streamRegistrar's commands prints what C's iocsh prints
/// (oracle/epics/help.out): C registers them with no usage text.
#[test]
fn help_matches_c() {
    let _run = RUN.lock();
    epics_base_rs::runtime::task::test_block_on(async {
        let db = Arc::new(PvDatabase::new());
        let got = shell(&db, "help streamReload streamReportRecord streamSetLogfile").await;
        let mut want = std::fs::read_to_string(oracle_dir().join("help.out")).unwrap();
        // C's ANSI_BOLD is compiled in; rs leaves it out under NO_COLOR
        if !epics_base_rs::server::iocsh::use_ansi_color() {
            while let Some(i) = want.find('\x1b') {
                let end = i + want[i..].find('m').unwrap();
                want.replace_range(i..=end, "");
            }
        }
        assert_eq!(String::from_utf8_lossy(&got), want);
    });
}

/// `streamSetLogfile file` sends the error lines to the file too, one that
/// cannot be opened leaves the file before it in place, and no argument
/// closes it.
#[test]
fn set_logfile() {
    let _run = RUN.lock();
    epics_base_rs::runtime::task::test_block_on(async {
        let db = Arc::new(PvDatabase::new());
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let bad = dir.path().join("none/log");
        shell(&db, &format!("streamSetLogfile {}", log.display())).await;
        shell(&db, &format!("streamSetLogfile {}", bad.display())).await;
        stream_rs::error::stream_error(b"logged\n");
        shell(&db, "streamSetLogfile").await;
        stream_rs::error::stream_error(b"after\n");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.ends_with(" logged\n"), "{text:?}");
        assert!(!bad.exists());
    });
}

// devboStream.c
cases!(
    bo_wl,
    bo_wl_mask,
    bo_wl_mask0,
    bo_wl_val2,
    bo_wu,
    bo_wu_mask,
    bo_we,
    bo_ws,
    bo_ws_one,
    bo_wd,
    bo_init_long,
    bo_init_zero,
    bo_init_neg,
    bo_init_mask,
    bo_init_mask_set,
    bo_init_ulong_big,
    bo_init_enum,
    bo_init_string,
    bo_init_string_zero,
    bo_init_string_bad,
    bo_init_string_long,
    bo_init_double,
    bo_init_proc,
    bo_init_rerun,
    bo_readback,
    bo_readback_mask,
    bo_readback_string,
    bo_intr,
    bo_readback_dol,
    bo_bad_link,
);

// devaoStream.c
cases!(
    ao_w,
    ao_w_aslo_one,
    ao_wl,
    ao_wl_neg,
    ao_wlinr,
    ao_wu_neg,
    ao_wurval_neg,
    ao_init,
    ao_init_proc,
    ao_init_long,
    ao_init_ulong,
    ao_init_deadband,
    ao_init_string,
    ao_readback,
    ao_readback_long,
    ao_intr,
    ao_init_rerun,
    ao_bad_link,
    ao_wu_big,
    ao_init_rerun_nan,
    ao_init_ulong_wrap,
    ao_init_rerun_mdel,
);

// devwaveformStream.c
cases!(
    wf_double,
    wf_float,
    wf_double_full,
    wf_double_part,
    wf_double_none,
    wf_double_to_long,
    wf_long,
    wf_ulong,
    wf_short,
    wf_ushort,
    wf_char,
    wf_uchar,
    wf_int64,
    wf_uint64,
    wf_long_double,
    wf_long_float,
    wf_long_to_string,
    wf_ulong_fmt,
    wf_string,
    wf_chars,
    wf_chars_full,
    wf_chars_none,
    wf_chars_fail,
    wf_string_to_double,
    wf_w_double,
    wf_w_float,
    wf_w_long_d,
    wf_w_ulong_d,
    wf_w_short_d,
    wf_w_ushort_d,
    wf_w_char_d,
    wf_w_uchar_d,
    wf_w_int64_d,
    wf_w_uint64_d,
    wf_w_string_d,
    wf_w_long,
    wf_w_ulong,
    wf_w_short,
    wf_w_ushort,
    wf_w_char,
    wf_w_uchar,
    wf_w_int64,
    wf_w_uint64,
    wf_w_double_l,
    wf_w_string,
    wf_w_chars,
    wf_w_chars_full,
    wf_w_double_s,
    wf_w_empty,
);

// devaaoStream.c
cases!(
    aao_w_double,
    aao_w_float,
    aao_w_long_d,
    aao_w_ulong_d,
    aao_w_short_d,
    aao_w_ushort_d,
    aao_w_char_d,
    aao_w_uchar_d,
    aao_w_int64_d,
    aao_w_uint64_d,
    aao_w_long,
    aao_w_ulong_l,
    aao_w_short_l,
    aao_w_ushort_l,
    aao_w_char_l,
    aao_w_uchar_l,
    aao_w_int64_l,
    aao_w_uint64_l,
    aao_w_double_l,
    aao_w_string,
    aao_w_chars,
    aao_w_chars_full,
    aao_w_uchars,
    aao_w_double_s,
    aao_w_empty,
    aao_init_double,
    aao_init_float,
    aao_init_full,
    aao_init_none,
    aao_init_d_long,
    aao_init_long,
    aao_init_l_double,
    aao_init_l_float,
    aao_init_ulong,
    aao_init_short,
    aao_init_ushort,
    aao_init_char,
    aao_init_uchar,
    aao_init_int64,
    aao_init_uint64,
    aao_init_l_string,
    aao_init_string,
    aao_init_chars,
    aao_init_chars_full,
    aao_init_s_double,
    aao_init_mpst,
    aao_readback,
    aao_init_rerun,
    aao_init_chars_none,
    aao_w_uint64_init,
    aao_w_chars_shorter,
);

// devbiStream.c
cases!(
    bi_long,
    bi_long_zero,
    bi_enum,
    bi_enum_zero,
    bi_wenum,
    bi_mask,
    bi_long_neg,
    bi_ulong_big,
    bi_str_one,
    bi_str_zero,
    bi_str_none,
    bi_str_long,
    bi_str_trunc,
    bi_double,
    bi_wlong,
    bi_wulong,
    bi_wstr,
    bi_wstr_zero,
    bi_wdouble,
);

// devcalcoutStream.c
cases!(
    co_wd,
    co_wl,
    co_wu,
    co_wl_big,
    co_wu_big,
    co_ws,
    co_ocal,
    co_oopt,
    co_odly,
    co_rb,
    co_rb_alarm,
    co_init,
    co_init_long,
    co_init_ulong,
    co_init_ulong_big,
    co_init_str,
    co_init_proc,
    co_rerun,
    co_rerun_nan,
    co_we,
    co_wen,
    co_init_enum,
    co_init_enumn,
);

// devint64inStream.c
cases!(
    i64_long_neg,
    i64_long_big,
    i64_ulong_big,
    i64_double,
    i64_enum,
    i64_wenum,
    i64_enum_neg,
    i64_wenum_neg,
    i64_string,
    i64_wlong,
    i64_wulong_neg,
    i64_wdouble,
    i64_intr_bad,
);

// devint64outStream.c
cases!(
    i64o_init,
    i64o_init_mdel,
    i64o_init_big,
    i64o_init_ulong,
    i64o_write,
    i64o_wbig,
    i64o_wulong_neg,
    i64o_wdouble,
    i64o_wstring,
    i64o_rdouble,
    i64o_enum,
    i64o_enum_neg,
    i64o_wenum,
    i64o_wenum_neg,
);

// devlonginStream.c
cases!(
    li_long_neg,
    li_long_big,
    li_ulong_big,
    li_double,
    li_string,
    li_wlong,
    li_wulong_neg,
    li_wdouble,
    li_enum,
    li_wenum,
    li_enum_neg,
    li_wenum_neg,
);

// devlsiStream.c
cases!(
    lsi_ok,
    lsi_full,
    lsi_exact,
    lsi_mismatch_after,
    lsi_fixwidth_short,
    // VAL is dumped past LEN to the NUL a failed read wrote ("abcdef", LEN 2).
    lsi_fixwidth_grow,
    lsi_wfailed,
    lsi_double,
    lsi_write,
    lsi_wdouble,
);

// devlsoStream.c
cases!(
    lso_write,
    lso_readback,
    lso_long,
    lso_bad_out,
    lso_bad_in,
    lso_fixw,
    lso_init,
    lso_init_rerun,
    lso_init_always,
);

// devmbbiStream.c
cases!(
    mbbi_val,
    mbbi_rval,
    mbbi_rval_none,
    mbbi_nobt,
    mbbi_shft,
    mbbi_mask_shft,
    mbbi_strs_only,
    mbbi_trunc,
    mbbi_trunc_rval,
    mbbi_neg,
    mbbi_ulong,
    mbbi_string,
    mbbi_string_none,
    mbbi_double,
    mbbi_init_states,
    mbbi_init_nobt,
    mbbi_wval,
    mbbi_wrval,
    mbbi_wrval_nomask,
    mbbi_wulong,
    mbbi_wstring,
    mbbi_wdouble,
    mbbi_wstring_big,
);

// devmbbiDirectStream.c
cases!(
    mbbid_val,
    mbbid_nobt,
    mbbid_shft,
    mbbid_neg,
    mbbid_ulong,
    mbbid_ulong_mask,
    mbbid_double,
    mbbid_init,
    mbbid_init_nobt,
    mbbid_wval,
    mbbid_wval_neg,
    mbbid_wulong_neg,
    mbbid_wrval,
    mbbid_wrval_shft,
    mbbid_bad_link,
);

// devmbboStream.c
cases!(
    mbbo_w,
    mbbo_w_states,
    mbbo_w_nomatch,
    mbbo_w_shft,
    mbbo_wl_neg,
    mbbo_wu,
    mbbo_wu_rval_neg,
    mbbo_wd_rval_neg,
    mbbo_wstring,
    mbbo_wstring_big,
    mbbo_wdouble,
    mbbo_init,
    mbbo_init_states,
    mbbo_init_unknown,
    mbbo_init_shft,
    mbbo_init_string,
    mbbo_init_string_none,
    mbbo_noinit_states,
    mbbo_bad_link,
    mbbo_w_mask,
    mbbo_rdouble,
    mbbo_init_illegal,
    mbbo_init_cos,
    mbbo_init_cos_rerun,
);

// devmbboDirectStream.c
cases!(
    mbbod_w,
    mbbod_w_mask,
    mbbod_w_shft,
    mbbod_wu_neg,
    mbbod_wd_neg,
    mbbod_wd_mask_neg,
    mbbod_wu_mask,
    mbbod_wdouble,
    mbbod_wstring,
    mbbod_timeout,
    mbbod_noinit,
    mbbod_init,
    mbbod_init_shft,
    mbbod_init_ulong,
    mbbod_rdouble,
    mbbod_bad_link,
    mbbod_wu_mask_cut,
);

// devscalcoutStream.c
cases!(
    sc_wdouble,
    sc_wocal,
    sc_wlong,
    sc_wbig,
    sc_wulong_neg,
    sc_wstring,
    sc_wstring_num,
    sc_wosv,
    sc_rdouble,
    sc_rlong,
    sc_rulong,
    sc_rstring,
    sc_renum,
    sc_odly,
    sc_init,
    sc_init_str,
    sc_bad_link,
);

// devstringinStream.c
cases!(
    si_long,
    si_mismatch_after,
    si_short_chars,
    si_charset_miss,
    si_fixwidth_short,
    si_double,
    si_write,
    si_wdouble,
);

// devstringoutStream.c
cases!(
    so_write,
    so_readback,
    so_empty,
    so_long,
    so_bad_out,
    so_bad_in,
    so_init,
    so_init_rerun,
    so_init_always,
    so_scan_fixw,
    so_scan_charset,
    so_scan_charset_x,
);

// devmbboDirectStream.c's `sevr = 0` before streamInitRecord's SEVR line
cases!(mbbod_noinit_udf, mbbod_reload_sevr);
