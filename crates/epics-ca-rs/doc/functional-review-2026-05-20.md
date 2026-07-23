# Functional Review - 2026-05-20

Scope: `epics-ca-rs` current workspace state, compared against
`~/codes/epics-base` CA client (`modules/ca/src/client`) and rsrv / access
security (`modules/database/src/ioc/rsrv`,
`modules/libcom/src/as`). This file records open functional gaps and bugs that
are still visible in the current Rust code. Older findings that current code has
closed are not repeated here.

## Summary

| ID | Type | Severity | Area | Status |
|----|------|----------|------|--------|
| CA-FR-1 | Missing C behavior | High | ACF `CALC` / `INP*` | Done |
| CA-FR-2 | Bug | Medium | client id allocation | Done |
| CA-FR-3 | Missing C behavior | Medium | client priority / contexts | Done |
| CA-FR-4 | Missing C tool behavior | Medium | CLI request options | Done |
| CA-FR-5 | Missing C behavior | Medium | sync group lifecycle | Done |
| CA-FR-6 | Bug | Medium | SimplePv monitor masks | Done |
| CA-FR-7 | Bug | High | DBR payload sizing | Done |
| CA-FR-8 | Missing C behavior | Medium | channel filters on READ / SimplePv | Done |

## CA-FR-1: ACF `CALC` / `INP*` rules fail closed instead of evaluating

Rust parses `INP(A..U)` declarations and `CALC(...)` clauses, but it does not
resolve `INP*` database links at access-check time. A syntactically valid
`CALC` clause is compiled and then the rule is marked ignored, so access is
denied even when epics-base would evaluate the expression to true.

Rust evidence:

- `crates/epics-base-rs/src/server/access_security.rs:423-427` stores `INP`
  links but documents that the link values are not resolved.
- `crates/epics-base-rs/src/server/access_security.rs:1531-1562` compiles a
  valid `CALC` expression and then sets `ignore = true`.
- `crates/epics-base-rs/src/server/access_security.rs:2342-2360` pins this
  behavior with `calc_rule_is_disabled_when_unevaluable`.

C reference:

- `~/codes/epics-base/modules/libcom/src/as/asLibRoutines.c:117` allocates
  per-ASG `pavalue` storage for CALC inputs.
- `asLibRoutines.c:957-964` calls `calcPerform()` when the inputs changed.
- `asLibRoutines.c:1038-1042` grants the rule when the CALC result is true and
  the used inputs are not marked bad.
- `asLibRoutines.c:1385-1404` compiles `CALC` with `postfix()`.

Impact:

An ACF rule such as:

```text
ASG(OPS) {
    INPA("permit.VAL")
    RULE(1, WRITE) { CALC("A=1") }
}
```

will grant writes in epics-base while `permit.VAL == 1`. In this Rust
implementation the same rule is inert. That is fail-closed, but it is still a
behavioral divergence for sites that use dynamic ACF rules.

Fix direction:

Add an access-security input resolver owned by the server/database layer. The
resolver must map each `AsgInp` link to the current numeric value, track bad
inputs, recompute affected ASGs on input change, and let `AccessGate::check`
consume the resulting per-rule CALC state instead of disabling the rule at
parse time.

Regression tests to add:

- A `RULE(...){ CALC("A=1") }` grants access when the linked PV is `1`.
- The same rule denies when the linked PV is `0`.
- A bad or disconnected input denies the CALC-gated rule.
- A linked PV update changes access without requiring an ACF reload.

## CA-FR-2: client `cid` / `ioid` / `subid` allocation can collide after wrap

Rust uses process-global atomic counters and only skips zero after wrap.
There is no live-table collision check for `cid`, `ioid`, or `subid`.

Rust evidence:

- `crates/epics-ca-rs/src/channel.rs:6-8` defines `NEXT_CID`, `NEXT_IOID`, and
  `NEXT_SUBID` as global `AtomicU32` counters.
- `crates/epics-ca-rs/src/channel.rs:10-19` states the fix is partial and that
  a full free-list / live-table collision check is deferred.
