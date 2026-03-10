use crate::ast::*;
use crate::error::{CompileError, CompileResult, Span};
use crate::lexer::{SpannedToken, Token};

pub struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<SpannedToken>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|t| t.span.clone())
            .unwrap_or(Span {
                offset: 0,
                line: 0,
                column: 0,
            })
    }

    fn peek(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .map(|t| &t.token)
            .unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> &Token {
        let t = &self.tokens[self.pos].token;
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, expected: &Token) -> CompileResult<()> {
        if self.peek() == expected {
            self.advance();
            Ok(())
        } else {
            Err(CompileError::syntax(
                self.span(),
                format!("expected {:?}, got {:?}", expected, self.peek()),
            ))
        }
    }

    fn expect_ident(&mut self) -> CompileResult<String> {
        match self.peek().clone() {
            Token::Ident(name) => {
                let name = name.clone();
                self.advance();
                Ok(name)
            }
            _ => Err(CompileError::syntax(
                self.span(),
                format!("expected identifier, got {:?}", self.peek()),
            )),
        }
    }

    fn expect_string(&mut self) -> CompileResult<String> {
        match self.peek().clone() {
            Token::StringLit(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            _ => Err(CompileError::syntax(
                self.span(),
                format!("expected string literal, got {:?}", self.peek()),
            )),
        }
    }

    // --- Top-level ---

    pub fn parse_program(&mut self) -> CompileResult<Program> {
        let span = self.span();
        self.expect(&Token::Program)?;
        let name = self.expect_ident()?;

        // Optional semicolon after program name
        if self.peek() == &Token::Semi {
            self.advance();
        }

        let mut options = Vec::new();
        let mut definitions = Vec::new();
        let mut entry = None;
        let mut state_sets = Vec::new();
        let mut exit = None;

        loop {
            match self.peek() {
                Token::Eof => break,
                Token::Option_ => {
                    let opt = self.parse_option()?;
                    options.push(opt.clone());
                    definitions.push(Definition::Option(opt));
                }
                Token::Int | Token::Short | Token::Long | Token::Float | Token::Double
                | Token::String_ | Token::Char | Token::Unsigned => {
                    definitions.push(Definition::VarDecl(self.parse_var_decl()?));
                }
                Token::Assign => {
                    definitions.push(Definition::Assign(self.parse_assign()?));
                }
                Token::Monitor => {
                    definitions.push(Definition::Monitor(self.parse_monitor()?));
                }
                Token::Sync => {
                    definitions.push(Definition::Sync(self.parse_sync()?));
                }
                Token::EvFlag => {
                    definitions.push(Definition::EvFlag(self.parse_evflag()?));
                }
                Token::Entry => {
                    self.advance();
                    entry = Some(self.parse_block()?);
                }
                Token::Exit => {
                    self.advance();
                    exit = Some(self.parse_block()?);
                }
                Token::Ss => {
                    state_sets.push(self.parse_state_set()?);
                }
                Token::EmbeddedLine(_) => {
                    if let Token::EmbeddedLine(code) = self.peek().clone() {
                        let code = code.clone();
                        self.advance();
                        definitions.push(Definition::EmbeddedCode(code));
                    }
                }
                _ => {
                    return Err(CompileError::syntax(
                        self.span(),
                        format!("unexpected token at top level: {:?}", self.peek()),
                    ));
                }
            }
        }

        Ok(Program {
            name,
            options,
            definitions,
            entry,
            state_sets,
            exit,
            span,
        })
    }

    fn parse_option(&mut self) -> CompileResult<ProgramOption> {
        self.expect(&Token::Option_)?;
        self.expect(&Token::Plus)?;

        let ident = self.expect_ident()?;
        self.expect(&Token::Semi)?;

        match ident.as_str() {
            "s" => Ok(ProgramOption::Safe),
            "r" => Ok(ProgramOption::Reentrant),
            "m" => Ok(ProgramOption::Main),
            _ => Err(CompileError::syntax(
                self.span(),
                format!("unknown option: +{ident}"),
            )),
        }
    }

    fn parse_type_spec(&mut self) -> CompileResult<TypeSpec> {
        let ts = match self.peek() {
            Token::Int => { self.advance(); TypeSpec::Int }
            Token::Short => { self.advance(); TypeSpec::Short }
            Token::Long => { self.advance(); TypeSpec::Long }
            Token::Float => { self.advance(); TypeSpec::Float }
            Token::Double => { self.advance(); TypeSpec::Double }
            Token::String_ => { self.advance(); TypeSpec::String }
            Token::Char => { self.advance(); TypeSpec::Char }
            Token::Unsigned => {
                self.advance();
                let inner = if matches!(
                    self.peek(),
                    Token::Int | Token::Short | Token::Long | Token::Char
                ) {
                    self.parse_type_spec()?
                } else {
                    TypeSpec::Int
                };
                TypeSpec::Unsigned(Box::new(inner))
            }
            _ => {
                return Err(CompileError::syntax(
                    self.span(),
                    format!("expected type, got {:?}", self.peek()),
                ));
            }
        };
        Ok(ts)
    }

    fn parse_var_decl(&mut self) -> CompileResult<VarDecl> {
        let span = self.span();
        let type_spec = self.parse_type_spec()?;
        let name = self.expect_ident()?;

        // Parse array dimensions: name[N][M]...
        let mut dimensions = Vec::new();
        while self.peek() == &Token::LBracket {
            self.advance();
            let dim = self.parse_expr()?;
            self.expect(&Token::RBracket)?;
            dimensions.push(dim);
        }

        let init = if self.peek() == &Token::Assign_ {
            self.advance();
            if self.peek() == &Token::LBrace {
                // Brace initializer: = {1, 2, 3}
                Some(self.parse_brace_init()?)
            } else {
                Some(self.parse_expr()?)
            }
        } else {
            None
        };

        self.expect(&Token::Semi)?;

        Ok(VarDecl {
            type_spec,
            name,
            dimensions,
            init,
            span,
        })
    }

    fn parse_brace_init(&mut self) -> CompileResult<Expr> {
        let span = self.span();
        self.expect(&Token::LBrace)?;
        let mut elements = Vec::new();
        if self.peek() != &Token::RBrace {
            elements.push(self.parse_expr()?);
            while self.peek() == &Token::Comma {
                self.advance();
                if self.peek() == &Token::RBrace {
                    break; // trailing comma
                }
                elements.push(self.parse_expr()?);
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Expr::ArrayInit(elements, span))
    }

    fn parse_assign(&mut self) -> CompileResult<Assign> {
        let span = self.span();
        self.expect(&Token::Assign)?;
        let var_name = self.expect_ident()?;

        let pv_name = if self.peek() == &Token::To {
            self.advance();
            Some(self.expect_string()?)
        } else {
            // assign var; (assigns to var name as PV name)
            None
        };

        self.expect(&Token::Semi)?;
        Ok(Assign {
            var_name,
            pv_name,
            span,
        })
    }

    fn parse_monitor(&mut self) -> CompileResult<Monitor> {
        let span = self.span();
        self.expect(&Token::Monitor)?;
        let var_name = self.expect_ident()?;
        self.expect(&Token::Semi)?;
        Ok(Monitor { var_name, span })
    }

    fn parse_sync(&mut self) -> CompileResult<Sync> {
        let span = self.span();
        self.expect(&Token::Sync)?;
        let var_name = self.expect_ident()?;

        // "sync var to ef_name" or "sync var ef_name"
        if self.peek() == &Token::To {
            self.advance();
        }

        let ef_name = self.expect_ident()?;
        self.expect(&Token::Semi)?;
        Ok(Sync {
            var_name,
            ef_name,
            span,
        })
    }

    fn parse_evflag(&mut self) -> CompileResult<EvFlagDecl> {
        let span = self.span();
        self.expect(&Token::EvFlag)?;
        let name = self.expect_ident()?;
        self.expect(&Token::Semi)?;
        Ok(EvFlagDecl { name, span })
    }

    // --- State sets ---

    fn parse_state_set(&mut self) -> CompileResult<StateSet> {
        let span = self.span();
        self.expect(&Token::Ss)?;
        let name = self.expect_ident()?;
        self.expect(&Token::LBrace)?;

        let mut local_vars = Vec::new();
        let mut states = Vec::new();

        loop {
            match self.peek() {
                Token::RBrace => {
                    self.advance();
                    break;
                }
                Token::State => {
                    states.push(self.parse_state()?);
                }
                Token::Int | Token::Short | Token::Long | Token::Float | Token::Double
                | Token::String_ | Token::Char | Token::Unsigned => {
                    local_vars.push(self.parse_var_decl()?);
                }
                _ => {
                    return Err(CompileError::syntax(
                        self.span(),
                        format!("expected state or variable in ss, got {:?}", self.peek()),
                    ));
                }
            }
        }

        Ok(StateSet {
            name,
            local_vars,
            states,
            span,
        })
    }

    fn parse_state(&mut self) -> CompileResult<State> {
        let span = self.span();
        self.expect(&Token::State)?;
        let name = self.expect_ident()?;
        self.expect(&Token::LBrace)?;

        let mut entry_block = None;
        let mut transitions = Vec::new();
        let mut exit_block = None;

        loop {
            match self.peek() {
                Token::RBrace => {
                    self.advance();
                    break;
                }
                Token::Entry => {
                    self.advance();
                    entry_block = Some(self.parse_block()?);
                }
                Token::Exit => {
                    self.advance();
                    exit_block = Some(self.parse_block()?);
                }
                Token::When => {
                    transitions.push(self.parse_transition()?);
                }
                _ => {
                    return Err(CompileError::syntax(
                        self.span(),
                        format!("expected when/entry/exit in state, got {:?}", self.peek()),
                    ));
                }
            }
        }

        Ok(State {
            name,
            entry: entry_block,
            transitions,
            exit: exit_block,
            span,
        })
    }

    fn parse_transition(&mut self) -> CompileResult<Transition> {
        let span = self.span();
        self.expect(&Token::When)?;
        self.expect(&Token::LParen)?;

        let condition = if self.peek() == &Token::RParen {
            None // empty condition = always true
        } else {
            Some(self.parse_expr()?)
        };

        self.expect(&Token::RParen)?;

        let body = self.parse_block()?;

        // Target: "state <name>" or "exit"
        let target = match self.peek() {
            Token::State => {
                self.advance();
                let target_name = self.expect_ident()?;
                TransitionTarget::State(target_name)
            }
            Token::Exit => {
                self.advance();
                TransitionTarget::Exit
            }
            _ => {
                return Err(CompileError::syntax(
                    self.span(),
                    format!("expected 'state' or 'exit' after when block, got {:?}", self.peek()),
                ));
            }
        };

        Ok(Transition {
            condition,
            body,
            target,
            span,
        })
    }

    // --- Blocks and statements ---

    fn parse_block(&mut self) -> CompileResult<Block> {
        let span = self.span();
        self.expect(&Token::LBrace)?;
        let mut stmts = Vec::new();

        loop {
            if self.peek() == &Token::RBrace {
                self.advance();
                break;
            }
            stmts.push(self.parse_stmt()?);
        }

        Ok(Block { stmts, span })
    }

    fn parse_stmt(&mut self) -> CompileResult<Stmt> {
        match self.peek() {
            Token::If => self.parse_if(),
            Token::While => self.parse_while(),
            Token::For => self.parse_for(),
            Token::Break => {
                self.advance();
                self.expect(&Token::Semi)?;
                Ok(Stmt::Break)
            }
            Token::Return => {
                self.advance();
                let val = if self.peek() != &Token::Semi {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                self.expect(&Token::Semi)?;
                Ok(Stmt::Return(val))
            }
            Token::LBrace => Ok(Stmt::Block(self.parse_block()?)),
            Token::Int | Token::Short | Token::Long | Token::Float | Token::Double
            | Token::String_ | Token::Char | Token::Unsigned => {
                Ok(Stmt::VarDecl(self.parse_var_decl()?))
            }
            Token::EmbeddedLine(_) => {
                if let Token::EmbeddedLine(code) = self.peek().clone() {
                    let code = code.clone();
                    self.advance();
                    Ok(Stmt::EmbeddedCode(code))
                } else {
                    unreachable!()
                }
            }
            _ => {
                let expr = self.parse_expr()?;
                self.expect(&Token::Semi)?;
                Ok(Stmt::Expr(expr))
            }
        }
    }

    fn parse_if(&mut self) -> CompileResult<Stmt> {
        self.expect(&Token::If)?;
        self.expect(&Token::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(&Token::RParen)?;

        let then_block = if self.peek() == &Token::LBrace {
            self.parse_block()?
        } else {
            let span = self.span();
            let stmt = self.parse_stmt()?;
            Block {
                stmts: vec![stmt],
                span,
            }
        };

        let else_block = if self.peek() == &Token::Else {
            self.advance();
            if self.peek() == &Token::LBrace {
                Some(self.parse_block()?)
            } else {
                let span = self.span();
                let stmt = self.parse_stmt()?;
                Some(Block {
                    stmts: vec![stmt],
                    span,
                })
            }
        } else {
            None
        };

        Ok(Stmt::If(cond, then_block, else_block))
    }

    fn parse_while(&mut self) -> CompileResult<Stmt> {
        self.expect(&Token::While)?;
        self.expect(&Token::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(&Token::RParen)?;
        let body = if self.peek() == &Token::LBrace {
            self.parse_block()?
        } else {
            let span = self.span();
            let stmt = self.parse_stmt()?;
            Block {
                stmts: vec![stmt],
                span,
            }
        };
        Ok(Stmt::While(cond, body))
    }

    fn parse_for(&mut self) -> CompileResult<Stmt> {
        self.expect(&Token::For)?;
        self.expect(&Token::LParen)?;
        let init = if self.peek() == &Token::Semi {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        self.expect(&Token::Semi)?;
        let cond = if self.peek() == &Token::Semi {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect(&Token::Semi)?;
        let step = if self.peek() == &Token::RParen {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        self.expect(&Token::RParen)?;
        let body = if self.peek() == &Token::LBrace {
            self.parse_block()?
        } else {
            let span = self.span();
            let stmt = self.parse_stmt()?;
            Block {
                stmts: vec![stmt],
                span,
            }
        };
        Ok(Stmt::For(init, cond, step, body))
    }

    // --- Expressions (precedence climbing) ---

    fn parse_expr(&mut self) -> CompileResult<Expr> {
        self.parse_assignment()
    }

    fn parse_assignment(&mut self) -> CompileResult<Expr> {
        let lhs = self.parse_ternary()?;

        match self.peek() {
            Token::Assign_ => {
                let span = self.span();
                self.advance();
                let rhs = self.parse_assignment()?;
                Ok(Expr::Assign(Box::new(lhs), Box::new(rhs), span))
            }
            Token::PlusEq => {
                let span = self.span();
                self.advance();
                let rhs = self.parse_assignment()?;
                Ok(Expr::CompoundAssign(
                    Box::new(lhs),
                    BinOp::Add,
                    Box::new(rhs),
                    span,
                ))
            }
            Token::MinusEq => {
                let span = self.span();
                self.advance();
                let rhs = self.parse_assignment()?;
                Ok(Expr::CompoundAssign(
                    Box::new(lhs),
                    BinOp::Sub,
                    Box::new(rhs),
                    span,
                ))
            }
            Token::StarEq => {
                let span = self.span();
                self.advance();
                let rhs = self.parse_assignment()?;
                Ok(Expr::CompoundAssign(
                    Box::new(lhs),
                    BinOp::Mul,
                    Box::new(rhs),
                    span,
                ))
            }
            Token::SlashEq => {
                let span = self.span();
                self.advance();
                let rhs = self.parse_assignment()?;
                Ok(Expr::CompoundAssign(
                    Box::new(lhs),
                    BinOp::Div,
                    Box::new(rhs),
                    span,
                ))
            }
            _ => Ok(lhs),
        }
    }

    fn parse_ternary(&mut self) -> CompileResult<Expr> {
        let cond = self.parse_or()?;
        if self.peek() == &Token::Question {
            let span = self.span();
            self.advance();
            let then_expr = self.parse_expr()?;
            self.expect(&Token::Colon)?;
            let else_expr = self.parse_ternary()?;
            Ok(Expr::Ternary(
                Box::new(cond),
                Box::new(then_expr),
                Box::new(else_expr),
                span,
            ))
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self) -> CompileResult<Expr> {
        let mut lhs = self.parse_and()?;
        while self.peek() == &Token::Or {
            let span = self.span();
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::BinaryOp(Box::new(lhs), BinOp::Or, Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> CompileResult<Expr> {
        let mut lhs = self.parse_bitor()?;
        while self.peek() == &Token::And {
            let span = self.span();
            self.advance();
            let rhs = self.parse_bitor()?;
            lhs = Expr::BinaryOp(Box::new(lhs), BinOp::And, Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_bitor(&mut self) -> CompileResult<Expr> {
        let mut lhs = self.parse_bitxor()?;
        while self.peek() == &Token::BitOr {
            let span = self.span();
            self.advance();
            let rhs = self.parse_bitxor()?;
            lhs = Expr::BinaryOp(Box::new(lhs), BinOp::BitOr, Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_bitxor(&mut self) -> CompileResult<Expr> {
        let mut lhs = self.parse_bitand()?;
        while self.peek() == &Token::BitXor {
            let span = self.span();
            self.advance();
            let rhs = self.parse_bitand()?;
            lhs = Expr::BinaryOp(Box::new(lhs), BinOp::BitXor, Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_bitand(&mut self) -> CompileResult<Expr> {
        let mut lhs = self.parse_equality()?;
        while self.peek() == &Token::BitAnd {
            let span = self.span();
            self.advance();
            let rhs = self.parse_equality()?;
            lhs = Expr::BinaryOp(Box::new(lhs), BinOp::BitAnd, Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_equality(&mut self) -> CompileResult<Expr> {
        let mut lhs = self.parse_comparison()?;
        loop {
            let op = match self.peek() {
                Token::Eq => BinOp::Eq,
                Token::Ne => BinOp::Ne,
                _ => break,
            };
            let span = self.span();
            self.advance();
            let rhs = self.parse_comparison()?;
            lhs = Expr::BinaryOp(Box::new(lhs), op, Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_comparison(&mut self) -> CompileResult<Expr> {
        let mut lhs = self.parse_shift()?;
        loop {
            let op = match self.peek() {
                Token::Lt => BinOp::Lt,
                Token::Le => BinOp::Le,
                Token::Gt => BinOp::Gt,
                Token::Ge => BinOp::Ge,
                _ => break,
            };
            let span = self.span();
            self.advance();
            let rhs = self.parse_shift()?;
            lhs = Expr::BinaryOp(Box::new(lhs), op, Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> CompileResult<Expr> {
        let mut lhs = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Token::Shl => BinOp::Shl,
                Token::Shr => BinOp::Shr,
                _ => break,
            };
            let span = self.span();
            self.advance();
            let rhs = self.parse_additive()?;
            lhs = Expr::BinaryOp(Box::new(lhs), op, Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> CompileResult<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            let span = self.span();
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::BinaryOp(Box::new(lhs), op, Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> CompileResult<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::Percent => BinOp::Mod,
                _ => break,
            };
            let span = self.span();
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = Expr::BinaryOp(Box::new(lhs), op, Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> CompileResult<Expr> {
        match self.peek() {
            Token::Minus => {
                let span = self.span();
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::UnaryOp(UnaryOp::Neg, Box::new(expr), span))
            }
            Token::Not => {
                let span = self.span();
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::UnaryOp(UnaryOp::Not, Box::new(expr), span))
            }
            Token::BitNot => {
                let span = self.span();
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::UnaryOp(UnaryOp::BitNot, Box::new(expr), span))
            }
            Token::PlusPlus => {
                let span = self.span();
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::PreIncr(Box::new(expr), span))
            }
            Token::MinusMinus => {
                let span = self.span();
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr::PreDecr(Box::new(expr), span))
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> CompileResult<Expr> {
        let mut expr = self.parse_primary()?;

        loop {
            match self.peek() {
                Token::PlusPlus => {
                    let span = self.span();
                    self.advance();
                    expr = Expr::PostIncr(Box::new(expr), span);
                }
                Token::MinusMinus => {
                    let span = self.span();
                    self.advance();
                    expr = Expr::PostDecr(Box::new(expr), span);
                }
                Token::LBracket => {
                    let span = self.span();
                    self.advance();
                    let index = self.parse_expr()?;
                    self.expect(&Token::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(index), span);
                }
                Token::Dot => {
                    let span = self.span();
                    self.advance();
                    let field = self.expect_ident()?;
                    expr = Expr::Field(Box::new(expr), field, span);
                }
                Token::Arrow => {
                    let span = self.span();
                    self.advance();
                    let field = self.expect_ident()?;
                    expr = Expr::Field(Box::new(expr), field, span);
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    fn parse_primary(&mut self) -> CompileResult<Expr> {
        match self.peek().clone() {
            Token::IntLit(v) => {
                let span = self.span();
                let v = v;
                self.advance();
                Ok(Expr::IntLit(v, span))
            }
            Token::FloatLit(v) => {
                let span = self.span();
                let v = v;
                self.advance();
                Ok(Expr::FloatLit(v, span))
            }
            Token::StringLit(s) => {
                let span = self.span();
                let s = s.clone();
                self.advance();
                Ok(Expr::StringLit(s, span))
            }
            Token::Ident(name) => {
                let span = self.span();
                let name = name.clone();
                self.advance();

                // Function call?
                if self.peek() == &Token::LParen {
                    self.advance();
                    let mut args = Vec::new();
                    if self.peek() != &Token::RParen {
                        args.push(self.parse_expr()?);
                        while self.peek() == &Token::Comma {
                            self.advance();
                            args.push(self.parse_expr()?);
                        }
                    }
                    self.expect(&Token::RParen)?;
                    Ok(Expr::Call(name, args, span))
                } else {
                    Ok(Expr::Ident(name, span))
                }
            }
            Token::LParen => {
                let span = self.span();
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(Expr::Paren(Box::new(expr), span))
            }
            _ => Err(CompileError::syntax(
                self.span(),
                format!("expected expression, got {:?}", self.peek()),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn parse(input: &str) -> Program {
        let tokens = Lexer::new(input).tokenize().unwrap();
        Parser::new(tokens).parse_program().unwrap()
    }

    #[test]
    fn test_minimal_program() {
        let p = parse("program test ss s1 { state init { when () { } exit } }");
        assert_eq!(p.name, "test");
        assert_eq!(p.state_sets.len(), 1);
        assert_eq!(p.state_sets[0].name, "s1");
        assert_eq!(p.state_sets[0].states.len(), 1);
        assert_eq!(p.state_sets[0].states[0].transitions.len(), 1);
    }

    #[test]
    fn test_var_assign_monitor() {
        let p = parse(r#"
            program test
            double x;
            assign x to "PV:x";
            monitor x;
            ss s1 { state init { when () { } exit } }
        "#);
        let defs = &p.definitions;
        assert!(matches!(defs[0], Definition::VarDecl(_)));
        assert!(matches!(defs[1], Definition::Assign(_)));
        assert!(matches!(defs[2], Definition::Monitor(_)));
    }

    #[test]
    fn test_evflag_sync() {
        let p = parse(r#"
            program test
            double x;
            assign x to "PV:x";
            monitor x;
            evflag ef_x;
            sync x to ef_x;
            ss s1 { state init { when () { } exit } }
        "#);
        let has_evflag = p
            .definitions
            .iter()
            .any(|d| matches!(d, Definition::EvFlag(_)));
        let has_sync = p
            .definitions
            .iter()
            .any(|d| matches!(d, Definition::Sync(_)));
        assert!(has_evflag);
        assert!(has_sync);
    }

    #[test]
    fn test_option_safe() {
        let p = parse(r#"
            program test
            option +s;
            ss s1 { state init { when () { } exit } }
        "#);
        assert_eq!(p.options.len(), 1);
        assert!(matches!(p.options[0], ProgramOption::Safe));
    }

    #[test]
    fn test_state_transition() {
        let p = parse(r#"
            program test
            ss s1 {
                state a {
                    when (x > 0) {
                        y = 1;
                    } state b
                }
                state b {
                    when () { } exit
                }
            }
        "#);
        let ss = &p.state_sets[0];
        assert_eq!(ss.states.len(), 2);
        let t = &ss.states[0].transitions[0];
        assert!(matches!(t.target, TransitionTarget::State(ref n) if n == "b"));
    }

    #[test]
    fn test_delay_and_pvput() {
        let p = parse(r#"
            program test
            double counter;
            assign counter to "PV:counter";
            ss s1 {
                state counting {
                    when (delay(1.0)) {
                        counter += 1.0;
                        pvPut(counter);
                    } state counting
                }
            }
        "#);
        let state = &p.state_sets[0].states[0];
        assert_eq!(state.transitions.len(), 1);
        let cond = state.transitions[0].condition.as_ref().unwrap();
        assert!(matches!(cond, Expr::Call(name, _, _) if name == "delay"));
    }

    #[test]
    fn test_entry_exit_blocks() {
        let p = parse(r#"
            program test
            entry { x = 1; }
            ss s1 { state init { when () { } exit } }
            exit { x = 0; }
        "#);
        assert!(p.entry.is_some());
        assert!(p.exit.is_some());
    }

    #[test]
    fn test_if_else_in_action() {
        let p = parse(r#"
            program test
            int x;
            ss s1 {
                state init {
                    when (x > 0) {
                        if (x > 5) {
                            y = 1;
                        } else {
                            y = 0;
                        }
                    } state init
                }
            }
        "#);
        let body = &p.state_sets[0].states[0].transitions[0].body;
        assert!(matches!(body.stmts[0], Stmt::If(_, _, Some(_))));
    }

    #[test]
    fn test_array_declaration() {
        let p = parse(r#"
            program test
            double arr[10];
            ss s1 { state init { when () { } exit } }
        "#);
        let vd = match &p.definitions[0] {
            Definition::VarDecl(vd) => vd,
            _ => panic!("expected VarDecl"),
        };
        assert_eq!(vd.name, "arr");
        assert_eq!(vd.dimensions.len(), 1);
        assert!(matches!(&vd.dimensions[0], Expr::IntLit(10, _)));
    }

    #[test]
    fn test_array_with_init() {
        let p = parse(r#"
            program test
            int arr[3] = {1, 2, 3};
            ss s1 { state init { when () { } exit } }
        "#);
        let vd = match &p.definitions[0] {
            Definition::VarDecl(vd) => vd,
            _ => panic!("expected VarDecl"),
        };
        assert_eq!(vd.name, "arr");
        assert!(matches!(&vd.init, Some(Expr::ArrayInit(elems, _)) if elems.len() == 3));
    }

    #[test]
    fn test_embedded_code_top_level() {
        let p = parse(r#"
            program test
            %% use std::io;
            ss s1 { state init { when () { } exit } }
        "#);
        let has_embedded = p.definitions.iter().any(|d| matches!(d, Definition::EmbeddedCode(_)));
        assert!(has_embedded);
    }

    #[test]
    fn test_safe_monitor_program() {
        // This mirrors a simplified safeMonitor.st
        let p = parse(r#"
            program safeMonitorTest
            option +s;
            double cnt = 1.0;
            assign cnt to "cnt";
            monitor cnt;
            evflag ef_cnt;
            sync cnt to ef_cnt;
            ss read {
                int n = 1;
                state react {
                    when (n > 10) {
                    } exit
                    when (efTestAndClear(ef_cnt)) {
                        n++;
                    } state react
                }
            }
            ss write {
                state send {
                    when (delay(0.04)) {
                        cnt += 1.0;
                        pvPut(cnt);
                    } state send
                }
            }
        "#);
        assert_eq!(p.name, "safeMonitorTest");
        assert_eq!(p.state_sets.len(), 2);
        assert_eq!(p.state_sets[0].name, "read");
        assert_eq!(p.state_sets[0].local_vars.len(), 1);
        assert_eq!(p.state_sets[1].name, "write");
    }
}
