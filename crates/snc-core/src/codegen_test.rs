#[cfg(test)]
mod tests {
    use crate::codegen::generate;
    use crate::ir::*;

    fn demo_ir() -> SeqIR {
        SeqIR {
            program_name: "demo".to_string(),
            options: ProgramOptions {
                safe_mode: true,
                ..Default::default()
            },
            channels: vec![
                IRChannel {
                    id: 0,
                    var_name: "counter".to_string(),
                    pv_name: "{P}counter".to_string(),
                    var_type: IRType::Double,
                    monitored: true,
                    sync_ef: Some(0),
                },
                IRChannel {
                    id: 1,
                    var_name: "light".to_string(),
                    pv_name: "{P}light".to_string(),
                    var_type: IRType::Int,
                    monitored: false,
                    sync_ef: None,
                },
            ],
            event_flags: vec![IREventFlag {
                id: 0,
                name: "ef_counter".to_string(),
                synced_channels: vec![0],
            }],
            variables: vec![
                IRVariable {
                    name: "counter".to_string(),
                    var_type: IRType::Double,
                    channel_id: Some(0),
                    init_value: None,
                },
                IRVariable {
                    name: "light".to_string(),
                    var_type: IRType::Int,
                    channel_id: Some(1),
                    init_value: None,
                },
            ],
            state_sets: vec![
                IRStateSet {
                    name: "counter_ss".to_string(),
                    id: 0,
                    local_vars: vec![],
                    states: vec![
                        IRState {
                            name: "init".to_string(),
                            id: 0,
                            entry: None,
                            transitions: vec![IRTransition {
                                condition: Some("ctx.delay(1.0)".to_string()),
                                action: IRBlock::new(
                                    "ctx.local_vars.counter = 0.0;\nctx.pv_put(CH_COUNTER, CompType::Default).await;",
                                ),
                                target_state: Some(1),
                            }],
                            exit: None,
                        },
                        IRState {
                            name: "counting".to_string(),
                            id: 1,
                            entry: None,
                            transitions: vec![
                                IRTransition {
                                    condition: Some("ctx.local_vars.counter >= 10.0".to_string()),
                                    action: IRBlock::new(""),
                                    target_state: Some(2),
                                },
                                IRTransition {
                                    condition: Some("ctx.delay(1.0)".to_string()),
                                    action: IRBlock::new(
                                        "ctx.local_vars.counter += 1.0;\nctx.pv_put(CH_COUNTER, CompType::Default).await;",
                                    ),
                                    target_state: Some(1),
                                },
                            ],
                            exit: None,
                        },
                        IRState {
                            name: "done".to_string(),
                            id: 2,
                            entry: None,
                            transitions: vec![IRTransition {
                                condition: Some("ctx.delay(0.1)".to_string()),
                                action: IRBlock::new(""),
                                target_state: None, // exit
                            }],
                            exit: None,
                        },
                    ],
                },
                IRStateSet {
                    name: "light_ss".to_string(),
                    id: 1,
                    local_vars: vec![],
                    states: vec![IRState {
                        name: "idle".to_string(),
                        id: 0,
                        entry: None,
                        transitions: vec![
                            IRTransition {
                                condition: Some("ctx.ef_test_and_clear(EF_EF_COUNTER)".to_string()),
                                action: IRBlock::new(
                                    "if ctx.local_vars.counter > 0.0 {\n    ctx.local_vars.light = 1;\n} else {\n    ctx.local_vars.light = 0;\n}\nctx.pv_put(CH_LIGHT, CompType::Default).await;",
                                ),
                                target_state: Some(0),
                            },
                            IRTransition {
                                condition: Some("ctx.delay(15.0)".to_string()),
                                action: IRBlock::new(""),
                                target_state: None, // exit
                            },
                        ],
                        exit: None,
                    }],
                },
            ],
            entry_block: None,
            exit_block: None,
        }
    }

    #[test]
    fn test_codegen_produces_valid_structure() {
        let ir = demo_ir();
        let code = generate(&ir);

        // Check key structural elements
        assert!(code.contains("use seq::prelude::*;"));
        assert!(code.contains("const CH_COUNTER: usize = 0;"));
        assert!(code.contains("const CH_LIGHT: usize = 1;"));
        assert!(code.contains("const EF_EF_COUNTER: usize = 0;"));
        assert!(code.contains("struct demoVars {"));
        assert!(code.contains("counter: f64,"));
        assert!(code.contains("light: i32,"));
        assert!(code.contains("impl ProgramVars for demoVars"));
        assert!(code.contains("impl ProgramMeta for demoMeta"));
        assert!(code.contains("async fn counter_ss("));
        assert!(code.contains("async fn light_ss("));
        assert!(code.contains("#[tokio::main]"));
        assert!(code.contains("ProgramBuilder::<demoVars, demoMeta>"));
    }

    #[test]
    fn test_codegen_channel_defs() {
        let ir = demo_ir();
        let code = generate(&ir);
        assert!(code.contains("monitored: true,"));
        assert!(code.contains("monitored: false,"));
        assert!(code.contains("sync_ef: Some(0),"));
        assert!(code.contains("sync_ef: None,"));
    }

    #[test]
    fn test_codegen_state_machine_structure() {
        let ir = demo_ir();
        let code = generate(&ir);
        // Check the main loop structure
        assert!(code.contains("ctx.enter_state(0);"));
        assert!(code.contains("ctx.wakeup().notify_one();"));
        assert!(code.contains("ctx.wait_for_wakeup().await;"));
        assert!(code.contains("ctx.sync_dirty_vars();"));
        assert!(code.contains("ctx.has_transition()"));
        assert!(code.contains("ctx.take_transition()"));
    }
}
