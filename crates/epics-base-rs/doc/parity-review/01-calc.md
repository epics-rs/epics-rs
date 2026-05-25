# CALC Expression Engine — Parity Review

Rust: `crates/epics-base-rs/src/calc/` (`engine/`, `math/`, `mod.rs`)
C reference: `epics-base/modules/libcom/src/calc/` (`postfix.c`, `calcPerform.c`, `postfix.h`, `postfixPvt.h`)

## Context

The Rust `calc/engine/` is **not a port of epics-base `postfix.c`/`calcPerform.c`**. It is a
re-implementation modeled on synApps `sCalcPostfix.c` / `aCalcPostfix.c` (string + array CALC,
the aCalcout/sCalcout territory), with an epics-base-compatible *subset* for plain numeric CALC.
Consequently many findings below are structural divergences from epics-base semantics, not
isolated typos. Where a feature only exists in synApps (string ops, array ops, `>?`/`<?`,
`UNTIL`, `NRNDM`, double-letter vars) there is no C reference and it is reviewed only for
internal correctness, per scope.

## Summary

| Severity | Count |
|----------|-------|
| Critical | 1 |
| High     | 7 |
| Medium   | 6 |
| Low      | 5 |
| **Total**| **19** |

---

## Critical

### C-1. `calcPerform` final-result check missing — multi-value / empty stack accepted silently
- **Rust:** `calc/engine/numeric.rs:342` — `Ok(stack.last().copied().unwrap_or(0.0))`
- **C:** `calcPerform.c:418-422` — `if (ptop != stack + 1) return -1; *presult = *ptop;`
- **Diverges:** C requires the runtime stack to hold **exactly one** value at `END_EXPRESSION`
  and returns an error (`-1`) otherwise. The Rust evaluator never checks the residual stack
  depth: if the postfix leaves multiple values it silently returns the topmost; if it leaves
  zero values it silently returns `0.0`.
- **Impact:** A postfix stream that the compiler did not fully validate (or that was hand-built,
  or produced by a future compiler bug) yields a wrong scalar instead of an error. The
  compile-time `runtime_depth` check in `postfix.rs` is the only guard; any gap there becomes a
  silently-wrong computed value at runtime instead of a detected fault. C catches it
  unconditionally at evaluation time. Note the compiler's own final check
  (`postfix.rs:589`) only tests `operand_needed`, never `runtime_depth == 1` (see H-1), so this
  runtime guard is genuinely load-bearing in C and absent here.

---

## High

### H-1. Compiler omits the `runtime_depth == 1` end-of-expression check
- **Rust:** `calc/engine/postfix.rs:589-591` — only `if operand_needed && !output.is_empty()`
- **C:** `postfix.c:499-502` — `if (operand_needed || runtime_depth != 1) *perror = CALC_ERR_INCOMPLETE;`
  and `postfix.c:452-455` at each `;` — `if (runtime_depth > 1) *perror = CALC_ERR_TOOMANY;`
- **Diverges:** C rejects an expression whose net runtime stack depth is not exactly 1
  (`CALC_ERR_INCOMPLETE`), and rejects `runtime_depth > 1` at a `;` terminator with the distinct
  code `CALC_ERR_TOOMANY`. The Rust compiler tracks `runtime_depth` and checks only
  `>= 30` (overflow) and `< 0` (underflow); it never verifies the final depth equals 1, and has
  no `Semicolon`-time `> 1` check.
- **Impact:** Expressions such as `1 2` (two operands, no operator) or `A;B` (two
  unassigned sub-expressions) compile successfully in Rust but are rejected by C. `CalcError::TooMany`
  is defined in `error.rs:5` but is never produced anywhere. Wrong/missing error code; combined
  with C-1 the wrong value is then returned at runtime instead of any error at all.

### H-2. `max()` / `min()` NaN propagation differs from C
- **Rust:** `calc/engine/numeric.rs:286-313` — `if v > result || result.is_nan() { result = v; }`
  (and `<` for min). `result` is seeded from the **last-pushed** (top-of-stack) argument.
- **C:** `calcPerform.c:191-207` — `top = *ptop--; if (*ptop < top || isnan(top)) *ptop = top;`
  The accumulator `*ptop` is the **deepest** argument; `top` is each successively shallower one.
  NaN is adopted only when the *incoming* operand `top` is NaN.
