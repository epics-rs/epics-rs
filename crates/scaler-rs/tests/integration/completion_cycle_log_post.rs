//! R12-62 — the count-completion cycle posts each changed `Sn` TWICE.
//!
//! C `scalerRecord.c::process()`:
//!
//!   * the done-interrupt arm sets `ss = SCALER_STATE_IDLE` (`:367-368`);
//!   * `updateCounts()` (`:453` → `:578-584`) posts every `Sn` whose count
//!     moved with `DBE_VALUE`;
//!   * `ss == IDLE` then admits the `monitor()` call at `:471-474`, and
//!     `monitor()` (`:757-773`) posts EVERY active `Sn` with a literal
//!     `DBE_LOG` — unconditionally, change or no change.
//!
//! So on the ONE cycle that carries the final counts, a changed `Sn` produces
//! two `db_post_events`: `DBE_VALUE`, then `DBE_LOG`. The port modelled the
//! `DBE_LOG` sweep as the `else` arm of the change check, so a changed `Sn`
//! could only ever emit the `DBE_VALUE` half — a `DBE_LOG`-only archiver never
//! received the final counts, which is the only `Sn` sample it wants.

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — all 2 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p scaler-rs
// --all-features`, 112/112). scaler-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use scaler_rs::device_support::scaler_asyn::{
    DoneInterrupt, ScalerAsynDeviceSupport, ScalerDriver,
};
use scaler_rs::records::scaler::{MAX_SCALER_CHANNELS, ScalerRecord};

use std::sync::{Arc, Mutex};

/// What the hardware holds: the counts `read()` returns and the record's
/// done interrupt user.
struct Hardware {
    counts: [u32; MAX_SCALER_CHANNELS],
    done_user: Option<DoneInterrupt>,
}

struct HardwareDriver(Arc<Mutex<Hardware>>);

impl ScalerDriver for HardwareDriver {
    fn reset(&mut self) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn read(
        &mut self,
        counts: &mut [u32; MAX_SCALER_CHANNELS],
    ) -> epics_base_rs::error::CaResult<()> {
        *counts = self.0.lock().unwrap().counts;
        Ok(())
    }
    fn write_preset(
        &mut self,
        _channel: usize,
        preset: u32,
    ) -> epics_base_rs::error::CaResult<u32> {
        Ok(preset)
    }
    fn arm(&mut self, _start: bool) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn register_done_interrupt(&mut self, user: DoneInterrupt) {
        self.0.lock().unwrap().done_user = Some(user);
    }
    fn num_channels(&self) -> usize {
        4
    }
}

async fn scaler_db() -> (PvDatabase, Arc<Mutex<Hardware>>) {
    let db = PvDatabase::new();
    let hardware = Arc::new(Mutex::new(Hardware {
        counts: [0; MAX_SCALER_CHANNELS],
        done_user: None,
    }));
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.nch = 4;
    ScalerAsynDeviceSupport::new(Box::new(HardwareDriver(hardware.clone())))
        .init(&mut rec)
        .unwrap();
    rec.init_record(1).unwrap();
    db.add_record("SCAL", Box::new(rec)).await.unwrap();
    (db, hardware)
}

async fn watch(db: &PvDatabase, field: &str, sid: u32, mask: EventMask) -> EventReader {
    let rec = db.get_record("SCAL").unwrap();
    let mut inst = rec.write();
    inst.add_subscriber(field, sid, DbFieldType::Long, mask.bits())
        .expect("subscription must be accepted")
}

async fn process(db: &PvDatabase) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("SCAL", &mut visited)
        .await
        .unwrap();
}

fn drain(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

/// Hardware holds new counts and its done interrupt has fired: the next
/// cycle's dset `done()` (`scalerRecord.c:366`) and `read()` (`:572`) see
/// them.
fn hardware_finishes_with(hardware: &Mutex<Hardware>, counts: [u32; 4]) {
    let mut hw = hardware.lock().unwrap();
    hw.counts[..4].copy_from_slice(&counts);
    hw.done_user.as_ref().unwrap().callback(1);
}

#[tokio::test]
async fn r12_62_completion_cycle_posts_sn_with_both_value_and_log() {
    let (db, hardware) = scaler_db().await;
    // Two separate subscribers on S1: one archiver (DBE_LOG only), one
    // value client (DBE_VALUE only). C's completion cycle serves both.
    let mut s1_log = watch(&db, "S1", 1, EventMask::LOG).await;
    let mut s1_value = watch(&db, "S1", 2, EventMask::VALUE).await;

    // CNT is pp(TRUE): the put starts the count (ss = COUNTING). No
    // monitor() runs on a counting cycle, so no idle sweep here.
    db.put_record_field_from_ca("SCAL", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    drain(&mut s1_log);
    drain(&mut s1_value);

    // The completion cycle: counts moved AND ss lands on IDLE.
    hardware_finishes_with(&hardware, [4242, 0, 0, 0]);
    process(&db).await;

    assert_eq!(
        drain(&mut s1_value),
        1,
        "updateCounts() posts the changed S1 with DBE_VALUE (scalerRecord.c:582)"
    );
    assert_eq!(
        drain(&mut s1_log),
        1,
        "monitor()'s sweep posts the SAME S1 with DBE_LOG on that same cycle \
         (scalerRecord.c:771) — the archiver must receive the final counts"
    );
}

/// The sweep half alone, unchanged: an idle cycle with no count movement still
/// posts every `Sn` with `DBE_LOG` and nothing with `DBE_VALUE`.
#[tokio::test]
async fn r12_62_idle_cycle_still_posts_log_only() {
    let (db, _hardware) = scaler_db().await;
    let mut s1_log = watch(&db, "S1", 1, EventMask::LOG).await;
    let mut s1_value = watch(&db, "S1", 2, EventMask::VALUE).await;

    process(&db).await;

    assert_eq!(
        drain(&mut s1_log),
        1,
        "an idle process runs monitor() → one DBE_LOG per active channel"
    );
    assert_eq!(
        drain(&mut s1_value),
        0,
        "S1 did not move — updateCounts() posts no DBE_VALUE"
    );
}
