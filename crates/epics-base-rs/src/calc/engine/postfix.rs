use super::error::CalcError;
use super::opcodes::{ControlOp, CoreOp, Opcode};
use super::token::{ConstName, FuncName, Token};
use super::{CompiledExpr, ExprKind};

// Operator precedence levels.
//
// NOTE: these numeric levels follow the synApps `sCalcPostfix.c` table, NOT
// epics-base `postfix.c` (which uses priorities 0-8 — see postfix.c:147-180).
// Only the *relative* ordering matters for the compiled output, and it is
// preserved for the epics-base operator subset:
//   || < && < cmp < +/- < */% < power < unary/functions
//
//  2: ||, |, OR, XOR
//  3: &&, &, AND, >>, <<
//  4: >?, <?                          (synApps-only)
//  5: ==, !=, <, <=, >, >=, #
//  6: +, -
//  7: *, /, %
//  8: ^, **
//  9/10: unary operators, functions (in_stack=9, in_coming=10)

#[derive(Debug, Clone)]
enum StackEntry {
    Op {
        token: Token,
        in_stack_pri: u8,
    },
    /// C's `(` element (`sCalcPostfix.c:102`): a `UNARY_OPERATOR` with
    /// `in_stack_pri` 0 and code `NOT_GENERATED`. A barrier that emits nothing.
    LParen,
    /// C's `[` and `{` elements (`sCalcPostfix.c:215-216`,
    /// `aCalcPostfix.c:212-213`). They are NOT parens: they are
    /// `BINARY_OPERATOR`s whose code IS generated — `CLOSE_BRACKET` /
    /// `CLOSE_CURLY` emit it (`sCalcPostfix.c:686`, `:715`) after popping down to
    /// this entry.
    ///
    /// Two numbers make them what they are. `in_stack_pri` 0 makes them a barrier
    /// like `(`, so nothing pops them off. `in_coming_pri` **11** is the highest
    /// value in any of the three tables — above a function's 9 — which is why a
    /// `[` or `{` arriving right after a `)` does NOT pop the function still
    /// sitting below it, and the function is therefore applied to the SUBRANGE /
    /// REPLACE result rather than to its own argument (R13-2).
    ///
    /// This is a stack entry of its own rather than a second `LParen` because the
    /// two are barriers for DIFFERENT things: C's `SEPARATOR` (`:614-615`) stops
    /// at `(`, `[` and `{` alike, but only `CLOSE_PAREN` (`:652-656`) hands the
    /// argument count to a vararg function below it. Sharing one variant let a `,` inside
    /// `MAX(4,7)[0,0]`'s bracket bump `MAX`'s argument count.
    Subrange {
        /// `{` (sCalc `REPLACE`, aCalc `SUBRANGE_IP`) rather than `[` (`SUBRANGE`
        /// in both).
        in_place: bool,
        /// C accumulates the effect ON this entry: `-1` from the table, then `-1`
        /// more per `,` (`SEPARATOR`, `:630`). `CLOSE_BRACKET` adds the total
        /// (`:687`). So `AA[i,j]` is -2 — subject and two arguments in, one out.
        runtime_effect: i32,
    },
    VarargFunc {
        func: FuncName,
        in_stack_pri: u8,
        nargs: u8,
    },
    CondEnd,
    Store(StoreTarget),
    /// C's `UNTIL_END` element (`sCalcPostfix.c:779-783`): reading `UNTIL` PUSHES
    /// one of these onto the OPERATOR stack, carrying the in-stack priority of
    /// the `UNTIL` element itself (0) and an explicit `runtime_effect = 0`. It is
    /// therefore emitted by the ordinary flush rules and by nothing else — a `)`
    /// or an inner `;` stops at the `(` above it, and only the end of the
    /// enclosing statement (or of the expression) reaches it.
    ///
    /// That is the whole loop-closing rule, and it is why `UNTIL(a:=a+1;a>3)`
    /// puts `UNTIL_END` AFTER the condition: the `;` inside the parentheses
    /// cannot see this entry. The port used to keep a private `until_stack` and
    /// close the loop at the first `;` it met, which is the `;` between the body
    /// and the condition — so the condition was compiled outside the loop and
    /// `UNTIL_END` peeked an empty stack.
    UntilEnd {
        /// Where the paired `Until` placeholder sits in `output`; it is patched
        /// with this entry's own position when the entry is flushed.
        until_pc: usize,
    },
}

/// What a `:=` on the operator stack will store into — C's four STORE codes,
/// and the whole of what `STORE_OPERATOR` can produce.
///
/// C reaches them by TWO routes and both are here, which is the point of the
/// enum: `A:=`/`AA:=` RETRACT the fetch already written to the postfix and turn
/// it into a `STORE_x` (`sCalcPostfix.c:544-557`), while `@n:=`/`@@n:=` rewrite
/// the pending `A_FETCH`/`A_SFETCH` operator IN PLACE on the stack (`:516-529`)
/// — the index expression it was going to consume stays in the postfix, and the
/// store pops it at run time. Same element, same runtime_effect -1, two shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreTarget {
    /// `STORE_A`..`STORE_P` — a named scalar arg.
    Var(u8),
    /// `STORE_AA`..`STORE_LL` — a named doubled-letter arg (a STRING in sCalc,
    /// an ARRAY in aCalc, a second numeric bank in base).
    DoubleVar(u8),
    /// `A_STORE` — `@n := v`, the scalar arg `n` indexes. sCalc and aCalc.
    Dyn,
    /// `A_SSTORE` (sCalc) / `A_ASTORE` (aCalc) — `@@n := v`, the doubled-letter
    /// arg `n` indexes. Which of the two it compiles to is the engine's business
    /// (`flush_stack_entry`), as with `[` and `{`.
    DynDouble,
}

