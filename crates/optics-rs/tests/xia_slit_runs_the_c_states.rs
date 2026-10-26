//! The compiled `xia_slit.st` on `xia_slit.db`, talking to the [`Hsc1`]
//! mock through an asyn record on a `drvAsynIPPort`. The expected values
//! are the ones C xia_slit produced against the same mock and template.
//! There is one case per boundary of the state machine.
//!
//! Where the hand port these cases replace differed from C, the case
//! holds C's behaviour:
//! - A `DONE` whose first number does not scan keeps the stale `pos_a`.
//!   The hand port refused the reply.
//! - An `ERROR;` whose code does not scan keeps the previous `error` and
//!   shows that error's message, or an unknown error when that code is
//!   outside `hscErrors`. The hand port treated it as an unknown error.
//! - A move's dry read (`start_H_move`, `while (!s_sevr)`,
//!   xia_slit.st:943-951) is skipped when the last locate's timeout left
//!   SEVR set, so a queued `DONE` for the moving axis is processed. The
//!   hand port always dropped it.
//! - The reply FIFO (`INPUT_TO_FIFO`) overwrites on wrap, so
//!   `FIFO_SIZE` replies read in one go are all lost. The hand port
//!   dropped only the oldest reply.

mod hsc1_mock;
#[path = "hsc1_mock/rig.rs"]
mod rig;

use hsc1_mock::Hsc1;
use optics_rs::snl::xia_slit::xia_slit;
use rig::{Rig, eventually, f64_of, put, quiet, string_of};

const HV: &[&str] = &["H-1", "V-1"];
const IDS: &[(&str, &str)] = &[("T:hsc1:hID", "H-1"), ("T:hsc1:vID", "V-1")];

/// What init sends to the H-1 / V-1 pair.
const BOOT_SEEN: [&str; 13] = [
    "!ALL K", "!ALL P", "!H-1 R 5", "!H-1 R 6", "!H-1 R 7", "!H-1 R 1", "!H-1 R 2", "!V-1 R 7",
    "!V-1 R 6", "!V-1 R 5", "!V-1 R 1", "!V-1 R 2", "!ALL P",
];

/// H-1 has blades at 1 and 2 (raw 1200 and 1600 against origin 800) and
/// limits -2..10. V-1 keeps the defaults: both blades at 0, limits -1..10.
fn h1(hsc: &Hsc1) {
    hsc.set("H-1", |m| {
        m.outer = 4800;
        m.origin = 800;
        m.a = 1200;
        m.b = 1600;
    });
}

fn v(rig: &Rig, pv: &str) -> f64 {
    f64_of(&rig.db, &format!("T:hsc1:{pv}"))
}

fn s(rig: &Rig, pv: &str) -> String {
    string_of(&rig.db, &format!("T:hsc1:{pv}"))
}

async fn put_hsc(rig: &Rig, pv: &str, value: &str) {
    put(&rig.db, &format!("T:hsc1:{pv}"), value).await;
}

/// Wait for init to send its last command, then give it time to read the
/// replies.
async fn booted(rig: &Rig) {
    let n = BOOT_SEEN.len();
    assert!(
        eventually(|| rig.hsc.seen().len() >= n).await,
        "init sent {:?}",
        rig.hsc.seen()
    );
    quiet().await;
}

/// The program on H-1 / V-1 ([`h1`], then `setup`) after init.
async fn boot(setup: impl FnOnce(&Hsc1)) -> Rig {
    let rig = rig::start(
        &xia_slit,
        "xia_slit.db",
        HV,
        |hsc| {
            h1(hsc);
            setup(hsc);
        },
        IDS,
    )
    .await;
    booted(&rig).await;
    rig
}

/// The commands sent after init.
fn after_boot(rig: &Rig) -> Vec<String> {
    rig.hsc.seen()[BOOT_SEEN.len()..].to_vec()
}

/// Wait until a command starting with `prefix` has been sent after init,
/// then give the program time to read the replies.
async fn sent(rig: &Rig, prefix: &str) {
    assert!(
        eventually(|| after_boot(rig).iter().any(|c| c.starts_with(prefix))).await,
        "no {prefix:?} in {:?}",
        after_boot(rig)
    );
    quiet().await;
}

