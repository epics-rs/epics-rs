//! The compiled `pf4.st` on `pf4common.db`/`pf4bank.db` and four `bo`
//! hardware bits: the `{BP}{Bn}` monitors, `displayBit` as a NoMon output,
//! and the `bankctl` 3→2→1 cascade through its own posts
//! (`pf4.st:268-332`). The hand port's cases, on C's states.

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_seq_rs::{SeqThreadId, SeqVar, seq, seq_stop};
use optics_rs::snl::pf4::{RecalcFilters, numInArray, pf4};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/");
const HW: &str = r#"
record(bo, "PF:A1") {}
record(bo, "PF:A2") {}
record(bo, "PF:A3") {}
record(bo, "PF:A4") {}
record(ao, "PF:mono") { field(VAL, "10") }
"#;

fn macros(kv: &[(&str, &str)]) -> HashMap<String, String> {
    kv.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The program on the IOC; the scan owner keeps the scan threads up.
struct Pf4 {
    ioc: Arc<PvDatabase>,
    _scan: ScanOwner,
    tid: SeqThreadId,
}

/// Bank A with Al 0.2 mm and Ti 0.02 mm, the other two empty, switched on.
async fn boot() -> Pf4 {
    boot_with("").await
}

/// [`boot`], with `extra` loaded after the bank.
async fn boot_with(extra: &str) -> Pf4 {
    boot_on(HW, extra).await
}

/// [`boot_with`] on the hardware records `hw`.
async fn boot_on(hw: &str, extra: &str) -> Pf4 {
    let p = macros(&[("P", "PF:"), ("H", "pf:"), ("A", "A"), ("B", "B")]);
    let ioc = IocBuilder::new()
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(hw, &macros(&[]))
        .unwrap()
        .db_file(&format!("{DB}pf4common.db"), &p)
        .unwrap()
        .db_file(
            &format!("{DB}pf4bank.db"),
            &macros(&[("P", "PF:"), ("H", "pf:"), ("B", "A")]),
        )
        .unwrap()
        .db_string(extra, &macros(&[]))
        .unwrap()
        .build_served()
        .await
        .unwrap()
        .0;
    let scan = ScanOwner::start(ioc.clone());
    let tid = seq(
        &pf4,
        "P=PF:,H=pf:,B=A,BP=PF:,B1=A1,B2=A2,B3=A3,B4=A4,M=PF:mono",
        0,
        &ioc,
    )
    .expect("pf4 starts");
    settle().await;
    put(&ioc, "PF:pf:f1A", EpicsValue::Double(0.2)).await;
    put(&ioc, "PF:pf:f2A", EpicsValue::Double(0.02)).await;
    put(&ioc, "PF:pf:f3A", EpicsValue::Double(0.0)).await;
    put(&ioc, "PF:pf:f4A", EpicsValue::Double(0.0)).await;
    put(&ioc, "PF:pf:Z2A", EpicsValue::Enum(1)).await;
    put(&ioc, "PF:pf:bankA", EpicsValue::Enum(1)).await;
    settle().await;
    Pf4 {
        ioc,
        _scan: scan,
        tid,
    }
}

async fn settle() {
    task::sleep(Duration::from_millis(1000)).await;
}

async fn put(ioc: &PvDatabase, pv: &str, v: EpicsValue) {
    let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
    ioc.put_record_field_from_ca(rec, field, v).await.unwrap();
}

async fn f64_of(ioc: &PvDatabase, pv: &str) -> f64 {
    ioc.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn string_of(ioc: &PvDatabase, pv: &str) -> String {
    match ioc.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

/// `RecalcFilters` as `bankControl` calls it for the booted bank: the
/// transmissions, and the bit patterns for `numInArray`.
fn bank() -> ([f64; 16], SeqVar<[i16; 16]>) {
    let (bits, xmit) = (SeqVar::new([0i16; 16]), SeqVar::new([0.0; 16]));
    RecalcFilters(
        10.0, &bits, &xmit, 0, 3, 0, 1, 0, 0, 0.2, 0.02, 0.0, 0.0, "", "", "", "",
    );
    (xmit.get(), bits)
}

/// Switching the bank on goes `bankOff` → `recalcBank` → 3 (labels) → 2
/// (`trans`) → 1, each step taken from the `bankctl` post of the one before.
#[epics_base_rs::epics_test]
async fn switching_the_bank_on_runs_3_2_1() {
    let f = boot().await;
    assert_eq!(f64_of(&f.ioc, "PF:pf:bankA").await, 1.0);
    assert_eq!(string_of(&f.ioc, "PF:pf:fPosA.ZRST").await, "1.000e+00");
    // Al 0.2 mm and Ti 0.02 mm both in, C's `%.3e`.
    assert_eq!(string_of(&f.ioc, "PF:pf:fPosA.FFST").await, "9.263e-02");
    assert_eq!(f64_of(&f.ioc, "PF:pf:transA").await, 1.0);
    seq_stop(f.tid);
}

/// `{BP}{Bn}` is monitored: a hardware bit set from outside shows on
/// `displayBit` and moves `fPos` to the position holding its pattern.
///
/// With the bank on, C runs `filterPos` without end (every label write posts
/// `fPos`), and a bit put landing in its 0.2 s sleep is written back and its
/// flag cleared. `recalcBank`'s `bankctl` 3 runs past that clear, so the bit
/// is set from `bankA`'s forward link there.
#[epics_base_rs::epics_test]
async fn a_hardware_bit_sets_display_bit_and_fpos() {
    let f = boot_with(KICK_A2).await;
    let (xmit, bits) = bank();
    let pos = numInArray(16, &bits, 2) as usize;
    // `trans` is put by bankControl 2, after filterBits puts `fPos`.
    let want = (pos as f64, 1.0, 1.0, xmit[pos]);
    let mut got = (0.0, 0.0, 0.0, 0.0);
    for _ in 0..100 {
        got = (
            f64_of(&f.ioc, "PF:pf:fPosA").await,
            f64_of(&f.ioc, "PF:pf:displayBit2A").await,
            f64_of(&f.ioc, "PF:A2").await,
            f64_of(&f.ioc, "PF:pf:transA").await,
        );
        if got == want {
            break;
        }
        task::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(got, want, "fPosA, displayBit2A, A2, transA");
    seq_stop(f.tid);
}

/// `PF:A2` 1 whenever `bankA` is written 3.
const KICK_A2: &str = r#"
record(mbbo, "PF:pf:bankA") { field(FLNK, "PF:kick") }
record(calcout, "PF:kick") {
    field(INPA, "PF:pf:bankA NPP")
    field(CALC, "A=3")
    field(OOPT, "When Non-zero")
    field(OUT, "PF:A2 PP")
}
"#;

/// An `fPos` past 15, which the mbbo takes with an INVALID alarm, selects
/// nothing in `filterPos` or `bankControl` 2, and a later position still
/// moves the blades.
///
/// C defect fixed: C read `bits[16]`/`xmit[16]` and put `1 << 16`.
#[epics_base_rs::epics_test]
async fn an_fpos_past_15_selects_nothing() {
    let f = boot().await;
    put(&f.ioc, "PF:pf:fPosA", EpicsValue::Enum(16)).await;
    settle().await;
    for b in ["PF:A1", "PF:A2", "PF:A3", "PF:A4"] {
        assert_eq!(f64_of(&f.ioc, b).await, 0.0, "{b}");
    }
    put(&f.ioc, "PF:pf:bankA", EpicsValue::Enum(2)).await;
    settle().await;
    assert_eq!(f64_of(&f.ioc, "PF:pf:bankA").await, 1.0);
    assert_eq!(f64_of(&f.ioc, "PF:pf:transA").await, 1.0);
    assert_eq!(f64_of(&f.ioc, "PF:pf:bitFlagA").await, 1.0);
    let (xmit, bits) = bank();
    let pos = numInArray(16, &bits, 2);
    put(&f.ioc, "PF:pf:fPosA", EpicsValue::Enum(pos as u16)).await;
    settle().await;
    assert_eq!(f64_of(&f.ioc, "PF:A2").await, 1.0);
    assert_eq!(f64_of(&f.ioc, "PF:pf:transA").await, xmit[pos as usize]);
    seq_stop(f.tid);
}

/// The hardware bits as a pattern, `PF:A1` the low bit.
async fn blades(ioc: &PvDatabase) -> i16 {
    let mut b = 0;
    for i in 0..4 {
        b |= (f64_of(ioc, &format!("PF:A{}", i + 1)).await as i16) << i;
    }
    b
}

/// An `fPos` past 15 landing in `filterPos`'s `FILTER_WAIT` sleep: the pass
/// in progress finishes on the position it started with, and a later
/// position still moves the blades.
#[epics_base_rs::epics_test]
async fn an_fpos_past_15_during_the_sleep_leaves_the_pass_whole() {
    let f = boot().await;
    let (_, bits) = bank();
    let p1 = numInArray(16, &bits, 3);
    put(&f.ioc, "PF:pf:fPosA", EpicsValue::Enum(p1 as u16)).await;
    task::sleep(Duration::from_millis(80)).await;
    put(&f.ioc, "PF:pf:fPosA", EpicsValue::Enum(16)).await;
    task::sleep(Duration::from_millis(1500)).await;
    let after_p1 = blades(&f.ioc).await;
    let p2 = numInArray(16, &bits, 2);
    put(&f.ioc, "PF:pf:fPosA", EpicsValue::Enum(p2 as u16)).await;
    task::sleep(Duration::from_millis(1500)).await;
    let after_p2 = blades(&f.ioc).await;
    seq_stop(f.tid);
    assert_eq!(after_p1, 3, "the pass for p1 {p1} finished whole");
    assert_eq!(after_p2, 2, "p2 {p2} still moves the blades");
}

/// `OtherShow` fills `E`/`T` with the species' table padded by its last
/// entry; an `E` of NELM 400 takes the 300 the arrays hold.
///
/// C defects fixed: C passed NELM as the count, past `showEnergy[300]`; and
/// with the bank on C never reached the clause (see
/// [`a_repost_of_the_current_position_no_longer_reputs_the_blades`]).
#[epics_base_rs::epics_test]
async fn other_show_fills_at_most_the_300_entry_arrays() {
    let f = boot_with(r#"record(waveform, "PF:pf:E") { field(NELM, "400") }"#).await;
    put(&f.ioc, "PF:pf:OtherShow", EpicsValue::String("Cu".into())).await;
    settle().await;
    let cu = optics_rs::data::chantler::find_material("Cu").unwrap();
    let EpicsValue::FloatArray(e) = f.ioc.get_pv("PF:pf:E").unwrap() else {
        panic!("E is FTVL FLOAT");
    };
    assert_eq!(e.len(), 300);
    assert_eq!(&e[..cu.kev.len()], cu.kev);
    assert!(
        e[cu.kev.len()..]
            .iter()
            .all(|&v| v == cu.kev[cu.kev.len() - 1])
    );
    seq_stop(f.tid);
}

/// Each hardware bit counts its processings, so a put of the value it
/// already holds still shows. `useMono` 0, so the boot's first `monoenergy`
/// event starts no `ENERGY_WAIT`, whose `recalcBank` would rewrite the
/// labels during the case.
const COUNT_BITS: &str = r#"
record(bo, "PF:pf:useMono") { field(VAL, "0") }
record(bo, "PF:A1") { field(FLNK, "PF:n1") }
record(bo, "PF:A2") { field(FLNK, "PF:n2") }
record(bo, "PF:A3") { field(FLNK, "PF:n3") }
record(bo, "PF:A4") { field(FLNK, "PF:n4") }
record(calc, "PF:n1") { field(INPA, "PF:n1 NPP") field(CALC, "A+1") }
record(calc, "PF:n2") { field(INPA, "PF:n2 NPP") field(CALC, "A+1") }
record(calc, "PF:n3") { field(INPA, "PF:n3 NPP") field(CALC, "A+1") }
record(calc, "PF:n4") { field(INPA, "PF:n4 NPP") field(CALC, "A+1") }
"#;

async fn bit_puts(ioc: &PvDatabase) -> [f64; 4] {
    let mut n = [0.0; 4];
    for (i, c) in n.iter_mut().enumerate() {
        *c = f64_of(ioc, &format!("PF:n{}", i + 1)).await;
    }
    n
}

/// A position whose bits differ from the blades moves them as C does,
/// each bit put once (`filterPos`), then `recalcBank` takes the bank
/// through 3 again, whose label writes restore `FFST`.
#[epics_base_rs::epics_test]
async fn a_new_position_puts_every_bit_and_recalculates() {
    let f = boot_with(COUNT_BITS).await;
    let (xmit, bits) = bank();
    let pos = numInArray(16, &bits, 1);
    let before = bit_puts(&f.ioc).await;
    // Not the current position's label, so it posts no fPos.
    put(&f.ioc, "PF:pf:fPosA.FFST", EpicsValue::String("x".into())).await;
    put(&f.ioc, "PF:pf:fPosA", EpicsValue::Enum(pos as u16)).await;
    settle().await;
    let after = bit_puts(&f.ioc).await;
    for i in 0..4 {
        assert_eq!(after[i] - before[i], 1.0, "PF:A{}", i + 1);
    }
    assert_eq!(f64_of(&f.ioc, "PF:A1").await, 1.0);
    assert_eq!(f64_of(&f.ioc, "PF:pf:transA").await, xmit[pos as usize]);
    assert_eq!(string_of(&f.ioc, "PF:pf:fPosA.FFST").await, "9.263e-02");
    assert_eq!(f64_of(&f.ioc, "PF:pf:bankA").await, 1.0);
    seq_stop(f.tid);
}

/// Deviation from C: a write of the current position's label posts `fPos`
/// (`mbboRecord.c:288-290`), and with the blades already there `filterPos`
/// puts no bit and runs no `recalcBank`. C re-put all four and took the
/// bank through 3, whose label writes posted `fPos` again, without end.
#[epics_base_rs::epics_test]
async fn a_repost_of_the_current_position_no_longer_reputs_the_blades() {
    let f = boot_with(COUNT_BITS).await;
    let before = bit_puts(&f.ioc).await;
    put(&f.ioc, "PF:pf:fPosA.ZRST", EpicsValue::String("x".into())).await;
    settle().await;
    assert_eq!(bit_puts(&f.ioc).await, before);
    // No bankControl 3 rewrote the label.
    assert_eq!(string_of(&f.ioc, "PF:pf:fPosA.ZRST").await, "x");
    seq_stop(f.tid);
}

/// `displayBit` is an unmonitored output (`pf4.st:46`): a put to it is not
/// read back, and the hardware bit stays where it was.
#[epics_base_rs::epics_test]
async fn a_display_bit_put_leaves_the_hardware_bit() {
    let f = boot().await;
    put(&f.ioc, "PF:pf:displayBit1A", EpicsValue::Enum(1)).await;
    settle().await;
    assert_eq!(f64_of(&f.ioc, "PF:A1").await, 0.0);
    assert_eq!(f64_of(&f.ioc, "PF:pf:fPosA").await, 0.0);
    seq_stop(f.tid);
}

/// `PVPUT` is `pvPut(var, SYNC)` (`seqPVmacros.h:167-171`): `filterPos` puts
/// `b1` and waits for it before `displayBit1` (`pf4.st:231`). With `b1` a
/// `busy` left set, `displayBit1` stays 0 while the put is pending, and
/// the busy reset completes it. `A2`-`A4` are processed at init, so taking
/// them out posts nothing: a first process posts its alarm change, which
/// can land after `efClear(b2_mon)`..`efClear(b4_mon)`, in C as here, and
/// send `idle` on to `filterBits`.
#[epics_base_rs::epics_test]
async fn filter_pos_waits_for_a_bit_put_before_its_display_bit() {
    let hw = r#"
record(busy, "PF:A1") {}
record(bo, "PF:A2") { field(VAL, "0") field(PINI, "YES") }
record(bo, "PF:A3") { field(VAL, "0") field(PINI, "YES") }
record(bo, "PF:A4") { field(VAL, "0") field(PINI, "YES") }
record(ao, "PF:mono") { field(VAL, "10") }
"#;
    let f = boot_on(hw, "").await;
    let (_, bits) = bank();
    let pos = numInArray(16, &bits, 1);
    put(&f.ioc, "PF:pf:fPosA", EpicsValue::Enum(pos as u16)).await;
    settle().await;
    assert_eq!(f64_of(&f.ioc, "PF:A1").await, 1.0);
    assert_eq!(f64_of(&f.ioc, "PF:pf:displayBit1A").await, 0.0);
    // A notify put here would join PF:A1's restart list behind the pending
    // SYNC put and never land (dbNotify.c:213-220); an operator put does.
    f.ioc
        .put_record_field_from_ca_no_notify("PF:A1", "VAL", EpicsValue::Enum(0))
        .await
        .unwrap();
    settle().await;
    assert_eq!(f64_of(&f.ioc, "PF:pf:displayBit1A").await, 1.0);
    seq_stop(f.tid);
}