- `crates/epics-ca-rs/src/channel.rs:20-33` skips zero but returns the next
  non-zero value without consulting live channel, IO, or subscription maps.

C reference:

- `~/codes/epics-base/modules/ca/src/client/cac.cpp:533-535` allocates a
  channel and registers it through `chanTable.idAssignAdd()`.
- `cac.cpp:711-719` allocates write-notify IO through `ioTable.idAssignAdd()`.
- `cac.cpp:725-733` allocates read-notify IO through `ioTable.idAssignAdd()`.

Impact:

At high request rates, an `ioid` wrap can reuse an ID that still belongs to an
in-flight read or write. A late response could then wake the wrong waiter or
remove the wrong map entry. The code comment estimates wrap at about 11.9 hours
at 100k allocations per second; long-running gateways or stress tools can reach
that class of runtime.

Fix direction:

Move allocation into the owning registries:

- `cid`: coordinator channel table.
- `ioid`: shared in-flight read/write registry.
- `subid`: subscription registry.

Each allocator should probe for a vacant ID, reserve it atomically with the
registration, and release it when the owner removes the entry. A bounded scan
with an exhausted-ID error is preferable to returning a colliding ID.

Regression tests to add:

- Force allocator wrap with one live ID present and assert the live ID is not
  reissued.
- Verify stale read and write replies cannot complete a newly allocated waiter
  after wrap.
- Verify subscription cancel/data paths reject a wrapped `subid` collision.

## CA-FR-3: libca priority and attachable context semantics are not modeled

The Rust client exposes `CaClient::create_channel(&str)` with no priority
argument, and the transport key is `SocketAddr`, so all channels to a server
share one virtual circuit. libca treats priority as part of channel creation
and creates independent virtual circuits for different priorities to the same
server. Rust also lacks the C API shape where a thread can attach to an existing
CA context.

Rust evidence:

- `crates/epics-ca-rs/src/client/mod.rs:754-759` exposes `create_channel`
  without priority and immediately allocates only a `cid`.
- `crates/epics-ca-rs/src/client/transport.rs:246-314` queues and connects by
  `server_addr`.
- `crates/epics-ca-rs/crates/epics-ca-rs/doc/09-libca-parity.md:111-119` records the missing
  priority, per-priority virtual circuit, OS-thread priority, and attachable
  context behavior.

C reference:

- `~/codes/epics-base/modules/ca/src/client/cadef.h:498-508` defines the
  `ca_create_channel()` priority parameter and states that each priority used
  on a server creates an independent virtual circuit and data structures.
- `~/codes/epics-base/modules/ca/src/client/cac.cpp:512-520` range-checks the
  requested priority.
- `cac.cpp:539-559` creates or finds a virtual circuit with the requested
  priority.
- `cadef.h:1938` exposes `ca_attach_context()`.

Impact:

Sites that rely on CA priority for QoS isolation cannot express that in this
client. Bulk monitor traffic and latency-sensitive control traffic to the same
IOC share one TCP circuit and one Tokio task set. Libraries that expect to
attach worker threads to one CA context also need a Rust-specific design
instead of a libca-compatible control surface.

Fix direction:

Introduce a priority-aware channel creation API and include priority in the
transport circuit key, for example `(SocketAddr, priority)`. If C ABI parity is
required later, keep that model separate from idiomatic `CaClient` ownership so
thread attach/detach behavior can be emulated without weakening Rust lifetime
rules.

Regression tests to add:

- Creating two channels to the same server at different priorities opens two
  transport circuit entries.
- The TCP VERSION priority field matches the requested priority.
- Dropping one priority circuit does not disconnect channels on another
  priority circuit.

## CA-FR-4: CLI tools accept request-changing options without honoring them

Several `caget-rs`, `caput-rs`, `camonitor-rs`, and `cainfo-rs` options are
parsed but then reported as parity-only no-ops. The C tools use these same
options to change the DBR request type, CA priority, monitor mask, timestamp
source, long-string put behavior, or diagnostic call.

