//! motor's `monitor()` MARK posts (`motorRecord.cc:3456-3646`) against
//! oracleMotorIoc (motorRecord + the asyn motorSim axis, MRES 0.001, VELO 1,
//! DHLM 100, DLLM -100), three `camonitor -m v|a|l` clients on `M1 M1.RBV
//! M1.OFF M1.HLM M1.LLM M1.DHLM M1.DLLM M1.DIR M1.DMOV M1.LVIO` started after
//! a `caput M1 1` move. The streams below are the oracle's with the timestamps
//! and the connect values cut, compared per PV; the move's cross-field order is
//! compared on one client's merged stream.

mod common;

use common::{MotorFixture, drain};
use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::event_queue::{CircuitEvent, EventReader, EventUser, no_context};
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const PVS: [(&str, &str, DbFieldType); 10] = [
    ("M1", "VAL", DbFieldType::Double),
    ("M1.RBV", "RBV", DbFieldType::Double),
    ("M1.OFF", "OFF", DbFieldType::Double),
    ("M1.HLM", "HLM", DbFieldType::Double),
    ("M1.LLM", "LLM", DbFieldType::Double),
    ("M1.DHLM", "DHLM", DbFieldType::Double),
    ("M1.DLLM", "DLLM", DbFieldType::Double),
    ("M1.DIR", "DIR", DbFieldType::Enum),
    ("M1.DMOV", "DMOV", DbFieldType::Short),
    ("M1.LVIO", "LVIO", DbFieldType::Short),
];

/// `caput M1.HLM 50; M1.LLM -20; M1.DIR 1; M1.HLM 60; M1.OFF 2`: a user-limit
/// put posts its field from `dbPut` and again from its MARK, then the dial
/// limit it moved; DIR and OFF post RBV, OFF or VAL and both user limits from
/// their marks whether or not the value moved.
const LIMITS: &str = "\
M1.HLM 50
M1.HLM 50
M1.DHLM 50
M1.LLM -20
M1.LLM -20
M1.DLLM -20
M1.DIR Neg
M1.RBV 1
M1.HLM 22
M1.LLM -48
M1.OFF 2
M1.HLM 60
M1.HLM 60
M1.DLLM -58
M1.OFF 2
M1.RBV 1
M1 1
M1.HLM 60
M1.LLM -48
";

/// `caput M1.SET 1; M1.VAL 5; M1.SET 0` on a fresh record: the drive put's
/// `special()` posts the DMOV blink, and the cycle's offset redefinition
/// (do_work 2206-2227) marks RBV, VAL, OFF, both user limits and DMOV back.
const SET_MODE: &str = "\
M1.DMOV 0
M1.RBV 5
M1 5
M1.HLM 105
M1.LLM -95
M1.OFF 5
M1.DMOV 1
";

/// Each class's reader per PV, in `PVS` order.
fn subscribe(fx: &MotorFixture) -> Vec<Vec<EventReader>> {
    let mut sid = 0;
    [EventMask::VALUE, EventMask::ALARM, EventMask::LOG]
        .into_iter()
        .map(|class| {
            PVS.iter()
                .map(|&(_, field, ty)| {
                    sid += 1;
                    fx.subscribe(field, sid, ty, class)
                })
                .collect()
        })
        .collect()
}

/// What one class's readers carried, as `camonitor` prints it per PV.
fn lines(readers: &mut [EventReader]) -> Vec<String> {
    PVS.iter()
        .zip(readers)
        .flat_map(|(&(pv, _, _), rx)| {
            drain(rx).into_iter().map(move |v| {
                let v = match v {
                    EpicsValue::Double(v) => format!("{v}"),
                    EpicsValue::Short(v) => format!("{v}"),
                    EpicsValue::Enum(v) => ["Pos", "Neg"][usize::from(v)].to_string(),
                    other => panic!("{pv}: {other:?}"),
                };
                format!("{pv} {v}")
            })
        })
        .collect()
}

