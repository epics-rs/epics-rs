//! The errlog worker walks the live listener list once per message, as C's
//! `errlogThread` does under `listenerLock` (`errlog.c:801-815`), so a change
//! a callback makes applies within the same pass. R7.0.10 libCom: L1 and L3
//! registered; L1 blocks the worker on `stall` while `m1`..`m3` are posted, so
//! all three are one pass; on `m1`, L1 adds L2 and removes L3, then itself:
//!
//! ```text
//! L1: stall m1
//! L2: m1 m2 m3
//! L3: stall
//! ```

use std::sync::{Mutex, OnceLock, mpsc};

use epics_libcom_rs::runtime::log::{
    ErrlogListenerId, eltc, errlog_add_listener, errlog_flush, errlog_init, errlog_printf,
    errlog_remove_listener,
};

static SEEN: Mutex<[String; 3]> = Mutex::new([String::new(), String::new(), String::new()]);
static L1: OnceLock<ErrlogListenerId> = OnceLock::new();
static L3: OnceLock<ErrlogListenerId> = OnceLock::new();
static GO: Mutex<Option<mpsc::Receiver<()>>> = Mutex::new(None);

fn heard(n: usize, m: &str) {
    SEEN.lock().unwrap()[n].push_str(&m.replace('\n', " "));
}

#[test]
fn a_callbacks_add_and_removes_apply_within_the_pass() {
    errlog_init(0);
    eltc(false);
    let (go, wait) = mpsc::channel();
    *GO.lock().unwrap() = Some(wait);

    let l1 = errlog_add_listener(|m| {
        heard(0, m);
        if m == "stall\n" {
            GO.lock().unwrap().take().unwrap().recv().unwrap();
        }
        if m == "m1\n" {
            errlog_add_listener(|m| heard(1, m));
            errlog_remove_listener(*L3.get().unwrap());
            errlog_remove_listener(*L1.get().unwrap());
        }
    });
    L1.set(l1).unwrap();
    L3.set(errlog_add_listener(|m| heard(2, m))).unwrap();

    std::thread::spawn(move || {
        errlog_printf("stall\n");
        std::thread::sleep(std::time::Duration::from_millis(200));
        for m in ["m1\n", "m2\n", "m3\n"] {
            errlog_printf(m);
        }
        go.send(()).unwrap();
    })
    .join()
    .unwrap();
    errlog_flush();

    assert_eq!(
        *SEEN.lock().unwrap(),
        ["stall m1 ", "m1 m2 m3 ", "stall "].map(String::from)
    );
}
