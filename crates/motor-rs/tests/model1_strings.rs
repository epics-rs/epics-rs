//! A model-1 record's command strings, which C reads from the record in
//! its device's `build_trans`: INIT at init (motordevCom.cc:309-327), PREM
//! and POST at each move (devIM483PL.cc:191-205), and the OUT link's parm
//! (devMVP2001.cc:262).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure, PostMessage};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::DevSup;
use epics_base_rs::types::EpicsValue;
use motor_rs::device_state::{AxisSubscribers, StampedStatus};
use motor_rs::device_support::MotorAxis;
use motor_rs::devices::{
    device_support_factory, devices, model1_device, register_axis, register_model1_device,
};
use motor_rs::flags::MotorCommand;
use motor_rs::poll_loop::{ToAxis, execute_commands};
use motor_rs::trace::{MotorTrace, TraceUser};
use tokio::sync::mpsc;

static MODEL1: DevSup = model1_device("devTestStrings", "TestStrings");

fn user(card: i32) -> TraceUser {
    TraceUser::Model1 {
        device: MODEL1.choice,
        card,
        signal: 0,
    }
}

/// A done model-1 axis at `card`, signal 0, whose commands land on the
/// returned channel; no poller runs.
fn axis(card: i32) -> mpsc::UnboundedReceiver<ToAxis> {
    axis_with(card).0
}

/// [`axis`], with the subscribers its status posts go through.
fn axis_with(card: i32) -> (mpsc::UnboundedReceiver<ToAxis>, AxisSubscribers) {
    let subscribers = AxisSubscribers::new();
    subscribers.store(StampedStatus {
        seq: 1,
        status: MotorStatus {
            done: true,
            ..MotorStatus::default()
        },
    });
    let (poll_tx, _) = mpsc::channel(16);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    register_axis(MotorAxis::new(
        user(card),
        std::sync::Arc::new(std::sync::Mutex::new(motor_rs::sim_motor::SimMotor::new())),
        Duration::from_secs(1),
        poll_tx,
        cmd_tx,
        subscribers.clone(),
    ))
    .unwrap();
    (cmd_rx, subscribers)
}

async fn boot(db: &str) -> Arc<PvDatabase> {
    register_model1_device(&MODEL1);
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// Every command queued so far, in order.
fn sent(rx: &mut mpsc::UnboundedReceiver<ToAxis>) -> Vec<MotorCommand> {
    let mut out = Vec::new();
    while let Ok(ToAxis::Batch(batch)) = rx.try_recv() {
        out.extend(batch.commands);
    }
    out
}

#[tokio::test]
async fn init_is_the_first_command_a_record_sends() {
    let mut rx = axis(40);
    let _db = boot(
        r##"
record(motor, "S:init") { field(DTYP, "TestStrings") field(OUT, "#C40 S0 @") field(INIT, "PM 3") }
"##,
    )
    .await;
    let commands = sent(&mut rx);
    assert_eq!(
        commands.first(),
        Some(&MotorCommand::Primitive {
            init: "PM 3".into()
        }),
        "{commands:?}"
    );
    assert_eq!(
        commands
            .iter()
            .filter(|c| matches!(c, MotorCommand::Primitive { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn an_empty_init_sends_no_primitive() {
    let mut rx = axis(41);
    let _db = boot(
        r##"
record(motor, "S:noinit") { field(DTYP, "TestStrings") field(OUT, "#C41 S0 @") }
"##,
    )
    .await;
    let commands = sent(&mut rx);
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, MotorCommand::Primitive { .. })),
        "{commands:?}"
    );
}

#[tokio::test]
async fn the_out_parm_reaches_the_axis_when_the_record_binds() {
    let mut rx = axis(45);
    let mut bare = axis(46);
    let _db = boot(
        r##"
record(motor, "S:parm") { field(DTYP, "TestStrings") field(OUT, "#C45 S0 @2000,500") }
record(motor, "S:bare") { field(DTYP, "TestStrings") field(OUT, "#C46 S0 @") }
"##,
    )
    .await;
    let commands = sent(&mut rx);
    assert_eq!(
        commands.first(),
        Some(&MotorCommand::OutParm {
            parm: "2000,500".into()
        }),
        "{commands:?}"
    );
    let commands = sent(&mut bare);
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, MotorCommand::OutParm { .. })),
        "{commands:?}"
    );
}

/// Wait for the commands a put queued; none arrive later than this.
async fn settle(rx: &mut mpsc::UnboundedReceiver<ToAxis>) -> Vec<MotorCommand> {
    tokio::time::sleep(Duration::from_millis(50)).await;
    sent(rx)
}

