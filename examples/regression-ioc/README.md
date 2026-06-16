# regression-ioc

An in-process **regression IOC** that pins recurring bug-fix behaviors observed
across `v0.15.x`–`v0.20.x`. The early minor releases after each major bump
carried many fixes, and the same *families* of bug kept recurring as commits
landed. This crate boots a real IOC — CA + PVA servers over one shared
`PvDatabase` — and asserts the fixed behavior **over the wire**, so a future
regression in the same family fails a test instead of shipping.

## Recurring families pinned

| Family | What it pins | Records / tests |
|--------|--------------|-----------------|
| A | processing-chain ordering: FLNK runs on a caput; calcout OUT-link writes | `REG:A:*` → `a_*` |
| B | monitor posts **only on change** (a no-op re-put posts nothing) | `REG:B:BO` → `b_monitor_posts_only_on_change` |
| C | periodic `SCAN` runs while a server is up | `REG:C:*` → `c_periodic_scan_runs` |
| D | a caput to a `SCAN=Passive` motor's VAL drives the move (the v0.20.0 regression) | `REG:D:MTR` → `d_motor_*` |
| E | enum served as `DBR_ENUM` with choice labels (CA index + PVA NTEnum choices) | `REG:E:MBBO` → `e_enum_*` |
| F | a `DBF_MENU` record field (`.SCAN`) served as `DBR_ENUM` | `f_menu_field_served_as_dbr_enum` |
| G | alarm severity raised on a limit violation | `REG:G:AI` → `g_alarm_severity_on_limit_violation` |
| H | timestamp advances on each process (nsec `WallTime`) | `REG:H:AI` → `h_timestamp_advances_on_process` |
| I | monitor event-**mask** routing: a sub-MDEL change posts `DBE_LOG` only, so a `DBE_VALUE`-only subscriber must not see it (a `DBE_LOG` one must); a supra-MDEL change posts `DBE_VALUE` | `REG:I:AI` → `i_deadband_routes_value_vs_log_event_masks` |
| J | a stringout VAL round-trips a full-width (39-char) `DBR_STRING` byte-for-byte (no truncation/reordering); non-UTF8 + `$` long-string paths are CLI-scoped | `REG:J:SO` → `j_stringout_dbr_string_round_trip` |
| K | a `caput REC.PROC 0` forces a process (was a silent no-op for 0); put-with-callback returns only after processing completes | `REG:K:*` → `k_proc_zero_forces_process_and_put_completes` |
| M | a metadata-field write (`HOPR`) posts `DBE_PROPERTY` to a `DBE_PROPERTY` subscriber but **not** to a `DBE_VALUE`-only one (the `DBE_PROPERTY` axis of Family I) | `REG:M:AI` → `m_metadata_change_posts_dbe_property` |
| N | an `MS` (maximize-severity) input link propagates the **source** record's severity into the reader (`recGblInheritSevrMsg`), distinct from G's own-limit severity | `REG:N:*` → `n_ms_link_propagates_source_severity` |
| O | a record seeded nonzero (MLST/ALST seeded at init) must not post a duplicate VAL on an idempotent reprocess; a real change still posts (process-path no-change, vs B's write-path) | `REG:O:LO` → `o_seeded_record_suppresses_duplicate_post` |
| P | an array (waveform) channel exposes its VAL display **and** control limit metadata over `DBR_GR`/`DBR_CTRL` (EGU/PREC/HOPR/LOPR + HOPR/LOPR control limits), not a collapsed `[0,0]` range | `REG:P:WF` → `p_array_exposes_gr_and_ctrl_limit_metadata` |
| R | a **record-specific** `DBF_MENU` field (dfanout `SELM`) is served as `DBR_ENUM` by index over CA and as an NTEnum carrying that record's own choice labels over PVA (the per-record menu branch, vs F's shared `.SCAN`) | `REG:R:DF` → `r_record_specific_menu_field_served_as_dbr_enum` |
| L | an unsigned `DBF_ULONG` field (mbbo `MASK`, seeded `0x80000000`) is served over PVA with its native **unsigned** `uint` wire type, not sign-collapsed to `int` (PVA-only — CA has no unsigned wire types) | `REG:L:MBBO` → `l_unsigned_mask_field_keeps_native_pva_uint_type` |
| Q | a record's `UTAG` (DBF_UINT64, set in the db) reaches the PVA NTScalar `timeStamp.userTag`, not a constant 0 (PVA-only — CA has no userTag wire slot) | `REG:Q:AI` → `q_record_utag_served_as_pva_usertag` |

Family C is pinned both by the periodic-scan test and by the motor's
`io_intr_scan_independent` readback path (the actual v0.20.0 mechanism).

The records live in [`db/regression.db`](db/regression.db); the harness is
[`src/lib.rs`](src/lib.rs); the assertions are in [`tests/`](tests/).

## Run the tests

```sh
cargo nextest run -p regression-ioc            # all families, ~1s
cargo nextest run -p regression-ioc --retries 2  # absorb timing flakes under load
```

Each test boots its own IOC on free loopback ports (CA + ephemeral PVA) and
drives it with the real `CaClient` / `PvaClient`. CA tests are `#[serial]`
because the `EPICS_CA_*` client env is process-global under `cargo test`.

## Run the IOC by hand

```sh
cargo run -p regression-ioc --bin regression_ioc
# prints the CA + PVA ports; then, against those ports:
#   caget REG:E:MBBO   pvget REG:D:MTR   caput REG:D:MTR 5.0   ...
```

## CI

These tests run **automatically on every push** — no opt-in needed:

- `rust.yml` → `cargo nextest run --workspace` (Linux, default profile)
- `cross-platform.yml` → `cargo nextest run --workspace --profile ci` across the
  Windows / macOS / Linux × x64 / arm matrix; the `ci` profile retries
  timing-flaky network tests twice.

`examples/*` are workspace members, so `--workspace` picks this crate up. The
suite needs no external C tools, so it is **not** gated behind
`--profile interop` (that profile is for the pvxs / C-EPICS interop suites only).

## Not covered here

The caput-by-label match ("match ENUM value against the menu before numeric
index") lives entirely in the `caput` CLI (`crates/epics-ca-rs/src/bin/caput-rs.rs`)
and has no library-client equivalent, so this in-process harness cannot exercise
it; it is covered by caput-rs's own tests. See the note in `tests/families.rs`.
