//! C parity tests for retry/RMOD/FRAC/SPDB semantics.

use motor_rs::flags::*;
use motor_rs::record::MotorRecord;

use asyn_rs::interfaces::motor::MotorStatus;

fn make_record() -> MotorRecord {
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.limits.dhlm = 100.0;
    rec.limits.dllm = -100.0;
    rec.limits.hlm = 100.0;
    rec.limits.llm = -100.0;
    rec.limits.lvio = false;
    rec.vel.velo = 10.0;
    rec.vel.accl = 0.5;
    rec.vel.bvel = 5.0;
    rec.vel.bacc = 0.5;
    rec.stat.msta = MstaFlags::DONE;
    rec
}

fn complete_move(rec: &mut MotorRecord, target_pos: f64) {
    let status = MotorStatus {
        position: target_pos,
        encoder_position: target_pos,
        done: true,
        moving: false,
        // The modeled axis has an encoder; without this the poll-time
        // encoder check (C 3671-3675) demotes the UEIP=Yes tests.
        has_encoder: true,
        ..Default::default()
    };
    rec.process_motor_info(&status);
}

#[test]
fn rmod_default_retry_moves_to_original_target() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 5;
    rec.retry.rmod = RetryMode::Default;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    // Motor stops short by 1.0
    complete_move(&mut rec, 9.0);
    let effects = rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "retry marked in MIP"
    );
    assert_eq!(rec.retry.rcnt, 1);
    // C Default: retry moves to the original target position (dval)
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - 10.0).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }
}

#[test]
fn rmod_arithmetic_retry() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.01;
    rec.retry.rtry = 5;
    rec.retry.frac = 0.5;
    rec.retry.rmod = RetryMode::Arithmetic;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    complete_move(&mut rec, 9.0);
    let effects = rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "retry marked in MIP"
    );
    // C absolute (use_rel=false) retry: position = dval, regardless of RMOD.
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - 10.0).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }
}

#[test]
fn rmod_arithmetic_absolute_retry_targets_dval_for_rcnt_ge_2() {
    // C: the absolute (use_rel=false) retry path computes
    // position = currpos + frac*(newpos-currpos) with currpos == newpos == dval,
    // so it is always dval. RMOD's factor scaling only reaches the use_rel
    // path. A factor < 1 (rcnt >= 2) must NOT pull the absolute target off dval.
    let mut rec = make_record();
    rec.retry.rdbd = 0.01;
    rec.retry.rtry = 5;
    rec.retry.frac = 0.5;
    rec.retry.rmod = RetryMode::Arithmetic;
    // make_record sets neither UEIP nor URIP → use_rel == false.
    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);
    rec.retry.rcnt = 1; // one retry already done; this completion makes rcnt=2

    complete_move(&mut rec, 9.0); // error 1.0 > rdbd
    let effects = rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "retry marked in MIP"
    );
    assert_eq!(rec.retry.rcnt, 2); // factor = (5-2+1)/5 = 0.8 < 1
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        // Old (buggy) behaviour scaled this to 9.84; C parity is dval = 10.0.
        assert!((*position - 10.0).abs() < 1e-10, "got {position}");
    } else {
        panic!("expected MoveAbsolute");
    }
}

#[test]
fn rmod_geometric_retry() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.01;
    rec.retry.rtry = 5;
    rec.retry.rmod = RetryMode::Geometric;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    complete_move(&mut rec, 9.0);
    let effects = rec.check_completion();

    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "retry marked in MIP"
    );
    // Geometric: target = dval
    if let MotorCommand::MoveAbsolute { position, .. } = &effects.commands[0] {
        assert!((*position - 10.0).abs() < 1e-10);
    } else {
        panic!("expected MoveAbsolute");
    }
}