Rust evidence:

- `crates/epics-ca-rs/src/bin/caget-rs.rs:46-63` accepts priority and DBR type,
  while `:230-237` warns that `-p`, `-d`, and `-s` are not honoured.
- `crates/epics-ca-rs/src/bin/caput-rs.rs:166-170` warns that `-p` and `-S`
  are not honoured.
- `crates/epics-ca-rs/src/bin/camonitor-rs.rs:144-156` warns that `-p`, `-m`,
  non-server `-t`, and `-s` are not honoured.
- `crates/epics-ca-rs/src/bin/cainfo-rs.rs:53-59` warns that `-p` and `-s` are
  not honoured as C-compatible behavior.

C reference:

- `~/codes/epics-base/modules/ca/src/tools/tool_lib.c:58` stores
  `caPriority`, and `:589-593` passes it to `ca_create_channel()`.
- `caget.c:415-435` parses `-d`, `:175-187` selects the DBR request type, and
  `:214-217` passes that type to `ca_array_get()`.
- `caget.c:464-468` implements string-format and char-array string flags.
- `caput.c:306-308` implements `-S`, and `:514-522` sends char arrays as
  `DBR_CHAR` through the `ca_array_put*()` path at `:543-550`.
- `camonitor.c:235-253` implements timestamp source/type selection, `:285-303`
  implements event-mask parsing, and `:174-180` passes `eventMask` to
  `ca_create_subscription()`.
- `cainfo.c:77-79` calls `ca_client_status(statLevel)` for `-s`.

Impact:

Operator scripts that rely on C tool behavior can get different requests while
the command line still succeeds. Examples: `caget -d DBR_CTRL_DOUBLE pv` does
not request control metadata, `camonitor -m p pv` does not subscribe to property
events, `camonitor -t ci pv` does not switch to client/incremental timestamps,
and `caput -S pv text` does not use the long-string `DBR_CHAR` put path.

Fix direction:

Route parsed CLI options into the same lower-level knobs the library needs for
CA-FR-3:

- priority-aware channel creation for all tools;
- explicit DBR request-type selection for GET and monitor operations;
- monitor event-mask selection and timestamp source/type formatting;
- long-string `DBR_CHAR` put conversion for `caput -S`;
- a `ca_client_status`-equivalent diagnostic path for `cainfo -s`.

Regression tests to add:

- `caget-rs -d DBR_CLASS_NAME` requests and prints the class-name DBR.
- `camonitor-rs -m p` emits property-change events and suppresses ordinary value
  events when the mask excludes them.
- `camonitor-rs -t ci` prints client incremental timestamps.
- `caput-rs -S` writes a char array including the terminating NUL.
- `cainfo-rs -s 1` emits client-status diagnostics without requiring PV names.

## CA-FR-5: sync groups are single-use and lack C reset / test / stat semantics

libca synchronous groups are reusable state objects. `ca_sg_test()` can poll
completion, `ca_sg_reset()` discards outstanding timed-out requests while keeping
the same group id, `ca_sg_stat()` reports the group, and `ca_sg_block()` resets
the group's outstanding request knowledge after it returns. Rust models
`SyncGroup` as a single-use future collection consumed by `block(self, ...)`,
with no equivalent status, reset, or diagnostic surface.

Rust evidence:

- `crates/epics-ca-rs/src/client/sync_group.rs:21-23` documents the group as
  single-use and recommends deleting and recreating it for reset-like behavior.
- `sync_group.rs:61-114` exposes only `new`, `get`, `put`, `block`, `len`, and
  `is_empty`.
- `sync_group.rs:84-103` consumes `self` in `block`, so the caller cannot reuse
  the same group, call `test` after a timeout, or explicitly reset the tracked
  operations.

C reference:

- `~/codes/epics-base/modules/ca/src/client/cadef.h:1553-1557` describes
  `ca_sg_block`, `ca_sg_test`, and `ca_sg_reset` as part of the synchronous group
  contract.