/// Each command `pick` names, with the one before it.
fn with_previous(
    commands: &[MotorCommand],
    pick: fn(&MotorCommand) -> bool,
) -> Vec<(Option<&MotorCommand>, &MotorCommand)> {
    commands
        .iter()
        .enumerate()
        .filter(|(_, c)| pick(c))
        .map(|(i, c)| (i.checked_sub(1).map(|p| &commands[p]), c))
        .collect()
}

/// A move's strings; an empty `post` is a move POST did not arm.
fn strings(prem: &str, post: &str) -> MotorCommand {
    MotorCommand::MoveStrings {
        prem: prem.into(),
        post: (!post.is_empty()).then(|| {
            let message = PostMessage::default();
            message.set(post.as_bytes());
            message
        }),
    }
}

/// The POST a move was armed with, as the device would read it now.
fn armed_post(commands: &[MotorCommand]) -> Option<PostMessage> {
    commands.iter().find_map(|c| match c {
        MotorCommand::MoveStrings { post, .. } => post.clone(),
        _ => None,
    })
}

/// C arms a move with `&mr->post` (devIM483PL.cc:203-204) and the driver
/// copies it out when the move is done (drvIM483PL.cc:361-366): a POST put
/// during the move is what the device sends, an emptied one sends nothing,
/// and a POST set after an unarmed move's build is not sent for it.
#[tokio::test]
async fn post_is_read_when_the_move_is_done() {
    let (mut rx, subscribers) = axis_with(47);
    let db = boot(
        r##"
record(motor, "S:post") { field(DTYP, "TestStrings") field(OUT, "#C47 S0 @") field(MRES, "1") field(VELO, "1") field(POST, "MF") }
"##,
    )
    .await;
    db.scan_control().run();
    settle(&mut rx).await;
    let put = |field: &'static str, value: EpicsValue| {
        let db = db.clone();
        async move {
            db.put_record_field_from_ca("S:post", field, value)
                .await
                .unwrap()
        }
    };

    put("VAL", EpicsValue::Double(5.0)).await;
    let post = armed_post(&settle(&mut rx).await).expect("POST arms the move");
    put("POST", EpicsValue::String(" XY".into())).await;
    assert_eq!(post.get(), b" XY", "the field, not the text at the build");
    put("POST", EpicsValue::String("".into())).await;
    assert!(post.is_empty(), "emptied during the move: nothing to send");

    // A VAL put during the move is parked (C do_work 2457), so the next
    // move is built once this one is done.
    for pulse in subscribers.post(StampedStatus {
        seq: 2,
        status: MotorStatus {
            position: 5.0,
            done: true,
            ..MotorStatus::default()
        },
    }) {
        pulse.try_send(()).unwrap();
    }
    settle(&mut rx).await;
    put("VAL", EpicsValue::Double(6.0)).await;
    let commands = settle(&mut rx).await;
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "{commands:?}"
    );
    assert_eq!(armed_post(&commands), None, "{commands:?}");
    put("POST", EpicsValue::String("MF".into())).await;
    assert_eq!(
        db.get_pv("S:post.POST").unwrap(),
        EpicsValue::String("MF".into())
    );
}

#[tokio::test]
async fn a_move_carries_prem_and_post_ahead_of_it() {
    let mut rx = axis(42);
    let db = boot(
        r##"
record(motor, "S:pp") { field(DTYP, "TestStrings") field(OUT, "#C42 S0 @") field(MRES, "1") field(VELO, "1") field(PREM, "MO") field(POST, "MF") }
"##,
    )
    .await;
    db.scan_control().run();
    let at_init = settle(&mut rx).await;
    assert!(
        !at_init
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveStrings { .. })),
        "{at_init:?}"
    );
    db.put_record_field_from_ca("S:pp", "VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    let commands = settle(&mut rx).await;
    let moves = with_previous(&commands, |c| {
        matches!(c, MotorCommand::MoveAbsolute { .. })
    });
    assert_eq!(moves.len(), 1, "{commands:?}");
    assert_eq!(moves[0].0, Some(&strings("MO", "MF")), "{commands:?}");
    assert_eq!(
        commands
            .iter()
            .filter(|c| matches!(c, MotorCommand::MoveStrings { .. }))
            .count(),
        1
    );
}

