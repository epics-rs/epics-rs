//! The compiled `xiahsc.st` on `xiahsc.db`, talking to the [`Hsc1`] mock
//! through an asyn record on a `drvAsynIPPort`. The expected values are the
//! ones C xiahsc produced against the same mock and template. There is one
//! case per boundary of the state machine.
//!
//! Where the hand port these cases replace differed from C, the case holds
//! C's behaviour:
//! - The hand port was a serial actor fed by a command channel, and nothing
//!   started it, so no template PV reached it. The program now runs on the
//!   template as `seq xiahsc` runs it.
//! - The hand port stopped at the first ID check that failed and had no
//!   empty-ID check. C makes every check and shows the last failing one's
//!   message, so an empty vID reads `define V ID string`.
//! - The hand port's communications error read `communications error`. C's
//!   reads `communications error: <SEVR>`, and C only gets there through the
//!   asyn record's SEVR after a read times out.

mod hsc1_mock;
#[path = "hsc1_mock/rig.rs"]
mod rig;

use std::time::Duration;

use epics_base_rs::runtime::task;
use hsc1_mock::Hsc1;
use optics_rs::snl::xiahsc::xiahsc;
use rig::{Rig, eventually, f64_of, put, quiet, string_of};

const HV: &[&str] = &["H-1", "V-1"];
const IDS: &[(&str, &str)] = &[("T:hsc1:hID", "H-1"), ("T:hsc1:vID", "V-1")];

/// What init sends to the H-1 / V-1 pair.
const BOOT_SEEN: [&str; 7] = [
    "!ALL K", "!H-1 R 1", "!H-1 R 2", "!V-1 R 1", "!V-1 R 2", "!H-1 P", "!V-1 P",
];

