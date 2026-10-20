//! StreamEpics.cc's `%(record.FIELD)` redirect into another record's link
//! field: `matchValue`'s `dbPutField` reaches `dbPutFieldLink`
//! (`dbAccess.c:1261-1262`), which rewires the link, merges the new
//! target's lock set and processes nothing. C's bus callback holds no lock
//! there; the port makes the put holding neither the record nor the core,
//! the run suspended until its status is in.

mod redirect;

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{sleep, test_block_on};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use redirect::{ASYNC_REPLIES, REPLIES, WRITES, client, get, ioc, process};
use stream_rs::bus::StreamIoStatus;
use stream_rs::epics::stream_reload;
use stream_rs::error::{ProcessCapture, capture_process_stderr, set_stream_debug};

fn f(db: &PvDatabase, pv: &str) -> f64 {
    get(db, pv).to_f64().unwrap()
}

fn set_id(db: &PvDatabase, record: &str) -> u64 {
    db.lock_set_of(record).unwrap().id
}

/// `tb`, in the set of an idle stream record that names it, which its
/// install makes before any other: a link put to `tb` takes that set first
/// and waits there, holding nothing, while a test thread holds it.
const TB: &str = r#"
record(ao, "tb") {}
record(ai, "a0") { field(DTYP, "stream") field(INP, "@redirect.proto names_tb R") }
"#;

fn link(db: &PvDatabase, pv: &str) -> String {
    get(db, pv).text_g()
}

/// `pv` once it is `want`, or after two seconds.
async fn settled(db: &PvDatabase, pv: &str, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(2);
    while get(db, pv).text_g() != want && Instant::now() < deadline {
        sleep(Duration::from_millis(5)).await;
    }
    get(db, pv).text_g()
}

/// What `capture` took until `done` holds of it, or two seconds.
async fn printed(capture: &ProcessCapture, done: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut printed = String::new();
    while !done(&printed) && Instant::now() < deadline {
        printed.push_str(&String::from_utf8_lossy(&capture.take()));
        sleep(Duration::from_millis(1)).await;
    }
    printed
}

/// `record`'s lock set, held on a thread of its own until the sender is
/// dropped.
fn hold(db: &Arc<PvDatabase>, record: &'static str) -> mpsc::Sender<()> {
    let (release, released) = mpsc::channel::<()>();
    let (held, holding) = mpsc::channel();
    let db = db.clone();
    std::thread::spawn(move || {
        let _set = db.lock_records([record]);
        held.send(()).unwrap();
        let _ = released.recv();
    });
    holding.recv().unwrap();
    release
}

/// The C IOC's lines for `a`'s refused put into `lk.OUT`: matchValue's
/// failed `dbPutField` (StreamEpics.cc:1469-1474), then matchInput's for
/// the format whose value did not match with no scan tried
/// (StreamCore.cc), in that order.
async fn refused_lines(capture: &ProcessCapture) {
    let lines = [
        "a: dbPutField(lk.OUT, DBF_STRING, \"@instio\", 1) failed\n",
        "a: Format \"%s\" has data type string which is not supported by \"lk.OUT\".\n",
    ];
    let out = printed(capture, |p| p.contains(lines[1])).await;
    let at = lines.map(|l| out.find(l).unwrap_or_else(|| panic!("{l:?} in {out:?}")));
    assert!(at[0] < at[1], "{out:?}");
}