#[epics_base_rs::epics_test]
async fn init_reads_the_registers_and_sets_the_limits() {
    let rig = boot(|hsc| {
        hsc.set("H-1", |m| {
            m.csw = 70;
            m.step_delay = 3;
            m.backlash = 25;
        })
    })
    .await;
    assert_eq!(rig.hsc.seen(), BOOT_SEEN);
    for (pv, c) in [
        ("hStatPwrMI", 2.0),
        ("hStatLmtsBI", 1.0),
        ("hStatBanBI", 0.0),
        ("hStatEchoBI", 0.0),
        ("hStatLockBI", 0.0),
        ("hStatAliasBI", 1.0),
        ("hStatTextBI", 0.0),
        ("hStepDelayLI", 3.0),
        ("hGearBacklashLI", 25.0),
        ("vStatPwrMI", 0.0),
        ("hOuterLimitLI", 4800.0),
        ("hOriginLI", 800.0),
        ("l.LOPR", -2.0),
        ("l.HOPR", 10.0),
        ("width.LOPR", 0.0),
        ("width.HOPR", 20.0),
        ("h0.LOPR", -6.0),
        ("h0.HOPR", 6.0),
        ("t.HOPR", 10.0),
        ("lRB", 1.0),
        ("rRB", 2.0),
        ("l", 1.0),
        ("r", 2.0),
        ("width", 3.0),
        ("h0", 0.5),
        ("widthRB", 3.0),
        ("h0RB", 0.5),
        ("init", 0.0),
        ("hBusy", 0.0),
        ("h_Busy", 0.0),
        ("error", 0.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    assert_eq!(s(&rig, "port"), "T:asyn");
    assert_eq!(s(&rig, "errMsg"), "no error");
}

/// init refuses the pair: error 17 with `msg`, init stays set and nothing
/// is sent.
async fn refused(hid: &str, vid: &str, msg: &str) {
    let rig = rig::start(
        &xia_slit,
        "xia_slit.db",
        HV,
        h1,
        &[("T:hsc1:hID", hid), ("T:hsc1:vID", vid)],
    )
    .await;
    assert!(eventually(|| v(&rig, "error") == 17.0).await);
    quiet().await;
    assert_eq!(s(&rig, "errMsg"), msg);
    assert_eq!(v(&rig, "init"), 1.0);
    assert_eq!(rig.hsc.seen(), Vec::<String>::new());
}

#[epics_base_rs::epics_test]
async fn the_same_id_for_both_axes_is_refused() {
    refused("H-1", "H-1", "H & V IDs must be different").await;
}

#[epics_base_rs::epics_test]
async fn an_h_id_that_is_not_an_hsc_id_is_refused() {
    refused("Q", "V-1", "H ID not a valid HSC ID").await;
}

#[epics_base_rs::epics_test]
async fn an_empty_v_id_is_refused() {
    refused("H-1", "", "define V ID string").await;
}

/// xia_slit takes the bare serial-number form, which xiahsc refuses.
#[epics_base_rs::epics_test]
async fn a_bare_serial_number_id_is_taken() {
    let rig = rig::start(
        &xia_slit,
        "xia_slit.db",
        &["H-1", "V-1", "H1234"],
        h1,
        &[("T:hsc1:hID", "H1234"), ("T:hsc1:vID", "V-1")],
    )
    .await;
    booted(&rig).await;
    let want: Vec<String> = BOOT_SEEN
        .iter()
        .map(|c| c.replace("H-1", "H1234"))
        .collect();
    assert_eq!(rig.hsc.seen(), want);
    assert_eq!(v(&rig, "error"), 0.0);
    assert_eq!(s(&rig, "errMsg"), "no error");
    assert_eq!(v(&rig, "init"), 0.0);
}

#[epics_base_rs::epics_test]
async fn a_left_blade_move_sends_both_blades_and_polls_once() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "l", "1.5").await;
    sent(&rig, "!H-1 M ").await;
    assert_eq!(after_boot(&rig), ["!H-1 M 1400 1600", "!ALL P"]);
    for (pv, c) in [
        ("lRB", 1.5),
        ("rRB", 2.0),
        ("width", 3.5),
        ("h0", 0.25),
        ("hBusy", 0.0),
        ("h_Busy", 0.0),
        ("error", 0.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    assert_eq!(s(&rig, "errMsg"), "no error");
}

/// hOrient right-left swaps which motor is the left blade.
#[epics_base_rs::epics_test]
async fn a_right_left_axis_swaps_the_blades() {
    let rig = rig::start(
        &xia_slit,
        "xia_slit.db",
        HV,
        h1,
        &[
            ("T:hsc1:hID", "H-1"),
            ("T:hsc1:vID", "V-1"),
            ("T:hsc1:hOrient", "1"),
        ],
    )
    .await;
    booted(&rig).await;
    assert_eq!((v(&rig, "lRB"), v(&rig, "rRB")), (2.0, 1.0));
    put_hsc(&rig, "l", "2.5").await;
    sent(&rig, "!H-1 M ").await;
    assert_eq!(after_boot(&rig)[0], "!H-1 M 1200 1800");
    assert_eq!((v(&rig, "lRB"), v(&rig, "rRB")), (2.5, 1.0));
    assert_eq!(v(&rig, "hBusy"), 0.0);
}

#[epics_base_rs::epics_test]
async fn a_width_move_opens_both_blades_about_the_center() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "width", "4").await;
    sent(&rig, "!H-1 M ").await;
    assert_eq!(after_boot(&rig)[0], "!H-1 M 1400 1800");
    for (pv, c) in [("l", 1.5), ("r", 2.5), ("lRB", 1.5), ("rRB", 2.5)] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
}

