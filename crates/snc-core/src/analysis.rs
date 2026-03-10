use std::collections::HashMap;

use crate::ast::{self, *};
use crate::error::{CompileError, CompileResult};
use crate::ir::*;

/// Analyze a parsed AST and produce the lowered IR.
pub fn analyze(program: &Program) -> CompileResult<SeqIR> {
    let mut analyzer = Analyzer::new();
    analyzer.analyze_program(program)
}

struct Analyzer {
    channels: Vec<IRChannel>,
    event_flags: Vec<IREventFlag>,
    variables: Vec<IRVariable>,
    // Lookup maps
    var_to_channel: HashMap<String, usize>,
    var_types: HashMap<String, IRType>,
    ef_name_to_id: HashMap<String, usize>,
    // Sync: var_name → ef_name
    sync_map: HashMap<String, String>,
    // Options
    options: ProgramOptions,
}

impl Analyzer {
    fn new() -> Self {
        Self {
            channels: Vec::new(),
            event_flags: Vec::new(),
            variables: Vec::new(),
            var_to_channel: HashMap::new(),
            var_types: HashMap::new(),
            ef_name_to_id: HashMap::new(),
            sync_map: HashMap::new(),
            options: ProgramOptions::default(),
        }
    }

    fn analyze_program(&mut self, program: &Program) -> CompileResult<SeqIR> {
        // Pass 1: collect options
        for opt in &program.options {
            match opt {
                ProgramOption::Safe => self.options.safe_mode = true,
                ProgramOption::Reentrant => self.options.reentrant = true,
                ProgramOption::Main => self.options.main_flag = true,
            }
        }

        // Pass 2: collect declarations
        for def in &program.definitions {
            match def {
                Definition::VarDecl(vd) => self.collect_var_decl(vd)?,
                Definition::EvFlag(ef) => self.collect_evflag(ef)?,
                Definition::Assign(a) => self.collect_assign(a)?,
                Definition::Monitor(m) => self.collect_monitor(m)?,
                Definition::Sync(s) => self.collect_sync(s)?,
                Definition::Option(_) | Definition::CPreprocessor(_) | Definition::EmbeddedCode(_) => {}
            }
        }

        // Pass 3: resolve sync → event flag → channels
        self.resolve_sync()?;

        // Pass 4: analyze state sets
        let mut state_sets = Vec::new();
        for (ss_id, ss) in program.state_sets.iter().enumerate() {
            state_sets.push(self.analyze_state_set(ss, ss_id)?);
        }

        Ok(SeqIR {
            program_name: program.name.clone(),
            options: self.options.clone(),
            channels: self.channels.clone(),
            event_flags: self.event_flags.clone(),
            variables: self.variables.clone(),
            state_sets,
            entry_block: program.entry.as_ref().map(|b| self.lower_block(b)),
            exit_block: program.exit.as_ref().map(|b| self.lower_block(b)),
        })
    }

    fn collect_var_decl(&mut self, vd: &VarDecl) -> CompileResult<()> {
        let mut ir_type = self.convert_type(&vd.type_spec)?;
        // Wrap in Array if dimensions are present (innermost first)
        for dim_expr in vd.dimensions.iter().rev() {
            let size = self.eval_const_expr(dim_expr);
            ir_type = IRType::Array {
                element: Box::new(ir_type),
                size,
            };
        }
        let init_value = vd.init.as_ref().map(|e| self.expr_to_rust(e));
        self.var_types.insert(vd.name.clone(), ir_type.clone());
        self.variables.push(IRVariable {
            name: vd.name.clone(),
            var_type: ir_type,
            channel_id: None,
            init_value,
        });
        Ok(())
    }

    /// Evaluate a constant expression for array dimension sizing.
    fn eval_const_expr(&self, expr: &Expr) -> usize {
        match expr {
            Expr::IntLit(v, _) => *v as usize,
            _ => 0, // non-constant dimensions not supported
        }
    }

