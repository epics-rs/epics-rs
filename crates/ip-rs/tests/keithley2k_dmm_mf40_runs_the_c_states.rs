//! The compiled `Keithley2kDMM_mf40.st` on the shipped
//! `Keithley2kDMM_mf40.db`, driving a 2700 mock through the template's
//! devXxStrParm records. Each step's commands, and the values it leaves,
//! are the ones a C IOC (seq 2.2.9, asyn, calc, ip's devXxStrParm) sent and
//! posted against the same mock, template and puts.

mod keithley;
mod support;

use std::time::Duration;

use epics_base_rs::runtime::task;
use epics_base_rs::types::{EpicsValue, PvString};

use keithley::{Ioc, Meter, boot};
use support::eventually;

/// The 2700 behind `Dmm=D1:`.
const DMM: Meter = Meter {
    program: "Keithley2kDMM_mf40",
    dmm: "D1:",
    model: "2700",
    modes: &[
        ":conf:volt:dc",
        ":conf:volt:ac",
        ":conf:res",
        ":conf:fres",
        ":conf:freq",
        "conf:temp;:temp:tc:type j",
        ":conf:temp;:temp:tc:type k",
        ":conf:temp;:temp:tc:type t",
        ":conf:curr:dc",
        ":conf:curr:ac",
    ],
    read: "read?",
};

/// Boot's commands, once `read_meter`'s reset has set the reading format.
async fn boot_commands(ioc: &Ioc) -> Vec<String> {
    let mut got = Vec::new();
    let reset = eventually(|| {
        got.extend(ioc.mock.take());
        got.iter().any(|c| c == "form:elem read")
    })
    .await;
    assert!(reset, "{got:?}");
    task::sleep(Duration::from_millis(250)).await;
    got.extend(ioc.mock.take());
    got
}

/// Boot with 4 channels: `read_meter` resets the meter through
/// `init_string`, configures DC volts and closes channel 1, and 1 s on the
/// reset sets the reading format through `init_string1`.
async fn booted(ioc: &Ioc) {
    let got = boot_commands(ioc).await;
    assert_eq!(
        got,
        [
            "*rst",
            ":conf:volt:dc",
            "rout:clos (@101)",
            "form:elem read"
        ]
    );
}