/// With the outer limit at 4000, l.HOPR is 8. A target of 9 is within the
/// record's DRVH, so it is the program that refuses it, puts the targets
/// back and sends nothing.
#[epics_base_rs::epics_test]
async fn a_move_past_the_soft_limit_is_refused() {
    let rig = boot(|hsc| hsc.set("H-1", |m| m.outer = 4000)).await;
    assert_eq!(v(&rig, "l.HOPR"), 8.0);
    put_hsc(&rig, "l", "9").await;
    assert!(eventually(|| v(&rig, "error") == 15.0).await);
    quiet().await;
    for (pv, c) in [
        ("l", 1.0),
        ("width", 3.0),
        ("h0", 0.5),
        ("h_Busy", 0.0),
        ("hBusy", 0.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    assert_eq!(s(&rig, "errMsg"), "H soft limits exceeded");
    assert_eq!(rig.hsc.seen(), BOOT_SEEN);
}

/// From init on, every `!ALL P` is answered with `reply`, then a locate is
/// read. The result is checked against C's error, errMsg, hBusy, lRB and
/// rRB. V never goes busy and neither busy record is set.
async fn locate_reply(reply: &str, error: f64, msg: &str, h_busy: f64, l_rb: f64, r_rb: f64) {
    let rig = boot(|_| {}).await;
    rig.hsc.reply_to("!ALL P", reply);
    put_hsc(&rig, "locate", "1").await;
    sent(&rig, "!ALL P").await;
    assert_eq!(v(&rig, "error"), error, "{reply:?}");
    assert_eq!(s(&rig, "errMsg"), msg, "{reply:?}");
    for (pv, c) in [
        ("hBusy", h_busy),
        ("vBusy", 0.0),
        ("lRB", l_rb),
        ("rRB", r_rb),
        ("h_Busy", 0.0),
        ("v_Busy", 0.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{reply:?} {pv}");
    }
}

#[epics_base_rs::epics_test]
async fn an_error_reply_shows_the_controller_message() {
    locate_reply("%H-1 ERROR; 6", 6.0, "Value Out of Range", 0.0, 1.0, 2.0).await;
}

/// `sscanf(word3, "%d")` stops at the `;`.
#[epics_base_rs::epics_test]
async fn an_error_code_followed_by_a_semicolon_scans() {
    locate_reply("%H-1 ERROR; 6;", 6.0, "Value Out of Range", 0.0, 1.0, 2.0).await;
}

#[epics_base_rs::epics_test]
async fn an_error_reply_without_a_code_is_unknown() {
    locate_reply("%V-1 ERROR;", 16.0, "V-1: unknown error", 0.0, 1.0, 2.0).await;
}

/// An id past 24 characters is cut so the message still fits `errMsg`.
///
/// C defect fixed: C's `sprintf` ran this message past the 40-byte
/// `errMsg` for an id of 25 characters or more.
#[epics_base_rs::epics_test]
async fn an_unknown_error_from_a_long_id_fits_the_message() {
    locate_reply(
        "%ABCDEFGHIJKLMNOPQRSTUVWXYZ0123 ERROR;",
        16.0,
        "ABCDEFGHIJKLMNOPQRSTUVWX: unknown error",
        0.0,
        1.0,
        2.0,
    )
    .await;
}

/// The failed scan leaves error at its last value, 0, and the message is
/// `hscErrors[0]`.
#[epics_base_rs::epics_test]
async fn an_error_code_that_does_not_scan_keeps_the_last_error() {
    locate_reply("%H-1 ERROR; x", 0.0, "Missing Command", 0.0, 1.0, 2.0).await;
}

#[epics_base_rs::epics_test]
async fn the_last_error_code_shows_its_message() {
    locate_reply(
        "%H-1 ERROR; 13",
        13.0,
        "Invalid Motor Specified",
        0.0,
        1.0,
        2.0,
    )
    .await;
}

/// C defect fixed: C indexed the 14-entry `hscErrors` with any code.
#[epics_base_rs::epics_test]
async fn an_error_code_past_the_table_is_unknown() {
    locate_reply("%H-1 ERROR; 14", 14.0, "H-1: unknown error", 0.0, 1.0, 2.0).await;
}

/// The id is cut to 24 characters on this path as on the codeless one.
#[epics_base_rs::epics_test]
async fn a_long_id_with_a_code_past_the_table_fits_the_message() {
    locate_reply(
        "%ABCDEFGHIJKLMNOPQRSTUVWXY ERROR; 14",
        14.0,
        "ABCDEFGHIJKLMNOPQRSTUVWX: unknown error",
        0.0,
        1.0,
        2.0,
    )
    .await;
}

/// C defect fixed: C indexed `hscErrors` with a negative code.
#[epics_base_rs::epics_test]
async fn a_negative_error_code_is_unknown() {
    locate_reply("%H-1 ERROR; -1", -1.0, "H-1: unknown error", 0.0, 1.0, 2.0).await;
}

/// The refusal leaves error at 15, which the unscanned code keeps.
///
/// C defect fixed: C indexed `hscErrors` with that stale 15.
#[epics_base_rs::epics_test]
async fn an_error_code_that_does_not_scan_after_a_refusal_is_unknown() {
    let rig = boot(|hsc| hsc.set("H-1", |m| m.outer = 4000)).await;
    put_hsc(&rig, "l", "9").await;
    assert!(eventually(|| v(&rig, "error") == 15.0).await);
    quiet().await;
    rig.hsc.reply_to("!ALL P", "%H-1 ERROR; x");
    put_hsc(&rig, "locate", "1").await;
    sent(&rig, "!ALL P").await;
    assert_eq!(v(&rig, "error"), 15.0);
    assert_eq!(s(&rig, "errMsg"), "H-1: unknown error");
}

#[epics_base_rs::epics_test]
async fn a_busy_reply_sets_the_axis_busy() {
    locate_reply("%H-1 BUSY;", 0.0, "no error", 1.0, 1.0, 2.0).await;
}

/// Five words with `OK` second are a position report.
#[epics_base_rs::epics_test]
async fn an_ok_reply_with_two_numbers_is_a_position() {
    locate_reply("%H-1 OK 2000 2400 x", 0.0, "no error", 0.0, 3.0, 4.0).await;
}

#[epics_base_rs::epics_test]
async fn a_position_number_scans_up_to_its_first_letter() {
    locate_reply("%H-1 OK 2000abc 2400 x", 0.0, "no error", 0.0, 3.0, 4.0).await;
}

#[epics_base_rs::epics_test]
async fn a_done_position_scans_up_to_its_semicolon() {
    locate_reply("%H-1 1600; 2000 DONE;", 0.0, "no error", 0.0, 2.0, 3.0).await;
}

/// `pos_a` keeps 400, V-1's last `DONE`, so lRB is (400 - 800) / 400.
#[epics_base_rs::epics_test]
async fn a_done_whose_first_number_does_not_scan_keeps_the_stale_one() {
    locate_reply("%H-1 x 2000 DONE;", 0.0, "no error", 0.0, -1.0, 3.0).await;
}

#[epics_base_rs::epics_test]
async fn a_reply_it_does_not_understand_changes_nothing() {
    locate_reply("%H-1 WHAT", 0.0, "no error", 0.0, 1.0, 2.0).await;
}

/// `%s` splits on C white space only, so the reply is one word.
#[epics_base_rs::epics_test]
async fn a_no_break_space_does_not_split_words() {
    locate_reply("%H-1\u{a0}BUSY;", 0.0, "no error", 0.0, 1.0, 2.0).await;
}

/// The last locate's read ends in a timeout, so SEVR is still set when
/// the move starts and the dry read before it does not run. The queued
/// `DONE` for H-1 is then processed: lRB (1000 - 800) / 400 and rRB
/// (1700 - 800) / 400. The move itself polls busy.
#[epics_base_rs::epics_test]
async fn a_queued_done_for_the_moving_axis_is_processed() {
    let rig = boot(|_| {}).await;
    rig.hsc.set("H-1", |m| m.busy_polls = 1000);
    rig.hsc.send(" %H-1 1000 1700 DONE;");
    put_hsc(&rig, "l", "1.5").await;
    sent(&rig, "!H-1 M ").await;
    assert_eq!(after_boot(&rig)[..2], ["!H-1 M 1400 1600", "!ALL P"]);
    for (pv, c) in [
        ("lRB", 0.5),
        ("rRB", 2.25),
        ("hBusy", 1.0),
        ("h_Busy", 1.0),
        ("l", 1.5),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
}

#[epics_base_rs::epics_test]
async fn stop_kills_every_axis() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "stop", "1").await;
    sent(&rig, "!ALL K").await;
    assert_eq!(v(&rig, "stop"), 0.0);
}

/// `0 I` sets both motors of every module to their origin.
#[epics_base_rs::epics_test]
async fn calibrate_zeroes_every_axis() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "calib", "1").await;
    sent(&rig, "!ALL 0 I").await;
    assert_eq!(after_boot(&rig)[..2], ["!ALL 0 I", "!ALL P"]);
    for (pv, c) in [("calib", 0.0), ("lRB", 0.0), ("rRB", 0.0), ("l", 0.0)] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
}