impl StackEntry {
    fn in_stack_pri(&self) -> u8 {
        match self {
            StackEntry::Op { in_stack_pri, .. } => *in_stack_pri,
            StackEntry::LParen => 0,
            // `[` / `{`: in_stack_pri 0, like `(` — a barrier nothing pops.
            StackEntry::Subrange { .. } => 0,
            StackEntry::VarargFunc { in_stack_pri, .. } => *in_stack_pri,
            StackEntry::CondEnd => 0,
            StackEntry::Store(_) => 1,
            // C: `{"UNTIL", 0, 10, 0, UNTIL_OPERATOR, UNTIL}` — in_stack_pri 0,
            // so no incoming operator ever pops it off the stack.
            StackEntry::UntilEnd { .. } => 0,
        }
    }
}

fn binary_op(token: &Token) -> Option<(u8, u8)> {
    match token {
        Token::OrOr | Token::BitOr | Token::BitXor => Some((2, 2)),
        Token::AndAnd | Token::BitAnd | Token::Shr | Token::ShrLogical | Token::Shl => Some((3, 3)),
        Token::MaxOp | Token::MinOp => Some((4, 4)),
        Token::Eq | Token::Ne | Token::Lt | Token::Le | Token::Gt | Token::Ge => Some((5, 5)),
        Token::Plus | Token::Minus => Some((6, 6)),
        Token::Star | Token::Slash | Token::Percent => Some((7, 7)),
        Token::Caret | Token::DoubleStar => Some((8, 8)),
        _ => None,
    }
}

fn token_to_binary_opcode(token: &Token) -> Opcode {
    let core = match token {
        Token::Plus => CoreOp::Add,
        Token::Minus => CoreOp::Sub,
        Token::Star => CoreOp::Mul,
        Token::Slash => CoreOp::Div,
        Token::Percent => CoreOp::Mod,
        Token::Caret | Token::DoubleStar => CoreOp::Power,
        Token::Eq => CoreOp::Eq,
        Token::Ne => CoreOp::Ne,
        Token::Lt => CoreOp::Lt,
        Token::Le => CoreOp::Le,
        Token::Gt => CoreOp::Gt,
        Token::Ge => CoreOp::Ge,
        Token::AndAnd => CoreOp::And,
        Token::OrOr => CoreOp::Or,
        Token::BitAnd => CoreOp::BitAnd,
        Token::BitOr => CoreOp::BitOr,
        Token::BitXor => CoreOp::BitXor,
        Token::Shl => CoreOp::Shl,
        Token::Shr => CoreOp::Shr,
        Token::ShrLogical => CoreOp::ShrLogical,
        Token::MaxOp => CoreOp::MaxVal,
        Token::MinOp => CoreOp::MinVal,
        Token::PipeMinus => {
            return Opcode::String(super::opcodes::StringOp::SubLast);
        }
        _ => unreachable!(),
    };
    Opcode::Core(core)
}

fn is_vararg(func: &FuncName) -> bool {
    matches!(
        func,
        FuncName::Min
            | FuncName::Max
            | FuncName::Finite
            | FuncName::IsNan
            // aCalc's two VARARG_OPERATORs (`aCalcPostfix.c:140-141`). Their trailing
            // arguments NAME the scalar arguments the fitted coefficients are stored
            // into, so their count is genuinely variable: `FITQ(AA)` fits and stores
            // nothing, `FITQ(AA,C,D,E)` fits and stores all three coefficients.
            | FuncName::FitQ
            | FuncName::FitMQ
    )
}

/// Number of operands consumed by a *non-vararg* function.
///
/// Every function emits exactly one result, so its net runtime-stack
/// effect is `1 - func_arity(f)`. Vararg functions (`MIN`, `MAX`,
/// `FINITE`, `ISNAN`) never reach this path — they are tracked through
/// `StackEntry::VarargFunc` with an explicit `nargs` count.
///
/// The arity values are verified against the opcode implementations:
///   - core 1-arg math: `numeric.rs` (`Abs`..`Nint`, `IsInf`, `BitNot`)
///   - core 2-arg math: `numeric.rs` (`Atan2`, `Fmod`)
///   - string ops: `string.rs` (`ToString`..`Xor8Append`, `Printf`,
///     `Sscanf`)
///   - array ops: `array.rs` (`ConstIndex`..`FitMQ`)
fn func_arity(func: &FuncName) -> u8 {
    match func {
        // 0-arg: produce a value from ambient state, consume nothing.
        //   ARndm  -> ArrayOp::ArrayRandom  (array.rs: pushes only)
        //   Ix     -> ArrayOp::ConstIndex   (array.rs: pushes only)
        FuncName::ARndm | FuncName::Ix => 0,

        // 2-arg core math (numeric.rs: Atan2/Fmod pop2).
        FuncName::Atan2 | FuncName::Fmod => 2,

        // 2-arg string ops (string.rs: Printf/Sscanf/BinRead/BinWrite pop2).
        // C's element table settles this: `runtime_effect` is -1 for PRINTF
        // ($P), SSCANF ($S), BIN_READ ($R, READ) and BIN_WRITE ($W, WRITE)
        // alike (sCalcPostfix.c:173-195) — one net operand consumed, i.e. two
        // popped and one pushed. Only elements with `runtime_effect` 0 are
        // 1-in-1-out.
        FuncName::Printf | FuncName::Sscanf | FuncName::BinRead | FuncName::BinWrite => 2,

        // 2-arg array ops.
        //   NSmoo  -> ArrayOp::NSmooth   (pop n, pop array)
        //   NDeriv -> ArrayOp::NDeriv    (pop n, pop array)
        //   Cat    -> ArrayOp::Cat       (pop b, pop a)
        //   FitMPoly -> ArrayOp::FitMPoly (pop mask, pop y)
        //
        // FITMPOLY is C's only UNARY_OPERATOR with `runtime_effect = -1`
        // (`aCalcPostfix.c:143`) — two operands in, one out. It takes NO x: like
        // FITPOLY it fits against the ELEMENT INDEX (`aCalcPerform.c:1017-1018`,
        // `ps2->a[i] = i`), and its second operand is the MASK.
        FuncName::NSmoo | FuncName::NDeriv | FuncName::Cat | FuncName::FitMPoly => 2,

        // FITPOLY is a plain UNARY_OPERATOR, `runtime_effect = 0`
        // (`aCalcPostfix.c:142`): `FITPOLY(y)`, one operand in, one out. The port
        // used to demand an x array as well, which no aCalc expression supplies.
        FuncName::FitPoly => 1,

        // Everything else is a 1-arg function: one operand in, one out.
        //   core math: Abs, Sqrt, Sqr, Exp, Log10, LogE, Ln, Sin,
        //     Cos, Tan, Asin, Acos, Atan, Sinh, Cosh, Tanh, Ceil, Floor,
        //     Nint, Int, IsInf, Not
        //   string 1-arg: Dbl, Str, Len, Byte, TrEsc, Esc, Crc16, ModBus,
        //     Lrc, AModBus, Xor8, AddXor8
        //   array 1-arg: Avg, Std, FwhmFunc, Sum, AMax, AMin, IxMax,
        //     IxMin, IxZ, IxNz, Arr, AToD, Smoo, Deriv, Cum
        // Vararg funcs (Min, Max, Finite, IsNan) are unreachable here.
        _ => 1,
    }
}

