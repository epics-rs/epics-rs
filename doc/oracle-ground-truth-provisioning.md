# Provisioning the oracle's C ground truth on a second host

`crates/epics-oracle-rs` has 14 integration tests that refuse to run without a
real C EPICS tree — by design. They fail loudly rather than skipping, because
"exit 0 because I could not look" is exactly the false-clean the crate exists to
escape (`tests/oracle.rs` header, `tests/pva_oracle.rs` header).

That means a fresh host reports **14 failures** until the C side is provisioned.
This file states exactly what those 14 need, so a second box can be brought up
without reverse-engineering the harness again. Measured on the PREEMPT_RT box
(`192.168.2.129`, Ubuntu 26.04, glibc 2.43, gcc 15.2.0, 12 cores) on
2026-07-24.

## The five paths the harness resolves

All five have an env override; none is invented if absent.

| Env var | Resolved by | Fallback constant |
| --- | --- | --- |
| `EPICS_BASE_BIN` | `CTools::discover` | `/home/stevek/work/epics-base/bin/linux-x86_64` |
| `EPICS_ORACLE_IOC_BIN` | `CTools::discover` | `/home/stevek/work/oracle-ioc/bin/linux-x86_64/softIoc` |
| `EPICS_ORACLE_DBD` | `CTools::dbd_path` | `/home/stevek/work/oracle-ioc/dbd/softIoc.dbd` |
| `PVXS_BIN` | `PvxTools::discover` | `/home/stevek/work/epics-modules/pvxs/bin/linux-x86_64` |
| `EPICS_ORACLE_PVX_IOC_BIN` | `PvxTools::discover` | `/home/stevek/work/oracle-ioc/bin/linux-x86_64/softIocPVX` |

`EPICS_BASE_BIN` must contain `caget`, `caput`, `cainfo`, `camonitor`;
`PVXS_BIN` must contain `pvxget`, `pvxinfo`, `pvxlist`, `pvxmonitor`, `pvxput`
(`PvxTools::REQUIRED`). A missing binary is one loud error at discovery, not an
ERRORED verdict per case.

## Full sweep vs. the 14 tests: two different C trees

The **binary** (`cargo run -p epics-oracle-rs --bin oracle`) sweeps the whole
`.dbd` denominator, and for that the *fat* IOCs the fallback constants name are
mandatory: base's stock `softIoc` cannot serve
`busy`/`transform`/`sseq`/`acalcout`/`scalcout`/`asyn`, and stock `softIocPVX`
exits during boot on those same six types (`PvxTools::DEFAULT_IOC_BIN` docs:
835 of 3386 channels ERRORed, capping coverage at 75.3% by the *instrument*).
Those come from a separate `oracle-ioc/` tree that links busy/calc/asyn on top
of base; it is not provisioned on `.129`.

The **14 integration tests** are narrower and do not need them. They sweep `ai`
and `bi` only (`tests/oracle.rs` uses `["ai","bi"]`, `tests/pva_oracle.rs` pins
`RT = "bi"`), and the only thing they ask of the `.dbd` is that it be a real
denominator:

- `record_types.len() >= 30` — base's `softIoc.dbd` has exactly 34
- `Surface::denominator() > 2000`
- `excluded_noaccess > 0`
- `dbCommon` inlined (`ai` must have `SCAN` and `STAT`)

So a host that only needs the 14 green can point all five vars at a stock
base + stock pvxs build. That is what `.129` runs. It is a **thin** oracle: 34
record types, not the host's 40. Do not read a green run there as a full-surface
sweep — the binary's `--phase all` on the six extra types is not reproducible on
a thin host at all.

## Recipe used on .129 (thin oracle, ~25 min wall clock)

```sh
sudo apt-get install -y libreadline-dev libevent-dev cmake

mkdir -p ~/work/epics-modules

# --- EPICS base: CA client tools, softIoc, base's 34-type softIoc.dbd
git clone https://github.com/physwkim/epics-base.git ~/work/epics-base
cd ~/work/epics-base && git checkout 6a21e5a3c      # matches the primary host
make -j12 EPICS_HOST_ARCH=linux-x86_64

# --- pvxs: pvx* client tools + softIocPVX (QSRV2)
git clone https://github.com/physwkim/pvxs.git ~/work/epics-modules/pvxs
cd ~/work/epics-modules/pvxs && git checkout 9348ebc
git submodule update --init bundle/libevent          # 1fe626c4, libevent 2.2 master
echo "EPICS_BASE=$HOME/work/epics-base" > configure/RELEASE.local
export EPICS_HOST_ARCH=linux-x86_64
make -C bundle libevent                              # NOT built by the top-level make
make -j12
```

Two things that are easy to get wrong:

- `make -C bundle` alone prints help and exits 0. The target is
  `make -C bundle libevent` (alias for `libevent.$(EPICS_HOST_ARCH)`). Without
  it `configure/Makefile:16` finds no `bundle/usr/$(T_A)` and silently falls
  back to whatever system libevent is installed.
- `~/.cargo/bin` is not on the non-login `PATH` on this box; export it before
  any `cargo`/`cargo nextest` invocation.

## The env block

```sh
export PATH=$HOME/.cargo/bin:$PATH
export EPICS_BASE_BIN=$HOME/work/epics-base/bin/linux-x86_64
export EPICS_ORACLE_DBD=$HOME/work/epics-base/dbd/softIoc.dbd
export EPICS_ORACLE_IOC_BIN=$HOME/work/epics-base/bin/linux-x86_64/softIoc
export PVXS_BIN=$HOME/work/epics-modules/pvxs/bin/linux-x86_64
export EPICS_ORACLE_PVX_IOC_BIN=$HOME/work/epics-modules/pvxs/bin/linux-x86_64/softIocPVX
```

Both `softIoc` and `softIocPVX` derive their `.dbd` from their own exe dir
(`<TOP>/dbd/<prod>.dbd`), so no `-D` is passed and `EPICS_ORACLE_DBD` must name
the dbd of the **same** tree `EPICS_ORACLE_IOC_BIN` comes from. Pointing them at
different trees is how a denominator silently stops matching what the ground
truth can serve.

## Measured result on .129

`cargo nextest run -p epics-oracle-rs --no-fail-fast`, counted by exit code:

| | tests run | passed | failed | exit |
| --- | --- | --- | --- | --- |
| before (no C tree) | 151 | 137 | **14** | 100 |
| after (env block above) | 151 | **151** | 0 | **0** |

The 14: `oracle` — `the_dbd_yields_a_real_denominator`,
`the_pair_boots_on_distinct_ports_and_both_serve_the_record`,
`an_unconnectable_pv_errors_rather_than_silently_agreeing`,
`a_real_run_reconciles_its_counts_and_reports_coverage`,
`every_reported_difference_carries_a_runnable_reproducer`; `pva_oracle` —
`the_pva_pair_boots_on_distinct_ports`,
`concurrent_client_tools_do_not_break_each_other`,
`each_side_is_the_sole_server_on_its_port`,
`every_channel_of_the_dbd_surface_gets_exactly_one_case`,
`coverage_counts_only_fully_measured_channels`,
`each_measured_channel_carries_a_real_type_and_a_real_value_reading`,
`the_dbd_derived_shape_matches_the_ground_truth_on_every_measured_channel`,
`at_least_one_channel_agrees`, `an_unreachable_pv_scores_error_never_agreement`.

Green run repeated three times (17.6 s / 16.8 s / 13.0 s), exit 0 each time —
the PVA pair boot is the cost, ~6.6 s of it the two mandatory `pvxlist`
exclusivity proofs.
