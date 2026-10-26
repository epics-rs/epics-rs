use super::bytecode::Emitter;
use super::error::CalcError;
use super::opcodes::{ControlOp, CoreOp, Opcode};
use super::token::{ConstName, FuncName, Lexer, StoreRule, Terminator, Token};
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
    /// `in_stack_pri` 0 and code `NOT_GENERATED`. Each `,` charges -1 to it
    /// (`:630`); `)` hands that to a vararg function below (`:657-660`). Only a
    /// dynamic `:=` ever flushes it (`:532-544`), and then C writes its
    /// `NOT_GENERATED` byte and adds its effect like any other entry.
    LParen {
        runtime_effect: i32,
        /// The operand now being read: the whole group, or with `args` the
        /// argument, which each `,` ends.
        span: Span,
        /// The list of a function taking more than one operand. A bare
        /// `(3,K:=1)` leaves the 3 as one operand, as C evaluates it.
        args: bool,
    },
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
        /// The argument now being read; each `,` starts the next.
        span: Span,
    },
    /// A `VARARG_OPERATOR`. `runtime_effect` is C's: 0 from the table until
    /// `)` copies the paren's, and the count byte it writes is `1 - effect`.
    VarargFunc {
        func: FuncName,
        in_stack_pri: u8,
        runtime_effect: i32,
    },
    /// C's COND_END. It carries the true branch's sum until the false branch
    /// ends (`None` after), which may be before C emits it: a conditional in
    /// a true branch ends at that branch's `:`.
    CondEnd(Option<Arm>),
    Store {
        target: StoreTarget,
        in_stack_pri: u8,
    },
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
    UntilEnd,
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

/// An operand being read. Where it began is kept on each path
/// ([`Path::spans`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span(u32);

/// A `?` whose `:` has not been read: the span of its true branch, C's count
/// at the `?`, and the paths there, on which the false branch starts.
#[derive(Debug, Clone)]
struct Fork {
    then: Span,
    depth: i32,
    rest: Vec<Path>,
}

/// A conditional whose true branch has been read: that branch's span, the
/// paths it ended on (depths counted from zero), and which `:` this was.
#[derive(Debug, Clone)]
struct Arm {
    then: Span,
    ends: Vec<Path>,
    n: usize,
}

/// A value an operand should have pushed and did not (CBUG-AJ1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hole {
    /// `:=` pushes nothing (runtime_effect -1, `sCalcPostfix.c:226`).
    Store(i32),
    /// A conditional's value, on the path of a branch that left none.
    Missing(i32),
}

impl Hole {
    fn depth(self) -> i32 {
        match self {
            Hole::Store(d) | Hole::Missing(d) => d,
        }
    }
}

/// One way through the conditionals read so far. Depths are those of the
/// run-time stack.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Path {
    /// The run-time depth less C's count.
    off: i32,
    holes: Vec<Hole>,
    /// Each open span, with the depth and the number of holes it began at.
    spans: Vec<(Span, i32, usize)>,
}

impl Path {
    /// Holes are only ever removed here, so no span begins past the end.
    fn cut(&mut self, len: usize) -> Vec<Hole> {
        for span in &mut self.spans {
            span.2 = span.2.min(len);
        }
        self.holes.split_off(len)
    }

    fn end(&mut self, span: Span) -> Option<(i32, usize)> {
        let i = self.spans.iter().position(|s| s.0 == span)?;
        let (_, depth, len) = self.spans.remove(i);
        Some((depth, len))
    }

    /// The branch `then` ends: only whether it left its value is kept, and
    /// the depth is counted from zero until the conditional ends.
    fn finish(&mut self, then: Span, depth: i32) {
        let real = depth + self.off;
        if let Some((from, len)) = self.end(then) {
            let missing = real <= from || self.holes[len..].contains(&Hole::Missing(real));
            self.cut(len);
            if missing {
                self.holes.push(Hole::Missing(real));
            }
        }
        self.off = real;
    }
}

/// Past this many paths the source is accepted as C accepts it, rather than
/// spend compile time exponential in its conditionals.
const MAX_PATHS: usize = 256;