/// True when a function takes no operands and therefore behaves like an
/// operand itself (the next token must be an operator, not an operand).
fn is_nullary_func(func: &FuncName) -> bool {
    func_arity(func) == 0
}

fn func_to_opcode(func: &FuncName, nargs: u8) -> Opcode {
    let core = match func {
        FuncName::Abs => CoreOp::Abs,
        FuncName::Sqrt | FuncName::Sqr => CoreOp::Sqrt,
        FuncName::Exp => CoreOp::Exp,
        FuncName::Log10 => CoreOp::Log10,
        FuncName::LogE | FuncName::Ln => CoreOp::LogE,
        FuncName::Sin => CoreOp::Sin,
        FuncName::Cos => CoreOp::Cos,
        FuncName::Tan => CoreOp::Tan,
        FuncName::Asin => CoreOp::Asin,
        FuncName::Acos => CoreOp::Acos,
        FuncName::Atan => CoreOp::Atan,
        FuncName::Atan2 => CoreOp::Atan2,
        FuncName::Fmod => CoreOp::Fmod,
        FuncName::Sinh => CoreOp::Sinh,
        FuncName::Cosh => CoreOp::Cosh,
        FuncName::Tanh => CoreOp::Tanh,
        FuncName::Ceil => CoreOp::Ceil,
        FuncName::Floor => CoreOp::Floor,
        FuncName::Nint | FuncName::Int => CoreOp::Nint,
        FuncName::IsNan => CoreOp::IsNan(nargs),
        FuncName::IsInf => CoreOp::IsInf,
        FuncName::Finite => CoreOp::Finite(nargs),
        FuncName::Max => CoreOp::Max(nargs),
        FuncName::Min => CoreOp::Min(nargs),
        FuncName::Not => CoreOp::BitNot,
        FuncName::Dbl => return Opcode::String(super::opcodes::StringOp::ToDouble),
        FuncName::Str => return Opcode::String(super::opcodes::StringOp::ToString),
        FuncName::Len => return Opcode::String(super::opcodes::StringOp::Len),
        FuncName::ANeg => return Opcode::Array(super::opcodes::ArrayOp::ANeg),
        FuncName::APos => return Opcode::Array(super::opcodes::ArrayOp::APos),
        FuncName::DynFetch => return Opcode::Array(super::opcodes::ArrayOp::DynFetch),
        FuncName::DynAFetch => return Opcode::Array(super::opcodes::ArrayOp::DynAFetch),
        FuncName::SDynFetch => return Opcode::String(super::opcodes::StringOp::DynFetch),
        FuncName::SDynSFetch => return Opcode::String(super::opcodes::StringOp::DynSFetch),
        FuncName::ALenNoop => return Opcode::Array(super::opcodes::ArrayOp::LenNoop),
        FuncName::Byte => return Opcode::String(super::opcodes::StringOp::Byte),
        FuncName::TrEsc => return Opcode::String(super::opcodes::StringOp::TrEsc),
        FuncName::Esc => return Opcode::String(super::opcodes::StringOp::Esc),
        FuncName::Printf => return Opcode::String(super::opcodes::StringOp::Printf),
        FuncName::Sscanf => return Opcode::String(super::opcodes::StringOp::Sscanf),
        FuncName::BinRead => return Opcode::String(super::opcodes::StringOp::BinRead),
        FuncName::BinWrite => return Opcode::String(super::opcodes::StringOp::BinWrite),
        FuncName::Crc16 => return Opcode::String(super::opcodes::StringOp::Crc16),
        FuncName::ModBus => return Opcode::String(super::opcodes::StringOp::Crc16Append),
        FuncName::Lrc => return Opcode::String(super::opcodes::StringOp::Lrc),
        FuncName::AModBus => return Opcode::String(super::opcodes::StringOp::LrcAppend),
        FuncName::Xor8 => return Opcode::String(super::opcodes::StringOp::Xor8),
        FuncName::AddXor8 => return Opcode::String(super::opcodes::StringOp::Xor8Append),
        FuncName::Avg => return Opcode::Array(super::opcodes::ArrayOp::Average),
        FuncName::Std => return Opcode::Array(super::opcodes::ArrayOp::StdDev),
        FuncName::FwhmFunc => return Opcode::Array(super::opcodes::ArrayOp::Fwhm),
        FuncName::Sum => return Opcode::Array(super::opcodes::ArrayOp::ArraySum),
        FuncName::AMax => return Opcode::Array(super::opcodes::ArrayOp::ArrayMax),
        FuncName::AMin => return Opcode::Array(super::opcodes::ArrayOp::ArrayMin),
        FuncName::IxMax => return Opcode::Array(super::opcodes::ArrayOp::IndexMax),
        FuncName::IxMin => return Opcode::Array(super::opcodes::ArrayOp::IndexMin),
        FuncName::IxZ => return Opcode::Array(super::opcodes::ArrayOp::IndexZero),
        FuncName::IxNz => return Opcode::Array(super::opcodes::ArrayOp::IndexNonZero),
        FuncName::Arr => return Opcode::Array(super::opcodes::ArrayOp::ToArray),
        FuncName::Ix => return Opcode::Array(super::opcodes::ArrayOp::ConstIndex),
        FuncName::AToD => return Opcode::Array(super::opcodes::ArrayOp::ToDouble),
        FuncName::Smoo => return Opcode::Array(super::opcodes::ArrayOp::Smooth),
        FuncName::NSmoo => return Opcode::Array(super::opcodes::ArrayOp::NSmooth),
        FuncName::Deriv => return Opcode::Array(super::opcodes::ArrayOp::Deriv),
        FuncName::NDeriv => return Opcode::Array(super::opcodes::ArrayOp::NDeriv),
        FuncName::FitPoly => return Opcode::Array(super::opcodes::ArrayOp::FitPoly),
        FuncName::FitMPoly => return Opcode::Array(super::opcodes::ArrayOp::FitMPoly),
        FuncName::FitQ => return Opcode::Array(super::opcodes::ArrayOp::FitQ(nargs)),
        FuncName::FitMQ => return Opcode::Array(super::opcodes::ArrayOp::FitMQ(nargs)),
        FuncName::Cum => return Opcode::Array(super::opcodes::ArrayOp::Cum),
        FuncName::Cat => return Opcode::Array(super::opcodes::ArrayOp::Cat),
        FuncName::ARndm => return Opcode::Array(super::opcodes::ArrayOp::ArrayRandom),
    };
    Opcode::Core(core)
}

