#![no_main]
//! Fuzz the UDP search-response decoder reachable via `try_parse_frame`
//! → `decode_search_response`. Targets the path that caps a
//! peer-controlled `count` field driving `Vec::with_capacity`. Should
//! never panic, regardless of malformed addr blob, unterminated
//! string, oversized count, or truncated cid array.

use epics_pva_rs::client_native::decode::{decode_search_response, try_parse_frame};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(Some((frame, _))) = try_parse_frame(data) {
        let _ = decode_search_response(&frame);
    }
});
