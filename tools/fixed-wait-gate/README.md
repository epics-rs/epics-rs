# fixed-wait-gate

Dev-only. Fails a change that adds a fixed wait to a test. `publish = false`;
it reads source text and git history.

Test code is every Rust file under a `tests/` directory, and under `src/` the
lines `source_guard::test_lines` marks (a `#[cfg(test)]` item, a function under
`#[test]`, `#[tokio::test]` or `#[epics_test]`) plus every file that only such
items declare as a module (`#[cfg(test)] mod tests;`, a `#[path]` to
`send_buf_tests.rs`). A file production code also declares ships, and is not.

## The rule

> A test bounds a wait for something to happen by `fact_budget::FACT_BUDGET`
> (a timeout of it, or a loop to `Instant::now() + FACT_BUDGET`), and closes a
> claim that something did not happen on `fact_budget::barrier::until` /
> `until_async`. It does not sleep a fixed time or bound a wait by a literal
> duration.

A fixed wait bets on how fast the runner is, and a correct test fails on a slow
one: c47da8c0e slept 800 ms in `crates/motor-rs/tests/dol_at_init.rs` until
5b169d3e7 replaced the sleep with a wait for the fact.

The gate counts as a fixed wait a call to `sleep` or `sleep_*`; a call whose
name has a `timeout`, `deadline`, `wait` or `until` segment and takes a
`Duration::from_*(<number>)`; `now() + Duration::from_*(<number>)`, which
`deadline_from_now(d)` and `deadline_after(now(), d)` are too; and,
inside a `loop` or `while`, `elapsed()` compared with such a duration.
Comments and string literals are skipped.

A sleep or a timeout on each attempt inside a `loop` or `while` body is its
poll interval: the loop's bound decides the test, so the bound is what is
checked, and a timeout in a `while` head is part of that bound. A timeout in
the body whose expiry leaves the loop is that bound, and counts: its `Err` arm,
`else` or `is_err()` branch breaks, returns or continues an outer loop, or
the result goes to `?`, `.expect`, `.unwrap`, a `panic!`, an `assert!` that
fails on it (`assert!(r.is_ok())`, `assert!(matches!(r, Ok(_)))`) or a helper
this file defines that does. A sleep that is a `select!` arm's future is a
timeout on the other arms, read by the same rule: its arm's body is where its
expiry goes, and it counts only with a literal duration. One whose expiry
only retries (`Err(_) => continue`, `let _ =`, an `if let Ok(..)` with no
`else`) is the interval and does not. A setter (`set_read_timeout`) and std's
`Condvar::wait_timeout*`, whose result does not say the time ran out, are
never the bound. A `for` loop's bound is a count, so its sleeps and timeouts
count: when the body breaks, returns or continues an outer loop it is a poll
bounded by count x interval, and when it never does, its sleep is pacing
between stimuli.

## Why a ratchet

Test code held 2823 fixed waits at 451531667 (1339 under `tests/`, 1484
under `src/`). The gate compares the waits the changed files' test code holds
at the merge base with what it holds now, matched
by callee and duration, so moving or re-indenting one passes, and a new one or
a new number fails.

A window whose only job is to prove nothing happens may stay when its line, or
the comment directly above it, says why:

```rust
// absence-window: the refused put must produce no monitor update
tokio::time::sleep(Duration::from_millis(200)).await;
```

A duration that waits for nothing, such as a value under test or pacing
between stimuli, may stay marked the same way:

```rust
let client = PvaClient::builder()
    .timeout(Duration::from_secs_f64(1e19)) // not-a-wait: the unbounded timeout is under test
    .build();
```

`not-a-wait` does not keep the sleep or timeout of a `for` loop that breaks or
returns, or a timeout whose expiry ends its loop: both wait for something.
Both marks need a reason.

## What it does not see

A duration held in a named constant (`timeout(SHORT, ..)`), a wait helper whose
name has none of the four segments, a deadline built from a variable start
(`t0 + Duration::from_secs(5)`, `deadline_after(t0, ..)`), and an `elapsed()`
bound outside a loop, which is an assertion rather than a wait.

A timeout's result is followed through three `let` bindings and same-file
helpers in all. A helper in another file is not followed: finding it needs name
resolution across files, and a ratchet that rescans only the changed files
would not rescan a loop whose helper alone changed. Nor is a method on
anything but `self`, whose type the text does not say, a path call
(`util::check(r)`), a macro other than `assert!` and `matches!`, or a helper
that turns the expiry into a value (`fn got(r) -> bool`) for the caller to
decide on.

## Running

```bash
cargo run -p fixed-wait-gate -- origin/main   # what CI runs, with the PR base
cargo nextest run -p fixed-wait-gate          # the gate itself
```

## License

EPICS Open License (workspace `LICENSE`).
