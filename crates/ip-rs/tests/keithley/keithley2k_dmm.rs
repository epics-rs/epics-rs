//! The compiled `Keithley2kDMM.st` on the shipped `Keithley2kDMM_mf.db`,
//! driving a 2700 mock through the template's devXxStrParm records. Each
//! step's commands, and the values it leaves, are the ones a C IOC (seq
//! 2.2.9, asyn, calc, ip's devXxStrParm) sent and posted against the same
//! mock, template and puts.

use std::time::Duration;

use epics_base_rs::runtime::task;
use epics_base_rs::types::{EpicsValue, PvString};

use crate::harness::{Ioc, Meter, boot};
use crate::support::eventually;

/// The 2700 behind `Dmm=D1:`.
const DMM: Meter = Meter {
    program: "Keithley2kDMM",
    template: "Keithley2kDMM_mf",
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
        ":conf:temp;:temp:tc:type n",
        ":conf:temp;:temp:tc:type t",
        ":conf:temp;:temp:tc:type e",
        ":conf:temp;:temp:tc:type r",
        ":conf:temp;:temp:tc:type s",
        ":conf:temp;:temp:tc:type b",
        ":conf:curr:dc",
        ":conf:curr:ac",
    ],
    read: "read?",
};

/// Boot: `mode_change` configures DC volts and closes channel 1, and past
/// 10 channels closes channel 11 too; `read_meter` resets the meter
/// through `init_string`. The two state sets race, in C too, so only each
/// one's own order is fixed.
async fn booted(ioc: &Ioc, channels: u32) {
    let mut mode = vec![":conf:volt:dc", "rout:clos (@101)"];
    if channels > 10 {
        mode.push("rout:clos (@111)");
    }
    assert!(eventually(|| ioc.mock.pending() > mode.len()).await);
    task::sleep(Duration::from_millis(250)).await;
    let got = ioc.mock.take();
    let (reset, rest): (Vec<_>, Vec<_>) = got.iter().partition(|c| *c == "*rst");
    assert_eq!(reset, ["*rst"], "{got:?}");
    assert_eq!(rest, mode, "{got:?}");
}

/// Single-channel reads: each sends `read?` behind whatever mode and relay
/// changed since the last, and `Dmm_raw` takes the reading. The channel and
/// mode are put first, as the boot race leaves `previous_mode` and
/// `previous_chan` at either 0 or -1.
#[epics_base_rs::epics_test]
async fn single_reads_follow_the_selected_channel_and_mode() {
    let ioc = boot(&DMM, 4).await;
    booted(&ioc, 4).await;
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

    // With 4 channels, dmm_chan2 closes nothing itself; the 11-22 range
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
    booted(&ioc, 4).await;
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

    ioc.step("init_dmm", 1.0, &["*rst"]).await;
    assert_eq!(ioc.f64_of("init_dmm"), 0.0);
}

/// On a 2700, channels 21 and 22 measure current only: their modes, DC and
/// AC amps, are `dmm_modes` 13 and 14. `dmm_chan2` closes channels 11-22
/// past 10 channels.
#[epics_base_rs::epics_test]
async fn a_2700_reads_channels_21_and_22_in_amps() {
    let ioc = boot(&DMM, 22).await;
    booted(&ioc, 22).await;
    assert!(eventually(|| ioc.str_of("ch21_units") == "ADC").await);
    ioc.step("ch21_mode_sel", 1.0, &[]).await;
    assert_eq!(ioc.str_of("ch21_units"), "AAC");
    assert_eq!(ioc.str_of("ch22_units"), "ADC");
    for n in 1..=20 {
        ioc.put(&format!("Ch{n}_on_off"), EpicsValue::Double(1.0))
            .await;
    }
    ioc.step("single_multi", 1.0, &[]).await;
    ioc.read(&[
        ":conf:curr:ac",
        "rout:clos (@121)",
        "read?",
        ":conf:curr:dc",
        "rout:clos (@122)",
        "read?",
    ])
    .await;
    assert_eq!(ioc.f64_of("Ch21_raw"), 121.14);
    assert_eq!(ioc.f64_of("Ch22_raw"), 122.13);

    ioc.step("single_multi", 0.0, &[]).await;
    ioc.step("dmm_ch_range", 1.0, &[]).await;
    ioc.step("dmm_chan2", 11.0, &["rout:clos (@122)"]).await;
    ioc.read(&[":conf:volt:dc", "read?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 122.0);
}
