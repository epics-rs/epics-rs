//! A target with no i64 step count (NaN, ±Inf, or |DVAL/MRES| of 2^63 steps
//! or more) is one C dispatches anyway: its NINT casts are unchecked, and the
//! oracle on motorSim reads DMOV 0, MOVN 1, MIP 0x20 after `dbpf N.VAL nan`,
//! `-inf`, `inf` or `-1e300`. The port refuses it before the move block, SET
//! mode included, like a refused move, so nothing is sent and the axis is not
//! polled again and again.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::poll_loop::PollCommand;
use motor_rs::trace::TraceUser;

/// What one axis was asked to do.
#[derive(Default)]
struct Calls {
    polls: AtomicU32,
    moves: AtomicU32,
    sets: AtomicU32,
}

/// A done axis at step 0 that counts its polls, moves and position loads.
struct Counting(Arc<Calls>);

impl AsynMotor for Counting {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        self.0.moves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        self.0.moves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        self.0.sets.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        self.0.polls.fetch_add(1, Ordering::SeqCst);
        *status = MotorStatus {
            done: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// One axis per record, polled only on demand. NT:dol reads a NaN through
/// its closed-loop DOL, NT:set takes a NaN DVAL in SET mode, the others a
/// VAL put. Each pass refuses the target, so the drive fields keep the last
/// one, DMOV ends 1 and no axis is re-polled.
#[tokio::test(flavor = "multi_thread")]
async fn a_target_without_a_step_count_is_refused_before_the_move_block() {
    let port = "NOSTEP";
    let vals = [
        ("NT:nan", f64::NAN),
        ("NT:pinf", f64::INFINITY),
        ("NT:ninf", f64::NEG_INFINITY),
        ("NT:huge", -1e300),
    ];
    let recs: Vec<&str> = vals
        .iter()
        .map(|(rec, _)| *rec)
        .chain(["NT:dol", "NT:set"])
        .collect();
    register_controller_port(&open_owner(), port, recs.len() as i32, ()).unwrap();
    let mut controller = ControllerBuilder::new();
    let calls: Vec<Arc<Calls>> = recs.iter().map(|_| Arc::default()).collect();
    let axes: Vec<ControllerAxis> = calls
        .iter()
        .enumerate()
        .map(|(addr, calls)| {
            controller.axis(
                Arc::new(Mutex::new(Counting(calls.clone()))),
                TraceUser::Asyn {
                    port: port.into(),
                    addr: addr as i32,
                },
            )
        })
        .collect();
    let hour = Duration::from_secs(3600);
    let first = controller.axes();
    let poller = controller.build(hour, hour, 0).start();
    let deadline = Instant::now() + Duration::from_secs(5);
    while first.first_polls_pending() > 0 {
        assert!(Instant::now() < deadline, "the startup pass never ended");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let polls: Vec<_> = axes.iter().map(|a| a.poll_cmd_tx.clone()).collect();
    for ControllerAxis { axis, .. } in axes {
        register_axis(axis).unwrap();
    }
    let mut db: String = recs
        .iter()
        .enumerate()
        .map(|(addr, rec)| {
            let extra = match *rec {
                "NT:dol" => r#"field(OMSL, "closed_loop") field(DOL, "NT:src")"#,
                _ => "",
            };
            format!(
                r#"record(motor, "{rec}") {{ field(DTYP, "asynMotor") field(OUT, "@asyn({port},{addr})") field(MRES, "1") {extra} }}
"#
            )
        })
        .collect();
    db.push_str(r#"record(ao, "NT:src") { field(VAL, "0") }"#);
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build_served()
        .await
        .unwrap();
    for (rec, val) in vals {
        db.put_record_field_from_ca(rec, "VAL", EpicsValue::Double(val))
            .await
            .unwrap();
    }
    db.put_record_field_from_ca("NT:src", "VAL", EpicsValue::Double(f64::NAN))
        .await
        .unwrap();
    db.put_record_field_from_ca("NT:dol", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("NT:set", "SET", EpicsValue::Short(1))
        .await
        .unwrap();
    db.put_record_field_from_ca("NT:set", "DVAL", EpicsValue::Double(f64::NAN))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let settled: Vec<u32> = calls
        .iter()
        .map(|c| c.polls.load(Ordering::SeqCst))
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let now: Vec<u32> = calls
        .iter()
        .map(|c| c.polls.load(Ordering::SeqCst))
        .collect();
    assert_eq!(now, settled, "the poller is idle");
    for (rec, calls) in recs.iter().zip(&calls) {
        assert_eq!(calls.moves.load(Ordering::SeqCst), 0, "{rec} sent no move");
        assert_eq!(
            calls.sets.load(Ordering::SeqCst),
            0,
            "{rec} loaded no position"
        );
        for field in ["VAL", "DVAL", "RVAL", "LDVL"] {
            assert_eq!(num(&db, &format!("{rec}.{field}")), 0.0, "{rec}.{field}");
        }
        assert_eq!(num(&db, &format!("{rec}.DMOV")), 1.0, "{rec}.DMOV");
        assert_eq!(num(&db, &format!("{rec}.MIP")), 0.0, "{rec}.MIP");
    }
    polls[0].send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}
