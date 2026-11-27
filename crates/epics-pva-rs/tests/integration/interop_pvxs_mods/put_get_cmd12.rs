//! A PUT_GET (command 12) request on a live channel, against pvxs 1.5.3
//! softIocPVX and against the rs server.
//!
//! `ServerConn::handle_PUT_GET() {}` (`serverconn.cpp:259`) is in
//! `bevRead`'s switch, so a PUT_GET is drained without a reply and, unlike
//! ARRAY, without the "Ignore unexpected command" line. An ARRAY sent after
//! it marks when the PUT_GET has been handled.

use epics_pva_rs::proto::Command;

use super::array_cmd14::{ignore_line, on_pvxs};

#[test]
fn pvxs_drains_a_put_get_request_silently() {
    let (replies, me, err) = on_pvxs(&[Command::PutGet, Command::Array]);
    assert_eq!(replies, Vec::<u8>::new());
    assert!(err.contains(&ignore_line(me, Command::Array)), "{err}");
    let line = ignore_line(me, Command::PutGet);
    assert!(!err.contains(&line), "{line:?} in:\n{err}");
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rs_drains_a_put_get_request_silently_as_pvxs_does() {
    let (replies, me, heard) = super::array_cmd14::on_rs(&[Command::PutGet, Command::Array]).await;
    assert_eq!(replies, Vec::<u8>::new());
    assert!(heard.contains(&ignore_line(me, Command::Array)), "{heard}");
    let line = ignore_line(me, Command::PutGet);
    assert!(!heard.contains(&line), "{line:?} in:\n{heard}");
}
