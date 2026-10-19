//! `Stream::report`'s bytes go out as C's `printf` writes them:
//! `tests/oracle/dbior/bytes.out` (see `run-bytes.sh`) has a converter on
//! conversion byte 0xE9 and a protocol string holding a raw 0xE9, both
//! printed as that one byte.

#[path = "dbior/harness.rs"]
mod harness;

use stream_rs::StreamBuffer;
use stream_rs::converter::{StreamFormatConverter, register_converter};
use stream_rs::cstr::CPtr;
use stream_rs::format::{SIGNED_FORMAT, StreamFormat};

/// `HighConverter.cc`.
struct HighConverter;

impl StreamFormatConverter for HighConverter {
    fn parse(&self, _: &StreamFormat, _: &mut StreamBuffer, _: &mut CPtr<'_>, _: bool) -> i32 {
        SIGNED_FORMAT
    }
}

#[test]
fn dbior_stream_prints_high_bytes_as_c() {
    register_converter("HighConverter", b"\xe9", Box::new(HighConverter));
    epics_base_rs::runtime::task::test_block_on(async {
        let db = harness::boot(
            r#"record(stringout, "R3") { field(DTYP, "stream") field(OUT, "@h.proto hi dummy") }"#,
        )
        .await;
        harness::assert_levels(&db, "bytes.out", &["1", "2"]).await;
    });
}