- `cadef.h:1604-1610` states that `ca_sg_block()` waits only for requests issued
  after the last `ca_sg_block`, `ca_sg_reset`, or `ca_sg_create`.
- `cadef.h:1639-1654` defines `ca_sg_test()` returning `ECA_IODONE` or
  `ECA_IOINPROGRESS`.
- `cadef.h:1656-1673` defines `ca_sg_reset()` as resetting outstanding request
  count to zero.
- `~/codes/epics-base/modules/ca/src/client/syncgrp.cpp:128-148` calls
  `sync_group_reset()` after `ca_sg_block()`.
- `syncgrp.cpp:156-198` implements `ca_sg_reset()` and `ca_sg_stat()`.
- `syncgrp.cpp:203-241` implements `ca_sg_test()`.

Impact:

Applications ported from libca that keep a `CA_SYNC_GID` across repeated batches
cannot express the same lifecycle in Rust. They also cannot poll for completion
without consuming the group, discard timed-out outstanding operations while
retaining the handle, or ask for sync-group diagnostics. Code that expects a
successful `ca_sg_block()` to reset the group before submitting the next batch
must be rewritten around Rust-specific group recreation.

Fix direction:

Keep the ergonomic batch helper if desired, but add a reusable sync group owner
with explicit operation state. `block(&mut self, timeout)` should clear tracked
operations on successful completion like libca, `reset(&mut self)` should discard
tracked outstanding operations, `test(&mut self)` should return a completion
status without consuming the group, and `stat()` should expose or print the
diagnostic state needed by parity users.

Regression tests to add:

- After a successful `block`, the same `SyncGroup` accepts a second get/put batch
  and waits only for the second batch.
- `reset` after a timeout makes `test` report complete until new operations are
  added.
- `test` reports in-progress while a scheduled operation is pending and done once
  it completes.
- `stat` reports a bad group or current outstanding operation counts through the
  chosen Rust diagnostic shape.

## CA-FR-6: `SimplePv` monitor subscriptions ignore the requested `DBE_*` mask

The CA server parses the EVENT_ADD mask and stores it on `SimplePv`
subscribers, but the `ProcessVariable` emission paths do not consult it.
Record-backed PVs do filter by mask, so this divergence only affects the
`SimplePv` / gateway-style path.

Rust evidence:

- `crates/epics-ca-rs/src/server/tcp.rs:3022-3026` decodes the EVENT_ADD mask
  from the client payload.
- `tcp.rs:3094-3097` passes that mask to
  `ProcessVariable::add_subscriber()` for `ChannelTarget::SimplePv`.
- `crates/epics-base-rs/src/server/pv.rs:355-370` stores the mask in
  `Subscriber.mask`.
- `pv.rs:298-338` sends every value update to every subscriber without checking
  whether the subscription mask includes `DBE_VALUE` or `DBE_LOG`.
- `pv.rs:251-295` sends every alarm post to every subscriber without checking
  whether the subscription mask includes `DBE_ALARM`.
- `crates/epics-base-rs/src/server/record/record_instance.rs:1878-1880`
  implements the expected filter for record fields by intersecting the post mask
  with each subscriber mask.

C reference:

- `~/codes/epics-base/modules/database/src/ioc/rsrv/camessage.c:1803-1813`
  stores the client monitor mask and passes it to `db_add_event()`.
- `~/codes/epics-base/modules/database/src/ioc/db/dbEvent.c:892-900` queues an
  event only when `caEventMask & pevent->select` is non-zero.
- `~/codes/epics-base/modules/ca/src/tools/camonitor.c:285-303` lets users pick
  the same mask bits with `camonitor -m`.

Impact:

`camonitor -m a simple:pv` can receive ordinary value updates even though it
asked only for alarm events. Conversely, a `DBE_VALUE`-only subscriber can
receive `post_alarm()` emissions. Scripts that use CA masks to split alarm
views from value streams cannot rely on the Rust simple-PV server path matching
rsrv behavior.

