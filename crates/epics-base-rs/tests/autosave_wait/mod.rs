//! A test-only iocsh command that holds a script until the save_restore task
//! has served every op sent before it, so the script's order against the task
//! is what the task did, not what a sleep made likely. The task serves ops
//! only after a whole pass (save_restore.c:1129-1131), so a wait straight
//! after the first `create_*_set` also outlasts that set's first
//! `connect_list`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::iocsh::registry::{
    ArgValue, CommandContext, CommandDef, CommandOutcome,
};

/// `wait_ops_served`, whose line fails after a minute.
pub fn wait_ops_served(holder: Arc<Mutex<AutosaveStartupConfig>>) -> CommandDef {
    const BOUND: Duration = Duration::from_secs(60);
    CommandDef::new(
        "wait_ops_served",
        vec![],
        "hold the script until the save_restore task has served every op sent before it",
        move |_args: &[ArgValue], ctx: &CommandContext| {
            let mgr = holder
                .lock()
                .unwrap()
                .manager
                .clone()
                .ok_or_else(|| "wait_ops_served: no save_restore task".to_string())?;
            ctx.block_on(async {
                tokio::select! {
                    _ = mgr.ops_served() => Ok(CommandOutcome::Continue),
                    _ = epics_base_rs::runtime::task::sleep(BOUND) => {
                        Err(format!("wait_ops_served: not served in {BOUND:?}"))
                    }
                }
            })
        },
    )
}
