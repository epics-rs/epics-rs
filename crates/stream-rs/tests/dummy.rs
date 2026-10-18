//! The dummy bus against C StreamDevice: `tests/oracle/dummy/dummy.log` is
//! what C printed attaching records to "dummy", "dummyX" and "dummy 3" and
//! processing the first once (see `tests/oracle/dummy/run.sh`).

mod common;

use std::sync::Arc;

use stream_rs::bus::{self, Client};
use stream_rs::core::StartMode;
use stream_rs::error::{capture_stderr, set_stream_debug, take_captured};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/dummy");

/// The lines `run.sh` keeps, cut the same way: no thread name, no file:line,
/// pointers masked, and only DummyInterface's checks.
fn kept(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|l| {
            let l = l.split_once(' ')?.1;
            let l = match l.split_once(": ") {
                Some((at, rest)) if at.contains(".rs:") => rest,
                _ => l,
            };
            Some(l.to_string())
        })
        .filter(|l| {
            let other_check = l.contains(" check ") && !l.ends_with(" check DummyInterface");
            let other_match = l.starts_with("StreamBusInterface::find ")
                && (l.ends_with(" matches") || l.ends_with(" does not match"))
                && !l.starts_with("StreamBusInterface::find DummyInterface ");
            !other_check
                && !other_match
                && (l.starts_with("StreamBusInterface::find")
                    || l.starts_with("StreamCore::")
                    || l.starts_with("Cannot find a bus"))
        })
        .map(|l| match l.find("businterface=0x") {
            Some(i) => format!("{}businterface=0x*", &l[..i]),
            None => l,
        })
        .collect()
}

#[test]
fn a_record_on_the_dummy_bus_goes_offline_as_in_c() {
    common::setup();
    stream_rs::dummy::register();
    let file = format!("{DIR}/d.proto");
    capture_stderr();
    set_stream_debug(1);
    let (rec, ok) = common::load(&file, "p", "rec", &[]);
    assert!(ok);
    assert!(rec.attach_bus("dummy", 0, ""));
    let (other, _) = common::load(&file, "p", "other", &[]);
    assert!(!other.attach_bus("dummyX", 0, ""));
    let (addr, _) = common::load(&file, "p", "addr", &[]);
    assert!(addr.attach_bus("dummy", 3, ""));
    assert!(!rec.start_protocol(StartMode::StartNormal));
    set_stream_debug(0);
    let want: Vec<String> = std::fs::read_to_string(format!("{DIR}/dummy.log"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(kept(&take_captured()), want);
}

/// DummyInterface takes "dummy" only, by C's strcmp, and refuses its lock
/// and unlock; the rest is StreamBusInterface's, nothing supported.
#[test]
fn the_dummy_bus_takes_dummy_only_and_refuses_everything() {
    stream_rs::dummy::register();
    let (rec, _) = common::load(&format!("{DIR}/d.proto"), "p", "rec", &[]);
    let client: Arc<dyn Client> = rec;
    for busname in ["Dummy", "dummy ", "dummyX", ""] {
        assert!(bus::find(&client, busname, 0, "").is_none(), "{busname:?}");
    }
    let found = bus::find(&client, "dummy", -1, "p").unwrap();
    assert_eq!(found.name(), "dummy");
    let dummy = found.bus();
    assert!(!dummy.lock_request(5000));
    assert!(!dummy.unlock());
    assert!(!dummy.write_request(b"X", 100));
    assert!(!dummy.read_request(100, 100, -1, false));
    assert!(!dummy.supports_event());
    assert!(!dummy.supports_async_read());
    assert!(!dummy.connect_request(100));
    assert!(!dummy.disconnect_request());
}