#[test]
fn rmod_inposition_settle_loop_holds_dmov() {
    // C RMOD_I (motorRecord.cc:1432-1438): no move is re-issued; the
    // settle watchdog re-arms unconditionally — with DLY == 0 it fires
    // immediately — and maybeRetry (1077-1080) holds DMOV at FALSE
    // across the loop. Each cycle counts against RTRY (1059 ++rcnt has
    // no RMOD_I exemption); only close-enough or give-up finalizes.
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 5;
    rec.retry.rmod = RetryMode::InPosition;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    // Cycle 1: motor stops with error > rdbd — no move re-issued, the
    // settle wait re-arms, DMOV stays low.
    complete_move(&mut rec, 9.0);
    let effects = rec.check_completion();
    assert!(effects.commands.is_empty(), "no move re-issued");
    assert!(!rec.stat.dmov, "DMOV holds FALSE through the settle loop");
    assert_eq!(rec.stat.phase, MotionPhase::DelayWait);
    assert!(rec.stat.mip.contains(MipFlags::RETRY));
    assert!(rec.stat.mip.contains(MipFlags::DELAY_REQ));
    assert!(effects.schedule_delay.is_some(), "watchdog re-armed");
    assert_eq!(rec.retry.rcnt, 1);

    // Cycle 2: watchdog fires, servo still out of position — another
    // settle cycle, still not done.
    rec.set_event(MotorEvent::DelayExpired);
    let _ = rec.do_process();
    complete_move(&mut rec, 9.05);
    let effects = rec.check_completion();
    assert!(effects.commands.is_empty());
    assert!(!rec.stat.dmov);
    assert_eq!(rec.retry.rcnt, 2);

    // Servo settles within RDBD: the next evaluation finalizes.
    rec.set_event(MotorEvent::DelayExpired);
    let _ = rec.do_process();
    complete_move(&mut rec, 10.0);
    let _ = rec.check_completion();
    assert!(rec.stat.dmov, "close-enough ends the settle loop");
    assert_eq!(rec.stat.phase, MotionPhase::Idle);
    assert!(rec.stat.mip.is_empty());
    assert_eq!(rec.retry.rcnt, 2, "the converged pass does not count");
}

#[test]
fn rmod_inposition_settle_loop_gives_up_at_rtry() {
    // C 1059-1075: the give-up branch has no RMOD_I exemption — when
    // the servo never settles, ++rcnt exceeds RTRY, MISS latches, and
    // the record finalizes.
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 2;
    rec.retry.rmod = RetryMode::InPosition;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    complete_move(&mut rec, 9.0);
    rec.check_completion();
    assert_eq!(rec.retry.rcnt, 1);
    rec.set_event(MotorEvent::DelayExpired);
    let _ = rec.do_process();
    complete_move(&mut rec, 9.0);
    rec.check_completion();
    assert_eq!(rec.retry.rcnt, 2);
    assert!(!rec.stat.dmov);

    // rcnt == rtry: the next far evaluation gives up.
    rec.set_event(MotorEvent::DelayExpired);
    let _ = rec.do_process();
    complete_move(&mut rec, 9.0);
    rec.check_completion();
    assert!(rec.retry.miss, "give-up latches MISS");
    assert!(rec.stat.dmov, "give-up finalizes");
    assert_eq!(rec.retry.rcnt, 3, "the give-up pass still counts");
}

#[test]
fn rcnt_increments_and_resets() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 3;
    rec.retry.rmod = RetryMode::Geometric;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    // Retry 1
    complete_move(&mut rec, 9.0);
    rec.check_completion();
    assert_eq!(rec.retry.rcnt, 1);

    // Retry 2
    complete_move(&mut rec, 9.5);
    rec.check_completion();
    assert_eq!(rec.retry.rcnt, 2);

    // Retry 3
    complete_move(&mut rec, 9.8);
    rec.check_completion();
    assert_eq!(rec.retry.rcnt, 3);

    // Exhausted — MISS set, finalize
    complete_move(&mut rec, 9.85); // still > rdbd
    let _effects = rec.check_completion();
    assert!(rec.retry.miss);
    assert!(rec.stat.dmov);

    // New move resets RCNT; MISS stays latched through the dispatch —
    // C touches miss only inside maybeRetry (1072 set, 1092 clear) —
    // and clears when the new move lands close enough.
    rec.pos.dval = 20.0;
    rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.retry.rcnt, 0);
    assert!(rec.retry.miss, "dispatch must not clear MISS");
    complete_move(&mut rec, 20.0);
    rec.check_completion();
    assert!(!rec.retry.miss, "close-enough evaluation clears MISS");
}