- **Diverges:** C's rule is "result becomes `top` if `top` is NaN, regardless of the accumulator";
  Rust's rule is "result becomes `v` if the *accumulator* is NaN". These are not equivalent.
  Example `max(nan, 1)`: C pushes `nan` then `1`; nargs=2; `top=1`, `*ptop=nan`; `nan<1` is
  false, `isnan(1)` is false → result stays **nan**. Rust pops `1` first as `result`, then pops
  `nan` as `v`; `nan>1` false, `result.is_nan()` (result=1) false → result stays **1**.
  So C `max(nan,1)=nan`, Rust `max(nan,1)=1`. For `max(1, nan)` both give nan but for different
  reasons. The two implementations disagree whenever a NaN is not in the accumulator slot.
- **Impact:** Wrong computed value for any `max`/`min` whose NaN argument is not the first one.

### H-3. `nint()` uses `i64` truncation; C uses `epicsInt32`
- **Rust:** `calc/engine/numeric.rs:235-243` — `let rounded = if a>=0.0 {(a+0.5) as i64} else {(a-0.5) as i64}; stack.push(rounded as f64);`
- **C:** `calcPerform.c:291-294` — `*ptop = (epicsInt32)(top>=0 ? top+0.5 : top-0.5);`
- **Diverges:** C casts the rounded value to a **32-bit** signed integer; values outside
  `[-2^31, 2^31-1]` wrap modulo 2^32 (implementation-defined, but in practice wrap). Rust casts
  to `i64`; Rust `f64 as iN` *saturates* (does not wrap) for out-of-range and maps NaN to 0.
  For `nint(3e9)` C yields a wrapped negative 32-bit value; Rust yields `3000000000.0`.
  For `nint(1e30)` C yields some wrapped 32-bit value; Rust saturates to `i64::MAX` as f64.
- **Impact:** Wrong computed value for `nint` of any argument whose rounded magnitude exceeds
  2^31. Same issue in `string.rs:313-318` and `array.rs:298-303`.

### H-4. `MODULO` integer conversion uses `i64`, not `epicsInt32` with the C `d2i` rule
- **Rust:** `calc/engine/numeric.rs:63-70` — `if b as i64 == 0 {NAN} else {((a as i64)%(b as i64)) as f64}`
- **C:** `calcPerform.c:162-168` — `itop = (epicsInt32)*ptop--; if(itop) *ptop = (epicsInt32)*ptop % itop; else *ptop = epicsNAN;`
- **Diverges:** Two divergences. (a) Operand width: C truncates both operands to **32-bit**;
  Rust uses 64-bit. `5e9 % 3` differs (C wraps `5e9` into int32 first). (b) Zero detection:
  C tests `(epicsInt32)den`; Rust tests `den as i64`. A denominator like `4294967296.0`
  (= 2^32) is `0` as `epicsInt32` (C → NaN result) but non-zero as `i64` (Rust → divides,
  no NaN). Also a denominator in `(0,1)` truncates to int 0 in both, OK.
- **Impact:** Wrong computed value / wrong NaN behavior for `%` with large operands. Same in
  `string.rs:96-103` and `array.rs:71-79`.

### H-5. Bitwise operators use plain `as i32`, not the C `d2i`/`d2ui` conversion
- **Rust:** `calc/engine/numeric.rs:122-150` — `(a as i32)`, `(a as u32)` directly.
- **C:** `calcPerform.c:325-326` — `#define d2i(x) ((x)<0?(epicsInt32)(x):(epicsInt32)(epicsUInt32)(x))`
  and `d2ui` similarly; applied in `BIT_OR/AND/XOR/NOT` and the shift opcodes.
- **Diverges:** C deliberately routes **positive** doubles through `epicsUInt32` first so that a
  value with bit 31 set (e.g. `3000000000.0`) becomes the *unsigned* bit pattern, then
  reinterpreted as signed `epicsInt32` (negative). Rust `3e9_f64 as i32` **saturates** to
  `i32::MAX` (`0x7FFFFFFF`). So `3000000000 & 0xFFFFFFFF`: C → `-1294967296`
  (`0xB2D05E00`); Rust → `2147483647` (`0x7FFFFFFF`). NaN/Inf also differ:
  C `(epicsInt32)NaN` is implementation-defined, Rust `NaN as i32` is `0`.
- **Impact:** Wrong computed value for every bitwise/shift operation on operands ≥ 2^31 or
  non-finite. Affects `&`, `|`, `XOR`, `~`, `<<`, `>>`, `>>>`. Same defect in `string.rs:204-228`
  and `array.rs:176-210`.