/// The missing values C's depth count cannot see (CBUG-AJ1).
///
/// An operator whose operand pushed nothing pops whatever is below: the
/// previous statement's value, or the stack's sentinel element
/// (`sCalcPerform.c:408-409,845-847`, `aCalcPerform.c:418-420`). C only
/// checks that the depth stays >= 0 after the operator (`sCalcPostfix.c:820`),
/// which lets both through.
///
/// The branches of a conditional may leave different numbers of values, and
/// C charges `:` -1 so the two count once though COND_ELSE pops nothing, so
/// each path is followed on its own. A path that pops past a hole reads a
/// missing value and is dropped, and so does one whose result is a
/// conditional's missing value. The source is refused when a path read one
/// and none is left that ends with one value; a path ending at another depth
/// fails C's end-of-run check (`sCalcPerform.c:816`), as it fails the
/// evaluators here, so a source no path reads past is kept.
struct Holes {
    paths: Vec<Path>,
    /// A path read a missing value.
    read: bool,
    /// The spans open now. Every path has each of them, and no other.
    open: Vec<Span>,
    next: u32,
    /// More than [`MAX_PATHS`] paths were met.
    lost: bool,
}

impl Default for Holes {
    fn default() -> Self {
        Self {
            paths: vec![Path::default()],
            read: false,
            open: Vec::new(),
            next: 0,
            lost: false,
        }
    }
}

impl Holes {
    /// Drop the paths that read a missing value.
    fn drop_reads(&mut self, reads: impl Fn(&Path) -> bool) {
        let before = self.paths.len();
        self.paths.retain(|p| !reads(p));
        self.read |= self.paths.len() < before;
    }

    fn take(&mut self, depth: i32, pops: i32) {
        self.drop_reads(|p| {
            let floor = p.holes.iter().map(|h| h.depth()).max().unwrap_or(0);
            pops > depth + p.off - floor
        });
    }

    fn store(&mut self, depth: i32) {
        for p in &mut self.paths {
            p.holes.push(Hole::Store(depth + p.off));
        }
    }

    fn open(&mut self, depth: i32) -> Span {
        let span = Span(self.next);
        self.next += 1;
        self.open.push(span);
        for p in &mut self.paths {
            p.spans.push((span, depth + p.off, p.holes.len()));
        }
        span
    }

    /// An operand that left a value has its stores as statements, as
    /// `A?(0;C:=3.3):B` does, and a conditional's missing value buried unless
    /// the conditional came last.
    fn close(&mut self, span: Span, depth: i32) {
        self.open.retain(|s| *s != span);
        for p in &mut self.paths {
            let real = depth + p.off;
            if let Some((from, len)) = p.end(span)
                && real > from
            {
                let tail = p.cut(len);
                p.holes
                    .extend(tail.into_iter().filter(|h| *h == Hole::Missing(real)));
            }
        }
    }

    /// `?`, after it pops the condition.
    fn fork(&mut self, depth: i32) -> Fork {
        let then = self.open(depth);
        Fork {
            then,
            depth,
            rest: self.paths.clone(),
        }
    }

    /// `:`, before C's -1: the false branch starts where the true one did.
    fn branch(&mut self, fork: Fork, depth: i32, n: usize) -> Arm {
        let mut ends = std::mem::replace(&mut self.paths, fork.rest);
        for p in &mut ends {
            p.finish(fork.then, depth);
        }
        for p in &mut self.paths {
            p.off += fork.depth - (depth - 1);
        }
        self.even(depth - 1);
        Arm {
            then: fork.then,
            ends,
            n,
        }
    }

    /// The false branch is read too: go on along the paths through either.
    fn join(&mut self, arm: &Arm, depth: i32) {
        for p in &mut self.paths {
            p.finish(arm.then, depth);
        }
        self.open.retain(|s| *s != arm.then);
        self.paths.extend(arm.ends.iter().cloned());
        for p in &mut self.paths {
            p.off -= depth;
        }
        self.even(depth);
    }

