//! Hand-written Rust implementation of demo.st
//!
//! This is what the snc compiler would eventually generate.
//! Used to validate the seq runtime API (Phase A).

use seq::prelude::*;

// --- Channel IDs ---
const CH_COUNTER: usize = 0;
const CH_LIGHT: usize = 1;

// --- Event Flag IDs ---
const EF_COUNTER: usize = 0;

// --- State IDs for counter_ss ---
const COUNTER_INIT: usize = 0;
const COUNTER_COUNTING: usize = 1;
const COUNTER_DONE: usize = 2;

// --- State IDs for light_ss ---
const LIGHT_IDLE: usize = 0;

// --- Program Variables ---
#[derive(Clone)]
struct DemoVars {
    counter: f64,
    light: i32,
}

impl ProgramVars for DemoVars {
    fn get_channel_value(&self, ch_id: usize) -> EpicsValue {
        match ch_id {
            CH_COUNTER => EpicsValue::Double(self.counter),
            CH_LIGHT => EpicsValue::Long(self.light),
            _ => EpicsValue::Double(0.0),
        }
    }

    fn set_channel_value(&mut self, ch_id: usize, value: &EpicsValue) {
        match ch_id {
            CH_COUNTER => {
                if let Some(v) = value.to_f64() {
                    self.counter = v;
                }
            }
            CH_LIGHT => {
                if let Some(v) = value.to_f64() {
                    self.light = v as i32;
                }
            }
            _ => {}
        }
    }
}

// --- Program Metadata ---
struct DemoMeta;

impl ProgramMeta for DemoMeta {
    const NUM_CHANNELS: usize = 2;
    const NUM_EVENT_FLAGS: usize = 1;
    const NUM_STATE_SETS: usize = 2;

    fn channel_defs() -> Vec<ChannelDef> {
        vec![
            ChannelDef {
                var_name: "counter".into(),
                pv_name: "{P}counter".into(),
                monitored: true,
                sync_ef: Some(EF_COUNTER),
            },
            ChannelDef {
                var_name: "light".into(),
                pv_name: "{P}light".into(),
                monitored: false,
                sync_ef: None,
            },
        ]
    }

    fn event_flag_sync_map() -> Vec<Vec<usize>> {
        vec![
            vec![CH_COUNTER], // EF_COUNTER → [CH_COUNTER]
        ]
    }
}

// --- State set: counter_ss ---
async fn counter_ss(mut ctx: StateSetContext<DemoVars>) -> SeqResult<()> {
    // State names for logging
    let state_names = ["init", "counting", "done"];

    ctx.enter_state(COUNTER_INIT);
    ctx.wakeup().notify_one(); // initial trigger

    loop {
        if ctx.is_shutdown() {
            break;
        }

        // Entry actions
        if ctx.should_run_entry() {
            let name = state_names.get(ctx.current_state()).unwrap_or(&"?");
            tracing::info!("[counter_ss] entering state '{name}'");
        }

        // Inner evaluation loop
        loop {
            ctx.wait_for_wakeup().await;

            if ctx.is_shutdown() {
                return Ok(());
            }

            ctx.reset_wakeup();
            ctx.sync_dirty_vars();

            // Evaluate when conditions based on current state
            match ctx.current_state() {
                COUNTER_INIT => {
                    if ctx.delay(1.0) {
                        ctx.local_vars.counter = 0.0;
                        ctx.pv_put(CH_COUNTER, CompType::Default).await;
                        tracing::info!("[counter_ss] counter = 0.0, pvPut done");
                        ctx.transition_to(COUNTER_COUNTING);
                    }
                }
                COUNTER_COUNTING => {
                    if ctx.local_vars.counter >= 10.0 {
                        tracing::info!("[counter_ss] counter >= 10, transitioning to done");
                        ctx.transition_to(COUNTER_DONE);
                    } else if ctx.delay(1.0) {
                        ctx.local_vars.counter += 1.0;
                        ctx.pv_put(CH_COUNTER, CompType::Default).await;
                        tracing::info!(
                            "[counter_ss] counter = {:.1}, pvPut done",
                            ctx.local_vars.counter
                        );
                        ctx.transition_to(COUNTER_COUNTING);
                    }
                }
                COUNTER_DONE => {
                    if ctx.delay(0.1) {
                        // exit
                        tracing::info!("[counter_ss] done, exiting");
                        return Ok(());
                    }
                }
                _ => return Err(SeqError::InvalidStateId(ctx.current_state())),
            }

            if ctx.has_transition() {
                break; // exit inner loop to perform state transition
            }
        }

        // Exit actions
        if ctx.should_run_exit() {
            let name = state_names.get(ctx.current_state()).unwrap_or(&"?");
            tracing::info!("[counter_ss] exiting state '{name}'");
        }

        // Perform transition
        if let Some(next) = ctx.take_transition() {
            ctx.enter_state(next);
            ctx.wakeup().notify_one(); // guarantee at least one evaluation
        }
    }

    Ok(())
}

// --- State set: light_ss ---
async fn light_ss(mut ctx: StateSetContext<DemoVars>) -> SeqResult<()> {
    ctx.enter_state(LIGHT_IDLE);
    ctx.wakeup().notify_one();

    loop {
        if ctx.is_shutdown() {
            break;
        }

        // Entry
        if ctx.should_run_entry() {
            tracing::info!("[light_ss] entering state 'idle'");
        }

        // Inner loop
        loop {
            ctx.wait_for_wakeup().await;

            if ctx.is_shutdown() {
                return Ok(());
            }

            ctx.reset_wakeup();
            ctx.sync_dirty_vars();

            match ctx.current_state() {
                LIGHT_IDLE => {
                    if ctx.ef_test_and_clear(EF_COUNTER) {
                        if ctx.local_vars.counter > 0.0 {
                            ctx.local_vars.light = 1;
                        } else {
                            ctx.local_vars.light = 0;
                        }
                        ctx.pv_put(CH_LIGHT, CompType::Default).await;
                        tracing::info!(
                            "[light_ss] light = {}, counter = {:.1}",
                            ctx.local_vars.light,
                            ctx.local_vars.counter
                        );
                        ctx.transition_to(LIGHT_IDLE);
                    } else if ctx.delay(15.0) {
                        tracing::info!("[light_ss] timeout, exiting");
                        return Ok(());
                    }
                }
                _ => return Err(SeqError::InvalidStateId(ctx.current_state())),
            }

            if ctx.has_transition() {
                break;
            }
        }

        // Exit
        if ctx.should_run_exit() {
            tracing::info!("[light_ss] exiting state 'idle'");
        }

        if let Some(next) = ctx.take_transition() {
            ctx.enter_state(next);
            ctx.wakeup().notify_one();
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let macro_str = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "P=SEQ:".to_string());

    let initial = DemoVars {
        counter: 0.0,
        light: 0,
    };

    ProgramBuilder::<DemoVars, DemoMeta>::new("demo", initial)
        .macros(&macro_str)
        .add_ss(Box::new(|ctx| Box::pin(counter_ss(ctx))))
        .add_ss(Box::new(|ctx| Box::pin(light_ss(ctx))))
        .run()
        .await?;

    Ok(())
}