/// The oracle's stream in the same per-PV order.
fn expected(stream: &str) -> Vec<String> {
    PVS.iter()
        .flat_map(|&(pv, _, _)| {
            stream
                .lines()
                .filter(move |l| l.rsplit_once(' ').unwrap().0 == pv)
                .map(str::to_string)
        })
        .collect()
}

fn assert_streams(readers: &mut [Vec<EventReader>], stream: &str) {
    assert_eq!(lines(&mut readers[0]), expected(stream), "DBE_VALUE");
    assert_eq!(lines(&mut readers[1]), Vec::<String>::new(), "DBE_ALARM");
    assert_eq!(lines(&mut readers[2]), expected(stream), "DBE_LOG");
}

async fn put(fx: &MotorFixture, field: &str, value: f64) {
    fx.db
        .put_record_field_from_ca("M1", field, EpicsValue::Double(value))
        .await
        .unwrap();
}

#[tokio::test]
async fn limit_dir_and_off_puts_post_their_marks_as_the_oracle_does() {
    let mut fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    fx.move_to(1.0).await;
    let mut readers = subscribe(&fx);
    for (field, value) in [
        ("HLM", 50.0),
        ("LLM", -20.0),
        ("DIR", 1.0),
        ("HLM", 60.0),
        ("OFF", 2.0),
    ] {
        put(&fx, field, value).await;
    }
    assert_streams(&mut readers, LIMITS);
    // The oracle's `caget` after the steps.
    for (field, v) in [
        ("VAL", 1.0),
        ("OFF", 2.0),
        ("HLM", 60.0),
        ("LLM", -48.0),
        ("DHLM", 50.0),
        ("DLLM", -58.0),
        ("RBV", 1.0),
    ] {
        let pv = format!("M1.{field}");
        assert_eq!(fx.db.get_pv(&pv).unwrap(), EpicsValue::Double(v), "{pv}");
    }
}

#[tokio::test]
async fn a_set_mode_val_put_posts_the_blink_and_the_redefinition_as_the_oracle_does() {
    let fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    let mut readers = subscribe(&fx);
    for (field, value) in [("SET", 1.0), ("VAL", 5.0), ("SET", 0.0)] {
        put(&fx, field, value).await;
    }
    assert_streams(&mut readers, SET_MODE);
    // The oracle's `caget` after the steps.
    for (field, v) in [
        ("VAL", 5.0),
        ("OFF", 5.0),
        ("HLM", 105.0),
        ("LLM", -95.0),
        ("RBV", 5.0),
    ] {
        let pv = format!("M1.{field}");
        assert_eq!(fx.db.get_pv(&pv).unwrap(), EpicsValue::Double(v), "{pv}");
    }
}

/// The same steps behind `caput M1.SPMG 0`: the oracle redefines under Stop
/// too, since C's VAL collection precedes the stop_or_pause return (2237).
#[tokio::test]
async fn a_set_mode_val_put_under_spmg_stop_redefines_as_the_oracle_does() {
    let fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    let mut readers = subscribe(&fx);
    for (field, value) in [("SPMG", 0.0), ("SET", 1.0), ("VAL", 5.0), ("SET", 0.0)] {
        put(&fx, field, value).await;
    }
    assert_streams(&mut readers, SET_MODE);
}

/// `caput M1.MRES -0.001; M1.MRES 0.002` on a fresh record: velcheckB
/// (2876-2909) posts both user limits once in its MRES<0 branch and again at
/// its end, then the cycle posts them from load_pos' set_userlimits MARK, and
/// the poll completing the LOAD_POS posts DMOV back.
const MRES_FLIP: &str = "\
M1.HLM 100
M1.LLM -100
M1.HLM 100
M1.LLM -100
M1.HLM 100
M1.LLM -100
M1.OFF 0
M1.DMOV 0
M1.DMOV 1
M1.DLLM -200
M1.DHLM 200
M1.HLM 200
M1.LLM -200
M1.HLM 200
M1.LLM -200
M1.OFF 0
M1.DMOV 0
M1.DMOV 1
";

