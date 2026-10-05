# ioc-spawn-gate

Dev-only. Derives from the sources which nextest binaries spawn an EPICS IOC,
and fails if one of them is not declared in a test-group. `publish = false`,
not a dependency of anything — it owns one test and one census binary.

## The rule

> A test binary whose sources spawn an EPICS IOC process — C's `softIoc` /
> `softIocPVX`, or one of our own IOC binaries — must be a member of a declared
> nextest test-group, so its scheduling is a decision somebody made in
> `.config/nextest.toml` rather than a default nobody noticed.

Membership is **derived**, never remembered. The four binary names
`ca_softioc` used to carry were maintained by whoever thought of it:
`epics-oracle-rs` spawns three kinds of IOC across eighty-odd tests and was
never in the list, and neither were `interop_pvxs` nor the six
`epics-ca-rs` / `epics-bridge-rs` / `qsrv-ioc` binaries that spawn
`softioc-rs`. Nothing went red when they joined — they occasionally timed out
under load, which reads as flakiness rather than as a missing declaration. The
gate turns that into a red test at the moment the binary joins.

The IOC token set is derived the same way. One token is upstream and fixed —
`softioc`, which covers `softIocPVX` too, because a token matches as a
substring of the lower-cased line. The rest come from the workspace's own
`[[bin]]` targets: any bin whose name ends in `ioc`, `ioc-rs` or `ioc_rs`
counts, so a new `foo-ioc` binary joins the token set when its target exists,
with nobody remembering this file.

## Why a group and not one cap

"Spawns an IOC" and "needs serialising" are different claims, and only the
first is derivable from source. Forcing every spawner into `max-threads = 1`
was measured on `epics-oracle-rs` and is worse, not better — the numbers are
recorded beside the filters in `.config/nextest.toml`. So the gate insists that
somebody *decided*: `ca_softioc` (serialised) or `ioc_unthrottled` (measured to
need no cap). It never insists the cap be applied.

## Order matters

nextest resolves per-test overrides first-match-wins, so a declaration in a
later block is not the effective one. The gate models that, and reports a block
it cannot evaluate only when that block could shadow a declaration —
`package(ad-plugins-rs) and test(file_magick::)` is unevaluable but can never
claim a binary outside `ad-plugins-rs`, so it shadows nothing.

## Running

```bash
cargo run -p ioc-spawn-gate --bin ioc-spawn-census   # what the sources say
cargo nextest run -p ioc-spawn-gate                  # the gate itself
```

29 tests. `every_ioc_spawning_binary_is_declared_in_a_test_group` is the gate;
the rest pin the scan's boundaries — what counts as a spawn, how the mod graph
is followed (the gate reads module structure, not call sites), and which
filter blocks can shadow which.

## License

EPICS Open License (workspace `LICENSE`).