/// BUG 4 regression: the retry deadband comparison must be
/// boundary-INCLUSIVE. C `maybeRetry` (motorRecord.cc:1049) uses
/// `fabs(pmr->diff) >= pmr->rdbd`. At exactly `diff == rdbd` C retries;
/// an earlier Rust version used `diff > rdbd` and finalized instead.
#[test]
fn retry_fires_when_diff_exactly_equals_rdbd() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.25;
    rec.retry.rtry = 5;
    rec.retry.rmod = RetryMode::Default;

    // Target 0.5, motor stops at 0.25 — every value is exactly
    // representable in binary floating point, so `diff` is precisely
    // `0.5 - 0.25 == 0.25 == rdbd`.
    rec.pos.dval = 0.5;
    rec.plan_motion(CommandSource::Val);

    complete_move(&mut rec, 0.25);
    let diff = (rec.pos.dval - rec.pos.drbv).abs();
    assert_eq!(diff, rec.retry.rdbd, "test setup: diff must equal rdbd");

    let effects = rec.check_completion();

    // C `>=`: at the boundary the axis retries.
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "diff == rdbd must trigger a retry (C uses >=)"
    );
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert_eq!(rec.retry.rcnt, 1);
    assert_eq!(effects.commands.len(), 1, "retry must emit a move command");
}

/// BUG 4 regression: MISS must latch when the axis finalizes (retries
/// exhausted) with `fabs(diff) >= rdbd` — boundary-inclusive, matching
/// C `maybeRetry`.
#[test]
fn miss_latches_when_diff_exactly_equals_rdbd_at_exhaustion() {
    let mut rec = make_record();
    rec.retry.rdbd = 0.25;
    rec.retry.rtry = 1;
    rec.retry.rmod = RetryMode::Default;

    // Exactly representable: diff = 0.5 - 0.25 == 0.25 == rdbd.
    rec.pos.dval = 0.5;
    rec.plan_motion(CommandSource::Val);

    complete_move(&mut rec, 0.25);
    rec.retry.rcnt = 1; // retries exhausted (C ++rcnt > rtry)
    let diff = (rec.pos.dval - rec.pos.drbv).abs();
    assert_eq!(diff, rec.retry.rdbd, "test setup: diff must equal rdbd");

    let _effects = rec.check_completion();

    assert!(rec.stat.dmov, "axis finalizes (retries exhausted)");
    assert!(
        rec.retry.miss,
        "MISS must latch at diff == rdbd on exhaustion (C uses >=)"
    );
}

#[test]
fn spdb_suppresses_small_move() {
    let mut rec = make_record();
    rec.retry.spdb = 0.5;
    rec.pos.drbv = 10.0;

    // Move within SPDB deadband
    rec.pos.dval = 10.3; // |10.3 - 10.0| = 0.3 <= 0.5
    let effects = rec.plan_motion(CommandSource::Val);

    // No move initiated
    assert!(effects.commands.is_empty());
    assert!(rec.stat.dmov); // unchanged
}

#[test]
fn spdb_allows_larger_move() {
    let mut rec = make_record();
    rec.retry.spdb = 0.5;
    rec.pos.drbv = 10.0;

    // Move outside SPDB deadband
    rec.pos.dval = 11.0; // |11.0 - 10.0| = 1.0 > 0.5
    let effects = rec.plan_motion(CommandSource::Val);

    assert!(!effects.commands.is_empty());
    assert!(!rec.stat.dmov);
}

#[test]
fn spdb_vs_rdbd_are_independent() {
    let mut rec = make_record();
    rec.retry.spdb = 0.5; // move initiation deadband
    rec.retry.rdbd = 0.01; // completion deadband
    rec.retry.rtry = 3;
    rec.retry.rmod = RetryMode::Geometric;

    // Move large enough to pass SPDB
    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    // Motor stops with error > RDBD but < SPDB
    complete_move(&mut rec, 9.8); // error=0.2 > rdbd=0.01
    let effects = rec.check_completion();

    // Should still retry (RDBD controls retry, not SPDB)
    assert_eq!(rec.stat.phase, MotionPhase::MainMove);
    assert!(
        rec.stat.mip.contains(MipFlags::RETRY),
        "retry marked in MIP"
    );
    assert_eq!(rec.retry.rcnt, 1);
    assert!(!effects.commands.is_empty());
}