#[tokio::test]
async fn a_negative_mres_put_posts_the_user_limits_as_the_oracle_does() {
    let mut fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    let mut readers = subscribe(&fx);
    for mres in [-0.001, 0.002] {
        put(&fx, "MRES", mres).await;
        fx.poll_pass(|_| {}).await;
    }
    assert_streams(&mut readers, MRES_FLIP);
    for (field, v) in [("HLM", 200.0), ("LLM", -200.0), ("OFF", 0.0)] {
        let pv = format!("M1.{field}");
        assert_eq!(fx.db.get_pv(&pv).unwrap(), EpicsValue::Double(v), "{pv}");
    }
}

/// oracleMotorIoc on motor-status.db, `camonitor -m v|a|l M1.CNEN M1.MSTA`,
/// `caput M1:GAIN 1; M1:POWERED 1; M1.CNEN 0; M1:POWERED 0; M1:POWERED 1;
/// M1.CNEN 0; M1:GAIN 0; M1:POWERED 0; M1.CNEN 1`, the longouts setting the
/// axis's GAIN_SUPPORT and EA_POSITION. CNEN posts from its own put and from
/// the MSTA block when MSTA is marked and EA_POSITION differs (3541-3548).
const CNEN: &str = "\
M1.MSTA 2050
M1.MSTA 2082
M1.CNEN Enable
M1.CNEN Disable
M1.MSTA 2050
M1.MSTA 2082
M1.CNEN Enable
M1.CNEN Disable
M1.MSTA 34
M1.MSTA 2
M1.CNEN Enable
";

#[tokio::test]
async fn cnen_posts_from_its_put_and_the_msta_block_as_the_oracle_does() {
    let got = cnen_streams(&[
        "GAIN 1",
        "POWERED 1",
        "CNEN 0",
        "POWERED 0",
        "POWERED 1",
        "CNEN 0",
        "GAIN 0",
        "POWERED 0",
        "CNEN 1",
    ])
    .await;
    assert_eq!(got[0], CNEN, "DBE_VALUE");
    assert_eq!(got[1], "", "DBE_ALARM");
    assert_eq!(got[2], CNEN, "DBE_LOG");
}

/// The same IOC, `caput M1:GAIN 1; M1:COMMERR 1; M1.PROC 1; M1:POWERED 1;
/// M1.PROC 1`: COMM_ERR raises COMM/INVALID once (alarm_sub 3392-3398), so the
/// EA_POSITION poll is an alarm move and CNEN goes out with the MSTA block's
/// mask, DBE_ALARM included.
const CNEN_ALARM_VALUE: &str = "\
M1.MSTA 2050
M1.MSTA 2050 COMM INVALID
M1.MSTA 2082 COMM INVALID
M1.CNEN Enable COMM INVALID
";
const CNEN_ALARM_ALARM: &str = "\
M1.MSTA 2050 COMM INVALID
M1.MSTA 2050
M1.MSTA 2082 COMM INVALID
M1.CNEN Enable COMM INVALID
M1.MSTA 2082
";

#[tokio::test]
async fn a_cnen_derived_on_an_alarm_move_carries_the_alarm_as_the_oracle_does() {
    let got = cnen_streams(&["GAIN 1", "COMMERR 1", "PROC 1", "POWERED 1", "PROC 1"]).await;
    assert_eq!(got[0], CNEN_ALARM_VALUE, "DBE_VALUE");
    assert_eq!(got[1], CNEN_ALARM_ALARM, "DBE_ALARM");
    assert_eq!(got[2], CNEN_ALARM_VALUE, "DBE_LOG");
}