/// Emit one operator-stack entry to the output — C's
/// `*pout++ = pstacktop->code`, the single move every pop site performs.
///
/// `kind` is needed because `[` and `{` carry a per-engine code: `[` is
/// `SUBRANGE` in both synApps tables but `{` is `REPLACE` in sCalc
/// (`sCalcPostfix.c:216`) and `SUBRANGE_IP` in aCalc (`aCalcPostfix.c:213`).
fn flush_stack_entry(entry: &StackEntry, output: &mut Vec<Opcode>, kind: ExprKind) {
    match entry {
        StackEntry::Subrange { in_place, .. } => {
            output.push(match (kind, in_place) {
                (ExprKind::Array, false) => Opcode::Array(super::opcodes::ArrayOp::ArraySubrange),
                (ExprKind::Array, true) => {
                    Opcode::Array(super::opcodes::ArrayOp::ArraySubrangeInPlace)
                }
                (_, false) => Opcode::String(super::opcodes::StringOp::Subrange),
                (_, true) => Opcode::String(super::opcodes::StringOp::Replace),
            });
        }
        StackEntry::Op {
            token: Token::Minus,
            in_stack_pri: 9,
            ..
        } => {
            output.push(Opcode::Core(CoreOp::Neg));
        }
        StackEntry::Op {
            token: Token::Bang, ..
        } => {
            output.push(Opcode::Core(CoreOp::Not));
        }
        StackEntry::Op {
            token: Token::Tilde,
            ..
        } => {
            output.push(Opcode::Core(CoreOp::BitNot));
        }
        StackEntry::Op {
            token: Token::Func(f),
            ..
        } => {
            output.push(func_to_opcode(f, 1));
        }
        StackEntry::Op { token, .. } => {
            output.push(token_to_binary_opcode(token));
        }
        StackEntry::VarargFunc { func, nargs, .. } => {
            output.push(func_to_opcode(func, *nargs));
        }
        StackEntry::CondEnd => {
            output.push(Opcode::Core(CoreOp::CondEnd));
        }
        StackEntry::UntilEnd { until_pc } => {
            // The two halves of the loop learn each other's position here, the
            // one moment both are known.
            let end_pc = output.len();
            output.push(Opcode::Control(ControlOp::UntilEnd(*until_pc)));
            output[*until_pc] = Opcode::Control(ControlOp::Until(end_pc));
        }
        StackEntry::Store(target) => {
            use super::opcodes::{ArrayOp, StringOp};
            output.push(match target {
                StoreTarget::Var(idx) => Opcode::Core(CoreOp::StoreVar(*idx)),
                StoreTarget::DoubleVar(idx) => Opcode::Core(CoreOp::StoreDoubleVar(*idx)),
                // `A_STORE` is spelled once per engine, like every other opcode
                // the two synApps evaluators share.
                StoreTarget::Dyn => match kind {
                    ExprKind::Array => Opcode::Array(ArrayOp::DynStore),
                    _ => Opcode::String(StringOp::DynStore),
                },
                // `@@n:=` is A_SSTORE in sCalc (a string arg) and A_ASTORE in
                // aCalc (an array arg) — the same split as `{`.
                StoreTarget::DynDouble => match kind {
                    ExprKind::Array => Opcode::Array(ArrayOp::DynAStore),
                    _ => Opcode::String(StringOp::DynSStore),
                },
            });
        }
        StackEntry::LParen => {}
    }
}

