# fact-budget

Test-only. `FACT_BUDGET`, the one budget a test may put on a fact it expects
to happen, and `barrier`, the way a test claims that something did *not*
happen. It ports no C module.

Never published (`publish = false`). `epics-base-rs`, `epics-ca-rs`,
`epics-pva-rs` and `epics-seq-rs` take it as a `path`-only dev-dependency,
which `cargo package` drops from the uploaded manifest; the unpublished test
helpers `raw-ca` and `c-ca-tools` take it as a plain one. No feature flags.

## `FACT_BUDGET`

A per-call `Duration::from_secs(5)` in a test is not a bound on the code under
test; it is a bet on how much CPU the box gives the process while the rest of
the suite runs. `FACT_BUDGET` is 60 s, derived rather than chosen: half of the
shortest nextest kill in `.config/nextest.toml` (`slow-timeout` 30 s ×
`terminate-after` 4 on `profile.default`), so a test that really fails still
prints its own message before the SIGTERM. It is not a performance assertion;
a latency is asserted on a measured elapsed time.

## `barrier`: absence

`sleep(250 ms); assert!(nothing_arrived())` can only fail flakily: bring back
the behaviour it denies and it still passes whenever the peer is slower than
the window. An absence claim closes instead on an observation causally ordered
after the denied event. On a CA circuit a `CA_PROTO_READ_NOTIFY` round trip is
one: one TCP stream and one server-side message thread deliver every frame the
server was going to send earlier before its reply.

```rust
// crates/epics-ca-rs/tests/absence_barrier.rs
let mut src = vec!["hello", "world", "PROBE"].into_iter();
let seen = fact_budget::barrier::until(
    "clean",
    |s: &&str| *s == "DENIED", // must not arrive before the barrier
    |s: &&str| *s == "PROBE",  // the barrier
    |_| src.next(),            // the next observation, given the time left
);
assert_eq!(seen, vec!["hello", "world", "PROBE"]);
```

`until` fails if a denied item arrives before the barrier, and fails if the
source ends or `FACT_BUDGET` runs out first, since an absence claim with no
barrier proves nothing. `until_async` is the same for a source read by an async
call. No caller passes a duration.

## Not covered

A duration that is the evidence rather than a bound on it keeps its own number:
a per-attempt bound inside a retry loop whose expiry retries rather than fails,
a proxy's per-datagram forwarding bound, and an assertion whose subject is an
elapsed time.

## Testing

The crate has no tests of its own; `barrier`'s falsifiability cases live in
`epics-ca-rs`:

```bash
cargo nextest run -p epics-ca-rs --test absence_barrier
```

## License

[EPICS Open License](LICENSE)