/// Runs the steps (a driver status bit, or a put) on a fresh `M1` and returns
/// what `camonitor -m v|a|l M1.CNEN M1.MSTA` would print.
async fn cnen_streams(steps: &[&str]) -> Vec<String> {
    let mut fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let mut readers: Vec<[EventReader; 2]> = (0u32..)
        .step_by(2)
        .zip(classes)
        .map(|(sid, class)| {
            [
                fx.subscribe("MSTA", sid + 1, DbFieldType::ULong, class),
                fx.subscribe("CNEN", sid + 2, DbFieldType::Enum, class),
            ]
        })
        .collect();
    let mut got = vec![String::new(); 3];
    for step in steps {
        let (what, v) = step.split_once(' ').unwrap();
        let on = v == "1";
        match what {
            "GAIN" => fx.poll_pass(|m| m.gain_support = on).await,
            "POWERED" => fx.poll_pass(|m| m.powered = on).await,
            "COMMERR" => fx.poll_pass(|m| m.comms_error = on).await,
            field => put(&fx, field, if on { 1.0 } else { 0.0 }).await,
        }
        // Within a step MSTA posts before CNEN, as the MSTA block does.
        for (class, rx) in readers.iter_mut().enumerate() {
            for (pv, rx) in ["M1.MSTA", "M1.CNEN"].into_iter().zip(rx.iter_mut()) {
                while let Ok(e) = rx.try_recv() {
                    let v = match e.value().as_value() {
                        Some(EpicsValue::ULong(v)) => v.to_string(),
                        Some(EpicsValue::Enum(v)) => ["Disable", "Enable"][usize::from(*v)].into(),
                        other => panic!("{pv}: {other:?}"),
                    };
                    got[class] += &format!("{pv} {v}");
                    if e.sevr() != 0 {
                        got[class] += &format!(
                            " {} {}",
                            alarm_condition_string(e.stat()),
                            MENU_ALARM_SEVR[e.sevr() as usize]
                        );
                    }
                    got[class].push('\n');
                }
            }
        }
    }
    got
}

/// `caput M1.JVEL 1; M1.JOGF 1; M1.JOGF 0`, the jog's stop, then
/// `caput M1.MRES 0.002` and its LOAD_POS: special JOGF (3042-3047) and the
/// jog-stop request (2153) change MIP without a MARK, so only the dispatch,
/// postProcess and load_pos post it.
const MIP: &str = "\
M1.JOGF 1
M1.MIP 1
M1.DMOV 0
M1.JOGF 0
M1.MIP 0
M1.DMOV 1
M1.MIP 128
M1.DMOV 0
M1.MIP 0
M1.DMOV 1
";

#[tokio::test]
async fn mip_posts_from_c_s_marks_only_as_the_oracle_does() {
    let steps = [
        "JVEL 1",
        "JOGF 1",
        "poll",
        "JOGF 0",
        "stopped",
        "MRES 0.002",
        "poll",
    ];
    let got = mip_streams(&steps).await;
    assert_eq!(got[0], MIP, "DBE_VALUE");
    assert_eq!(got[1], "", "DBE_ALARM");
    assert_eq!(got[2], MIP, "DBE_LOG");
}

/// `caput M1.DLY 0.2; M1 0.1`: the done pass arms the delay (1445), the
/// watchdog's callbackFunc turns REQ into ACK (472-473) and its pass sets
/// MIP_DELAY (1418), neither marked, so MIP posts again only when the
/// GET_INFO pass ends the wait (1430) and maybeRetry finds it close enough.
const MIP_DLY: &str = "\
M1.MIP 32
M1.DMOV 0
M1.MIP 1056
M1.MIP 0
M1.DMOV 1
";

#[tokio::test]
async fn a_dly_wait_posts_mip_from_c_s_marks_only_as_the_oracle_does() {
    let got = mip_streams(&["DLY 0.2", "VAL 0.1", "poll", "delay", "poll"]).await;
    assert_eq!(got[0], MIP_DLY, "DBE_VALUE");
    assert_eq!(got[1], "", "DBE_ALARM");
    assert_eq!(got[2], MIP_DLY, "DBE_LOG");
}

