//! StreamEpics.cc:1440 `matchValue`'s put debug line prints the field
//! buffer itself, `fieldBuffer.expand()`: a double is its eight bytes.

mod redirect;

use std::io::Write;
use std::sync::Arc;

use parking_lot::Mutex;
use redirect::{REPLIES, ioc, process};
use stream_rs::error::{set_stream_debug, set_stream_debug_colored, set_stream_debug_file};

struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_double_put_shows_its_bytes() {
    REPLIES.lock().push(("Q7", "1.5"));
    epics_base_rs::runtime::task::test_block_on(async {
        let db = ioc(r#"
record(ai, "t4") {}
record(ao, "p") { field(DTYP, "stream") field(OUT, "@redirect.proto put_double R") }
"#)
        .await;
        let out = Arc::new(Mutex::new(Vec::new()));
        set_stream_debug_file(Some(Box::new(Sink(out.clone()))));
        set_stream_debug_colored(false);
        set_stream_debug(1);
        process(&db, "p", 1).await;
        set_stream_debug(0);
        set_stream_debug_file(None);

        let bytes: String = 1.5f64
            .to_ne_bytes()
            .iter()
            .map(|&c| match c {
                0x20..0x7f => (c as char).to_string(),
                _ => format!("<{c:02x}>"),
            })
            .collect();
        let out = String::from_utf8_lossy(&out.lock()).into_owned();
        let line = format!("dbPutField(t4.VAL, DBF_DOUBLE, {bytes}) status=0x0");
        assert!(out.contains(&line), "{line} not in {out}");
    });
}
