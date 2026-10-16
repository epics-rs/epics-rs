use std::sync::mpsc;
use std::time::Duration;

use parking_lot::Mutex;

/// Whether `write` returns while `writer` is held, as by a file write in
/// progress.
pub(crate) fn returns_while_busy<W>(writer: &Mutex<W>, write: impl FnOnce() + Send) -> bool {
    let busy = writer.lock();
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || {
            write();
            tx.send(()).unwrap();
        });
        let returned = rx.recv_timeout(Duration::from_secs(5)).is_ok();
        drop(busy);
        returned
    })
}