#[test]
fn rmod_geometric_use_rel_scales_and_floors_relative_leg() {
    // C 2370-2383: a use_rel retry scales relpos by 1/2^(rcnt-1) and
    // floors the scaled leg at one raw step. mres=0.1, remaining error
    // 0.3 EGU, rcnt 4→5: factor 1/16 gives 0.01875 EGU = 0.1875 steps,
    // floored to 1 step = 0.1 EGU.
    let mut rec = make_record();
    rec.conv.mres = 0.1;
    rec.conv.ueip = true; // → use_rel
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 10;
    rec.retry.frac = 1.0;
    rec.retry.rmod = RetryMode::Geometric;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);
    rec.retry.rcnt = 4; // four retries already counted

    complete_move(&mut rec, 9.7); // step-aligned; diff = 0.3 >= rdbd
    let effects = rec.check_completion();

    assert_eq!(rec.retry.rcnt, 5);
    assert!(rec.stat.mip.contains(MipFlags::RETRY));
    let dist = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveRelative { distance, .. } => Some(*distance),
        _ => None,
    });
    let dist = dist.expect("use_rel retry emits MoveRelative");
    assert!(
        (dist - 0.1).abs() < 1e-9,
        "scaled leg floored at one step (0.1 EGU), got {dist}"
    );
}

#[test]
fn rmod_arithmetic_use_rel_scales_relative_leg() {
    // C 2358-2368: arithmetic retry scales relpos by (rtry-rcnt+1)/rtry.
    // rtry=5, rcnt 1→2: factor (5-2+1)/5 = 0.8; relpos = 1.0 EGU → 0.8.
    let mut rec = make_record();
    rec.conv.mres = 0.001;
    rec.conv.ueip = true;
    rec.retry.rdbd = 0.01;
    rec.retry.rtry = 5;
    rec.retry.frac = 1.0;
    rec.retry.rmod = RetryMode::Arithmetic;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);
    rec.retry.rcnt = 1;

    complete_move(&mut rec, 9.0); // diff = 1.0
    let effects = rec.check_completion();

    assert_eq!(rec.retry.rcnt, 2);
    let dist = effects.commands.iter().find_map(|c| match c {
        MotorCommand::MoveRelative { distance, .. } => Some(*distance),
        _ => None,
    });
    let dist = dist.expect("use_rel retry emits MoveRelative");
    assert!(
        (dist - 0.8).abs() < 1e-9,
        "arithmetic factor 0.8 scales the leg, got {dist}"
    );
}

#[test]
fn rcnt_persists_after_giveup_until_next_dispatch() {
    // C maybeRetry 1059: the give-up pass is `++rcnt > rtry`, so RCNT
    // posts rtry+1 and KEEPS that value through the finalize — no
    // completion-time zeroing. Only the next dispatch resets it
    // (2352-2356).
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 2;
    rec.retry.rmod = RetryMode::Default;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);

    complete_move(&mut rec, 9.0);
    rec.check_completion(); // rcnt 1
    complete_move(&mut rec, 9.2);
    rec.check_completion(); // rcnt 2
    complete_move(&mut rec, 9.3); // still short: exhausted
    rec.check_completion();

    assert!(rec.stat.dmov);
    assert!(rec.retry.miss);
    assert_eq!(rec.retry.rcnt, 3, "give-up pass counts: RCNT = rtry + 1");

    // Next (non-retry) dispatch resets it.
    rec.pos.dval = 20.0;
    rec.plan_motion(CommandSource::Val);
    assert_eq!(rec.retry.rcnt, 0);
}

#[test]
fn home_dispatch_resets_rcnt() {
    // C 2069: the home dispatch resets the retry counter.
    let mut rec = make_record();
    rec.retry.rdbd = 0.1;
    rec.retry.rtry = 2;

    rec.pos.dval = 10.0;
    rec.plan_motion(CommandSource::Val);
    complete_move(&mut rec, 9.0);
    rec.check_completion();
    assert_eq!(rec.retry.rcnt, 1);
    // Abandon the retry with a stop, then home.
    rec.ctrl.stop = true;
    rec.plan_motion(CommandSource::Stop);
    complete_move(&mut rec, 9.0);
    rec.check_completion();

    rec.ctrl.homf = true;
    rec.plan_motion(CommandSource::Homf);
    assert_eq!(rec.retry.rcnt, 0, "home dispatch resets RCNT (C 2069)");
}