### H-6. String evaluator: `=`/`==` compares doubles with an epsilon; C uses exact `==`
- **Rust:** `calc/engine/string.rs:118` — `(StackValue::Double(x),Double(y)) => (x-y).abs() < 1e-11`
- **C:** `calcPerform.c:385-388` — `EQUAL: *ptop = *ptop == top;` (exact IEEE compare)
- **Diverges:** The numeric evaluator (`numeric.rs:81-83`) correctly does exact `a == b`. The
  string evaluator instead treats doubles within `1e-11` as equal. (synApps sCalc *does* use a
  fuzzy compare, so this is faithful to sCalc — but it diverges from epics-base CALC and from
  the Rust numeric path, so the same expression gives different truth values depending on which
  evaluator runs it.)
- **Impact:** `1e-12 == 0` is false under numeric CALC, true under string CALC. Inconsistent and
  non-C for any record routed through the string evaluator. `Ne` at `string.rs` mirrors it.

### H-7. String evaluator: division by zero yields NaN instead of IEEE Inf
- **Rust:** `calc/engine/string.rs:88-95` — `if b == 0.0 { push(NAN) } else { push(a/b) }`
- **C:** `calcPerform.c:157-160` — `DIV: top=*ptop--; *ptop /= top;` (IEEE: `1.0/0.0 = +Inf`,
  `-1.0/0.0 = -Inf`, `0.0/0.0 = NaN`)
- **Diverges:** The numeric evaluator (`numeric.rs:58-62`) correctly uses bare `a / b` (IEEE).
  The string evaluator forces **all** division by zero to NaN, losing the sign and turning
  `1/0` (C: `+Inf`) into NaN.
- **Impact:** Wrong computed value for any divide-by-zero in a string-CALC expression; also
  inconsistent with the numeric path.

---

## Medium

### M-1. `fmod()` uses Rust `%` on f64 — matches C `fmod` but argument order must be checked
- **Rust:** `calc/engine/numeric.rs:280-283` — `let (a,b)=pop2(); stack.push(a % b);`
- **C:** `calcPerform.c:262-265` — `FMOD: top=*ptop--; *ptop = fmod(*ptop, top);`
- **Diverges:** Rust `%` on `f64` is defined as `fmod` (truncated remainder), and `pop2`
  returns `(a,b)` with `a` deeper — equivalent to C `fmod(*ptop, top)`. This case is **correct**.
  Noted only because `atan2` (next) has the same shape but the args are intentionally swapped;
  do not "fix" `fmod` to match. No action needed.

### M-2. `atan2` argument order — verify against C's deliberate reversal
- **Rust:** `calc/engine/numeric.rs:276-279` — `let (a,b)=pop2(); stack.push(b.atan2(a));`
- **C:** `calcPerform.c:225-228` — `ATAN2: top=*ptop--; *ptop = atan2(top, *ptop); /* Args backwards! */`
- **Diverges:** C computes `atan2(top, *ptop)` where `top` is the **second** (shallower)
  argument and `*ptop` the first. So for infix `atan2(A,B)`: A pushed first (`*ptop`), B second
  (`top`), result `= atan2(B, A)`. Rust: `pop2` gives `a=A` (deep), `b=B` (top); pushes
  `b.atan2(a) = B.atan2(A) = atan2(B, A)`. **This matches C.** Verified correct — listed so a
  future reviewer does not "correct" it.

### M-3. `0x` hex literal accepts values up to 64-bit; C uses `epicsParseUInt32`
- **Rust:** `calc/engine/token.rs:390-393` — `u64::from_str_radix(s,16).map(|v| v as f64)`
- **C:** `postfix.c:283` — `epicsParseUInt32(psrc, &lit_ui, 0, &pnext)` (32-bit unsigned;
  out-of-range → `CALC_ERR_BAD_LITERAL`)
- **Diverges:** C rejects a hex literal that does not fit in `epicsUInt32`. Rust accepts up to
  `0xFFFFFFFFFFFFFFFF` and converts via `u64 as f64` (with rounding above 2^53).
- **Impact:** `0x1FFFFFFFF` compiles in Rust (≈ 8.59e9) but is a `CALC_ERR_BAD_LITERAL` in C.
  Edge-case error-code divergence.

### M-4. Decimal literals are not classified as int vs double; lose C's `LITERAL_INT` path
- **Rust:** `calc/engine/token.rs:396-397` & `postfix.rs:251-255` — every numeric literal is a
  single `f64` `Number`, emitted as `PushConst(f64)`.