    fn collect_evflag(&mut self, ef: &EvFlagDecl) -> CompileResult<()> {
        let id = self.event_flags.len();
        self.ef_name_to_id.insert(ef.name.clone(), id);
        self.event_flags.push(IREventFlag {
            id,
            name: ef.name.clone(),
            synced_channels: Vec::new(),
        });
        Ok(())
    }

    fn collect_assign(&mut self, assign: &Assign) -> CompileResult<()> {
        let pv_name = assign
            .pv_name
            .as_deref()
            .unwrap_or(&assign.var_name)
            .to_string();

        let ch_id = self.channels.len();
        let var_type = self
            .var_types
            .get(&assign.var_name)
            .cloned()
            .ok_or_else(|| {
                CompileError::semantic(
                    assign.span.clone(),
                    format!("assign: undefined variable '{}'", assign.var_name),
                )
            })?;

        self.channels.push(IRChannel {
            id: ch_id,
            var_name: assign.var_name.clone(),
            pv_name,
            var_type,
            monitored: false,
            sync_ef: None,
        });

        self.var_to_channel.insert(assign.var_name.clone(), ch_id);

        // Update the variable's channel_id
        if let Some(var) = self.variables.iter_mut().find(|v| v.name == assign.var_name) {
            var.channel_id = Some(ch_id);
        }

        Ok(())
    }

    fn collect_monitor(&mut self, monitor: &Monitor) -> CompileResult<()> {
        if let Some(&ch_id) = self.var_to_channel.get(&monitor.var_name) {
            self.channels[ch_id].monitored = true;
            Ok(())
        } else {
            Err(CompileError::semantic(
                monitor.span.clone(),
                format!(
                    "monitor: variable '{}' is not assigned to a PV",
                    monitor.var_name
                ),
            ))
        }
    }

    fn collect_sync(&mut self, sync: &Sync) -> CompileResult<()> {
        self.sync_map
            .insert(sync.var_name.clone(), sync.ef_name.clone());
        Ok(())
    }

    fn resolve_sync(&mut self) -> CompileResult<()> {
        for (var_name, ef_name) in &self.sync_map {
            let ef_id = self
                .ef_name_to_id
                .get(ef_name)
                .copied()
                .ok_or_else(|| {
                    CompileError::Other(format!("sync: undefined event flag '{ef_name}'"))
                })?;

            let ch_id = self.var_to_channel.get(var_name).copied().ok_or_else(|| {
                CompileError::Other(format!(
                    "sync: variable '{var_name}' is not assigned to a PV"
                ))
            })?;

            self.channels[ch_id].sync_ef = Some(ef_id);
            self.event_flags[ef_id].synced_channels.push(ch_id);
        }
        Ok(())
    }