Fix direction:

Make `ProcessVariable` enforce `Subscriber.mask` before channel filters and
queueing. Alarm posts should require `DBE_ALARM`. Value posts need an explicit
policy: either preserve today's event class as `DBE_VALUE`, or define simple-PV
value writes as `DBE_VALUE | DBE_LOG` and apply the same intersection rule.

Regression tests to add:

- A `DBE_ALARM` subscriber to a `SimplePv` does not receive a normal value set.
- A `DBE_VALUE` subscriber to a `SimplePv` does not receive `post_alarm()`.
- A `DBE_VALUE | DBE_ALARM` subscriber receives both event classes.
- The record-field monitor path still filters by its existing mask intersection.

## CA-FR-7: DBR payload sizing does not match C `dbr_size_n()` for TIME / GR / CTRL types

Rust encodes the DBR payload bodies with type-specific layouts, but the shared
`dbr_buffer_size()` helper uses coarse class-level metadata sizes. The helper is
then used by server paths that pad or truncate explicit-count monitor responses
and no-read-access events, so those frames can be sized differently from
epics-base `dbr_size_n()`.

Rust evidence:

- `crates/epics-base-rs/src/types/codec.rs:83-92` knows that TIME_SHORT,
  TIME_ENUM, TIME_CHAR, and TIME_DOUBLE carry RISC alignment padding.
- `crates/epics-base-rs/src/types/dbr.rs:190` still returns a flat 12-byte TIME
  metadata size for every native type.
- `dbr.rs:191-206` uses one broad GR / CTRL formula for all non-string,
  non-enum types, even though short, float, char, long, and double DBR structs
  have different limit element widths and padding.
- `dbr.rs:278-285` pins `DBR_TIME_DOUBLE` as `12 + 8`, omitting the 4-byte
  RISC pad that the encoder emits.
- `crates/epics-ca-rs/src/server/monitor.rs:157-166` uses
  `dbr_buffer_size()` to resize explicit-count monitor payloads.
- `crates/epics-ca-rs/src/server/tcp.rs:4298-4310` uses the same helper for
  initial monitor snapshots.
- `tcp.rs:4258-4263` uses it to size no-read-access EVENT_ADD payloads.

C reference:

- `~/codes/epics-base/modules/ca/src/client/db_access.h:250-300` defines
  TIME_SHORT / TIME_ENUM / TIME_CHAR / TIME_DOUBLE with RISC pad fields before
  the value.
- `db_access.h:308-402` defines GR structs whose metadata size depends on the
  native value type.
- `db_access.h:410-516` defines CTRL structs with the same type-specific
  sizing.
- `db_access.h:519-534` defines `dbr_size_n(TYPE, COUNT)` as
  `dbr_size[TYPE] + (COUNT - 1) * dbr_value_size[TYPE]`.

Impact:

An explicit-count `DBR_TIME_DOUBLE` monitor response can be truncated or padded
as if the value starts at byte 12, while the encoded DBR body places the value
after byte 16. GR / CTRL monitor responses can be over-padded because the helper
uses double-width-style metadata for smaller native types. Clients that rely on
the CA header count and payload size to match C `dbr_size_n()` can mis-frame
payloads, see trailing zeros as extra data, or receive a no-read-access event
with a body size that does not match the requested DBR type.

Fix direction:

Make `dbr_buffer_size()` table-driven by DBR type, mirroring C `dbr_size[]` and
`dbr_value_size[]`, or derive sizes from the same metadata-layout functions used
by `encode_dbr()`. Then use that single size owner for explicit-count padding,
truncation, and no-read-access event frames.

Regression tests to add:

- `dbr_buffer_size(DBR_TIME_DOUBLE, Double, 1)` equals the encoded
  `DBR_TIME_DOUBLE` length including the 4-byte RISC pad.
- TIME_SHORT, TIME_ENUM, and TIME_CHAR sizes include their C pad fields.
- GR / CTRL short, float, char, long, and double sizes match the corresponding
  C struct layouts.