#[test]
fn a_redirect_into_a_link_field_rewires_it() {
    REPLIES.lock().push(("H7", "t2"));
    test_block_on(async {
        let db = ioc(r#"
record(ao, "t1") {}
record(ao, "t2") {}
record(ao, "lk") { field(OUT, "t1") }
record(ai, "a") { field(DTYP, "stream") field(INP, "@redirect.proto put_link R") }
"#)
        .await;
        assert_eq!(process(&db, "a", 1).await, ["H7"]);
        assert_eq!(f(&db, "a.SEVR"), 0.0, "the put answered Ok");
        assert_eq!(get(&db, "lk.OUT").text_g(), "t2 NPP NMS");
        assert_eq!(f(&db, "lk"), 0.0, "dbPutFieldLink processes nothing");
        assert_eq!(set_id(&db, "lk"), set_id(&db, "t2"));
        assert_ne!(set_id(&db, "lk"), set_id(&db, "t1"));
        db.put_record_field_from_ca_no_notify("lk", "VAL", EpicsValue::Double(4.0))
            .await
            .unwrap();
        assert_eq!(f(&db, "t2"), 4.0, "lk writes its new target");
        assert_eq!(f(&db, "t1"), 0.0);
    });
}

/// A link text `dbCanSetLink` refuses under the lock, an `INST_IO` address
/// in a soft ao's OUT: matchValue returns false, so the input fails with
/// the C IOC's two lines, INVALID/CALC and the link as it was.
#[test]
fn a_link_put_refused_under_the_lock_fails_the_input() {
    REPLIES.lock().push(("H7", "@instio"));
    test_block_on(async {
        let db = ioc(r#"
record(ao, "t1") {}
record(ao, "lk") { field(OUT, "t1") }
record(ai, "a") { field(DTYP, "stream") field(INP, "@redirect.proto put_link R") }
"#)
        .await;
        let capture = capture_process_stderr();
        assert_eq!(process(&db, "a", 1).await, ["H7"]);
        refused_lines(&capture).await;
        assert_eq!(settled(&db, "a.SEVR", "3").await, "3", "INVALID");
        assert_eq!(f(&db, "a.STAT"), 12.0, "CALC");
        assert_eq!(link(&db, "lk.OUT"), "t1 NPP NMS");
    });
}

/// The same with a `@mismatch` handler, which runs once the input failed;
/// the record still takes INVALID/CALC, as the C IOC's does.
#[test]
fn a_mismatch_handler_runs_after_a_refused_link_put() {
    REPLIES.lock().push(("H8", "@instio"));
    test_block_on(async {
        let db = ioc(r#"
record(ao, "t1") {}
record(ao, "lk") { field(OUT, "t1") }
record(ai, "a") { field(DTYP, "stream") field(INP, "@redirect.proto put_link_mm R") }
"#)
        .await;
        let capture = capture_process_stderr();
        assert_eq!(process(&db, "a", 2).await, ["H8", "MM"]);
        refused_lines(&capture).await;
        assert_eq!(settled(&db, "a.SEVR", "3").await, "3", "INVALID");
        assert_eq!(f(&db, "a.STAT"), 12.0, "CALC");
        assert_eq!(link(&db, "lk.OUT"), "t1 NPP NMS");
    });
}

/// A put the link takes: the run goes on from it, to the line's next
/// format and then the next command.
#[test]
fn the_run_goes_on_from_its_link_put() {
    REPLIES.lock().push(("H9", "t2 5"));
    test_block_on(async {
        let db = ioc(r#"
record(ao, "t1") {}
record(ao, "t2") {}
record(ao, "t") {}
record(ao, "lk") { field(OUT, "t1") }
record(ai, "a") { field(DTYP, "stream") field(INP, "@redirect.proto put_link_on R") }
"#)
        .await;
        assert_eq!(process(&db, "a", 2).await, ["H9", "after"]);
        assert_eq!(link(&db, "lk.OUT"), "t2 NPP NMS");
        assert_eq!(f(&db, "t"), 5.0);
        assert_eq!(f(&db, "a.SEVR"), 0.0);
    });
}

/// Two link puts in one line: the run suspends at each in turn.
#[test]
fn two_link_puts_in_one_line() {
    REPLIES.lock().push(("HB", "t2 t3"));
    test_block_on(async {
        let db = ioc(r#"
record(ao, "t1") {}
record(ao, "t2") {}
record(ao, "t3") {}
record(ao, "lk") { field(OUT, "t1") }
record(ao, "lk2") { field(OUT, "t1") }
record(ai, "a") { field(DTYP, "stream") field(INP, "@redirect.proto put_links R") }
"#)
        .await;
        assert_eq!(process(&db, "a", 2).await, ["HB", "after"]);
        assert_eq!(link(&db, "lk.OUT"), "t2 NPP NMS");
        assert_eq!(link(&db, "lk2.OUT"), "t3 NPP NMS");
        assert_eq!(f(&db, "a.SEVR"), 0.0);
    });
}

/// The second of two link puts refused: the first stays, the input fails.
#[test]
fn the_second_link_put_in_a_line_refused() {
    REPLIES.lock().push(("HB", "t2 @instio"));
    test_block_on(async {
        let db = ioc(r#"
record(ao, "t1") {}
record(ao, "t2") {}
record(ao, "lk") { field(OUT, "t1") }
record(ao, "lk2") { field(OUT, "t1") }
record(ai, "a") { field(DTYP, "stream") field(INP, "@redirect.proto put_links R") }
"#)
        .await;
        let capture = capture_process_stderr();
        assert_eq!(process(&db, "a", 1).await, ["HB"]);
        let line = "a: dbPutField(lk2.OUT, DBF_STRING, \"@instio\", 1) failed\n";
        let out = printed(&capture, |p| p.contains(line)).await;
        assert!(out.contains(line), "{out:?}");
        assert_eq!(settled(&db, "a.SEVR", "3").await, "3", "INVALID");
        assert_eq!(link(&db, "lk.OUT"), "t2 NPP NMS");
        assert_eq!(link(&db, "lk2.OUT"), "t1 NPP NMS");
        assert_eq!(WRITES.lock().as_slice(), ["HB"]);
    });
}

/// `lk` is in `a`'s own lock set, as the protocol's targets join it, and
/// another thread holds the new target's set, then wants `a`'s: the put,
/// made holding neither, takes both in id order, so neither waits on the
/// other.
#[test]
fn a_link_put_against_a_holder_of_its_new_target() {
    REPLIES.lock().push(("H7", "tb"));
    test_block_on(async {
        let db = ioc(&format!(
            r#"{TB}
record(ao, "t1") {{}}
record(ao, "lk") {{ field(OUT, "t1") }}
record(ai, "a") {{ field(DTYP, "stream") field(INP, "@redirect.proto put_link R") }}
"#
        ))
        .await;
        assert_eq!(set_id(&db, "lk"), set_id(&db, "a"));
        assert!(set_id(&db, "tb") < set_id(&db, "a"));
        let release = hold(&db, "tb");
        assert_eq!(process(&db, "a", 1).await, ["H7"]);
        let wants_a = {
            let db = db.clone();
            std::thread::spawn(move || {
                drop(db.lock_records(["a"]));
                drop(release);
            })
        };
        assert_eq!(settled(&db, "lk.OUT", "tb NPP NMS").await, "tb NPP NMS");
        assert!(wants_a.is_finished());
        assert_eq!(f(&db, "a.SEVR"), 0.0);
    });
}

/// A reload while the run waits for its link put aborts the run: the put,
/// made by then, stays, and its status finds no run to take on.
#[test]
fn a_reload_during_the_link_put_keeps_the_put_and_drops_the_run() {
    REPLIES.lock().push(("H9", "tb 5"));
    test_block_on(async {
        let db = ioc(&format!(
            r#"{TB}
record(ao, "t1") {{}}
record(ao, "t") {{}}
record(ao, "lk") {{ field(OUT, "t1") }}
record(ai, "a") {{ field(DTYP, "stream") field(INP, "@redirect.proto put_link_on R") }}
"#
        ))
        .await;
        assert!(set_id(&db, "tb") < set_id(&db, "a"));
        let release = hold(&db, "tb");
        assert_eq!(process(&db, "a", 1).await, ["H9"]);
        let mut out = Vec::new();
        stream_reload(Some("a"), &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "a: Protocol reloaded\n");
        drop(release);
        assert_eq!(settled(&db, "lk.OUT", "tb NPP NMS").await, "tb NPP NMS");
        sleep(Duration::from_millis(300)).await;
        assert_eq!(WRITES.lock().as_slice(), ["H9"], "the aborted run went on");
        assert_eq!(f(&db, "t"), 0.0);
        // and the reloaded protocol runs
        assert_eq!(process(&db, "a", 2).await, ["H9", "after"]);
        assert_eq!(f(&db, "t"), 5.0);
    });
}

/// An I/O Intr record's protocol starts under its hold (initHook
/// AfterIocRunning, then recordProcessCallback's restart) and its read is
/// answered inline: the run suspends inside that start, and the hold's end
/// makes the put and takes the run on.
#[test]
fn a_link_put_from_inside_the_io_intr_start() {
    ASYNC_REPLIES.lock().extend([("il", "t2"), ("il", "t3")]);
    test_block_on(async {
        let db = ioc(r#"
record(ao, "t1") {}
record(ao, "t2") {}
record(ao, "t3") {}
record(ao, "lk") { field(OUT, "t1") }
record(ai, "il") { field(DTYP, "stream") field(INP, "@redirect.proto intr_link RI") field(SCAN, "I/O Intr") }
"#)
        .await;
        assert_eq!(settled(&db, "lk.OUT", "t3 NPP NMS").await, "t3 NPP NMS");
        let deadline = Instant::now() + Duration::from_secs(2);
        while WRITES.lock().len() < 2 && Instant::now() < deadline {
            sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(WRITES.lock().as_slice(), ["after", "after"]);
    });
}

/// An entry made while the run waits for its link put waits too, as C's
/// would for the stream's mutex, and goes in once the put's status took
/// the run on: an asynchronous connect, which belongs to no run.
#[test]
fn an_entry_during_the_link_put_goes_in_after_it() {
    REPLIES.lock().push(("H7", "tb"));
    test_block_on(async {
        let db = ioc(&format!(
            r#"{TB}
record(ao, "t1") {{}}
record(ao, "lk") {{ field(OUT, "t1") }}
record(ai, "a") {{ field(DTYP, "stream") field(INP, "@redirect.proto put_link R") }}
"#
        ))
        .await;
        assert!(set_id(&db, "tb") < set_id(&db, "a"));
        let release = hold(&db, "tb");
        assert_eq!(process(&db, "a", 1).await, ["H7"]);
        let capture = capture_process_stderr();
        set_stream_debug(1);
        let a = client("a");
        let connect =
            std::thread::spawn(move || a.async_connect_callback(StreamIoStatus::StreamIoSuccess));
        sleep(Duration::from_millis(200)).await;
        assert!(!connect.is_finished(), "the connect waits for the put");
        drop(release);
        let connected = "StreamCore::connectCallback(a, StreamIoSuccess)";
        let out = printed(&capture, |p| p.contains(connected)).await;
        set_stream_debug(0);
        let put = out.find("Stream::matchValue(a): dbPutField(lk.OUT");
        let conn = out.find(connected);
        assert!(matches!((put, conn), (Some(p), Some(c)) if p < c), "{out}");
        assert_eq!(link(&db, "lk.OUT"), "tb NPP NMS");
    });
}
