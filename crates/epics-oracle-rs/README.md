# epics-oracle-rs — differential oracle harness

Boots the compiled C IOC and the Rust IOC **on the same `.db`**, drives both
with **identical client operations**, and diffs only what a client can observe.

Ground truth is the **fat** C IOC built under `oracle-ioc/`, not base's stock
`softIoc`: it serves base's 34 record types plus `busy`/`transform`/`sseq`/
`acalcout`/`scalcout`/`asyn`, and its expanded dbd is what supplies the
denominator. There are two lanes — CA against `softIoc`, and PVA against pvxs
QSRV2 in `softIocPVX`.

It exists to make "clean" a measurement instead of an opinion. It replaced
reading C and Rust side by side, which ran nineteen rounds without converging:
the per-round finding count stayed flat in the 22–54 band and then rose to 114
once the auditor pool widened — the signature of a process bounded by how much
surface it had looked at, not by how many defects were left — and Round 18
reopened three entries previously recorded as *verified clean*. More rounds
answer neither symptom, because reading has no denominator. The `.dbd` is one,
which is why coverage below is a percentage rather than an impression.

## Shape

```
oracle-ioc/dbd/softIoc.dbd ──► dbd.rs ──► surface.rs ──► THE DENOMINATOR
     (the fat spec)           (parse)   (enumerate)   record types × fields
                                │
                                ▼
                          cases.rs  boundary values, per DBF type
                                │
             ┌──────────────────┴──────────────────┐
             ▼                                     ▼
      C softIoc  (ground truth)            Rust IOC (oracle-ioc)
             └──────────────┬──────────────────────┘
                            ▼
                the SAME reference client tools
        CA:  caget / caput / cainfo / camonitor
        PVA: pvxget / pvxinfo / pvxmonitor  (separate phases)
                            │
                            ▼
                   diff.rs ──► allowlist.rs ──► report.rs
                                                 JSON + human + reproducer
```

**Both sides are driven by the reference implementation's client tools** — base's
C tools on the CA lane, pvxs's on the PVA lane. That is the core of the method.
It keeps `epics-ca-rs`'s *client* out of the experiment (a client bug would
otherwise show up as a server "diff"), and it measures the contract actually
owed: Tier 1 says *a C client must not be able to tell the difference*, so the
honest experiment puts a real C client in front of both.

## The four verdicts

| verdict | meaning |
|---|---|
| **AGREED** | both sides produced a reading, and they match |
| **EXPECTED DEVIATION** | they differ, and an enabled allowlist row justifies it. Four buckets, three justification bases: `NOT-REPRODUCED` and `REPRODUCED` must name the `CBUG-…` id of the upstream C defect they refuse to reproduce, and say in their own `why` what C does and why it is wrong; `DESIGN-DIVERGENCE` and `INSTRUMENT-SUPERSET` are justified by their own `why` and are exempt from that citation rule. All four match, fire and go stale identically |
| **DEFECT** | they differ and nothing justifies it |
| **ERROR** | no reading was obtained — IOC would not boot, PV never connected, tool timed out. **Never scored as agreement.** |

`ran == agreed + expected_deviation + defect + errored`, asserted by
`Counts::check()`. The run verdict is `report::run_failures`, and the binary's
exit code is that same judgement — non-zero on any DEFECT, **any ERROR**, any
record type the port did not implement, and any STALE allowlist row. A run that
could not look is not a pass, and neither is one that did not look everywhere.

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

Needs **two** C trees, and `CTools::discover` fails loudly if either is missing:

- base's built tree for the client tools — default
  `/home/stevek/work/epics-base/bin/linux-x86_64`, override `EPICS_BASE_BIN`.
- the fat ground-truth IOC — default
  `/home/stevek/work/oracle-ioc/bin/linux-x86_64/softIoc`, override
  `EPICS_ORACLE_IOC_BIN`. `--dbd` defaults to that same tree's expanded dbd.

The PVA phases additionally need the pvxs tree (`PVXS_BIN`) and the fat
`softIocPVX` beside the fat `softIoc`. Nothing is ever skipped when a
prerequisite is absent — a silently skipped oracle is the false-clean we are
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
- **array/waveform put-and-readback** across the element-count boundaries
  (zero-length, single, partial, exactly `NELM`, one past `NELM`), comparing the
  payload, the returned element count and `NORD`. `record_stmt_fields` gives the
  reproducer its own `NELM`/`FTVL`, which is what the phase was waiting on.

**Not yet measured — clean seams, not silent gaps**
- **Multi-put sequences into one record.** The put probe drives exactly one put
  per record instance (that is what makes each case isolated and its reproducer
  minimal). CBUG-E1 needs *three* successive puts into one compress record, so
  the harness cannot fire it. The row reports **UNEXERCISED, not STALE**, and
  that distinction now decides the exit code: `compress.VAL` is `DBF_NOACCESS`
  so the scalar phases never enumerate it, and `compress` declares `NSAM` rather
  than `NELM`/`FTVL` so the array phase skips it too. No case in the row's scope
  ever runs, which is coverage rather than a finding — a STALE row would fail
  every run.
- **calc-expression cases.** The CBUG-A*/C*/F1..F5 entries live inside the calc
  engines and need a generator that drives `CALC` expressions, not field
  boundaries. Until that exists those rows are deliberately absent from the
  allowlist rather than present-and-never-firing.
- **Multi-value PVA puts and the PVA allowlist.** The two PVA phases
  (`--phase pva-read`, `--phase pva-monitor`) are real and measured, but sit
  outside `--phase all` on purpose: different ground truth (`softIocPVX`), a
  different instrument (`pvxget`/`pvxinfo`/`pvxmonitor`), and no CA allowlist,
  since every `CBUG-…` row records C `softIoc`'s CA-side behaviour and justifies
  nothing about QSRV2. Folding them into `all` would merge two populations whose
  verdicts are not comparable into one set of counts.

## What the first run measured

The numbers below are from the 2026-07-13 run and **predate the harness fixes
of 2026-08-22** (measurement failures that were being scored as readings, and a
field-coverage fraction that counted monitor cases). They stand as the last
measurement taken, not as the current state; they are owed a re-run.

`FINDINGS.md` carries the numbers and every reproducer. In short: the
denominator is **2551 CA-observable fields across 34 record types**, of which
**2462 (96.5 %)** produced a reading on both sides and were diffed. 89 fields
ERRORED — every one of them because the port does not serve a field C does.

Coverage is reported as a percentage of the `.dbd`-derived denominator: **every**
record type in the dbd × its CA-observable fields. A type the port cannot load
does not shrink the denominator — its fields are exactly the ones that went
unmeasured, so removing them would hide the gap. `DBF_NOACCESS`
fields are excluded from that denominator and counted separately — they are raw C
pointers in the record struct and no CA client can reach them, so counting them
would inflate the denominator while measuring nothing.
