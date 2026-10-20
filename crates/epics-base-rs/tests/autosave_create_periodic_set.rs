//! `create_periodic_set(filename, period, macrostring)` (save_restore.c:2291):
//! C's PERIODIC method, saved every `MAX(period, MIN_PERIOD)` seconds.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::save_set::SaveStrategy;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext};
use epics_base_rs::server::records::ao::AoRecord;

fn holder(dir: &tempfile::TempDir) -> Arc<Mutex<AutosaveStartupConfig>> {
    std::fs::write(dir.path().join("settings.req"), "IOC:setpoint\n").unwrap();
    let mut cfg = AutosaveStartupConfig::new();
    cfg.request_file_paths.push(dir.path().to_path_buf());
    cfg.save_file_path = Some(dir.path().to_path_buf());
    Arc::new(Mutex::new(cfg))
}

fn create_periodic_set(
    holder: &Arc<Mutex<AutosaveStartupConfig>>,
    db: &Arc<PvDatabase>,
    period: i64,
) {
    let cmds = AutosaveStartupConfig::register_startup_commands(holder.clone());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let db = db.clone();
    std::thread::spawn(move || {
        let ctx = CommandContext::new(db, bridge);
        cmds.iter()
            .find(|c| c.name == "create_periodic_set")
            .expect("create_periodic_set is registered")
            .handler
            .call(
                &[
                    ArgValue::String("settings.req".into()),
                    ArgValue::Int(period),
                ],
                &ctx,
            )
            .expect("create_periodic_set");
    })
    .join()
    .unwrap();
}

async fn interval(period: i64) -> Duration {
    let dir = tempfile::tempdir().unwrap();
    let holder = holder(&dir);
    create_periodic_set(&holder, &Arc::new(PvDatabase::new()), period);
    let mgr = holder.lock().unwrap().manager.clone().unwrap();
    match &mgr.sets()[0].0.methods()[..] {
        [SaveStrategy::Periodic { interval }] => *interval,
        other => panic!("expected one periodic method, got {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn the_period_is_floored_at_min_period() {
    assert_eq!(interval(0).await, Duration::from_secs(4));
    assert_eq!(interval(-1).await, Duration::from_secs(4));
    assert_eq!(interval(3).await, Duration::from_secs(4));
    assert_eq!(interval(30).await, Duration::from_secs(30));
}

/// Unlike a monitor set, nothing has to change: the set saves on the timer.
#[epics_macros_rs::epics_test]
async fn the_set_saves_every_period() {
    let dir = tempfile::tempdir().unwrap();
    let holder = holder(&dir);
    let db = Arc::new(PvDatabase::new());
    db.add_record("IOC:setpoint", Box::new(AoRecord::new(1.5)))
        .await
        .unwrap();
    // The create starts the holder's manager and task; a second manager
    // would share its op queue and race it.
    create_periodic_set(&holder, &db, 4);
    let mgr = holder.lock().unwrap().manager.clone().unwrap();

    let sav = dir.path().join("settings.sav");
    let mut saves = 0;
    let mut last = None;
    for _ in 0..120 {
        let now = std::fs::metadata(&sav).and_then(|m| m.modified()).ok();
        if now.is_some() && now != last {
            saves += 1;
            last = now;
            if saves == 2 {
                break;
            }
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(saves, 2, "two saves 4 s apart with nothing changed");
    mgr.shutdown();
}