- **C:** `postfix.c:259-291` — a `LITERAL_DOUBLE` element whose value equals its `epicsInt32`
  cast is re-encoded as `LITERAL_INT`; this is observable via `calcExprDump` and affects the
  exact stored byte stream.
- **Impact:** Behaviorally `calcPerform` treats both identically (`LITERAL_INT` is just widened
  to double on push), so *evaluation* results match. The divergence is only in the postfix
  byte-stream representation and disassembly. Low runtime impact, but any code comparing
  compiled postfix buffers byte-for-byte against epics-base will mismatch. Listed Medium because
  it also means a literal like `4294967296` (exact integer but > 2^31) takes the double path in
  both — consistent — so no value error.

### M-5. `calcErrorStr` equivalent / error-code numbering not exposed
- **Rust:** `calc/engine/error.rs` — `CalcError` is an enum with `Display`, but there is no
  mapping to the **numeric** `CALC_ERR_*` codes (0–13) nor a `calcErrorStr`-compatible function.
- **C:** `postfix.c:515-538` — `calcErrorStr` maps the integer codes; `postfix.h:83-109` fixes
  the numbering.
- **Impact:** Any consumer expecting the epics-base integer error codes (records storing
  `perror`, IOC shell output) cannot get them. Also the Rust enum has 22 variants vs C's 14
  codes, several with no C equivalent. Feature/representation gap; Medium because record-level
  code that reports `perror` will diverge.

### M-6. `cond_search` nesting semantics differ from C's `count`-based scan
- **Rust:** `calc/engine/numeric.rs:355-384` — depth counter: `CondIf` → `depth+=1`;
  `CondElse` matches only at `depth==0`; `CondEnd` matches at `depth==0` else `depth-=1`.
- **C:** `calcPerform.c:527-557` — `cond_search` uses `count` starting at 1; `op==match`
  decrements `count` (return when 0); `COND_IF` increments `count`. Note C's scan **only**
  special-cases `COND_IF` for nesting; it does not track `COND_ELSE`/`COND_END` pairing the way
  the Rust depth model does.
- **Diverges:** For a forward jump from a *false* `COND_IF` searching for its `COND_ELSE`,
  C counts every nested `COND_IF` (+1) and every occurrence of the *target* opcode (−1). The
  Rust model instead pairs `CondEnd` against `CondIf` for depth. For well-formed nested
  ternaries the two usually coincide, but they are not provably identical for all inputs (e.g.
  a nested ternary inside the *true* branch being skipped). Given the compiler emits balanced
  `CondIf/CondElse/CondEnd` triples this is likely benign, but it is an unverified semantic
  divergence and should have a targeted nested-ternary regression test
  (`a?(b?c:d):(e?f:g)` with each branch selected).
- **Impact:** Potential wrong branch selection for deeply nested conditionals; unproven.

---

## Low

### L-1. Operator-table comment in `postfix.rs` cites synApps priorities, not epics-base
- **Rust:** `calc/engine/postfix.rs:7-14` — comment "matching sCalcPostfix.c" with priority
  levels 2–10.
- **C (epics-base):** `postfix.c:147-180` uses priorities 0–8.
- **Impact:** Cosmetic/documentation. The *relative* ordering (|| < && < cmp < +/- < */% <
  power < unary) is preserved, so compiled output is equivalent for the epics-base operator
  subset. No runtime impact; flagged so the comment is not mistaken for an epics-base reference.

### L-2. `calcArgUsage` not implemented
- **C:** `calcPerform.c:429-507` — `calcArgUsage` returns input/store bitmaps.
- **Rust:** no equivalent in `calc/`.
- **Impact:** Feature gap. The calc/calcout record support needs this to know which `INPx`
  links must be read. If record code re-derives usage elsewhere this is moot; otherwise it is a
  missing API. Low here because it is outside the two cited C files' core eval path, but it is a
  documented part of the CALC engine (`postfix.h:341-366`).

### L-3. `calcExprDump` (disassembler) not implemented
- **C:** `postfix.c:541-654`.
- **Rust:** none.
- **Impact:** Feature gap, diagnostics only. Low.

