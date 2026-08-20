# Changelog

## v0.26.2 — 2026-08-20

Patch release. aSub channel cells are now allocated and filled by their
declared FTx/NOx and FTVx/NOVx, asyn gains the busy module's devBusyAsyn
device support plus a background-thread route for alarm pushes, and db/acf
macro expansion runs per line so quote state cannot leak across lines.
Additive API only (`ParamSetValue` gains a `Status` variant and busy's DTYP
menu gains asynInt32); workspace version 0.26.1 -> 0.26.2, pins unchanged.

### aSub cells take the shape FTx/NOx declares

The A..U cells all initialised as scalar `Double(0.0)` whatever FTA..FTU
declared, so a link store judged its destination by that stale scalar: an
array source was reduced to its first element and a string source was
dropped by the numeric funnel. Each cell now allocates from FTx/NOx as C
`initFields` does, every value entering A..U is shaped to that declaration,
a STRING channel reads a scalar link as DBR_STRING, and VALA..VALU take the
same rule — a subroutine's write lands in the FTVx-typed NOVx-element
buffer, so an array written without declaring NOVx keeps element 0 only, as
C requires. An empty array source reports NEx 0 rather than claiming an
element that was never delivered.

### asyn: driver-clearable busy records and background alarm pushes

devBusyAsyn is ported: a busy record over asynInt32 follows the driver
parameter with no `asyn:READBACK` tag, mirroring the C device support's
unconditional output interrupt registration, so the driver clears the busy
when the operation completes. And `ParamSetValue::Status` gives a poll
thread the C `setParamStatus` + `callParamCallbacks` route it lacked — a
driver thread can now raise and clear COMM/INVALID on its readback
parameters when the device stops responding.

### Macro expansion is per line, as macLib sees it

C feeds `macExpandString` one line at a time, so its quote tracking resets
at each newline. Expanding a whole db/acf file in one call let an
apostrophe in a comment suppress every `$(...)` below it; `parse_db` and
`asInit` now expand per line.

## v0.26.1 — 2026-08-20

Patch release. `ad-plugins-rs` moves to rust-hdf5 0.5.1, which brings that
crate's 0.4/0.5 read overhaul to the HDF5 file plugin. An asyn parameter batch
now fires an interrupt for every address list it wrote, an NTNDArray frame no
longer boxes its pixels one `ScalarValue` at a time on the PVA path, and the
merged-upstream fix wave for epics-base #853/#856/#871/#934/#944 and asyn #217
is in. Workspace version 0.26.0 -> 0.26.1, internal `[workspace.dependencies]`
pins move to 0.26.1 in lockstep.

### `ad-plugins-rs` reads and writes HDF5 through rust-hdf5 0.5.1

The jump from 0.3.2 picks up that crate's read-path rewrite — a typed full
read landing in the vector it returns rather than a second buffer of the same
size, a hyperslab of an unfiltered chunked dataset reading only the byte
ranges the selection intersects, a per-dataset cache of partly-consumed
filtered chunk images, and `zlib-rs` inflating what `flate2` compressed.
rust-hdf5's own probes against libhdf5 1.14.6 put a 128 MiB contiguous read at
0.89x of its time where it was 1.79x, and opening plus reading 2000 small
datasets at 0.29x where it was 2.52x. Stored bytes are unchanged.

One thing on this side had to move. 0.5.1 resolves a dataset's path against
the groups that already exist — libhdf5's rule with
`H5Pset_create_intermediate_group` off — so `Hdf5Writer::open_swmr` builds the
layout group tree before creating the streaming dataset that carries the
nested layout path as its name. The group that path names is now the dataset's
placement, and the `assign_dataset_to_group` call that used to re-parent it
afterwards is gone: one owner rather than a create followed by a move.

### An asyn parameter batch flushes every address list it wrote

`callParamCallbacks` consumes one address list's changed flags, and a C driver
calls it once per list it wrote. `RequestOp::CallParamCallbacks` bundles the
sets and the flush, so flushing the request address alone stored the other
lists' values while firing no interrupt for them: a multi-device driver
publishing six joint angles at addresses 0..6 in one call left five records at
UDF for the life of the IOC, while the array parameter carrying the same six
numbers updated normally. Found in the ur-robot RTDE receive driver.

### An NTNDArray frame no longer costs 24x its pixels on the PVA path

`NdArrayBuffer::into_scalar_array` boxed every element into a `ScalarValue`,
24 bytes wide because the enum carries a `String(PvString)` variant, so a
640x480 RGB8 frame turned 0.9 MB of pixels into a 22 MB `Vec<ScalarValue>`
that the encoder then walked element by element. It now returns the
`PvField::ScalarArrayTyped` variant the encoder bulk-copies, and
`nt_nd_array_value` borrows the pixel buffer instead of cloning it first. On a
RealSense D405 IOC serving colour and depth at 640x480 @ 15 fps, the two PVA
plugins' CPU increment falls from 135.3% to 14.3% of one core. The wire format
does not change.

### The merged-upstream fix wave

