# epics-oracle-rs — differential oracle harness

Boots the compiled C `softIoc` and the Rust IOC **on the same `.db`**, drives
both with **identical CA operations**, and diffs only what a client can observe.

It exists to make "clean" a measurement instead of an opinion. See
`doc/strategy-2026-07-13.md` §3.2 for the rationale: nineteen audit rounds of
reading C and Rust side by side never converged, because there was no
denominator and "verified clean" verdicts kept turning out false.

## Shape

```
softIoc.dbd ──► dbd.rs ──► surface.rs ──► THE DENOMINATOR
  (spec)        (parse)    (enumerate)    record types × CA-observable fields
                                │
                                ▼
                          cases.rs  boundary values, per DBF type
                                │
             ┌──────────────────┴──────────────────┐
             ▼                                     ▼
      C softIoc  (ground truth)            Rust IOC (oracle-ioc)
             └──────────────┬──────────────────────┘
                            ▼
                   the SAME C CA tools
              caget / caput / cainfo / camonitor
                            │
                            ▼
                   diff.rs ──► allowlist.rs ──► report.rs
                                                 JSON + human + reproducer
```

**Both sides are driven by the C client tools.** That is the core of the method.
It keeps `epics-ca-rs`'s *client* out of the experiment (a client bug would
otherwise show up as a server "diff"), and it measures the contract actually
owed: Tier 1 says *a C client must not be able to tell the difference*, so the
honest experiment puts a real C client in front of both.

## The four verdicts

| verdict | meaning |
|---|---|
| **AGREED** | both sides produced a reading, and they match |
| **EXPECTED DEVIATION** | they differ, and a NOT-REPRODUCED entry in `doc/upstream-c-bugs.md` justifies it (the port deliberately refuses to reproduce a C bug) |
| **DEFECT** | they differ and nothing justifies it |
| **ERROR** | no reading was obtained — IOC would not boot, PV never connected, tool timed out. **Never scored as agreement.** |

`ran == agreed + expected_deviation + defect + errored`, asserted by
`Counts::check()`. The binary exits non-zero on any DEFECT **or any ERROR**: a
run that could not look is not a pass.

The allowlist is a data file (`allowlist/expected-deviations.toml`) citing CBUG
ids, not inline code. A row that **stops firing** is reported as STALE — the
deviation vanished, which is either a port regression or an upstream fix. That
makes the harness and the catalogue check each other.

## Running

```sh
cargo build -p epics-oracle-rs
ORACLE_IOC_BIN=target/debug/oracle-ioc \
  cargo run -p epics-oracle-rs --bin oracle -- --phase all --json out.json

# one record type, one phase
cargo run -p epics-oracle-rs --bin oracle -- --phase monitor --record-types calc
```

Needs the built C tree (default `/home/stevek/work/epics-base/bin/linux-x86_64`,
override with `EPICS_BASE_BIN`). If it is absent the harness **fails loudly**
rather than skipping — a silently skipped oracle is the false-clean we are
escaping.

## Port discipline (this has bitten the repo before)

Every port is taken **by binding**, never by hard-coding and never by
probe-then-rebind.

- **Rust side:** true bind-`:0`-and-read-back. `CaServer::from_parts(db, 0, ..)`
  binds the sockets and *then* reports the port it got; `oracle-ioc` prints it as
  `ORACLE_IOC_PORT <n>` and the harness reads it back.
- **C side:** `softIoc` takes its port from the environment and cannot inherit a
  pre-bound fd, so bind-read-back is **not available**. The substitute is
  allocate-then-**verify**: find a number free on *both* UDP and TCP by binding
  it, then scan the IOC's own startup output and treat any bind complaint as a
  boot failure to retry on a fresh port.

That verification is load-bearing. Booting `softIoc` on a taken port does **not**
fail — it prints `cas WARNING: Configured TCP port was unavailable ... two or
more servers share the same UDP port` and *keeps serving*. A `caget` aimed at one
IOC can then be answered by the other, and the harness would score a diff, or an
agreement, against the wrong server.

## What it measures today, and what it does not

Stated plainly, because an inflated coverage number is the failure this harness
was built to end.

**Measured**
- native DBF type, element count, access rights (`cainfo`)
- value in both string and numeric form (`caget` vs `caget -n`) — enum/menu
  fields compare their **strings**, so "right ordinal, wrong label" is caught
- put accept/reject, and the rejection *reason* when both sides refuse
- `STAT`/`SEVR` after each put
- **monitor event sequence and count** (`camonitor` over a fixed window)

**Not yet measured — clean seams, not silent gaps**
- **Array/waveform put-and-readback** (`NORD`/`NELM`, 0-length and over-NELM).
  `cases::array_cases` and `CaTools::caget_array`/`caput_array` exist and are
  unit-tested; nothing calls them yet, because array cases need a `.db` that
  declares `NELM`/`FTVL` per instance rather than the empty `record(t, "N") {}`
  the generic generator emits. That is the one seam to pick up next.
- **Multi-put sequences into one record.** The put probe drives exactly one put
  per record instance (that is what makes each case isolated and its reproducer
  minimal). CBUG-E1 needs *three* successive puts into one compress record, so
  the harness cannot fire it and correctly reports the row STALE.
- **calc-expression cases.** The CBUG-A*/C*/F1..F5 entries live inside the calc
  engines and need a generator that drives `CALC` expressions, not field
  boundaries. Until that exists those rows are deliberately absent from the
  allowlist rather than present-and-never-firing.
- **PVA.** CA only.

Coverage is reported as a percentage of the `.dbd`-derived denominator
(record types the port implements × their CA-observable fields). `DBF_NOACCESS`
fields are excluded from that denominator and counted separately — they are raw C
pointers in the record struct and no CA client can reach them, so counting them
would inflate the denominator while measuring nothing.