### L-4. `simple_random()` is an LCG, not C `rand()`/`RAND_MAX`; `>= 1.0` possible? — verify range
- **Rust:** `calc/engine/numeric.rs:386-402` — returns `((s>>11) as f64)/2^53 + f64::MIN_POSITIVE`.
- **C:** `calcPerform.c:509-521` — `(double)rand()/RAND_MAX`, range `[0,1]` inclusive.
- **Diverges:** Sequence is necessarily different (acceptable — RNG). Range: C can return
  exactly `0.0` and exactly `1.0`. Rust adds `f64::MIN_POSITIVE` so it is never exactly `0.0`,
  and the max is `(2^53-1)/2^53 + MIN_POSITIVE < 1.0`, so Rust returns a half-open-ish
  `(0, 1)`. Minor distribution/endpoint divergence; documented CALC contract is "between 0 and
  1" so both comply. Low.

### L-5. `FetchVal` semantics differ between evaluators and from C `FETCH_VAL`
- **Rust:** `calc/engine/numeric.rs:34-37` — `FetchVal` pushes a *copy of the current stack top*
  (`stack.last().unwrap_or(0.0)`). `string.rs:40-44` — `FetchVal` is aliased to `NormalRandom`
  and pushes a Gaussian random number.
- **C:** `calcPerform.c:74-76` — `FETCH_VAL: *++ptop = *presult;` pushes the **previous result**
  (the `VAL` field passed in via `presult`).
- **Diverges:** C `VAL` reads the record's previous VAL. The Rust numeric evaluator has no
  `presult` input (`eval` takes only `NumericInputs`), so `VAL` is approximated as "duplicate
  top of stack" — wrong whenever `VAL` is used as a genuine input. The string evaluator's alias
  to a normal-random generator is outright incorrect for `VAL`.
- **Impact:** `VAL` in an expression does not return the previous calculation result. For the
  numeric path the value is wrong (and underflow-prone if `VAL` is the first token: it pushes
  `0.0`). Marked Low only because epics-rs record-level code may supply `VAL` as a regular
  `Var`/argument and never emit `FetchVal`; if it does emit `FetchVal`, escalate to High.

---

## Math extensions (`calc/math/`) — internal-correctness notes (no C reference)

These are synApps-aCalc-style helpers with no epics-base equivalent; reviewed only for obvious
internal bugs per scope.

- `math/stats.rs:21-64` `fwhm` — uses `half_max = (max+min)/2` (midpoint of max and global
  min), not `max/2`. Genuine FWHM is half of the *maximum*. For a peak sitting on a non-zero
  baseline this is a defensible definition, but it is not "full width at *half maximum*"; if
  parity with aCalc `FWHM` is intended, verify aCalc's definition. Not a crash.
- `math/stats.rs:43-51` `fwhm` left-crossing — `data[i+1]-data[i]` can be `0` (flat region) →
  division producing `inf`/`NaN` in `frac`. Edge case, no panic.
- `math/interp.rs:79-90` `find_closest` indexes `x_data[0]` without an empty-slice guard, but
  every caller (`poly_interp`) checks `n==0` first, so not reachable. OK.
- `math/fitting.rs:26-86` `fitpoly` — Cramer's-rule 3x3 solve with `1e-30` singular cutoff;
  numerically weak for large `x` (normal-equations conditioning) but functionally correct for
  the test ranges. No bug.
- `math/derivative.rs`, `math/stats.rs` smoothing — boundary handling sets edges to `0.0`;
  internally consistent. No bug.
- `checksum.rs` — `crc16` (0xA001), `lrc`, `xor8` verified against the in-file known-answer
  tests (`crc16("123456789")==0x4B37`). Correct.

---

## Notes for the maintainer

The single most important epics-base-parity gap is the **missing runtime-depth validation**
(C-1 + H-1): C validates expression well-formedness both at compile time (`runtime_depth != 1`
→ `CALC_ERR_INCOMPLETE`, `> 1` at `;` → `CALC_ERR_TOOMANY`) and again at runtime
(`ptop != stack+1` → error). The Rust port has neither the compile-time final-depth check nor
the runtime check, so malformed-but-not-caught postfix produces silent wrong values instead of
errors, and `CalcError::TooMany` is dead code.

The integer-conversion family (H-3, H-4, H-5) is one structural defect — the port uses native
Rust `as iN`/`as uN` casts (saturating, NaN→0) wherever C uses the carefully-crafted `d2i`/
`d2ui` macros and `epicsInt32` truncation. Every site (`numeric.rs`, `string.rs`, `array.rs`
for `Mod`, `Nint`, and all bitwise/shift ops) needs the same `d2i`/`d2ui` helper to match C's
wrap-on-overflow 32-bit semantics.
