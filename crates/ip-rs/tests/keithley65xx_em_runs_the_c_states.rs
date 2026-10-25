//! The compiled `Keithley65xxEM.st` on the shipped `Keithley65xxEM.db`,
//! driving a 6517A mock through the template's devXxStrParm records. Each
//! step's commands, and the values it leaves, are the ones a C IOC (seq
//! 2.2.9, asyn, calc, ip's devXxStrParm) sent and posted against the same
//! mock, template and puts.

mod keithley;
mod support;

use std::time::Duration;

use epics_base_rs::runtime::task;
use epics_base_rs::types::{EpicsValue, PvString};

use keithley::{Ioc, Meter, boot};
use support::eventually;

/// The 6517A behind `Dmm=E1:`.
const EM: Meter = Meter {
    program: "Keithley65xxEM",
    dmm: "E1:",
    model: "6517A",
    modes: &[
        ":sens:func 'volt'",
        ":sens:func 'curr'",
        ":sens:func 'res'",
        ":sens:func 'char'",
    ],
    read: "*trg;fetch?",
};

/// `init_string` through `init_string6`, with the template's 1 s and two
/// 0.1 s delays between them.
const RESET: [&str; 7] = [
    "*rst",
    "route:open:all",
    "init:cont on",
    "trig:coun inf",
    "trig:sour bus",
    "syst:zcheck off",
    "form:elem read",
];

/// Boot with 4 channels: `mode_change` configures DC volts and closes
/// channel 1, `read_meter` resets the meter. The two state sets race, in C
/// too, so only each one's own order is fixed.
async fn booted(ioc: &Ioc) {
    let got = ioc.boot_commands().await;
    let (reset, mode): (Vec<_>, Vec<_>) = got.iter().partition(|c| RESET.contains(&c.as_str()));
    assert_eq!(reset, RESET, "{got:?}");
    assert_eq!(mode, [":sens:func 'volt'", "rout:clos (@1)"], "{got:?}");
}