impl MotorFixture {
    /// The poll loop firing the last armed DLY watchdog, and the pass it wakes.
    async fn expire_delay(&mut self) {
        use motor_rs::poll_loop::PollCommand;
        let mut id = None;
        while let Ok(cmd) = self._poll_cmd_rx.try_recv() {
            if let PollCommand::ScheduleDelay(d, _) = cmd {
                id = Some(d);
            }
        }
        self.state.lock().unwrap().expired_delay_id = Some(id.expect("no delay armed"));
        self.db
            .process_record_readback("M1", &mut ProcStack::new())
            .await
            .unwrap();
    }
}

/// Runs the steps (a put, a driver poll, the jog's stop, the DLY watchdog)
/// and returns each class's JOGF, MIP and DMOV posts.
async fn mip_streams(steps: &[&str]) -> Vec<String> {
    let mut fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    let pvs = [("M1.JOGF", "JOGF"), ("M1.MIP", "MIP"), ("M1.DMOV", "DMOV")];
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let mut sid = 0;
    let mut readers: Vec<Vec<EventReader>> = classes
        .into_iter()
        .map(|class| {
            pvs.iter()
                .map(|&(_, field)| {
                    sid += 1;
                    fx.subscribe(field, sid, DbFieldType::Short, class)
                })
                .collect()
        })
        .collect();
    let mut got = vec![String::new(); 3];
    for &step in steps {
        match step.split_once(' ') {
            Some((field, v)) => put(&fx, field, v.parse().unwrap()).await,
            None if step == "stopped" => fx.poll_pass(|m| m.moving = false).await,
            None if step == "delay" => fx.expire_delay().await,
            None => fx.poll_pass(|_| {}).await,
        }
        // Within a step the oracle carries JOGF, then MIP, then DMOV.
        for (class, rx) in readers.iter_mut().enumerate() {
            for (&(pv, _), rx) in pvs.iter().zip(rx.iter_mut()) {
                for v in drain(rx) {
                    let v = match v {
                        EpicsValue::Short(v) => i32::from(v),
                        EpicsValue::UShort(v) => i32::from(v),
                        other => panic!("{pv}: {other:?}"),
                    };
                    got[class] += &format!("{pv} {v}\n");
                }
            }
        }
    }
    got
}

/// One client's monitor on `field`, on that client's queue `user`, so the
/// queue's order across fields is the post order.
fn subscribe_on(
    fx: &MotorFixture,
    user: &EventUser,
    field: &str,
    sid: u32,
    field_type: DbFieldType,
    mask: EventMask,
) -> EventReader {
    fx.db
        .get_record("M1")
        .unwrap()
        .write()
        .add_subscriber_on(user, field, sid, field_type, mask.bits(), no_context())
        .unwrap()
}

/// Every event queued on `user`, oldest first, as `(sid, value)`.
fn drain_user(user: &EventUser) -> Vec<(u32, EpicsValue)> {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut out = Vec::new();
    while let std::task::Poll::Ready(event) = user.poll_next(&mut cx) {
        if let CircuitEvent::Event { sid, event, .. } = event {
            out.push((sid, event.value().as_value().cloned().unwrap()));
        }
    }
    out
}

// A move as one `camonitor` on all twelve fields the move posts sees it,
// either subscription order: the order across fields is C `monitor()`'s.

const MOVE_FIELDS: [(&str, DbFieldType); 12] = [
    ("VAL", DbFieldType::Double),
    ("DVAL", DbFieldType::Double),
    ("RBV", DbFieldType::Double),
    ("RRBV", DbFieldType::Long),
    ("DRBV", DbFieldType::Double),
    ("DIFF", DbFieldType::Double),
    ("RDIF", DbFieldType::Long),
    ("MSTA", DbFieldType::Long),
    ("TDIR", DbFieldType::Short),
    ("MIP", DbFieldType::Short),
    ("MOVN", DbFieldType::Short),
    ("DMOV", DbFieldType::Short),
];

