//! matchValue's line for a failed put prints the scan buffer as `"%.*s"`
//! at `consumed` (StreamEpics.cc:1467-1472): the last scan's input count,
//! not the string's length. The lines are those of C streamApp on this
//! database, its bus a socket giving the same replies.

mod redirect;

use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{sleep, test_block_on};
use redirect::{REPLIES, ioc, process};
use stream_rs::error::capture_process_stderr;

const DB: &str = r#"
record(waveform, "ws") { field(FTVL, "STRING") field(NELM, "3") field(DISP, "1") }
record(waveform, "wc") { field(FTVL, "CHAR") field(NELM, "10") field(DISP, "1") }
record(ao, "t1") {}
record(ao, "lk") { field(OUT, "t1") }
record(ai, "a1") { field(DTYP, "stream") field(INP, "@redirect.proto fail_s3 R") }
record(ai, "a2") { field(DTYP, "stream") field(INP, "@redirect.proto fail_s2 R") }
record(ai, "a3") { field(DTYP, "stream") field(INP, "@redirect.proto fail_sep R") }
record(ai, "a4") { field(DTYP, "stream") field(INP, "@redirect.proto fail_ch R") }
record(ai, "a5") { field(DTYP, "stream") field(INP, "@redirect.proto fail_lk R") }
"#;

/// Process `record`, whose request is `query` and its reply `reply`, and
/// wait for `line` on stderr.
fn prints(record: &str, query: &'static str, reply: &'static str, line: &str) {
    REPLIES.lock().push((query, reply));
    test_block_on(async {
        let db = ioc(DB).await;
        let capture = capture_process_stderr();
        assert_eq!(process(&db, record, 1).await, [query]);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut printed = String::new();
        while !printed.contains(line) && Instant::now() < deadline {
            printed.push_str(&String::from_utf8_lossy(&capture.take()));
            sleep(Duration::from_millis(1)).await;
        }
        assert!(printed.contains(line), "{printed:?}");
    });
}

/// Three elements, the last of three bytes: the first is cut to them.
#[test]
fn a_string_array_prints_its_first_element_to_the_last_scans_count() {
    prints(
        "a1",
        "F1",
        "abcdef gh ij",
        "a1: dbPutField(ws.VAL, DBF_STRING, \"abc\", 3) failed\n",
    );
}

/// The last element scanned no input.
#[test]
fn a_last_element_of_no_input_prints_nothing() {
    prints(
        "a2",
        "F2",
        "abcdef gh",
        "a2: dbPutField(ws.VAL, DBF_STRING, \"\", 3) failed\n",
    );
}

/// The second element's separator is missing: the scan that ended the
/// loop answered -1, which as a precision prints the whole first element.
#[test]
fn a_failed_scan_after_the_first_prints_the_whole_first_element() {
    prints(
        "a3",
        "F3",
        "abcdef,gh",
        "a3: dbPutField(ws.VAL, DBF_STRING, \"abcdef,gh\", 1) failed\n",
    );
}

/// A lookahead takes "abc" and consumes nothing; the count is the bytes
/// put, the NUL among them.
#[test]
fn a_char_array_prints_to_its_scans_count() {
    prints(
        "a4",
        "F4",
        "abcdef",
        "a4: dbPutField(wc.VAL, DBF_CHAR, \"\", 4) failed\n",
    );
}

/// The link put refused under the lock, its line printed after the hold.
#[test]
fn an_owed_link_put_prints_to_its_scans_count() {
    prints(
        "a5",
        "F5",
        "@instio",
        "a5: dbPutField(lk.OUT, DBF_STRING, \"\", 1) failed\n",
    );
}