/// Compile a token stream for ONE engine.
///
/// `tokens` must come from `token::tokenize(_, kind)`: that is where the C
/// compiler's `ELEMENT` table is applied, so every symbol here is one this
/// engine has. The resulting `CompiledExpr` carries only opcodes its evaluator
/// can execute.
/// Compile a token stream. A source with no tokens is NOT special-cased: C only
/// short-circuits the literal empty string (`*psrc == '\0'`), and an expression
/// that merely *lexes* to nothing — `"   "` — walks the normal path and comes
/// out `CALC_ERR_INCOMPLETE`, because `operand_needed` is still set at the end.
/// Compiled C, all three engines: `postfix("   ")` = -1, error 8. The empty
/// SOURCE is handled by each engine's entry point in `calc::mod`, where C puts
/// it.
pub fn compile(tokens: &[Token], kind: ExprKind) -> Result<CompiledExpr, CalcError> {
    let mut output: Vec<Opcode> = Vec::new();
    let mut stack: Vec<StackEntry> = Vec::new();
    let mut operand_needed = true;
    let mut runtime_depth: i32 = 0;
    let mut cond_count: i32 = 0;
    let mut pos = 0;
    // C `*ppostfix = USES_STRING` (`sCalcPostfix.c:447-471`) — latched as each
    // element is LOOKED UP, which is the only moment the retract-and-rewrite of
    // `:=` has not yet erased the evidence. sCalc's marker only; the numeric and
    // array compilers have none.
    let mut uses_string = false;
    // The ceiling this flavour's `*Perform` can actually hold, from its own
    // element table (C `CALCPERFORM_STACK` 80 / `SCALC_STACKSIZE` 30 /
    // `ACALC_STACKSIZE` 20). One shared compiler, three C limits: a literal here
    // was right for sCalc and wrong in OPPOSITE directions for the other two.
    let stack_size = super::token::runtime_stack_size(kind);
    // No `bracket_depth` / `brace_depth` counters: the open `[` / `{` IS a
    // `StackEntry::Subrange` on the operator stack, exactly as in C, so the stack
    // already answers "is one open, and which". A side counter was a second
    // record of the same fact and could disagree with the stack.

    while pos < tokens.len() {
        let token = &tokens[pos];
        pos += 1;

        if kind == ExprKind::String && token_is_string_element(token) {
            uses_string = true;
        }

        if operand_needed {
            match token {
                Token::Number(v) => {
                    output.push(Opcode::Core(CoreOp::PushConst(*v)));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::Var(idx) => {
                    output.push(Opcode::Core(CoreOp::PushVar(*idx)));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::DoubleVar(idx) => {
                    output.push(Opcode::Core(CoreOp::PushDoubleVar(*idx)));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::FetchVal => {
                    output.push(Opcode::Core(CoreOp::FetchVal));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::FetchAval => {
                    output.push(Opcode::Array(super::opcodes::ArrayOp::FetchAval));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::FetchSval => {
                    output.push(Opcode::Core(CoreOp::FetchSval));
                    runtime_depth += 1;
                    operand_needed = false;
                    // C sCalcPostfix.c:452 lists FETCH_SVAL among the opcodes
                    // that mark the postfix USES_STRING.
                }
                Token::Rndm => {
                    output.push(Opcode::Core(CoreOp::Random));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::Nrndm => {
                    output.push(Opcode::Core(CoreOp::NormalRandom));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::Const(c) => {
                    match c {
                        ConstName::Pi => output.push(Opcode::Core(CoreOp::Pi)),
                        ConstName::D2R => output.push(Opcode::Core(CoreOp::D2R)),
                        ConstName::R2D => output.push(Opcode::Core(CoreOp::R2D)),
                        ConstName::S2R => output.push(Opcode::Core(CoreOp::S2R)),
                        ConstName::R2S => output.push(Opcode::Core(CoreOp::R2S)),
                    }
                    runtime_depth += 1;
                    operand_needed = false;
                }

                Token::StringLiteral(s) => {
                    output.push(Opcode::String(super::opcodes::StringOp::PushString(
                        s.clone(),
                    )));
                    runtime_depth += 1;
                    operand_needed = false;
                }

                // Unary operators
                Token::Minus => {
                    pop_higher_or_equal(&mut stack, 10, &mut output, &mut runtime_depth, kind);
                    stack.push(StackEntry::Op {
                        token: Token::Minus,
                        in_stack_pri: 9,
                    });
                }
                Token::Bang => {
                    pop_higher_or_equal(&mut stack, 10, &mut output, &mut runtime_depth, kind);
                    stack.push(StackEntry::Op {
                        token: Token::Bang,
                        in_stack_pri: 9,
                    });
                }
                Token::Tilde => {
                    pop_higher_or_equal(&mut stack, 10, &mut output, &mut runtime_depth, kind);
                    stack.push(StackEntry::Op {
                        token: Token::Tilde,
                        in_stack_pri: 9,
                    });
                }

                Token::LParen => {
                    stack.push(StackEntry::LParen);
                }

                // C `UNTIL_OPERATOR` (`sCalcPostfix.c:759-784`): flush the
                // operators of >= priority (in_coming_pri 10), emit `UNTIL` to
                // the output, then push the `UNTIL_END` half onto the OPERATOR
                // stack, where the ordinary flush rules will place it. Both
                // halves have runtime_effect 0, and `operand_needed` is
                // untouched — the loop body still owes an operand.
                Token::UntilKeyword => {
                    pop_higher_or_equal(&mut stack, 10, &mut output, &mut runtime_depth, kind);
                    let until_pc = output.len();
                    // Patched by `flush_stack_entry` when the paired `UntilEnd`
                    // entry comes off the stack.
                    output.push(Opcode::Control(ControlOp::Until(0)));
                    stack.push(StackEntry::UntilEnd { until_pc });
                }

                Token::Func(func) => {
                    pop_higher_or_equal(&mut stack, 10, &mut output, &mut runtime_depth, kind);
                    if is_vararg(func) {
                        stack.push(StackEntry::VarargFunc {
                            func: func.clone(),
                            in_stack_pri: 9,
                            nargs: 1,
                        });
                    } else {
                        stack.push(StackEntry::Op {
                            token: token.clone(),
                            in_stack_pri: 9,
                        });
                    }
                    // A nullary function (ARNDM, IX) supplies its own
                    // operand: it consumes nothing and pushes one value.
                    // The next token must therefore be an operator, like
                    // any other operand. Functions with arity >= 1 still
                    // need an operand to follow.
                    if !is_vararg(func) && is_nullary_func(func) {
                        operand_needed = false;
                    }
                }

                _ => return Err(CalcError::Syntax),
            }
        } else {
            match token {
                t if binary_op(t).is_some() => {
                    let (isp, icp) = binary_op(t).unwrap();
                    pop_higher_or_equal(&mut stack, icp, &mut output, &mut runtime_depth, kind);
                    stack.push(StackEntry::Op {
                        token: t.clone(),
                        in_stack_pri: isp,
                    });
                    operand_needed = true;
                }

                // C `CLOSE_PAREN` (`sCalcPostfix.c:634-663`): pop to the `(`,
                // remove it, and STOP. The function below the `(` is NOT emitted
                // — it stays on the operator stack at in_stack_pri 9 and is
                // emitted later, by whatever pops it.
                //
                // That deferral is the whole of R13-2. `[` and `{` come in at
                // in_coming_pri 11, above 9, so they do not pop the function, and
                // `LEN("abcd")[0,1]` compiles to `"abcd", 0, 1, SUBRANGE, LEN` —
                // the function applied to the SUBRANGE. C's own postfix dump for
                // `ABS(1)[0,1]` is `1, 0, 1, SUBRANGE, ABS_VAL`. The port used to
                // flush the function here, which bound it to its own argument
                // instead: compiled C vs the old port, `LEN("abcd")[0,1]` = 2 vs 4,
                // `SQRT(4){"2","9"}` = 2 vs 9, `INT("12.9")[0,1]` = 12 vs 13, and
                // aCalc `AMAX(AA)[0,1]` (AA=[3,1,2,9]) = 3 vs 9.
                //
                // Every OTHER operator has in_coming_pri <= 10, so it pops the
                // function on arrival and the emitted order is unchanged: `SQRT(4)+1`
                // is still `4, SQRT, 1, ADD`.
                Token::RParen => loop {
                    match stack.last() {
                        None => return Err(CalcError::ParenNotOpen),
                        Some(StackEntry::LParen) => {
                            stack.pop();
                            // C `:651-662`: a VARARG_OPERATOR below the `(`
                            // inherits the paren's accumulated argument count.
                            // The port keeps that count on the entry itself and
                            // has been maintaining it at each `,`, so there is
                            // nothing to transfer — and, like C, nothing to emit.
                            break;
                        }
                        _ => {
                            let entry = stack.pop().unwrap();
                            runtime_depth += stack_effect(&entry);
                            flush_stack_entry(&entry, &mut output, kind);
                        }
                    }
                },

                // C `SEPARATOR` (`sCalcPostfix.c:612-631`): pop to the innermost
                // `(`, `[` or `{` — all three are separator barriers — and charge
                // one more argument to it.
                Token::Comma => {
                    loop {
                        match stack.last() {
                            None => return Err(CalcError::BadSeparator),
                            Some(e) if is_barrier(e) => break,
                            _ => {
                                let entry = stack.pop().unwrap();
                                runtime_depth += stack_effect(&entry);
                                flush_stack_entry(&entry, &mut output, kind);
                            }
                        }
                    }
                    match stack.last_mut() {
                        // A `,` inside `[` / `{` belongs to the SUBRANGE, and C
                        // charges it to that entry (`:630`). It must not reach a
                        // vararg function further down: compiled C gives
                        // `MAX(4,7)[0,0]` = 7, i.e. `MAX` still took two arguments,
                        // not three. Sharing one stack variant with `(` is what let
                        // the bracket's `,` bump the vararg's count.
                        Some(StackEntry::Subrange { runtime_effect, .. }) => *runtime_effect -= 1,
                        // A `,` directly inside a vararg call's `(` is one more
                        // argument to that function.
                        Some(StackEntry::LParen) => {
                            let lparen_idx = stack.len() - 1;
                            if lparen_idx > 0 {
                                if let StackEntry::VarargFunc { nargs, .. } =
                                    &mut stack[lparen_idx - 1]
                                {
                                    *nargs += 1;
                                }
                            }
                        }
                        _ => unreachable!("the loop above only breaks on a barrier"),
                    }
                    operand_needed = true;
                }

                Token::Question => {
                    pop_higher_strict(&mut stack, 0, &mut output, &mut runtime_depth, kind);
                    output.push(Opcode::Core(CoreOp::CondIf));
                    runtime_depth -= 1;
                    cond_count += 1;
                    operand_needed = true;
                }

                Token::Colon => {
                    pop_higher_strict(&mut stack, 0, &mut output, &mut runtime_depth, kind);
                    output.push(Opcode::Core(CoreOp::CondElse));
                    runtime_depth -= 1;
                    cond_count -= 1;
                    if cond_count < 0 {
                        return Err(CalcError::Conditional);
                    }
                    stack.push(StackEntry::CondEnd);
                    operand_needed = true;
                }

                Token::Semicolon => {
                    while let Some(entry) = stack.last() {
                        if matches!(entry, StackEntry::LParen) {
                            break;
                        }
                        let entry = stack.pop().unwrap();
                        runtime_depth += stack_effect(&entry);
                        flush_stack_entry(&entry, &mut output, kind);
                    }
                    // A pending `UNTIL_END` is NOT closed here by a rule of its
                    // own: it is an ordinary operator-stack entry, so the loop
                    // above already flushed it if and only if it was above the
                    // innermost `(`. Inside `UNTIL(...)` it is not, which is why
                    // the condition ends up INSIDE the loop.
                    //
                    // C postfix.c:452-455 — at a `;` terminator the net runtime
                    // depth must not exceed 1.
                    if cond_count != 0 {
                        return Err(CalcError::Conditional);
                    }
                    if runtime_depth > 1 {
                        return Err(CalcError::TooMany);
                    }
                    operand_needed = true;
                }

                // C `STORE_OPERATOR` (`sCalcPostfix.c:515-567`, `aCalcPostfix.c:483-...`).
                // It has TWO shapes, and C tries them in this order:
                //
                //  1. A DYNAMIC store. Search the operator stack for a pending
                //     `A_FETCH` (`@`) or `A_SFETCH`/`A_AFETCH` (`@@`) — the unary
                //     operator whose index expression has already been emitted —
                //     and rewrite THAT entry in place into `A_STORE` / `A_SSTORE`
                //     (`A_ASTORE` in aCalc). Nothing is retracted from the output:
                //     the index stays, the value follows it, and the store opcode
                //     pops both. Operators sitting ABOVE the rewritten entry are
                //     flushed (`:500-511`), since `:=` has in_coming_pri 0.
                //
                //  2. Otherwise the STATIC store: retract the fetch just emitted
                //     (FETCH_A..FETCH_P or FETCH_AA..FETCH_LL) and park a STORE on
                //     the stack. Anything else is CALC_ERR_BAD_ASSIGNMENT.
                //
                // Either way the store is left ON the operator stack, so a store
                // from an earlier `:=` in a chain stays there — which is how
                // `A:=B:=5` reaches the end at depth -1 and is CALC_ERR_INCOMPLETE
                // rather than underflowing mid-parse.
                Token::Assign => {
                    let pending_dyn = stack.iter().rposition(|e| {
                        matches!(
                            e,
                            StackEntry::Op {
                                token: Token::Func(
                                    FuncName::SDynFetch
                                        | FuncName::SDynSFetch
                                        | FuncName::DynFetch
                                        | FuncName::DynAFetch
                                ),
                                ..
                            }
                        )
                    });
                    match pending_dyn {
                        Some(at) => {
                            // C: "Move operators of >= priority to the output, but
                            // stop before ps1" — `:=` has in_coming_pri 0, so every
                            // entry above the fetch goes.
                            while stack.len() > at + 1 {
                                let entry = stack.pop().unwrap();
                                runtime_depth += stack_effect(&entry);
                                flush_stack_entry(&entry, &mut output, kind);
                            }
                            let StackEntry::Op {
                                token: Token::Func(f),
                                ..
                            } = &stack[at]
                            else {
                                unreachable!("rposition matched a Func entry")
                            };
                            let target = match f {
                                FuncName::SDynFetch | FuncName::DynFetch => StoreTarget::Dyn,
                                _ => StoreTarget::DynDouble,
                            };
                            stack[at] = StackEntry::Store(target);
                        }
                        None => {
                            let target = match output.last() {
                                Some(Opcode::Core(CoreOp::PushVar(idx))) => StoreTarget::Var(*idx),
                                Some(Opcode::Core(CoreOp::PushDoubleVar(idx))) => {
                                    StoreTarget::DoubleVar(*idx)
                                }
                                _ => return Err(CalcError::BadAssignment),
                            };
                            output.pop();
                            stack.push(StackEntry::Store(target));
                        }
                    }
                    // The `:=` element's own runtime_effect, -1, charged on both
                    // paths (`sCalcPostfix.c:566`). On the static path it cancels
                    // the fetch that was just retracted; on the dynamic path it
                    // accounts for the index the store will pop.
                    runtime_depth -= 1;
                    operand_needed = true;
                }

                // `expr[i,j]` and `expr{i,j}`. Both delimiters are in the two
                // synApps tables, but `{` does NOT mean the same thing in both
                // engines: sCalc has `[`=SUBRANGE / `{`=REPLACE
                // (sCalcPostfix.c:215-216) while aCalc has `[`=SUBRANGE /
                // `{`=SUBRANGE_IP, the in-place variant (aCalcPostfix.c:212-213).
                // `flush_stack_entry` picks by `kind`.
                //
                // C reads them as BINARY_OPERATORs with in_coming_pri 11 — so the
                // pop-on-arrival flushes only operators at in_stack_pri >= 11, and
                // NOTHING in any table has one. In particular a function below a
                // just-closed `)` (in_stack_pri 9) survives, which is R13-2.
                Token::LBracket | Token::LBrace => {
                    let in_place = matches!(token, Token::LBrace);
                    pop_higher_or_equal(&mut stack, 11, &mut output, &mut runtime_depth, kind);
                    stack.push(StackEntry::Subrange {
                        in_place,
                        // The table's own `runtime_effect` column: -1
                        // (sCalcPostfix.c:215-216). Each `,` charges one more.
                        runtime_effect: -1,
                    });
                    operand_needed = true;
                }

                // C `CLOSE_BRACKET` / `CLOSE_CURLY` (`sCalcPostfix.c:666-719`):
                // pop to the matching `[` / `{`, then emit THAT entry's code and
                // add its accumulated runtime effect. The close delimiter itself
                // generates nothing — the opener carries the opcode.
                Token::RBracket | Token::RBrace => {
                    let want_in_place = matches!(token, Token::RBrace);
                    let not_open = if want_in_place {
                        CalcError::BraceNotOpen
                    } else {
                        CalcError::BracketNotOpen
                    };
                    loop {
                        match stack.last() {
                            None => return Err(not_open),
                            Some(StackEntry::Subrange { in_place, .. })
                                if *in_place == want_in_place =>
                            {
                                let entry = stack.pop().unwrap();
                                runtime_depth += stack_effect(&entry);
                                flush_stack_entry(&entry, &mut output, kind);
                                break;
                            }
                            _ => {
                                let entry = stack.pop().unwrap();
                                runtime_depth += stack_effect(&entry);
                                flush_stack_entry(&entry, &mut output, kind);
                            }
                        }
                    }
                }

                Token::PipeMinus => {
                    pop_higher_or_equal(&mut stack, 6, &mut output, &mut runtime_depth, kind);
                    stack.push(StackEntry::Op {
                        token: Token::PipeMinus,
                        in_stack_pri: 6,
                    });
                    operand_needed = true;
                }

                _ => return Err(CalcError::Syntax),
            }
        }

        if runtime_depth < 0 {
            return Err(CalcError::Underflow);
        }
        if runtime_depth >= stack_size {
            return Err(CalcError::Overflow);
        }
    }

    // C postfix.c flushes the residual operator stack to the output and
    // accumulates their runtime effect before the final well-formedness check.
    while let Some(entry) = stack.pop() {
        match entry {
            StackEntry::LParen => return Err(CalcError::ParenOpen),
            _ => {
                runtime_depth += stack_effect(&entry);
                flush_stack_entry(&entry, &mut output, kind);
            }
        }
    }

    if cond_count != 0 {
        return Err(CalcError::Conditional);
    }

    // End-of-expression well-formedness. ONE rule, the same in all three C
    // compilers (`postfix.c:499-502`, `sCalcPostfix.c:862-870`,
    // `aCalcPostfix.c:790-799`): the program must leave exactly one value on the
    // runtime stack.
    //
    // An assignment has runtime_effect -1 (`sCalcPostfix.c:226`) and pushes
    // nothing, so a store-TERMINATED source ends at depth 0 and is rejected:
    // compiled sCalcPostfix answers CALC_ERR_INCOMPLETE for `A:=5`, `AA:="x"`,
    // `A:=5;B:=6` and `A:=B:=5`, and 0 for `A:=5;A`. An expression that assigns
    // must still say what its value is.
    //
    // The port exempted depth 0 when the last emitted opcode was a store, which
    // accepted every one of those. There is no exemption: depth 1 or Incomplete.
    //
    // A source that produced no opcodes at all lands here too — `operand_needed`
    // is still set, so it is Incomplete, which is what C answers for a
    // whitespace-only expression, the only way to get here (the empty string
    // never reaches the compiler; see `calc::compile`).
    if operand_needed || runtime_depth != 1 {
        return Err(CalcError::Incomplete);
    }

    output.push(Opcode::Core(CoreOp::End));

    // No post-pass gate on which opcodes this engine may emit: `token::ElementTable`
    // is the single owner of that (one table per C compiler, applied while lexing,
    // exactly as C's `get_element` does), and every token it spells now compiles to
    // an opcode this engine's evaluator runs — including `[` and `{`, which are a
    // string slice in sCalc and an array subrange in aCalc.

    Ok(CompiledExpr {
        code: output,
        kind,
        uses_string,
    })
}

/// C's USES_STRING test, applied where C applies it: to the ELEMENT the lexer
/// just looked up (`sCalcPostfix.c:447-471`), not to the opcode that survives
/// compilation.
///
/// The two are not the same set. `:=` RETRACTS the fetch it follows and emits a
/// store instead (`:552-557`), so `AA:="x"` compiles to a program with no
/// string-typed opcode in it — yet C has already stamped it USES_STRING, from
/// the `FETCH_AA` it looked up a moment earlier, and runs the string evaluator.
/// Recording the flag here, at the lookup, is the only place that can see it.
///
/// The list is C's, symbol for symbol — an ALLOWLIST, and a narrower one than
/// "mentions a string": `DBL`, `BYTE`, `|-` and `@` are all absent from it, so a
/// program whose only string element is one of those keeps the double-only
/// evaluator (which is R14-2's subject).
fn token_is_string_element(token: &Token) -> bool {
    match token {
        // FETCH_AA..FETCH_LL, FETCH_SVAL, LITERAL_STRING.
        Token::DoubleVar(_) | Token::FetchSval | Token::StringLiteral(_) => true,
        // SUBRANGE (`[`) and REPLACE (`{`) — the sCalc table's codes for them.
        Token::LBracket | Token::LBrace => true,
        Token::Func(f) => matches!(
            f,
            FuncName::Str           // TO_STRING
                | FuncName::Printf  // PRINTF
                | FuncName::BinWrite // BIN_WRITE
                | FuncName::Sscanf  // SSCANF
                | FuncName::BinRead // BIN_READ
                | FuncName::SDynSFetch // A_SFETCH (`@@`)
                | FuncName::TrEsc   // TR_ESC
                | FuncName::Esc     // ESC
                | FuncName::Crc16   // CRC16
                | FuncName::ModBus  // MODBUS
                | FuncName::Lrc     // LRC
                | FuncName::AModBus // AMODBUS
                | FuncName::Xor8    // XOR8
                | FuncName::AddXor8 // ADD_XOR8
                | FuncName::Len // LEN
        ),
        _ => false,
    }
}

fn stack_effect(entry: &StackEntry) -> i32 {
    match entry {
        StackEntry::Op {
            token: Token::Minus,
            in_stack_pri: 9,
            ..
        } => 0,
        StackEntry::Op {
            token: Token::Bang, ..
        } => 0,
        StackEntry::Op {
            token: Token::Tilde,
            ..
        } => 0,
        StackEntry::Op {
            token: Token::Func(f),
            ..
        } => {
            // A non-vararg function consumes `func_arity(f)` operands and
            // pushes exactly one result.
            1 - func_arity(f) as i32
        }
        StackEntry::Op { .. } => -1,
        StackEntry::VarargFunc { nargs, .. } => 1 - (*nargs as i32),
        StackEntry::CondEnd => 0,
        StackEntry::Store(_) => -1,
        StackEntry::LParen => 0,
        // The effect C accumulated on the `[` / `{` entry itself: -1 from the
        // table plus -1 per `,`.
        StackEntry::Subrange { runtime_effect, .. } => *runtime_effect,
        // C `sCalcPostfix.c:783` sets `runtime_effect = 0` on the UNTIL_END
        // element explicitly: the opcode only PEEKS the condition
        // (`sCalcPerform.c:1999`, `if (ps->d == 0)`), so the condition survives
        // as the value of the whole `UNTIL(...)`.
        StackEntry::UntilEnd { .. } => 0,
    }
}

/// A `(`, `[` or `{` still open. C's pop loops all stop here — every one of the
/// three carries `in_stack_pri` 0, so no incoming operator's `>=` / `>` test can
/// reach past it, and the explicit `while (pstacktop > stack)` guard keeps the
/// bottom of the stack safe.
fn is_barrier(entry: &StackEntry) -> bool {
    matches!(entry, StackEntry::LParen | StackEntry::Subrange { .. })
}

fn pop_higher_or_equal(
    stack: &mut Vec<StackEntry>,
    incoming_pri: u8,
    output: &mut Vec<Opcode>,
    runtime_depth: &mut i32,
    kind: ExprKind,
) {
    while let Some(entry) = stack.last() {
        if is_barrier(entry) || entry.in_stack_pri() < incoming_pri {
            break;
        }
        let entry = stack.pop().unwrap();
        *runtime_depth += stack_effect(&entry);
        flush_stack_entry(&entry, output, kind);
    }
}

fn pop_higher_strict(
    stack: &mut Vec<StackEntry>,
    incoming_pri: u8,
    output: &mut Vec<Opcode>,
    runtime_depth: &mut i32,
    kind: ExprKind,
) {
    while let Some(entry) = stack.last() {
        if is_barrier(entry) || entry.in_stack_pri() <= incoming_pri {
            break;
        }
        let entry = stack.pop().unwrap();
        *runtime_depth += stack_effect(&entry);
        flush_stack_entry(&entry, output, kind);
    }
}