/// The put's DMOV post, then the move-start pass.
const MOVE_START: [&str; 6] = ["DMOV 0", "DIFF 1", "RDIF 1000", "VAL 1", "DVAL 1", "MIP 32"];

const MOVE_DONE: [&str; 10] = [
    "RBV 1",
    "RRBV 1000",
    "DRBV 1",
    "DIFF 0",
    "RDIF 0",
    "MSTA 2",
    "TDIR 0",
    "MIP 0",
    "MOVN 0",
    "DMOV 1",
];

fn stream(user: &EventUser, order: &[usize]) -> Vec<String> {
    drain_user(user)
        .into_iter()
        .map(|(sid, value)| {
            let field = MOVE_FIELDS[order[sid as usize]].0;
            let value = match value {
                EpicsValue::Double(v) => v.to_string(),
                EpicsValue::Long(v) => v.to_string(),
                EpicsValue::ULong(v) => v.to_string(),
                EpicsValue::Short(v) => v.to_string(),
                EpicsValue::UShort(v) => v.to_string(),
                other => panic!("{field}: unexpected {other:?}"),
            };
            format!("{field} {value}")
        })
        .collect()
}

async fn move_streams(reversed: bool) -> (Vec<String>, Vec<String>) {
    let mut fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    let user = EventUser::new();
    let mut order: Vec<usize> = (0..MOVE_FIELDS.len()).collect();
    if reversed {
        order.reverse();
    }
    let _readers: Vec<_> = order
        .iter()
        .enumerate()
        .map(|(sid, &at)| {
            let (field, ty) = MOVE_FIELDS[at];
            subscribe_on(
                &fx,
                &user,
                field,
                sid as u32,
                ty,
                EventMask::VALUE | EventMask::LOG,
            )
        })
        .collect();
    let _ = drain_user(&user);

    fx.start_move(1.0).await;
    let start = stream(&user, &order);
    fx.poll_pass(|m| {
        m.moving = true;
        m.direction = true;
        m.position = 0.5;
    })
    .await;
    let _ = drain_user(&user);
    fx.poll_pass(|m| {
        m.moving = false;
        m.direction = false;
        m.position = 1.0;
    })
    .await;
    (start, stream(&user, &order))
}

#[tokio::test]
async fn monitors_attached_in_declared_order_see_c_post_order() {
    let (start, done) = move_streams(false).await;
    assert_eq!(start, MOVE_START);
    assert_eq!(done, MOVE_DONE);
}

#[tokio::test]
async fn monitors_attached_in_reverse_order_see_c_post_order() {
    let (start, done) = move_streams(true).await;
    assert_eq!(start, MOVE_START);
    assert_eq!(done, MOVE_DONE);
}

/// `caput M1.DHLM -1; M1.HLSV MAJOR; M1.PROC 1; M1.HLSV NO_ALARM; M1.VELO 2;
/// M1.HLSV MAJOR; M1.VELO 3` on oracleMotorIoc, `camonitor -m a M1.MSTA M1
/// M1.VELO M1.RHLM M1.RLLM`: a DVAL over DHLM raises HIGH at HLSV. Each alarm
/// move posts the five fields above C's short circuit and, the put's implicit
/// GET_INFO having marked STUP below it (motorRecord.cc:2546-2551), the whole
/// lower group to its last entry RLLM, none of them moved (:3510-3646).
const SOFT_LIMIT_ALARM: &str = "\
M1.MSTA 2 HIGH MAJOR
M1 0 HIGH MAJOR
M1.VELO 1 HIGH MAJOR
M1.RHLM -1000 HIGH MAJOR
M1.RLLM -100000 HIGH MAJOR
M1.MSTA 2
M1 0
M1.VELO 1
M1.RHLM -1000
M1.RLLM -100000
M1.MSTA 2 HIGH MAJOR
M1 0 HIGH MAJOR
M1.VELO 2 HIGH MAJOR
M1.RHLM -1000 HIGH MAJOR
M1.RLLM -100000 HIGH MAJOR
";