/// `"%*s %*s %d"` reads `600;` as 600, so the low limit is (0 - 600) / 400.
/// A target of -1.1 is clamped by the record's DRVL of -1 before the
/// program sees it.
#[epics_base_rs::epics_test]
async fn an_origin_reply_scans_up_to_its_semicolon() {
    let rig = boot(|hsc| hsc.reply_to("!H-1 R 2", "%H-1 OK 600;")).await;
    for (pv, c) in [
        ("hOriginLI", 600.0),
        ("hOuterLimitLI", 4800.0),
        ("l.LOPR", -1.5),
        ("l.HOPR", 10.5),
        ("error", 0.0),
        ("lRB", 1.5),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    put_hsc(&rig, "l", "-1.1").await;
    sent(&rig, "!H-1 M ").await;
    assert_eq!(after_boot(&rig), ["!H-1 M 200 1600", "!ALL P"]);
    assert_eq!((v(&rig, "l"), v(&rig, "lRB")), (-1.0, -1.0));
    assert_eq!(v(&rig, "error"), 0.0);
    assert_eq!(s(&rig, "errMsg"), "no error");
}

/// An origin reply that does not scan counts as an error in init_limits, so
/// none of the H limits are written. They keep the template's values, not
/// the 20 an 8400 outer limit would give, and the program keeps its
/// default origin of 400 without writing it. A target of 15 is clamped by
/// the record's DRVH of 10.
#[epics_base_rs::epics_test]
async fn an_origin_reply_that_does_not_scan_keeps_the_template_limits() {
    let rig = boot(|hsc| {
        hsc.set("H-1", |m| m.outer = 8400);
        hsc.reply_to("!H-1 R 2", "%H-1 OK");
    })
    .await;
    for (pv, c) in [
        ("hOriginLI", 0.0),
        ("hOuterLimitLI", 8400.0),
        ("l.LOPR", -1.0),
        ("l.HOPR", 10.0),
        ("r.HOPR", 10.0),
        ("r.DRVH", 10.0),
        ("width.HOPR", 20.0),
        ("error", 0.0),
        ("lRB", 2.0),
        ("rRB", 3.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    assert_eq!(s(&rig, "errMsg"), "no error");
    put_hsc(&rig, "r", "15").await;
    sent(&rig, "!H-1 M ").await;
    assert_eq!(after_boot(&rig), ["!H-1 M 1200 4400", "!ALL P"]);
    assert_eq!((v(&rig, "r"), v(&rig, "rRB")), (10.0, 10.0));
    assert_eq!(v(&rig, "error"), 0.0);
}

/// `n` modules, H-1 first and V-1 last, so each `!ALL P` gets `n` replies.
/// Init reads the `K` and `P` replies of every module in one go, `2n` of
/// them, which wraps the FIFO and loses H-1's position. Both blades
/// therefore read 0 after init. Then both axes move on the controller and
/// a locate is read.
async fn fifo(n: usize) -> Rig {
    let ids: Vec<String> = std::iter::once("H-1".to_string())
        .chain((1..n - 1).map(|k| format!("X{k:02}")))
        .chain(std::iter::once("V-1".to_string()))
        .collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let rig = rig::start(&xia_slit, "xia_slit.db", &ids, h1, IDS).await;
    booted(&rig).await;
    assert_eq!((v(&rig, "lRB"), v(&rig, "rRB")), (0.0, 0.0));
    rig.hsc.set("H-1", |m| (m.a, m.b) = (2000, 2400));
    rig.hsc.set("V-1", |m| (m.a, m.b) = (800, 1200));
    put_hsc(&rig, "locate", "1").await;
    sent(&rig, "!ALL P").await;
    quiet().await;
    rig
}

#[epics_base_rs::epics_test]
async fn one_reply_short_of_the_fifo_size_is_read_in_full() {
    let rig = fifo(39).await;
    for (pv, c) in [("lRB", 3.0), ("rRB", 4.0), ("tRB", 1.0), ("bRB", 2.0)] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
}

/// The 40th reply moves the write index back onto the read index, so the
/// FIFO looks empty and no reply is processed.
#[epics_base_rs::epics_test]
async fn fifo_size_replies_are_all_lost() {
    let rig = fifo(40).await;
    for (pv, c) in [("lRB", 0.0), ("rRB", 0.0), ("tRB", 0.0), ("bRB", 0.0)] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    assert_eq!(v(&rig, "error"), 0.0);
    assert_eq!(s(&rig, "errMsg"), "no error");
}

/// Put `locate` and wait until its `!ALL P` is sent.
async fn locate(rig: &Rig) {
    let n = rig.hsc.seen().len();
    put_hsc(rig, "locate", "1").await;
    assert!(
        eventually(|| rig.hsc.seen().len() > n).await,
        "no locate after {:?}",
        rig.hsc.seen()
    );
}

/// A silent controller fails a locate's read with no reply. Two such
/// locates in a row leave error 0; the third shows error 18.
///
/// C defect fixed: nothing enters C's `comm_error`, so a silent
/// controller kept "no error".
#[epics_base_rs::epics_test]
async fn a_third_locate_without_a_reply_is_a_comm_error() {
    let rig = boot(|_| {}).await;
    rig.hsc.set_silent(true);
    locate(&rig).await;
    locate(&rig).await;
    quiet().await;
    assert_eq!(v(&rig, "error"), 0.0);
    assert_eq!(s(&rig, "errMsg"), "no error");
    locate(&rig).await;
    let state = || (v(&rig, "error"), s(&rig, "errMsg"));
    assert!(
        eventually(|| state() == (18.0, "communications error".into())).await,
        "error, errMsg {:?}",
        state()
    );
}

/// A move to a silent controller polls without a reply until the error,
/// which releases hBusy and the h_Busy busy record.
///
/// C defect fixed: C kept both busy and polled every 0.2 s.
#[epics_base_rs::epics_test]
async fn a_comm_error_releases_a_moving_axis() {
    let rig = boot(|_| {}).await;
    rig.hsc.set_silent(true);
    put_hsc(&rig, "l", "1.5").await;
    sent(&rig, "!H-1 M ").await;
    let state = || {
        (
            ["error", "hBusy", "h_Busy"].map(|pv| v(&rig, pv)),
            s(&rig, "errMsg"),
        )
    };
    assert!(
        eventually(|| state() == ([18.0, 0.0, 0.0], "communications error".into())).await,
        "error, hBusy, h_Busy, errMsg {:?}",
        state()
    );
    assert_eq!((v(&rig, "lRB"), v(&rig, "vBusy")), (1.0, 0.0));
}

/// Every init_limits read times out, so init ends in error 18 after one
/// locate. The limits keep the template's values.
///
/// C defect fixed: C counted the timeouts but kept "no error".
#[epics_base_rs::epics_test]
async fn a_controller_silent_from_boot_is_a_comm_error() {
    let rig = rig::start(
        &xia_slit,
        "xia_slit.db",
        HV,
        |hsc| {
            h1(hsc);
            hsc.set_silent(true);
        },
        IDS,
    )
    .await;
    booted(&rig).await;
    assert_eq!(rig.hsc.seen(), BOOT_SEEN);
    for (pv, c) in [
        ("error", 18.0),
        ("hBusy", 0.0),
        ("l.LOPR", -1.0),
        ("l.HOPR", 10.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    assert_eq!(s(&rig, "errMsg"), "communications error");
}

/// One reply after the error clears it through the reply's `if (error)`.
#[epics_base_rs::epics_test]
async fn one_reply_clears_a_comm_error() {
    let rig = boot(|_| {}).await;
    rig.hsc.set_silent(true);
    for _ in 0..3 {
        locate(&rig).await;
    }
    assert!(eventually(|| v(&rig, "error") == 18.0).await);
    rig.hsc.reply_to("!ALL P", "%H-1 1600 2000 DONE;");
    rig.hsc.set_silent(false);
    locate(&rig).await;
    assert!(eventually(|| v(&rig, "error") == 0.0).await);
    quiet().await;
    assert_eq!(s(&rig, "errMsg"), "no error");
    assert_eq!((v(&rig, "lRB"), v(&rig, "rRB")), (2.0, 3.0));
}

const CHILD: &str = "XIA_SLIT_DEBUG_CHILD";

/// `DEBUG_PRINT1`'s header names the program (`seqPVmacros.h:228-231`).
///
/// C defect fixed: C never assigned `SNLtaskName` and printed it, NULL,
/// with `%s`.
#[epics_base_rs::epics_test]
async fn debug_lines_name_the_program() {
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "debug_lines_name_the_program",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let line = stdout
            .lines()
            .find(|l| l.ends_with("WRITE_HSC: <!ALL P>"))
            .expect("no WRITE_HSC: <!ALL P> line");
        assert!(line.contains(",hsc1,10> "), "{line}");
        return;
    }
    let pre = [IDS[0], IDS[1], ("T:hsc1:debug", "10")];
    let rig = rig::start(&xia_slit, "xia_slit.db", HV, h1, &pre).await;
    booted(&rig).await;
}
