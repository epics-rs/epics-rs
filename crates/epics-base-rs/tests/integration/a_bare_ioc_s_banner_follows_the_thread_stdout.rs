//! C `coreRelease` (epicsRelease.c, which includes `epicsStdio.h`) prints
//! iocInit's banner to the calling thread's stdout, so a caller that set one
//! gets it there. [`IocBuilder::build_ioc`] is that `iocInit`.

use std::io::Write;
use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::stdio::redirect_stdout;
use epics_base_rs::runtime::task::BlockingBridge;
use epics_base_rs::server::ioc_builder::IocBuilder;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[epics_macros_rs::epics_test]
async fn the_banner_goes_to_the_building_thread_s_stdout() {
    let bridge = BlockingBridge::capture();
    let sink = Sink::default();
    let out = sink.clone();
    let _ioc = std::thread::spawn(move || {
        redirect_stdout(out, || bridge.block_on(IocBuilder::new().build_ioc()))
    })
    .join()
    .unwrap()
    .unwrap();

    let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<_> = text.lines().collect();
    let rule = "#".repeat(76);
    assert_eq!(lines.len(), 5, "{text}");
    assert_eq!((lines[0], lines[4]), (&*rule, &*rule));
    assert!(lines[1].starts_with("## EPICS R"), "{text}");
}
