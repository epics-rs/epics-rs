use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;
use scaler_rs::ScalerRecord;
use std::collections::HashMap;

// ============================================================
// Scaler: CNT start/stop via framework
// ============================================================

#[tokio::test]
async fn test_scaler_count_start_stop() {
    let db_str = r#"
record(scaler, "TEST:SC") {
    field(FREQ, "1000000")
    field(TP, "1.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Initial state
    let ss = server.get("TEST:SC.SS").await.unwrap();
    assert_eq!(ss, EpicsValue::Short(0), "SS should be IDLE initially");

    // Start counting: put CNT=1 then process
    server
        .put("TEST:SC.CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:SC", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let ss = server.get("TEST:SC.SS").await.unwrap();
    assert_eq!(
        ss,
        EpicsValue::Short(2),
        "SS should be COUNTING after CNT=1 + process"
    );

    let us = server.get("TEST:SC.US").await.unwrap();
    assert_eq!(us, EpicsValue::Short(3), "US should be USER_COUNTING");

    // Stop: put CNT=0 then process
    server
        .put("TEST:SC.CNT", EpicsValue::Short(0))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:SC", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let ss = server.get("TEST:SC.SS").await.unwrap();
    assert_eq!(ss, EpicsValue::Short(0), "SS should be IDLE after stop");
}

// ============================================================
// Scaler: TP <-> PR1 conversion
// ============================================================

#[tokio::test]
async fn test_scaler_tp_pr1_conversion() {
    let db_str = r#"
record(scaler, "TEST:SC2") {
    field(FREQ, "1000000")
    field(TP, "2.0")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    let pr1 = server.get("TEST:SC2.PR1").await.unwrap();
    // PR1 is DBF_ULONG (scalerRecord.dbd:945) -> native EpicsValue::ULong.
    assert_eq!(pr1, EpicsValue::ULong(2_000_000), "PR1 = TP * FREQ");
}

// ============================================================
// Scaler: DLY delayed start via AsyncPendingReprocess
// ============================================================

#[tokio::test]
async fn test_scaler_dly_delayed_start() {
    let db_str = r#"
record(scaler, "TEST:SC3") {
    field(FREQ, "1000000")
    field(TP, "1.0")
    field(DLY, "0.2")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Start counting with DLY=0.2s
    // special("CNT") sets US=WAITING, then process returns AsyncPendingReprocess
    server
        .put("TEST:SC3.CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:SC3", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // During DLY wait
    let us = server.get("TEST:SC3.US").await.unwrap();
    assert_eq!(us, EpicsValue::Short(1), "US should be WAITING during DLY");

    // Wait for DLY to expire + framework re-process
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let ss = server.get("TEST:SC3.SS").await.unwrap();
    assert_eq!(
        ss,
        EpicsValue::Short(2),
        "SS should be COUNTING after DLY expires"
    );

    let us = server.get("TEST:SC3.US").await.unwrap();
    assert_eq!(
        us,
        EpicsValue::Short(3),
        "US should be USER_COUNTING after DLY"
    );
}

// ============================================================
// Scaler: preset auto-enables gate
// ============================================================

#[tokio::test]
async fn test_scaler_preset_auto_gate() {
    let db_str = r#"
record(scaler, "TEST:SC4") {
    field(FREQ, "1000000")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    // Set PR5 via framework — triggers special
    server
        .put("TEST:SC4.PR5", EpicsValue::Long(5000))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let g5 = server.get("TEST:SC4.G5").await.unwrap();
    assert_eq!(g5, EpicsValue::Short(1), "G5 should auto-enable");

    let d5 = server.get("TEST:SC4.D5").await.unwrap();
    assert_eq!(d5, EpicsValue::Short(1), "D5 should auto-enable");
}

// ============================================================
// Scaler: indexed field access via framework
// ============================================================

#[tokio::test]
async fn test_scaler_indexed_fields() {
    let db_str = r#"
record(scaler, "TEST:SC5") {
    field(FREQ, "1000000")
    field(NM1, "clock")
    field(NM2, "detector")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    let nm1 = server.get("TEST:SC5.NM1").await.unwrap();
    assert_eq!(nm1, EpicsValue::String("clock".into()));

    let nm2 = server.get("TEST:SC5.NM2").await.unwrap();
    assert_eq!(nm2, EpicsValue::String("detector".into()));

    let s1 = server.get("TEST:SC5.S1").await.unwrap();
    // S1 is DBF_ULONG (scalerRecord.dbd:1334) -> native EpicsValue::ULong.
    assert_eq!(s1, EpicsValue::ULong(0));
}

// ============================================================
// Scaler: PR1/TP/FREQ monitor events after a count-start driver
// write-back — C scalerRecord.c:424-430 (db_post_events).
// ============================================================

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::DbFieldType;
use scaler_rs::MAX_SCALER_CHANNELS;
use scaler_rs::device_support::scaler_asyn::{ScalerAsynDeviceSupport, ScalerDriver};
use scaler_rs::device_support::scaler_soft::SoftScalerDriver;

/// A driver that quantizes channel-0 presets up to a multiple of
/// `QUANTUM` and runs its own clock at `DRIVER_FREQ` — models the
/// Joerger VS64 the C comment at `scalerRecord.c:397-403` describes.
struct QuantizingDriver;

impl QuantizingDriver {
    const QUANTUM: u32 = 4096;
    const DRIVER_FREQ: f64 = 1.25e7;

    fn quantize(preset: u32) -> u32 {
        preset.div_ceil(Self::QUANTUM) * Self::QUANTUM
    }
}

impl ScalerDriver for QuantizingDriver {
    fn reset(&mut self) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn read(
        &mut self,
        _counts: &mut [u32; MAX_SCALER_CHANNELS],
    ) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn write_preset(&mut self, channel: usize, preset: u32) -> epics_base_rs::error::CaResult<u32> {
        if channel == 0 {
            Ok(Self::quantize(preset))
        } else {
            Ok(preset)
        }
    }
    fn arm(&mut self, _start: bool) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn actual_frequency(&self) -> Option<f64> {
        Some(Self::DRIVER_FREQ)
    }
    fn done(&mut self) -> bool {
        false
    }
    fn num_channels(&self) -> usize {
        8
    }
}

/// Bug 1 regression — C `scalerRecord.c:424-430`.
///
/// A count-start whose driver adjusts the channel-0 preset (and adopts
/// its own clock) must produce `DBE_VALUE` monitor events on `.PR1`,
/// `.TP`, and `.FREQ`. `run_start_count` runs inside
/// `DeviceSupport::handle_command`, AFTER the process snapshot was
/// built and notified; pre-fix the framework never posted those fields
/// so CA/PVA clients subscribed to PR1/TP/FREQ stayed stale.
#[tokio::test]
async fn test_count_start_posts_pr1_tp_freq_monitor_events() {
    let db = PvDatabase::new();
    db.add_record("TEST:SCMON", Box::new(ScalerRecord::default()))
        .await
        .unwrap();

    // Attach asyn device support backed by the quantizing driver.
    {
        let rec = db.get_record("TEST:SCMON").unwrap();
        let mut inst = rec.write();
        let mut support = ScalerAsynDeviceSupport::new(Box::new(QuantizingDriver));
        support.init(&mut *inst.record).unwrap(); // nch <- 8
        let scaler = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .unwrap();
        scaler.freq = 1e7;
        scaler.tp = 1.0; // requested 1 s count
        scaler.init_record(1).unwrap();
        inst.device = Some(Box::new(support));
    }

    // Subscribe to PR1/TP/FREQ with DBE_VALUE — the C `db_post_events`
    // mask at scalerRecord.c:425-430.
    let (mut pr1_rx, mut tp_rx, mut freq_rx) = {
        let rec = db.get_record("TEST:SCMON").unwrap();
        let mut inst = rec.write();
        let pr1 = inst
            // PR1 is DBF_ULONG (scalerRecord.dbd:945); subscribe with the native type.
            .add_subscriber("PR1", 1, DbFieldType::ULong, EventMask::VALUE.bits())
            .expect("PR1 subscription accepted");
        let tp = inst
            .add_subscriber("TP", 2, DbFieldType::Double, EventMask::VALUE.bits())
            .expect("TP subscription accepted");
        let freq = inst
            .add_subscriber("FREQ", 3, DbFieldType::Double, EventMask::VALUE.bits())
            .expect("FREQ subscription accepted");
        (pr1, tp, freq)
    };

    // Drive CNT=1 -> REQSTART, then process: the CNT block emits
    // CMD_RESET + CMD_START_COUNT, which the framework dispatches.
    {
        let rec = db.get_record("TEST:SCMON").unwrap();
        let mut inst = rec.write();
        let scaler = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .unwrap();
        scaler.cnt = 1;
        scaler.special("CNT", true).unwrap();
        assert_eq!(scaler.us, 2, "CNT=1 -> REQSTART");
    }

    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links("TEST:SCMON", &mut visited, 0)
        .await
        .unwrap();

    // The driver adjusted PR1 and adopted its own clock, so all three
    // fields must have produced a monitor event.
    let pr1_evt = pr1_rx
        .try_recv()
        .expect("PR1 monitor event must be posted after a count-start driver write-back (C:425)");
    let tp_evt = tp_rx
        .try_recv()
        .expect("TP monitor event must be posted after the count-start TP recompute (C:427)");
    let freq_evt = freq_rx
        .try_recv()
        .expect("FREQ monitor event must be posted after the driver adopted its clock (C:430)");

    // The posted values must be the reconciled ones, not stale.
    let expected_pr1 = QuantizingDriver::quantize(12_500_000); // NINT(1.0*1.25e7)
    assert_eq!(
        pr1_evt.snapshot.value.clone(),
        // PR1 is DBF_ULONG (scalerRecord.dbd:945) -> native EpicsValue::ULong.
        EpicsValue::ULong(expected_pr1),
        "posted PR1 must be the driver-programmed (quantized) preset"
    );
    assert_eq!(
        freq_evt.snapshot.value.clone(),
        EpicsValue::Double(QuantizingDriver::DRIVER_FREQ),
        "posted FREQ must be the driver's actual clock"
    );
    let expected_tp = expected_pr1 as f64 / QuantizingDriver::DRIVER_FREQ;
    match tp_evt.snapshot.value.clone() {
        EpicsValue::Double(v) => assert!(
            (v - expected_tp).abs() < 1e-12,
            "posted TP must be recomputed from effective PR1/FREQ: got {v}, want {expected_tp}"
        ),
        other => panic!("TP event value must be Double, got {other:?}"),
    }
}

/// Value-change monitor-mask regression — C `scalerRecord.c` posts
/// CNT/T/VAL/PR1/TP/FREQ and each active channel with a literal
/// `DBE_VALUE` on a value change
/// (scalerRecord.c:430 for FREQ); `DBE_LOG` appears ONLY in the idle
/// `monitor()` sweep (line 771). A value-change FREQ post must therefore
/// carry `DBE_VALUE` with the LOG bit stripped — so a `DBE_LOG`-only
/// subscriber gets no event on a counting change, while a `DBE_VALUE`
/// subscriber does. Pre-fix the framework posted every changed field
/// `DBE_VALUE | DBE_LOG`, so the LOG-only subscriber wrongly fired.
#[tokio::test]
async fn test_value_change_post_is_value_only_no_log_bit() {
    let db = PvDatabase::new();
    db.add_record("TEST:SCVO", Box::new(ScalerRecord::default()))
        .await
        .unwrap();

    {
        let rec = db.get_record("TEST:SCVO").unwrap();
        let mut inst = rec.write();
        let mut support = ScalerAsynDeviceSupport::new(Box::new(QuantizingDriver));
        support.init(&mut *inst.record).unwrap(); // nch <- 8
        let scaler = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .unwrap();
        scaler.freq = 1e7;
        scaler.tp = 1.0;
        scaler.init_record(1).unwrap();
        inst.device = Some(Box::new(support));
    }

    // Two subscribers on the SAME value-only field (FREQ): one that
    // accepts DBE_VALUE|DBE_LOG (so it always receives, and reports the
    // posted mask), and one that accepts DBE_LOG ONLY (the boundary —
    // it must NOT fire on a value change).
    let (mut freq_vallog_rx, mut freq_logonly_rx) = {
        let rec = db.get_record("TEST:SCVO").unwrap();
        let mut inst = rec.write();
        let vallog = inst
            .add_subscriber(
                "FREQ",
                1,
                DbFieldType::Double,
                (EventMask::VALUE | EventMask::LOG).bits(),
            )
            .expect("FREQ VALUE|LOG subscription accepted");
        let logonly = inst
            .add_subscriber("FREQ", 2, DbFieldType::Double, EventMask::LOG.bits())
            .expect("FREQ LOG-only subscription accepted");
        (vallog, logonly)
    };

    {
        let rec = db.get_record("TEST:SCVO").unwrap();
        let mut inst = rec.write();
        let scaler = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .unwrap();
        scaler.cnt = 1;
        scaler.special("CNT", true).unwrap();
    }

    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links("TEST:SCVO", &mut visited, 0)
        .await
        .unwrap();

    // The VALUE|LOG subscriber receives the FREQ change; its posted mask
    // carries DBE_VALUE but NOT DBE_LOG (the LOG bit is stripped).
    let evt = freq_vallog_rx.try_recv().expect(
        "FREQ change must reach a DBE_VALUE subscriber after the driver adopted its clock (C:430)",
    );
    assert!(
        evt.mask.contains(EventMask::VALUE),
        "FREQ post must carry DBE_VALUE, got {:?}",
        evt.mask
    );
    assert!(
        !evt.mask.contains(EventMask::LOG),
        "FREQ value-change post must NOT carry DBE_LOG (C posts a literal DBE_VALUE, \
         scalerRecord.c:430); got {:?}",
        evt.mask
    );

    // The DBE_LOG-only subscriber must see nothing — the value-change post
    // no longer intersects its mask. (Pre-fix it fired on every change.)
    assert!(
        freq_logonly_rx.try_recv().is_err(),
        "a DBE_LOG-only subscriber must NOT receive a scaler value-change event \
         (DBE_LOG is reserved for the idle monitor() sweep, scalerRecord.c:771)"
    );
}

/// Bug 2 regression — C `scalerRecord.c:405-428`.
///
/// `old_pr1` is captured at C `:406` BEFORE the `:409-410`
/// `pr1 = NINT(tp*freq)` self-consistency guard. When the user wrote a
/// `TP` whose `frac(tp*freq) >= 0.5`, the guard alone changes PR1
/// (truncating `tp_to_pr1` vs rounding NINT), so C's `:424`
/// `old_pr1 != pr1` test fires and TP is recomputed — even with a
/// driver that does not adjust anything. Pre-fix the port captured
/// `old_pr1` AFTER the guard, so `old_pr1 == pr1` always and the TP
/// recompute / monitor post never happened.
#[tokio::test]
async fn test_count_start_guard_triggers_tp_recompute_and_post() {
    let db = PvDatabase::new();
    db.add_record("TEST:SCGUARD", Box::new(ScalerRecord::default()))
        .await
        .unwrap();

    // freq * tp chosen so frac(tp*freq) >= 0.5: 1.00000006 * 1e7 =
    // 10_000_000.6 -> truncating tp_to_pr1 = 10_000_000, but the
    // count-start guard's NINT = 10_000_001.
    let user_tp = 1.000_000_06_f64;
    let freq = 1e7_f64;

    // Attach soft device support — the SoftScalerDriver returns every
    // preset unchanged and owns no clock, so ONLY the :409-410 guard
    // can change PR1.
    {
        let rec = db.get_record("TEST:SCGUARD").unwrap();
        let mut inst = rec.write();
        let mut support = ScalerAsynDeviceSupport::new(Box::new(SoftScalerDriver::new(8)));
        support.init(&mut *inst.record).unwrap();
        let scaler = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .unwrap();
        scaler.freq = freq;
        // Simulate the user writing TP: special() runs the truncating
        // tp_to_pr1, so pr[0] = trunc(tp*freq) = 10_000_000.
        scaler.tp = user_tp;
        scaler.special("TP", true).unwrap();
        assert_eq!(
            scaler.pr[0], 10_000_000,
            "user TP write -> truncating tp_to_pr1"
        );
        inst.device = Some(Box::new(support));
    }

    let mut tp_rx = {
        let rec = db.get_record("TEST:SCGUARD").unwrap();
        let mut inst = rec.write();
        inst.add_subscriber("TP", 4, DbFieldType::Double, EventMask::VALUE.bits())
            .expect("TP subscription accepted")
    };

    {
        let rec = db.get_record("TEST:SCGUARD").unwrap();
        let mut inst = rec.write();
        let scaler = inst
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .unwrap();
        scaler.cnt = 1;
        scaler.special("CNT", true).unwrap();
        assert_eq!(scaler.us, 2, "CNT=1 -> REQSTART");
    }

    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links("TEST:SCGUARD", &mut visited, 0)
        .await
        .unwrap();

    // The guard changed PR1 from 10_000_000 (pre-guard, == old_pr1) to
    // 10_000_001 (NINT). The non-adjusting driver left it there, so
    // C:424 `old_pr1 != pr1` fires: TP is recomputed and posted.
    let tp_evt = tp_rx.try_recv().expect(
        "guard-only PR1 change must trigger the C:424-427 TP recompute + monitor post; \
         old_pr1 must be captured BEFORE the :409-410 guard",
    );
    let expected_tp = 10_000_001_f64 / freq; // pr1 / freq, C:426
    match tp_evt.snapshot.value.clone() {
        EpicsValue::Double(v) => assert!(
            (v - expected_tp).abs() < 1e-12,
            "TP must be recomputed from the guard-adjusted PR1/FREQ: got {v}, want {expected_tp}"
        ),
        other => panic!("TP event value must be Double, got {other:?}"),
    }

    // Confirm the final record state matches C.
    let rec = db.get_record("TEST:SCGUARD").unwrap();
    let mut inst = rec.write();
    let scaler = inst
        .record
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<ScalerRecord>())
        .unwrap();
    assert_eq!(scaler.pr[0], 10_000_001, "PR1 = NINT(tp*freq) after guard");
    assert!(
        (scaler.tp - expected_tp).abs() < 1e-12,
        "TP recomputed from effective PR1/FREQ"
    );
}

// ============================================================
// Scaler: a put to a non-pp field (a preset) must NOT process.
//
// C scalerRecord.dbd marks only CNT and CONT pp(TRUE); every preset
// (PR1..PR64) is special(SPC_MOD), fully applied by special() without
// processing. Before the `"scaler" => &["CNT", "CONT"]` pp_fields_for
// entry the record had no entry and ran process() on every put, so a
// preset put spuriously entered the AutoCount block and armed the
// counter. Decisive signal: with CONT=1 and DLY1=0 a process arms
// immediately (SS -> COUNTING); a non-processing preset put leaves
// SS IDLE.
// ============================================================

#[tokio::test]
async fn test_scaler_preset_put_does_not_process() {
    let db_str = r#"
record(scaler, "TEST:SCNP") {
    field(FREQ, "1000000")
    field(CONT, "1")
    field(DLY1, "0")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // No PINI / no process yet: idle despite CONT=1.
    assert_eq!(
        server.get("TEST:SCNP.SS").await.unwrap(),
        EpicsValue::Short(0),
        "SS must be IDLE before any process"
    );

    // Put a preset (PR1) — a special(SPC_MOD), non-pp field. Must apply
    // via special() but must NOT process (no AutoCount arming).
    db.put_record_field_from_ca("TEST:SCNP", "PR1", EpicsValue::ULong(5000))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        server.get("TEST:SCNP.SS").await.unwrap(),
        EpicsValue::Short(0),
        "a preset put must NOT process — SS must stay IDLE (AutoCount not armed)"
    );
    // special() did apply the preset.
    assert_eq!(
        server.get("TEST:SCNP.PR1").await.unwrap(),
        EpicsValue::ULong(5000),
        "special() must apply the preset value"
    );

    // Sanity: a real process (PROC) DOES arm AutoCount with CONT=1/DLY1=0.
    db.put_record_field_from_ca("TEST:SCNP", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        server.get("TEST:SCNP.SS").await.unwrap(),
        EpicsValue::Short(2),
        "PROC must process and arm AutoCount (SS -> COUNTING)"
    );
}

// ============================================================
// W10-E2 — the DLY watchdog is what starts the count
// ============================================================
//
// C `special(CNT)` with DLY > 0 (scalerRecord.c:657-661) only arms a timer:
//
//     pscal->us = USER_STATE_WAITING;
//     callbackRequestDelayed(pdelayCallback, pscal->dly);
//
// and `delayCallbackFunc` (:216-231) is what starts the count when it expires:
//
//     if (pscal->us == USER_STATE_WAITING && pscal->cnt) {
//         pscal->us = USER_STATE_REQSTART;
//         (void)scanOnce((void *)pscal);
//     }
//
// That `scanOnce` has NO `if (pscal->scan)` guard — unlike the DLY == 0 arm at
// :655 — so the count starts DLY seconds after the CNT write no matter how the
// record is scanned. The port armed no timer: it back-filled the start from
// whatever process cycle happened to arrive next — for a periodically scanned
// scaler that is up to one scan period late, and for one with no scan source
// at all (SCAN = "I/O Intr", "Event") it never comes.

/// A periodically-scanned scaler whose next scan tick is 10 seconds out: the
/// CNT put cannot process it (it is not Passive), so within the window of this
/// test ONLY the DLY watchdog can start the count.
#[tokio::test]
async fn w10_e2_dly_start_does_not_depend_on_a_scan_source() {
    let db_str = r#"
record(scaler, "TEST:SC_E2") {
    field(SCAN, "10 second")
    field(FREQ, "1000000")
    field(TP, "1.0")
    field(DLY, "0.1")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    server
        .put("TEST:SC_E2.CNT", EpicsValue::Short(1))
        .await
        .unwrap();

    // Mid-wait: WAITING, not counting.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        server.get("TEST:SC_E2.US").await.unwrap(),
        EpicsValue::Short(1),
        "US == USER_STATE_WAITING while the watchdog runs"
    );
    assert_eq!(
        server.get("TEST:SC_E2.SS").await.unwrap(),
        EpicsValue::Short(0),
        "SS == SCALER_STATE_IDLE — the count has not started yet"
    );

    // Past expiry: delayCallbackFunc's scanOnce has started the count.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(
        server.get("TEST:SC_E2.SS").await.unwrap(),
        EpicsValue::Short(2),
        "SS == SCALER_STATE_COUNTING: the watchdog started the count with no scan source"
    );
    assert_eq!(
        server.get("TEST:SC_E2.US").await.unwrap(),
        EpicsValue::Short(3),
        "US == USER_STATE_COUNTING"
    );
}

/// The abort path: `caput CNT 0` during the wait cancels the watchdog
/// (`epicsTimerCancel`, scalerRecord.c:645), so the count must never start —
/// the armed re-entry finds `us != WAITING` and does nothing, C's own
/// `us == WAITING && cnt` guard on a raced callback.
#[tokio::test]
async fn w10_e2_aborting_during_the_wait_never_starts_the_count() {
    let db_str = r#"
record(scaler, "TEST:SC_E2B") {
    field(SCAN, "10 second")
    field(FREQ, "1000000")
    field(TP, "1.0")
    field(DLY, "0.1")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();

    server
        .put("TEST:SC_E2B.CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    server
        .put("TEST:SC_E2B.CNT", EpicsValue::Short(0))
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(
        server.get("TEST:SC_E2B.SS").await.unwrap(),
        EpicsValue::Short(0),
        "the cancelled watchdog must not arm the scaler"
    );
    assert_eq!(
        server.get("TEST:SC_E2B.US").await.unwrap(),
        EpicsValue::Short(0),
        "US == USER_STATE_IDLE after the abort"
    );
}