/// A jog is C's JOG, which the device builds with PREM (devIM483PL.cc:
/// 191-205); a JVEL put during it is special()'s SET_ACCEL and JOG_VELOCITY
/// (motorRecord.cc:3059-3072), which carry neither PREM nor POST.
#[tokio::test]
async fn a_jog_carries_prem_and_its_jvel_retune_does_not() {
    let mut rx = axis(43);
    let db = boot(
        r##"
record(motor, "S:jog") { field(DTYP, "TestStrings") field(OUT, "#C43 S0 @") field(MRES, "1") field(JVEL, "1") field(JAR, "4") field(PREM, "MO") field(POST, "MF") }
"##,
    )
    .await;
    db.scan_control().run();
    settle(&mut rx).await;
    db.put_record_field_from_ca("S:jog", "JOGF", EpicsValue::Short(1))
        .await
        .unwrap();
    let commands = settle(&mut rx).await;
    let jogs = with_previous(&commands, |c| {
        matches!(c, MotorCommand::MoveVelocity { .. })
    });
    assert_eq!(jogs.len(), 1, "{commands:?}");
    assert_eq!(jogs[0].0, Some(&strings("MO", "MF")), "{commands:?}");

    db.put_record_field_from_ca("S:jog", "JVEL", EpicsValue::Double(2.0))
        .await
        .unwrap();
    let commands = settle(&mut rx).await;
    let sent: Vec<_> = commands
        .iter()
        .filter(|c| {
            !matches!(
                c,
                MotorCommand::RecordFields(_) | MotorCommand::MoveDirection { .. }
            )
        })
        .collect();
    assert_eq!(
        sent,
        [
            &MotorCommand::SetAccel { acceleration: 4.0 },
            &MotorCommand::JogVelocity {
                min_velocity: 0.0,
                velocity: 2.0,
                acceleration: 4.0,
            },
        ],
        "{commands:?}"
    );
}

#[tokio::test]
async fn a_move_without_prem_or_post_carries_nothing() {
    let mut rx = axis(44);
    let db = boot(
        r##"
record(motor, "S:none") { field(DTYP, "TestStrings") field(OUT, "#C44 S0 @") field(MRES, "1") field(VELO, "1") }
"##,
    )
    .await;
    db.scan_control().run();
    settle(&mut rx).await;
    db.put_record_field_from_ca("S:none", "VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    let commands = settle(&mut rx).await;
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveAbsolute { .. })),
        "{commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, MotorCommand::MoveStrings { .. })),
        "{commands:?}"
    );
}

/// What a driver hook received.
#[derive(Default)]
struct Hooks {
    seen: Arc<Mutex<Vec<String>>>,
}

impl AsynMotor for Hooks {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        self.seen.lock().unwrap().push("move".into());
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_move_strings(
        &mut self,
        _: &AsynUser,
        prem: &[u8],
        post: Option<PostMessage>,
    ) -> AsynResult<()> {
        let post = post.map(|p| p.get()).unwrap_or_default();
        let (prem, post) = (
            String::from_utf8_lossy(prem),
            String::from_utf8_lossy(&post),
        );
        self.seen
            .lock()
            .unwrap()
            .push(format!("strings {prem}|{post}"));
        Ok(())
    }
    fn set_out_parm(&mut self, _: &AsynUser, parm: &[u8]) -> AsynResult<()> {
        let parm = String::from_utf8_lossy(parm);
        self.seen.lock().unwrap().push(format!("parm {parm}"));
        Ok(())
    }
    fn primitive(&mut self, _: &AsynUser, init: &[u8]) -> AsynResult<()> {
        let init = String::from_utf8_lossy(init);
        self.seen.lock().unwrap().push(format!("primitive {init}"));
        Ok(())
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        Ok(MotorStatus::default())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::NONE
    }
}

/// Run `commands` on a model-1 axis; what its hooks received.
fn run(commands: &[MotorCommand]) -> Vec<String> {
    let mut motor = Hooks::default();
    let seen = motor.seen.clone();
    let trace = MotorTrace::with_user(user(0));
    execute_commands(&mut motor, &trace, commands, &mut |_| {});
    seen.lock().unwrap().clone()
}

#[test]
fn primitive_reaches_the_driver() {
    assert_eq!(
        run(&[MotorCommand::Primitive {
            init: "PM 3".into()
        }]),
        ["primitive PM 3"]
    );
}

#[test]
fn move_strings_reach_the_driver_ahead_of_their_move() {
    assert_eq!(
        run(&[
            strings("MO", "MF"),
            MotorCommand::MoveAbsolute {
                position: 1.0,
                min_velocity: 0.0,
                velocity: 1.0,
                acceleration: 1.0,
            },
        ]),
        ["strings MO|MF", "move"]
    );
}

#[test]
fn the_out_parm_reaches_the_driver() {
    assert_eq!(
        run(&[MotorCommand::OutParm {
            parm: "2000,500".into()
        }]),
        ["parm 2000,500"]
    );
}