/// H-1 has blades at 1 and 2 (raw 1200 and 1600 against origin 800) and
/// limits -2..10. V-1 keeps the defaults: both blades at 0, limits -1..10.
/// Both answer `P` in the `P <a> <b> DONE;` form xiahsc scans.
fn h1(hsc: &Hsc1) {
    hsc.set("H-1", |m| {
        m.outer = 4800;
        m.origin = 800;
        m.a = 1200;
        m.b = 1600;
        m.echo = true;
    });
    hsc.set("V-1", |m| m.echo = true);
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

/// Wait for `n` commands, then give the program time to read the replies.
async fn seen_n(rig: &Rig, n: usize) {
    assert!(
        eventually(|| rig.hsc.seen().len() >= n).await,
        "sent {:?}",
        rig.hsc.seen()
    );
    quiet().await;
}

/// The program on H-1 / V-1 ([`h1`], then `setup`, then the `pre` puts)
/// after init.
async fn boot_with(setup: impl FnOnce(&Hsc1), pre: &[(&str, &str)]) -> Rig {
    let rig = rig::start(
        &xiahsc,
        "xiahsc.db",
        HV,
        |hsc| {
            h1(hsc);
            setup(hsc);
        },
        pre,
    )
    .await;
    seen_n(&rig, BOOT_SEEN.len()).await;
    rig
}

async fn boot(setup: impl FnOnce(&Hsc1)) -> Rig {
    boot_with(setup, IDS).await
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

/// The H axis reads blades `l` and `r` with no move in progress.
fn h_axis(rig: &Rig, l: f64, r: f64) {
    for (pv, c) in [
        ("l", l),
        ("r", r),
        ("lRB", l),
        ("rRB", r),
        ("width", l + r),
        ("h0", (r - l) / 2.0),
        ("widthRB", l + r),
        ("h0RB", (r - l) / 2.0),
        ("hBusy", 0.0),
        ("vBusy", 0.0),
        ("error", 0.0),
    ] {
        assert_eq!(v(rig, pv), c, "{pv}");
    }
    assert_eq!(s(rig, "errMsg"), "no error");
}

#[epics_base_rs::epics_test]
async fn init_reads_the_registers_and_sets_the_limits() {
    let rig = boot(|_| {}).await;
    assert_eq!(rig.hsc.seen(), BOOT_SEEN);
    for (pv, c) in [
        ("l.LOPR", -2.0),
        ("l.HOPR", 10.0),
        ("r.LOPR", -2.0),
        ("r.HOPR", 10.0),
        ("width.LOPR", 0.0),
        ("width.HOPR", 20.0),
        ("h0.LOPR", -6.0),
        ("h0.HOPR", 6.0),
        ("t.LOPR", -1.0),
        ("t.HOPR", 10.0),
        ("height.LOPR", 0.0),
        ("height.HOPR", 20.0),
        ("v0.LOPR", -5.5),
        ("v0.HOPR", 5.5),
        ("t", 0.0),
        ("b", 0.0),
        ("tRB", 0.0),
        ("bRB", 0.0),
        ("height", 0.0),
        ("v0", 0.0),
        ("heightRB", 0.0),
        ("v0RB", 0.0),
        ("init", 0.0),
        ("enable", 1.0),
        ("locate", 0.0),
        ("stop", 0.0),
        ("calib", 0.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    h_axis(&rig, 1.0, 2.0);
    assert_eq!(s(&rig, "port"), "T:asyn");
}

/// init refuses the pair: error 17 with `msg`, init stays set and nothing
/// is sent.
async fn refused(ids: &[&str], hid: &str, vid: &str, msg: &str) {
    let rig = rig::start(
        &xiahsc,
        "xiahsc.db",
        ids,
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
    refused(HV, "H-1", "H-1", "H & V IDs must be different").await;
}

#[epics_base_rs::epics_test]
async fn an_h_id_that_is_not_an_hsc_id_is_refused() {
    refused(HV, "Q", "V-1", "H ID not a valid HSC ID").await;
}

/// `"%c-%d%s"` needs the hyphen; xia_slit's bare form is not tried here.
#[epics_base_rs::epics_test]
async fn a_bare_serial_number_id_is_refused() {
    refused(
        &["H-1", "V-1", "H1234"],
        "H1234",
        "V-1",
        "H ID not a valid HSC ID",
    )
    .await;
}

/// Both the `V ID not a valid HSC ID` and the empty-ID checks fail; the
/// last message put is the one shown.
#[epics_base_rs::epics_test]
async fn an_empty_v_id_is_refused() {
    refused(HV, "H-1", "", "define V ID string").await;
}

#[epics_base_rs::epics_test]
async fn the_xiahsc_prefixed_id_form_is_taken() {
    let rig = rig::start(
        &xiahsc,
        "xiahsc.db",
        &["XIAHSC-H-1", "V-1"],
        |hsc| {
            hsc.set("XIAHSC-H-1", |m| {
                m.outer = 4800;
                m.origin = 800;
                m.a = 1200;
                m.b = 1600;
                m.echo = true;
            });
            hsc.set("V-1", |m| m.echo = true);
        },
        &[("T:hsc1:hID", "XIAHSC-H-1"), ("T:hsc1:vID", "V-1")],
    )
    .await;
    seen_n(&rig, BOOT_SEEN.len()).await;
    let want: Vec<String> = BOOT_SEEN
        .iter()
        .map(|c| c.replace("H-1", "XIAHSC-H-1"))
        .collect();
    assert_eq!(rig.hsc.seen(), want);
    assert_eq!(v(&rig, "init"), 0.0);
    h_axis(&rig, 1.0, 2.0);
}

#[epics_base_rs::epics_test]
async fn a_left_blade_move_sends_both_blades_and_polls_once() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "l", "1.5").await;
    sent(&rig, "!H-1 M ").await;
    assert_eq!(after_boot(&rig), ["!H-1 M 1400 1600", "!H-1 P", "!V-1 P"]);
    h_axis(&rig, 1.5, 2.0);
}

/// hOrient right-left swaps which motor is the left blade.
#[epics_base_rs::epics_test]
async fn a_right_left_axis_swaps_the_blades() {
    let rig = boot_with(
        |_| {},
        &[
            ("T:hsc1:hID", "H-1"),
            ("T:hsc1:vID", "V-1"),
            ("T:hsc1:hOrient", "1"),
        ],
    )
    .await;
    assert_eq!((v(&rig, "lRB"), v(&rig, "rRB")), (2.0, 1.0));
    put_hsc(&rig, "l", "2.5").await;
    sent(&rig, "!H-1 M ").await;
    assert_eq!(after_boot(&rig), ["!H-1 M 1200 1800", "!H-1 P", "!V-1 P"]);
    h_axis(&rig, 2.5, 1.0);
}

#[epics_base_rs::epics_test]
async fn a_width_move_opens_both_blades_about_the_center() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "width", "4").await;
    sent(&rig, "!H-1 M ").await;
    assert_eq!(after_boot(&rig), ["!H-1 M 1400 1800", "!H-1 P", "!V-1 P"]);
    h_axis(&rig, 1.5, 2.5);
}

#[epics_base_rs::epics_test]
async fn a_height_move_opens_both_v_blades_about_the_center() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "height", "2").await;
    sent(&rig, "!V-1 M ").await;
    assert_eq!(after_boot(&rig), ["!V-1 M 800 800", "!H-1 P", "!V-1 P"]);
    for (pv, c) in [
        ("t", 1.0),
        ("b", 1.0),
        ("tRB", 1.0),
        ("bRB", 1.0),
        ("heightRB", 2.0),
        ("v0RB", 0.0),
        ("vBusy", 0.0),
        ("error", 0.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    assert_eq!(s(&rig, "errMsg"), "no error");
}

/// With the outer limit at 4000, l.HOPR is 8. A target of 9 is within the
/// record's HOPR of 10, so it is the program that refuses it, puts the
/// target back and sends nothing.
#[epics_base_rs::epics_test]
async fn a_move_past_the_soft_limit_is_refused() {
    let rig = boot(|hsc| hsc.set("H-1", |m| m.outer = 4000)).await;
    assert_eq!(v(&rig, "l.HOPR"), 8.0);
    put_hsc(&rig, "l", "9").await;
    assert!(eventually(|| v(&rig, "error") == 15.0).await);
    quiet().await;
    for (pv, c) in [
        ("l", 1.0),
        ("r", 2.0),
        ("lRB", 1.0),
        ("width", 3.0),
        ("h0", 0.5),
        ("hBusy", 0.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    assert_eq!(s(&rig, "errMsg"), "H soft limits exceeded");
    assert_eq!(rig.hsc.seen(), BOOT_SEEN);
}

/// Three `BUSY;` replies, then `DONE`: the axis is busy until the fourth
/// poll, each 0.25 s apart.
#[epics_base_rs::epics_test]
async fn a_busy_move_polls_until_done() {
    let rig = boot(|hsc| hsc.set("H-1", |m| m.busy_polls = 3)).await;
    put_hsc(&rig, "l", "1.5").await;
    assert!(eventually(|| v(&rig, "hBusy") == 1.0).await);
    assert!(eventually(|| v(&rig, "hBusy") == 0.0).await);
    quiet().await;
    let mut want = vec!["!H-1 M 1400 1600"];
    for _ in 0..4 {
        want.extend(["!H-1 P", "!V-1 P"]);
    }
    assert_eq!(after_boot(&rig), want);
    h_axis(&rig, 1.5, 2.0);
}

/// A new target while the axis is busy kills that axis only, then moves it
/// again. The readbacks are not taken while it stays busy.
#[epics_base_rs::epics_test]
async fn a_new_target_interrupts_a_busy_move() {
    let rig = boot(|hsc| hsc.set("H-1", |m| m.busy_polls = 1000)).await;
    put_hsc(&rig, "l", "1.5").await;
    sent(&rig, "!H-1 M 1400 1600").await;
    assert_eq!(v(&rig, "hBusy"), 1.0);
    put_hsc(&rig, "l", "2").await;
    sent(&rig, "!H-1 M 1600 1600").await;
    let cmds = after_boot(&rig);
    assert_eq!(cmds[0], "!H-1 M 1400 1600");
    let k = cmds.iter().position(|c| c == "!H-1 K").expect("!H-1 K");
    assert_eq!(cmds[k + 1], "!H-1 M 1600 1600");
    assert!(
        cmds[1..k]
            .iter()
            .chain(&cmds[k + 2..])
            .all(|c| c == "!H-1 P" || c == "!V-1 P"),
        "{cmds:?}"
    );
    for (pv, c) in [
        ("l", 2.0),
        ("r", 2.0),
        ("lRB", 1.0),
        ("rRB", 2.0),
        ("width", 4.0),
        ("h0", 0.0),
        ("widthRB", 3.0),
        ("h0RB", 0.5),
        ("hBusy", 1.0),
        ("vBusy", 0.0),
        ("error", 0.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    assert_eq!(s(&rig, "errMsg"), "no error");
}

/// From init on, every `!H-1 P` is answered with `reply`, then a locate is
/// read. `validateResponse` compares the ID from the reply's second byte,
/// then `"%*s %*s %d %d"` must convert two numbers, else
/// `"%*s %*s %s"` reading `BUSY;` sets the axis busy. Anything else
/// changes nothing.
async fn locate_reply(reply: &str, l: f64, r: f64, h_busy: f64) {
    let rig = boot(|_| {}).await;
    rig.hsc.reply_to("!H-1 P", reply);
    put_hsc(&rig, "locate", "1").await;
    sent(&rig, "!H-1 P").await;
    assert_eq!(after_boot(&rig)[..2], ["!H-1 P", "!V-1 P"], "{reply:?}");
    assert!(
        after_boot(&rig).iter().all(|c| c.ends_with(" P")),
        "{reply:?} {:?}",
        after_boot(&rig)
    );
    for (pv, c) in [
        ("l", l),
        ("r", r),
        ("lRB", l),
        ("rRB", r),
        ("width", l + r),
        ("h0", (r - l) / 2.0),
        ("widthRB", l + r),
        ("h0RB", (r - l) / 2.0),
        ("hBusy", h_busy),
        ("vBusy", 0.0),
        ("error", 0.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{reply:?} {pv}");
    }
    assert_eq!(s(&rig, "errMsg"), "no error", "{reply:?}");
}

#[epics_base_rs::epics_test]
async fn a_reply_from_another_module_is_ignored() {
    locate_reply("%V-1 P 2000 2400 DONE;", 1.0, 2.0, 0.0).await;
}

/// The reply's first byte is never compared.
#[epics_base_rs::epics_test]
async fn a_reply_with_another_first_byte_is_taken() {
    locate_reply("XH-1 P 2000 2400 DONE;", 3.0, 4.0, 0.0).await;
}

/// A leading space shifts the ID by one byte, so it no longer matches.
#[epics_base_rs::epics_test]
async fn a_reply_with_a_leading_space_is_ignored() {
    locate_reply(" %H-1 P 2000 2400 DONE;", 1.0, 2.0, 0.0).await;
}

/// Without the echoed `P`, the scan skips `2000` and fails on `DONE;`.
#[epics_base_rs::epics_test]
async fn a_reply_without_the_echoed_command_is_ignored() {
    locate_reply("%H-1 2000 2400 DONE;", 1.0, 2.0, 0.0).await;
}

/// The second `%d` fails on `;2400`, and `2000;2400` is not `BUSY;`.
#[epics_base_rs::epics_test]
async fn a_semicolon_between_the_positions_is_ignored() {
    locate_reply("%H-1 P 2000;2400 DONE;", 1.0, 2.0, 0.0).await;
}

/// `DONE;` is never read.
#[epics_base_rs::epics_test]
async fn a_position_reply_without_done_is_taken() {
    locate_reply("%H-1 P 2000 2400", 3.0, 4.0, 0.0).await;
}

/// `%s` reads `BUSY;` and the trailing word is never read; the busy axis
/// is then polled every 0.25 s.
#[epics_base_rs::epics_test]
async fn a_busy_reply_with_a_trailing_word_sets_the_axis_busy() {
    locate_reply("%H-1 P BUSY; 7", 1.0, 2.0, 1.0).await;
}

/// An origin reply that does not scan counts as an error in init, so none
/// of the H limits are written and they keep the template's values. The
/// program keeps its default origin of 400, so the blades read
/// (1200 - 400) / 400 and (1600 - 400) / 400.
#[epics_base_rs::epics_test]
async fn an_origin_reply_that_does_not_scan_keeps_the_template_limits() {
    let rig = boot(|hsc| hsc.reply_to("!H-1 R 2", "%H-1 OK")).await;
    assert_eq!(rig.hsc.seen(), BOOT_SEEN);
    for (pv, c) in [
        ("l.LOPR", -1.0),
        ("l.HOPR", 10.0),
        ("width.HOPR", 20.0),
        ("t.HOPR", 10.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
    h_axis(&rig, 2.0, 3.0);
}

#[epics_base_rs::epics_test]
async fn stop_kills_every_axis_and_stays_set() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "stop", "1").await;
    sent(&rig, "!ALL K").await;
    assert_eq!(after_boot(&rig), ["!ALL K", "!H-1 P", "!V-1 P"]);
    assert_eq!(v(&rig, "stop"), 1.0);
}

/// `0 I` sets both motors of every module to their origin.
#[epics_base_rs::epics_test]
async fn calibrate_zeroes_every_axis() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "calib", "1").await;
    sent(&rig, "!ALL 0 I").await;
    assert_eq!(after_boot(&rig), ["!ALL 0 I", "!H-1 P", "!V-1 P"]);
    for (pv, c) in [
        ("calib", 0.0),
        ("lRB", 0.0),
        ("rRB", 0.0),
        ("l", 0.0),
        ("tRB", 0.0),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
}

/// Five seconds in idle without an event is a locate.
#[epics_base_rs::epics_test]
async fn an_idle_program_polls_every_five_seconds() {
    let rig = boot(|_| {}).await;
    assert!(eventually(|| after_boot(&rig).len() >= 2).await);
    quiet().await;
    assert_eq!(after_boot(&rig), ["!H-1 P", "!V-1 P"]);
}

/// A silent controller times the H read out. SEVR is then MAJOR, so the V
/// write is skipped, and idle reports error 18 and waits in comm_error.
/// Only `init` brings it back, through a full init.
#[epics_base_rs::epics_test]
async fn a_read_timeout_is_a_communications_error_until_init() {
    let rig = boot(|_| {}).await;
    rig.hsc.set_silent(true);
    put_hsc(&rig, "locate", "1").await;
    assert!(eventually(|| v(&rig, "error") == 18.0).await);
    quiet().await;
    assert_eq!(s(&rig, "errMsg"), "communications error: 2");
    assert_eq!(v(&rig, "init"), 0.0);
    assert_eq!(after_boot(&rig), ["!H-1 P"]);
    rig.hsc.set_silent(false);
    quiet().await;
    assert_eq!(v(&rig, "error"), 18.0);
    assert_eq!(after_boot(&rig), ["!H-1 P"]);
    put_hsc(&rig, "init", "1").await;
    seen_n(&rig, BOOT_SEEN.len() * 2 + 1).await;
    assert_eq!(after_boot(&rig)[1..], BOOT_SEEN);
    assert_eq!(v(&rig, "init"), 0.0);
    h_axis(&rig, 1.0, 2.0);
}

/// Disabled, the program sends nothing for a new target. Enabled again, it
/// runs init, and the readback then puts the target back to the blade.
#[epics_base_rs::epics_test]
async fn a_disabled_program_ignores_targets_and_reinits_on_enable() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "enable", "0").await;
    quiet().await;
    put_hsc(&rig, "l", "1.5").await;
    quiet().await;
    assert_eq!(rig.hsc.seen(), BOOT_SEEN);
    assert_eq!(v(&rig, "lRB"), 1.0);
    put_hsc(&rig, "enable", "1").await;
    seen_n(&rig, BOOT_SEEN.len() * 2).await;
    assert_eq!(after_boot(&rig), BOOT_SEEN);
    assert_eq!(v(&rig, "init"), 0.0);
    h_axis(&rig, 1.0, 2.0);
}

/// A locate after the H blades went to `l`, `r`.
async fn locate_at(rig: &Rig, l: f64, r: f64) {
    rig.hsc.set("H-1", |m| {
        m.a = (800.0 + 400.0 * l) as i32;
        m.b = (800.0 + 400.0 * r) as i32;
    });
    put_hsc(rig, "locate", "1").await;
}

/// A locate's readbacks and targets are its own reading, though the
/// previous locate's lRB echo lands inside it (C defect,
/// xiahsc.st:795-799).
#[epics_base_rs::epics_test]
async fn a_late_readback_echo_leaves_the_locate_at_its_reading() {
    let rig = boot(|_| {}).await;
    // The boot locate's lRB echo is in.
    task::sleep(Duration::from_millis(5000)).await;
    locate_at(&rig, 1.5, 2.0).await;
    assert!(eventually(|| v(&rig, "lRB") == 1.5).await);
    // Under a 3 s lRB echo delay and a 2 s rRB put delay, the next locate
    // puts lRB before the 1.5 echo lands and reads it after.
    task::sleep(Duration::from_millis(1500)).await;
    locate_at(&rig, 1.25, 2.5).await;
    assert!(eventually(|| v(&rig, "widthRB") != 3.5).await);
    quiet().await;
    h_axis(&rig, 1.25, 2.5);
}

/// A client's lRB put landing inside a locate is dropped: the targets and
/// the gap and center readbacks are the locate's reading, where C took the
/// put (C defect, xiahsc.st:795-799). lRB keeps the put until the next
/// locate, as in C.
#[epics_base_rs::epics_test]
async fn a_readback_put_inside_a_locate_is_dropped() {
    let rig = boot(|_| {}).await;
    locate_at(&rig, 1.25, 2.5).await;
    assert!(eventually(|| v(&rig, "lRB") == 1.25).await);
    // Under a 2 s rRB put delay, the put lands between the locate's lRB put
    // and its reads.
    task::sleep(Duration::from_millis(500)).await;
    put_hsc(&rig, "lRB", "3").await;
    assert!(eventually(|| v(&rig, "widthRB") != 3.0).await);
    quiet().await;
    for (pv, c) in [
        ("l", 1.25),
        ("r", 2.5),
        ("lRB", 3.0),
        ("rRB", 2.5),
        ("width", 3.75),
        ("h0", 0.625),
        ("widthRB", 3.75),
        ("h0RB", 0.625),
    ] {
        assert_eq!(v(&rig, pv), c, "{pv}");
    }
}

/// A client's lRB put between locates is put back by the next one: the
/// put guards compare against the monitored readback, as in C.
#[epics_base_rs::epics_test]
async fn a_readback_put_between_locates_is_put_back() {
    let rig = boot(|_| {}).await;
    put_hsc(&rig, "lRB", "3").await;
    quiet().await;
    put_hsc(&rig, "locate", "1").await;
    assert!(eventually(|| v(&rig, "lRB") == 1.0).await);
    quiet().await;
    h_axis(&rig, 1.0, 2.0);
}