    fn analyze_state_set(&self, ss: &StateSet, ss_id: usize) -> CompileResult<IRStateSet> {
        let local_vars: Vec<IRVariable> = ss
            .local_vars
            .iter()
            .map(|vd| {
                let mut ir_type = self.convert_type(&vd.type_spec).unwrap();
                for dim_expr in vd.dimensions.iter().rev() {
                    let size = self.eval_const_expr(dim_expr);
                    ir_type = IRType::Array {
                        element: Box::new(ir_type),
                        size,
                    };
                }
                IRVariable {
                    name: vd.name.clone(),
                    var_type: ir_type,
                    channel_id: None,
                    init_value: vd.init.as_ref().map(|e| self.expr_to_rust(e)),
                }
            })
            .collect();

        let mut states = Vec::new();
        let state_name_to_id: HashMap<String, usize> = ss
            .states
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.clone(), i))
            .collect();

        for (state_id, state) in ss.states.iter().enumerate() {
            let transitions = state
                .transitions
                .iter()
                .map(|t| {
                    let condition = t.condition.as_ref().map(|e| self.condition_to_rust(e));
                    let action = self.lower_block(&t.body);
                    let target_state = match &t.target {
                        TransitionTarget::State(name) => {
                            Some(*state_name_to_id.get(name).unwrap_or(&0))
                        }
                        TransitionTarget::Exit => None,
                    };
                    IRTransition {
                        condition,
                        action,
                        target_state,
                    }
                })
                .collect();

            states.push(IRState {
                name: state.name.clone(),
                id: state_id,
                entry: state.entry.as_ref().map(|b| self.lower_block(b)),
                transitions,
                exit: state.exit.as_ref().map(|b| self.lower_block(b)),
            });
        }

        Ok(IRStateSet {
            name: ss.name.clone(),
            id: ss_id,
            local_vars,
            states,
        })
    }

    fn lower_block(&self, block: &ast::Block) -> IRBlock {
        let mut code = String::new();
        for stmt in &block.stmts {
            code.push_str(&self.stmt_to_rust(stmt));
        }
        IRBlock { code }
    }

    // --- Expression → Rust code string ---

    fn condition_to_rust(&self, expr: &Expr) -> String {
        self.expr_to_rust(expr)
    }

    fn expr_to_rust(&self, expr: &Expr) -> String {
        match expr {
            Expr::IntLit(v, _) => v.to_string(),
            Expr::FloatLit(v, _) => format_float(*v),
            Expr::StringLit(s, _) => format!("\"{s}\""),
            Expr::Ident(name, _) => self.ident_to_rust(name),
            Expr::BinaryOp(lhs, op, rhs, _) => {
                let l = self.expr_to_rust(lhs);
                let r = self.expr_to_rust(rhs);
                let op_str = binop_to_rust(*op);
                format!("{l} {op_str} {r}")
            }
            Expr::UnaryOp(op, e, _) => {
                let e = self.expr_to_rust(e);
                let op_str = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "!",
                    UnaryOp::BitNot => "!",
                };
                format!("{op_str}{e}")
            }
            Expr::Call(name, args, _) => {
                let args_str: Vec<String> = args.iter().map(|a| self.expr_to_rust(a)).collect();
                self.call_to_rust(name, &args_str)
            }
            Expr::Assign(lhs, rhs, _) => {
                let l = self.expr_to_rust(lhs);
                let r = self.expr_to_rust(rhs);
                format!("{l} = {r}")
            }
            Expr::CompoundAssign(lhs, op, rhs, _) => {
                let l = self.expr_to_rust(lhs);
                let r = self.expr_to_rust(rhs);
                let op_str = binop_to_rust(*op);
                format!("{l} {op_str}= {r}")
            }
            Expr::Paren(e, _) => {
                let inner = self.expr_to_rust(e);
                format!("({inner})")
            }
            Expr::PostIncr(e, _) => {
                let e = self.expr_to_rust(e);
                format!("{{ {e} += 1; {e} - 1 }}")
            }
            Expr::PostDecr(e, _) => {
                let e = self.expr_to_rust(e);
                format!("{{ {e} -= 1; {e} + 1 }}")
            }
            Expr::PreIncr(e, _) => {
                let e = self.expr_to_rust(e);
                format!("{{ {e} += 1; {e} }}")
            }
            Expr::PreDecr(e, _) => {
                let e = self.expr_to_rust(e);
                format!("{{ {e} -= 1; {e} }}")
            }
            Expr::Ternary(cond, then_e, else_e, _) => {
                let c = self.expr_to_rust(cond);
                let t = self.expr_to_rust(then_e);
                let e = self.expr_to_rust(else_e);
                format!("if {c} {{ {t} }} else {{ {e} }}")
            }
            Expr::Field(e, field, _) => {
                let e = self.expr_to_rust(e);
                format!("{e}.{field}")
            }
            Expr::Index(e, idx, _) => {
                let e = self.expr_to_rust(e);
                let i = self.expr_to_rust(idx);
                format!("{e}[{i}]")
            }
            Expr::Cast(_, e, _) => self.expr_to_rust(e),
            Expr::ArrayInit(elements, _) => {
                let elems: Vec<String> = elements.iter().map(|e| self.expr_to_rust(e)).collect();
                format!("[{}]", elems.join(", "))
            }
        }
    }

    fn ident_to_rust(&self, name: &str) -> String {
        // If it's a channel-assigned variable, prefix with ctx.local_vars.
        if self.var_to_channel.contains_key(name) || self.var_types.contains_key(name) {
            format!("ctx.local_vars.{name}")
        } else {
            name.to_string()
        }
    }

    fn resolve_comp_type(&self, args: &[String], default: &str) -> String {
        if args.len() >= 2 {
            let comp_arg = args[1].trim();
            match comp_arg {
                "ASYNC" | "ctx.local_vars.ASYNC" => "CompType::Async".to_string(),
                "SYNC" | "ctx.local_vars.SYNC" => "CompType::Sync".to_string(),
                "DEFAULT" | "ctx.local_vars.DEFAULT" => "CompType::Default".to_string(),
                _ => default.to_string(),
            }
        } else {
            default.to_string()
        }
    }

    fn resolve_var_name<'a>(&'a self, args: &'a [String]) -> &'a str {
        args.first()
            .map(|a| a.strip_prefix("ctx.local_vars.").unwrap_or(a))
            .unwrap_or("")
    }

    fn call_to_rust(&self, name: &str, args: &[String]) -> String {
        match name {
            "delay" => format!("ctx.delay({})", args.join(", ")),
            "pvGet" => {
                let var_name = self.resolve_var_name(args);
                if let Some(&ch_id) = self.var_to_channel.get(var_name) {
                    let comp = self.resolve_comp_type(args, "CompType::Default");
                    format!("ctx.pv_get({ch_id}, {comp}).await")
                } else {
                    format!("/* pvGet({}) - unresolved */", args.join(", "))
                }
            }
            "pvPut" => {
                let var_name = self.resolve_var_name(args);
                if let Some(&ch_id) = self.var_to_channel.get(var_name) {
                    let comp = self.resolve_comp_type(args, "CompType::Default");
                    format!("ctx.pv_put({ch_id}, {comp}).await")
                } else {
                    format!("/* pvPut({}) - unresolved */", args.join(", "))
                }
            }
            "pvGetComplete" => {
                let var_name = self.resolve_var_name(args);
                if let Some(&ch_id) = self.var_to_channel.get(var_name) {
                    format!("ctx.pv_get_complete({ch_id}).await")
                } else {
                    format!("/* pvGetComplete({}) - unresolved */", args.join(", "))
                }
            }
            "pvPutComplete" => {
                let var_name = self.resolve_var_name(args);
                if let Some(&ch_id) = self.var_to_channel.get(var_name) {
                    format!("ctx.pv_put_complete({ch_id}).await")
                } else {
                    format!("/* pvPutComplete({}) - unresolved */", args.join(", "))
                }
            }
            "pvGetCancel" => {
                let var_name = self.resolve_var_name(args);
                if let Some(&ch_id) = self.var_to_channel.get(var_name) {
                    format!("ctx.pv_get_cancel({ch_id}).await")
                } else {
                    format!("/* pvGetCancel({}) - unresolved */", args.join(", "))
                }
            }
            "pvPutCancel" => {
                let var_name = self.resolve_var_name(args);
                if let Some(&ch_id) = self.var_to_channel.get(var_name) {
                    format!("ctx.pv_put_cancel({ch_id}).await")
                } else {
                    format!("/* pvPutCancel({}) - unresolved */", args.join(", "))
                }
            }
            "pvStatus" => {
                let var_name = self.resolve_var_name(args);
                if let Some(&ch_id) = self.var_to_channel.get(var_name) {
                    format!("ctx.pv_status({ch_id})")
                } else {
                    format!("/* pvStatus({}) - unresolved */", args.join(", "))
                }
            }
            "pvSeverity" => {
                let var_name = self.resolve_var_name(args);
                if let Some(&ch_id) = self.var_to_channel.get(var_name) {
                    format!("ctx.pv_severity({ch_id})")
                } else {
                    format!("/* pvSeverity({}) - unresolved */", args.join(", "))
                }
            }
            "pvMessage" => {
                let var_name = self.resolve_var_name(args);
                if let Some(&ch_id) = self.var_to_channel.get(var_name) {
                    format!("ctx.pv_message({ch_id})")
                } else {
                    format!("/* pvMessage({}) - unresolved */", args.join(", "))
                }
            }
            "pvAssignCount" => {
                format!("{}", self.channels.len())
            }
            "pvMonitorCount" => {
                let count = self.channels.iter().filter(|ch| ch.monitored).count();
                format!("{count}")
            }
            "efSet" => {
                let ef_name = self.resolve_var_name(args);
                if let Some(&ef_id) = self.ef_name_to_id.get(ef_name) {
                    format!("ctx.ef_set({ef_id})")
                } else {
                    format!("/* efSet({}) - unresolved */", args.join(", "))
                }
            }
            "efTest" => {
                let ef_name = self.resolve_var_name(args);
                if let Some(&ef_id) = self.ef_name_to_id.get(ef_name) {
                    format!("ctx.ef_test({ef_id})")
                } else {
                    format!("/* efTest({}) - unresolved */", args.join(", "))
                }
            }
            "efClear" => {
                let ef_name = self.resolve_var_name(args);
                if let Some(&ef_id) = self.ef_name_to_id.get(ef_name) {
                    format!("ctx.ef_clear({ef_id})")
                } else {
                    format!("/* efClear({}) - unresolved */", args.join(", "))
                }
            }
            "efTestAndClear" => {
                let ef_name = self.resolve_var_name(args);
                if let Some(&ef_id) = self.ef_name_to_id.get(ef_name) {
                    format!("ctx.ef_test_and_clear({ef_id})")
                } else {
                    format!("/* efTestAndClear({}) - unresolved */", args.join(", "))
                }
            }
            "pvConnected" => {
                let var_name = self.resolve_var_name(args);
                if let Some(&ch_id) = self.var_to_channel.get(var_name) {
                    format!("ctx.pv_connected({ch_id})")
                } else {
                    format!("/* pvConnected({}) - unresolved */", args.join(", "))
                }
            }
            "pvConnectCount" => "ctx.pv_connect_count()".to_string(),
            "pvChannelCount" => "ctx.pv_channel_count()".to_string(),
            _ => format!("{name}({})", args.join(", ")),
        }
    }

    fn stmt_to_rust(&self, stmt: &Stmt) -> String {
        match stmt {
            Stmt::Expr(e) => format!("{};\n", self.expr_to_rust(e)),
            Stmt::VarDecl(vd) => {
                let mut ir_type = self.convert_type(&vd.type_spec).unwrap();
                for dim_expr in vd.dimensions.iter().rev() {
                    let size = self.eval_const_expr(dim_expr);
                    ir_type = IRType::Array {
                        element: Box::new(ir_type),
                        size,
                    };
                }
                let init = vd
                    .init
                    .as_ref()
                    .map(|e| self.expr_to_rust(e))
                    .unwrap_or_else(|| ir_type.default_value());
                let rust_type = ir_type.rust_type();
                format!("let mut {}: {rust_type} = {init};\n", vd.name)
            }
            Stmt::If(cond, then_b, else_b) => {
                let c = self.expr_to_rust(cond);
                let t = self.block_to_rust(then_b);
                if let Some(else_block) = else_b {
                    let e = self.block_to_rust(else_block);
                    format!("if {c} {{\n{t}}} else {{\n{e}}}\n")
                } else {
                    format!("if {c} {{\n{t}}}\n")
                }
            }
            Stmt::While(cond, body) => {
                let c = self.expr_to_rust(cond);
                let b = self.block_to_rust(body);
                format!("while {c} {{\n{b}}}\n")
            }
            Stmt::For(init, cond, step, body) => {
                // Convert C-style for to Rust loop
                let mut code = String::new();
                if let Some(init) = init {
                    code.push_str(&format!("{};\n", self.expr_to_rust(init)));
                }
                let cond_str = cond
                    .as_ref()
                    .map(|c| self.expr_to_rust(c))
                    .unwrap_or_else(|| "true".to_string());
                let body_str = self.block_to_rust(body);
                let step_str = step
                    .as_ref()
                    .map(|s| format!("{};\n", self.expr_to_rust(s)))
                    .unwrap_or_default();
                code.push_str(&format!(
                    "while {cond_str} {{\n{body_str}{step_str}}}\n"
                ));
                code
            }
            Stmt::Break => "break;\n".to_string(),
            Stmt::Return(val) => match val {
                Some(e) => format!("return {};\n", self.expr_to_rust(e)),
                None => "return;\n".to_string(),
            },
            Stmt::Block(b) => {
                let inner = self.block_to_rust(b);
                format!("{{\n{inner}}}\n")
            }
            Stmt::EmbeddedCode(code) => format!("{code}\n"),
        }
    }

    fn block_to_rust(&self, block: &ast::Block) -> String {
        let mut code = String::new();
        for stmt in &block.stmts {
            code.push_str(&self.stmt_to_rust(stmt));
        }
        code
    }

    fn convert_type(&self, ts: &TypeSpec) -> CompileResult<IRType> {
        match ts {
            TypeSpec::Int => Ok(IRType::Int),
            TypeSpec::Short => Ok(IRType::Short),
            TypeSpec::Long => Ok(IRType::Long),
            TypeSpec::Float => Ok(IRType::Float),
            TypeSpec::Double => Ok(IRType::Double),
            TypeSpec::String => Ok(IRType::String),
            TypeSpec::Char => Ok(IRType::Char),
            TypeSpec::Unsigned(inner) => self.convert_type(inner), // simplified
        }
    }
}