#[tokio::test]
async fn a_soft_limit_alarm_posts_the_monitor_list_to_its_tail_as_the_oracle_does() {
    const ALARM_PVS: [(&str, &str, DbFieldType); 5] = [
        ("M1.MSTA", "MSTA", DbFieldType::ULong),
        ("M1", "VAL", DbFieldType::Double),
        ("M1.VELO", "VELO", DbFieldType::Double),
        ("M1.RHLM", "RHLM", DbFieldType::Double),
        ("M1.RLLM", "RLLM", DbFieldType::Double),
    ];
    let mut fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    let mut readers: Vec<EventReader> = (1u32..)
        .zip(ALARM_PVS)
        .map(|(sid, (_, field, ty))| fx.subscribe(field, sid, ty, EventMask::ALARM))
        .collect();
    for (field, v) in [
        ("DHLM", -1.0),
        ("HLSV", 2.0),
        ("PROC", 1.0),
        ("HLSV", 0.0),
        ("VELO", 2.0),
        ("HLSV", 2.0),
        ("VELO", 3.0),
    ] {
        put(&fx, field, v).await;
        // The status the put's implicit GET_INFO asked for (STUP BUSY to
        // OFF), as motorSim answers it before the next put.
        fx.poll_pass(|_| {}).await;
    }
    let got: Vec<String> = ALARM_PVS
        .iter()
        .zip(&mut readers)
        .flat_map(|(&(pv, _, _), rx)| {
            let mut lines = Vec::new();
            while let Ok(e) = rx.try_recv() {
                let v = match e.value().as_value() {
                    Some(EpicsValue::ULong(v)) => v.to_string(),
                    Some(EpicsValue::Double(v)) => format!("{v}"),
                    other => panic!("{pv}: {other:?}"),
                };
                let mut line = format!("{pv} {v}");
                if e.sevr() != 0 {
                    line += &format!(
                        " {} {}",
                        alarm_condition_string(e.stat()),
                        MENU_ALARM_SEVR[e.sevr() as usize]
                    );
                }
                lines.push(line);
            }
            lines
        })
        .collect();
    let want: Vec<String> = ALARM_PVS
        .iter()
        .flat_map(|&(pv, _, _)| {
            SOFT_LIMIT_ALARM
                .lines()
                .filter(move |l| l.split(' ').next() == Some(pv))
                .map(str::to_string)
        })
        .collect();
    assert_eq!(got, want);
}

/// The same alarm on a put that finds STUP still BUSY: C fires no implicit
/// GET_INFO (motorRecord.cc:2546), nothing below the short circuit is marked,
/// and the alarm move posts the five fields above it, to their last entry
/// MSTA, and stops (:3550-3551). motorSim answers GET_INFO before the next put
/// reaches the IOC, so the fixture's unanswered status stands in for the
/// window the oracle cannot hold open.
#[tokio::test]
async fn an_alarm_move_with_nothing_marked_posts_to_msta_and_stops() {
    let fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    let mut msta = fx.subscribe("MSTA", 1, DbFieldType::ULong, EventMask::ALARM);
    let mut velo = fx.subscribe("VELO", 2, DbFieldType::Double, EventMask::ALARM);
    let mut rllm = fx.subscribe("RLLM", 3, DbFieldType::Double, EventMask::ALARM);
    put(&fx, "DHLM", -1.0).await;
    put(&fx, "HLSV", 2.0).await;
    let sevr = |rx: &mut EventReader| {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| e.sevr())
            .collect::<Vec<_>>()
    };
    assert_eq!(sevr(&mut msta), [2], "MSTA");
    assert_eq!(sevr(&mut velo), Vec::<u16>::new(), "VELO");
    assert_eq!(sevr(&mut rllm), Vec::<u16>::new(), "RLLM");
}