/// Single-channel reads: each sends `*trg;fetch?` behind whatever relay
/// changed since the last, and `Dmm_raw` takes the reading. The channel and
/// mode are put first, as the boot race leaves `previous_mode` and
/// `previous_chan` at either 0 or -1.
#[epics_base_rs::epics_test]
async fn single_reads_follow_the_selected_channel_and_mode() {
    let ioc = boot(&EM, 4).await;
    booted(&ioc).await;
    assert_eq!(ioc.str_of("model"), "6517A");
    assert_eq!(ioc.f64_of("channels"), 4.0);
    assert_eq!(ioc.str_of("units"), "VOLT");

    ioc.step("dmm_chan", 2.0, &["rout:clos (@3)"]).await;
    ioc.step("ch_mode_sel", 1.0, &[":sens:func 'curr'"]).await;
    assert_eq!(ioc.str_of("units"), "AMP");
    ioc.read(&["*trg;fetch?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 3.1);

    // Coulombs opens every relay, a branch kept from the 2000's 4-wire
    // ohms, so the next read closes channel 3 again.
    ioc.step("ch_mode_sel", 3.0, &[":sens:func 'char'", "rout:open:all"])
        .await;
    assert_eq!(ioc.str_of("units"), "COULOMB");
    assert_eq!(ioc.str_of("ch_close"), "rout:open:all");
    ioc.read(&["rout:clos (@3)", "*trg;fetch?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 3.3);
}

/// A multi-channel scan reads each channel that is on, in its own mode,
/// into its `Ch<n>_raw`; a mode change goes through `dmm_delay_read`. A
/// channel's mode change rewrites every channel's units label. `init_dmm`
/// resets the meter.
#[epics_base_rs::epics_test]
async fn a_multi_scan_reads_each_channel_that_is_on_in_its_own_mode() {
    let ioc = boot(&EM, 4).await;
    booted(&ioc).await;
    ioc.step("ch_mode_sel", 3.0, &[":sens:func 'char'", "rout:open:all"])
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
    for (n, units) in [(1, "VOLT"), (2, "AMP"), (3, "VOLT"), (4, "VOLT")] {
        assert_eq!(ioc.str_of(&format!("ch{n}_units")), units, "ch{n}");
    }
    assert_eq!(ioc.str_of("ch5_units"), "VDC");
    ioc.step("Ch3_on_off", 1.0, &[]).await;

    ioc.read(&[
        ":sens:func 'volt'",
        "rout:clos (@1)",
        "*trg;fetch?",
        ":sens:func 'curr'",
        "rout:clos (@2)",
        "*trg;fetch?",
        ":sens:func 'volt'",
        "rout:clos (@4)",
        "*trg;fetch?",
    ])
    .await;
    assert_eq!(ioc.f64_of("Ch1_raw"), 1.0);
    assert_eq!(ioc.f64_of("Ch2_raw"), 2.1);
    assert_ne!(ioc.f64_of("Ch3_raw.UDF"), 0.0, "Ch3_raw was read");
    assert_eq!(ioc.f64_of("Ch4_raw"), 4.0);

    ioc.step("init_dmm", 1.0, &RESET).await;
    assert_eq!(ioc.f64_of("init_dmm"), 0.0);
}

/// A `channels` macro past the program's 10 channels is cut to 10. C
/// scanned an 11th relay through `ch_on_off[10]` and `ch_mode[10]`, and its
/// `pvPut(ch_raw[10])` processed `conf_dcv` instead of a reading.
#[epics_base_rs::epics_test]
async fn a_channels_macro_past_ten_is_cut_to_ten() {
    let ioc = boot(&EM, 11).await;
    ioc.boot_commands().await;
    assert_eq!(ioc.f64_of("channels"), 10.0);
    ioc.step("dmm_chan", 2.0, &["rout:clos (@3)"]).await;
    ioc.step("ch_mode_sel", 1.0, &[":sens:func 'curr'"]).await;
    ioc.step("ch10_mode_sel", 1.0, &[]).await;
    assert_eq!(ioc.str_of("ch10_units"), "AMP");
    ioc.step("single_multi", 1.0, &[]).await;

    let mut scan = vec![":sens:func 'volt'".to_string()];
    for n in 1..=10 {
        if n == 10 {
            scan.push(":sens:func 'curr'".into());
        }
        scan.push(format!("rout:clos (@{n})"));
        scan.push("*trg;fetch?".into());
    }
    ioc.read(&scan.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
    assert_eq!(ioc.f64_of("Ch10_raw"), 10.1);
}

/// A single read that sends a mode command reads through `dmm_delay_read`,
/// 0.2 s on, as a multi-channel read does. C set `previous_mode` before its
/// test for the delay, so it read at once: here after `init_dmm`, it sent
/// `*trg;fetch?` 0 ms behind `:sens:func 'curr'`.
#[epics_base_rs::epics_test]
async fn a_single_read_after_a_mode_command_is_delayed() {
    let ioc = boot(&EM, 4).await;
    booted(&ioc).await;
    ioc.step("dmm_chan", 2.0, &["rout:clos (@3)"]).await;
    ioc.step("ch_mode_sel", 1.0, &[":sens:func 'curr'"]).await;
    ioc.step("init_dmm", 1.0, &RESET).await;

    ioc.put("done_read", EpicsValue::Double(1.0)).await;
    assert!(eventually(|| ioc.mock.pending() >= 2).await);
    task::sleep(Duration::from_millis(100)).await;
    assert_eq!(ioc.mock.take(), [":sens:func 'curr'", "rout:clos (@3)"]);
    ioc.commands(&["*trg;fetch?"]).await;
    assert!(eventually(|| ioc.f64_of("done_read") == 0.0).await);
    assert_eq!(ioc.f64_of("Dmm_raw"), 3.1);
    assert_eq!(ioc.str_of("units"), "AMP");
}

/// A numeric put past the four modes (the mode mbbos define four states)
/// changes no units and configures nothing, and the meter reads on in the
/// mode it has. C indexed `unit_strings` and `dmm_modes` past their ends:
/// `ch_mode_sel 4` set `units` to "k:", and `ch2_mode_sel 6` crashed its
/// IOC.
#[epics_base_rs::epics_test]
async fn a_mode_past_the_four_is_ignored() {
    let ioc = boot(&EM, 4).await;
    booted(&ioc).await;
    ioc.step("dmm_chan", 2.0, &["rout:clos (@3)"]).await;
    ioc.step("ch_mode_sel", 1.0, &[":sens:func 'curr'"]).await;
    ioc.step("ch_mode_sel", 4.0, &[]).await;
    assert_eq!(ioc.str_of("units"), "AMP");
    ioc.read(&["*trg;fetch?"]).await;
    assert_eq!(ioc.f64_of("Dmm_raw"), 3.1);

    ioc.step("ch2_mode_sel", 6.0, &[]).await;
    assert_eq!(ioc.str_of("ch2_units"), "VOLT");
    ioc.step("single_multi", 1.0, &[]).await;
    ioc.read(&[
        ":sens:func 'volt'",
        "rout:clos (@1)",
        "*trg;fetch?",
        "rout:clos (@2)",
        "*trg;fetch?",
        "rout:clos (@3)",
        "*trg;fetch?",
        "rout:clos (@4)",
        "*trg;fetch?",
    ])
    .await;
    assert_eq!(ioc.f64_of("Ch2_raw"), 2.0);
}