    /// Give every path the open spans, a span one did not see begun beginning
    /// where the path rejoined, and drop the paths that repeat one.
    fn even(&mut self, depth: i32) {
        let mut paths: Vec<Path> = Vec::with_capacity(self.paths.len());
        for mut p in self.paths.drain(..) {
            p.spans.retain(|s| self.open.contains(&s.0));
            for &span in &self.open {
                if !p.spans.iter().any(|s| s.0 == span) {
                    p.spans.push((span, depth + p.off, p.holes.len()));
                }
            }
            p.spans.sort_by_key(|s| s.0.0);
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        if paths.len() > MAX_PATHS {
            paths.clear();
            self.lost = true;
        }
        self.paths = paths;
    }

    /// The result is the top of the stack.
    fn refused(&mut self, depth: i32) -> bool {
        self.drop_reads(|p| p.holes.contains(&Hole::Missing(depth + p.off)));
        self.read && !self.lost && !self.paths.iter().any(|p| depth + p.off == 1)
    }
}

impl StackEntry {
    fn in_stack_pri(&self) -> u8 {
        match self {
            StackEntry::Op { in_stack_pri, .. } => *in_stack_pri,
            StackEntry::LParen { .. } => 0,
            // `[` / `{`: in_stack_pri 0, like `(` — a barrier nothing pops.
            StackEntry::Subrange { .. } => 0,
            StackEntry::VarargFunc { in_stack_pri, .. } => *in_stack_pri,
            StackEntry::CondEnd(_) => 0,
            StackEntry::Store { in_stack_pri, .. } => *in_stack_pri,
            // C: `{"UNTIL", 0, 10, 0, UNTIL_OPERATOR, UNTIL}` — in_stack_pri 0,
            // so no incoming operator ever pops it off the stack.
            StackEntry::UntilEnd => 0,
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
    };
    Opcode::Core(core)
}

/// Emit one operator-stack entry to the output — C's
/// `*pout++ = pstacktop->code`, the single move every pop site performs.
///
/// `kind` is needed because `[` and `{` carry a per-engine code: `[` is
/// `SUBRANGE` in both synApps tables but `{` is `REPLACE` in sCalc
/// (`sCalcPostfix.c:216`) and `SUBRANGE_IP` in aCalc (`aCalcPostfix.c:213`).
fn flush_stack_entry(entry: &StackEntry, output: &mut Emitter, kind: ExprKind) {
    match entry {
        StackEntry::Subrange { in_place, .. } => {
            output.op(match (kind, in_place) {
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
            output.op(Opcode::Core(CoreOp::Neg));
        }
        StackEntry::Op {
            token: Token::Bang, ..
        } => {
            output.op(Opcode::Core(CoreOp::Not));
        }
        StackEntry::Op {
            token: Token::Tilde,
            ..
        } => {
            output.op(Opcode::Core(CoreOp::BitNot));
        }
        StackEntry::Op {
            token: Token::Func(f),
            ..
        } => {
            output.op(func_to_opcode(f, 1));
        }
        StackEntry::Op { token, .. } => {
            output.op(token_to_binary_opcode(token));
        }
        StackEntry::VarargFunc {
            func,
            runtime_effect,
            ..
        } => {
            output.op(func_to_opcode(func, (1 - runtime_effect) as u8));
        }
        StackEntry::CondEnd(_) => {
            output.op(Opcode::Core(CoreOp::CondEnd));
        }
        StackEntry::UntilEnd => {
            output.op(Opcode::Control(ControlOp::UntilEnd));
        }
        StackEntry::Store { target, .. } => {
            use super::opcodes::{ArrayOp, StringOp};
            output.op(match target {
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
        StackEntry::LParen { .. } => output.op(Opcode::Control(ControlOp::NotGenerated)),
    }
}

/// Compile `expr` for ONE engine, reading each element from that engine's
/// `ELEMENT` table as C's `get_element(operand_needed, ...)` does — so the list
/// searched, and the position a lexing error is reported at, follow the
/// parser's state.
///
/// A source with no elements is NOT special-cased: C only
/// short-circuits the literal empty string (`*psrc == '\0'`), and an expression
/// that merely *lexes* to nothing — `"   "` — walks the normal path and comes
/// out `CALC_ERR_INCOMPLETE`, because `operand_needed` is still set at the end.
/// Compiled C, all three engines: `postfix("   ")` = -1, error 8. The empty
/// SOURCE is handled by each engine's entry point in `calc::mod`, where C puts
/// it.
pub fn compile(expr: &str, kind: ExprKind) -> Result<CompiledExpr, CalcError> {
    let mut lexer = Lexer::new(expr, kind);
    let mut output = Emitter::new(kind);
    let mut stack: Vec<StackEntry> = Vec::new();
    let mut operand_needed = true;
    let mut runtime_depth: i32 = 0;
    let mut holes = Holes::default();
    // C's `cond_count`: each `?` still waiting for its `:`, and how many `:`
    // had been read then.
    let mut conds: Vec<(Fork, usize)> = Vec::new();
    let mut elses = 0;
    // C `*ppostfix = USES_STRING` (`sCalcPostfix.c:447-471`) — latched as each
    // element is LOOKED UP, which is the only moment the retract-and-rewrite of
    // `:=` has not yet erased the evidence. sCalc's marker only; the numeric and
    // array compilers have none. The marker is the program's head byte, which
    // the emitter owns.
    // The ceiling this flavour's `*Perform` can actually hold, from its own
    // element table (C `CALCPERFORM_STACK` 80 / `SCALC_STACKSIZE` 30 /
    // `ACALC_STACKSIZE` 20). One shared compiler, three C limits: a literal here
    // was right for sCalc and wrong in OPPOSITE directions for the other two.
    let stack_size = super::token::runtime_stack_size(kind);
    // No `bracket_depth` / `brace_depth` counters: the open `[` / `{` IS a
    // `StackEntry::Subrange` on the operator stack, exactly as in C, so the stack
    // already answers "is one open, and which". A side counter was a second
    // record of the same fact and could disagree with the stack.

    while let Some(token) = lexer.next(operand_needed)? {
        let token = &token;

        if kind == ExprKind::String && token_is_string_element(token) {
            output.set_uses_string();
        }

        if operand_needed {
            match token {
                Token::Number(v) => {
                    output.op(Opcode::Core(CoreOp::PushConst(*v)));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::Var(idx) => {
                    output.op(Opcode::Core(CoreOp::PushVar(*idx)));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::DoubleVar(idx) => {
                    output.op(Opcode::Core(CoreOp::PushDoubleVar(*idx)));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::FetchVal => {
                    output.op(Opcode::Core(CoreOp::FetchVal));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::FetchAval => {
                    output.op(Opcode::Array(super::opcodes::ArrayOp::FetchAval));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::ConstIx => {
                    output.op(Opcode::Array(super::opcodes::ArrayOp::ConstIndex));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::ARndm => {
                    output.op(Opcode::Array(super::opcodes::ArrayOp::ArrayRandom));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::FetchSval => {
                    output.op(Opcode::Core(CoreOp::FetchSval));
                    runtime_depth += 1;
                    operand_needed = false;
                    // C sCalcPostfix.c:452 lists FETCH_SVAL among the opcodes
                    // that mark the postfix USES_STRING.
                }
                Token::Rndm => {
                    output.op(Opcode::Core(CoreOp::Random));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::Nrndm => {
                    output.op(Opcode::Core(CoreOp::NormalRandom));
                    runtime_depth += 1;
                    operand_needed = false;
                }
                Token::Const(c) => {
                    match c {
                        ConstName::Pi => output.op(Opcode::Core(CoreOp::Pi)),
                        ConstName::D2R => output.op(Opcode::Core(CoreOp::D2R)),
                        ConstName::R2D => output.op(Opcode::Core(CoreOp::R2D)),
                        ConstName::S2R => output.op(Opcode::Core(CoreOp::S2R)),
                        ConstName::R2S => output.op(Opcode::Core(CoreOp::R2S)),
                    }
                    runtime_depth += 1;
                    operand_needed = false;
                }

                Token::StringLiteral(s) => {
                    output.string(s);
                    runtime_depth += 1;
                    operand_needed = false;
                }

                // Unary operators
                Token::Minus => {
                    pop_higher_or_equal(
                        &mut stack,
                        10,
                        &mut output,
                        &mut runtime_depth,
                        &mut holes,
                        kind,
                    );
                    stack.push(StackEntry::Op {
                        token: Token::Minus,
                        in_stack_pri: 9,
                    });
                }
                Token::Bang => {
                    pop_higher_or_equal(
                        &mut stack,
                        10,
                        &mut output,
                        &mut runtime_depth,
                        &mut holes,
                        kind,
                    );
                    stack.push(StackEntry::Op {
                        token: Token::Bang,
                        in_stack_pri: 9,
                    });
                }
                Token::Tilde => {
                    pop_higher_or_equal(
                        &mut stack,
                        10,
                        &mut output,
                        &mut runtime_depth,
                        &mut holes,
                        kind,
                    );
                    stack.push(StackEntry::Op {
                        token: Token::Tilde,
                        in_stack_pri: 9,
                    });
                }

                Token::LParen => {
                    let args = match stack.last() {
                        Some(StackEntry::VarargFunc { .. }) => true,
                        Some(StackEntry::Op {
                            token: Token::Func(f),
                            ..
                        }) => func_arity(f) > 1,
                        _ => false,
                    };
                    stack.push(StackEntry::LParen {
                        runtime_effect: 0,
                        span: holes.open(runtime_depth),
                        args,
                    });
                }

                // C `UNTIL_OPERATOR` (`sCalcPostfix.c:759-784`): flush the
                // operators of >= priority (in_coming_pri 10), emit `UNTIL` to
                // the output, then push the `UNTIL_END` half onto the OPERATOR
                // stack, where the ordinary flush rules will place it. Both
                // halves have runtime_effect 0, and `operand_needed` is
                // untouched — the loop body still owes an operand.
                Token::UntilKeyword => {
                    pop_higher_or_equal(
                        &mut stack,
                        10,
                        &mut output,
                        &mut runtime_depth,
                        &mut holes,
                        kind,
                    );
                    output.op(Opcode::Control(ControlOp::Until));
                    stack.push(StackEntry::UntilEnd);
                }

                Token::Func(func) => {
                    pop_higher_or_equal(
                        &mut stack,
                        10,
                        &mut output,
                        &mut runtime_depth,
                        &mut holes,
                        kind,
                    );
                    if is_vararg(func) {
                        stack.push(StackEntry::VarargFunc {
                            func: func.clone(),
                            in_stack_pri: 9,
                            runtime_effect: 0,
                        });
                    } else {
                        stack.push(StackEntry::Op {
                            token: token.clone(),
                            in_stack_pri: 9,
                        });
                    }
                }

                _ => return Err(CalcError::Syntax),
            }
        } else {
            match token {
                t if binary_op(t).is_some() => {
                    let (isp, icp) = binary_op(t).unwrap();
                    pop_higher_or_equal(
                        &mut stack,
                        icp,
                        &mut output,
                        &mut runtime_depth,
                        &mut holes,
                        kind,
                    );
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
                        Some(StackEntry::LParen {
                            runtime_effect,
                            span,
                            ..
                        }) => {
                            let paren = *runtime_effect;
                            holes.close(*span, runtime_depth);
                            stack.pop();
                            // C `:651-662`: a VARARG_OPERATOR below the `(`
                            // inherits the paren's accumulated effect, and the
                            // `(` itself emits nothing.
                            if let Some(StackEntry::VarargFunc { runtime_effect, .. }) =
                                stack.last_mut()
                            {
                                *runtime_effect = paren;
                            }
                            break;
                        }
                        _ => {
                            let entry = stack.pop().unwrap();
                            flush(&entry, &mut output, &mut runtime_depth, &mut holes, kind);
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
                                flush(&entry, &mut output, &mut runtime_depth, &mut holes, kind);
                            }
                        }
                    }
                    let (effect, span, args) = match stack.last_mut() {
                        // A `,` inside `[` / `{` belongs to the SUBRANGE, and C
                        // charges it to that entry (`:630`). It must not reach a
                        // vararg function further down: compiled C gives
                        // `MAX(4,7)[0,0]` = 7, i.e. `MAX` still took two arguments,
                        // not three. Sharing one stack variant with `(` is what let
                        // the bracket's `,` bump the vararg's count.
                        Some(StackEntry::Subrange {
                            runtime_effect,
                            span,
                            ..
                        }) => (runtime_effect, span, true),
                        Some(StackEntry::LParen {
                            runtime_effect,
                            span,
                            args,
                        }) => (runtime_effect, span, *args),
                        _ => unreachable!("the loop above only breaks on a barrier"),
                    };
                    *effect -= 1;
                    if args {
                        holes.close(*span, runtime_depth);
                        *span = holes.open(runtime_depth);
                    }
                    operand_needed = true;
                }

                Token::Question => {
                    pop_higher_strict(
                        &mut stack,
                        0,
                        &mut output,
                        &mut runtime_depth,
                        &mut holes,
                        kind,
                    );
                    output.op(Opcode::Core(CoreOp::CondIf));
                    holes.take(runtime_depth, 1);
                    runtime_depth -= 1;
                    conds.push((holes.fork(runtime_depth), elses));
                    operand_needed = true;
                }

                Token::Colon => {
                    pop_higher_strict(
                        &mut stack,
                        0,
                        &mut output,
                        &mut runtime_depth,
                        &mut holes,
                        kind,
                    );
                    output.op(Opcode::Core(CoreOp::CondElse));
                    let Some((fork, first)) = conds.pop() else {
                        return Err(CalcError::Conditional);
                    };
                    // A conditional inside the true branch ends here, though
                    // its COND_END stays on the stack below this one.
                    for entry in stack.iter_mut().rev() {
                        if let StackEntry::CondEnd(arm) = entry
                            && let Some(inner) = arm.take_if(|a| a.n >= first)
                        {
                            holes.join(&inner, runtime_depth);
                        }
                    }
                    let arm = holes.branch(fork, runtime_depth, elses);
                    elses += 1;
                    runtime_depth -= 1;
                    stack.push(StackEntry::CondEnd(Some(arm)));
                    operand_needed = true;
                }

                Token::Semicolon => {
                    let rule = super::token::expr_terminator(kind);
                    while let Some(entry) = stack.last() {
                        if matches!(entry, StackEntry::LParen { .. }) {
                            if rule == Terminator::Statement {
                                return Err(CalcError::ParenOpen);
                            }
                            break;
                        }
                        let entry = stack.pop().unwrap();
                        flush(&entry, &mut output, &mut runtime_depth, &mut holes, kind);
                    }
                    // A pending `UNTIL_END` is NOT closed here by a rule of its
                    // own: it is an ordinary operator-stack entry, so the loop
                    // above already flushed it if and only if it was above the
                    // innermost `(`. Inside `UNTIL(...)` it is not, which is why
                    // the condition ends up INSIDE the loop.
                    if rule == Terminator::Statement {
                        if !conds.is_empty() {
                            return Err(CalcError::Conditional);
                        }
                        if runtime_depth > 1 {
                            return Err(CalcError::TooMany);
                        }
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
                    let store = super::token::store_rule(kind);
                    if store == StoreRule::TopLevel && !stack.is_empty() {
                        return Err(CalcError::BadAssignment);
                    }
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
                                flush(&entry, &mut output, &mut runtime_depth, &mut holes, kind);
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
                            // The index, which the -1 below charges.
                            holes.take(runtime_depth, 1);
                            stack[at] = StackEntry::Store {
                                target,
                                in_stack_pri: store.in_stack_pri(),
                            };
                        }
                        None => {
                            let target = match output.last() {
                                Some(Opcode::Core(CoreOp::PushVar(idx))) => StoreTarget::Var(idx),
                                Some(Opcode::Core(CoreOp::PushDoubleVar(idx))) => {
                                    StoreTarget::DoubleVar(idx)
                                }
                                _ => return Err(CalcError::BadAssignment),
                            };
                            output.retract_last();
                            stack.push(StackEntry::Store {
                                target,
                                in_stack_pri: store.in_stack_pri(),
                            });
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
                    pop_higher_or_equal(
                        &mut stack,
                        11,
                        &mut output,
                        &mut runtime_depth,
                        &mut holes,
                        kind,
                    );
                    stack.push(StackEntry::Subrange {
                        in_place,
                        // The table's own `runtime_effect` column: -1
                        // (sCalcPostfix.c:215-216). Each `,` charges one more.
                        runtime_effect: -1,
                        span: holes.open(runtime_depth),
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
                            Some(StackEntry::Subrange { in_place, span, .. })
                                if *in_place == want_in_place =>
                            {
                                holes.close(*span, runtime_depth);
                                let entry = stack.pop().unwrap();
                                flush(&entry, &mut output, &mut runtime_depth, &mut holes, kind);
                                break;
                            }
                            _ => {
                                let entry = stack.pop().unwrap();
                                flush(&entry, &mut output, &mut runtime_depth, &mut holes, kind);
                            }
                        }
                    }
                }

                Token::PipeMinus => {
                    pop_higher_or_equal(
                        &mut stack,
                        6,
                        &mut output,
                        &mut runtime_depth,
                        &mut holes,
                        kind,
                    );
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
            StackEntry::LParen { .. } => return Err(CalcError::ParenOpen),
            _ => flush(&entry, &mut output, &mut runtime_depth, &mut holes, kind),
        }
    }

    if !conds.is_empty() {
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
    // After every check of C's, so a source C refuses keeps C's error.
    if holes.refused(runtime_depth) {
        return Err(CalcError::Underflow);
    }

    // No post-pass gate on which opcodes this engine may emit: `token::ElementTable`
    // is the single owner of that (one table per C compiler, applied while lexing,
    // exactly as C's `get_element` does), and every token it spells now compiles to
    // an opcode this engine's evaluator runs — including `[` and `{`, which are a
    // string slice in sCalc and an array subrange in aCalc.

    Ok(CompiledExpr::new(output.finish()?, kind))
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
        StackEntry::VarargFunc { runtime_effect, .. } => *runtime_effect,
        StackEntry::CondEnd(_) => 0,
        StackEntry::Store { .. } => -1,
        StackEntry::LParen { runtime_effect, .. } => *runtime_effect,
        // The effect C accumulated on the `[` / `{` entry itself: -1 from the
        // table plus -1 per `,`.
        StackEntry::Subrange { runtime_effect, .. } => *runtime_effect,
        // C `sCalcPostfix.c:783` sets `runtime_effect = 0` on the UNTIL_END
        // element explicitly: the opcode only PEEKS the condition
        // (`sCalcPerform.c:1999`, `if (ps->d == 0)`), so the condition survives
        // as the value of the whole `UNTIL(...)`.
        StackEntry::UntilEnd => 0,
    }
}

/// A `(`, `[` or `{` still open. C's pop loops all stop here — every one of the
/// three carries `in_stack_pri` 0, so no incoming operator's `>=` / `>` test can
/// reach past it, and the explicit `while (pstacktop > stack)` guard keeps the
/// bottom of the stack safe.
fn is_barrier(entry: &StackEntry) -> bool {
    matches!(
        entry,
        StackEntry::LParen { .. } | StackEntry::Subrange { .. }
    )
}

/// Emit an entry leaving the operator stack and charge its effect. A store
/// pushes nothing, `(` emits a no-op and COND_END runs as one; every other entry
/// pushes one value.
fn flush(
    entry: &StackEntry,
    output: &mut Emitter,
    runtime_depth: &mut i32,
    holes: &mut Holes,
    kind: ExprKind,
) {
    let effect = stack_effect(entry);
    let pushes = match entry {
        StackEntry::Store { .. } | StackEntry::LParen { .. } | StackEntry::CondEnd(_) => 0,
        _ => 1,
    };
    holes.take(*runtime_depth, pushes - effect);
    *runtime_depth += effect;
    match entry {
        StackEntry::Store { .. } => holes.store(*runtime_depth),
        StackEntry::CondEnd(Some(arm)) => holes.join(arm, *runtime_depth),
        _ => {}
    }
    flush_stack_entry(entry, output, kind);
}

fn pop_higher_or_equal(
    stack: &mut Vec<StackEntry>,
    incoming_pri: u8,
    output: &mut Emitter,
    runtime_depth: &mut i32,
    holes: &mut Holes,
    kind: ExprKind,
) {
    while let Some(entry) = stack.last() {
        if is_barrier(entry) || entry.in_stack_pri() < incoming_pri {
            break;
        }
        let entry = stack.pop().unwrap();
        flush(&entry, output, runtime_depth, holes, kind);
    }
}

fn pop_higher_strict(
    stack: &mut Vec<StackEntry>,
    incoming_pri: u8,
    output: &mut Emitter,
    runtime_depth: &mut i32,
    holes: &mut Holes,
    kind: ExprKind,
) {
    while let Some(entry) = stack.last() {
        if is_barrier(entry) || entry.in_stack_pri() <= incoming_pri {
            break;
        }
        let entry = stack.pop().unwrap();
        flush(&entry, output, runtime_depth, holes, kind);
    }
}