- Explicit-count monitor padding uses the corrected DBR size for TIME and
  GR / CTRL requests.

## Reviewed but not open

`CA_PROTO_READ_BUILD` (16) and `CA_PROTO_SIGNAL` (25) are not counted as an
open gap in this review. epics-base defines the opcodes in
`~/codes/epics-base/modules/ca/src/client/caProto.h:103` and `:112`, but rsrv's
TCP jump table maps those slots to `bad_tcp_cmd_action`
(`modules/database/src/ioc/rsrv/camessage.c:2312`, `:2321`). Current Rust maps
unsupported server commands through the same error-and-close policy in
`crates/epics-ca-rs/src/server/tcp.rs:3872-3890`.

## CA-FR-8: channel filters are monitor-only and do not run on READ or SimplePv subscriptions

epics-base channel filters are attached to the database channel, not just to one
monitor callback. A `REC.{"arr":...}` or `REC.{"dbnd":...}` read path runs the
filter chain before returning the DBR payload, and event delivery runs the same
channel pre-chain before queueing monitor data. Rust stores the JSON suffix on
the CA channel, but only the record-field `EVENT_ADD` path consumes it.

Rust evidence:

- `crates/epics-ca-rs/src/server/tcp.rs:1923-1926` splits the channel-filter
  JSON suffix and stores it as `ChannelEntry.filter_suffix`.
- `tcp.rs:2177-2282` handles `CA_PROTO_READ` / `CA_PROTO_READ_NOTIFY` by calling
  `get_full_snapshot()` and encoding that snapshot directly; it never parses or
  applies `entry.filter_suffix`.
- `tcp.rs:3900-3903` confirms `get_full_snapshot()` only returns the raw
  `SimplePv` or record-field snapshot.
- `tcp.rs:3246-3258` attaches parsed filters only after a record-field
  `EVENT_ADD`.
- `tcp.rs:3095-3205` creates a `SimplePv` subscription through
  `ProcessVariable::add_subscriber()` without passing `entry.filter_suffix`.
- `crates/epics-base-rs/src/server/pv.rs:380-395` therefore gives every
  `SimplePv` subscriber an empty `FilterChain`, even when the original channel
  name carried a filter suffix.

C reference:

- `~/codes/epics-base/modules/database/src/ioc/db/db_access.c:160-167` creates a
  read field-log and runs `dbChannelRunPreChain()` / `dbChannelRunPostChain()`
  when filters exist on a read channel.
- `~/codes/epics-base/modules/database/src/ioc/db/dbChannel.c:640-649` does the
  same for `dbChannelGetField()`.
- `~/codes/epics-base/modules/database/src/ioc/db/dbEvent.c:896-902` runs
  `dbChannelRunPreChain()` before queueing monitor events.

Impact:

`caget 'REC.{"arr":{"s":0,"e":9}}'` and `caget 'REC.{"ts":{...}}'` return the
unfiltered Rust value while epics-base returns the filtered/transformed value.
Record-field monitors are closer because Rust attaches the filter on
`EVENT_ADD`, but Rust `SimplePv` monitors ignore the suffix entirely. Clients
that use CA filters to slice arrays, apply deadbands, or rewrite timestamps see
different data depending on whether the server is rsrv or epics-ca-rs.

Fix direction:

Make the parsed channel filter chain part of the channel target or a shared
channel context, not only a record-field subscriber side effect. Apply it in the
READ / READ_NOTIFY path before DBR encoding, and pass it into
`ProcessVariable::add_subscriber()` so `SimplePv` event delivery uses the same
filter chain as record-field delivery.

Regression tests to add:

- A filtered record-field `READ_NOTIFY` applies an `arr` transform before DBR
  encoding.
- A filtered record-field monitor still applies the same chain on updates.
- A filtered `SimplePv` monitor applies `dbnd` / `arr` instead of using an empty
  chain.
- A malformed filter suffix preserves the current permissive "empty chain with
  warning" behavior.