- **CA server:** both write actions clamp `m_count` to the channel's final
  element count and cross-check `dbr_size_n` against the postsize before
  access (epics-base #934), carrying #944's scalar-`DBR_STRING` exemption —
  without it every default-mode `caput` drops, since libca frames a scalar
  string as `ALIGN(strlen+1)` rather than 40 bytes. An `EVENT_ADD` whose
  `mon_info` payload is truncated is now dropped instead of defaulted to a
  `DBE_VALUE|DBE_ALARM` mask C never applies.

- **CA links:** the iocInit link wait is gated on `init_ready` — monitor and
  attribute fetch both complete — instead of `is_connected`, which the first
  monitor event satisfies before the detached CTRL fetch lands, so records
  could init against absent metadata (epics-base #856).

- **Access security:** `dump_report` quotes UAG and HAG members C's
  `asDumpQuoted` way, via `epicsStrPrintEscaped` ported into
  `epics-libcom-rs` (epics-base #871).

- **asyn:** enabling auto-connect while the port or addressed device is down
  arms `connect_retry_at`, so the flip alone brings the link up (asyn #217).

- **QSRV:** `make_monitor_snapshot` applies the `Q:time:tag` nsec split that
  `snapshot_for_field` ends with, so a monitor and a GET of the same channel
  no longer disagree; both producers share one `finish_field_snapshot` tail
  (the same drift as pvxs #189).

- **RTEMS:** an opt-in static-IP arm in the network bring-up — first interface
  through `rtems_bsd_ifconfig`, link-up waited on a `PF_ROUTE` socket opened
  before it, addresses from `EPICS_RTEMS_STATIC_IP`/`_NETMASK`/`_GATEWAY`
  compile-time defines, falling back to DHCP on any failure (epics-base #853).

## v0.26.0 — 2026-08-11

Minor release. The upstream-issue audit lands its fix wave across the CA
client/server, iocsh, db links, and access security; the QSRV PUT path is
measured past pvxs QSRV2 on both server CPU and throughput; and the CA client
no longer silently writes a wrong value on a cross-typed put. Five breaking
API changes (epics-ca-rs, epics-base-rs, epics-pva-rs, epics-bridge-rs).
Workspace version 0.25.4 -> 0.26.0, internal `[workspace.dependencies]` pins
move to 0.26.0 in lockstep.

### Breaking

- **epics-ca-rs: `protocol::check_write_element_count` is renamed
  `check_write_request`** and now owns the DBR-type bound as well as the
  element-count bound, in C's order (`comQueSend.cpp:323/330`): a scalar
  `put_as_dbr_*` with a type past `LAST_BUFFER_TYPE` is refused client-side
  instead of reaching the server, which answers ECA_BADTYPE and drops the
  circuit.

- **epics-base-rs: `AcfCell` is a newtype, and `IocApplication.acf` holds
  one** in place of `Option<AccessSecurityConfig>`. The cell is the IOC's
  single live Access Security policy — every server built from the app must
  share it so a later `asInit`/ACF reload reaches all of them. The old alias
  invited re-wrapping the config in a fresh cell, which left the interactive
  shell storing into a copy the servers never read.

- **epics-base-rs: the record snapshot's `description` is `PvString`, not
  `String`.** A non-UTF-8 DESC must reach the client byte-preserved, the
  same rule `units` already follows.

- **epics-pva-rs: the native server's per-op credential fields
  (`account`/`method`/`host`/`authority`/`roles`) collapse into one shared
  `ClientCreds`,** handed to access checks by reference instead of
  re-cloning five strings per check.

- **epics-bridge-rs: `AccessContext` carries `access` plus
  `creds: Arc<ClientCreds>`** in place of its five owned string fields, for
  the same reason.

### The CA client writes the value you asked for

`put`, `put_with_timeout` and `put_nowait` stamp the channel's native type on
the frame but encoded the caller's variant verbatim, so `Long(1)` to an ENUM
field shipped an ENUM header over i32 bytes and the server decoded the leading
zero bytes as index 0 — a silent wrong-value write with a successful callback,
found live under a sequencer daemon. The value now routes through
`convert_to(native_type)` before encoding, as C's `nciu::write` does.
`CaClient::drop` no longer panics on a thread without a tokio runtime (that
unwind panic was masking the daemon's real bring-up error), and
`EPICS_CA_NAME_SERVERS` entries are re-resolved through DNS on every redial
instead of once at startup, with a nameserver's TLS SNI row keyed by hostname
rather than its startup address.

### QSRV PUT overtakes pvxs QSRV2

Back-to-back on the same host, db and client (20,000 puts, 3 runs each), the
shipped `qsrv-rs` reads 99.0–102.0 µs of server CPU per put at 3,781–3,842
puts/s against pvxs `softIocPVX`'s 103.5–104.0 µs at 3,693–3,724 — every Rust
run beats every pvxs run on both metrics (`doc/qsrv-put-perf.md`). What got it
there: the server bins serve from one tokio worker (an idle multi-worker pool
costs ~30 µs/put in wake/steal churn while one client's traffic is a single
runnable task); per-peer channel resolution and the ACF grant per
(channel, credential) are cached; a RULE's CALC compiles once at ACF parse; the
INIT reply's inline type encoding is memoized; the negotiated `FieldDesc` and
peer credentials are shared by `Arc`; a PUT delta decodes into the channel's
scratch value; and a ready EXEC body runs inline instead of spawning a task.

### The upstream-issue audit lands its fixes

The 2026-08-09 sweep (`doc/upstream-issue-audit-2026-08-09.md`, 115 open
epics-base issues triaged) turned into fixes across the workspace:

- Framework link reads honor the record's declared DBR type, and
  dbPut-analogue writes to DBF link fields are refused, as C refuses them.
- iocsh: `<`/`iocshLoad` nesting is bounded at 32 levels; out-of-quote
  backslash is honored in every line scanner; `db*` and directory-stack
  failures return `Err` instead of printing and continuing;
  `IOCSH_STARTUP_SCRIPT` is recorded on the first script load; `asInit`
  stores into the IOC's live `AcfCell`.
- Access security: HAG hostnames are re-resolved periodically under
  `asCheckClientIP`, so a DHCP-moved client host does not keep its old grant.
- db: SEARCH answers for record-own DBF_NOACCESS names as C does; a
  substitutions-file entry that yields no template loads warns;
  `putStringUlong`'s via-double fallback is ported for DBF_ULONG.
- CA server: a compound DBR put (`DBR_STS_*`, `DBR_TIME_*`, …) no longer
  drops the circuit (`doc/ca-compound-dbr-put.md`).
- PVA: the server writer queue is bounded in bytes, not frames; qsrv refuses
  a channel on a field the record does not have and serves
  `display.description` from DESC as pvxs does; a refused CREATE_CHANNEL's
  re-search parks a full ring out; the client emits the member name in the
  `Tree` inline branch.
- procServ: child exec allocations move before `forkpty`, off the
  post-fork async-signal-unsafe window.

The `--all-features` build of epics-ca-rs compiles again (`MdnsAnnouncer` is
re-exported beside `MdnsBackend`, whose `announce_helper` returns it).

## v0.25.4 — 2026-08-04

Patch release. The IPv6 SEARCH socket now builds on the embedded targets too,
completing the half v0.25.3 left out. On the CA server, an out-of-buffers burst
no longer disconnects every client it is serving, and a subscriber that stops
reading no longer grows the server without bound. A default `histogram` record
no longer alarms on its first process. Workspace version 0.25.3 -> 0.25.4.

### The IPv6 SEARCH socket binds on every backend too

v0.25.3 left the IPv6 half out of the embedded targets, on the reading that
`socket2` and `tokio::net` were required to build it. They were an
implementation choice, not a platform limit: `std` covers the bind, the group
join and the loop-back option, and `IPV6_V6ONLY` / `IPV6_MULTICAST_HOPS` are
plain `setsockopt` calls whose constants both target `libc`s carry.

- **`epics_base_rs::net::search_udp::SearchUdpSocketV6`** is a sibling type
  rather than a variant of `SearchUdpSocket`, because the two families differ
  in what a socket *is* here and not only in the address it carries: the v4
  host arm is a per-NIC bundle with `IP_PKTINFO` attribution and a
  `255.255.255.255` fanout, and v6 has none of the three. Folding them into one
  type would give four of its methods a second meaning, "unsupported for this
  variant". The `exec_backend` receive pump, its stop flag and its joining
  `Drop` are address-family neutral and are shared as code.

- `IPV6_V6ONLY` is set on an **unbound** socket, the same ordering constraint
  `SO_REUSEADDR` / `SO_REUSEPORT` have, which is why neither family can go
  through `std::net::UdpSocket::bind` — it binds as it constructs.

- `epics-pva-rs`'s `V6Socket` is gone (153 lines), along with the uninhabited
  `enum V6Socket {}` whose `exec_backend` methods were `match *self {}`.

- Both cross-compile ratchets still read **zero** target errors, now covering
  both address families. Compiling is not running: whether an RTEMS BSP or the
  VxWorks stack has IPv6 enabled is an on-target question, and a failed bind
  degrades the client to IPv4-only by design.

### The CA server rides out an out-of-buffers burst instead of dropping clients

C's `cas_send_bs_msg` (`rsrv/caserverio.c:65-101`) continues its send loop on
both `SOCK_EINTR` and `SOCK_ENOBUFS`, sleeping 15 s for the latter. Both port
drivers wrote frames with `write_all`, whose contract is the opposite — any
`Err` ends the call — so an ENOBUFS burst that C rides out disconnected every
CA client the IOC was serving.

- The policy lives *under* every writer rather than at each call site: the
  hosted driver has three write sites and the blocking driver two, and a retry
  bolted onto the drain would have left the unsolicited `CA_PROTO_VERSION`
  greeting and the out-of-band monitor frame exactly as they were. Resumption
  is exact for the same reason C's is — the adapter sits at the `write` /
  `poll_write` level, where a retry re-offers only the bytes the kernel did not
  take.

### A CA subscriber that stops reading no longer grows the server without bound

The hosted driver's monitor producer pushed into an unbounded `mpsc` decoupled
from the socket, so a subscriber that stopped reading grew the server at a
measured 9.34 kB/s — 32.8 MB/hour — per 100 Hz subscription, with no knee. The
blocking driver that the embedded targets run was never exposed: its event
thread writes the socket directly under the send lock, so back-pressure reaches
the bounded `EvQue` ring and the ring coalesces, which is what C gets from
`event_task` blocking on `SEND_LOCK`.

- Bounding the channel instead would deadlock, because the connection loop is
  both a producer (reply handlers) and the sole drain. A credit rides inside
  the queued frame and is released by the drain owner's `Drop` once the bytes
  are in the socket writer, so the producer stops dequeuing and the backlog
  coalesces in the ring. In-loop reply handlers are exempt and unaffected.
  Re-measured at 0.000 kB/s with the drain still provably parked; the harness
  and all five run logs are in `doc/ca-stuck-reader-measurement.md`.

### A default `histogram` record no longer alarms on its first process

`LLIM` and `ULIM` carry no `initial(...)` in `histogramRecord.dbd`, so a bare
`record(histogram)` loads at `0 == 0` and satisfies C's `llim >= ulim`. Taking
that condition verbatim into the CBUG-F12 refusal made `check_alarms` raise
SOFT/INVALID on the first process of every unconfigured histogram where C
raises NO_ALARM — 14 oracle defects on SCAN/PROC/UDF. `add_count` keeps `>=`:
binning nothing into an empty range is arithmetic, not a judgement.

## v0.25.3 — 2026-07-30

Patch release. The CA and PVA *clients* now bind a UDP SEARCH socket on the
embedded targets, closing a gap that made a broadcast-only PV silently
unreachable from an RTEMS or VxWorks IOC. Both halves are verified on target,
not merely cross-compiled. The CA blocking server's accepted clients get
`SO_KEEPALIVE`, as C's do. Workspace version 0.25.2 -> 0.25.3.

### The clients bind a SEARCH socket on every backend

On `exec_backend` the CA and PVA clients bound no UDP socket **by type**: the
transport was selected by `cfg`, so a target IOC's own `ca://` and `pva://`
record links could resolve only through an explicitly configured
`EPICS_CA_NAME_SERVERS` / `EPICS_PVA_NAME_SERVERS`. A PV reachable only by
broadcast never connected, and did so without a diagnostic. The server side was
already complete; this was the client half alone.

- **`epics_base_rs::net::search_udp::SearchUdpSocket`** is the one type both
  clients bind. On `tokio_backend` it delegates to the existing per-NIC
  `AsyncUdpV4` bundle unchanged. On `exec_backend` it is a single wildcard
  `std::net::UdpSocket` plus a receive-pump thread — libca's own model
  (`udpiiu.cpp:174` creates one socket, not a per-NIC bundle). `Drop` joins the
  pump: the pump co-owns the socket, so a stop flag alone leaves the port held
  for up to the pump's wake interval and the next bind fails `AddrInUse`.

- **`epics_base_rs::net::iface_v4`** enumerates IPv4 interfaces through
  `getifaddrs`, which RTEMS 6 and VxWorks 7 both provide, replacing the
  `if-addrs` dependency that builds for neither. `IfaceV4::search_destination()`
  is C's rule from `osdNetIfAddrs.c:130-151`. The stubs it replaces returned
  nothing on the embedded targets, so `EPICS_CA_AUTO_ADDR_LIST=YES` — C's
  default, and the whole of an unconfigured site's discovery — expanded to an
  empty list there.

- **`fanout_to` takes the operator's egress constraint as a parameter** rather
  than exposing a second entry point, so "which interfaces may this leave
  through" is one question with a default answer instead of one a caller can
  ask on one path and forget on another.

- Two host-visible behaviour changes follow from sourcing everything through
  `iface_v4`, both toward C: interface enumeration now tests `IFF_UP`, and
  point-to-point peer addresses are included.

- The IPv6 half stays out of the embedded targets deliberately. Both its
  binders are `socket2` and its receive is `tokio::net`, so `V6Socket` is an
  uninhabited enum on `exec_backend` — `Option<V6Socket>` is `None` by
  construction and neither crate reaches the target's compiled unit. That is
  why both cross-compile ratchets still read zero target errors with the v4
  transport compiled in.

### Measured on target

- RTEMS 6 (QEMU xilinx-zynq-a9): `realtime-pva-ioc` resolved `RTEMS:PVA:AO` by
  broadcast with **no name server configured** — the SEARCH response carried
  the IOC's own GUID.
- VxWorks 7 (`x86_64-wrs-vxworks`): a probe RTP exercised the bind, the
  interface walk, the pump round-trip, the broadcast fanout and the
  `Drop`-releases-the-port regression. `SO_RCVTIMEO` is accepted here; it is
  `SO_SNDTIMEO` that this target refuses with `ENOPROTOOPT`, and the pump sets
  none. Transcript and its seven claims: `doc/vxworks-port.md` §5.6.

### A wedged CA client is reaped again on the embedded targets

`handle_client_blocking` had no `SO_KEEPALIVE`. `write_frame_locked` parks in
`write_all` under the send lock, which is C's shape — `cas_send_bs_msg` loops on
a blocking `send()` under `SEND_LOCK` with no bound either — and C survives it
because `create_client` sets the option on every accepted socket. The hosted
driver did so through `socket2`, which builds for neither embedded triple, so
the reactor-free driver had the option nowhere.
`runtime::socket::enable_keepalive` now carries it, and `set_nodelay` stops
being best-effort in the same breath: C refuses the client when either option
fails.

### One flaky executor test closed

`a_yielding_task_releases_the_worker_to_a_queued_task` held its slow task for
a fixed number of yields and asserted the queued task arrived first — true
only if the test thread enqueues that task before the worker exhausts the
count, which under load it does not. The slow task is now gated by the test
thread, so its own stated precondition holds by construction. Measured at
5/400 failures pinned to one CPU before, 0/400 after.

## v0.25.2 — 2026-07-28

Patch release. Three on-target rounds on the VxWorks 7 guest and the armv7
RTEMS 6 board turned the embedded port's open questions into measurements, and
the defects those measurements exposed are fixed here. Thread admission is now
bounded by reserved address space rather than by a heap figure the target
cannot answer; a bounded socket write no longer rests on `MSG_DONTWAIT`, a flag
Darwin ignores and VxWorks was never measured honouring; the CA server frames a
DBR reply in one reused buffer, as C does. `asyn-rs` gains a VxWorks serial
backend and declares the RTEMS termios ABI against the BSP headers instead of
trusting `libc` for it. `cargo doc -D warnings` becomes a gate on the host and
on both embedded targets, private items included. Workspace version 0.25.1 ->
0.25.2.

### Thread admission is bounded by reserved address space

- **The CA connection wall is an address-space ceiling, not a heap one.** On
  VxWorks each admitted client reserves its declared stack plus roughly 1 MiB,
  and the wall moves linearly with guest RAM (0.2058 clients/MB, R² = 0.998,
  ≈ 4.86 MB/client) until the pool's own cap binds — so 1024 MB versus 1280 MB
  is no threshold. No RTP query tracks that ceiling: `sysctl` answers `ENOENT`,
  `memFindMax` sits flat at 256 KiB, and there is no `getrlimit`; an `mmap`
  ladder matches the wall to the byte. `WorkerPool` therefore bounds admission
  against a declared reservation budget — `EPICS_RS_POOL_RESERVATION_MB`,
  defaulting to 160 MiB on both embedded targets — rather than against a number
  the target will not give. On RTEMS, where `malloc_free_space` does answer,
  the boot check confirms or clamps that default instead of taking it on faith.

- **Thread memory is charged per target, not by an `embedded` flag.**
  `ThreadMemoryTarget` replaces the boolean: RTEMS's measured per-thread
  overhead is 0 and VxWorks's is 1 MiB, and the shared flag had been charging
  RTEMS the VxWorks figure, costing it 3× its admission.

- **A set now retires with its worker.** A panicking worker used to leak its
  pool set; the set and its thread handles are retired together. `WorkerPool`
  also asks the target for a set's mutex object before spawning, because a
  VxWorks pthread mutex allocates its semaphore lazily on *first lock* and
  reports exhaustion as `EINVAL`, so `std::sync::Mutex` panics "invalid
  argument" — root-caused on target to `semMCreate` returning NULL at 588
  semaphores.

- Every fixed IOC thread, including the CA and PVA entry points, is charged to
  the pool's account, and a refusal names the admission gate that refused
  rather than an errno. `AcquireError` keeps a full pool and a refused spawn
  distinct.

### A bounded write no longer rests on `MSG_DONTWAIT`

`epics-libcom-rs::runtime::blocking_io::write_frame_deadline` and
`asyn-rs`'s `write_with_retry` both bounded a frame write with `poll(POLLOUT)`
plus a send that could not park, the second half resting on `MSG_DONTWAIT`.
XNU decides whether `sosend` sleeps from the socket's own `SS_NBIO`, so on
macOS the send blocked to completion and the deadline did not hold. VxWorks 7
implements no `SO_SNDTIMEO` to fall back on (`ENOPROTOOPT`, measured on
target), so where this code was written to run the bound rested on an untested
assumption. Both crates now own the socket's blocking mode at the single site
where a socket becomes live, and poll both directions — C's shape under
`USE_POLL`. `doc/darwin-send-dontwait-gap.md` carries the measurement.

### CA server and client

- One send buffer is reused across deliveries and a DBR reply is framed in it
  with C's reserve-in-place, replacing a per-reply allocation; the two
  implementations of `read_reply` sizing are merged into one.
- The CAS-client stack class drops to Medium on two measurements: armv7-RTEMS
  high-water of 24,432 B for CAS-client and 3,816 B for CAS-event against a
  realistic record set, and 65,912 B on VxWorks against C-on-VxWorks's `Big` at
  22,000 B.
- A refused client is announced exactly once. The refusal status is constrained
  by the protocol rather than chosen freely — only `CA_K_WARNING` statuses are
  safe for libca and `ECA_MAXIOC` aborts the client — so the gate that refused
  rides in the diagnostic text.
- Client: a server exception is raised on the circuit's receive path; a write
  exception's identity binds to the request rather than the channel; a
  name-service circuit takes the same liveness rule as a data circuit and
  releases its socket before the backoff.

### asyn-rs on the embedded targets

- The serial backend is one seam with three arms. VxWorks binds `sioLib`
  numbering, and RTEMS declares its own termios ABI against
  `arm-rtems6/include/sys/_termios.h` in the BSP sysroot, because `libc` adds a
  `c_line` member RTEMS does not have and drops the speed fields — 102 errors
  and 0 bound flag constants before the ABI was owned locally.
  `cfset[io]speed` now reports what it refuses instead of dropping it.
- `unix://` is refused on VxWorks at parse time, matching C's `HAS_AF_UNIX`,
  and the serial fd stays non-blocking for its whole life.
- `epics-libcom-rs` gains `runtime::socket`, through which asyn's IP drivers are
  routed, so the two crates no longer carry separate socket surfaces.

### Record processing and the runtime

- A put-notify wait-set is released when a chain entry is refused, and a
  link-chain bound refusal raises an alarm instead of truncating a legal `FLNK`
  chain silently.
- One `Snapshot` is shared across a post's subscribers, and a wide-value
  monitor is capped at one queued entry.
- A dropped `Sleep` cancels its `delayed_timer` entry. The bring-up census and
  the diagnostic threads get band owners in `ioc_role`, the census below client
  service.

### Documentation gate

`cargo doc` runs with `-D warnings` on the host and on both embedded targets,
documenting private items on every row, and `asyn-rs` joins the
`rustdoc-embedded` closure. The intra-doc links that had never resolved are
resolved across every crate; where public documentation cited a crate-private
item, the citation became a code span rather than a link.
`doc/rustdoc-embedded-only-census.md` records which citations exist only on an
embedded row.

### Removed

`epics-libcom-rs::runtime::blocking_io::SEND_TICKS_PER_DEADLINE` and
`send_tick_for`, which existed to divide a caller's deadline into socket send
timeouts. `write_frame_deadline` now owns its bound, so neither has a caller.

## v0.25.1 — 2026-07-26

Patch release. VxWorks 7 (`x86_64-wrs-vxworks`) joins RTEMS 6 as a supported
embedded target, reached by generalizing the RTEMS cfg into a capability cfg
rather than by adding a second OS special case. Both embedded targets now take
their `libc` fixes from one pinned public fork branch, which is what lets the
VxWorks closure be type-checked in CI instead of only on the bring-up box.
Also: a mandatory IOC thread that cannot start is now process-fatal instead of
silently absent, and the `release-embedded` profile plus
`scripts/embedded-image.sh` give both targets a deployable-image entry point.
Additive API only; no breaking changes. Workspace version 0.25.0 -> 0.25.1.

### VxWorks 7 (x86_64-wrs-vxworks)

- **`epics_embedded_target` replaces `target_os = "rtems"` as the porting
  cfg.** Every arm that meant "this target has no reactor, no ambient OS
  services, one address family" now keys on the capability rather than the OS
  name, in `epics-libcom-rs`, `epics-base-rs`, `epics-ca-rs`, `epics-pva-rs`
  and `epics-bridge-rs`; the arms that are genuinely RTEMS-specific (the boot
  shim, the BSP link contract) deliberately stay on `target_os`. Adding the
  second embedded target touched cfg predicates, not protocol code.

- **The statistics funnel becomes a per-OS backend selection.**
  `epics-rtems-boot` owns it for both targets: `MemUsage` fields are
  individually optional, so a target reports only what its OS can answer, and a
  VxWorks backend binds `taskIdSelf`, `taskInfoGet`, `rtpIoTableSizeGet` and
  mimalloc's `current_commit` for the task, stack, fd and memory census. The fd
  walk is bounded by `rtpIoTableSizeGet` (a `size_t` return, per `ioLib.h:533`)
  rather than `sysconf`. The console census moved into the same funnel, so
  `epics-ca-rs` and `epics-bridge-rs` print one format on both targets.

- **`scripts/vxworks-check.sh`**, the portability gate: a binary census plus
  every target row, on the same bidirectional-census discipline as
  `rtems-check.sh`. VxWorks also gets its own SCHED_FIFO priority mapping in
  `epics-libcom-rs`'s task layer and its own entropy source for the PVA server
  GUID.

- **One libc fork branch for both embedded targets.** The workspace pin moves
  to `physwkim/libc` branch `epics-rs-0.2`, which now carries the VxWorks
  `pread`/`pwrite`/`killpg`/`getentropy` shims alongside the RTEMS
  sockaddr/type-width fixes. A manifest `[patch.crates-io]` never reaches
  `-Zbuild-std`, which resolves std against rust-src's own
  `library/Cargo.lock`, so the new `scripts/libc-std-patch.sh` derives a
  config-level patch from that same pin — a clone of the pinned rev relabelled
  to the libc version the toolchain's lock demands, emitted as an alias entry
  so the workspace graph keeps the committed resolution while the std graph
  takes the relabelled copy. `vxworks-check.sh` and `scripts/embedded-image.sh`
  both call it, leaving the manifest line the single source of truth for what
  libc is compiled.

- **The VxWorks closure is now type-checked in CI.** With the patched libc
  public, the `vxworks-census` job installs the same stock `nightly` +
  `rust-src` as `rtems-closure` and runs the whole gate rather than the census
  alone. Linking stays on the bring-up box, because a `.vxe` needs the
  proprietary Wind River SDK — a gap now stated as a fact rather than left as
  an absent gate.

- `doc/vxworks-port.md` documents the target and toolchain contract, the cfg
  architecture, the priority model, and what was measured on target: 11/11 gate
  rows, CA and PVA round-trips over the wire, the census blocks verbatim, and
  (§5.5) a five-row strip/LTO size matrix for both targets and both binaries.

### Runtime and asyn

- **A mandatory IOC thread whose spawn fails is now process-fatal.** Such a
  thread resolved its own spawn `Result` with `.expect(..)`, and on a
  `panic = "unwind"` target that kills only the calling thread — measured on a
  VxWorks RTP, an `EAGAIN` from the periodic-scan spawn left the process
  answering CA with zero periodic scanning. `MandatoryThread` makes the failure
  process-fatal, matching C, where a rate that was never created wedges
  `iocInit`; the areaDetector plugin data threads route through it.

- **`asyn-rs` port creation is fallible.** `create_port_runtime` ended in
  `.expect(..)` too, so a `*Configure` line whose port thread failed to start
  unwound iocsh while later st.cmd lines ran against a port that did not exist.
  A port whose runtime thread cannot start is now never registered, matching
  `registerDriver`'s unwind-before-`ellAdd` order in C.

### Embedded images

- **`release-embedded` profile and `scripts/embedded-image.sh`.** Cargo has no
  target-conditional profile, so the "what do we ship" default lives at the
  entry point: `./scripts/embedded-image.sh <rtems|vxworks> <ca|pva>` builds on
  a profile inheriting `release` with `strip = "symbols"`, `lto = "fat"`,
  `codegen-units = 1`. Measured against the dev images the gates build: RTEMS
  CA 122,884,636 B -> 4,604,848 B, VxWorks CA 116,768,688 B -> 4,287,696 B,
  both boot-verified and round-tripped. A plain `cargo build --release` is
  unchanged.

- **The target IOC binaries are renamed `realtime-ca-ioc` and
  `realtime-pva-ioc`** (were `rtems-ca-ioc` / `rtems-pva-ioc`), since they now
  serve two embedded targets.

### Fixes

- `epics-pva-rs`: `SO_SNDTIMEO` is best-effort on the blocking accept path.
  VxWorks returns `ENOPROTOOPT`, which was fatal and closed every accepted
  connection.
- `epics-ca-rs`, `epics-pva-rs`: a failing `peer_addr()` is logged before the
  accepted connection is dropped.

## v0.25.0 — 2026-07-24

Minor release. RTEMS 6 goes from "type-checks" to a verified embedded target —
a reactor-free execution model, blocking CA/PVA drivers, worker-pool thread
accounting, and `pva://`/`ca://` links, all measured on QEMU/BSP hardware — and
Linux PREEMPT_RT becomes a first-class real-time deployment with
priority-inheritance locking proven on a real RT kernel. The runtime/socket
layer is extracted into the new `epics-libcom-rs` crate, and the async test
suites move onto `#[epics_test]`, whose driver is selected by the build's
backend. Four breaking API changes (epics-ca-rs, epics-pva-rs) plus one on the
iocsh surface. Workspace version 0.24.3 -> 0.25.0, internal
`[workspace.dependencies]` pins move to 0.25.0 in lockstep.

### Breaking

- **epics-ca-rs: the audit and replay writers go through the filesystem
  seam.** File-backed constructors take the seam handle instead of opening
  `std::fs` paths directly, so the RTEMS target and tests control the sink.

- **epics-pva-rs: the ACF host is derived from the peer socket, never from
  the wire.** A client-asserted host name can no longer influence access
  decisions; `with_server_derived(peer)` is the single funnel.

- **epics-pva-rs: `ChannelSource` returns the monitor stream, not an
  `mpsc` receiver.** Custom sources hand back the stream type; the server
  owns queueing/pipelining uniformly (this is what lets one source serve
  both the tokio and the RTEMS blocking drivers).

- **epics-pva-rs: the RTEMS dependency gate moved off `server_native::tcp`
  onto the accept path**, so the module surface compiled for the target no
  longer drags the tokio server in.

- **epics-base-rs: the iocsh surface no longer traffics in
  `tokio::runtime::Handle`.** `CommandContext::runtime_handle()` is gone;
  `CommandContext::bridge()` returns a `runtime::task::BlockingBridge`
  (tokio backend: a captured handle; exec backend: the global executor), and
  `IocShell::new` / `optics-rs::seq_start` take the bridge. Registered
  commands become startable on runtimes without a tokio reactor — RTEMS
  included — and a command that tries to smuggle a raw handle is now a
  compile error.

### RTEMS 6 (armv7-rtems-eabihf)

The workspace cross-compiles and *boots*: `rtems-ca-ioc` and `rtems-pva-ioc`
serve CA and PVA (including QSRV Q:group PVs) from a libbsd BSP, verified on
QEMU xilinx-zynq with live client traffic.

- **Reactor-free execution model** (`rtems-exec-model`): the `runtime::task`
  seam routes `spawn`/timers to a cooperative band-priority background
  executor (release-on-Pending, re-enqueue-on-wake), and connection I/O runs
  on parked blocking threads (`park_on`). No tokio reactor exists on target.
- **Blocking protocol drivers**: sans-io CA server/client and the PVA
  server/client byte paths run on dedicated blocking threads with the same
  wire behavior as the async drivers (refusal parity, recv accumulation
  caps, ECA_TOLARGE/ECA_ALLOCMEM).
- **Thread accounting via worker pools**: CA server clients, PVA connection
  sets, and every protocol dial borrow threads from bounded pools
  (capacity = fd wall − 1, so the refusal path always keeps a descriptor),
  closing the per-thread 128 B RTEMS TLS leak and the per-attempt creation
  residue. A CA circuit is retired the moment either pump dies (per-circuit
  death guard), so a dial broken after establish always reaches the redial
  owner.
- **`pva://` and `ca://` links on target**: pvalink and calink run over
  name-server TCP search (no UDP socket on the target arm), mounted in the
  IOC binaries; on-target two-IOC gates pass for both.
- **Scanning hoisted into the IOC core** (`ScanOwner`), and QSRV group
  drains moved to a dedicated thread — never a shared band worker.
- **Real thread priorities**: every IOC thread is banded
  (`posix = 56 + epics`, a deliberate deviation from base-on-RTEMS-6's
  linear map, recorded in `doc/`), measured on target via SCHED_FIFO.
- **Toolchain**: `epics-rtems-boot` carries the boot shim and link
  contract; the `has-thread-local: true` target-spec deviation is applied
  automatically by a rustc wrapper — plain `cargo build` is the whole
  interface, and `./scripts/rtems-check.sh` type-checks the closure without
  a cross toolchain.

### Linux PREEMPT_RT

- **`epics-base-rs/linux-rt`**: the record-gate and scan-side lock family
  becomes `PTHREAD_PRIO_INHERIT` priority-inheritance mutexes; every gate
  scope was restructured to hold zero awaits (dbProcess parity), external
  link opens are staged at iocInit (dbCaAddLink parity), and link puts go
  through dbCa-parity staging.
- **Opt-in SCHED_FIFO banding** on hosted targets via
  `EPICS_RS_ALLOW_RT_PRIORITY=YES` (default on for RTEMS, off elsewhere).
- **Measured**: on a PREEMPT_RT kernel the record-gate priority inversion
  collapses from 24.9 ms to 10.1 ms worst-case with PI on
  (`doc/rtlinux-rt-measurement.md`); the scan leg is solved by FIFO
  (84.7×). `examples/rt-probe` is the measurement rig.

### New crate: `epics-libcom-rs`

- **`epics_base_rs::runtime` and `epics_base_rs::net` are now their own
  crate.** They move to `epics-libcom-rs` — named for C's `libCom`, whose
  scope is exactly these two halves (`epicsThread`/`epicsTime`/`errlog`/
  `envDefs` and `osiSock`) — so a consumer that wants the concurrency and
  socket primitives can take them without the record system. (#55)

  **No downstream source change.** `epics-base-rs` re-exports both modules at
  their original paths (`pub use epics_libcom_rs::{net, runtime};`), so
  `epics_base_rs::runtime::…` and `epics_base_rs::net::…` resolve as before.
  `WallTime` moved with its producer and is re-exported at
  `epics_base_rs::types::WallTime`. Both feature levers are forwarded, so
  `--features epics-base-rs/linux-rt` and
  `--features epics-base-rs/rtems-exec-model` are unchanged for callers.

- **Two seams that used to be intra-crate are now pinned at compile time**
  (`const _: () = assert!(…)`): the scanOnce band count against the
  periodic-rate count, and the `exec_backend`/`tokio_backend` predicate both
  build scripts derive against `epics_libcom_rs::EXEC_BACKEND` — a feature
  forward that stops being wired fails the build instead of splitting the
  workspace across two task backends.

### optics-rs (SNL)

- **SNL `pvPut` now translates to put-and-process, not put-and-post.**
  seq's `pvPut` is a CA put — dbPutField semantics: write the field, then
  process through the PP gate. The ports wrote-and-posted without ever
  processing, so every readback the state machines maintain stayed
  UDF/INVALID forever. All 70 sites across the six SNL ports now go through
  the dbPutField fire-and-forget tier, carrying the writer origin (simple-PV
  posts are origin-tagged end to end) so a record's own posts are not
  mistaken for external CA puts. (#57)

### Testing: `#[epics_test]`

- **The backend picks the test driver, not the test.** `#[epics_test]`
  expands to a plain `#[test]` driving the body through
  `runtime::task::test_block_on` — tokio `current_thread` on the default
  backend, `park_on` under `rtems-exec-model` — so the feature-ON suites
  exercise 1,251 migrated test bodies through the exec seam.
  Reactor-bound tests (tokio::net via production code, flavored runtimes,
  tokio-as-subject) stay `#[tokio::test]`, and each crate's
  `rtems_exec_model_gate` census pins their exact count — a new unmarked
  `#[tokio::test]` fails the gate. (#58)

### CI

- New standing gates: the RTEMS closure type-check, `linux-rt`,
  `rtems-exec-model` (plus a 50-iteration exec-backend cancel stress job),
  and `no-default-features`. `cargo fmt` runs in CI
  (thanks @bolinocroustibat, #50; also #51, #53).

### Fixed

- **epics-ca-rs**: the server's per-client worker pool capacity is 141
  (fd wall − 1) — at 142 the wall client's accept fails ENFILE before the
  pool can refuse, so the documented ECA_ALLOCMEM refusal could never
  execute; a refusal that happens after accept needs a descriptor to happen
  on. Also: nameserver recv cap; circuit-retirement redial ownership (see
  RTEMS section).
- **epics-base-rs**: both iocsh threads are banded at
  `ThreadPriority::Iocsh`; CP link holders process on access-rights loss
  (dbCa.c:1076-1102 parity); `cancel()`/`is_done()` terminal state has a
  single owner (`reached_terminal_state`), fixing an exec-backend
  cancel/completion race pre-existing on the seam.
- **epics-pva-rs**: monitor connection transitions are owned by
  `ConnEventOwner`; `SubscriptionHandle::wait` removed; the gateway
  disconnect boundary fires on plain upstream loss.

## v0.24.3 — 2026-07-20

Patch release. Adds the `dbLoadTemplate` iocsh command to `epics-base-rs`,
plus three parity fixes across `epics-base-rs`, `epics-ca-rs`, and `asyn-rs`.
Additive API only; no breaking changes. Workspace version 0.24.2 -> 0.24.3.

### epics-base-rs

- **New `dbLoadTemplate(subFile [, globalMacros])` iocsh command** — the
  counterpart to `dbLoadRecords` for `.substitutions` template files. It
  expands each pattern row and installs the resulting records through the
  *same* routine as `dbLoadRecords` (identical duplicate-name merge,
  `apply_fields`, load-then-init ordering, alias registration, and
  post-load passes), so a template-loaded record is byte-for-byte identical
  to a directly-loaded one. Command-line global macros are the lowest
  precedence and are overridden per row, matching C `dbLoadTemplate`. The
  `dbLoadRecords` install loop and include-path resolution are extracted
  into shared helpers both commands now use. (#47)

- **`histogram` signals inverted limits consistently (CBUG-F12 refused).**
  When `LLIM >= ULIM` the record now raises the alarm through `nsta`/`nsev`
  on *both* the compute and array-read paths, instead of reproducing the C
  bug on one of them.

### epics-ca-rs

- **CA server clamps a request's element count to the channel capacity.**
  A read/subscribe asking for more elements than the channel holds is
  clamped to the channel's max rather than honored as given.

### asyn-rs

- **Both asyn escapers render NUL as `\0` through one shared table
  (CBUG-D4 refused).** The two escape paths previously diverged; a NUL byte
  is now escaped identically on both.

### epics-pva-rs

- **Doc fix:** corrected a stale `ack_at_from` comment that still referenced
  the removed CBUG-B12 sentinel. No behavior change.

## v0.24.2 — 2026-07-19

Patch release. Two DTYP-resolution fixes in `epics-base-rs` (the `dbpf`
device-support path), plus a documentation reclassification (CBUG-B25). Additive
API only; no breaking changes. Workspace version 0.24.1 -> 0.24.2.

### epics-base-rs

- **`dbpf <rec>.DTYP <device-support-name>` in an st.cmd no longer fails
  iocInit.** `cmd_dbpf` took the field type from `declared_field_type_of(DTYP)`
  (reports `Enum`) and parsed the value through the small static menu table, so
  every device-support name (`"Async Soft Channel"`, `"Asyn Scaler"`, …) missed
  the table and returned `Err("invalid enum or menu string")` — and inside an
  st.cmd that aborts iocInit before the CA server binds. DTYP values now route
  straight to the put path (a numeric index → `Enum(i)`, a name → `String`
  validated against `device_choices()`); the generic Enum parse still covers
  every non-DTYP field.

- **DTYP put validation/store now uses the merged device menu, not the
  static-only half.** The read/announce side (`device_choices`) already merged
  the static `device()` declarations with runtime-contributed device support
  (asyn, scaler-rs), but the two write sites (`coerce_put_value` CA-put
  validation, `put_common_field`'s DTYP Enum arm) consulted only the static
  half — so a contributed name failed validation, and for a downstream custom
  record type with no static device menu (scaler) it fell through to
  `S_db_noRSET`. Both write sites now route through a single-source helper
  `device_menu_registry::merged_device_menu` (declared + contributed), so put
  and read stay symmetric: a client can put exactly the DTYP names it can read.

### ad-plugins-rs

- **CBUG-B25 reclassified: the port already matches upstream.** The
  `NDPluginTimeSeries` integer averaging has divided *before* narrowing (the
  correct order) since `d8f27b88`; ADCore #596 (merged upstream 2026-07-16)
  applies the same fix to C, so the port and current upstream C now agree. The
  module header and `averaged_value` doc comment are reframed from a live
  "deliberate deviation / reproduces C's bug" to "matches upstream since #596";
  the worked example now shows the correct `200`, not C's pre-#596 `29`. No
  behaviour change.

### Docs

- `doc/upstream-c-bugs.md`: reconciled the upstream-PR submission status against
  live GitHub (20 PRs by author, was 4 in the stale catalogue), added a single
  authoritative filed-PR table, and reclassified CBUG-B25
  REPRODUCED -> NOT-REPRODUCED (fixed upstream #596).

## v0.24.1 — 2026-07-18

Patch release. Closes the Type3 differential-oracle parity gap: after this
release the `epics-oracle-rs` differential harness is DEFECT 0 across all three
phases (Channel Access, PVA read, PVA monitor). Eight fixes, one per finding,
each grounded in the C reference (`epics-base`, `pvxs`). Additive record-trait
API only; no breaking changes. Workspace version 0.24.0 -> 0.24.1.

### epics-base-rs

- **sseq TIME lags the VAL post by one cycle.** `sseqRecord.c::asyncFinish`
  posts VAL and runs `recGblFwdLink` *before* `recGblGetTimeStamp`, so the VAL
  monitor carries the pre-update timestamp and the restamp advances TIME for the
  BUSY post and the next cycle. New hook `Record::restamps_time_after_completion()`.

- **A sync UDF mbbo/mbboDirect leaves TIME at the EPICS epoch.** C
  `mbboRecord.c` / `mbboDirectRecord.c` take `else if (prec->udf) goto CONTINUE`,
  skipping the pre-output `recGblGetTimeStampSimm`; the only post-`CONTINUE`
  stamp is `if (pact)`-guarded (async completion), so a soft (sync) UDF record
  never stamps until VAL is defined. New hook
  `Record::skips_timestamp_when_undefined()` (mbbo/mbboDirect only). The
  async-completion path still stamps unconditionally, matching C's `if (pact)`.

- **An empty-SNAM aSub resets VAL and stops over-posting on scan.** C
  `aSubRecord.c` short-circuits an empty SNAM `do_sub` to `return 0` before the
  `S_db_BadSub` check, so `process` sets VAL to 0 each cycle and `monitor()`
  posts nothing when unchanged. The port had conflated empty-SNAM with bad-SNAM
  and never wrote VAL, so a `.1 second`-scanned aSub re-posted the stale put
  value on every scan.

### epics-pva-rs, epics-bridge-rs

- **`alarm.message` now serves the record's own `amsg`** (native PVA and
  QSRV/bridge), preferring a non-empty carried amsg over a re-synthesized
  condition string (pvxs `iocsource.cpp:230-236`). Closes the fabricated-amsg
  family: generic UDF records fall back to `"UDF"`, only `mbboDirect` uses
  `"UDFS"`, and the CALC-family literals are corrected to their real C strings.
  New seam `Record::udf_alarm_message()`.

### epics-oracle-rs

- DTYP parity is exercised (asyn device menus registered; asyn device support
  served in the DTYP choice list), the ASYN.BOUT live-length divergence is
  recorded as an intentional design-divergence, and QSRV2 demo device support
  is allowlisted as an instrument superset.

## v0.24.0 — 2026-07-18

Minor release. A full C-parity hardening pass driven by the new differential
oracle (`epics-oracle-rs`), which boots a C `softIoc` and the Rust port on the
same database and diffs their Channel Access and PVA behaviour: roughly 800
fixes landed across asyn, the calc engines, the database/record metadata
surface, Channel Access, and PVA/QSRV2. Two breaking asyn API changes plus new
oracle differential-testing phases. Workspace version 0.23.0 -> 0.24.0,
internal `[workspace.dependencies]` pins move to 0.24.0 in lockstep.

### asyn-rs

- **BREAKING: `ParamSetValue`'s per-type variants are replaced by
  `ParamSetValue::Value { reason, addr, value: ParamValue }`** (plus a
  `UInt32Digital` masked set). The actor-path carrier had enumerated only a
  subset of the parameter types the store supports, so a driver background
  thread pushing e.g. an `Int64` or `Float32Array` through
  `set_params_and_notify` silently lost the update. The carrier now holds a
  `ParamValue` applied through one exhaustive `ParamList::set_value`, so a new
  variant is a compile error rather than a dropped update, and a set the
  parameter cannot hold is now traced (`ASYN_TRACE_ERROR`, as C
  `asynPortDriver::setIntegerParam`) and returned instead of vanishing with a
  success reply. Build values with `ParamSetValue::new` and
  `ParamSetValue::uint32_digital`; `UInt64`/`UInt64Array` are refused with an
  explicit error.

- **BREAKING: `PortDriver::drv_user_create`, `PortHandle::drv_user_create` and
  `PortHandle::drv_user_create_blocking` take `&DrvUserRequest` instead of
  `(&str, i32)`.** A record's bind now carries the interface the record reads
  the parameter through (its DTYP), not just the drvInfo string and addr, so
  an on-demand driver (C Autoparam lazy creation) no longer has to guess the
  parameter type and silently bypass the record's conversions (ai
  ASLO/AOFF/SMOO, asynInt32 ai ESLO/EOFF, bi/mbbi RVAL state tables).
  `DrvUserRequest` is `#[non_exhaustive]` with a `new` + builder; in-tree
  drivers (modbus, mqtt) that name the type in their drvInfo are unaffected.

- **`PortHandle::submit_blocking` no longer panics inside the framework's own
  runtimes.** It reached for `tokio::task::block_in_place`, which panics
  unless the runtime is multi-threaded — but the port actor
  (`PortActor::run_with_shutdown`) and every `ad-core` driver thread
  (`ad_core_rs::runtime::run_thread_named`) build **current-thread** runtimes.
  Any driver doing blocking device I/O from its own task therefore aborted
  that thread on its first transfer: `can call blocking only when running on
  the multi-threaded runtime`. Motor drivers escaped it only because
  `#[epics_main]` happens to build a multi-threaded runtime.

  The predicate "am I inside a runtime?" is replaced by the one that actually
  decides whether blocking is safe: **"am I the thread that would have to
  service this request?"**. Each port actor now carries an identity that it
  publishes on its own thread, and every blocking entry point on `PortHandle`
  (`submit_blocking`, `set_params_and_notify_blocking`,
  `AsyncCompletionHandle::wait_blocking`, and so all of `SyncIOHandle`) goes
  through a single gate:

  - Called from the port's **own actor thread** — a `PortDriver` method
    calling back into its own port — it returns an error naming the port.
    Blocking there can never be woken: the actor is the thread that would have
    to run the request it is waiting on.
  - Called from **anywhere else** — a plain thread, a current-thread runtime,
    a multi-threaded worker — it parks the caller on the actor's reply. The
    actor owns its own OS thread, so the reply arrives regardless of what the
    caller's runtime is doing.

  **Behaviour change:** `set_params_and_notify_blocking` previously returned
  an error whenever it was called from inside *any* tokio runtime. It now
  succeeds there (which is what an acquisition task on an `ad-core` driver
  thread needs) and errors only on the genuinely unserviceable case above.

- ~110 further asyn parity fixes across `CallParamCallbacks`, drv_user
  plumbing, trace/report output, and the interface adapters.

### calc / sCalc / aCalc

- `NINT` and `MODULO` now narrow their operands per dialect, mirroring each
  engine's own C: base `calcPerform` routes through `d2i()` (matching
  epics-base PR #925, which fixes an `INT_MIN % -1` SIGFPE and the raw
  double→int cast), while the sCalc/aCalc engines keep their own conversions.
  ~100 further parity fixes across the calc / transform / swait / sseq /
  acalcout / scalcout record family.

### epics-base-rs — records & database metadata

- **`aai` / `aao` / `subArray` VAL no longer advertise a 0-length Channel
  Access channel.** `field_native_count` returns each record's `cvt_dbaddr`
  capacity (`nelm`, or `malm` for `subArray`), so array puts and gets over CA
  are served instead of refused with "Invalid element count requested".
- The record metadata rsets (`get_graphic_double`, `get_control_double`,
  `get_alarm_double`, units / precision) are now routed per field from each
  record type's own C source, with a single owner per slot, instead of being
  read from the VAL cache. ~100 fixes across `db` / `records` / `base`.
- Ported the `dbDbLink` / `dbConstLink` metadata `lset` slots.

### bi record

- Added the `AFTC` / `AFVL` alarm-range filter fields (EPICS PR #817 parity).

### epics-pva-rs / QSRV2

- Serve `display.precision` that pvxs QSRV2 drops when a field NULLs
  `get_graphic_double` (CBUG-G1; a documented deviation from upstream pvxs).
- An external PUT is routed through `dbPutField`, a monitor update is marked
  by its own DBE class rather than the whole NT, and the double→NT integer
  leaf step uses C++ cast semantics. ~30 further PVA / gateway parity fixes.

### epics-ca-rs

- `ECA_PUTCBINPROG` is 362, not 366. ~40 Channel Access client / server and
  catools parity fixes.

### epics-oracle-rs

- New PVA monitor phase (`--phase pva-monitor`): subscribe, drive, and diff
  the monitor events between the C IOC and the port.
- New array phase measuring the `aai` / `aao` / `subArray` / `waveform` VAL
  seam.
- `pvxput` / `pvxmonitor` added to the PVA instrument.

## v0.23.0 — 2026-07-12

Minor release. The workspace version is bumped to `0.23.0` and the internal
crate dependency requirements move from `0.22.0` to `0.23.0` in lockstep. The
bump is owed by a **breaking** removal in the db loader; the rest is bug
fixes (one commit per finding, PRs #11–#16) plus one additive test-sync API.

### db loader (epics-base-rs) — **BREAKING**

- **`dbLoadRecords` `DTYP=` is a plain macro, not a force-override.** In C,
  `dbLoadRecords` macros are pure text substitution (`dbLexRoutines.c` runs
  them through macLib during lexing) with no DTYP special case. The Rust
  loader additionally rewrote the DTYP field of *every* record in the loaded
  file that had one, corrupting any multi-record file whose helper records
  carry a literal DTYP — the vendored synApps `scaler.db` family is exactly
  that shape: loading with `DTYP=Scaler-rs` silently rebound the two
  `Soft Channel` `bo` helpers to the hardware DTYP. The override is deleted
  rather than gated, restoring the uniform rule: macros substitute only
  where the file writes `$(DTYP)`.

  **Removed:** `db_loader::override_dtyp` (the force-override's only
  consumer was `dbLoadRecords` itself). (#14)

- **Device-support init now receives the link text of record-owned
  INP/OUT.** Record types that declare INP/OUT in their own `field_list`
  (scaler, motor, acalcout, epid, throttle — mirroring their C `.dbd`)
  routed the field to `Record::put_field` only, so the
  `DeviceSupportContext` handed every dynamic device-support factory
  `ctx.out == ""` — a factory that disambiguates hardware by parsing the
  link (C `devScalerAsyn.c` picks its board from `prec->out`) could not
  tell two boards apart. The two meanings the common link fields carried
  are split: `common.inp`/`.out` is always the link *text* the `.db` wrote,
  for every record type; `parsed_inp`/`parsed_out` is the framework's
  dispatch of that link and stays unarmed when the record type owns the
  field (the record drives the link itself — the framework must not drive
  it twice). Ownership is derived from `field_list()`, so a new C-faithful
  record type gets the correct behaviour by construction. (#12)

### areaDetector (ad-core-rs, ad-plugins-rs)

- **`NDFileNumCaptured` is published per frame in stream mode.** C++
  `NDPluginFile::processCallbacks` sets it after every successful stream
  write; the port pushed it only when the NumCapture target was reached, so
  an unbounded stream (`NumCapture=0`, the ophyd-async writer convention)
  reported `NumCaptured_RBV=0` forever and bounded streams showed no
  progress until completion. (#11)

- **`$(ADCORE)` resolves from ad-core-rs itself, not a sibling-directory
  guess.** `AdIoc::new()` built asset paths out of ad-plugins-rs's own
  `CARGO_MANIFEST_DIR` (`"../ad-core-rs"`, `"../calc"`, …) — under a
  registry checkout the sibling is version-suffixed, so `$(ADCORE)` never
  resolved and every AdIoc-based IOC died before `iocInit()` on
  `< $(ADCORE)/ioc/commonPlugins.cmd`. ad-core-rs now exports
  `AD_CORE_DIR` (the convention std-rs/scaler-rs/motor-rs already use) and
  AdIoc, mini_ioc and xrt_ioc consume it. `CALC`/`BUSY`/`AUTOSAVE` pointed
  at crates epics-rs does not ship and are dropped: AdIoc publishes a path
  only for assets it actually owns. (#13)

- **AdIoc st.cmd can now create and configure asyn ports.** AdIoc never
  registered the asyn iocsh command set, and the commands it did have
  resolved port names through a different registry than the one
  `drvAsyn*PortConfigure` published into — so the socket-detector prologue
  (Pilatus/marCCD-style `drvAsynIPPortConfigure` + EOS + trace setup) was
  either an unknown command (fatal before `iocInit()`) or a silent
  "port not found". The full asyn command set is now installed on both the
  startup and interactive shells, and every port publishes into and
  resolves through the one process registry. Additive API: `AdIoc::ports()`,
  `AdIoc::app()`, `IocApplication::startup_commands()`,
  `PortManager::with_trace_manager()`, asyn_record `port_names`/
  `unregister_port`. (#15)

- **Plugin tests no longer synchronize by sleeping.** The control→data
  param channel now carries a `Barrier` message the data thread acks only
  at full quiescence — every previously queued param change applied AND
  the array queue drained — exposed as
  `PluginRuntimeHandle::wait_params_applied(timeout)` (additive API). All
  51 sleep-synchronized test sites across ad-core-rs and ad-plugins-rs are
  converted to the barrier fence or to polling an observable the frame
  flushes — the two tests that flaked on a loaded CI runner
  (`test_rewire_ndarray_port_at_runtime`, `test_driver_to_stats_pipeline`)
  among them. The array-queue condition exists because arrays travel a
  separate channel: a param applied while an older array still waits in
  the queue would retroactively change how that array is processed.
  (#16, #17)

## v0.22.1 — 2026-07-08

Patch release. The workspace version moves to `0.22.1`; the internal crate
dependency requirements stay at `0.22.0` (caret `^0.22.0` resolves the new
`0.22.1` crates). No public API change — a bug fix plus a docs edit.

### CA client (epics-ca-rs)

- **Do not emit `NativeTypeChanged` on first connect.** `NativeTypeChanged`
  is a *transition* signal — a channel's native DBR type changing across
  reconnects (an IOC redefining the record, or a reconnect to a
  differently-typed record). On the first connect there is no prior type:
  `previous_native` is `None`, so `(None, Some) => true` in the
  `native_changed` match made every first connect emit `NativeTypeChanged`
  even though the type was merely *discovered* — the `Connected` event
  already carries it. For consumers that refetch CTRL metadata on
  `NativeTypeChanged` (rsdm's CA plugin, PyDM-style layers) this fired a
  redundant metadata round-trip on every connection, and where the connect
  handler was not idempotent it re-emitted the initial value, leaking a
  duplicate sample. The consumer-facing event is now gated on
  `previous_native.is_some()`, restoring parity with epics-base `168775775`
  (`camonitor`'s `onceConnected`-guarded type-change path — first connect
  never triggers it). The `native_changed` flag is unchanged; it still
  drives the auto-derived decoder reset in `restore_for_channel`.

### docs

- **README Motivation section de-personalized** — the first-person "As a
  controls engineer … I needed" passage is rewritten into a neutral
  project voice; substance and the concrete sim-detector example are
  unchanged.

## v0.22.0 — 2026-07-06

Minor release. The workspace version is bumped to `0.22.0` and the internal
crate dependency requirements move from `0.21.0` to `0.22.0` in lockstep. The
bump is owed by a **breaking** removal in the motor framework; it also adds two
asyn octet iocsh commands.

### motor (motor-rs, asyn-rs, epics-base-rs) — **BREAKING**

- **Removed the position-compare-output (PCO) surface.** The PCO base API had
  been ported as C parity with `epics-modules/motor` commit `05b25c1d`
  (PR #248), which adds `enablePCO` / `PCO_*` params to the base
  `asynMotorAxis` class. That commit lives only on the
  `add_position_compare_output` branch — the PR is **open and not merged** to
  `master`, and `master`'s `asynMotorAxis` has no PCO API. Tracking an unmerged
  PR as if it were upstream base API was wrong. PCO-capable drivers (Aerotech,
  Newport XPS, Galil, ACSMotion) expose PCO driver-privately (e.g. iocsh
  commands) until upstream actually merges a base interface to mirror.

  Removed surface:

  - **asyn-rs**: `AsynMotor::enable_pco` / `set_pco_config` trait hooks (a
    comment at the trait records why PCO is intentionally absent).
  - **motor-rs**: `PcoFields`, `MotorRecord.pco`, `CommandSource::PcoEnable`,
    `MotorCommand::EnablePco` / `SetPcoConfig`, the planner `PcoEnable` arm,
    device-support dispatch, the `PCO_*` record fields (descriptions +
    get/put), `SimMotor` PCO state, and the PCO tests.
  - **epics-base-rs**: `PCO_ENABLE` from the motor `pp(TRUE)` extension list.

  `PCOF` / `ICOF` / `DCOF` (PID coefficients) are unrelated and untouched.

  This removes public API from asyn-rs (trait methods) and motor-rs
  (fields / enums / record fields) shipped since `v0.18.0`, which is why the
  release is a semver-minor bump rather than a `0.21.x` patch.

### asyn (asyn-rs)

- **`asynOctetSetInputEos` / `asynOctetSetOutputEos` iocsh commands** —
  registered in `build_asyn_commands`, matching C `asynShellCommands.c`. Both
  the `IocApplication` and direct-shell registration paths gain the commands.
  The `eos` argument is escape-decoded via `raw_from_escaped` (a port of EPICS
  `epicsStrnRawFromEscaped` in libcom `epicsString.c`), so a literal `"\r\n"`
  typed in `st.cmd` becomes the two bytes CR LF. Routing reuses the existing
  `PortHandle::set_{input,output}_eos_blocking` path, so the driver remains the
  single owner of the 2-byte terminator limit. `addr` is accepted for CLI
  parity but not routed (EOS is port-wide on single-address octet ports). The
  `Get` variants are not included. Fills the long-standing gap where input /
  output EOS could not be set from iocsh.

## v0.21.0 — 2026-07-03

Minor release. The workspace version is bumped to `0.21.0` and the internal
crate dependency requirements move from `0.20.0` to `0.21.0` in lockstep,
alongside an HDF5 dependency bump and one additive HDF5 writer option.

### areaDetector plugins (ad-plugins-rs)

- **rust-hdf5 `0.2.28` → `0.3.2`**, with the `parallel` feature enabled
  (`rayon` + `deflate`) alongside the existing `threadsafe` and
  `all_filters`. The `parallel` feature parallelises HDF5
  compression/IO via `rayon` (pure Rust, no MPI), improving throughput on
  the HDF5/NeXus file writers. The `0.3.x` feature set is otherwise
  identical to `0.2.28`.
- **`AD_HDF5_FSYNC_ON_CLOSE` env var** — opt-in no-fsync fast close for the
  HDF5 plugin's standard (non-SWMR) write path. Unset (or any value outside
  `0`/`false`/`no`/`off`) keeps the durable default. Setting it falsey skips
  the close-time fsync via rust-hdf5 0.3.2's `H5File::close_no_sync`, cutting
  close latency (fsync of a large dataset dominates close — hundreds of ms)
  at the cost of durability against power/OS crash until the OS flushes its
  page cache. The file is still finalized (complete and readable) on close;
  only the fsync is skipped. SWMR is unaffected. Process-global: it applies
  to every HDF5 file the IOC writes.

## v0.20.4 — 2026-07-02

Patch release: 511 commits on top of `v0.20.3`, one commit per finding
(the `fix(...)` / `feat(...)` git log is the ledger) — 261 fixes and 58
additive parity features, plus refactors and tests. No breaking public
API changes (the new trait/method surface is additive). The sweep is
dominated by an asyn device-support port (`asyn-rs`), a `base` record and
device-support parity pass (`epics-base-rs`), and a full `procServ` port
in `epics-tools-rs`, with further module-driver, gateway, and protocol
fixes.

### asyn (asyn-rs)

Large asyn device-support and driver port:

- **Octet family**: `asynOctetCmdResponse` (stringin/waveform/lsi write a
  command then read the reply), `asynOctetWriteBinary` (waveform,
  full-NORD bytes), `write_octet` returns bytes transferred, and a
  per-record octet length cap threaded through `drv_user_create`.
- **EOS interposition**: auto-install the EOS interpose on
  `drvAsynIPPortConfigure` and the FTDI configure path, and route runtime
  EOS through the `OctetInterpose` stack (DRV-2 family).
- **Averaging / TimeSeries**: `asynInt32Average` / `asynFloat64Average`
  device support, Average Mode 1 (I/O Intr SVAL-decimation), and
  `devAsynXXXTimeSeries` waveform device support.
- **Conversions**: `asynFloat64` ao ASLO/AOFF readback + write conversion.
- **Serial / GPIB**: a Win32 serial backend, `drvAsynSerialPortConfigure`
  and serial configure with default EOS, Prologix GPIB
  (`prologixGPIBConfigure`, `AsynGpib`), and serial fd / byte counters in
  `report()`.

### base/db (epics-base-rs)

Record and device-support parity:

- **New records**: `permissive`, `state`, and the array `aCalcout` (with
  ODLY delayed output); `scalcout` ODLY/DLYA output delay.
- **Device support**: "Db State" (`devBiDbState` / `devBoDbState`), "Soft
  Timestamp" (`devTimestamp.c`), and "stdio" (`devStdio.c`).
- **Record fields / behaviour**: aSub EFLG / LFLG / SUBL / PREC / INAM,
  ai SVAL, waveform RARM / BUSY TimeSeries control, aao OMSL=closed_loop
  DOL array pull, LINR>=3 breakpoint-table linearisation for ai/ao, ao
  raw→eng readback back-convert, swait OUT-write / OEVT deferral by ODLY,
  OEVT output events for the calc-output family, and the `Q:form` info tag
  wired into the display form index.

### epics-tools (procServ)

A full `procServ` port: command-key flags with caret notation, the
control-endpoint surface (`-P` specs, interface bind, UNIX-socket
permissions, abstract sockets), C-exact child-lifecycle and
welcome-banner messages, `--coresize` / RLIMIT_CORE, `--killsig` / `-K`,
`--noautorestart` / `--oneshot`, `-e` / `--exec`, `--logfile -` to stdout,
the `PROCSERV_PID` default pidfile, `-d` / `-q` / `PROCSERV_DEBUG`, the
C-faithful `-c` / `-I` / `-n` aliases, and the `-p` / `-P` swap to match C.

### modules (modbus / mqtt / motor / optics / std)

Driver and record parity fixes across `modbus-rs`, `mqtt-rs`, `motor-rs`,
and `optics-rs`; `std-rs` throttle SYNC valueSync with OV/SIV
classification (no spurious process on non-VAL puts).

### bridge / qsrv

QSRV and dual-gateway fixes (CA and PVA gateways, pvalink), including
config-key precedence (CLI vs TOML on `clap` value source) and the
GW-series shadow-metadata / reconnect convergence work.

### pva / ca

The `pvxsr` iocsh server-report command, `$SSLKEYLOGFILE` TLS secret
logging, single-owner `ServerReport` assembly and `EpicsValue`↔PVA
value-leaf mapping, and a v6-beacon loopback test deflake.

### ad-plugins / ad-core

`rust-hdf5` bumped `0.2.23` → `0.2.28`.

## v0.20.3 — 2026-06-22

Patch release: 118 C-parity regression fixes plus 5 additive parity
features on top of `v0.20.2`, one commit per finding (the `fix(...)` /
`feat(...)` git log is the ledger). The sweep is dominated by a full
areaDetector plugin pass (`ad-plugins` / `ad-core` against
`epics-modules/ADCore` and the plugin suite) and the round-5
record-processing field-output review of `base`. No breaking public API
changes — one additive `DeviceSupport` trait-method pair (default no-op)
for the asyn output-callback ring fix.

### ad-plugins (areaDetector)

Round-3 full-plugin parity sweep against the C plugin suite plus an HDF5
file-writer overhaul:

- **HDF5**: the default layout is the C NeXus tree (not flat `/data`);
  multi-extra-dimension datasets build N+1 fixed leading axes on the
  standard and uncompressed-SWMR paths (`configureDims`, via rust-hdf5
  0.2.21→0.2.23 `write_chunk_at`); N-bit packs a reduced-precision
  datatype (degrading to lossless when unrepresentable), SZIP uses the
  nearest-neighbor mask, BLOSC defaults to byte shuffle with the correct
  `cd_values` slot order; every dataset carries its ndattribute
  element-attributes, `NDArrayNumDims` / `NDArrayDim*`, and the four
  self-describing `NDAttr*` attributes; multi-detector frames route by
  `detector_data_destination`; pre-compressed arrays take a direct chunk
  write; performance / attribute datasets chunk at the C targets; string
  attribute datasets are rank-1 fixed-length `H5T_C_S1`.
- **Codec**: `bslz4` rewritten to the canonical bitshuffle+LZ4 byte
  format, Blosc `cd_values` use the standard `H5Zblosc` slot order and
  default clevel 5, and the codec processor is compression-aware so
  Decompress receives compressed input.
- **Plugins**: a full `NDPosPlugin` (17 params, XML position parsing,
  `ExpectedID` stepping, `NDPos_CurrentPos`); ROI / ROIStat color
  collapse, RGB `dataType`, edge clamping, and rank-dispatched stats;
  Stats centroid/profile rank guards and `HIST_*` int32 typing; Process
  auto offset/scale arming the next frame, per-frame flat-field validity,
  and C clip order; Overlay shape ordinals, independent Cross arms,
  inclusive Rectangle bounds, and `>=128` text skip; FFT rank from input
  ndims at padded length; CircularBuff NaN/Inf trigger guard, pre-count
  validation, octet status, and per-frame trigger posts;
  Attribute / AttrPlot reset-on-any-write and missing-attribute skip;
  plus JPEG RGB, Bayer borders, netCDF globals, standalone TimeSeries
  ingest, Scatter rerouting, and false-color LUTs.

### ad-core

`NDArrayPool::convert` sums binning windows in the target type; RGB→mono
is unweighted `(R+G+B)/3` truncated; `NDArrayCallbacks=0` withholds
downstream delivery; plugin output publishes `NDCodec` /
`NDCompressedSize`; `NDDimensions` post at fixed `ND_ARRAY_MAX_DIMS`; a
`MinCallbackTime`- or StdArrays-throttled frame is not counted as
dropped; StdArrays serves its waveform regardless of `NDArrayCallbacks`;
scatter reroutes past full consumers; `destination_matches` replicates
`attrIsProcessingRequired`; file-plugin control attrs honor C
string-typed reads.

### base/db

Round-5 record-processing field-output review, one fix per finding:
`calc` / `calcout` `VAL` token reads the previous result; `ao`
Incremental `DOL` increments from `PVAL`; a constant `DOL` is applied
once at init; a `calcout` `ODLY` delaying cycle defers `FLNK` and
`VAL` / `OVAL` monitors; `sel` Specified mode fetches only the selected
input and freezes on a failed fetch; `seq` reads `DOLn` back into `DOn`
and coerces it; `dfanout` folds an `OUT`-link failure into the same-cycle
`LINK_ALARM`; `ai` `RVAL` posts with `VAL`'s raw monitor mask; analog
`checkAlarms` returns early on `UDF`; `waveform` / `aai` / `aao` post
MPST/APST On-Change with a byte-exact `epicsMemHash`; `fanout` / `seq`
`SHFT` defaults to -1; `sub` / `aSub` run on the main engine. Plus
db-load common-field string coercion, `DBF_MENU` label resolution (load +
runtime), `UTAG` load, array `DBR_GR` / `DBR_CTRL` limit metadata, and
non-`Double` input-link coercion.

### asyn

Array I/O Intr converts to the record's interface element type; an
`asyn:READBACK` output record reads the driver callback back into `VAL`
instead of re-writing it (fixing the AD `Acquire` re-trigger loop), and
its callback ring stays balanced when the readback races the record's own
put so the bo returns to Done after a fast acquire (C
`devAsynInt32.c::outputCallbackCallback`).

### trap-write / std / scaler / bridge

`asTrapWrite` Before/After is owned by an RAII guard that fires AfterWrite
on every exit path; `devTimeOfDay` formats the TSE-resolved timestamp;
scaler value-changes post `DBE_VALUE` only; QSRV `String`→integer PUT
uses C base-0 radix.

### optics / mqtt / procserv (synApps)

`optics`: `Io` flux / absorption-coefficient and scaler-`DESC` parity,
and `table` YANG offset rotation, limit zeroing, speed restore, and
Newport user limits. `mqtt`: octet value terminates at the first NUL,
FLAT STRING stores verbatim, FLAT numeric rejects surrounding whitespace.
`procserv`: RFC1143 telnet negotiation and manage-procs info-file format.

### tests

`examples/regression-ioc` gains the J–S family batch (event-MASK routing,
`.PROC` force-process, MS-link alarm propagation, `DBE_PROPERTY` posting,
duplicate-post suppression, array `DBR_GR` / `DBR_CTRL` limits,
record-specific `DBF_MENU`, `UTAG`→PVA userTag); `examples/sim-detector`
gains an end-to-end `Acquire` readback regression.

## v0.20.2 — 2026-06-15

Patch release: 46 C-parity regression fixes plus 2 additive parity
features (asyn runtime enum re-propagation, kohzu/ml-mono tweak
buttons) on top of `v0.20.1`, one commit per finding (the `fix(...)` /
`feat(...)` git log is the ledger). No public API changes. The
output-form sweep covers asyn device support, base/db monitor +
conversion, CA `READ_NOTIFY` error replies, the native PVA monitor and
the QSRV/gateway, motor readback, and the std / scaler / optics synApps
modules.

### asyn

The round-1 asyn parity batch against `epics-modules/asyn` closed
device-support, conversion, and policy gaps:

- Reads surface driver alarm state: I/O Intr and scalar reads map the
  stored param `asynStatus` to a record alarm (C
  `devAsynInt32.c:561-563/843-847`), `callParamCallbacks` skips
  undefined scalars (C `asynPortDriver.cpp:845`), and the asyn record
  raises READ/WRITE/COMM severities with an overflow `MINOR` (C
  `performIO`).
- Conversion parity: `asynInt32` ai routes raw to `RVAL` and runs
  `convert` (C `processAi`); `asynFloat64` ai applies `ASLO`/`AOFF` +
  `SMOO`; an octet I/O error resets the transfer fields (C
  `1547/1560-1631`).
- Queue/transport policy: strict FIFO within a priority (C
  `asynManager.c:1612-1613`), no abort of a dequeued request on the I/O
  timeout, auto-reconnect throttled to one attempt per 2s window (C
  `autoConnectDevice 712-739`), lifecycle/state ops bypass the block
  divert, and `enable`/`disable` refuses a defunct port.
- Link parsing and enums: `@asynMask` 3rd arg is signed `nbits` for
  `asynInt32`, addr/mask parse with C `strtol` base-0 (hex/octal), the
  driver `asynEnum` table propagates onto record state fields at init
  and re-propagates at runtime posting `DBE_PROPERTY`. The delay
  interpose sleeps after every char including the last; the asyn record
  `DBIT` readback queries `"bits"`, not `"csize"`.

### base/db

- A scalar `DBF_CHAR` renders signed in the `DBR_STRING` conversion.
- `MLST`/`ALST`/`LALM` seed from `VAL` at init, suppressing a spurious
  first-cycle monitor.
- An alarm-ack (`ACKT`/`ACKS`) put posts the `DBE_ALARM` mask plus a
  record-wide alarm event.

### CA

- A `READ_NOTIFY` get-failure ships a `dbr_size_n` zero body at the
  requested element count and preserves the server `ECA` code via
  `ServerError`; bad-SID handlers emit an `ECA_INTERNAL` frame before
  closing (C `logBadId`).

### PVA (native + QSRV/gateway)

- Cooked monitor builders emit an empty overrun bitset, and the monitor
  INIT reply subcommand is the state-derived `0x08`, not echoed (pvxs
  `servermon.cpp 135,174-176`).
- A QSRV plain-array group member advertises a scalar-array leaf (pvxs
  `iocsource.cpp:632-643`); the PVA gateway forwards the upstream
  `changedBitSet` to cooked monitors (pva2pva `moncache.cpp:142,189`).

### motor

- `DIFF`/`RDIF` re-post on every device-callback pass (C `3764-3767`);
  `LOAD_POS` leaves `RBV` in the pre-`LOAD_POS` frame (C `3771-3817`).

### std

- `timestamp` posts `VAL` only when the rendered string changes (C
  `timestampRecord.c:152-163`) and rounds the `%03f` fractional field
  to the nearest ms; `epid` `VAL` deadband is owned solely by
  `check_deadband_ext` (C `epidRecord.c:346-374`), and the `OUTL` write
  is gated on `fbon` with early returns.

### scaler

- The soft driver disarm clears counts unconditionally (C
  `drvScalerSoft.c:315-329`); `S1..Snch` re-post with `DBE_LOG` on
  every idle process (C `scalerRecord.c:770-787`).

### optics (synApps)

- `orient`: the Mode constraint mapping matches the C menu order
  (`orient.h 27-29`), and a singular A0/OMTX recalc publishes identity,
  not a stale matrix.
- `kohzu`/`ml-mono`: implements the E/lambda/theta tweak (inc/dec)
  buttons, writes the forbidden-reflection Alert flag from
  `calc2dSpacing`, reverts setpoints to the prior command on a
  soft-limit violation; a standalone ml-mono Y move retracks `yOffset`
  and the Z setpoint.
- `PF4`: Al/Ti/Glass use the analytic absorption-length fits (not the
  Chantler table); `filterAl/Ti/Gl` post only when a blade uses that
  material and the bank is on; `invTrans` posts only when `trans > 0`,
  via a single emitter.
- `flexCombinedMotion`: a standard-mode give-up no longer writes
  `{FM}.VAL`.
- `QXBPM`: `set_defaults` preserves the calibrated offsets, and
  `pos:x`/`pos:y` divide unguarded to match C `NaN`/`Inf`.

### Tooling

- New `examples/regression-ioc` end-to-end harness boots a real
  in-process CA+PVA IOC and pins recurring bug-fix families
  (v0.15.x–v0.20.x) over the wire (11 tests, `publish = false`).
- Lockfile: `aes` `0.9.0 -> 0.9.1` (`0.9.0` yanked).

## v0.20.1 — 2026-06-13

Patch release: two C-parity regression fixes on top of `v0.20.0`. No API
changes.

### Motor: `caput` to a motor field moves the motor again

`v0.20.0` added `"motor"` to the `dbPutField` pp-field processing gate
(`fix(base)` `750851bd`), so a CA put to a `pp(TRUE)` field re-processes
the record only when `SCAN=="Passive"` (C `dbAccess.c:1263-1268`). But
`motor.template` shipped `SCAN="I/O Intr"` — a Rust-only hack to route
SimMotor poll feedback through `setup_io_intr` — so `SCAN != Passive`
and a `caput` to `VAL`/`DVAL`/`RVAL`/`JOG`/... no longer re-processed:
the motor sat idle. `kohzuCtl`'s coordinated moves write `VAL` through
the same gated put path, so the kohzu DCM was a victim too.

The structural cause was that `setup_io_intr` wired poll feedback *only*
for `SCAN=="I/O Intr"`, forcing a motor to choose between poll feedback
(needs `I/O Intr`) and put re-processing (needs `Passive`) when it needs
both. C has no such conflict: `motorRecord` stays `SCAN="Passive"`
(`dbCommon` default) and its asyn `statusCallback` does
`dbScanLock`+`dbProcess` on every readback regardless of `SCAN`.

Fix (`fix(motor)` `12b666fb`): the device support is now authoritative
over its own callback-driven processing via the new
`DeviceSupport::io_intr_scan_independent()` (default `false`; `motor` →
`true`). The I/O Intr wiring processes a record on every callback pulse
when `SCAN=="I/O Intr"` **or** the device reports independence, and
`motor.template` reverts to `SCAN="Passive"`. Behavior is unchanged for
every device that keeps the default.

### asyn: revive the SCAN-independent readback path

`fix(asyn)` `4507d164`: an asyn record flagged
`info(asyn:READBACK,"1")` on a non-`I/O Intr` scan (upstream PRs
#60/#208 — output records that follow driver-side changes regardless of
`SCAN`) had a working `io_intr_receiver` but was never wired end to end,
because the old `setup_io_intr` SCAN gate dropped it. The asyn adapter
now overrides `io_intr_scan_independent()` to `self.asyn_readback`, so a
readback-flagged record is wired and processed on every interrupt
callback even at `SCAN="Passive"`. Records without the info tag are
unaffected.

## v0.20.0 — 2026-06-13

Minor-major release: the motor record completes a full C-parity sweep
against `epics-modules/motor` `motorRecord.cc`, and the monitor
subsystem gains per-field DBE event masks end to end. The version bumps
to `0.20.0` (not a patch) because the motor driver-forward work adds
variants to the public exhaustive `MotorCommand` enum and methods to the
`AsynMotor` trait — a semver-breaking API change.

### Breaking changes

- `MotorCommand` gained `SetPidGain`, `SetHighLimit`, and `SetLowLimit`
  variants so PID-gain (`PCOF`/`ICOF`/`DCOF`) and soft-limit
  (`HLM`/`LLM`/`DHLM`/`DLLM`) puts forward to the driver, mirroring C
  `special()` (pidcof `3003-3026`, soft-limit `4076-4328`). Matching on
  `MotorCommand` exhaustively now requires handling the new variants;
  `AsynMotor` provides default implementations so existing drivers
  compile unchanged.
- New motor fields: `RHLS`/`RLLS` raw limit-switch readbacks, `VERS`
  driver-version, and the momentary command fields `SSET`/`SUSE`/`FOF`/
  `VOF`.

### Motor record C-parity

The round-3 parity sweep closed the remaining `do_work` / `process` /
`special` divergences (one commit per finding; the `fix(motor):` git log
is the ledger). Highlights:

- Closed-loop DOL read failure drives UDF (C `1999-2005`); the
  `LOAD_POS` block exempts the offset-only `SET` redefinition (C
  `2206-2227`); RDBL-error refusal gates only positional moves (C
  `2418-2453`).
- Retry / MIP timing: `RMOD_I` re-arms the settle watchdog
  unconditionally (C `1432-1438`); post-delay retry evaluation gates on
  MIP motion bits (C `1427`).
- The move-block entry gate (`dval != ldvl || !dmov`, C `2240`) now
  refuses an exact same-target re-put after a MISS give-up, falling to
  the chain-end implicit `GET_INFO` (C `2540-2557`) instead of
  re-dispatching. A consequence: an exact same-target re-put no longer
  pulses `DMOV`; sub-step (`< 1` step) requests still do.
- The `dbPutField` processing gate now models the `motorRecord.dbd`
  `pp(TRUE)` set (plus five `special()`-transport extension fields), so
  a put to a non-`pp` field no longer runs a spurious process pass
  (extra FLNK / monitors / implicit `GET_INFO`).

### Monitor posting — per-field DBE masks (epics-base-rs)

- `MonitorEvent` now carries the per-event DBE mask (C
  `db_field_log.mask`), and `ProcessSnapshot` carries per-field posting
  masks, restoring C `db_post_events(field, mask)` granularity: the
  deadband-tracked field narrows per crossing (MDEL → `DBE_VALUE`,
  ADEL → `DBE_LOG`), and the alarm fields post with their individual C
  masks (`SEVR`=`DBE_VALUE`, `STAT`/`AMSG`=`DBE_ALARM`|`DBE_VALUE`).
- The SIMM (simulation) monitor tail now posts per-field with change
  detection and honors the MDEL/ADEL deadband, instead of re-sending
  `VAL`/`SEVR`/`STAT` on every simulated cycle under one shared mask.
- The `process_local` `LCNT` reentrancy guard raises `SCAN_ALARM`
  exactly once (C `dbAccess.c:544-557`) rather than re-posting the
  unchanged alarm fields on every reentrant attempt past the threshold.

### QSRV group monitor (BRIDGE-79)

- A group monitor now narrows each member's marked leaves to the wire
  fields the event's update type actually assigns: the self-triggered
  field uses the event's own DBE mask intersected with
  `DBE_VALUE|DBE_ALARM|DBE_PROPERTY` (pvxs `groupsource.cpp:331-337`),
  while other triggered targets keep the `Value|Alarm` refresh. An
  alarm-only post no longer re-sends the value leaf, and an
  archive-only post contributes no self leaves.

### Other fixes

- `optics`, `ca`, `asyn`, `server`, and the `mini-beamline` example
  received targeted parity / correctness fixes (see the `fix(...)` git
  log).
- `asyn-rs` now enables the `epics` feature by default.

## v0.19.2 — 2026-06-11

Patch release making the cross-platform CI matrix introduced in `v0.19.1`
fully green. `v0.19.1` fixed the Windows *build*; this release fixes the
Windows and macOS *test and runtime* failures the matrix then surfaced on
the previously-untested cells (Windows × {x86_64, arm64}, macOS arm64).
No API changes; behavior on the already-passing Linux x86_64 path is
unchanged.

- Windows `Instant` panics ("overflow when subtracting duration from
  instant"): `Instant` is QPC-since-boot on Windows, so `Instant -
  Duration` panics whenever machine uptime is shorter than the span.
  Fixed the `IfaceMap::new` `last_refresh` seed (a back-dated `Instant`
  that took down every NIC-enumerating test) and two production
  rolling-window prunes — the CA circuit-breaker failure window and the
  CA UDP rate-limiter GC — by reformulating as
  `saturating_duration_since` instead of subtracting from an `Instant`.
- Windows `SystemTime` precision: `Snapshot` timestamps now hold
  nsec-precise `WallTime` instead of `SystemTime`, which truncates
  sub-100 ns on Windows (FILETIME is 100 ns granularity).
- Loopback NIC enumeration order: Windows does not enumerate the loopback
  at socket index 0. The `recv_*_with_drop_count` tests that read
  `sockets.first()` now bind a single loopback socket so the socket the
  method reads is the one the test sends to on every platform.
- PVA `ORIGIN_TAG`: receive the loopback-multicast forward on the
  `lo_mcast` socket itself and drop the same-port co-bind that failed to
  bind on Windows.
- macOS timing-fragile test: the circuit-breaker half-open cooldown test
  now asserts the doubled-cooldown value directly instead of racing a
  `thread::sleep` lower bound against it, which flaked under macOS CI
  load.
- Detector examples: skip the idle-frame writeback when the asyn port is
  torn down on shutdown, removing a teardown-race error on `exit`.
- Portability hygiene: the `check_path` test uses the platform temp dir
  rather than a hard-coded `/tmp`; the Windows `iocsh` prompt no longer
  pads spaces after `epics>`; dropped an unused `spvirit-server` dev
  dependency that broke the windows-arm64 build; scrubbed inert
  `spvirit` references from comments and test names.

## v0.19.1 — 2026-06-11

Patch release fixing cross-platform builds. `v0.19.0` did not compile on
Windows: the wildcard UDP collector's original-destination recovery in
`epics-pva-rs` referenced the `libc` crate (a `cfg(unix)`-only
dependency) without `#[cfg(unix)]` gating, so a Windows build failed with
`error[E0433]: failed to resolve: use of unresolved module or unlinked
crate 'libc'`. No API changes; Unix behavior is identical to `v0.19.0`
and consumer code builds unchanged.

- Gate the Unix-only UDP orig-dest recovery path behind `#[cfg(unix)]`
  with non-Unix fallbacks, fixing the Windows build break.
- Recover each datagram's original destination on Windows via the
  Winsock `WSARecvMsg` extension function plus `IP_PKTINFO`/
  `IPV6_PKTINFO`, mirroring the Unix `recvmsg`/cmsg path (pvxs
  `os/WIN32/osdSockExt.cpp` parity).
- Select the v4 orig-dest socket option by IP-stack family
  (`linux`/`android` → `IP_PKTINFO`; the BSD/Apple family, including the
  `IP_PKTINFO`-less freebsd/openbsd/dragonfly → `IP_RECVDSTADDR`)
  instead of a `linux`/`not(linux)` split, fixing an `android` build
  break where `libc::IP_RECVDSTADDR` is undefined.
- `epics-bridge-rs`: gate a Unix-only `GatewayCommand` import behind
  `#[cfg(unix)]` to silence a Windows-only unused-import warning.
- CI: add a cross-platform build matrix (macOS / Linux / Windows ×
  arm64 / x86_64) that builds and tests the whole workspace per cell.

## v0.19.0 — 2026-06-11

A large C/C++ parity-hardening release: 761 commits since `v0.18.6`
(integration merges included), dominated by ~557 parity/bug fixes across
the native PVA protocol, the QSRV/bridge gateway and pvalink, CA, asyn,
and base/db, plus ~66 additive features.

Versioned as a minor (`0.19.0`) bump given the breadth of the change,
which includes source-facing trait-contract refinements (e.g. the
`ChannelSource` cluster) accumulated since `v0.18.6`. No commit in the
range carries an explicit breaking-change marker (`feat!`/`fix!`), but
external source/driver authors implementing the affected traits should
review those surfaces before upgrading; consumer code that only uses the
client/server/IOC APIs is expected to build unchanged.

### Native PVA protocol (epics-pva-rs)

- Typed multi-field PUT path (`PutLeaf`, no stringify); GET-with-request-
  value client API; `ChannelArray` (cmd 14) operation surface; `PUT_GET`
  `getGet`/`getPut` subcommands gated behind a `serve_put_get` capability.
- pvRequest: typed record options + a `RawPvRequest` escape hatch;
  effective `Config` with `expand()`; per-client UDP `SEARCH` config; a
  core wildcard UDP collector with orig-dest fanout; requester endpoint
  in `SEARCH` advertisements.
- CLI: `pvget`/`pvmonitor` `Value::format()` `-F tree|delta` and `-#`
  array-limit; `pvput` JSON-value args, NTEnum-by-label, and the legacy
  positional bare-token array form; `pvinfo -D` effective-config report;
  default NTTable rendering via Base `printTable` parity.
- Monitor overrun carried through coalesce into the wire bitset; a
  periodic client channel-cache cleaner.

### QSRV / bridge (epics-bridge-rs)

- pvalink folded into the `qsrv` feature (default-on PVA links); pvalink
  is NOT held by the iocInit external-link wait (pvxs parity — it opens
  in the background); pvxs async shared-channel owner for OUT links;
  alarm split into an ungated snapshot + a gated contribution; remote
  `timeStamp.userTag` adopted on `time=true` links.
- QSRV: group loading wired into the IOC startup lifecycle; `asTrapWrite`
  put-logging on every PUT via `WriteGrant`; EPICS `$` long-string field
  modifier; pvxs-compatible `pvxsl`/`pvxgl` diagnostics; `DbSubscription`s
  gated on PVA monitor `START`/`STOP` (onStart parity).
- CA/PVA gateways: ACF/access enforcement; no-cache forwarding modes;
  C-compatible report files + split downstream/upstream routing and
  cache-timeout knobs; procfs-derived load/CPU and event/post-rate stats;
  control PVs for caput-triggered commands.

### base / db / records (epics-base-rs)

- DBF link-class types + a canonical per-field link classifier; a unified
  `macLib` expander (full macro language in autosave); shadowed
  `DBR_GR`/`DBR_CTRL` metadata + `DBE_PROPERTY` on `ProcessVariable`.
- sseq driven as a per-step async PACT machine with concurrent `WAITn`
  put-callbacks; a PACT async-record re-entry primitive; live
  `DOL`/`LNK`/`checkLinks` diagnostics.
- calcout link-status menus (`INAV`..`OUTV`); simulation blocks for
  `mbbo`/`histogram`/`waveform`/`aai`/`aao`; `lsi`/`lso` `MPST`/`APST`
  menuPost fields.
- iocInit external-link wait gated to the CA facility's local-target
  links (C `dbCaRun`/`dbLink.c` parity) — non-local CA links and
  `pva://` links no longer block startup; `DBF_USHORT`/`DBF_ULONG` are
  first-class field types (signed-literal `strtoul` parity).

### CA (epics-ca-rs) / asyn (asyn-rs) / drivers

- CA: cumulative subscription event posted/processed counters; gateway
  `serverEventRate`/`serverPostRate` from downstream `CaServer` stats.
- asyn: default port trace mask is `ERROR` only (C `asynManager.c`
  parity); immediate trace-readback posts on a trace change; abort an
  in-flight async request on `AQR`; off-scan-thread `performIO` on
  `can_block` ports.
- ad-core: ProcessPlugin "no input array cached" warning gated behind the
  asyn `WARNING` trace mask; `DBF_USHORT`/`DBF_ULONG` → `NDAttr` mapping.
  motor: `DOL`/`RDBL`/`RLNK` link wiring. procserv: read-only log/viewer
  port (`-l`/`--logport`, `--restrict`) and `-F`/`--timefmt` banner.

See the `git log v0.18.6..v0.18.7` history for the full per-commit audit
trail.

## v0.18.6 — 2026-05-27

A parity-hardening release. Multiple rounds of C/C++ parity audit against
EPICS base, libca, and pvxs land fixes across all four core crates;
server/client TLS identity handling is tightened; the two `0.18.5`
breaking API changes are reverted to their `0.18.4` shapes; and a batch
of additive helpers is added for source/client authors.

**API compatibility.** The two breaking changes introduced in `0.18.5`
are reverted to their `0.18.4` shapes — `ArrayFilterConfig.incr` is a
`pub` field again (still held `>= 1`, now via the clamping `incr()` read
accessor), and `ChannelSource::subscribe_checked_opts` returns
`Receiver<PvField>` again. Code written against `0.18.4` builds
unchanged; code that adapted to the `0.18.5` shapes must revert those two
call sites. Everything else new in this release is additive.

### Parity fixes (EPICS base / libca / pvxs)

- **epics-base-rs** — post `DBE_PROPERTY` on metadata-field writes;
  post `VALUE|LOG` for non-VAL writes on Passive-scan records and strip
  `VALUE|LOG` (not only `VALUE`) on deadband bypass; honor CPP vs CP link
  scan-gates; populate control-info limits for `int64in`, `ao`/`longout`/
  `int64out`, and `waveform`/`aai`/`aao`/`compress`; defer CA
  `WRITE_NOTIFY` completion until the whole link chain settles; iocsh
  macro/redirect/`#-` fixes.
- **epics-ca-rs** — implement the `$` long-string channel suffix; reject
  `EVENT_ADD` masks of 0 or above `UCHAR_MAX` (after channel lookup);
  honor record precision and enum state-labels in `DBR_STRING`
  conversion; correct `send_ca_error` payload size for extended-header
  requests.
- **epics-pva-rs** — RPC `INIT` sends type + full value; `GET_FIELD`
  slow path cancels on teardown; `CONNECTION_VALIDATION` buffer size,
  server-derived roles, and anonymous/ca credentials; correct
  `QosFlags` `MONITOR_START`/`STOP`; NT type-ids and
  timeStamp/valueAlarm/display/alarm fields; beacon, SEARCH /
  SEARCH_RESPONSE, and discovery-pong handling; monitor flow-control.
- **epics-bridge-rs (gateway)** — ACF/access enforcement and read-only
  defaults; preserve upstream alarm/timestamp on forwarding; qsrv
  trigger resolution, deterministic group field order, and group-PUT
  validation; pvlist `DENY FROM` fail-closed; RPC gated as WRITE-class.

### TLS hardening

- Reject embedded-NUL and empty CN/SAN/issuer in cert identity mapping.
- Request a client certificate by default, matching pvxs
  `SSL_VERIFY_PEER`.
- Resolve server keychain + options PVAS-first, closing a fail-open gap.
- Populate the x509 authority from the trust store on a partial chain.

### New API surface (additive)

- **epics-pva-rs** — `SharedArray<T>` copy-on-write container; `util`
  module (Escaper / SigInt / Indented / Detailed / Timer / MPMCFIFO
  analogues); `MonitorControlOp` + `PostError` and the op-handle surface
  for source authors; discrete `errors::*` exception types alongside the
  `PvaError` enum; `config::apply_defs` env-override helper;
  `PvaClient::request()` pvRequest-builder entry point.
- **epics-ca-rs** — `ca_version()` version string; `CaChannel::v42_ok()`
  capability check; `MonitorHandle::channel()` / `subid()`; a `CaChannel`
  user-data slot (set/get/clear); runtime client user/host-name override
  (applies to new circuits only).

## v0.18.5 — 2026-05-22

The largest functional-review batch to date, plus a broad CA/PVA
wire-protocol parity sweep against EPICS base and pvxs. CA client
tooling reaches flag parity with the C `caget`/`caput`/`camonitor`/
`cainfo`; the native PVA server/client gain report counters, monitor
pause and `onStart` edge callbacks, and pipeline-window watermarks; the
CA/PVA gateway enforces ACF rules and PVA credentials; and the parity
sweep (BFR / PVXS-SR findings) closes monitor, RPC, and PUT lifecycle
and flow-control divergences.

**Breaking changes (vs `0.18.4`, within the `0.18.x` line):** two public
API shapes changed.

- `epics_base_rs::server::database::filters::arr::ArrayFilterConfig.incr`
  changed from a `pub` field to a private field. Construct via
  `ArrayFilterConfig::new()` / `Default` (both clamp `incr` to `>= 1`);
  read it through the `incr()` accessor. Struct-literal construction of
  this type no longer compiles. The field is private on purpose: the
  slice helpers divide and step by `incr`, so the `>= 1` invariant is
  held by construction rather than re-checked at every use.
- `epics_pva_rs::server_native::ChannelSource::subscribe_checked_opts`
  changed its return type from `Receiver<PvField>` to
  `Receiver<MonitorUpdate>` (BRIDGE-FR-12: the cooked monitor stream now
  carries the trigger `marked` bitset). External `ChannelSource`
  implementations that override this method must update the return type;
  a source that owns its record directly can wrap its `subscribe_checked`
  stream with the new `plain_monitor_updates` helper (`marked: None`).

### epics-ca-rs — client tooling flag parity and circuit accounting

- **CA-FR-1** — the CA server now evaluates CALC-gated ACF rules.
- **CA-FR-2** — channel/operation id allocation (cid/ioid/subid) is
  registry-owned and collision-checked.
- **CA-FR-3** — channels are priority-aware; virtual circuits are keyed
  per `(address, CA priority)`, and `ca_get_ioc_connection_count` now
  counts one circuit per `(addr, priority)` rather than per distinct
  address (matching libca `caServerID` / `cac::circuitCount`).
- **CA-FR-4** — `caget -d` requests the exact DBR type (not a class
  band); `caput -S` writes a NUL-terminated `DBR_CHAR` array, checks
  ENUM type first, and escape-decodes; `camonitor` honors `-m` event
  masks (default `VALUE|ALARM`, invalid letters warn and revert) and
  `-t` timestamp rendering modes (`r`/`i`/`I` baselined on the first
  server stamp); `cainfo -s` emits client-status diagnostics.
- **CA-FR-5** — reusable synchronous group with test/reset/stat.
- **CA-FR-8** — channel filters run on the READ and `SimplePv` paths.
- Monitor delivery hardening: `EVENTS_ON` resume is no longer lost in
  the flow-control gate, and pause coalescing folds the producer
  overflow slot rather than only the receive side.

### epics-pva-rs — server/client features and parity fixes

- **PVA-FR-1** — compound arrays preserve null elements.
- **PVA-FR-2** — per-connection (client) and per-channel (server) report
  counters with credentials and reset.
- **PVA-FR-4** — monitor watermarks fire from the pipeline window;
  composite sources now report the **owning** source's watermark levels
  (resolved through the async `has_pv` owner check) instead of the first
  source returning any levels.
- **PVA-FR-5** — `any` no longer advertises a degraded descriptor.
- **PVA-FR-6** — `pvput-rs` supports `field=value` multi-field
  assignment.
- **PVA-FR-7** — `PvaOperation::wait` is retriable after a timeout.
- **PVA-FR-8** — server monitor pause holds the latest update.
- **PVA-FR-9** — distinct `PvaError::Interrupted` vs `Timeout`.
- **PVA-FR-10** — the MONITOR overrun bitset is carried into the
  server-side squash.
- **PVA-FR-11** — server monitor `onStart(bool)` edge callback.
- **PVA-FR-12** — one-shot operations fail at the op timeout instead of
  hanging.
- Broad parity sweep (BFR-4..15, PVXS-SR-5/9/21): RPC EXEC malformed
  args are fatal rather than coerced to Null (BFR-4); GET/PUT/RPC
  last-request (`0x10`) lifecycle (BFR-5); GET_FIELD on an unknown SID
  replies an error instead of a fabricated Variant (BFR-6); PROCESS INIT
  pvRequest routes through the structured decode boundary (BFR-8); raw
  monitor re-encode errors on a missing/malformed overrun bitset and
  terminates on cross-endian re-encode failure (BFR-9/11/14); MONITOR
  finish routes through the read-loop owner so the op is removed
  (BFR-12); error replies echo the request data subcmd (BFR-13); in-
  flight data-phase EXEC is gated and re-EXEC ignored (BFR-15); `ackAny`
  is a true percentage of the queue (PVXS-SR-5); inbound message size
  defaults to unbounded with an opt-in cap (PVXS-SR-9); a panicking
  source handler becomes an exec error reply (PVXS-SR-21).
- Pipeline/flow-control: malformed MONITOR ACK resets the connection
  instead of fabricating credits; ACK refill saturates the window
  instead of wrapping; the initial monitor snapshot consumes a window
  credit; `ackAny` clamps watermarks at `ackAt-1`.
- Server `_filter` chain applies to the initial monitor snapshot (not
  just updates) and fails closed rather than fabricating a value leaf.
- Bulk-PUT throughput benchmark (in-process + external IOC) and an
  expanded pvxs-captured golden suite (NTTable / NTURI descriptors).

### epics-base-rs — access control, monitor accounting, arr filter

- **CA-FR-1 / PVA-FR-3** — the access gate evaluates CALC via the `INP*`
  resolver and matches role UAG members against the account string or
  the roles slice.
- **CA-FR-6** — `SimplePv` emission honors the `DBE_*` subscriber mask.
- **CA-FR-7** — table-driven `dbr_buffer_size` matching C `dbr_size_n`.
- **BFR-10** — record-field monitor overflow routes through the shared
  drop-accounting owner.
- arr filter: `incr >= 1` holds by construction (private field +
  clamping constructors, not a runtime clamp), and the filter no-ops at
  length `<= 1` so count and value agree on scalars.

### epics-bridge-rs — CA/PVA gateway and QSRV

- **BRIDGE-FR-1/2/10** — CA gateway ACF read/monitor enforcement, ALIAS
  serving, host-scoped `pvlist` `DENY FROM`.
- **BRIDGE-FR-3/4** — CA link alarm severity modifiers and metadata
  getters.
- **BRIDGE-FR-5/8/11/14** — PVA gateway credential preflight,
  credentialed CREATE_CHANNEL, reachable pause watermark, credential-
  scoped flush/drop.
- **BRIDGE-FR-6/7/12** — QSRV group monitors subscribe the configured
  member field, meta value-event mask, explicit trigger target sets.
- **BRIDGE-FR-9/13/15/16** — pvalink legacy suffix parsing, disconnected
  stale-read alarm, `link_names` for `iocInit` wait, `proc` `ProcMode`
  enum.

## v0.18.4 — 2026-05-20

`epics-ca-rs` monitor-delivery hardening + a new client-side
per-subscription pause API, and an `epics-pva-rs` NTNDArray wire-shape
fix.

### epics-ca-rs — monitor delivery no longer drops the latest value

- The CA client used to deliver monitor updates with
  `try_send` into a bounded queue (default 256) and **silently drop**
  on overflow. Under load (full test suite + Python GIL contention)
  that lost terminal transitions — e.g. a motor `DMOV` 1→0 — so an
  ophyd `MoveStatus` could hang forever. Delivery now coalesces: on a
  full queue the latest value is kept in a per-subscription slot
  rather than dropped, mirroring the C IOC `dbEvent.c`
  replace-last-pending behaviour. The terminal value always reaches
  the consumer.
- The per-subscription delivery buffer is a small, single-invariant
  state machine (`error` / `ready` / `gated` cells with
  `gated.is_some() ⇒ paused`). Errors (`ECA_DISCONN`, monitor status)
  bypass pause and are delivered ahead of values; coalescing is
  uniform (latest-wins) including across a pause boundary.
- Flow control is single-owner: only a bounded-channel send/receive
  moves the per-circuit outstanding count, so the coalesce slot can't
  trip the wire-level `EVENTS_OFF` and freeze sibling subscriptions on
  the same circuit. `EPICS_CA_MONITOR_QUEUE` is clamped to at least the
  `EVENTS_OFF` threshold so a lone full channel can still engage flow
  control.
- **New API** (`MonitorHandle`, additive): `pause()` / `resume()` /
  `is_paused()` — client-side per-subscription pause with no CA wire
  message (wire compatibility preserved). While paused, value updates
  coalesce into the slot and are withheld; pre-pause channel backlog
  and connection/terminal errors are still delivered.

### epics-pva-rs — NTNDArray descriptor matches pvxs

- `nt::nd_array::nt_nd_array_desc` drifted from pvxs
  `nt.cpp::NTNDArray::build`: the `value` union enumerated variants in
  ScalarType-enum order and the `dimension` element had an empty
  `struct_id`. Aligned to pvxs (signed-then-unsigned variant order;
  `dimension_t` struct id) and verified with pvxs's own `pvxget`
  against the Rust server. `NdArrayBuffer::selector` indices updated to
  match.
- Expanded the pvxs wire-golden suite to 90 byte-exact cases now
  derived from bytes captured at run time from `libpvxs`
  (`tools/pvxs-golden-capture/`) rather than hand-read from source —
  scalars, scalar arrays, Size boundaries, sub-structure/union/variant,
  Status, headers, BitMask, NT descriptors, floating-point specials,
  UTF-8.

## v0.18.3 — 2026-05-20

Build fix for non-macOS targets. `v0.18.2` is yanked from crates.io.

- `epics-ca-rs` — `addr_list.rs` read the point-to-point destination
  address through `libc::ifaddrs::ifa_dstaddr`, a field that exists
  only on macOS/BSD. The Linux `ifaddrs` struct carries the
  `dstaddr`/`broadaddr` union as a single `ifa_ifu` field, so
  `v0.18.2` failed to compile on Linux. The lookup is now
  `cfg`-selected (`ifa_ifu` on Linux, `ifa_dstaddr` elsewhere); the
  enclosing `ifa_dstaddr_for_ipv4` walk is already `#[cfg(unix)]`, so
  Windows is unaffected. Verified with `cargo check` for
  `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-gnu`.

## v0.18.2 — 2026-05-20

Follow-up to `v0.18.1` (which was tagged but not published to
crates.io). Carries everything in the `v0.18.1` section below plus:

- `epics-pva-rs` — the pvRequest builder no longer flattens dotted
  field paths. `field(a.b.c)` now nests `a { b { c {} } }` in the
  request structure instead of emitting a single literal `a.b.c`
  member, so `request2mask` resolves the intended leaf.

## v0.18.1 — 2026-05-20

Regression-hardening point release on top of `v0.18.0`. Rolls up the
`epics-pva-rs` / `epics-bridge-rs` deferred punchlists, the parallel
gateway/QSRV/pvalink track work, and a full merge-regression review of
the integrated branch.

### Merge-regression review

`docs/merge-regression-review-2026-05-19.md` audited the integrated
branch for defects introduced while combining the punchlist tracks and
catalogued 37 items — 25 branch-regression candidates (`MR-R1`..`MR-R25`)
and 12 pre-existing defects observed during the review (`EX-R1`..`EX-R12`),
plus two post-fix items (`PF-R1`, `PF-R2`). **38 fixed at root cause; one
(`MR-R18`) verified not-reproduced.** Highlights:

- **CA server** — `WRITE_NOTIFY` busy gate now runs before side effects;
  UDP batch responder no longer reparses a stale datagram under a new
  peer; wildcard multicast responders no longer duplicate ordinary
  search replies; `TRAPWRITE` is reported from the matched ACF rule;
  denied autosize monitors emit a non-zero DBR body; initial/restore
  monitor frames pad an over-requested count.
- **CA client** — a legitimate server migration no longer surfaces as a
  false multiply-defined diagnostic; a write-side timeout closes the
  circuit instead of keeping a desynchronized stream;
  `EPICS_CA_MAX_SEARCH_PERIOD` matches the C default / lower bound.
- **PVA server** — `SharedPV` monitor queues are no longer closed by a
  cloned `MonitorOutbox` drop; role claims reach the QSRV ACF; GET /
  PUT_GET honor INIT pvRequest options; `PUT_GET` / `PROCESS` data
  phases verify the IOID's operation kind; pipeline credit is consumed
  only by emitted frames; an unadvertised auth selection reverts the
  connection credential to anonymous; server-side transformation
  filters' rewritten value reaches the wire.
- **PVA client** — `share_udp(true)` clients keep configured TCP name
  servers; `pvget_many` warm path no longer skips its own responses or
  leaks reusable IOIDs; `CMD_DESTROY_CHANNEL` cleans the `ioid_to_cmd`
  map; the pvRequest string parser can carry a `_filter` JSON value.
- **`DBF_UINT64` round-trip** — native PVA and QSRV `ulong` / `ulong[]`
  PUTs preserve the full unsigned 64-bit range (no `i32`/`f64`
  narrowing); the `arr` channel filter slices `UInt64Array` waveforms;
  pvalink OUT routes `UInt64Array` / `Int64Array` through the typed
  path; pvalink INP no longer truncates remote 64-bit values.
- **Record lock** — every foreign full-processing and record-write
  entry is routed through the advisory `dbScanLock`/`DBManyLock`-
  equivalent gate, closing the atomic-group/direct-write race the
  punchlist opened; transaction owners use `_already_locked` variants.
- **PVA gateway** — per-credential upstream clients inherit the base
  client's upstream address/transport; identity pools are keyed by
  account, method, host, and authority.

### Other fixes

- `motor-rs` — `DHLM`/`DLLM` EGU limits are preserved when a template
  loads `field(DHLM)` before `field(MRES)`.
- `ad-core-rs` / `ad-plugins-rs` — `EpicsValue::UInt64`/`UInt64Array`
  handled in NDAttribute conversion; `NdAttribute` / `NtNdArray`
  construction realigned with the current struct shapes.

Verification on the release commit: `cargo clippy --workspace
--all-targets --all-features -- -D warnings` clean,
`cargo nextest run --workspace` 4707 passed, `cargo test --doc
--workspace` 0 failed.

## v0.18.0 — 2026-05-18

First `main` release rolling up the `review/codebase-hardening-202605`
branch. `main` was at `v0.17.0`; this release encompasses everything
recorded under `v0.17.1` (C-parity hardening, 126 commits across
eleven review rounds) and `v0.17.2` (areaDetector RBV constructor-time
flag-consumption fix), plus the new entries below. See those sections
for the full audit trail.

### modbus-rs — published on crates.io as `epics-modbus-rs`

The bare `modbus-rs` name on crates.io is owned by an unrelated project,
so the workspace crate is now published as `epics-modbus-rs`. The Rust
library name is kept as `modbus_rs` via `[lib] name = "modbus_rs"`, so
consumers continue to write `use modbus_rs::...`; only the `crates.io`
package name changes. The on-disk path (`crates/modbus-rs/`) and the
workspace dep alias (`modbus-rs = { workspace = true }`) are unchanged.

### modbus-rs — absolute-mode array I/O length safety

- **fix(modbus-rs)** `read_int32_array` absolute-mode requests now use
  the record array length, not the driver-wide `modbusLength`, so a
  short waveform record never asks the device for more registers than
  it can buffer (`dac0dd6`).
- **fix(modbus-rs)** Absolute-mode array writes clamp the request
  length to `modbusLength` to prevent overrun on the device side
  (`28160ae`).
- **fix(modbus-rs)** Relative-mode array writes apply the same clamp
  for the `modbusLength`-bounded register window
  (`4b5a38c`).

Workspace: `cargo fmt --all` clean, `cargo clippy --workspace
--all-targets -- -D warnings` clean.

## v0.17.2 — 2026-05-15

Patch release.

### ad-core-rs — areaDetector static RBV records populate at IOC startup

- **fix(ad-core-rs)** Drop the premature `call_param_callbacks(0)` at
  the end of `ADDriverBase::new`. Constructor time has no record
  subscribers (`dbLoadRecords` runs strictly after), so the call
  silently consumed the change flags of every just-set parameter
  (`MAX_SIZE_X/Y`, `SIZE_X/Y`, `BIN_X/Y`, `IMAGE_MODE`,
  `NUM_IMAGES`, `NUM_EXPOSURES`, `ACQUIRE_TIME`, `ACQUIRE_PERIOD`,
  `STATUS`, `DATA_TYPE`, `COLOR_MODE`, ...) and fired
  `InterruptManager.notify` into an empty mailbox list before
  clearing the flags. Records registering during `dbLoadRecords`
  got fresh mailboxes with no buffered value and — for static
  parameters never re-set during operation — waited forever.
  Symptom on synthetic detectors (mini-beamline `MovingDotDetector`):
  `MaxSizeX_RBV` / `MaxSizeY_RBV` / `SizeX_RBV` / `SizeY_RBV` /
  `BinX_RBV` / `BinY_RBV` / `NumImages_RBV` at `VAL=0`,
  `Timestamp=<undefined>` indefinitely. Real cameras masked the
  gap by refreshing these from device polling on first acquire;
  synthetic detectors exposed it. Fix removes the call; pending
  flags now accumulate through construction and flush on the
  first post-iocInit `call_param_callbacks` (acquire start,
  write-driven update, dirty-flag handler). (`870acc3`)

  **Anchor**: `call_param_callbacks` inside an `fn new` body.
  Workspace audit shows ad_driver.rs:92 was the only constructor-time
  site; all other call sites live in operational methods
  (`prepare_array`, `write_int32`, `write_float64`, `write_*`) and
  are correct.

  Verified on a live mini-beamline IOC: after restart, `MaxSizeX_RBV=640`,
  `MaxSizeY_RBV=480`, `SizeX_RBV=640`, `SizeY_RBV=480`, `BinX_RBV=1`,
  `BinY_RBV=1`, `NumImages_RBV=100`, all with concrete timestamps. No
  regression on previously-working RBVs (`Manufacturer_RBV`,
  `Model_RBV`, `DataType_RBV`).

## v0.17.1 — 2026-05-15

C-parity hardening release: 126 commits across eleven review rounds.
Rounds 1-5 ran four parallel sub-agent teams (A: `epics-base-rs`,
B: `asyn-rs`, C: `epics-pva-rs` ↔ pvxs C++, D: `epics-ca-rs`) auditing
each crate against its C reference in worktree isolation, with each
finding gated on the global *Fixes from reported defects* +
*Invariant-driven fixes* rules (anchor → workspace `rg` → classify
every hit → bundle-fix in one commit). Rounds 6-11 focused
exclusively on `epics-ca-rs` server/client wire correctness. Each
fix carries its own audit-trail commit body; this entry is the
summary. See `docs/c-parity-review-2026-05-15.md` (round 1) +
`docs/c-parity-review-2026-05-15-round2.md` (round 2).

Workspace: `cargo nextest --workspace` 3633/3633 PASS (32 skipped),
`cargo test --doc --workspace` clean, `cargo clippy --workspace
--all-targets -- -D warnings` clean.

No wire-protocol breaking changes. Several rsrv error-emission paths
now match libca byte-for-byte where they previously diverged
silently (see CA server section below); these are bug fixes from
the libca-client perspective, not protocol breaks.

### epics-base-rs — record / processing / iocsh parity

- **fix(record)** `dbProcess` entry-level PACT guard + AsyncPending
  sets pact — closes mid-cycle re-entry hole on FLNK/scan/CA-put
  callers. C `dbAccess.c:537-559` parity (silent bail until
  MAX_LOCK=10). Owner-driven continuations use new
  `process_record_continuation` to bypass the guard
  (`16e0ff6`, `27e0bb0`).
- **fix(record)** PUTF lifecycle — keep through process cycle, clear
  on FLNK end + propagate through DB OUT/FLNK
  (`925b46f`, `7168911`).
- **fix(record)** ReprocessAfter continuation invariant — only the
  async cycle owner bypasses PACT guard; foreign callers always
  routed through `process_record_with_links` (`27e0bb0`).
- **fix(record)** `check_deadband` fires on NaN/infinity transition
  (recGbl.c parity); `rec_gbl_reset_alarms` INVALID_ALARM clamp +
  ACKS auto-raise; AMSG event posting on amsg-only alarm-message
  change; TSE=-1 always overwrites via BestTime; SDIS disable bail
  clears rpro/putf; OMSL=closed_loop honored on `dfanout`; UDF
  cleared synchronously on primary-field CA put
  (`d9aa8ea`, `73880c7`, `334f8ac`, `4d2a007`, `c3f0fdc`,
  `76645cc`, `dd6b9f5`).
- **fix(record)** `ai`/`ao` SPC_LINCONV — LINR/EGUF/EGUL puts rebase
  eoff + reprime SMOO (`544ae66`).
- **feat(record)** TPRO diagnostic on PACT entry guard (`31f57b4`).
- **feat(epics-base-rs)** calc record analog alarm limits + AFTC
  filter (HIHI/HHSV/HIGH/.../LLSV + C `calcRecord.c:339-381`
  hysteresis) (`6dc7293`).
- **fix(iocsh)** `substitute_env_vars` accepts `${NAME}` brace form;
  `substitute_macros` backslash escape blocks `\$` expansion;
  fd-numbered redirect parser (`N>` / `N>>`)
  (`63b4551`, `531ec4f`, `54fc7ac`).

### asyn-rs — paramList / trace / interrupt callback parity

- **fix(asyn-rs)** trace setters fire `asynExceptionTrace*` —
  match C asyn manager (`e3d481d`).
- **fix(asyn-rs)** connect / disconnect edge-only (no duplicate
  announces) (`e7c36fc`).
- **refactor(asyn-rs)** `set_connected` owner-API closes
  Connect-edge invariant (`3766c2c`).
- **fix(asyn-rs)** `OctetWriteRead` flushes input first — match C
  asyn (`6a2b7ca`).
- **fix(asyn-rs)** strict `get_*_strict` variants + setter
  defined-flip on first write — C parity (`ad575cf`).
- **feat(asyn-rs)** `TraceManager::output_device_*` + `asyn_trace_device!`
  macros — match C `tracePrint` device → port → global hierarchy
  (`242a4ce`).
- **fix(asyn-rs)** `InterruptValue` carries `aux_status` +
  `alarmStatus/Severity` — C `asynPortDriver.cpp:631-642` parity
  for callback plumbing (`0b9e8d5`).
- **feat(asyn-rs)** paramList strict `create_param` +
  UInt32Digital rising/falling masks + interrupt config helpers
  (`657d7c4`, `7c31e08`).
- **feat(asyn-rs)** asynOctet `eomReason` plumbing through
  `OctetRead` path (`ae50b6c`).
- **feat(asyn-rs)** asynRecord ENBL/AUCT propagate to port +
  handle exposes `multi_device`; OEOS/IEOS routes through driver
  `setInputEos` hook + DBIT key parity; iocsh trace setters honor
  per-device `addr` (`137672d`, `2e2b5a9`, `7883367`).
- **feat(asyn-rs)** `PortDriver` gains `io_read_octet_eom` + UInt32
  interrupt config (`7c31e08`).
- **feat(asyn-rs)** iocsh module registers six `asynShellCommand`
  entries (`40efa0b`).

### epics-pva-rs — pvxs C++ parity

- **fix(pva)** SEARCH `MustReply` flag honored + emit `found=0` on
  empty reply (pvxs `server.cpp:730-744`) (`3bea633`).
- **fix(pva)** `DESTROY_CHANNEL` on unknown SID silently drops
  (pvxs `serverchan.cpp:382-386`) (`20d0d60`).
- **fix(pva)** echo request `subcmd` in PUT/GET/RPC data response
  (pvxs `serverget.cpp:83`) — fixes PUT_GET readback wire desync
  for pvxs C++ clients (`5a3245a`).
- **fix(pva)** reject `GET_FIELD` when IOID collides with active op
  (pvxs `serverintrospect.cpp:159`) (`80a447b`).
- **fix(pva)** reject zero version byte in frame header (pvxs
  `pvaproto.h:687` `from_wire`) (`a6e63c6`).
- **fix(pva)** role-aware direction-bit check on frame parse (pvxs
  `conn.cpp:160`) (`749de8d`).
- **fix(pva)** consume pipeline nack on MONITOR INIT (pvxs
  `servermon.cpp:493`) (`fd099c1`).
- **fix(pva)** reject unadvertised auth method on ConnValidation
  (pvxs `serverconn.cpp:238`) (`e617f36`).
- **fix(pva)** `CANCEL_REQUEST` preserves subscriber task (pvxs
  `serverconn.cpp:262`) (`0a081bc`).
- **fix(pva)** `CREATE_CHANNEL count > 1` multi-name handling
  (pvxs `serverchan.cpp:269`) (`c0105ca`).
- **fix(pva)** cap `FieldDesc` nesting depth at 20 (pvxs
  `dataencode.cpp:71`) (`7e2e63d`).

### epics-ca-rs — wire / server / client / repeater C parity

CA server `send_ca_error` field-swap fix (★★) — `m_cid` carried ECA
status and `m_available=0`, opposite of C `vsend_err`
(`rsrv/camessage.c:139-224`) and libca decoder (`cac.cpp:1118`).
Every server-emitted `CA_PROTO_ERROR` looked like `ECA_NORMAL` to C
clients, masking real failures (`21240ad`).

- **fix(ca-server)** SEARCH reply `m_cid = ~0U` sentinel + TCP
  `m_postsize = 0` — restores C `camessage.c:2193-2287` semantics;
  multi-NIC routing preserved via per-interface UDP binding +
  client-side "use UDP src addr" decode (`6ea50bd`, doc-only
  follow-up `148c4b7`).
- **fix(ca-server)** EVENT_CANCEL with unknown sub-id replies
  `ECA_BADMONID` + channel-mismatch detection
  (`90c56e8`, `0cbee2d`, `4f532cf`).
- **fix(ca-server)** TCP ECHO echoes back request header + payload
  (`b7e7722`).
- **fix(ca-server)** CREATE_CHAN cap reached emits
  `ERROR/ECA_ALLOCMEM` + per-client cap disconnects + V<9 nElem
  cap at `0xfffe` (`6089b84`, `0f7c949`, `b36d4f4`).
- **fix(ca-server)** CLEAR_CHANNEL aborts pending WRITE_NOTIFY
  tasks for sid + on unknown SID disconnects silently
  (`06ec795`, `24ea874`).
- **fix(ca-server)** drop TCP after `CA_PROTO_ERROR` on unknown
  command / WRITE bad type / READ bad type
  (`5ab609f`, `fdf6ead`, `5bf852e`).
- **fix(ca-server)** enforce `CA_MINIMUM_SUPPORTED_VERSION` on TCP
  VERSION + UDP SEARCH; TCP SEARCH from minor<4 client
  disconnects; non-VERSION command from pre-V4.4 →
  `ECA_DEFUNCT`; SEARCH postsize≤1 reject
  (`dbb4b28`, `7c6af61`, `88d1911`, `773523c`).
- **fix(ca-server)** EVENT_ADD silent-drops on bad type / unknown
  SID; READ + WRITE on unknown SID silent-drop (C `logBadId`
  parity); UDP non-VERSION/non-SEARCH cmd stops parsing
  (`9fdbc37`, `e5d2922`, `a4e5435`).
- **fix(ca-server)** HOST_NAME/CLIENT_NAME 511-byte cap +
  post-claim freeze + null-terminator check
  (`6b4d512`, `f5ec57d`, `5636637`).
- **fix(ca-server)** `ECA_TOLARGE` wire reply for oversized
  payload; bound `CA_PROTO_ERROR` diagnostic at 480 bytes
  (`fb94fb2`, `e0de0fa`).
- **fix(ca-server)** READ_SYNC echoes request header; CA_PROTO_READ
  response carries client cid; NOT_FOUND parity — drop UDP
  DOREPLY, echo request fields on TCP; VERSION reply zero-fill +
  beacon `m_available=0`; WRITE access-denial → `ECA_NOWTACCESS`
  (`c5d9f41`, `f05952b`, `d3312ab`, `7c49850`, `3542f4e`).
- **fix(ca-server,multi-NIC)** bind every `EPICS_CAS_INTF_ADDR_LIST`
  interface + secondary broadcast UDP socket when
  `INTF_ADDR_LIST` is specific IP + `AUTO_BEACON_ADDR_LIST` +
  `BEACON_PERIOD` C parity + drop deprecated `EPICS_CA_ADDR_LIST`
  beacon fallback + honour deprecated `EPICS_CA_BEACON_PERIOD`
  fallback
  (`e21e4c0`, `9c62f61`, `c450499`, `cd5a3db`, `e58b405`).
- **fix(ca-wire)** misaligned `m_postsize` reject — C `& 0x7`
  rule (`f690729`).
- **fix(ca-wire)** extended-header threshold — `>= 0xFFFF`,
  not `> 0xFFFF` (`7fcc26e`).
- **fix(ca-client)** `CA_PROTO_ERROR` status reads `m_available`,
  not `m_cid` (mirror of server-side `21240ad`); MonitorData
  delivery gated on `hdr.cid == ECA_NORMAL`; CREATE_CHAN inherits
  stashed ACCESS_RIGHTS; HOST_NAME / CLIENT_NAME handshake order;
  beacon `m_count=0` fallback honors `EPICS_CA_SERVER_PORT`;
  AUTO_ADDR_LIST loopback fallback when bcast enum empty
  (`d2472cc`, `8616655`, `94e877c`, `50282a3`, `51934ac`,
  `6091794`).
- **fix(ca-client,cap-tokens)** signed-beacon verifier lookup must
  use UDP source IP; lookup uses `hdr.available` (repeater rewrite
  target), not `meta.src`; `cap-tokens` build error (_src typo +
  missing sha2 dep) (`e162924`, `fcd8eca`, `7d6af83`).
- **fix(ca-client)** demote reconnect-restored log from `eprintln`
  to `tracing::debug` (`4a0563a`).
- **fix(ca-repeater)** fanout remainder after stripped REGISTER +
  tighten `m_available` rewrite; reject REPEATER_REGISTER from
  non-loopback peers; honour `EPICS_CA_REPEATER_PORT` for daemon
  bind + REGISTER targets (`9facef5`, `e7bdb4a`, `cf8ae27`).

### Documentation

- **docs(parity)** `docs/c-parity-review-2026-05-15.md` — round 1
  multi-team audit log (`5f61621`).
- **docs(parity)** `docs/c-parity-review-2026-05-15-round2.md` —
  round 2 audit log + tool-level retrospective on agent worktree
  base divergence (`211cf3a`, `148c4b7` correction).
- **docs(ca-proto)** clarify `EPICS_CA_MAX_ARRAY_BYTES` default
  divergence from C (`e73b1c7`).
- **docs(recgbl)** record `check_deadband_ext` NaN-as-sentinel
  design rationale (`82305e5`).

### Internal

- **build** drop accidentally committed sim-detector autosave tmp
  (`db926af`).
- **style** workspace `cargo fmt --all` post round-2 merges
  (`96ce0a7`); `is_multiple_of` for clippy::manual_is_multiple_of
  (`50f5e4c`).

## v0.17.0 — 2026-05-14

Upstream-features release: 192 commits closing out asyn-rs C-source
audit, PVA IPv6 stages 1-6, server-side channel filters, record-layer
processing parity, and a commit-by-commit C-source review pass that
produced 13 targeted fixes (`docs/review-rounds-2026-05-14.md`).

### Wire-protocol breaking changes

Two enum value tables on the CA/PVA wire have been renumbered to match
the C reference. Mixed Rust-vs-C client/server deployments must update
together; pure-Rust deployments are unaffected internally but observers
attached via `caget` / `pvget` will see the C-correct values.

- **fix(recgbl)** `alarm_status` enum renumbered to match
  `menuAlarmStat.dbd` wire order (e.g. `LINK_ALARM` was 13, now 14).
  Also adds the missing `BAD_SUB`, `READ_ACCESS`, `WRITE_ACCESS`
  values. Commit `da3230c`.
- **fix(compress)** `menuCompressALG` enum renumbered to match
  `menuCompressALG.dbd` wire order: Circular Buffer is alg=4 (was 3),
  Average is alg=3, Median (alg=5) added. Commit `e0cb6c8`.

### asyn-rs — C source audit closure

Full audit of `~/codes/epics-modules/asyn` against `crates/asyn-rs`.
Every item in `docs/asyn-rs-c-audit.md` is now verified, ported, or
explicitly skipped with a one-line rationale.

- **feat** `drvAsynIPServerPort` — full TCP child-port model with
  `parent:N` subports and TCP/UDP/UDP* protocol suffixes
  (`598d81b`, `1e2716a`, `cee7d7d`, `9ff5659`).
- **feat** UDP server mode (SOCK_DGRAM) — port of
  `drvAsynIPServerPort.c` UDP path (`cee7d7d`).
- **feat** RS485 — full `struct serial_rs485` with 5 `setOption` keys
  and `getOption` (`38e7743`).
- **feat** Prologix GPIB driver — port of `drvPrologixGPIB.c`
  (`c2a3f6f`).
- **feat** USBTMC + VXI-11 driver scaffolds matching C iocsh
  (`448724e`, `2b67092`).
- **feat** `asyn:FIFO` ring buffer matching C `devAsynInt32
  createRingBuffer` (`04ef574`).
- **feat** `ai` LINEAR ESLO/EOFF from `getBounds` — matches C
  `convertAi` (`614e7eb`).
- **feat** `ASYN_DESTRUCTIBLE` shutdown lifecycle (opt-in) (`a20aede`).
- **feat** `AsynParamSet` flat group helper — matches C++
  `asynParamSet` (`5817fad`).
- **fix** `asynMask` propagates `computeShift(mask)` into record
  `SHFT`/`MASK` (`e96561b`).
- **fix** `asynOctet` buffer size — `SIZV`-driven for `lsi`/`lso`/
  `printf` (`55dc8fd`).
- **fix** `asyn:INITIAL_READBACK` auto-parse + correct READBACK docs;
  removed invented init-readback on input records
  (`f2370af`, `4b6e2f7`).
- **fix** `hostInfo setOption` — full protocol reparse + socket close
  delay (`40fa1d0`).
- **fix** trace mask token parsing — match C asyn, removed invented
  `STATE` bit (`9691605`).
- **fix** `TCP&`/`UDP&`/`UDP*` protocol suffix semantics — match C
  asyn (`9ff5659`).
- **fix** FTDI 9 positional `iocshArg` — match C
  `drvAsynFTDIPortConfigure` (`5d2253c`).
- **fix** `RingAverager` replaced with `SumAverager` — match C
  `devAsynInt32` average (`7befd0e`).

### PVA — IPv6 + filters + autoExec parity

- **feat** PVA IPv6 Stages 1-6 — server/client TCP bind, UDP SEARCH
  emit + recv, multicast beacon emit + recv (PR #205)
  (`835e2c5`, `4cc40a8`, `abf9344`, `312d578`, `e25d281`, `0cd85a5`,
  `a3e7c74`).
- **feat** server-side channel filter wire-through (PR #205
  follow-up) (`69c7999`).
- **feat** `decodeError` carries source `file:line` for diagnostics
  (`d525ace`).
- **fix(pva)** IPv6 UDP responder must set `IPV6_V6ONLY=1` to avoid
  v4 overlap (`9b27f5b`).
- **fix(pva)** server PUT executes immediately; `autoExec` is a
  client-only knob. Removed invented `put_pending` queue/commit
  (`65db161`).
- **fix(pva)** emit first beacon immediately on server start
  (matches pvxs `cc5071cd22c4`) (`763681d`).
- **fix(pva)** skip name-server reconnect during `PvaClient`
  shutdown (matches pvxs `4d12da87205e`) (`809baa9`).
- **feat(net)** UDP RX overflow detection via Linux `SO_RXQ_OVFL`
  (pvxs `a064677e3625`) (`2a9b52a`, `6738aa3`).

### epics-base-rs — record / processing / filter parity

13 fixes from the commit-by-commit review (see
`docs/review-rounds-2026-05-14.md`):

- **feat(filters)** `ts` filter — full num/epoch/str modes
  (`Generate`/`Double`/`Seconds`/`Nanoseconds`/`Array`/`StringEpics`,
  Epics vs Unix epoch). C `ts.c` parity (`9a1c324`).
- **feat(filters)** `sync` filter — all 6 modes via `dbState` model
  (`710fe62`).
- **feat(compress)** C-parity closure — `NUSE` clamp via
  `linearise_val`, ILIL/IHIL fields, INX cycle counter,
  `push_array_average` for alg=3, `put_one` LIFO via pre-decrement
  (`d77f7d5`).
- **feat(longout)** OOCH-driven OUT-change force-write — first PR
  `#6c573b4` analog, routed via `Record::special` hook
  (`f823a0f`, `82049f4`).
- **fix(filters)** `dbnd` C-parity — strict `>`, C-style NaN/Inf
  `c_delta` helper, alarm passes update `last_sent`; JSON `r` is C
  percent → fraction (`83ee47a`, `6489ef6`).
- **fix(filters)** `arr` is a transformation filter — alarm bypass
  removed; asymmetric `resolve_start`/`resolve_end` clamps
  (`6a0cc82`).
- **fix(filters)** `decimate` / `sync` — only `DBE_PROPERTY`
  bypasses (not `DBE_ALARM`) (`6a0cc82`, `e26af3e`).
- **fix(processing)** MS-class link propagation matches C
  `recGblInheritSevrMsg` — MS/MSI use LINK_ALARM no msg, MSS keeps
  source stat+msg (`09c4109`).
- **fix(processing)** `SIMM=RAW` float→int floor narrowing — C
  parity (`1cc2629`).
- **fix(processing)** `complete_async_record` subscriber-snapshot
  gated on actual change (`2054ab7`).
- **fix(records)** `subArray` `INDX`/`NELM` clamp to `MALM` — C
  parity (`29199b3`).
- **fix(records)** `longout` `OOPT` first-cycle force only for
  `On_Change` — C parity (`bd9d1c7`).
- **fix(records)** `is_metadata_field` — add `HHSV`/`HSV`/`LSV`/
  `LLSV` + `ZSV`/`OSV`/`COSV` (`cc1c4aa`).
- **fix(iocsh)** `dbLoadRecords` merges same-type duplicates —
  match C `dbLexRoutines::dbloadRecord` (`48e225b`).
- **fix(iocsh)** prompt color is bright green (C `ANSI_GREEN`),
  drop `\x01\x02` brackets that break cursor on some terminals
  (`1be96ec`, `91daa1b`).
- **fix(types)** drop spurious octal parse for Float/Double — C
  parity (`87c645d`).

### CA — server stats + repeater + access control

- **feat(ca-server)** wire `dbServerStats` subscription +
  bytes_in/bytes_out counters (PR #592)
  (`14d0b03`, `f68f17c`).
- **feat(ca-server,acf)** wire mTLS identity into ACF
  `METHOD`/`AUTHORITY` (PR #641) (`23360e6`).
- **feat(ca-server)** split TCP/UDP server ports via
  `EPICS_CAS_SERVER_PORT` (`9d8a34b`).
- **feat(ca-repeater)** `-d`/`-dd` debug switch (closes
  epics-base PR #831) (`d0d59f7`).
- **feat(ca-client)** honor `EPICS_RS_CLIENT_IGNORE` quarantine
  list (renamed from `EPICS_IOC_IGNORE_SERVERS`)
  (`8615bb4`, `f3738ce`).
- **feat(ca-client)** shorten echo probe on suspend wake (Issue
  #190) (`a409311`).
- **fix(ca,pva)** guard CLI timeout against NaN/Inf/non-positive
  (pvxs `1655d68e` analog) (`e77358b`).
- **fix(ca-server)** `EPICS_CAS_SERVER_PORT` — UDP and TCP use the
  same port for C parity (`d20f8b7`).

### Miscellaneous

- **feat** iocsh ANSI color + HAG DNS TTL refresh (closes
  `c0da3dd` + PR #862/#863) (`60188d3`).
- **feat** bulk 9-A archaeology audit closure — 12 high + 13
  medium upstream items (`22fd25d`).
- **feat** `lnkCalc` + `autoExec=false` PUT + filter on read +
  FTDI scaffold (`d545303`).
- **feat** PI mutex + serial break + averaging device +
  camonitor type-change (`e1387d8`).

### Reverts (all on this branch; not on main)

Six reverts in the asyn-rs work — speculative scaffolds that drifted
from the C source were rolled back after audit (`9bae27e`, `2f32958`,
`f589453`, `1eeb5ae`, `b29d7b5`, `c88262a`). Several were
re-implemented properly afterward.

### Internal

- **docs** `docs/review-rounds-2026-05-14.md` — 161-commit
  commit-by-commit C-source review log producing 13 fixes.
- **docs** `docs/asyn-rs-c-audit.md` — 22-item C-source audit
  refreshed, all items resolved.

### Post-tag amendments (re-tagged 2026-05-14)

The v0.17.0 tag was moved forward to include four follow-up commits
that landed in the same day, after CI on Linux surfaced two issues
invisible to our macOS pre-push pass:

- **fix(net)** Linux-only `async_udp_v4.rs` — `try_io` closure
  return is auto-flattened by `UdpSocket::try_io`, so the match arm
  needs `return Ok(out)` not `return out` (E0308). Two raw-pointer
  derefs in `sockaddr_storage_to_socketaddr` now have explicit
  `unsafe { ... }` blocks per edition 2024
  `unsafe-op-in-unsafe-fn`. Both fixes are `#[cfg(target_os =
  "linux")]`-gated, so macOS hosts never compiled them. (`41e3273`)
- **test(ca)** `protocol_tests.rs` — two CaClient tests that
  mutate `EPICS_CA_*` via `std::env::set_var` raced under libtest's
  multi-thread runner. Added `#[serial]` matching the convention
  in `client_server.rs`. nextest sidesteps the race by giving each
  test its own process; libtest does not. (`3dd5961`)
- **ci** Switched the workflow from `cargo test --workspace` to
  `cargo nextest run --workspace` so CI matches our local
  pre-push pass (each test in its own process, no shared env
  state). Added a separate `cargo test --doc --workspace` step
  because nextest does not cover doctests. (`14149f4`)
- **docs(readme)** Version refs in the install / Cargo.toml
  snippets bumped from `0.15` to `0.17`, plus a v0.17.0 release
  blurb. (`70bfcf3`)

## v0.16.2 — 2026-05-12

Wire-faithful introspection for native PVA PVs registered by IOC code
(NDPluginPva, custom benchmark PVs). The qsrv adapter previously
recovered each PV's `FieldDesc` from its current value at every
introspection query — fine for structure-rooted normative types where
every field is exercised, but **lossy** for types whose root variant
cannot be reconstructed from the value alone:

- top-level `UnionArray` → empty `variants` list (item `selector`
  indices would misalign on best-effort recovery),
- `Union` → only the currently-selected variant (sibling variants
  in the original descriptor missing),
- empty `ScalarArray` / `StructureArray` → fall back to `Double` /
  empty struct.

Top-level UnionArray PVs were thus not round-trippable; NTNDArray's
`value` union advertised only the current frame's pixel type rather
than all ten scalar-array variants.

### epics-bridge-rs (qsrv adapter)

- **feat**: `PvaPvHandle` gains an optional `descriptor: Option<FieldDesc>`
  carrying the producer's canonical introspection. `QsrvPvStore::get_introspection`
  prefers it over `PvField::descriptor` recovery; absent, falls back
  to the prior value-derived path (preserving behavior for callers
  that don't populate it).
- **feat**: invariant gate `assert_handle_root_kind` enforced at both
  registration paths (`register_pva_pv_global` for the producer-side
  global registry, `QsrvPvStore::register_pva_pv` for the runner-side
  wire-up). Mismatched root kind (e.g. `FieldDesc::UnionArray` with a
  `PvField::Structure` value) panics at the producer call site rather
  than surfacing as garbled wire frames on a downstream client.
- **fix**: `register_pva_pv` doc — `get_snapshot` → `get_value`.
- **test**: four tests — supplied-descriptor wins, documented lossy
  fallback when `None`, panic on root mismatch at both gates, and
  end-to-end wire round-trip via `PvaServer` + `PvaClient::pvinfo`.

### ad-plugins-rs (NDPluginPva)

- **feat**: `NDPvaConfigure` now passes `nt_nd_array_desc()` as the
  canonical descriptor at registration. Clients introspecting an
  NTNDArray PV see all ten variants of the `value` union instead of
  whichever scalar-array variant the current frame happens to use.

### epics-pva-rs (`PvField::descriptor`)

- **docs**: enumerate the five lossy recovery paths (empty
  `ScalarArray`, empty `StructureArray`, `Union` siblings,
  `UnionArray` variants, `Variant`/`Null` degrade to bare `Variant`)
  and point callers at the plumb-through API
  (`PvaPvHandle::descriptor`) when they need wire-faithful
  introspection.

### examples/mini-beamline

- **feat**: `register_pva_bundle` forwards the bundle's
  type-stability-guard `expected_desc` through to `PvaPvHandle`.

## v0.16.1 — 2026-05-11

Beacon-monitor false-positive fix on the CA client side. Long-lived
CA clients with a mature steady-state EMA were firing a stream of
`PeriodCollapse` warnings — `tracing::warn!("IOC may have restarted")`
+ transport-watchdog sticky flags + search/EchoProbe cascades — every
time an unrelated peer client connected to the same IOC. The
signature (id monotonic + interval suddenly far below the EMA) was
never a real restart: it identifies the IOC's `rsrv online_notify_task`
`beacon_reset` ramp-up cascade kicked off by any peer's TCP
accept/disconnect (`server/beacon.rs:124`, `tcp.rs:450`), not a
server restart. Real restarts reset `beacon_id` to 0 and trip
`IdMismatch` instead, and our own broken circuits already get
`BeaconControl::ResetServer` pre-emptively from the coordinator.

### epics-ca-rs (client)

- **fix**: retire the `PeriodCollapse` classification path in
  `beacon_monitor::handle_beacon`. The branch (id monotonic +
  `actual_interval < period_estimate / 3` past the 50 ms floor)
  now self-resets `period_estimate` + `count` and returns `None`
  instead of emitting `BeaconAnomalyKind::PeriodCollapse`. The
  post-condition mirrors `apply_reset_server`, so the ramp-up
  cascade reseeds the EMA from the live cadence without waking
  search or firing EchoProbes on healthy circuits. `IdMismatch`
  remains the sole real-restart classification; the
  `PeriodCollapse` variant is retained (`#[allow(dead_code)]`) for
  the negative-assertion test surface and future
  distinguishing-signature work.
- **test**: rename
  `period_collapse_classifies_as_period_collapse` →
  `monotonic_id_sub_period_clears_ema_no_anomaly`. The new test
  asserts that no `ForceRescanServer` is emitted on the
  monotonic-id sub-period beacon and that `period_estimate` is
  cleared and `count` zeroed (then +1 for the accepted beacon).
- **test**: add `peer_connect_ramp_up_does_not_fire_period_collapse`
  — drives a mature steady-state EMA (1000 beacons, 15 s estimate)
  through the IOC's documented 20/40/80/…/10240 ms ramp-up cascade
  with monotonic ids and asserts no PeriodCollapse fires on any
  ramp-up step. Captures the exact production symptom that prompted
  the fix.

## v0.16.0 — 2026-05-11

Access-security as a closed invariant: every wire op (CA + PVA) now
flows through a type-state `AccessChecked` token minted by a
per-source `AccessGate`. The unforgeable token replaces ad-hoc
`name: &str + ctx` ACL plumbing and makes "missed an ACF site" a
compile error rather than a runtime audit finding. The release also
absorbs ~30 upstream epics-base / asyn / pva PR-equivalents queued
during v0.13–v0.15 reviews (alias-aware DB lookup, alarm-message
strings, JSON link options, hardware-link parser, asyn UInt64,
asyn:READBACK, dbCreateRecord/dbDeleteRecord/dbglob/dbgrep, calc
A–U inputs, dfanout IVOA, bi/mbbi AFTC, …) and the resulting
50-plus self-review rounds (`upstream-tracking` doc captures
provenance per item).

### epics-base-rs

- **feat**: `AccessChecked` + `AccessGate` type-state foundation.
  `AccessChecked` cannot be constructed outside `AccessGate::check`,
  so any code path that operates on a PV by name must thread it
  through and inspect `allows_read()` / `allows_write()`. Closes
  the "missed an ACF site" pattern that surfaced across review
  rounds 32–39. `AccessLevel::ReadWrite` / `Read` / `NoAccess`
  exposed on the token.
- **feat**: ACF `METHOD` / `AUTHORITY` rule clauses (epics-base
  PR #563 + #618 partial). Rules can now scope on the auth method
  (`anonymous`, `ca`, `x509`, `cap-token`) and the cert authority
  / issuer DN; `check_access_method` extends the legacy
  `check_access_asl` without breaking the empty-`method`/`authority`
  rules (legacy ACFs keep working). `check_access_asl` re-routed
  through the method-aware path so both surfaces apply the same
  semantics.
- **feat**: alarm message `AMSG` / `NAMSG` strings on every record
  (epics-base PR #568 + #566). `AMSG` populated on `UDF` /
  `CALC` / device-support alarm paths; `NAMSG` carries the
  not-acknowledged form so HMI displays can render an audit trail
  per alarm.
- **feat**: record alias parsing + alias-aware lookup (epics-base
  PR #336). `PvDatabase::add_alias` validates names, refuses
  collisions across simple-PV / record / alias namespaces, and
  the `find_entry` / `get_record` / `has_name` family resolves
  aliases transparently. `dbpr` / `dbsr` / `dbgrep` / `dbglob`
  iterate aliases too. Field I/O (`get_pv` / `put_pv`),
  link processing, CP-link registration, and PVA `channelList` /
  qsrv all routed through the alias-aware path so an alias is a
  drop-in replacement for the record name anywhere.
- **feat**: JSON-style inline link options + hardware-link parser
  (epics-base PR #86 + #213). Inputs / outputs accept
  `{"const":42}` / `{"calc":"A*2","args":["@OTHER.VAL"]}`-style
  inline link syntax; the hardware-link grammar carries
  `card`/`signal`/`parm` for VME-style records.
- **feat**: dfanout `IVOA` / `IVOV` invalid-output handling
  (epics-base PR #688). On UDF/INVALID input the record routes
  to "do nothing" / "set to IVOV" / "set to IVOV on every
  out-link" per `IVOA` (0..2). Route 2 fans the IVOV value to
  every per-record output.
- **feat**: bi / mbbi `AFTC` alarm-filter time-constant (epics-base
  PR #817). Suppresses alarm transitions shorter than `AFTC`
  seconds so noisy contacts don't flap STATE / SEVR.
- **feat**: bi / bo / mbbi accept enum-string on VAL writes
  (epics-base PR #183). `caput bi:val "ON"` resolves through
  ZNAM/ONAM (bi/bo) or `OneSTR..FfvlSTR` (mbbi) instead of
  requiring the numeric index.
- **feat**: calc engine inputs A–L extended to A–U (epics-base
  PR #655). Existing CALC / CALCOUT / ACALCOUT / SCALCOUT record
  types grew the new `INP[M..U]` fields and the lexer/parser
  accept the extended identifier set.
- **feat**: db-loader captures `info(tag, value)` directives on
  records, exposes them through `RecordInstance::info_tag` and
  the `dbpr` level-≥2 output. asyn-side consumers
  (`info(asyn:READBACK, …)`) plug into the parameter-callback
  registry through this.
- **feat**: db-loader validates record / alias names per
  epics-base PR #78 (`A-Z 0-9 _ - + : ; [ ] < >` allowed; first
  char alpha-or-underscore; empty/too-long rejected).
- **feat**: iocsh `afterIocRunning <command>` queue (epics-base
  PR #558). Commands queued at parse time run after PINI
  completes, through a fresh IocShell so user shell commands and
  built-ins are both visible.
- **feat**: iocsh `dbCreateRecord <type> <name>` + `dbDeleteRecord
  <name>` (epics-base PR #812 + #505). Record deletion cleans
  every back-reference (scan index, CP links, alias targets,
  device support) under a single registration mutex so the DB
  can shed records at runtime without leaving phantoms.
- **feat**: iocsh `dbglob <pattern>` alias-aware + `dbgrep <pattern>
  [<fields>...]` (epics-base PR #613 / #626). `dbglob` now
  surfaces aliases alongside records; `dbgrep` accepts a
  positional fields list so `dbgrep "*:TEMP" VAL EGU` prints
  exactly the requested fields.
- **feat**: iocsh bound history + `pushd` / `popd` / `dirs`
  (epics-base PR #459 + #497). History capped at 1024 lines;
  directory stack mirrors bash so multi-IOC `iocshLoad`-from-
  subdir setups stay navigable.
- **feat**: `iocsh dbpr` surfaces `info()` tags at level ≥2 and
  record aliases alongside the canonical name. `dbpf` typos
  suggest the nearest field name (Levenshtein within 2) instead
  of just rejecting.
- **feat**: `dbsr` / `dbgrep` / `dbglob` include simple PVs (not
  just records) when iterating, so monitor / audit tooling sees
  the full PV surface.
- **fix**: route record `ASL` through ACF checks for every
  record-aware path (CA + PVA + qsrv). Records previously had
  `asl` parsed but unused by the runtime; the per-record gate
  now disables rules below the record's ASL when checking access,
  matching C-EPICS semantics. Accept `.db` String form (`"ASL0"`
  / `"ASL1"`) as well as the integer in `dbd`-loaded records.
- **fix**: serialize `update_scan_index` + un-block `records.read`
  on PINI scan (review round 35). PINI scans previously held the
  records read-lock across the entire periodic-task spawn,
  blocking interactive `dbpr` / monitor traffic while a large DB
  initialised; the spawn now happens after dropping the lock and
  `update_scan_index` is gated by the registration mutex so
  removals don't race in.
- **fix**: scan + scan_event use a `JoinSet` per scan rate so
  cancellation (DB drop / `dbDeleteRecord`) aborts every
  periodic task instead of leaving orphans (round 45).
- **fix**: reap dead subscriber `Sender`s before the per-PV /
  per-field cap check (round 46). Without the reap, a client
  that connected, subscribed N times, and disconnected would
  burn the N slots until the next non-empty fanout, eventually
  blocking new MONITORs with a stale-cap rejection.
- **fix**: canonicalise `cp_links` registration (round 15) — the
  hash key is now the canonical record name so an alias
  registering as a CP source resolves to the same set the
  canonical name would. Closes the alias-aware-lookup invariant
  (round 12).

### epics-ca-rs (client)

- **feat**: `EPICS_CA_ADDR_LIST` DNS re-resolution wired into the
  search engine (R50-G2). Hostname entries (`my-host` vs.
  `1.2.3.4`) now re-resolve on every retransmit fanout, so an
  IOC that moves IPs is rediscovered without a client restart.
  `AddrEntry::new` preserves the original hostname so the
  resolver loop knows whether to re-query DNS.
- **feat**: hostname-preserving `AddrEntry` for DNS re-resolution
  (Launchpad #488). Replaces the legacy `SocketAddr`-only entry
  shape that lost the original hostname after the first
  resolution.
- **feat**: bound nameserver TCP send queue (Launchpad
  #739789). A nameserver that hangs no longer grows the queue
  unbounded; backpressure surfaces as a connect error after the
  per-entry deadline.
- **feat**: honour `EPICS_CA_AUTO_ADDR_LIST=NO` with empty
  `EPICS_CA_ADDR_LIST` — previously the client fell back to the
  broadcast list even when both flags asked it not to.
- **fix**: per-server beacon EMA reset on TCP (re)connect — a
  reconnect now starts fresh instead of inheriting the previous
  session's beacon-drift estimate.
- **fix**: `CaClient` and `ServerConnection` abort spawned tasks
  on `Drop` so a dropped client doesn't leak the search /
  beacon / read-loop tasks.

### epics-ca-rs (server)

- **feat**: CA type-state ACF gate (round 44). CA server enforces
  the same `AccessGate` → `AccessChecked` flow as PVA, so the
  PUT / READ / EVENT_ADD / PUT_ACKT / PUT_ACKS paths all consult
  one source of truth.
- **fix**: enforce `access_rights` on CA READ + EVENT_ADD
  (round 38) — a peer flagged `NoAccess` on the ASG now gets
  `ECA_NORDACCESS` on `caget` and an `EVENT_CANCEL` -shaped
  teardown on `camonitor` instead of silently receiving values.
  Default flipped to deny-first.
- **fix**: gate `PUT_ACKT` / `PUT_ACKS` on access; tear
  subscriptions on re-eval to `NoAccess` (round 39). An ACF
  reload that demotes a peer mid-monitor closes the existing
  subscription instead of leaving it streaming under the old
  policy.

### epics-pva-rs (server)

- **feat**: `PvaServer::reload_acf_from(&Path)` + `AcfCell`
  runtime swap (round 28). The ACF policy can be hot-swapped
  without restarting the server; an `acl_version` counter bumps
  on every swap so MONITOR loops detect the change and re-check
  on the next event.
- **feat**: per-PV ASG resolver (round 30D) — sources hand back a
  `(ASG, ASL)` tuple per PV name so a single composite gateway
  can serve PVs from multiple ASGs without splitting into
  separate sources.
- **feat**: `AccessChecked` type-state across every PVA op —
  `get_value_checked` (round 41), `put_value_checked` /
  `subscribe_checked` / `subscribe_raw_checked` / `rpc_checked`
  (round 42). Legacy `*_ctx` paths removed (round 43): the
  AccessGate is now the single allowable entry point.
- **fix**: PVA PUT / GET / MONITOR / RPC enforce ACF (rounds 17,
  18, 37). Pre-fix the wire layer pulled an `AccessControl`
  per op but never consulted it; now every op flows through
  `AccessGate::check` before reaching the source.
- **fix**: gateway-side ACF enforcement (round 29). A gateway
  in front of an unguarded IOC now applies its own ACF; the
  downstream peer's credentials are forwarded to the upstream
  client pool through `ChannelContext` (PG-G10).
- **fix**: ctx-aware `get_value` for `PUT_GET` readback and raw
  MONITOR initial snapshot (round 30A). The readback / initial
  paths consulted the legacy ctx-less surface, so a deny-ACL
  source still emitted the readback value; both now go through
  `get_value_checked`.
- **fix**: composite `access()` gate aggregates inner ACL
  versions (R50-G1). The composite previously inherited the
  default `Open` gate (version=0 forever), so the monitor
  reload loop never noticed a child's `set_acf` bump. The
  aggregator surfaces inner bumps via a `wrapping_sum` of every
  inner gate's version — a `max(...)` shape produced false
  negatives when a smaller inner bumped under the existing
  peak (R50 follow-up).
- **fix**: monitor reload re-check routed through
  `ChannelSource::revalidate_read` (R50 audit-3). The composite
  aggregate gate is a change-signal only; allow/deny revalidation
  resolves the matched inner source by name and routes the
  check through THAT inner gate (the same gate
  `subscribe_checked` / `get_value_checked` consult). Pre-fix
  the recv loop called `access_gate().check(...)` on the
  composite — an `Open` gate — and kept forwarding events
  after a child flipped to deny.
- **fix**: include aliases in `channelList` output (round 14) —
  PVA `info` channels now report the alias surface so external
  discovery tooling sees the same PV inventory as `dbpr`.
- **fix**: reject duplicate registrations + route IVOA=2 to per-
  record output (round 30B+C), close ACF/registration invariant
  gaps (round 32), and per-PV ASG resolver (round 30D) — the
  "round 30" set tightened registration single-ownership so a
  re-add of an existing PV is an error rather than a silent
  shadow.
- **fix**: `EPICS_PVAS_AUTO_BEACON_ADDR_LIST=NO` honours the
  empty list — previously the server-side code fell back to the
  broadcast list (parallel to the CA-side round-25 fix).

### epics-bridge-rs (pva_gateway / qsrv / pvalink)

- **fix**: `GatewayChannelSource::set_acf` is the single owner
  of ACL-change visibility (round 49 follow-up). Every external
  ACF swap now bumps `acl_version` through the same call so
  downstream monitor loops can detect the change; no other path
  can mutate ACL state without bumping the counter.
- **fix**: pvalink dedup in-flight registry opens (round 36).
  Concurrent first-callers no longer each spin up their own
  `PvaLink` + monitor task; a `Notify` parks the loser on the
  winner's open future. Eliminates 2× search / connect round-
  trips per concurrent open under DCL.
- **fix**: bridge `AccessControl` to epics-base ACF (round 19).
  qsrv records now consult the same ACF that the native CA / PVA
  servers do — a unified gate across the three surfaces.
- **fix**: 4 external audit findings each in rounds 48, 49 —
  raw-subscribe counter underflow, gateway acf_cell single
  owner, pvalink Notify wake-loss, version capture order
  before-mint, audit details in the round commits.

### asyn-rs

- **feat**: `AsynUInt64` / `AsynUInt64Array` interfaces, plus
  `UInt64` `ParamValue` / `ParamType` plumbing (asyn PR #231).
  Drivers can now register and post `u64`-typed parameters
  end-to-end without truncating to `i32`.
- **feat**: `AsynFloat64::get_limits` (asyn PR #218). Returns
  `(low, high)` for the bound parameter so device-support
  layers can enforce range without a per-driver hack.
- **feat**: `asyn:READBACK` driver-callback path (asyn PR #60 /
  #208). A db `info(asyn:READBACK, "PORT,addr,REASON")`
  directive wires a `paramCallback`-driven readback into the
  bound record without separate `ao` + `ai` plumbing.
- **feat**: `IocBuilder::register_asyn_device_support` companion
  helper so an asyn-backed IOC can register drivers + records in
  one builder chain.
- **fix**: reject duplicate port names instead of silent shadow
  (asyn PR #34).
- **fix**: forward `info()` tags from `wire_device_support` too
  so asyn-side parameter-callback registration sees them.

### Stability / tests

- **test**: regression-guard 3 audit-rated upstream defects
  (stability suite). Pins the wire-format expectation for the
  symptoms that drove these review rounds so a future
  regression fails the test instead of the production server.

### Notes

- `cargo clippy --workspace --all-targets -- -D warnings` clean
  on the release commit. Workspace `nextest run` passes all
  3206 tests.
- `cargo workspaces` is the publish driver — crates are
  published in dependency order. Example crates
  (`examples/*`) and `*-fuzz` crates remain `publish = false`.
- The `feat/upstream-pr-fixes` branch carries a parallel
  `upstream-tracking` doc (`docs/upstream-tracking.md`) with
  per-PR provenance for every feature in this release; consult
  that file when wondering "which epics-base PR does X track?".

## v0.15.0 — 2026-05-07

Five Layer-2 feature-map gaps closed with audit-driven follow-throughs.
Each feature ships as one logical commit on top of the v0.14.2 patch
line; the public surface grows in a forward-compatible direction
(`#[non_exhaustive]` applied where new variants / fields are likely).

### epics-base-rs

- **feat**: per-variant `DBR_GR_*` (21..27), `DBR_CTRL_*` (28..34),
  `DBR_STS_*` named constants alongside the existing `DBR_TIME_*`,
  plus `DBR_STRING..DBR_DOUBLE` natives and `DBR_INT`/`DBR_STS_INT`/
  `DBR_TIME_INT`/`DBR_GR_INT`/`DBR_CTRL_INT` aliases for `Short`.
  Adds `DbFieldType::sts_dbr_type()` / `gr_dbr_type()` mirroring the
  existing `time_dbr_type()` / `ctrl_dbr_type()`. (CA-263 / CA-264)
- **feat**: `DBR_CLASS_NAME` (38) wire encode/decode in
  `types/codec.rs`. `Snapshot` grows a `class_name: Option<String>`
  field populated by the server before encoding so `caget -d
  CLASS_NAME` returns the actual recordType (`ai`, `bo`, `waveform`,
  …) instead of an empty/garbage response. The encode path
  early-returns CLASS_NAME *before* `convert_and_serialize` —
  otherwise a waveform PV's value is `Display`'d into a throwaway
  joined string. The decode path rejects payloads shorter than 40
  bytes with `CaError::Protocol` instead of silently truncating.
  `LAST_BUFFER_TYPE` follows. (CA-268)
- **feat**: `Snapshot` annotated `#[non_exhaustive]` so future field
  additions don't break external struct-literal construction; bridge
  `qsrv/pvif.rs` test sites migrated to `Snapshot::new` + field
  assignment.

### epics-ca-rs (client)

- **feat**: `CaChannel::search_attempts() -> u32` — libca
  `ca_search_attempts(chid)` parity (CA-035). Counts every UDP
  SEARCH fanout (immediate first SEARCH after `Schedule` AND each
  bucket-tick retransmit). One increment per fanout call regardless
  of how many UDP datagrams the addr-list / nameserver duplication
  produces. Cleared synchronously inside `run_coordinator`'s
  `TransportEvent::ChannelCreated` arm *before* waking
  `connect_waiters` and *before* emitting
  `ConnectionEvent::Connected` so a caller awakened by `Connected`
  cannot observe the pre-connect non-zero count. The shared atomic
  counter uses `fetch_add(1)` so beacon `poke()`'s reset of the
  scheduler's internal backoff counter doesn't make the user-visible
  diagnostic regress.
- **feat**: `CaClient::set_exception_handler` /
  `clear_exception_handler` — libca `ca_add_exception_event`
  (CA-130). Per-client (not process-global) slot dispatched from
  `run_coordinator` for `TransportEvent::ServerError` (server-emitted
  `CA_PROTO_ERROR`) and `TransportEvent::ServerDisconnect`
  (`CA_PROTO_SERVER_DISCONN`). New `CaException` /
  `CaExceptionKind` types, both `#[non_exhaustive]`. `CaException
  .status` carries the ECA code parsed from the response header's
  `hdr.cid` (where the server places `eca_status` per
  `send_ca_error`); the original request's command code is appended
  to `message` text as `(while processing cmd=N)` so the diagnostic
  context survives without confusing it with the ECA.
  `TransportEvent::ServerError` grows an explicit `eca_status: u32`
  field — previously the transport read the first u16 of the
  payload (= original cmd byte) and routed it as `status`, leaving
  handler users matching on `ECA_BADTYPE` to receive
  `CA_PROTO_READ_NOTIFY`-shaped values.

### epics-ca-rs (server)

- **feat**: `DBR_CLASS_NAME` wire-correct emission across **every**
  emission site — `READ` / `READ_NOTIFY`, the `EVENT_ADD`
  per-event encode loop in `server/tcp.rs`,
  `send_monitor_snapshot`, *and* `server/monitor.rs::send_event`
  (the `SimplePv` subscription path that goes through
  `spawn_monitor_sender`). Forces `element_count = 1` regardless of
  the underlying value count and populates `Snapshot.class_name`
  from `record.record_type()` on monitor paths. Without this
  override at every site, a waveform PV with `N` elements emitted
  `count=N` + 40-byte body which makes C clients parse `40*N` body
  bytes and fail.

### epics-pva-rs (client)

- **feat**: `PvaClient::pvput_build(name, |&mut PvField| -> Result)`
  — pvxs `PutBuilder::fetchPresent(true).build(cb)` parity
  (PVA-065). One round-trip read-modify-write — useful for
  "increment a counter", "toggle a bit", "splice into an array"
  workloads where the new value depends on the current. **Scope**:
  closure sees and round-trips the `.value` subfield only.
  Modifications to `alarm` / `timeStamp` / `display` / any other
  structure subfield are **not** persisted — the PUT pvRequest is
  `field(value)` to match pvxs `Put` semantics and avoid silently
  writing back stale alarm/severity. Use `pvput_field` for
  non-value subfields.

### epics-pva-rs (config)

- **feat**: `config::env::expand_dollar_vars(&str) -> String` —
  pvxs `Config::expand()` parity (PVA-466). `$(VAR)` and `${VAR}`
  substitution against the process environment; unset macros
  collapse to empty (matches the C IOC `dbLoadRecords` convention),
  unterminated `$(...` is preserved verbatim so callers fail loudly
  instead of silently swallowing trailing text. Wired into **every**
  path-bearing or addr-bearing PVA env reader: `parse_addr_list_with_port`
  (transitively `EPICS_PVA_ADDR_LIST` /
  `EPICS_PVAS_BEACON_ADDR_LIST` / `EPICS_PVA_NAME_SERVERS` /
  `server_addr_list`), `list_intf_addresses`,
  `server_intf_addr_list`, `server_ignore_addr_list`,
  `search_engine::join_addr_list_multicast` (multicast group join —
  active SEARCH was already covered, but multicast beacon
  discovery / fast-reconnect was silently dropping templated
  groups), the `env_has_dest` emptiness probe, the legacy
  `client_native::search::parse_addr_list` public surface, and the
  three TLS keychain paths (`EPICS_PVAS_TLS_KEYCHAIN`,
  `EPICS_PVA_TLS_CA_KEYCHAIN`, `EPICS_PVA_TLS_KEYCHAIN`).
  `EPICS_PVAS_TLS_KEYCHAIN_PASSWORD` is intentionally skipped
  (passwords should not substitute env refs).

### Notes

- All public types added in this release (`CaException`,
  `CaExceptionKind`, `Snapshot.class_name` carrier, `MonitorEvent`-
  carrying paths) extend their containers with `#[non_exhaustive]`
  where applicable so future field/variant additions are
  forward-compatible.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
  `Snapshot` growing pushed `caget-rs`'s `GetResult::Time(Snapshot)`
  variant past `clippy::large_enum_variant`; resolved by boxing
  (`Time(Box<Snapshot>)`).

## v0.14.2 — 2026-05-06

Patch follow-up to v0.14.1. The headline fix is a PVA client search
parity bump: a remote peer that claims `0.0.0.0` or `127.0.0.1` as
its server address in the SEARCH_RESPONSE no longer drives the
client into ECONNREFUSED.

### epics-pva-rs (client)

- **fix**: peer-aware `rewrite_loopback` in the search engine. The
  UDP source address (peer) is now plumbed through
  `handle_search_response` and into `rewrite_loopback`, which
  substitutes the peer's IP whenever the wire-supplied server
  address is unspecified **or** loopback and the packet came from
  a non-loopback peer; loopback peers still fall back to
  `127.0.0.1`. Symptom: `pvget` against a remote Soft IOC bound to
  `INADDR_ANY` (or a pvAccessCPP server configured via
  `EPICS_PVAS_INTF_ADDR_LIST=127.0.0.1`) failed with
  `Connection Refused` on the connect because the previous
  `rewrite_loopback(addr)` blindly mapped the unspecified address
  to `127.0.0.1`. This goes beyond pvxs/pvAccessCPP parity:
  pvxs `procSearchReply` (client.cpp:842-843) and pvAccessCPP
  `SearchResponseHandler` (clientContextImpl.cpp:2626-2627) only
  rewrite `INADDR_ANY`; neither overrides explicit-loopback wire
  claims. The Rust client now does, on the assumption that a
  non-loopback peer cannot truthfully claim `127.0.0.1` as its
  reachable address.
- **test**: replace `rewrite_loopback_replaces_unspecified` (which
  asserted the old `→ LOCALHOST` behaviour and would have panicked
  under the new logic) with four targeted tests pinning each
  branch of the rewrite truth table:
  unspecified+remote-peer / unspecified+loopback-peer /
  explicit-loopback+remote-peer / explicit-loopback+loopback-peer.
- **style**: drop a `needless_return` introduced by the parent
  fix; `rewrite_loopback` is now an `if`/`else` expression.
  `cargo clippy --workspace --all-targets -- -D warnings` is
  green again.

### epics-bridge-rs

- **docs**: rephrase `PvaPvHandle` and the `PVA_PV_REGISTRY` doc
  comments. The registry hosts native PVA PVs produced by IOC
  code (NTNDArray, aggregate benchmark bundles, …), not only
  areaDetector NDPluginPva handles. No behaviour change.

### examples/mini-beamline (`publish = false`)

- **add**: native PVA waveform bundle. A 1 Hz publisher emits a
  unified `NTAggregate`-shaped structure (`mb:wfX:bundle`) plus
  per-record CA aliases. Demonstrates the bridge-rs native PVA
  registry path end-to-end.
- **fix**: type-stability + drop counting after a code review
  against pvxs `SharedPV` semantics. Field-name prefix and bundle
  `FieldDesc` are locked via `OnceLock` after the first publish;
  later mismatches are dropped with a log. `dropped_events`
  atomic counts `tx.try_send` failures so bench scripts can
  assert no silent loss. 9 unit tests cover trailing-index
  edges, `mark_processed` cycling, descriptor stability, and
  factory zero-count rejection.

## v0.14.1 — 2026-05-04

Performance follow-up to v0.14.0. Bulk CA reads (`bulk_caget`,
`get_many_with_timeout`, ophyd-epicsrs `bulk_get_pvs`) get a 2-3×
end-to-end speedup at N=100 PVs against the in-process softIoc:
roughly 220 µs → 90 µs. The wins come from two complementary
changes — a warm-GET registry on the CA client side and batched
response flushing on the CA server side — that together close
most of the bulk-read gap to PVA.

### epics-ca-rs (client)

- **perf**: warm-GET cache on `CaChannel`. The first successful
  default GET on a channel populates a `CachedRead { ioid, sid,
  server_addr, data_type, element_count, slot }`; subsequent
  `get_many_with_timeout` calls refill the slot's
  `Sender<ReadReply>` and reuse the persistent ioid, skipping
  per-call `alloc_ioid` + DashMap insert/remove + oneshot
  allocation. The new `ReadWaiter::Warm` registry variant lets
  the per-server `read_loop` dispatcher peek a Warm waiter via a
  read-locked `DashMap::get` (instead of write-locked `remove`)
  and take the response Sender from its `WarmReplySlot`. Mirrors
  `epics-pva-rs` `CachedGet` — see `pvget_many` in
  `client_native/context.rs` for the original design.
- **perf**: `get_many_with_timeout` drains pending oneshots
  sequentially (PVA `pvget_many` pattern). The read_loop fires
  per-server response burst back-to-back, so most rx's are ready
  by the time the await reaches them; sequential await is as
  fast as `FuturesUnordered` for the warm case and saves the
  per-item bookkeeping overhead.
- Cache invalidation: snapshot mismatch on
  `(server_addr, sid, data_type, element_count)` evicts the
  cached entry and removes its registry row; the next call
  falls back to cold and re-populates. `drain_waiters_for_cids`
  treats `Warm` and `OneShot` waiters the same — both reaped on
  disconnect, with the slot-held Sender (if any) signalled
  `Disconnected`.

### epics-ca-rs (server)

- **perf**: batched dispatch flush in `handle_client`. The
  per-message `flush()` calls inside `dispatch_message` and the
  `send_cmd_error` / `send_ca_error` helpers were forcing one TCP
  write per response, capping bulk-read throughput at the
  server-side response rate (~2.2 µs/PV at N=100). They now run
  into a 64 KB `BufWriter` with no inline flush; `handle_client`
  flushes once per outer read iteration after the inner
  message-drain loop, collapsing N responses into a single
  syscall per inbound TCP burst. Subscription delivery, async
  `WRITE_NOTIFY` completion, `send_monitor_snapshot`, and
  `reeval_access_rights` keep their own flushes — they run
  independently of the read loop and need their data pushed
  promptly. `READ_SYNC` becomes a no-op since the outer-loop
  flush fires for any iteration with offset > 0.
- **fix**: dispatch error path now flushes before propagating.
  `handle_client` matches dispatch's three result arms
  separately: `Ok(Ok(()))` is the hot path; `Ok(Err(e))`
  best-effort flushes any responses queued by earlier
  successful handlers in this batch (and any error reply emitted
  by the failing handler via `send_cmd_error`) before returning
  the error; `Err(_)` (send-timeout) intentionally skips the
  flush because the cancelled future may have left a partial
  frame in the buffer.

### bench

- **add**: `e2e_bulk_get_many_scaling` group runs the in-process
  softIoc bulk read at N = 10 / 20 / 50 / 100 PVs from a single
  client. Captures the per-N scaling shape so future server /
  client changes can be compared against it without eyeballing
  single-N numbers.
- **add**: `e2e_bulk_caget_many_cached_100pvs` measures the warm
  by-name `caget_many` path (channel cache hit + batched
  `get_many` underneath). Complements `e2e_bulk_get_many` which
  starts from pre-built channel handles.

### measurements (in-process softIoc, criterion)

| N   | v0.14.0  | v0.14.1 | Δ     |
| --- | -------- | ------- | ----- |
| 10  | 43.9 µs  | 36.6 µs | -17 % |
| 20  | 61.8 µs  | 43.3 µs | -32 % |
| 50  | 115.8 µs | 60.0 µs | -48 % |
| 100 | 223.3 µs | 93.3 µs | -58 % |

Per-PV server dispatch cost: 2.2 µs/PV → 0.9 µs/PV at N=100.

## v0.14.0 — 2026-05-04

Per-op coordinator round-trips removed from the CA client hot path
(Option C). Reads, writes and the channel-info lookups they depend
on now go straight from `CaChannel` to the per-server transport
without traversing the `tokio::select!` coordinator loop. Against a
localhost IOC, parallel `bulk_caget` workloads that previously
serialised through ~25 µs of coordinator-iteration overhead per
touch (3 touches per op) now run at the network round-trip floor.
Lifecycle work — search, connect, disconnect, beacon anomaly,
subscription registration — still flows through the coordinator,
which keeps the wire protocol semantics and reconnect logic
unchanged.

A second small batch of changes (PVA writer-task frame coalescing,
batched `caget_many` / `get_many` helpers, layer-1 reference
feature map covering 177 CA + 174 PVA upstream items) ships in the
same release.

### epics-ca-rs

- **perf (Phase A)**: direct read/write registry. `ch.get` /
  `ch.put` insert reply oneshots into a shared `InFlightOps`
  (DashMap keyed by ioid); the per-server transport `read_loop`
  removes and fulfils them on `CA_PROTO_READ_NOTIFY` /
  `CA_PROTO_WRITE_NOTIFY`. Removes 2 of 3 coordinator touches per
  op. `CoordRequest::ReadNotify` / `WriteNotify` and
  `TransportEvent::ReadResponse` / `ReadError` / `WriteResponse`
  removed (no longer constructed).
- **perf (Phase B)**: per-channel snapshot sidecar. The coordinator
  publishes `ChannelSnapshotPublic { sid, native_type,
  element_count, server_addr, access_rights, state }` into a shared
  `Arc<DashMap<u32, _>>` on every lifecycle change (ChannelCreated,
  AccessRightsChanged, ChannelCreateFailed, ServerDisconnect,
  TcpClosed, DropChannel, CircuitUnresponsive, CircuitResponsive).
  `CaChannel` hot paths read directly via `snapshot()`, eliminating
  the `CoordRequest::GetChannelInfo` round-trip — the third coord
  touch.
- **perf (Phase D)**: transport-shared `ServerLastRxAt` sidecar.
  The transport `read_loop` stamps it on every received TCP frame
  so `ca_receive_watchdog_delay` stays accurate for read-only and
  write-only workloads, whose responses no longer reach the
  coordinator.
- **perf (Phase E)**: per-server `DirectServerWriter` sidecar. Hot
  ops enqueue CA frames straight to the per-server writer task via
  `Arc<DashMap<SocketAddr, DirectServerWriter>>`, bypassing
  `transport::run_transport_manager` once a circuit is operational.
  Backpressure preserved via the existing
  `pending_frames: AtomicUsize` and `SEND_BACKPRESSURE_FRAMES`
  threshold (now public via `types::SEND_BACKPRESSURE_FRAMES`).
- **api**: batched read helpers — `CaClient::caget_many[_with_timeout]`
  and `CaClient::get_many[_with_timeout]`. Spawns N reads in
  parallel and joins; the canonical fast path for "read N PVs at
  once".
- **fix**: `DropChannel` now drains the in-flight registry for the
  cid. Prevents a bounded leak when a caller drops a get/put future
  (cancellation) and then drops the channel before either the
  response arrives or a disconnect drain runs.
- **test**: new `e2e_bulk_caget_parallel_20pvs` and
  `e2e_bulk_get_many_100pvs` benches; `bench_caget` /
  `bench_caput` / `bench_bulk_caget` no longer collide on a fixed
  port (`unused_local_port()`). New
  `response_arrives_before_disconnect_drain` regression pins the
  in-flight-vs-drain race semantics.
- **internal**: `CoordRequest::GetChannelInfo` and the unused
  `ChannelSnapshot` struct removed; coordinator state simplified
  (per-server `last_rx_at` / `read_waiters` / `write_waiters` maps
  gone — moved to the sidecars).

### epics-pva-rs

- **perf**: `ServerConn` writer task now drains queued frames via
  `try_recv` after the first await and issues a single
  `write_all(&batch)` for the combined buffer. Eliminates one
  syscall per frame under bursty workloads (subscribe storms,
  reconnect floods); cancel / backpressure semantics unchanged.

### docs

- **add**: `docs/reference-feature-map/{README,ca,pva}.md` — Layer 1
  stable inventory of the CA and PVA public API and wire protocol
  surfaces, extracted from `epics-base/modules/ca` (libca + rsrv,
  177 entries, pinned at `c9817fa59`) and `pvxs` (174 entries,
  pinned at `9beba6b`). Each entry has a stable ID, symbol,
  upstream `header:line` citation, and a one-line description,
  grouped by functional area. Layer 2 (epics-rs coverage overlay)
  is intentionally left as a future artifact.

## v0.13.6 — 2026-05-03

Fixes a false-positive `PeriodCollapse` cascade against IOCs that
emit standard `online_notify_task` ramp-up beacons (20 ms doubling
to 15 s) — including epics-ca-rs's own server, so an IOC built on
this crate would trip its own client-side beacon monitor on every
fresh run. Symptom in the field: `get_with_metadata(timeout=2.0)`
against the mini-beamline IOC failed for the first ~10 s of the
run, driven by transport watchdog flag → echo probe → 5 s
timeout → spurious channel disconnect.

### epics-ca-rs

- **fix**: `BeaconState::period_estimate` is now `Option<Duration>`
  with `None` initial. The first observed inter-beacon interval is
  adopted as the initial estimate (libca `bhe.cpp:51,199` parity:
  `averagePeriod = -DBL_MAX` until `averagePeriod = currentPeriod`
  on the second beacon), and the EMA blends in from there. The
  prior `Duration::from_secs(15)` placeholder caused every
  ramp-up beacon past the 50 ms `MIN_PERIOD_COLLAPSE_INTERVAL`
  floor to satisfy `actual_interval < period_estimate / 3` and
  classify as `PeriodCollapse`. The PeriodCollapse classification
  now skips when `period_estimate.is_none()`, which is by
  construction true until the second beacon — and after that
  the EMA tracks the actual server cadence rather than a hardcoded
  guess.
- **test**: regression guard
  `rsrv_rampup_beacons_do_not_fire_period_collapse` simulates the
  full 11-beacon ramp-up sequence (20 ms, 40 ms, …, 10.24 s) and
  asserts no `PeriodCollapse` fires past the initial
  `FirstSighting`. Total `epics-ca-rs --lib` is now 96 tests.

## v0.13.5 — 2026-05-03

Doctest-only patch on top of v0.13.4. The `process_bucket` doc
in `epics-ca-rs/src/client/search.rs` had a 4-space-indented
pseudo-code line that rustdoc treated as a Rust code block,
breaking `cargo test -p epics-ca-rs` (doctests). Production
behaviour unchanged from v0.13.4 — only docs.

### epics-ca-rs

- **fix**: wrap the `next = (idx + min(attempt, nBuckets)) %
  nBuckets` formula in a ```text fence so rustdoc skips it
  instead of compiling. Caught by GitHub Actions CI; pre-publish
  verification used `cargo test --lib` which skips doctests.

## v0.13.4 — 2026-05-03

PVA + CA reconnect-after-IOC-restart parity with pvxs
`Channel::disconnect`. Skips v0.13.3 because the PVA-only fix
matured into a paired PVA+CA bundle in the same release cycle.

End-to-end pvmonitor-rs / camonitor-rs reconnect after a brief
IOC restart was 5-30 s + dependent on beacon arrival luck;
post-fix it's ≤ 1 s on both paths (next 1 Hz tick).

### epics-pva-rs

- **fix**: `placement_bucket` / `cascade_smoothed_next` free fns
  extracted; `Reconnect` placement is now `current_bucket`
  (pvxs `client.cpp:213` parity, holdoff = 0). Per-channel retry
  escalation follows pvxs `tickSearch:1193-1196` (`nSearch+1`
  bucket forward push: 1 s, 2 s, 3 s, …, 30 s cap) plus 100+
  delta cascade smoothing (`client.cpp:1199-1206`). Removes the
  earlier `(current+1+sid%30)` formula and the misnamed
  `holdoff_cycles + RETRY_HOLDOFF_BUCKETS=10` that conflated
  pvxs's pre-CREATE_CHANNEL holdoff with the steady-state retry
  cadence.
- **fix**: `Channel::ensure_active` no longer applies an outer
  timeout to `Reconnect` `find()`. The 200 ms cap was
  cancelling SEARCHes before their bucket fired (F6 zombie-drop
  path). The engine now drives recovery indefinitely until
  SEARCH_RESPONSE arrives or the caller drops the future.
- **fix**: paired with the above, single-shot ops
  (`pvget` / `pvput` / `pvrpc`) now wrap `ensure_active` in
  `tokio::time::timeout(op_timeout, …)`. Without this the
  user-facing future would hang forever when the server
  permanently disappears. Monitor loops keep the bare
  unbounded await — their cancel path is `SubscriptionHandle`
  drop.
- **internal**: stale "sid-hashed" comments rewritten;
  `RECONNECT_FIND_TIMEOUT` constant deleted; `Pending.holdoff_cycles`
  field deleted; SearchReason doc rewritten with the new model.
- **test**: 7 unit tests against the extracted free fns
  (placement, escalation, smoothing default, smoothing boundary
  at delta=100), 3 integration tests
  (`reconnect_search_broadcasts_within_one_tick`,
  `hurry_up_kicks_pending_searches_at_fast_tick_cadence`,
  `retry_escalation_pvxs_pattern`), 1 regression guard
  (`reconnect_find_does_not_complete_without_response` — catches
  any future PR that reintroduces a caller-side timeout). Total
  `epics-pva-rs --lib` is now 191 tests.

### epics-ca-rs

- **fix**: same five-part pvxs-parity port applied to
  `epics-ca-rs/src/client/search.rs`. CA's 3-way `SearchReason`
  (Initial / BeaconAnomaly / Reconnect) folds into the shared
  `placement_bucket` shape: Initial and BeaconAnomaly land at
  `current+1` (immediate broadcast / fast-tick-driven retransmit
  pair), Reconnect at `current_bucket`. Pre-existing comment on
  `handle_request` flagged this as the gap that "made ca-rs
  single-channel reconnect feel slower than pva-rs"; the port
  closes it.
- **fix**: `process_bucket` re-arm is inline (split-borrow on
  `state.{pending, buckets}`) so `cascade_smoothed_next` sees
  the running per-tick bucket buildup. The earlier batched
  `rearm: Vec<(u32, usize)>` defeated within-tick smoothing — a
  5000-channel mass-disconnect saw delta=0 for every cid and
  piled them all into `current+1`. PVA-rs uses the equivalent
  pattern; this commit recovers parity.
- **internal**: `RETRY_HOLDOFF_CYCLES` const + `holdoff_cycles`
  field deleted; bucket-placement / retry-escalation logic
  shared via the same free-fn pair as PVA.
- **test**: replaced the now-incorrect `reconnect_bucket_spread`
  test (it asserted cid-hash spread, which we no longer do)
  with five unit tests against the extracted free fns + 2
  integration tests (`reconnect_search_broadcasts_within_one_tick`,
  `retry_escalation_pvxs_pattern`). Total `epics-ca-rs --lib`
  is now 95 tests.

## v0.13.2 — 2026-05-03

Single-fix patch for the multi-IOC-on-one-host workflow: starting a
second PVA IOC while a first was already running used to panic the
server task with `Os { code: 98, kind: AddrInUse }` from the
hard-coded TCP port 5075 bind. CA path was unaffected (its server
defaults to ephemeral TCP). UDP 5076 was already shareable across
local PVA processes via SO_REUSEADDR + SO_REUSEPORT, and the
v0.13.0 ORIGIN_TAG forwarding path already routes SEARCH between
co-bound UDP responders — only the TCP single-bind was the
remaining bottleneck.

### epics-pva-rs

- **fix**: `PvaServer::start` falls back to ephemeral (`tcp_port = 0`)
  on `AddrInUse` / `PermissionDenied` if the requested port is
  non-zero. Single retry, mirrors pvxs `serverconn.cpp:493-499`
  (`bind_addr.setPort(0); fallback = false; continue;`). The
  actually-bound port flows out via `bound_tcp_port` and is what
  the UDP responder advertises in SEARCH_RESPONSE / beacons, so
  clients still reach the second server. A `tracing::warn!` with
  requested / bound / error fields is emitted on fallback so
  operators can see when 5075 was contended.
- **test**: 2 unit tests in `tcp_fallback_tests` —
  port-blocked-by-pinned-listener triggers fallback (no panic,
  different port returned, blocker still holds original);
  requested-port-available happy path. Total `epics-pva-rs --lib`
  is now 184 tests (up from 182).

## v0.13.1 — 2026-05-03

CA client beacon-watchdog rewrite for libca parity. Eliminates a
false-positive disconnect storm that surfaced whenever multiple
`CaClient` instances co-existed in one process (e.g. pyepics shim +
ophyd-async backend + integration-test fixture, all in the same
Python process). Each fresh client used to fire a spurious
`EchoProbe` on its very first beacon from every IOC; under load
the 5-s echo deadline tripped and produced cascading
`TcpClosed` → reconnect events that the user perceived as random
"restored N subscriptions" log spam.

The fix splits beacon handling into a libca-style dual-path
design: search-engine wake-up on classified anomalies, and a
per-circuit watchdog that healthy beacons refresh and anomaly
beacons leave alone. Internal-only API changes — no public
breaking changes.

### epics-ca-rs

- **fix**: beacon anomalies are now classified into
  `BeaconAnomalyKind::{FirstSighting, IdMismatch, PeriodCollapse}`.
  Only the latter two emit warn-level "IOC may have restarted"
  diagnostics and feed the
  `ca_client_beacon_anomalies_total` metric; `FirstSighting` is a
  per-client bookkeeping event (our beacon map was empty for this
  server, not "the IOC restarted") and now logs at debug under
  `ca_client_beacon_first_sighting_total`. Resolves the misleading
  warns that started appearing for every IOC at process start when
  several `CaClient`s came up in parallel.
- **fix**: removed the immediate `EchoProbe` on beacon anomaly
  (libca's `tcpRecvWatchdog` model). The transport's per-circuit
  read loop is now structured around a single pinned
  `tokio::time::Sleep` whose deadline is mutated in place by:
  data arrival (clear flags + reset to now + 30 s — libca
  `messageArrivalNotify`), healthy beacon arrival (reset to
  now + 30 s when the anomaly flag isn't set — libca
  `beaconArrivalNotify`), and anomaly beacon arrival (set sticky
  flag, deadline UNCHANGED — libca `beaconAnomalyNotify`).
  Idle expiry still sends an echo and switches to the 5-s
  echo-pending state; the change is that we no longer pre-empt
  the 30-s schedule on a beacon anomaly. The previous fast-track
  was the trigger for the disconnect storm under load.
- **fix**: libca `bhe.cpp:179` parity (narrowed) — a beacon whose
  sequence number jumps forward by 2 or 3, or backwards by 1-4,
  is dropped as a duplicate-route artifact rather than classified
  as `IdMismatch`. Without this, multi-NIC sites that delivered
  redundant copies tripped the watchdog flag and suppressed
  healthy-beacon refreshes for ~30 s on what was in reality a
  perfectly healthy IOC. We deliberately narrow libca's
  256-id backwards window to 4 because our `IdMismatch` branch
  catches restart-to-1 directly via the id sequence (libca relies
  on period-collapse instead and would otherwise swallow that
  signal into the dedup path).
- **fix**: `BeaconArrival` routing falls back to port-only
  matching when the beacon's announced address doesn't exactly
  match an operational circuit's `server_addr`. Two real-world
  cases fold into the same fallback: INADDR_ANY (the IOC sent
  `available = 0` and the upstream repeater didn't rewrite it)
  and multi-homed IOCs whose beacon NIC differs from the NIC the
  search-reply came in on. Cross-host port collisions cause a
  benign false-refresh — the wrong circuit's deadline is pushed
  by 30 s, but its own watchdog still detects death within
  30 + 5 s if it actually died. Routing logic extracted into
  `beacon_arrival_targets` and unit-tested.
- **fix**: `connect_server` now runs in a `tokio::task::JoinSet`
  rather than awaiting inline in the transport command loop.
  Previously a 5-s TCP / 15-s TLS handshake on server A blocked
  every other command — `BeaconArrivalNotify` for already-
  connected circuits, `CreateChannel` for server B, etc. — for
  the duration. Per-server FIFO is preserved by a
  `HashMap<SocketAddr, Vec<TransportCommand>>` queue that drains
  on connect completion. Connect failure surfaces
  `ChannelCreateFailed` for queued `CreateChannel` commands and
  a single `TcpClosed` so the coordinator can clear server
  state.
- **internal**: introduced `CoordRequest::BeaconArrival` (separate
  from `ForceRescanServer`); `TransportCommand::EchoProbe` removed
  in favour of `TransportCommand::BeaconArrivalNotify`. Read loop
  drops `Arc<Notify>` echo_probe in favour of an
  `mpsc::UnboundedSender<bool>` per circuit. The `biased;` hint on
  read_loop's select is removed — tokio's randomized polling
  gives starvation-free fairness, and the bias was risking the
  opposite of what the comment claimed.
- **test**: 3 new transport `read_loop` virtual-clock tests
  (healthy-beacon-extends, anomaly-suppresses-refresh,
  data-clears-flag) using `tokio::test(start_paused = true)` and
  `tokio::io::duplex` for the mock peer; 5 new `beacon_monitor`
  classification / dedup tests; 6 new `beacon_arrival_targets`
  routing tests. Total `epics-ca-rs --lib` is now 89 tests, up
  from 76.
- **dev-deps**: added `tokio` with `test-util` feature for the
  virtual-clock tests.

## v0.13.0 — 2026-05-01

Operational fixes for the local-IOC zero-config workflow + CLI tool
output parity with the C counterparts. Two SEARCH-path bugs that
left `pvget-rs PV` hanging against a local pva-rs server are fixed
at root cause; the four CA / four PVA tools now match `caget` /
`pvget` byte-for-byte on the legacy `-M nt` path. New ORIGIN_TAG
forwarding port lands the pvxs UDP-collector mechanism for
multi-server-on-one-host topologies.

### Breaking
- CLI tool default output (caget-rs / camonitor-rs / caput-rs /
  cainfo-rs / pvget-rs / pvmonitor-rs / pvput-rs) now matches the
  legacy C tools byte-for-byte (PV name padded to 30 chars, `%g`
  6-digit precision, double-space ts↔value, `Old :` / `New :` echo
  with timestamps). Scripts that parsed the previous shape need
  updating.

### epics-pva-rs
- **perf**: `pvget-rs <PV>` first-response latency 1 s → 5–10 ms
  (legacy `pvget` parity). Two root causes: search-engine
  `Multi`/FindAll deadline check was tied to the 1 Hz tick — a
  `SEARCH_RESPONSE` arriving in 5 ms still sat in `accumulated`
  until the next tick; `channel.ensure_active` used `find_all()`
  (always 200 ms) for the initial resolve. Added a
  `tokio::time::sleep_until(earliest_deadline)` arm to the engine
  select; switched ensure_active to `find()` (delivers on first
  response) wrapped in a `MULTI_SERVER_WINDOW` timeout.
- **fix**: `EPICS_PVA_AUTO_ADDR_LIST=YES` (the default) now adds
  per-NIC IPv4 broadcast addresses + `127.0.0.1` to SEARCH
  destinations. Previously only `255.255.255.255` was sent — macOS
  doesn't reliably translate that into per-subnet broadcast, so
  local IOCs bound to `192.168.X.255:5076` never saw the SEARCH.
- **fix**: client beacon socket no longer binds the loopback NIC.
  SO_REUSEPORT load-balanced inbound packets between the client
  beacon socket and any local pva-rs server's UDP responder,
  randomly losing SEARCH traffic.
- **fix**: `EPICS_PVA_ADDR_LIST` DNS hostnames (`localhost`,
  `ioc01.lab.local`, etc.) now resolve. Spawn-time resolve via
  `parse_addr_list_with_port`; IPv4 preferred over IPv6 since
  macOS's `localhost` → `::1` ordering broke
  `EPICS_PVA_ADDR_LIST=localhost`.
- **feat**: ORIGIN_TAG forwarding port (server-side). UDP responder
  receives `CMD_ORIGIN_TAG` packets on `224.0.0.128` loopback
  multicast and forwards unicast SEARCHes via the same channel for
  multi-server-on-one-host topologies. `Origin` enum
  (Direct / FromOriginTag) tags how a SEARCH reached the responder;
  anti-loop guard prevents re-forwarding.
- **feat**: `PvaCodec::build_origin_tag_prefix` /
  `try_peel_origin_tag` codec helpers (24-byte prefix shape, BE PVA
  header `cmd=22 len=16` + IPv4-mapped IPv6 dest).
- **feat**: legacy `pv*` flag set on every PVA tool (`-V`, `-w`,
  `-r`, `-p`, `-M`, `-v`, `-q`, `-d`).

### epics-ca-rs
- **fix**: `EPICS_CA_ADDR_LIST` with an unresolvable hostname token
  now drops the bad entry and continues. Previously the first bad
  token propagated via `?` and panicked main() (caget / camonitor /
  caput).
- **feat**: full C-tool flag set on every CA tool — `-V`, `-t`,
  `-a`, `-d`, `-c`, `-p`, `-n`, `-#`, `-S`, `-e`/`-f`/`-g`, `-s`,
  `-lx`/`-lo`/`-lb`, `-0x`/`-0o`/`-0b`, `-F`. `-a`/`-l` echo real
  DBR_TIME server timestamps + alarm pairs.

### epics-base-rs
- **feat**: `IfaceMap::spawn_refresh(period)` — periodic
  `getifaddrs()` poller (pvxs `IfMapDaemon` parity, 15 s default).
  Refreshes the shared interface snapshot so dynamic-NIC scenarios
  (DHCP / hot-plug / VM live-migration) update SEARCH/beacon
  destinations without a process restart.
- **feat**: `AsyncUdpV4::bind_non_loopback(port, broadcast)` —
  bind one socket per IPv4 NIC except loopback. Used by the PVA
  client beacon socket to avoid SO_REUSEPORT races with co-located
  servers.
- **feat**: `bind_loopback_mcast(port)` — wildcard-bound UDP
  socket joined to `224.0.0.128` via `127.0.0.1` for the
  ORIGIN_TAG forwarding channel.

## v0.12.0 — 2026-04-30

PVA/CA stability sweep. A cold-eye cross-review across pva-rs / ca-rs
/ bridge-rs surfaced dead-letter disconnect paths and silent
beacon-anomaly handling that unit tests had missed; pvxs git-history
archaeology (1220 commits triaged) ported the remaining
protocol-level gaps.

### Breaking
- `SearchEngine::find` / `find_all` gain a `reason: SearchReason`
  argument so mass-reconnect cascades spread across the bucket ring
  instead of bursting in a single tick.

### epics-pva-rs
- Server-initiated `CMD_DESTROY_CHANNEL` now cancels every in-flight
  op for the destroyed SID; monitor streams previously hung silently
  until the whole TCP connection died.
- `CMD_MESSAGE` from server is decoded and logged at the level
  matching its `mtype` — previously corrupted the monitor data
  stream (mtype=0) or was silently dropped (Warn/Err).
- `DESTROY_REQUEST` payload corrected to `sid + ioid` (no spurious
  subcmd byte) for pvxs interop.
- `DiscoverPing` uses the spec wire format (`MustReply` flag, empty
  protocol/channel list, raw `::`); old packet shape was silently
  ignored by pvxs servers.
- `random_guid()` seeds from `/dev/urandom`; `BeaconTracker` caps
  at 20 000 entries with warn-once on cap (DoS / poison-feed
  defence).
- Reason-aware search: `Reconnect` is sid-hashed across all 30
  buckets with no immediate fire; `Initial` keeps the immediate
  broadcast for fast single-channel latency.
- `find_all` flushes `Vec::new()` at deadline + drops closed
  responders so missing PVs no longer hang past the caller's
  timeout.

### epics-ca-rs
- `CMD_SERVER_DISCONN` wakes blocked `caget` / `caput` waiters with
  `CaError::Disconnected` (was leaving them parked until the
  caller's outer timeout).
- Reason-aware search scheduler matches the pva-rs split.
- Beacon stale-prune (180 s) replaces the every-beacon soft-poke
  that trapped multi-IOC networks in 200 ms fast-tick mode.
- Multi-NIC duplicate-beacon detection (same `cid`) + 50 ms
  period-collapse floor.
- Nameserver TCP read task no longer leaks on client shutdown.

### epics-bridge-rs
- `BeaconAnomaly::request()` from `ca_gateway` now actually emits a
  beacon via the new `CaServer::beacon_anomaly_handle` pulse —
  previously silent on the wire.

### Tooling / docs
- `archaeology/pvxs/` — full pvxs 1220-commit cross-check with
  per-batch verdicts.

## v0.11.1 — 2026-04-29

Patch release. v0.11.0 shipped a regression in which default
`pvmonitor` callers stalled after ~5 events; this release fixes that
and supersedes v0.11.0 (which has been yanked from crates.io).

### epics-pva-rs
- **fix**: server-side pipeline credit window (P-G11, originally added
  in v0.10.5 → 0.11.0 round-6) was applied unconditionally to every
  Monitor op. pvxs only enables flow control when the client's
  pvRequest sets `record._options.pipeline=true`; without that opt-in,
  default `pvmonitor` callers stalled after the initial snapshot + 4
  updates. The window is now gated on the actual pvRequest option.

## v0.11.0 — 2026-04-29

Highlights since v0.10.5:

### epics-pva-rs
- Axum-style PVA service framework: `#[pva_service]` attribute macro,
  `#[derive(NTScalar)]` / `#[derive(NTTable)]`, `pvget_typed` /
  `pvput_typed` / `pvmonitor_typed` typed entry points.
- Zero-copy `ScalarArray` encode/decode (memcpy fast path) and
  raw-frame monitor forwarding (default-on in bridge gateways).
- `PvaClient` API: `discover()`, `pvget_with_request()`,
  `pvput_with_request()`, `pvmonitor_with_request()`, `pvput_field()`,
  `pvinfo()` now uses `GET_FIELD` instead of full `GET`,
  per-server TLS SNI from `EPICS_CA_NAME_SERVERS` hostnames,
  peer connection stats, async `Operation` handle.
- `PvaServer::start` binds the TCP listener synchronously inside
  `start()`, removing the pick-and-drop race that previously needed
  cross-binary serialisation in tests.

### epics-bridge-rs
- `pva_gateway` — PVA-to-PVA proxy mirroring pvAccessCPP's `pva2pva`
  / `p2pApp`, with multi-downstream fan-out and tower-style
  middleware (`ReadOnlyLayer`, `AclLayer`, `AuditLayer` with mpsc
  sink + `Put` / `Get` / `Subscribe` / `Rpc` event kinds).
- Stability gap closures from kodex-driven re-audits (rounds 4–18):
  segmented-message reassembly, `Vec::with_capacity` OOM caps,
  beacon burst-then-slowdown smoothing, server task leak on
  disconnect, GET_FIELD on unknown SID, audit-string allocation
  cap, search-request OOM follow-up.

### epics-tools-rs
- `procserv` — Rust port of `epics-modules/procServ` (forkpty
  child supervisor with restart policy and telnet log shell).

### Tooling
- `cargo-nextest` adoption (`.config/nextest.toml`) — default-suite
  warm runtime drops from ~30 s to ~7 s. Test-groups cap concurrency
  on PVA listener / softIoc / tempfile-bound suites.
- Workspace clippy clean under `-D warnings`.

## v0.10.5 — 2026-04-28 — libca/RSRV deeper parity + kodex-driven review fixes

Continues the v0.10.4 line by closing the deeper libca/RSRV gaps surfaced
by kodex 0.9.0 analysis, then applying the actionable items from a
layer-by-layer code review across `pva-rs`, `ca-rs`, and `bridge-rs`.

### epics-ca-rs — libca/RSRV API parity

**Round 1 (commit `957d506`)**
- `SyncGroup` (`ca_sg_create`/`get`/`put`/`block` analog) — batch async
  ops with collective wait via `try_join_all`.
- Runtime address-list mutation: `CaClient::add_address(addr)` /
  `set_address_list(addrs)` (libca `addAddrToChannelAccessAddressList` /
  `configureChannelAccessAddressList`).
- `casr` iocsh command (RSRV `casr` analog) on `epics_ca_rs::server::iocsh`,
  reading from `Arc<ServerStats>` (connects/disconnects/uptime).
- `Channel::on_access_rights_change(cb)` callback wrapper.

**Round 2 (commit `007346e`)**
- `Channel::on_connection_change(cb)` — libca `ca_change_connection_event`
  analog. Filters Connected / Disconnected events from the broadcast.
- `Channel::host_name()` — libca `ca_host_name` analog returning the
  resolved server address.
- `Channel::receive_watchdog_delay() -> Duration` — libca
  `ca_receive_watchdog_delay`. New `CoordRequest::GetWatchdogDelay`
  variant; coordinator tracks per-server `last_rx_at` updated from every
  TransportEvent that implies an inbound frame.
- `CaClient::ioc_connection_count() -> usize` — libca
  `ca_get_ioc_connection_count`.
- Server-side ACF reload broadcast (RSRV `sendAllUpdateAS` analog):
  `CaServer.acf_reload_tx: broadcast::Sender<()>`; each accepted TCP
  client races read against reload notifications via `tokio::select!`,
  re-pushing `CA_PROTO_ACCESS_RIGHTS` for every open channel on signal.
  Both `reload_acf*()` and the introspection `POST /reload-acf` route
  fire it.

### Review-driven fixes

- **Dead code removal**: `epics-pva-rs/src/client_native/{ops.rs, conn.rs}`
  deleted (~750 LOC). The legacy one-shot `Connection`+`op_*` path was
  superseded by `ops_v2` (Channel-aware with auto-reconnect) months ago;
  only a stale doc comment in `channel.rs:19` referenced it.
- **bridge-rs group `field[N]` indexing semantic fix**: `qsrv::group::get_nested_field`
  changed return type from `Option<&PvField>` to `Option<Cow<PvField>>`.
  `field[N]` on a `ScalarArray` now returns the indexed element wrapped
  as `PvField::Scalar`; `field[N].child` on a `StructureArray` descends
  into the element and continues navigating. Previously both cases
  silently returned the whole array, breaking NTTable column[N] paths.
- **bridge-rs gateway PUT/GET channel reuse**: `UpstreamManager` now stores
  `UpstreamSubscription { channel: Arc<CaChannel>, task }` per upstream
  PV. Direct put/get reuse the subscribed channel instead of opening a
  fresh one — 3 round-trips → 1 RT per write/read.
- **CaChannel clone safety**: `CaChannel` was `Clone` but its `Drop` impl
  fired `CoordRequest::DropChannel` per drop, so cloning + dropping the
  clone tore down the original. Introduced a private `ChannelLifecycle`
  guard wrapped in `Arc`; `DropChannel` now fires exactly once when the
  last clone is dropped. Fixes a latent bug in `SyncGroup::get/put`
  where each scheduled future cloned the channel.
- **pvalink resolver sync fast path**: added `PvaLink::try_read_cached()`
  and `PvaLinkRegistry::try_get()` so the record-link `ExternalPvResolver`
  closure (and `LinkSet::get_value` / `is_connected`) hit a sync
  `parking_lot` cache without ever calling `block_on` when the monitor
  has already delivered. `block_on` is only paid on first-open / first-event.
- **pvalink scheme tightening**: `strip_scheme` no longer accepts `ca://`
  — pvalink handles PVA only; `ca://` belongs to the libca link scheme.

## v0.10.4 — 2026-04-28 — pvxs API parity (src + ioc + tools) and lset abstraction

A nine-commit pass closing every kodex-flagged gap relative to the
pvxs upstream. The `pva-rs` ↔ `pvxs/src` surface is now functionally
equivalent (modulo C++ STL idioms that don't translate to Rust);
`bridge-rs` ↔ `pvxs/ioc` covers QSRV + pvalink at iocsh + record-link
levels; new CLIs in `pva-rs/src/bin` mirror `pvxs/tools`.

### epics-pva-rs — pvxs API parity (3 rounds)

**Round 1 — lifecycle, multi-source, monitor handle, name servers**
- `PvaClient`: `close`, `hurry_up`, `cache_clear`,
  `ignore_server_guids`, per-call forced server (`pvget_from`,
  `pvput_to`), `name_servers` env wired (`EPICS_PVA_NAME_SERVERS`),
  multicast UDP join, `report` snapshot.
- `PvaServer`: `start` / `stop` / `wait` / `run` / `interrupt` (SIGINT
  trap), `client_config()`, `config()`, `report()`, `ignore_addrs`
  ACL, `monitor_*_watermark` diagnostics.
- `CompositeSource` multi-source registry with priority order.
- `pvmonitor_handle` returns `SubscriptionHandle` (pause/resume/
  stats/stop). `SubscriptionStat` metrics.
- Wire: `build_monitor_pause` / `build_monitor_resume` (subcmd
  0x04/0x44).

**Round 2 — SharedPV callbacks, builders, Discover**
- `SharedPV`: `on_first_connect`, `on_last_disconnect`, `on_put`,
  `on_rpc`, `attach`, `fetch`, `prune_subscribers`.
- `MonitorBuilder::server` → `pvmonitor_handle_from(pv, addr, cb)`.
- `ConnectBuilder`: `server()`, `sync_cancel(bool)`,
  `ConnectHandle::wait()`. `SubscriptionHandle::stop_sync` (pvxs
  syncCancel(true) analog).
- `PvaClientBuilder`: `priority(0..7)`, `tcp_timeout(Duration)`,
  `share_udp(bool)` (process-wide search engine via OnceCell),
  `MonitorEvent` / `MonitorEventMask` typed events.
- Discover: `Discovered::Online` carries `peer` + `proto`; beacon
  parser rewrites 0.0.0.0 → peer.ip(). `SearchEngine::ping_all()` /
  `PvaClient::ping_all()` (`DiscoverBuilder::pingAll`).
- `PvaServerConfig::isolated()` + `PvaServer::isolated()` factories.
- `PvRequestBuilder` (`field`/`record`/`pv_request`/`raw_request`/
  `build`) — pvxs RequestBuilder parity.

**Round 3 — TypeDef, Value coercion, ca-auth roles, log reload**
- `Value::clone_empty()` (pvxs `cloneEmpty` parity).
- `Value::copy_in` / `copy_out` / `try_copy_in` / `try_copy_out`
  (pvxs naming aliases).
- `TypeDef` + `Member` fluent builder for `FieldDesc` trees.
- `crate::log` module: `init_filter` (reload::Layer), `set_global_handle`,
  `set_log_filter(spec)`, `set_log_level(target, level)`. pvxs
  `logger_config_str` / `logger_level_set` parity.
- `auth::posix_groups()` POSIX `getgrouplist(2)` wrapper.
- ca-auth wire payload now advertises `groups: string[]`; server-side
  `ClientCredentials.roles` reads `groups`/`roles` either name.
- `ClientCredentials::peer_label(peer)` formatter.
- `PvaServerConfig::auth_complete` post-validation hook.
- `version_int()` const fn + `VERSION` const.

### epics-bridge-rs — QSRV + pvalink (3 rounds)

**Round 1 — iocsh integration**
- `dbLoadGroup` / `processGroups` / `qsrvStats` iocsh commands,
  bound to a shared `Arc<BridgeProvider>`. `BridgeProvider.groups`
  switched to `parking_lot::RwLock` for interior mutability.

**Round 2 — pvalink record-link wiring**
- `PvaLinkResolver` wraps the registry + tokio handle + read counter.
  `install_pvalink_resolver(db, handle)` registers both an
  `ExternalPvResolver` closure and (in round 3) the new
  `LinkSet`.
- `pvxr` / `pvxrefdiff` / `dbpvxr` iocsh commands.
- `wait_for_link_connected(pv, timeout)` (pvxs
  `testqsrvWaitForLinkConnected` analog).

**Round 3 — completeness pass**
- `resetGroups` iocsh + `BridgeProvider::reset_groups()`.
- `dbLoadGroup` macros arg now expands `${NAME}` against the
  `name=value,...` macros string with `std::env::var` fallback.
- `BridgeProvider::group_member` / `get_group_field` /
  `put_group_field` (pvxs `getGroupField`/`putGroupField`).
- `op_stats()` cumulative counters (channels created, GET, PUT,
  SUBSCRIBE) surfaced via `qsrvStats`.
- `PvaLinkConfig::scan_on_update`. `PvaLink::is_connected` /
  `alarm_message` / `time_stamp` / `latest_value` (pvxs lset
  helpers).
- `PvaLinkResolver::set_enabled` / `is_enabled`. `pvalink_enable`
  / `pvalink_disable` iocsh.

### epics-base-rs — LinkSet abstraction

- New `LinkSet` trait + `LinkSetRegistry` keyed on URL scheme.
- `PvDatabase::register_link_set(scheme, lset)` /
  `link_set(scheme)` / `registered_link_schemes()`.
- `resolve_external_pv` dispatches through the lset registry first,
  falls back to the legacy `ExternalPvResolver`. Backward-compat.
- `PvDatabase::record_link_fields(name)` enumerates link-shaped
  String fields by parsing each value through `parse_link_v2` —
  underpins per-record `dbpvxr`.
- `dbpvxr <record>` is now a real per-record dump: connected /
  value / alarm / timeStamp for each `pva://` link, plus single-line
  descriptions of `ca://` / db / constant links.

### epics-pva-rs/src/bin — pvxs/tools parity

- `pvcall-rs`: RPC client. `field=value` → NTURI request → `pvrpc`.
- `pvlist-rs`: server discovery via `SearchEngine::discover` +
  optional `ping_all`. Verbose mode adds GUID / proto / peer.
- `pvxvct-rs`: PVA Virtual Cable Tester. Decodes SEARCH / BEACON
  frames. `-C` / `-S` direction filter, `-H` host filter.
- `mshim-rs`: beacon / search multicast shim. `-L listen` /
  `-F forward` endpoints, auto multicast join, same-peer feedback
  guard. Bug found via `testudpfwd` integration test: send_sock
  needs `set_nonblocking(true)` for tokio::UdpSocket::from_std.

### Tests — pvxs test/* parity (final round)

- 9 `testTypeDef` parity unit tests (TypeDef + Member builder).
- 6 `testqsingle` parity tests (BridgeChannel get/put e2e: ai /
  longin / stringin / waveform).
- 4 `testqgroup` parity tests (atomic + non-atomic group put-then-get,
  config parse + finalize).
- 2 `testudpfwd` integration tests (mshim-rs binary forwarding,
  invalid-endpoint exit code).
- 7 inline `parse_endpoint` unit tests in mshim-rs.

### Workspace bump

- `0.10.3` → `0.10.4` across workspace + dependency pins.

## v0.10.3 — 2026-04-28 — pva-rs reconnect machinery brought up to pvxs parity

A focused pass on `epics-pva-rs` client-side reconnect: the v0.10.2
review identified the gaps relative to pvxs (`pva-rs reconnect gaps vs
pvxs` in kodex). All five items closed in this release. The protocol
behaviour now mirrors pvxs `client.cpp` and `clientconn.cpp` — same
constants, same trigger conditions — without the libca-only extras
(penalty box, circuit breaker, multi-lane retry).

### Cooperative search-bucket ring

- Replaces the per-channel `BACKOFF_SECS` exponential schedule with a
  30-bucket ring rotated at 1 s. Each tick processes exactly one
  bucket so steady-state UDP search load is `O(pending / 30)` packets
  per second instead of `O(pending)`. Mirrors pvxs `client.cpp`
  `searchBuckets[nBuckets=30]`.
- New searches land in `(current + 1) % 30`; first retry rotates
  back to the same slot 30 ticks later; subsequent retries shift by
  an extra `RETRY_HOLDOFF_BUCKETS = 10` (matches pvxs
  `Channel::disconnect` holdoff at client.cpp:155-163).

### `poke()` — fast-tick mode on fresh server identity

- When a beacon delivers a new (server, GUID) pair (server restart or
  brand-new server), the search engine flips its tick interval to
  200 ms for one full revolution (≈ 6 s) so every pending search
  retries quickly. Reverts to 1 s afterwards. Mirrors pvxs
  `ContextImpl::poke()` (client.cpp:713) with the 30 s pokeHoldoff
  enforced by the same `first_announce` gate that drives the
  `Discovered` events.
- Periodic same-GUID beacons no longer pull pending searches'
  `last_attempt` forward (closes a regression: every 15 s beacon
  effectively reset every backoff regardless of whether anything
  changed).

### Beacon-timeout pruning + `Discovered::Timeout`

- `BeaconTracker::prune_stale(max_age)` walks the throttle map and
  evicts entries whose `last_seen` is older than `max_age`,
  returning the (server, guid) tuples that were dropped.
- The search engine now runs a `BEACON_CLEAN_INTERVAL = 180 s` tick
  that calls `prune_stale(BEACON_TIMEOUT = 360 s)` and emits
  `Discovered::Timeout` for each pruned server. Application
  observers subscribed via `SearchEngine::discover()` see online /
  timeout transitions both ways. Mirrors pvxs `tickBeaconClean`
  (client.cpp:1254).
- New `Discovered::Timeout { server, guid }` variant on the public
  enum.

### Connect-fail holdoff per channel

- `Channel` gained `holdoff_until: Option<Instant>` and
  `connect_fail_count: AtomicU32`. `ensure_active` now sleeps until
  `holdoff_until` before issuing a fresh search; on every full
  candidate-list failure the counter increments and the holdoff is
  set to `2^min(fails-1, 4)` seconds (1 s → 16 s cap). On the next
  successful Active transition both fields reset. Mirrors pvxs
  `Channel::disconnect` 10-bucket future-push, generalised so
  callers that retry tightly (e.g. monitor reconnect loops) don't
  spin against a dead server.

### Tests

- `beacon_throttle::tests::prune_stale_returns_aged_out_entries`
  covers the new prune API.
- All existing 128 lib tests + 118 parity tests + 4 stability tests
  pass against the new tick / bucket logic.

### What was deliberately NOT added

Per the v0.10.2 kodex `tech_debt` entry: pvxs deliberately omits the
libca penalty box, circuit breaker, and multi-lane retry buckets. We
match that decision — those features remain CA-only (in `epics-ca-rs`)
and would re-introduce complexity that pvxs ships without for a
reason. If a real flapping-server incident in production calls for
them later, the data point goes through CA-rs first.

## v0.10.2 — 2026-04-28 — kodex-driven cross-crate review pass

A self-review using the kodex knowledge graph as a baseline (so we
didn't re-examine the v0.10.1 wire-format fixes) plus three parallel
agent reviews surfaced a set of polish + correctness items that
weren't worth blocking v0.10.1 on but accumulate now into a clean
release.

### ca-rs (client TLS)

- `CaClientConfig::tls_server_name: Option<String>` (env
  `EPICS_CA_TLS_SERVER_NAME`) overrides the SNI / cert-hostname-
  verification name when wrapping a TCP virtual circuit in rustls.
  Without it, SNI fell back to the server's IP literal — which only
  validates against IP-bound certs. The override unblocks
  hostname-bound rustls cert verification for hostname-bound deployments.

### ca-rs (server cap-token verification)

- `CaServerBuilder::with_cap_token_verifier(verifier)` (feature
  `cap-tokens`) installs a `TokenVerifier` on the listener. CLIENT_NAME
  payloads beginning with `cap:` now flow through the verifier; the
  resolved `sub` claim becomes the ACF username. Verification failure
  yields an `unverified:<raw>` sentinel that ACF can deliberately deny.
  Plain (non-`cap:`) usernames pass through unchanged for legacy compat.
  Earlier code stored the raw payload as the username regardless of
  prefix — closing this loophole was the original intent of cap-tokens
  but the wiring was missing.

### ca-rs (signed beacon mixed mode)

- `EPICS_CA_BEACON_REQUIRE_SIGNED=NO` opts the verifier into a soft
  mode where unsigned beacons are accepted (with a counter increment)
  alongside signed ones. Lets operators run mixed deployments while
  servers roll out signing instead of forcing a flag day. Default
  remains strict.

### pva-rs (server identity + beacons)

- `ClientCredentials` parsed from CONNECTION_VALIDATION reply (method,
  account, host) and logged at handshake. Mirrors pvxs serverconn.cpp
  `server::ClientCredentials` at the wire-parse level. Available for
  future per-op authorisation hooks; today's use is `tracing` audit.
- Beacon `change_count` (u16) now increments whenever the source's
  `list_pvs()` set churns between ticks (compared via stable hash of
  the sorted name list). Sequence (u8) was already incrementing;
  together they let clients re-issue searches on PV-set churn even
  when the beacon stream is otherwise in lock-step (pvxs
  `server.cpp::doBeacons`).

### bridge-rs (live ACF reload)

- `BridgeProvider` now stores access policy in
  `Arc<parking_lot::RwLock<Arc<dyn AccessControl>>>` and vends an
  `Arc<LiveAccessProxy>` to each `AccessContext`. `set_access_control`
  swaps the inner Arc and is picked up by every existing channel on
  its next can_read / can_write call — matches C++ QSRV "ACF reload
  takes effect without recreating channels". The earlier direct-clone
  pattern pinned each channel to the policy at creation time.
- `BridgeProvider::live_access()` is a public helper for downstream
  code that constructs its own AccessContexts.

### bridge-rs (rename + lifecycle test)

- `qsrv::spvirit_adapter` → `qsrv::pva_adapter` (the file's own header
  comment already noted "no spvirit_* types appear in this module" —
  the name was the last remaining `spvirit_*` artifact in the
  workspace).
- `qsrv::monitor::tests::monitor_stop_releases_subscription` —
  start → poll → stop → idempotent stop → re-subscribe round-trip
  against a fresh BridgeMonitor on the same record. Locks in Drop
  semantics so a future refactor can't silently leak DbSubscription
  senders.

### Tests

- `qsrv::provider::tests::live_access_proxy_observes_policy_swap` —
  AccessContext bound to `live_access()` observes
  `set_access_control` mid-flight (regression for the cached-Arc
  pattern this release replaces).
- `server_native::udp::tests::beacon_payload_carries_sequence_and_change_count`
  — beacon byte layout regression: sequence at offset 13, change_count
  little-endian at offsets 14-15.
- `client::tls_sni_config_tests::tls_server_name_round_trip` —
  `CaClientConfig::tls_server_name` default + assignment.

### Build / publish hygiene

- `epics-base-rs` dropped the `experimental-rust-tls` feature
  passthrough into a dev-dep. `cargo publish` strips dev-deps before
  parsing features, so the passthrough broke `cargo workspaces
  publish`. Nothing outside the crate referenced it.

## v0.10.1 — 2026-04-28 — pvxs / pvAccessCPP wire-format interop

`epics-pva-rs` server and client are now byte-exact compatible with
the upstream EPICS C++ implementations (`pvxs` 1.x and `pvAccessCPP`
shipping with EPICS Base 7.x). The push came from a real-world
deployment where Base's `pvmonitor` either disconnected immediately
or printed garbled values against our server. A walk through pvxs
`servermon.cpp` / `serverget.cpp` / `serverchan.cpp` /
`clientget.cpp` exposed five separate wire-format mismatches. All
are fixed; e2e and unit tests cover each one.

### Wire-format fixes (server)

- **MONITOR data field order** — the payload is now `changed bitset →
  partial value → overrun bitset`, matching pvxs `servermon.cpp:173-
  175`. The previous order (`changed → overrun → value`) shifted the
  client's value-decode cursor by one byte whenever overrun was
  empty, corrupting timestamps and double values for every Base
  client.
- **MONITOR FINISH** — when the source's broadcast channel is
  dropped the subscriber task now emits `subcmd 0x10 + Status::OK`
  before exiting (pvxs `servermon.cpp:148`). Clients receive a
  graceful end-of-stream instead of waiting for a TCP timeout.
- **INIT type-descriptor encoding** — RPC INIT no longer emits the
  type descriptor (pvxs `serverget.cpp:97` —
  `if (cmd != CMD_RPC) to_wire(R, type)`). For
  GET/PUT/MONITOR it defaults to inline; the
  `0xFD` / `0xFE` cache markers are now opt-in via
  `PvaServerConfig::emit_type_cache` because pvAccessCPP doesn't
  parse them and reads past the payload boundary, breaking the next
  frame.
- **PUT_GET (`subcmd & 0x40`)** — the response now carries
  `bitset + partial value` after the status (pvxs
  `serverget.cpp:103-104`). Previously only `Status::OK` was sent and
  the client got no readback.
- **CreateChannel access_rights** — drop the unnecessary `u16`
  trailing the status. pvxs `serverchan.cpp:349-351` emits
  `cid + sid + status` only.
- **RPC DATA request** — server now decodes `type(arg) +
  full_value(arg)` instead of treating the channel introspection
  from INIT as the argument shape (pvxs `serverget.cpp:444-446`,
  `from_wire_type_value`).
- **MESSAGE / CancelRequest** — now dispatched. MESSAGE (cmd 18) is
  surfaced through `tracing` at the matching severity. CancelRequest
  (cmd 21) aborts the in-flight monitor task and resets
  `monitor_started` so a re-START respawns cleanly. Both previously
  fell through to a no-op catchall.
- **`request_to_mask`** — an empty pvRequest with no `field`
  substructure now selects every field (pvxs convention) instead of
  "root only". This was silently dropping every leaf for the
  canonical no-filter sentinel `[0xFD,0x02,0x00,0x80,0x00,0x00]` the
  Rust client sends by default.

### Wire-format fixes (client)

- **RPC INIT response** — the type descriptor is no longer expected
  (mirrors the server fix above).
- **RPC DATA response** — decoded as `status + type + full_value`
  (pvxs `clientget.cpp:415-421`). The previous bitset-driven path
  could not parse RPC replies at all.
- **RPC DATA request** — sends `type(arg) + full_value(arg)`. v1
  (`ops.rs`) and v2 (`ops_v2.rs`) paths both updated.
- **Monitor data overrun bitset** — the trailing `BitSet` is now
  consumed.
- **`OpResponse::Status` from `subcmd & 0x10`** — the FINISH frame
  is routed to the status path so `pvmonitor` returns `Ok(())`
  instead of hanging.
- **`OpDataResponse.response_desc: Option<FieldDesc>`** — new field
  carrying the server-side response type for RPC, so callers can
  reconstruct the result without relying on the now-empty INIT
  introspection.

### pvData decode tightening

- **Structure-array presence byte** — strict `0x00` (null) / `0x01`
  (present) per pvxs `dataencode.cpp:359-361`. The previous code
  also accepted `0xFF` and silently rewound the cursor on unknown
  markers; both branches were defensive guesses with no pvxs
  counterpart and could mask real protocol errors.
- **Union / UnionArray selector** — the manual peek-and-pushback was
  replaced by a direct `decode_size` match. `decode_size` already
  returns `None` for the 0xFF null marker, so the rewind dance was
  redundant and made the future "selector ≥ 254" extended-Size case
  awkward to handle.

### Server lifecycle / resource management

- **Spawned MONITOR subscribers are now cancelled deterministically**
  on DestroyRequest, DestroyChannel, CancelRequest, and connection
  end. The abort handle is wrapped in `Arc<AbortOnDrop>` and stashed
  on `OpState`; dropping the OpState (via HashMap removal or
  HashMap drop on connection teardown) fires the abort
  automatically. Previously, orphaned monitor tasks ran until their
  next write tripped on a closed socket — keeping the source's
  broadcast subscription alive in the meantime.
- **Per-connection write queue** — replaced
  `Arc<Mutex<SrvWrite>>` with a bounded `mpsc::channel` plus a
  single dedicated writer task. Producers (main read loop,
  heartbeat, monitor subscribers) `tx.send(buf).await` instead of
  serialising on the writer Mutex across `write_all().await`.
  A slow client now backpressures monitor delivery rather than
  blocking the heartbeat or other channels' writes.
  Configurable via `PvaServerConfig::write_queue_depth` (default
  1024). Writer-task I/O failures are logged at `debug!` with
  peer info before the connection tears down.

### pvRequest field filtering

- The pvRequest sent at INIT time is now translated through
  `request_to_mask` and stored on the OpState. GET and MONITOR
  emission consult the mask via `encode_pv_field_with_bitset`, so
  the server only ships the fields the client asked for. Previously
  the request was decoded and discarded; the wire always carried
  every field.

### Tests

- `parity/test_pvrequest_filter.rs` — e2e: empty pvRequest returns
  every field; `pvget_fields(["value"])` omits alarm/timeStamp on the
  wire.
- `parity/test_monitor_finish.rs` — e2e: dropping the source's
  broadcast sender mid-stream causes the client `pvmonitor` to
  return `Ok(())` via the FINISH frame.
- `server_native::tcp::tests` — synthetic-frame unit coverage for
  `handle_message` (every severity, truncated payload guard) and
  `handle_cancel_request` (abort guard fires, `monitor_started`
  resets).
- `server_native::tcp::tests::monitor_payload_orders_overrun_after_value`
  — round-trip regression for the corrected MONITOR layout.

### Configuration additions

- `PvaServerConfig::emit_type_cache: bool` (default `false`) — opt
  in to `0xFD`/`0xFE` type-cache markers in INIT and RPC responses.
- `PvaServerConfig::write_queue_depth: usize` (default `1024`) —
  bounded write queue capacity per connection.

## v0.9.4 — 2026-04-16

### Async / reliable plugin data path

- **`asyn-rs`, `ad-core-rs`** — plugin pipeline on a fully async data
  path with bounded backpressure for parameter updates and array
  propagation.
- **`ad-core-rs`** — driver-facing async runtime facade (`rt::spawn`,
  `rt::timeout`, `rt::CommandReceiver`, …) so drivers no longer depend
  on `tokio` directly. All example acquisition tasks migrated.

### Scan scheduler

- Dedupe entries on registration — a record can no longer be scanned
  twice after rate changes.
- Preserve PINI → init-hook ordering across the dual schedulers.

### mqtt-rs

- Connected PV no longer latches at 0 after a recoverable `rumqttc`
  state error. Connected=1 is now also restored on any inbound
  `Publish` or `PingResp`, not just `ConnAck`.
- `mqtt-ioc` installs `tracing_subscriber` (EnvFilter, default `info`,
  `RUST_LOG`-controlled) so MQTT connection errors and reconnects
  reach stdout.

## v0.9.3 — 2026-04-15 — First production-ready pvAccess support

`epics-rs` now ships a full pvAccess (PVA) stack — client, server,
and QSRV-equivalent bridge — powered by
[spvirit](https://github.com/ISISNeutronMuon/spvirit). PVA was
introduced experimentally in v0.9.2; v0.9.3 is the release where it
leaves experimental status and becomes a first-class peer to Channel
Access across the entire workspace.

### What spvirit provides

[spvirit](https://github.com/ISISNeutronMuon/spvirit) is a pure-Rust
implementation of the pvAccess wire protocol maintained by the ISIS
Neutron & Muon Source. `epics-pva-rs` wraps `spvirit-server` /
`spvirit-client` / `spvirit-codec` / `spvirit-types` (v0.1.9 from
crates.io) and exposes:

- **Client** — `search`, `get`, `put`, `monitor`, `info` over UDP
  discovery (port 5076) + TCP virtual circuits (port 5075)
- **Server** — `PvaServer` that hosts a `PvDatabase` and answers the
  full pvAccess command set
- **NormativeTypes** — NTScalar, NTEnum, NTScalarArray, NTNDArray,
  NTTable
- **BitSet-delta monitors**, segmentation, `SET_BYTE_ORDER`
  handshake, and connection validation

### epics-bridge-rs — QSRV-equivalent

Pure-Rust analogue of the C++ QSRV (`modules/pva2pva/pdbApp/`):
translates `epics-base-rs` record state into pvAccess `PvStructure`
values and vice versa.

- **Single-record channels** — NTScalar, NTEnum (with choices),
  NTScalarArray with full `alarm / timeStamp / display / control /
  valueAlarm` metadata
- **Group PV channels** — composite structures defined via
  `info(Q:group, …)` JSON tags on records (C++ QSRV JSON format
  compatible)
- **Monitor bridge** — initial Snapshot on connect, full Snapshot on
  every update, fan-in group monitor with trigger rules
- **pvRequest** — field selection, `record._options.process` / `block`
- **Pluggable access control** — ChannelProvider / Channel /
  PvaMonitor traits, record metadata cache

### Dual-protocol across every example

All seven remaining example IOCs now serve CA **and** PVA
simultaneously from the same `PvDatabase` via
`epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc`:

| Example           | Protocols                   |
|-------------------|-----------------------------|
| `mini-beamline`   | CA + PVA                    |
| `xrt-beamline`    | CA + PVA                    |
| `qsrv-ioc`        | CA + PVA                    |
| `sim-detector`    | CA + PVA                    |
| `ophyd-test-ioc`  | CA + PVA *(new in 0.9.3)*   |
| `scope-ioc`       | CA + PVA *(new in 0.9.3)*   |
| `mqtt-ioc`        | CA + PVA *(new in 0.9.3)*   |

The programmatic `random-signals` demo was removed in favour of a
uniform st.cmd-driven example set.

### PVA CLI tools

Shipped alongside the CA tools (`caget-rs`, `caput-rs`,
`camonitor-rs`, `cainfo-rs`, `ca-repeater-rs`):

- `pvget-rs` — read
- `pvput-rs` — write
- `pvmonitor-rs` — subscribe
- `pvinfo-rs` — type / introspection info

### Documentation

- "Experimental" status removed from `epics-pva-rs`,
  `epics-bridge-rs`, and the pvAccess CLI tool section in the
  top-level and per-crate READMEs.
- `epics-pva-rs` README refreshed — stale "server-side is planned"
  notes replaced with a working `PvaServer` +
  `run_ca_pva_qsrv_ioc` example.

### Acknowledgements

Huge thanks to the [spvirit](https://github.com/ISISNeutronMuon/spvirit)
maintainers at ISIS Neutron & Muon Source for the pvAccess wire-protocol
implementation that makes this release possible.

## v0.9.2 — 2026-04-16

### pvAccess / QSRV

- **pvAccess protocol support** — full client & server via [spvirit](https://crates.io/crates/spvirit-server) integration
- **QSRV bridge** — map EPICS records to PVA NormativeTypes (NTScalar, NTEnum, NTNDArray) via `info(Q:group)` JSON configuration
- **NDPluginPva** — serve AreaDetector NDArray as NTNDArray over pvAccess, compatible with C++ `pvget -m`
- **Dual-protocol CA+PVA runner** — `run_ca_pva_qsrv_ioc()` for all example IOCs
- **PVA CLI tools** — `pvget-rs`, `pvmonitor-rs`, `pvput-rs`, `pvinfo-rs` (renamed from `pvaget-rs` etc.)
- **spvirit 0.1.9** from crates.io (removed `[patch.crates-io]` path overrides)

### xrt-beamline example

- **Real-time ray tracing simulation** — Undulator → DCM Si(111) → HFM → VFM → Sample at 8 keV
- 25 motors driving [xrt-rs](https://github.com/physwkim/xrt-rs) ray tracing with AreaDetector output
- Accumulation over `AcquireTime` for improved statistics
- PyDM viewer with contrast control, xrtGlow 3D viewer with pyepics PV monitoring
- Coddington-calculated mirror radii (HFM R=3.27 km, VFM R=1.82 km)

### xrt-rs fixes (companion repo)

- **position_roll**: implement as roll addition matching xrt Python behavior
- **bracketing**: increase t_min clamp from -1e-6 to -100 mm for large pitch angles (DCM at 14°)
- **reflect()**: use `state==1` filter to prevent Over ray reprocessing

### Other

- Upgrade spvirit dependencies 0.1.8 → 0.1.9
- Fix clippy warnings across workspace

## v0.9.1 — 2026-04-13

### motor-rs

- **Fix RBV monitor updates during motion**: `process()` was returning
  `AsyncPendingNotify` on every poll cycle with only DMOV/VAL/DVAL/RVAL
  fields — RBV and DRBV were missing. Now uses `AsyncPendingNotify` only
  for the initial DMOV 1→0 transition; subsequent polls return `Complete`
  which posts monitors for all changed fields including RBV.
- **Fix missing DMOV monitor on back-to-back motions**: When a new put
  arrives while the previous motion's done status is consumed in the same
  process cycle, `dmov_notified` was not reset. Fixed by resetting the
  flag in `plan_motion()`.
- **Fix same-direction NTM retarget**: `ExtendMove` accepted the new
  DVAL but never re-dispatched a `MoveAbsolute` to the driver. On
  completion, `evaluate_position_error()` only retried under retry
  conditions (RTRY>0, RDBD>0). Now sets `verify_retarget_on_completion`
  so the completion path replans if DVAL ≠ DRBV regardless of retry
  settings.

### epics-ca-rs

- **CA repeater**: Rewrite to use per-client connected UDP sockets
  matching C EPICS architecture. Fixes compatibility with C CA clients
  (camonitor, caget) that could not register with the Rust repeater.
- **Pre-connection subscription**: `subscribe()` now registers
  subscriptions even when disconnected. On connect, the coordinator
  fills in native type and element count and issues `CA_PROTO_EVENT_ADD`.
  Eliminates the need for application-level resubscribe loops.
- **Add `get_with_timeout()`** for explicit timeout control on reads.
- **Monitor flow control**: Client-side backlog tracking replaces TCP
  read count heuristic. Server-side `FlowControlGate` with
  `coalesce_while_paused()` matching C EPICS `dbEvent.c` behavior.
- **Add `ioc` feature** to umbrella crate for IOC builds.
- **Fix proc macro path resolution**: `epics_main`/`epics_test` now
  resolve `epics_base_rs` path for umbrella crate users via
  `proc-macro-crate`.

### CA tools (C parity)

- **camonitor-rs**: Use server timestamp, print disconnect to stdout
  as `*** disconnected`, add `-w` initial connection timeout. Subscribe
  once and rely on library auto-restore (no resubscribe loop).
- **caput-rs**: Re-read value from server for `New` line. Apply `-w`
  timeout to all reads. Fix `-c` description.
- **caget-rs**: Parallel PV connect+read via `tokio::spawn`. Add `-w`
  timeout. Distinguish "Not connected" from "timeout" errors.
- **cainfo-rs**: Add `-w` timeout, use explicit channel connect.
- All tools: Rename help text from `rcaXXX` to `caXXX`.

## v0.9.0

### motor-rs — Complete C parity (~95 fixes across 12 review rounds)

#### State machine
- Fix MSTA bit positions for wire compatibility with C clients
- Fix all 4 retry modes (Default/Arithmetic/Geometric/InPosition)
- Fix SPMG Pause/Stop/Go transitions to match C postProcess pipeline
- Add MIP_EXTERNAL detection for externally-initiated motion
- Add clear_buttons on limit switch hit or PROBLEM
- Add stop-first pattern for home-while-moving and jog-while-moving
- Add DLY → DELAY_ACK → fresh poll → retry evaluation flow
- Add limit switch direction guard before retries (user_cdir)
- Implement two-phase jog backlash (BL1 slew + BL2 backlash velocity)
- Add sub-step deadband check with DMOV pulse for ophyd compatibility

#### Coordinate system
- Fix CDIR to account for MRES sign
- Fix DIR handler FOFF branching (Variable preserves VAL)
- Fix SET+FOFF=Frozen cascade for VAL/DVAL/RVAL
- Fix FOFF=Frozen in non-SET mode (no effect, matches C)
- Fix RDIF type (i32) and formula (NINT(diff/mres))
- Fix LVIO escape logic using ldvl, pretarget only for non-preferred direction
- Fix soft limit disable only when dhlm==dllm==0
- Add RHLM/RLLM fields for MRES cascade invariance

#### New features
- Add MoveRelative command and use_rel logic (ueip/urip)
- Add FRAC progressive approach scaling
- Add dual poll rate (moving/idle intervals, forced fast polls)
- Add auto power on/off with configurable delays
- Add deferred moves and profile moves framework
- Add RDBL/URIP readback link support
- Add velocity cross-calculation and range validation

#### Driver interface
- Expand MotorStatus with direction, slip_stall, comms_error, homed, gain_support, has_encoder, velocity
- Add move_velocity, move_relative, set_deferred_moves trait methods
- Add profile move trait methods (initialize, define, build, execute, abort, readback)
- Fix SetPosition to send dial coordinates (not raw steps)
- Fix MOVN ls_active to use raw limit switches before user mapping

### asyn-rs
- Fix race condition in PortManager register/unregister
- Fix COMM_ALARM constant, HTTP connect-per-transaction
- Fix write retry timeout, HTTP write reconnect, EOS storage
- Fix param defined tracking, IP port auto-disconnect
- Fix trace masks, serial flush, baud rates, break/ixany
- Fix asyn_record connect_device clearing drv_user_create error
- Add PortHandle convenience methods for new operations
- Add `set_params_and_notify` for atomic background thread parameter updates
- Add ParamSetValue::Float64Array for waveform parameter updates
- Add AsynMotor::move_relative, set_deferred_moves, profile move methods
- Move set_rs485_option out of PortDriver trait impl
- Document `set_params_and_notify` vs `write_int32_no_wait` for driver authors

### epics-base-rs
- Fix ai/ao conversion pipeline (ASLO/AOFF/ESLO/EOFF)
- Fix bi/bo records and COS alarm
- Fix calc division by zero to return NaN
- Fix mbbi/mbbo state handling and field access
- Fix sel record High/Low/Median algorithms
- Fix calcout missing OUT link write (pval timing + cached should_output)
- Fix WriteDbLink to use resolve_field for common fields (OUT/DOL)
- Fix monitor deadband for binary records (bi/bo/busy/mbbi/mbbo always post)
- Document DeviceReadOutcome ok() vs computed() convention

### ad-core-rs
- Fix ADDriverBase MaxSizeX/Y init from constructor args
- Fix NDArrayPool threshold and free-list logic
- Fix plugin runtime interrupt notifications
- Add ParamUpdate::Float64Array for waveform param updates in plugins

### ad-plugins-rs
- Fix ROIStat time series waveform readback (was accumulating but never writing to params)
- Fix ROI, Stats, Process, HDF5, TIFF, JPEG, NetCDF, Nexus plugins
- Add attr_plot param indices and buffer output infrastructure

### examples
- Migrate all acquisition tasks to set_params_and_notify
- Fix beam_current and time_of_day DeviceReadOutcome to skip ai conversion
- Fix moving_dot acquire_busy and status in writeInt32

## v0.8.3

### asyn-rs

- Remove unbounded sync channel from `InterruptManager`, replacing it with a simpler notification mechanism to eliminate memory leaks when interrupt callbacks accumulate faster than consumed.

### motor-rs

- Fix tight poll loop consuming excessive CPU when motor is in motion.
- Defer `StartPolling` to `after_init` hook to prevent premature polling during st.cmd and autosave restore.
- Throttle `StartPolling` and send only on idle-to-active transition, removing redundant poll requests.
- Clear `last_write` in init to prevent restore-triggered moves.
- Sync driver position from pass0-restored VAL during initialization.

### epics-base-rs

- Add `after_init` hooks that run after PINI processing, matching C EPICS `initHookAfterIocRun` timing.

### epics-ca-rs

#### Client

- **Fix**: Slow reconnection after IOC restart (~50s → ~5s). Beacon monitor was skipping `available=INADDR_ANY` beacons (all modern IOCs), reading the wrong header field for server port, and doing per-server rescan instead of global rescan.
- **Fix**: ECHO ping-pong loop causing 50%+ CPU usage. Client was echoing back the server's echo responses, creating a tight infinite loop after the first 30-second idle timeout.
- **Fix**: Search response `INADDR_ANY` check (`0xFFFFFFFF` → `0`) for C server interoperability.
- **Fix**: `handle_disconnect` operator precedence bug causing channels on unrelated servers to be incorrectly disconnected.
- **Fix**: Pending read/write waiters now receive `CaError::Disconnected` on server disconnect instead of hanging forever.
- **Fix**: `DropChannel` now properly cleans up all channel states (Connecting, Disconnected, Unresponsive).
- Beacon-TCP watchdog integration: immediate echo probe on beacon anomaly detects dead connections in ~5s instead of ~35s.
- Send buffer backpressure: close stalled connections at 4096 pending frames.
- Search datagram sequence validation to reject stale responses from previous rounds.
- TCP read buffer capped at 1MB to protect against malformed servers.
- Defensive bounds checks and malformed message logging.
- `align8` overflow protection with `saturating_add`.

#### Server

- **Fix**: Beacon header field swap (`data_type`/`count` were swapped), breaking C client interop.
- **Fix**: Search response `INADDR_ANY` sentinel (`0xFFFFFFFF` → `0`), matching C protocol.
- **Fix**: `WRITE_NOTIFY` response `count` field was hardcoded to 1 instead of echoing the request count.
- **Fix**: `CLEAR_CHANNEL` response was missing `data_type` and `count` fields.

#### Repeater

- **Fix**: Accept zero-length UDP registration for C client backward compatibility (pre-3.12 protocol).
- **Fix**: Fill in beacon `available` field with source IP on relay, matching C repeater behavior.

### optics-rs

- Add HSC and QXBPM async driver support with deferred poll start.

## v0.8.2

### epics-bridge-rs (new crate)

New umbrella crate for EPICS protocol bridges. Hosts feature-gated sub-modules:

- **`qsrv`** (default) — Record ↔ pvAccess channels (C++ EPICS QSRV equivalent). Single PVs (NTScalar/NTEnum/NTScalarArray) and multi-record group PVs with full metadata, pvRequest filtering, process/block put options, AccessControl enforcement on get/put/monitor, nested field paths, info(Q:group, ...) parsing, and trigger validation.
- **`ca-gateway`** (default) — CA fan-out gateway (C++ ca-gateway equivalent). Includes `.pvlist` parser with regex backreferences, ACF integration, lazy on-demand resolution via search hook, per-host connection tracking, statistics PVs, beacon throttle, putlog, runtime command interface, and an auto-restart supervisor.
- **`pvalink`**, **`pva-gateway`** — placeholders for future implementations.

The `ca-gateway-rs` daemon binary builds via `cargo build --release -p epics-bridge-rs --bin ca-gateway-rs` and lands in `target/release/ca-gateway-rs`.

The umbrella `epics-rs` crate gains a `bridge` feature that re-exports `epics-bridge-rs` as `epics_rs::bridge`.

### epics-base-rs

#### **Behavior change**: `PvDatabase::has_name()` / `find_entry()` now invoke an optional async search resolver on miss

`PvDatabase` gained `set_search_resolver(SearchResolver)` / `clear_search_resolver()` plus a new `SearchResolver` type alias. When set, both `has_name()` and `find_entry()` invoke the resolver on a database miss; the resolver may populate the database (e.g. by subscribing to an upstream IOC) and return `true` to make the lookup succeed on the immediate re-check.

**Compatibility**: with no resolver installed (the default), behavior is unchanged. However, callers that previously assumed `has_name()`/`find_entry()` were *cheap, side-effect-free* lookups should be aware these methods can now `.await` arbitrary work when a resolver is registered. The current in-tree usage (CA UDP search responder, TCP create-channel handler) is consistent with this design.

This hook is what enables `epics-bridge-rs::ca_gateway` to lazily subscribe upstream PVs on first downstream search instead of requiring a `--preload` file.

#### `Snapshot` / `DisplayInfo` — additive fields

- `DisplayInfo` gained `form: i16` (display format hint, from `Q:form` info tag) and `description: String` (DESC). Existing initializers need `..Default::default()` to remain forward-compatible — internal call sites have been updated.
- `Snapshot` gained `user_tag: i32` (from `Q:time:tag` nsec LSB splitting). Defaults to 0.

These fields propagate into PVA NTScalar `display.form` / `display.description` and `timeStamp.userTag` via `epics-bridge-rs::qsrv::pvif`.

### epics-ca-rs

#### **Breaking**: `tcp::run_tcp_listener()` signature changed

Added a 6th parameter:

```rust
pub async fn run_tcp_listener(
    db: Arc<PvDatabase>,
    port: u16,
    acf: Arc<Option<AccessSecurityConfig>>,
    tcp_port_tx: tokio::sync::oneshot::Sender<u16>,
    beacon_reset: Arc<tokio::sync::Notify>,
    conn_events: Option<broadcast::Sender<ServerConnectionEvent>>, // ← new
) -> CaResult<()>;
```

External callers of `run_tcp_listener()` must pass `None` (opt out of connection lifecycle events) or a `broadcast::Sender` to subscribe.

In-workspace consumers (`server::ca_server::CaServer::run` and `crates/epics-base-rs/tests/client_server.rs`) have been updated.

#### Additive: `CaServer::connection_events()` and `ServerConnectionEvent`

`CaServer` now exposes `connection_events()` which returns a `broadcast::Receiver<ServerConnectionEvent>` (`Connected(SocketAddr)` / `Disconnected(SocketAddr)`). Used by `epics-bridge-rs::ca_gateway` for per-host downstream client tracking. Servers that don't subscribe see no behavior change.

## v0.8.1

### Fix: Plugin param update re-entrancy (CPU 100% on idle)

Plugin `on_param_change` handlers that return `ParamUpdate` values (readback pushes)
previously used `write_int32_no_wait` which sends `Int32Write` to the port actor.
The port actor then calls `io_write_int32` → `on_param_change` again, causing
**infinite re-entrancy loops** (e.g., Overlay Position↔Center bidirectional update).

This is now fixed by introducing `ParamSetValue` and `set_params_and_notify()`,
which mirrors C ADCore's `setIntegerParam()` + `callParamCallbacks()` pattern:
values are stored directly in the param store without going through the driver's
write path, so `on_param_change` is never re-triggered.

- **asyn-rs**: Add `ParamSetValue` enum, extend `CallParamCallbacks` with inline param updates, add `PortHandle::set_params_and_notify()`
- **ad-core-rs**: `publish_result` now uses `set_params_and_notify` instead of `write_int32_no_wait` for plugin readback values
- **ad-plugins-rs**: Restore Overlay Position↔Center bidirectional readback (safe with new path)
- **commonPlugins.cmd**: Add missing `NDTimeSeriesConfigure` commands for Stats/ROIStat/Attr TS ports

## v0.8.0

### HDF5 Plugin — Complete Rewrite
- **Pure Rust HDF5**: Switch from fallback binary format to real HDF5 via `rust-hdf5` (crates.io `0.2`). No C dependencies.
- **Compression**: zlib, SZIP, LZ4, Blosc (with sub-codecs: BloscLZ, LZ4, LZ4HC, Snappy, Zlib, Zstd). All via `rust-hdf5` filter pipeline.
- **SWMR streaming**: Single Writer Multiple Reader support — `SwmrFileWriter` with `append_frame`, periodic flush, ordered fsyncs.
- **Store performance**: Write timing measurement with Run time / I/O speed readback.
- **Store attributes**: Controllable via param (on/off).
- **File number fix**: Last filename now shows the actual written file, not the next incremented number.

### NeXus File Plugin (New)
- **NDFileNexus**: HDF5-based NeXus format writer with `/entry/instrument/detector/data` group hierarchy via `rust-hdf5` group API.

### Plugin on_param_change — All Plugins Complete
- **Process**: Full `on_param_change` for all 34 params. Filter type presets (RecursiveAve, Average, Sum, Difference, RecursiveAveDiff, CopyToFilter). Auto offset/scale calc. Separate low/high clip threshold and value. Scale flat field param.
- **Transform**: `on_param_change` for TRANSFORM_TYPE.
- **ColorConvert**: `on_param_change` for COLOR_MODE_OUT and FALSE_COLOR.
- **Overlay**: 8 runtime-configurable overlay slots via addr, with Position↔Center bidirectional readback.
- **FFT**: `on_param_change` for direction, suppress DC, num_average, reset_average. Num averaged readback.
- **CircularBuff**: `on_param_change` for Start/Stop, trigger A/B attributes, calc expression, pre/post count, preset triggers, soft trigger, flush on trigger. Status/triggered/trigger count readback.
- **Codec**: `on_param_change` for mode, compressor (LZ4/JPEG/Blosc), JPEG quality, Blosc sub-compressor/level/shuffle. Compression factor and status readback. Blosc compress/decompress via `rust-hdf5` filter pipeline.
- **Stats**: `on_param_change` for compute_statistics toggle.
- **BadPixel**: `on_param_change` for BAD_PIXEL_FILE_NAME — loads JSON bad pixel list at runtime. Moved from stub to real processor.
- **Attribute**: 8-channel multi-addr attribute extraction with TimeSeries integration. Moved from stub to real processor.

### Scatter/Gather — C ADCore Compatible
- **Scatter**: Round-robin distribution via `ProcessResult::scatter_index`. New `NDArrayOutput::publish_to(index)` for selective delivery.
- **Gather**: Multi-upstream wiring in `NDGatherConfigure` — accepts multiple port names.

### TimeSeries Refactor
- **`TsReceiverRegistry`**: Shared registry pattern. Stats/ROIStat/Attribute store TS receivers; `NDTimeSeriesConfigure` picks them up. Eliminates duplicate TS port creation code.
- **`NDTimeSeriesConfigure`**: Fully implemented (no longer a stub).

### File Plugin Infrastructure
- **Lazy open / Delete driver file / Free buffer**: Params wired in `FilePluginController` (shared by all file plugins).
- **ROIStat**: 32 ROIs (up from 8), with `NDROIStatN.template` × 32 in commonPlugins.cmd.

### Dependencies
- **rust-hdf5**: Switch from git dependency to crates.io `0.2`. Pure Rust HDF5 with all compression filters.

## v0.7.12

### CA Client Connection Stability
- **TCP keepalive**: Enable `SO_KEEPALIVE` with 15s idle time and 5s probe interval on all CA TCP connections. OS detects dead sockets within ~30s on idle circuits.
- **Client-side echo heartbeat**: Send `CA_PROTO_ECHO` after 30s of idle (matching C EPICS `CA_CONN_VERIFY_PERIOD`). If no response within 5s (`CA_ECHO_TIMEOUT`), declare connection dead and trigger automatic re-search + subscription recovery. Detects hung server processes that TCP keepalive alone cannot catch.
- **`EPICS_CA_CONN_TMO` support**: Echo interval configurable via environment variable, matching C EPICS behavior.

### Motor Record
- **Fix MOVN not resetting to 0**: `finalize_motion()` now clears MOVN when motion completes. Previously MOVN was computed before the phase transition to Idle and never updated, causing ophyd `PVPositionerPC` (which reads `.MOVN`) to report moving=true after `move(wait=True)` returned.

### areaDetector Plugins
- **NDFileMagick plugin**: New file writer using the `image` crate. Supports PNG, JPEG, BMP, GIF, TIFF (format determined by file extension), UInt8/UInt16 data, mono and RGB color modes. Parameters: `MAGICK_QUALITY`, `MAGICK_BIT_DEPTH`, `MAGICK_COMPRESS_TYPE`.
- **Idempotent plugin Configure commands**: Skip if port already exists, allowing `commonPlugins.cmd` to be loaded multiple times with different `PREFIX` for alias records.
- **Activate NDFileMagick** in `commonPlugins.cmd`.

### Asyn Device Support
- **Initial readback for input records**: Enable `with_initial_readback()` for input records (stringin, longin, etc.), matching C EPICS `devAsynXxx` `init_common()` behavior. Fixes `PluginType_RBV` and other I/O Intr input records returning template defaults ("Unknown") instead of the driver's current value.

### Wiring
- **Fix sender loss on failed rewire**: Validate new upstream exists before extracting sender from old upstream. Previously a failed rewire (e.g., invalid port name) would drop the sender, causing all subsequent rewires to fail.

## v0.7.11

### CA Client Transport Rewrite
- **Single-owner writer task**: Replace `Arc<Mutex<OwnedWriteHalf>>` with a dedicated `write_loop` task + mpsc channel. Eliminates writer lock contention between command dispatch and read_loop (ECHO responses).
- **Batch coalescing**: Writer task drains all pending frames via `try_recv` before issuing a single `write_all`, reducing TCP segment count under burst load.
- **TCP_NODELAY**: Set on all CA transport connections. Fixes ~45ms stall on `get()` immediately after `put()` caused by Nagle's algorithm + delayed ACK interaction.
- **Immediate write-error propagation**: `write_loop` sends `TcpClosed` on socket write failure, so pending `get()`/`put()` waiters fail immediately instead of hanging until timeout.

### CA Client Connection Fix
- **Channel starvation during concurrent PV creation**: `WaitConnected` and `Found` responses arriving before `RegisterChannel` are now buffered in `pending_wait_connected` / `pending_found` maps and drained on registration, preventing lost connections and infinite search loops.

## v0.7.10

### CA Client Search Engine Rewrite (libca++ level)
- **Adaptive deadline scheduler**: BTreeSet-based global scheduler replaces per-PV exponential backoff — lane-indexed retry with `period = (1 << lane) * RTT estimate`, max 5 min (configurable via `EPICS_CA_MAX_SEARCH_PERIOD`, floor 60s)
- **Per-path RTT estimation**: Jacobson/Karels algorithm (RFC 6298) per server address, 32ms floor — backoff adapts to actual network conditions instead of fixed 100ms→2s
- **Batch UDP search**: multiple SEARCH commands packed into single datagrams (≤1024 bytes), reducing packet count by ~30-50x for large PV sets
- **AIMD congestion control**: `frames_per_try` with additive increase (+1 on >50% response rate) / multiplicative decrease (reset to 1 on <10%) — prevents network flooding during mass PV search
- **Beacon anomaly detection**: dedicated `BeaconMonitor` task registers with CA repeater, tracks per-server beacon sequence/period, detects IOC restart (ID gap or period drop) and triggers selective rescan with 5s fast-rescan window
- **Connect-feedback penalty box**: servers that fail TCP create are deprioritized for 30s — prevents repeated connection attempts to unreachable servers
- **Selective rescan**: coordinator maintains server→channel reverse index, beacon anomaly rescans only affected channels (not global storm)
- **Immediate search on Schedule**: drain queued requests and send in same event loop iteration — fixes starvation where burst `create_channel` calls could delay first UDP search indefinitely

### CA Client Connection Improvements
- **Keep connect waiters on ChannelCreateFailed**: waiters stay pending so immediate re-search can still resolve before caller timeout (was: drain waiters on first failure)
- **AccessRightsChanged on channel create and reconnect**: fire event immediately after channel becomes connected
- **DBE_LOG in monitor mask**: match pyepics default (DBE_VALUE | DBE_LOG | DBE_ALARM)
- **Search recv buffer**: 256KB SO_RCVBUF for burst search response handling
- **Internal CA timeouts**: read/subscribe raised from 5s to 30s

### CA Client API
- **`CaChannel::info()`**: get channel metadata (native type, element count, host, access rights) without performing a CA read
- **`Snapshot` monitors**: `CaChannel::subscribe()` returns `Snapshot` with EPICS timestamp and alarm status

### IOC Shell
- **Output redirection**: `> file` and `>> file` support in iocsh without libc dependency

### Asyn
- **Synchronous write**: `can_block=false` ports use direct write instead of async channel, fixing write_op type coercion

## v0.7.9

### File Plugin Architecture (C ADCore NDPluginFile pattern)
- **`FilePluginController<W: NDFileWriter>`**: generic file plugin controller extracted to `ad-core-rs`, matching C ADCore's `NDPluginFile` base class — all file control logic (auto_save, capture, stream, temp_suffix rename, create_dir, param updates, error reporting) in one place
- All file plugins (TIFF, HDF5, JPEG, NetCDF) now delegate to `FilePluginController` via composition, eliminating ~300 lines of duplicated control logic
- **Auto-save**: write each incoming array as a single file when `AutoSave=Yes` (matches C `processCallbacks` autoSave)
- **Stream mode auto-stop**: close stream when `NumCaptured >= NumCapture` (NumCapture > 0), matching C `doCapture(0)` pattern
- **Capture mode**: full buffer → flush → close cycle with `NumCaptured` tracking
- **Temp suffix rename**: write to `path.tmp`, rename to `path` on close (all three modes)
- **Create dir**: `create_dir != 0` triggers `create_dir_all` (was `> 0` only, negative values like `-5` were ignored)
- **Write message cleared on success**: prevents stale error messages from persisting after successful writes
- **printf-style file template**: proper `%s%s_%3.3d.tif` expansion with sequential `%s` → filePath/fileName, `%d` with width/precision

### Waveform FTVL=CHAR Support
- asynOctetWrite device support for waveform records with `FTVL=CHAR`
- `write_only` flag: `read()` performs write (waveform is input record type in EPICS)
- Dynamic `field_list()` returns FTVL-appropriate VAL type (prevents CA write coercion errors)
- String → CharArray coercion in `put_field` for FTVL=CHAR
- NELM padding preserved on put (resize to NELM, prevents element count shrink)
- Trailing null trimming from CharArray before OctetWrite

### Plugin Infrastructure
- `register_params` implemented for all 12+ areaDetector plugins (was missing, causing silent `drv_user_create` failures)
- `on_param_change` with `Vec<ParamUpdate>` return for immediate param feedback (FILE_PATH_EXISTS, FULL_FILE_NAME, etc.)
- `ParamUpdate::Octet` variant for string param updates from data plane
- Fix NDArrayPort rewire: skip no-op rewire when `new_port == current_upstream` (eliminates startup race condition errors)

### Other
- `AdIoc::register_record_type()` for custom record type registration
- `put_notify` completion: `complete_async_record` fires `put_notify_tx.send(())` for CA WRITE_NOTIFY responses
- ophyd-test-ioc: all plugin ports reused for ADSIM prefix, motor record type registered

## v0.7.8

### Universal Asyn Device Support (C EPICS pattern)
- **`universal_asyn_factory`**: single factory handles all standard asyn DTYPs (`asynInt32`, `asynFloat64`, `asynOctet`, all array types) by parsing `@asyn(PORT,ADDR,TIMEOUT)DRVINFO` links and resolving params via `drv_user_create` → `find_param`, matching C EPICS asyn behavior exactly
- **All custom device support eliminated**: `MovingDotDeviceSupport`, `PointDetectorDeviceSupport`, `SimDeviceSupport`, `ScopeDeviceSupport`, `PluginDeviceSupport` — replaced by universal factory (~1,800 lines removed)
- **`ParamRegistry` infrastructure removed**: `ParamRegistry`, `ParamInfo`, `RegistryParamType`, all `build_param_registry` functions — `drv_user_create`/`find_param` replaces them
- **Plugin dynamic factory removed**: `PluginManager` no longer provides device support dispatch — only manages lifecycle, port registration, and NDArray wiring

### Template Migration
- All templates converted from `$(DTYP)` to standard asyn DTYPs with `@asyn(PORT,...)DRVINFO` links
- CP-linked records use 2-stage pattern (C ADCore `NDOverlayN` pattern): Soft Channel link receiver → asyn record via `OUT PP`
- `commonPlugins_settings.req` aligned with C ADCore (added StdArrays, Scatter/Gather, AttributeN, file-type-specific .req)

### Array Data (C EPICS pattern)
- Full array type support: `Int8`, `Int16`, `Int32`, `Int64`, `Float32`, `Float64` (read + write)
- `PluginPortDriver::read_*_array` overrides serve pixel data from NDArray (matching C `NDPluginStdArrays::readArray`)
- Array data pushed via direct interrupt (bypasses port actor channel), matching C `arrayInterruptCallback` pattern
- `param_value_to_epics_value` handles all array `ParamValue` variants

### Param Names (C ADCore alignment)
- All `create_param` names aligned with C ADCore `#define` strings: `ACQ_TIME`, `ACQ_PERIOD`, `NIMAGES`, `STATUS`, `ENABLE_CALLBACKS`, `ARRAY_NDIMENSIONS`, etc.
- Added missing `NDPluginDriver` params: `MAX_THREADS`, `NUM_THREADS`, `SORT_MODE`, `SORT_TIME`, `SORT_SIZE`, `SORT_FREE`, `DISORDERED_ARRAYS`, `DROPPED_OUTPUT_ARRAYS`, `PROCESS_PLUGIN`, `MIN_CALLBACK_TIME`, `MAX_BYTE_RATE`

### Other
- Per-parameter callback flush (`call_param_callback`) to avoid unintended side-flush
- `normalize_asyn_dtyp`: strips direction suffixes (`asynOctetRead` → `asynOctet`, `asynFloat64ArrayIn` → `asynFloat64Array`)
- Graceful `drv_user_create` failure: silently disables device support for records without matching driver param
- MovingDot: binning support (BinX/BinY), fix NDArray dims order
- Autosave for MovingDot cam1, `commonPlugins_settings.req` fixes
- `PvDatabase::get_pv_blocking` for sync access from std::threads
- `AdIoc::keep_alive` for driver runtime lifetime management
- `EpicsTimestamp::to_system_time` for interrupt timestamp consistency
- Fix array interrupt: handle I64/U64 types, use NDArray timestamp (not wall clock)
- Fix ADCORE path in AdIoc (`ad-core` → `ad-core-rs`)
- ophyd-test-ioc: switch from MovingDot to SimDetector (provides GainX/Y, Noise, etc.)
- ophyd-test-ioc: use AdIoc, add ADSIM: prefix for ophyd test compatibility
- All crate READMEs: fix license to EPICS Open License, add missing READMEs

## v0.7.7

_Superseded by v0.7.8 — v0.7.7 was an intermediate release._

## v0.7.6

### Runtime Facade
- **asyn-rs**: add `runtime::sync` (mpsc, oneshot, broadcast, Notify, Mutex, RwLock), `runtime::task` (spawn, sleep, interval, RuntimeHandle), and `runtime::select!` re-exports — driver authors no longer need to depend on tokio directly
- **epics-base-rs**: add matching re-exports in `runtime::sync` and `runtime::task`, plus `select!` macro re-export and hidden `__tokio` re-export for macro hygiene

### Proc Macros
- **`#[epics_main]`**: attribute macro replacing `#[tokio::main]` — validates `async fn main()`, no args, no generics, no attribute arguments; builds multi-thread runtime via `epics_base_rs::__tokio`
- **`#[epics_test]`**: attribute macro replacing `#[tokio::test]` — validates async fn with no args/generics, rejects duplicate `#[test]`; builds current-thread runtime (matching `#[tokio::test]` default)

### Examples Modernized
- All examples (`mini-beamline`, `scope-ioc`, `sim-detector`, `ophyd-test-ioc`, `random-signals`) now use the runtime facade instead of tokio directly
- `scope-ioc`: `epics-base-rs` promoted from optional to required dependency
- Zero `tokio::` references remain in example code (except `#[tokio::main]` → `#[epics_main]`)

### Docs
- Quick Start: add binary location (`target/release/`) and PATH setup
- Quick Start: fix build command to use `--release`
- Update copyright name in LICENSE

## v0.7.5

### areaDetector PV Convention
- Adopt standard areaDetector PV convention (`P=mini:dot:`, `R=cam1:`) in mini-beamline
- Add NDStdArrays `image1` plugin to `commonPlugins.cmd`
- Include `ADBase.template` for full ADBase PV set (TriggerMode, Gain, etc.)
- Add missing param registry entries for NDArrayBase PVs
- Fix param name mismatches with C ADCore templates

### CA Server
- Non-blocking WRITE_NOTIFY: spawn background task for completion instead of blocking `dispatch_message`, matching C EPICS rsrv behavior
- Remove arbitrary 30s timeout — wait indefinitely for record completion

### MovingDot Driver
- Non-blocking port writes in device support and acquisition task to prevent tokio thread starvation
- Remove `call_param_callbacks` from driver write methods to prevent re-entrant message storms
- Add slit aperture simulation (SlitLeft/Right/Top/Bottom in pixels)
- Output UInt16 image data (realistic photon counts)
- Tolerate read failures during config refresh instead of aborting acquisition

### Waveform Record
- Add SHORT/USHORT and FLOAT FTVL support (was falling through to DOUBLE)
- Fix `DbFieldType`-to-`menuFtype` mapping in `new()`
- `PluginDeviceSupport`: native `EpicsValue` types for NDArray data

### AsynDeviceSupport
- Add public accessors (`reason`, `addr`, `handle`, `write_op_pub`)

### Docs
- Quick Start: add binary location (`target/release/`) and PATH setup
- Quick Start: fix build command to use `--release`
- Update copyright name in LICENSE

## v0.7.4

### New Crate
- **optics-rs**: Port of EPICS optics synApps module — table record (6-DOF, 4 geometry modes), Kohzu/HR/ML-mono DCM controllers, 4-circle orientation matrix, XIA PF4 dual filter, auto filter drive, HSC-1 slit, quad BPM, ion chamber, Chantler X-ray absorption data (22 elements), 36 database templates, PyDM UI screens, 362 tests including 46 golden tests vs C tableRecord.c

### dbAccess: C EPICS Parity
- **Three-tier DB write API** matching C EPICS semantics:
  - `put_pv` / `put_f64` = C `dbPut` — value + special, no monitor, no process
  - `put_pv_and_post` / `put_f64_post` = C `dbPut` + `db_post_events` — value + monitor on change
  - `put_record_field_from_ca` / `put_f64_process` = C `dbPutField` — value + process + monitor
- **Event source tagging** — origin ID prevents sequencer self-feedback loops; `DbChannel::with_origin()`, `DbMultiMonitor::new_filtered()`, origin-aware `DbSubscription`
- **DbChannel API**: add `put_i16_process`, `put_i32_process`, `put_string_process`, `get_i32`
- **TPRO** trace processing output when field is set
- **Pre-write special** hook in CA put path (`special(field, false)` before write)
- **Read-only field** enforcement in `put_record_field_from_ca`
- **ACKS/ACKT** alarm acknowledge with severity comparison
- **Menu string resolution** in type conversion (String → Enum/Short)
- **dbValueSize / dbBufferSize** equivalents
- **is_soft_dtyp**: recognize "Raw Soft Channel", "Async Soft Channel", "Soft Timestamp", "Sec Past Epoch"
- **stringout**: add OMSL/DOL fields and framework DOL processing support

### SNL Programs: CA → DbChannel Migration
- All 7 optics-rs SNL programs converted from CA client to direct database access:
  kohzu_ctl, hr_ctl, ml_mono_ctl, kohzu_ctl_soft, orient, pf4, filter_drive
- Origin tagging + filtered monitors prevent write-back loops
- Kohzu DCM: non-blocking move with `tokio::select!` retarget support

### Bug Fixes
- **I/O Intr read timeout**: cache interrupt value in adapter, skip blocking read on cache miss
- **ao DOL/OIF conflict**: remove duplicate DOL handling from ao process() (framework handles it)
- **put_pv_and_post timestamp**: update `common.time` before posting monitor events
- **Redundant monitors**: suppress duplicate events when value unchanged

### Breaking Changes
- Remove `epics-seq-rs`, `snc-core-rs`, `snc-rs` (replaced by native Rust async state machines in optics-rs and std-rs)

## v0.7.3

### New Crates
- **std-rs**: Port of EPICS std module — epid (PID/MaxMin feedback), throttle (rate-limited output), timestamp (formatted time strings) records, plus device support (Soft/Async/Fast Epid, Time of Day, Sec Past Epoch) and SNL programs (femto gain control, delayDo state machine)
- **scaler-rs**: Port of EPICS scaler module — 64-channel 32-bit counter record with preset-based counting, OneShot/AutoCount modes, DLY/DLY1 delayed start, RATE periodic display update, asyn device support, and software scaler driver

### Framework: ProcessOutcome / ProcessAction
- **Breaking**: `Record::process()` now returns `CaResult<ProcessOutcome>` instead of `CaResult<RecordProcessResult>`
- `ProcessOutcome` contains `result` (Complete/AsyncPending) + `actions` (side-effect requests)
- `ProcessAction::WriteDbLink` — record requests a DB link write without direct DB access
- `ProcessAction::ReadDbLink` — record requests a DB link read (pre-process execution)
- `ProcessAction::ReprocessAfter(Duration)` — delayed self re-process (replaces C `callbackRequestDelayed` + `scanOnce`)
- `ProcessAction::DeviceCommand` — record sends named commands to device support via `handle_command()`
- Processing layer executes actions at the correct point in the cycle (ReadDbLink before process, WriteDbLink/DeviceCommand after, ReprocessAfter via tokio::spawn)

### Framework: DeviceReadOutcome
- **Breaking**: `DeviceSupport::read()` now returns `CaResult<DeviceReadOutcome>` instead of `CaResult<()>`
- `DeviceReadOutcome` carries `did_compute` flag and `actions` list
- `did_compute`: signals that device support already performed the record's compute step (e.g., PID), passed to record via `set_device_did_compute()` before `process()`
- Device support actions are merged into the record's ProcessOutcome by the framework

### Framework: Other Improvements
- `Record::pre_process_actions()` — return ReadDbLink actions executed BEFORE process() (matches C `dbGetLink` immediate semantics)
- `Record::put_field_internal()` — bypasses read-only checks for framework-internal writes
- `Record::set_device_did_compute()` — framework signals device support compute status
- `DeviceSupport::handle_command()` — handle named commands from ProcessAction::DeviceCommand
- `field_io.rs`: `put_pv()` and `put_record_field_from_ca()` now call `on_put()` + `special()` for record-owned fields (was previously only for common fields)
- ReprocessAfter timer cancellation via generation counter in RecordInstance (prevents stale timer accumulation)

### Workspace Integration
- Add `std-rs` and `scaler-rs` to workspace members and default-members
- Add `std` and `scaler` feature flags to epics-rs umbrella crate
- Bundle 70+ database templates (.db) and autosave request files (.req)

### Testing
- Add 390+ new tests across all crates:
  - std-rs: 94 tests (epid PID algorithm, throttle rate limiting, timestamp formats, SNL state machines, framework integration, e2e autosave)
  - scaler-rs: 40 tests (64-channel field access, state machine, TP↔PR1 conversion, soft driver, DLY delayed start, COUT/COUTP link firing)
  - asyn-rs: 20 integration tests (port driver parameters, octet echo, error handling, interrupt callbacks, enum, blocking API)
  - ad-core-rs: 47 tests (NDArray types/dimensions, pool allocation/reuse/memory limits, attributes, concurrent access)
  - epics-macros-rs: 27 tests (derive macro field generation, type mapping, read-only, snake_case conversion)
  - epics-ca-rs: 30 tests (protocol header encoding, server builder, get/put API, field access, multiple record types)
  - epics-pva-rs: 49 tests (scalar types, PvStructure, serialization roundtrip, protocol header, codec)
  - epics-seq-rs: 30 tests (event flags, channel store, program builder, variable traits)
  - snc-core-rs: 42 tests (lexer tokenization, parser AST, codegen output, end-to-end pipeline)
  - snc-rs: 11 tests (CLI help, compilation, error handling, debug flags)

## v0.7.2

- Fix asyn-rs epics feature compilation (get_port export, AsynRecord import)
- Migrate record factory registration from global registry to IocApplication injection
- Replace global port registry with shared PortRegistry instance
- Add feature matrix to CI (asyn-rs/epics, ad-core-rs/ioc, ad-plugins-rs/ioc)
- Add IocApplication::register_record_type() method
- Add motor_record_factory() and asyn_record_factory() returning injectable tuples

## v0.7.1

### Architecture
- Extract `IocBuilder` from `CaServerBuilder` into epics-base-rs (protocol-agnostic IOC bootstrap)
- Move `IocApplication` to epics-base-rs with pluggable protocol runner closure
- Split `database.rs` into modules: field_io, processing, links, scan_index
- Split `record.rs` into modules: alarm, scan, link, common_fields, record_trait, record_instance
- Split `types.rs` into modules: value, dbr, codec
- Split `db_loader.rs` into parser + include expander modules
- Split `asyn_record.rs` registry into separate module
- Extract motor field dispatch to `field_access.rs`
- Remove thin wrapper crates (autosave-rs, busy-rs, epics-calc-rs) — now re-exported from epics-base-rs
- Remove legacy autosave API, migrate to SaveSetConfig/AutosaveManager
- Remove unused calc feature flags
- Crate directory names now match crate names (crates/motor → crates/motor-rs, etc.)

### API
- Reduce public API surface: 7 internal modules → pub(crate) (recgbl, scan_event, exception, interpose, protocol, transport, channel)
- Motor lib.rs: fields, coordinate → pub(crate); remove pub use fields::*, flags::*
- Add `create_record_with_factories()` for dependency injection (avoids global registry)
- `IocApplication::run()` now accepts a protocol runner: `.run(run_ca_ioc).await`

### Testing
- Move large inline test blocks to tests/ directory (3,337 lines)
- Add autosave integration test with mini-beamline (save + restore on restart)

### Fixes
- Fix ad-core path references after directory rename
- Fix remaining old crate directory references in README and examples
- Clean all clippy warnings

## v0.7.0

- **Breaking**: Separate Channel Access into `epics-ca-rs` crate
- **Breaking**: Separate pvAccess into `epics-pva-rs` crate
- **Breaking**: Rename crates for consistent `-rs` suffix (ad-core-rs, ad-plugins-rs, epics-macros-rs, epics-seq-rs, snc-core-rs, snc-rs)
- Add `epics-rs` umbrella crate with feature flags (ca, pva, motor, ad, calc, full, etc.)
- Remove msi from workspace (moved to separate repo)
- Add 113 C EPICS parity tests (ai/bi/bo record, deadband, alarm, calc engine, FLNK chains, CA wire protocol, .db parsing, autosave)
- Add SAFETY comments for production unwrap sites
- Clippy lint cleanup across all crates

## v0.6.1

- Fix monitor deadband for records without MDEL field
- Reset beacon interval on TCP connect/disconnect (C EPICS parity)
- Fix caput-rs to use fire-and-forget write like C caput, add `-c` flag for callback mode
- Show Old/New values in caput-rs output
- Support multiple PV names in CA/PVA CLI tools (caget, camonitor, cainfo, pvget, etc.)
- Add per-field change detection for monitor notifications
- Add DMOV same-position transition tests
- Poll motor immediately on StartPolling for faster DMOV response
- Add motor tests ported from ophyd (sequential moves, calibration, RBV updates, homing)
- Update minimum Rust version to 1.85+ for edition 2024

## v0.6.0

- Deferred write_notify via callback for motor records
- Motor display/ctrl metadata support
- SET mode RBV updates

## v0.5.2

- Fix monitor notify, DMOV transition, timestamp, and IPv4 resolution

## v0.5.1

- Add DMOV 1->0->1 monitor transition for motor moves

## v0.5.0

- Fix motor record process chain, client error handling, and connection speed
- Add ophyd-test-ioc example

## v0.4.6

- Add client-side DBR_TIME/CTRL decode and get_with_metadata() API

## v0.4.5

- Upgrade Rust edition 2021 -> 2024

## v0.4.4

- Bug fixes

## v0.4.3

- Add generalTime framework for priority-based time providers
- Add random-signals example
- Add GitHub Actions CI workflow

## v0.4.2

- Implement C-compatible autosave iocsh commands and request file infrastructure

## v0.4.1

- Implement full YUV color mode support and refactor color convert plugin

## v0.4.0

- Initial crates.io publish
- Move to epics-rs GitHub organization

## v0.3.0

- Unify workspace version management
