# Outbound SLIRP from an RTEMS guest — measured on target

Closes `doc/pvalink-rtems-design.md` §6 **item 4** and the entry that inherits
it unchanged, `doc/calink-rtems-design.md` §6 item 9:

> **Outbound SLIRP from guest to `10.0.2.2`.** Stage 5 topology A depends on
> it. The measurements in this tree exercise *inbound* `hostfwd` […];
> guest-initiated outbound TCP is standard SLIRP behaviour but is **not
> measured here**. Verify it with one `pvxget`-equivalent from the guest
> before building a stage on it.

(Both design docs cite this as an item to verify *once, for both tracks*. This
is that one verification.) Everything below is a reading taken from a booted
RTEMS 6 guest, corroborated on the host side; nothing is inferred from SLIRP
documentation.

**Answer: yes — outbound TCP works, and unlike the inbound direction it
cannot produce a false positive.** Details, and one quirk that changes what a
host-side IOC must bind, in §4.

## 1. What was measured, and with what

**Method point.** The verdict is taken from the **echoed payload**, never from
`connect()` returning `Ok`. That distinction is not pedantry here: the inbound
direction is already known to fake it — SLIRP completes the host-side TCP
accept for a `hostfwd` before it knows whether the guest answers, so `nc`
prints "Connection succeeded" against a guest that never saw the packet. This
probe therefore writes a line, reads it back, compares it, **and** logs the
same exchange independently on the host, so a pass requires both ends to agree.

| | |
|---|---|
| box | `coding-agent@192.168.2.128`, qemu-system-arm 8.2.2 |
| guest | `-M xilinx-zynq-a9 -m 256M`, `-nic user,model=cadence_gem` |
| image | `rtems-slirp-probe` (`crates/epics-ca-rs/src/bin/rtems-slirp-probe.rs`), built from this branch's probe commit |
| toolchain | `arm-rtems6-gcc 13.3.0`, BSP `xilinx_zynq_a9_qemu`, `RTEMS_BSP_PREFIX=~/rtems-bringup/tools` |
| RTEMS | `6.0.0.2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc`, RSB `5dbc1e08…`, Newlib `1b3dcfd` |
| libc | `0.2.188` from the path patch `~/rtems-bringup/libc-bringup` (widened `time_t` and `sockaddr_in::sin_len`) |
| host peer | a Python TCP+UDP echo bound `0.0.0.0:15076`, the one port this measurement was permitted to use |
| runs | 2, independent boots; identical verdicts (§3) |

Artifacts on the box under `~/rtems-bringup/slirp-probe/`: `probe.exe`,
`build-probe.sh`, `run-probe.sh`, `echo.py`, `probe.log` (guest console),
`echo.log` (host side), `pids.txt`.

### 1.1 The exact QEMU invocation

```
qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
  -serial null -serial mon:stdio \
  -nic user,model=cadence_gem \
  -kernel probe.exe
```

Two deliberate properties:

- **`-serial null -serial mon:stdio` is load-bearing** and unchanged from the
  recorded invocation: the BSP's kernel-IO base address is UART *1*, so a
  single `-serial mon:stdio` shows an empty console.
- **No `hostfwd`.** The recorded invocation carries `hostfwd` mappings, but
  those exist to let the host dial *in*; they are irrelevant to a guest dialling
  *out* and would bind host ports outside the single port this measurement was
  allowed. Their absence is therefore not a deviation that could affect the
  result — and it is itself a finding: **outbound needs no `hostfwd` at all.**

## 2. What the probe does

Three steps, each printing one grep-able line, plus a single `PROBE-OK` /
`PROBE-FAIL(...)` summary:

| step | what it proves |
|---|---|
| `PROBE-TCP` | `TcpStream::connect_timeout` to `10.0.2.2:15076`, write a line, read it back, compare. Passes only on payload equality. |
| `PROBE-REFUSED` | connect to `10.0.2.2:15077`, where nothing listens. Distinguishes a NAT that relays the far side's refusal from one that black-holes the SYN until the timeout expires. |
| `PROBE-UDP` | datagram round trip to the same host:port. Secondary — it is what CA name search will want later — so it runs **last** and its outcome does not change the TCP verdict. |

## 3. Result

Guest console, run 1 (run 2 differs only in the ephemeral port and in timings
below the tick quantum):

```
rtems-boot: main() reached

rtems-slirp-probe: start
rtems-slirp-probe: local address = 10.0.2.15:63178
rtems-slirp-probe: target = 10.0.2.2:15076 (echo), 10.0.2.2:15077 (no listener)
PROBE-TCP: OK echoed "PROBE-TCP-HELLO" (16 bytes); connect 22.63682ms, round trip 35.38809ms
PROBE-REFUSED: refused-port behaviour: ConnectionRefused after 1.97945ms (Connection refused (os error 111))
PROBE-UDP: OK echoed "PROBE-UDP-HELLO" from 10.0.2.2:15076 (15 bytes) in 5.63464ms
PROBE-OK
rtems-boot: IOC terminated with 0
```

Host echo, the same run — the independent half of the corroboration:

```
[…178.410] TCP echo listening on 0.0.0.0:15076
[…178.410] UDP echo listening on 0.0.0.0:15076
[…186.563] TCP accept from ('127.0.0.1', 44428)
[…186.574] TCP recv b'PROBE-TCP-HELLO\n'
[…186.574] TCP echoed 16 bytes
[…186.605] UDP recv b'PROBE-UDP-HELLO' from ('127.0.0.1', 45991)
[…186.605] UDP echoed 15 bytes to ('127.0.0.1', 45991)
```

Both runs, side by side:

| measurement | run 1 | run 2 |
|---|---|---|
| guest address (DHCP) | `10.0.2.15` | `10.0.2.15` |
| TCP outbound | **OK**, payload echoed | **OK**, payload echoed |
| TCP connect latency | 22.64 ms | 22.45 ms |
| TCP round trip | 35.39 ms | 34.73 ms |
| dead port | `ConnectionRefused` @ 1.98 ms | `ConnectionRefused` @ 1.87 ms |
| UDP round trip | **OK** @ 5.63 ms | **OK** @ 5.29 ms |
| boot → verdict | 9 s | 9 s |

DHCP: `offered/acknowledged 10.0.2.15 from 10.0.2.2`, default route via
`10.0.2.2`, lease 86400 s. The image ran to completion and RTEMS shut down
cleanly (`IOC terminated with 0`).

## 4. SLIRP behaviour observed

**(a) Outbound cannot produce the inbound direction's false positive.** The
dead-port control is the proof: `10.0.2.2:15077` returns `ConnectionRefused`
in ~2 ms, so SLIRP does **not** locally accept an outbound connect and
discover the truth afterwards — it relays the far side's real answer. The two
directions are therefore asymmetric, and the asymmetry favours us:

| direction | who completes the handshake | can `connect()` alone lie? |
|---|---|---|
| inbound (`hostfwd`) | SLIRP, locally, before the guest is consulted | **yes** — must assert on payload |
| outbound (guest → `10.0.2.2`) | the real host peer; refusal is relayed | no — measured |

Asserting on the echoed payload is still the right discipline for a future
gate, but for outbound it is belt-and-braces rather than the only thing
standing between a green light and a fiction.

**(b) The host sees the guest's source address as `127.0.0.1`, not
`10.0.2.15`.** This is the quirk with consequences. SLIRP NATs the guest onto
the host's **loopback**, so on the host side the peer is `('127.0.0.1',
<ephemeral>)`. Two things follow for the two-IOC gates:

1. **A host-side IOC must listen on `127.0.0.1` or `0.0.0.0`** — binding only
   the LAN address makes it unreachable from the guest, and the failure will
   present as a connection refusal, i.e. exactly like a stopped IOC. The echo
   in this measurement bound `0.0.0.0`; a `softIoc`/`softIocPVX` in the gate
   must not be narrowed past that.
2. **Access-security host rules on the host-side IOC will see `127.0.0.1` for
   every guest.** Both of our peer-derived host checks — PVA ACF's
   `with_server_derived(peer)` and `.pvlist DENY FROM` — take the host from
   the socket, by design. Under SLIRP that address is loopback for all guest
   traffic, so guests are mutually indistinguishable and any localhost-scoped
   rule matches them. Fine for a bring-up gate; it means such a gate **cannot**
   be used to test host-based access rules.

**(c) DHCP may need a second solicit.** Run 1 lost carrier mid-lease
(`carrier lost` → `carrier acquired` → re-solicit) and still reached `main()`
in ~8 s. `err: cgem0: ipv4_addroute: File exists` appears twice while the
routes are installed and is benign — the resulting table is correct.

**(d) Timings are tick-quantised.** `CONFIGURE_MICROSECONDS_PER_TICK` is
10 000, so the sub-10 ms figures above are coarse and reproducible only to
about that resolution. The ~22 ms TCP connect versus ~5 ms UDP round trip is
consistent across both runs, but no conclusion should be hung on the
difference.

## 5. What this does not measure

- **CA or PVA over the path.** This is a `std::net` reachability measurement.
  It removes the transport question from stage 5 / stage C6; it does not
  substitute for those stages.
- **Topology B (guest ↔ guest).** Still untried, still needs a shared netdev
  (`doc/pvalink-rtems-design.md` §6 item 5).
- **Sustained or concurrent outbound load.** One connection, twice.
- **Name resolution.** DHCP offered `10.0.2.3` as a DNS server; the probe used
  a literal address throughout, as the gates will.

## 6. Reproducing

The probe binary is **measurement scaffolding and is not merged** — it lives on
the branch that produced this document, in a commit separate from this one, the
same arrangement used for `doc/rtems-priority-probe.patch`. To re-run: restore
that commit's `crates/epics-ca-rs/src/bin/rtems-slirp-probe.rs` and its
`[[bin]]` entry, then on the box

```
~/rtems-bringup/slirp-probe/build-probe.sh   # cross-build, stage probe.exe
~/rtems-bringup/slirp-probe/run-probe.sh     # host echo + qemu + grade + clean up
```

`run-probe.sh` starts the host echo and QEMU, waits for the `PROBE-` verdict
line, and terminates only the two PIDs it started.