fn binop_to_rust(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
    }
}

fn format_float(v: f64) -> String {
    let s = v.to_string();
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn analyze_str(input: &str) -> SeqIR {
        let tokens = Lexer::new(input).tokenize().unwrap();
        let ast = Parser::new(tokens).parse_program().unwrap();
        analyze(&ast).unwrap()
    }

    #[test]
    fn test_basic_analysis() {
        let ir = analyze_str(r#"
            program test
            option +s;
            double x;
            assign x to "PV:x";
            monitor x;
            evflag ef_x;
            sync x to ef_x;
            ss s1 {
                state init {
                    when (efTestAndClear(ef_x)) {
                        x += 1.0;
                        pvPut(x);
                    } state init
                    when (delay(10.0)) {
                    } exit
                }
            }
        "#);

        assert_eq!(ir.program_name, "test");
        assert!(ir.options.safe_mode);
        assert_eq!(ir.channels.len(), 1);
        assert_eq!(ir.channels[0].var_name, "x");
        assert_eq!(ir.channels[0].pv_name, "PV:x");
        assert!(ir.channels[0].monitored);
        assert_eq!(ir.channels[0].sync_ef, Some(0));
        assert_eq!(ir.event_flags.len(), 1);
        assert_eq!(ir.event_flags[0].synced_channels, vec![0]);
        assert_eq!(ir.state_sets.len(), 1);
        assert_eq!(ir.state_sets[0].states.len(), 1);
        assert_eq!(ir.state_sets[0].states[0].transitions.len(), 2);
    }

    #[test]
    fn test_builtin_translation() {
        let ir = analyze_str(r#"
            program test
            double v;
            assign v to "PV:v";
            monitor v;
            evflag ef_v;
            sync v to ef_v;
            ss s1 {
                state init {
                    when (delay(1.0)) {
                        pvGet(v);
                        pvPut(v);
                        efSet(ef_v);
                        efClear(ef_v);
                    } state init
                }
            }
        "#);

        let trans = &ir.state_sets[0].states[0].transitions[0];
        assert_eq!(trans.condition.as_deref(), Some("ctx.delay(1.0)"));
        assert!(trans.action.code.contains("ctx.pv_get(0, CompType::Default).await"));
        assert!(trans.action.code.contains("ctx.pv_put(0, CompType::Default).await"));
        assert!(trans.action.code.contains("ctx.ef_set(0)"));
        assert!(trans.action.code.contains("ctx.ef_clear(0)"));
    }

    #[test]
    fn test_async_builtins() {
        let ir = analyze_str(r#"
            program test
            double v;
            assign v to "PV:v";
            ss s1 {
                state init {
                    when (delay(1.0)) {
                        pvGet(v, ASYNC);
                        pvPut(v, SYNC);
                    } state wait
                    when (pvGetComplete(v)) {
                    } state done
                }
                state wait {
                    when (pvGetComplete(v)) {
                    } state done
                }
                state done {
                    when () { } exit
                }
            }
        "#);

        let trans = &ir.state_sets[0].states[0].transitions[0];
        assert!(trans.action.code.contains("ctx.pv_get(0, CompType::Async).await"));
        assert!(trans.action.code.contains("ctx.pv_put(0, CompType::Sync).await"));

        let trans2 = &ir.state_sets[0].states[0].transitions[1];
        assert_eq!(
            trans2.condition.as_deref(),
            Some("ctx.pv_get_complete(0).await")
        );
    }

    #[test]
    fn test_pv_status_builtins() {
        let ir = analyze_str(r#"
            program test
            double v;
            assign v to "PV:v";
            monitor v;
            ss s1 {
                state init {
                    when (delay(1.0)) {
                        pvGet(v);
                        pvStatus(v);
                        pvSeverity(v);
                        pvMessage(v);
                    } state init
                }
            }
        "#);

        let trans = &ir.state_sets[0].states[0].transitions[0];
        assert!(trans.action.code.contains("ctx.pv_status(0)"));
        assert!(trans.action.code.contains("ctx.pv_severity(0)"));
        assert!(trans.action.code.contains("ctx.pv_message(0)"));
    }

    #[test]
    fn test_pv_assign_monitor_count() {
        let ir = analyze_str(r#"
            program test
            double a;
            double b;
            assign a to "PV:a";
            assign b to "PV:b";
            monitor a;
            ss s1 {
                state init {
                    when (delay(1.0)) {
                        pvAssignCount();
                        pvMonitorCount();
                    } state init
                }
            }
        "#);

        let trans = &ir.state_sets[0].states[0].transitions[0];
        // 2 channels assigned
        assert!(trans.action.code.contains("2"));
        // 1 monitored
        assert!(trans.action.code.contains("1"));
    }

    #[test]
    fn test_multi_state_set() {
        let ir = analyze_str(r#"
            program test
            double counter;
            assign counter to "PV:counter";
            int light;
            assign light to "PV:light";
            ss counter_ss {
                state counting {
                    when (delay(1.0)) {
                        counter += 1.0;
                        pvPut(counter);
                    } state counting
                }
            }
            ss light_ss {
                state idle {
                    when (delay(15.0)) {
                    } exit
                }
            }
        "#);

        assert_eq!(ir.state_sets.len(), 2);
        assert_eq!(ir.channels.len(), 2);
        assert_eq!(ir.variables.len(), 2);
    }

    #[test]
    fn test_array_variable() {
        let ir = analyze_str(r#"
            program test
            double arr[5];
            ss s1 {
                state init {
                    when () { } exit
                }
            }
        "#);

        assert_eq!(ir.variables.len(), 1);
        assert!(matches!(
            &ir.variables[0].var_type,
            IRType::Array { element, size: 5 } if **element == IRType::Double
        ));
        assert_eq!(ir.variables[0].var_type.rust_type(), "[f64; 5]");
        assert_eq!(ir.variables[0].var_type.default_value(), "[0.0; 5]");
    }

    #[test]
    fn test_array_with_brace_init() {
        let ir = analyze_str(r#"
            program test
            int vals[3] = {10, 20, 30};
            ss s1 {
                state init {
                    when () { } exit
                }
            }
        "#);

        assert_eq!(ir.variables[0].init_value, Some("[10, 20, 30]".to_string()));
    }

    #[test]
    fn test_local_vars() {
        let ir = analyze_str(r#"
            program test
            ss s1 {
                int n = 0;
                double avg = 0.0;
                state init {
                    when () { } exit
                }
            }
        "#);

        assert_eq!(ir.state_sets[0].local_vars.len(), 2);
        assert_eq!(ir.state_sets[0].local_vars[0].name, "n");
        assert_eq!(ir.state_sets[0].local_vars[0].init_value, Some("0".to_string()));
    }
}