/// Single-channel reads: each sends `read?` behind whatever mode and relay
/// changed since the last, and `Dmm_raw` takes the reading.
#[epics_base_rs::epics_test]
async fn single_reads_follow_the_selected_channel_and_mode() {
    let ioc = boot(&DMM, 4).await;
    booted(&ioc).await;
    assert_eq!(ioc.str_of("model"), "2700");
    assert_eq!(ioc.f64_of("channels"), 4.0);

    ioc.step("dmm_chan", 2.0, &["rout:clos (@103)"]).await;
    ioc.step("ch_mode_sel", 1.0, &[":conf:volt:ac"]).await;
    assert_eq!(ioc.str_of("units"), "VAC");
    ioc.read(&["read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 103.1);

    // 4-wire ohms opens every relay, so the next read closes channel 3 again.
    ioc.step("ch_mode_sel", 3.0, &[":conf:fres", "rout:open:all"])
        .await;
    assert_eq!(ioc.str_of("units"), "OHM");
    assert_eq!(ioc.str_of("ch_close"), "rout:open:all");
    ioc.read(&["rout:clos (@103)", "read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 103.3);

    // With 4 channels, dmm_chan2 closes nothing itself; the 11-20 range
    // still reads channel 15 through it.
    ioc.step("dmm_ch_range", 1.0, &[]).await;
    ioc.step("dmm_chan2", 4.0, &[]).await;
    ioc.read(&["rout:clos (@115)", "read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 115.3);
    ioc.step("dmm_ch_range", 0.0, &[]).await;
    ioc.read(&["rout:clos (@103)", "read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 103.3);
}

/// A multi-channel scan reads each channel that is on, in its own mode,
/// into its `Ch<n>_raw`; AC volts goes through `dmm_delay_read`. A channel's
/// mode change rewrites every channel's units label. `init_dmm` resets the
/// meter. 4-wire ohms first leaves every relay open and the meter off DC
/// volts, so the scan sends channel 1's mode and relay too.
#[epics_base_rs::epics_test]
async fn a_multi_scan_reads_each_channel_that_is_on_in_its_own_mode() {
    let ioc = boot(&DMM, 4).await;
    booted(&ioc).await;
    ioc.step("ch_mode_sel", 3.0, &[":conf:fres", "rout:open:all"])
        .await;
    ioc.step("single_multi", 1.0, &[]).await;
    for n in [1, 3, 4] {
        ioc.put(
            &format!("ch{n}_units"),
            EpicsValue::String(PvString::from("zz")),
        )
        .await;
    }
    ioc.step("ch2_mode_sel", 1.0, &[]).await;
    for (n, units) in [(1, "VDC"), (2, "VAC"), (3, "VDC"), (4, "VDC")] {
        assert_eq!(ioc.str_of(&format!("ch{n}_units")), units, "ch{n}");
    }
    assert_eq!(ioc.str_of("ch5_units"), "VDC");
    ioc.step("Ch3_on_off", 1.0, &[]).await;

    ioc.read(&[
        ":conf:volt:dc",
        "rout:clos (@101)",
        "read?",
        ":conf:volt:ac",
        "rout:clos (@102)",
        "read?",
        ":conf:volt:dc",
        "rout:clos (@104)",
        "read?",
    ])
    .await;
    assert_eq!(ioc.f64_of("Ch1_raw"), 101.0);
    assert_eq!(ioc.f64_of("Ch2_raw"), 102.1);
    assert_ne!(ioc.f64_of("Ch3_raw.UDF"), 0.0, "Ch3_raw was read");
    assert_eq!(ioc.f64_of("Ch4_raw"), 104.0);

    ioc.step("init_dmm", 1.0, &["*rst", "form:elem read"]).await;
    assert_eq!(ioc.f64_of("init_dmm"), 0.0);
}

/// A `channels` macro past the program's 40 channels is cut to 40. C
/// scanned a 41st relay through `ch_on_off[40]` and `ch_mode[40]`, and its
/// `pvPut(ch_raw[40])` processed `conf_dcv` instead of a reading.
#[epics_base_rs::epics_test]
async fn a_channels_macro_past_forty_is_cut_to_forty() {
    let ioc = boot(&DMM, 41).await;
    boot_commands(&ioc).await;
    assert_eq!(ioc.f64_of("channels"), 40.0);
    ioc.step("ch40_mode_sel", 1.0, &[]).await;
    assert_eq!(ioc.str_of("ch40_units"), "VAC");
}

/// A numeric put past the ten modes (the mode mbbos define 10 and 8
/// states) changes no units and configures nothing, and the meter reads on
/// in the mode it has. C indexed `unit_strings` and `dmm_modes` past their
/// ends; `ch2_mode_sel 12` crashed its IOC.
#[epics_base_rs::epics_test]
async fn a_mode_past_the_ten_is_ignored() {
    let ioc = boot(&DMM, 4).await;
    booted(&ioc).await;
    ioc.step("ch_mode_sel", 3.0, &[":conf:fres", "rout:open:all"])
        .await;
    ioc.step("ch_mode_sel", 10.0, &[]).await;
    assert_eq!(ioc.str_of("units"), "OHM");
    ioc.read(&["rout:clos (@101)", "read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 101.3);

    ioc.step("ch_mode_sel", 1.0, &[":conf:volt:ac"]).await;
    assert_eq!(ioc.str_of("units"), "VAC");
    ioc.step("ch2_mode_sel", 12.0, &[]).await;
    assert_eq!(ioc.str_of("ch2_units"), "VDC");
    ioc.step("single_multi", 1.0, &[]).await;
    ioc.read(&[
        ":conf:volt:dc",
        "read?",
        "rout:clos (@102)",
        "read?",
        "rout:clos (@103)",
        "read?",
        "rout:clos (@104)",
        "read?",
    ])
    .await;
    assert_eq!(ioc.f64_of("Ch2_raw"), 102.0);
}

/// `init_dmm` during a scan waits for the scan to end. C's mode_change
/// reset `scan_chan` at once, so the reading in flight landed in `Ch1_raw`
/// and the scan read channel 2 again.
#[epics_base_rs::epics_test]
async fn init_dmm_during_a_scan_waits_for_its_end() {
    let ioc = boot(&DMM, 4).await;
    booted(&ioc).await;
    ioc.step("ch_mode_sel", 1.0, &[":conf:volt:ac"]).await;
    ioc.step("dmm_chan", 3.0, &["rout:clos (@104)"]).await;
    ioc.step("single_multi", 1.0, &[]).await;
    ioc.mock.hold_read(1);
    ioc.step(
        "done_read",
        1.0,
        &[
            ":conf:volt:dc",
            "rout:clos (@101)",
            "read?",
            "rout:clos (@102)",
            "read?",
        ],
    )
    .await;
    ioc.step("init_dmm", 1.0, &[]).await;
    ioc.mock.release_read();
    ioc.commands(&[
        "rout:clos (@103)",
        "read?",
        "rout:clos (@104)",
        "read?",
        "*rst",
        "form:elem read",
    ])
    .await;
    for n in 1..=4 {
        assert_eq!(
            ioc.f64_of(&format!("Ch{n}_raw")),
            100.0 + n as f64,
            "Ch{n}_raw"
        );
    }
    assert_eq!(ioc.f64_of("done_read"), 0.0);
    assert_eq!(ioc.f64_of("init_dmm"), 0.0);
}

/// A read behind an `init_dmm` reset closes its relay after the reset, and
/// so does the read after it. C's mode_change put the reset from its own
/// thread, so a read read_meter took meanwhile closed the relay first, and
/// the next read went out with every relay open.
#[epics_base_rs::epics_test]
async fn a_read_behind_a_reset_closes_its_relay_after_it() {
    let ioc = boot(&DMM, 4).await;
    booted(&ioc).await;
    ioc.step("dmm_chan", 2.0, &["rout:clos (@103)"]).await;
    let reset = ioc.hold("init_string");
    ioc.put("init_dmm", EpicsValue::Double(1.0)).await;
    ioc.step("done_read", 1.0, &[]).await;
    reset.release();
    ioc.commands(&[
        "*rst",
        ":conf:volt:dc",
        "rout:clos (@103)",
        "read?",
        "form:elem read",
    ])
    .await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 103.0);
    ioc.read(&["read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 103.0);
}
