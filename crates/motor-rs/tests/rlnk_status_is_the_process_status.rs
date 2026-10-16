//! `motorRecord.cc:1495` returns RLNK's `dbPutLink` status from `process()`,
//! so a dead readback link fails the pass and an empty one does not.

use std::sync::Arc;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::{LinkPutOp, LinkSet, ProcStack, PvDatabase};
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use motor_rs::MotorRecord;
use motor_rs::flags::*;

/// `dbCa`'s -1 on a put to an unconnected link (`dbCa.c:558-560`).
struct Down;

#[epics_base_rs::async_trait]
impl LinkSet for Down {
    fn is_connected(&self, _: &[u8]) -> bool {
        false
    }
    fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, _: &[u8], _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        Err("channel disconnected".into())
    }
}

async fn readback_pass(rlnk: &str) -> Result<(), CaError> {
    use asyn_rs::interfaces::motor::MotorStatus;
    use motor_rs::device_state::StampedStatus;
    let state = motor_rs::device_state::new_shared_state();
    let mut rec = MotorRecord::new();
    rec.set_device_state(state.clone());
    rec.stat.msta = MstaFlags::DONE;
    rec.put_field("RLNK", EpicsValue::String(rlnk.into()))
        .unwrap();
    let db = PvDatabase::new();
    db.register_link_set("ca", Arc::new(Down)).await;
    db.add_record("M1", Box::new(rec)).await.unwrap();
    state.lock().unwrap().latest_status = Some(StampedStatus {
        seq: 1,
        status: MotorStatus {
            done: true,
            ..MotorStatus::default()
        },
    });
    db.process_record_readback("M1", &mut ProcStack::new())
        .await
}

#[tokio::test]
async fn a_dead_rlnk_is_the_process_status() {
    let r = readback_pass("NOPV:X CA").await;
    assert!(
        matches!(r, Err(CaError::ProcessStatus(s)) if s.get() == -1),
        "got {r:?}"
    );
}

#[tokio::test]
async fn an_empty_rlnk_returns_zero() {
    let r = readback_pass("").await;
    assert!(r.is_ok(), "got {r:?}");
}
