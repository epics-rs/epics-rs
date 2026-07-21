# item-7 stage C (blocking accept driver) + stage D (blocking UDP search responder) — scoping

Read-only investigation. No source file was edited.

**Measured at:** worktree `/home/stevek/work/epics-rs/.caucus/worktrees/integration`,
branch `integration/rtems-scope-b`, HEAD **`44681c76`** at start **and** end
(unchanged). Working tree was **clean at start**; at end it was **dirty in one
file** — see the box below. Every `file:line` is from HEAD `44681c76` unless
the path says `epics-base` or `pvxs`.

> ⚠ **The tree went dirty under the concurrent panel mid-investigation.**
> `crates/epics-pva-rs/src/server_native/blocking.rs` is modified but
> uncommitted (**+298 / −22**, `git diff --stat`), and the change is an
> **in-flight stage-4 implementation** (`ConnRegistry`, `ConnWake`, a
> registration guard). All `blocking.rs` line numbers below are therefore
> quoted against `git show 44681c76:crates/epics-pva-rs/src/server_native/blocking.rs`
> — the committed, reproducible blob, which is what I read. **§2.4
> re-verifies every affected conclusion against the dirty tree**, and one
> premise of §2 changes as a result. No other cited file went dirty
> (`git status --porcelain` listed only this one).

Both commits named in the brief are confirmed ancestors of the measured HEAD:

- `44681c76` "feat(pva): blocking thread-per-connection server driver (item 5 stage 3)" — *is* HEAD.
- `aa1af842` "refactor(pva): make the UDP search decode socket-free".

**Baseline build**, `cargo +nightly check -p epics-pva-rs --lib
--no-default-features -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf`:
**exit 0, exactly 4 warnings.** Those 4 warnings are the measurement instrument
for §3.

---

## 0. Summary of findings

1. **Stage C does NOT force stage 4.** `serve_connection_blocking` tears down
   its own connection completely and self-sufficiently
   (`blocking.rs:440-457`), and CA's shipped accept loop deliberately does not
   track client threads at all. Stage 4's socket registry is only needed to end
   *live* connections *from outside* — a `stop()` semantic CA does not have
   today either. §2. **But the ordering question is now moot**: the concurrent
   panel is landing stage 4 in the working tree as I write, and its
   `ConnRegistry` makes registration a *typed* requirement of
   `serve_connection_blocking`. Stage C should be written against it. §2.4.
2. **CA gives stage C a structural template, not reusable code.** Every one of
   the eight reuse candidates is a ~10-40 line shape that must be re-typed
   against PVA types; nothing is literally shareable because
   `BlockingCaServer` is generic over neither the source nor the
   per-connection handler. §1.
3. **pvxs DOES assign server thread priorities** — the tree the brief named as
   possibly-absent was found at a different local path, so the reference-source
   stop condition was not met (§4.0). The values are
   `epicsThreadPriorityCAServerLow-2` (TCP, `server.cpp:388`) and
   `epicsThreadPriorityCAServerLow-4` (UDP, `udp_collector.cpp:93`) = **18 and
   16**. This **contradicts item-7 decision 5's proposed "CaServerLow/19"** —
   the parity values are 18/16, not 19. §4.
4. **Stage D's blocker is a module boundary, not missing code.** The
   socket-free core `process_search_datagram` and its four helpers all still
   live *inside* `udp.rs`, which is `#[cfg(not(target_os = "rtems"))]`
   (`mod.rs:58-59`). `aa1af842` made the function socket-free but left it
   quarantined. Stage D's first move is the same extraction that `mod.rs:24-27`
   records for `tcp.rs`. §3.2.
5. **`rtems-pva-ioc` (stage G) has a shorter gap than expected**:
   `PvDatabaseSource` is already un-gated (`server/mod.rs:10` carries no `cfg`)
   and compiles on RTEMS today. What is missing is exactly stage C + stage D +
   a `[[bin]]` entry. §5.

---

## 1. The RTEMS accept loop — reuse from CA vs write new

CA's blocking driver is `crates/epics-ca-rs/src/server/blocking.rs` (2417
lines; production ends at the `#[cfg(test)]` boundary `:984`).

### 1.1 Reuse candidates, per element

| # | Element | CA `file:line` | Reusable how | What PVA must write new |
|---|---|---|---|---|
| C1 | Server struct holding `listener: TcpListener` + `shutdown: AtomicBool` | `blocking.rs:139-146` (flag at `:144`) | **Shape only** | `BlockingPvaServer` with the PVA-side fields: `DynSource`, `PvaServerConfig`, `Arc<PeerRegistry>`, `ChannelInvalidator`, `guid`, `tcp_port` |
| C2 | `bind()` — bind before any thread exists, so the port is owned by construction | `blocking.rs:150-165` (`TcpListener::bind` at `:155`) | **Verbatim shape**, different arg list | Nothing structural; carries the [[ca-server-port-ownership]] invariant across unchanged |
| C3 | `serve(&self)` accept loop over `listener.incoming()` | `blocking.rs:176-232` | **Shape only** | Body differs: CA spawns 1 thread/client, PVA spawns 1 thread that then forks into 3 inside `serve_connection_blocking` |
| C4 | Shutdown-flag re-check *both* on `Ok` and on `Err` arms | `blocking.rs:182` and `:219` | **Verbatim logic** | — |
| C5 | `shutdown()` — wake the parked `accept()` by dialing our own `local_addr()` | `blocking.rs:234-243` | **Verbatim logic** | — |
| C6 | Per-connection thread naming + priority application at thread top | `blocking.rs:200` (`apply_to_current_thread(ThreadPriority::CaServerLow)`) | **Shape**; the priority *value* changes | See §4 — PVA parity is `CaServerLow-2`, and there are 3 threads per connection, not 1 |
| C7 | `tcp_port()` accessor for the SEARCH reply to advertise | `blocking.rs:245-247` | **Verbatim** | — |
| C8 | UDP: `serve_udp_search(socket)` → `handle_udp_search_blocking(socket, …, &shutdown)` | `blocking.rs:263-265`, fn at `:446` | **Shape only** | PVA's datagram core is a different function (§3.2) |
| C9 | UDP bind with `SO_REUSEADDR`+`SO_REUSEPORT` via raw `libc`, `#[cfg(unix)]` / `#[cfg(not(unix))]` arms | `bind_udp_search` `:277`; `bind_udp_search_socket` `:302` (unix) and `:341` (non-unix) | **Verbatim shape** | PVA already has a bind path but it is in gated `udp.rs` (`bind_udp` `udp.rs:137`) |
| C10 | 200 ms `set_read_timeout` on the UDP socket so the recv loop can observe `shutdown` between datagrams | `blocking.rs:456` (`Duration::from_millis(200)`), loop `:469`, `recv_from` `:470` | **Verbatim logic** | — |
| C11 | `block_on_sync` around the async decode from a plain thread | `blocking.rs:502-507` | **Verbatim idiom** | `process_search_datagram` is `async fn` (`udp.rs:954`) with exactly one `.await` (`search_matched_cids`, seen at the +121 offset) — same treatment applies |

### 1.2 What is genuinely new (nothing in CA to copy)

- **The three-thread fan-out.** CA's client handler is one thread
  (`handle_client_blocking`, spawned at `blocking.rs:190-207`). PVA's
  `serve_connection_blocking` internally starts reader + operation + writer
  (module doc `blocking.rs:1-50`). The accept loop therefore spawns **one**
  thread which itself becomes the operation thread — so an N-client PVA server
  costs `3N+2` threads against CA's `N+2`. That is a stated RTEMS budget item
  that CA never had to declare.
- **The constructor arguments.** `serve_connection_blocking`
  (`blocking.rs:384-391` at HEAD) takes `stream, peer, source, config,
  peer_entry, channel_invalidator` — **six**; the in-flight stage-4 work adds a
  seventh, `registry: &ConnRegistry` (§2.4). On the host these are assembled in
  `accept.rs`:
  `ChannelInvalidator::new()` at `accept.rs:69`, `PeerEntry::new(is_tls_client)`
  at `:240`, `peers_for_task.insert(peer, …)` at `:241`. The RTEMS accept loop
  must reproduce that assembly — `accept.rs` itself is
  `#[cfg(not(target_os = "rtems"))]` (`mod.rs:29-30`) and cannot be reached.
- **`PvaServerConfig` construction.** `config.rs` is un-gated (`mod.rs:39`) and
  offers `Default` (`config.rs:337`), `isolated()` (`:385`) and `with_env()`
  (`:409`). `with_env()` is the IOC-correct one; it needs no new work, only a
  call site.

---

## 2. Composition: accept loop × 3 per-connection threads × `PvaServer::stop`

Answered from the actual teardown code, not from intent.

### 2.1 `PvaServer` cannot participate at all

`PvaServer` lives in `runtime.rs`, which is `#[cfg(not(target_os = "rtems"))]`
(`mod.rs:45-46`). Its fields are `Option<tokio::task::JoinHandle<…>>` for
udp/udp_v6/tcp plus three `tokio::task::AbortHandle`
(`runtime.rs:95-106`), and `impl Drop for PvaServer` (`runtime.rs:134-153`)
calls `.abort()` on each. There is no RTEMS-reachable code path here. So the
question is not "how does stage C compose with `PvaServer::stop`" but "what is
the RTEMS analogue of stop", and the answer is C5 above: an `AtomicBool` plus a
self-connect.

### 2.2 Per-connection teardown is already self-sufficient — stage 4 not required

`blocking.rs:440-457`, verbatim ordering:

1. `drop(frame_tx)` — the only strong sender (the adapter holds a weak handle),
   so the writer drains its queue, sees `None`, exits.
2. `writer.join()`.
3. `stream.shutdown(Shutdown::Both)` **on its own fd** — this is what returns
   the reader's `read`, which is parked with an effectively-infinite
   `SO_RCVTIMEO`.
4. `reader.join()`.
5. `drop(room)` — releases any producer parked on a full queue.

The module doc states the boundary explicitly (`blocking.rs:46-50`):

> Server-wide shutdown (a socket registry walked by `PvaServer::stop`) and the
> writer-exit `oneshot` arm are stage 4 (§4.2b, §4.3). This module tears down
> only its own connection, and does it without either.

### 2.3 CA proves the semantic is shippable without a registry

CA's `serve()` (`blocking.rs:176-232`) spawns each client thread and **drops
the `JoinHandle`** — the threads are detached and exit on client disconnect.
`shutdown()` (`:234-243`) stops the *accept loop* only; a live CA client
connection survives a `shutdown()` until its peer goes away. That is the
shipped, tested RTEMS CA semantic today.

**Conclusion (2), as of HEAD `44681c76`:** stage C can be written **without**
stage 4. It inherits CA's semantic: `stop()` = "stop accepting; live
connections drain on their own disconnect". Stage 4's socket registry upgrades
that to "stop also terminates live connections promptly", which is a strictly
additional capability, not a prerequisite. The one honest caveat to record in
the stage-C commit message: step 3 above shuts the socket *from inside its own
connection*, so a connection whose peer never disconnects and never sends will
hold 3 threads until process exit. That is precisely the gap stage 4 closes,
and it is the same gap CA ships with.

### 2.4 Re-verification against the dirty tree — one premise changes

The uncommitted `blocking.rs` in the working tree **implements stage 4**. The
concurrent panel got there first. Measured against the dirty file:

| Anchor | at `44681c76` | in the working tree |
|---|---|---|
| Module doc "# Not here" | `:46` | `:83` — and its text now reads "The **accept loop** that owns a `ConnRegistry` … is item 7", i.e. the *only* remaining "not here" is stage C itself |
| `pub fn serve_connection_blocking` | `:384` | `:625`, with a **7th parameter**: `registry: &ConnRegistry` |
| `drop(frame_tx)` | `:452` | `:702` |
| Reader wake | `stream.shutdown(Shutdown::Both)` on its own fd, `:454` | `registration.wake()` — the same syscall, but issued only through a registry-minted `ConnWake` handle |
| `drop(room)` | `:456` | `:712`, followed by `drop(registration)` at `:715` (the sole removal path) |
| `#[cfg(test)]` boundary | `:467` | `:725` |
| New public API | — | `ConnRegistry` `:216`, `::new` `:227`, `::stop` `:240`, `::live_connections` `:255` |

The doc block at `:61-71` states the invariant explicitly (MUST register before
either thread starts, stay registered until both join; MUST NOT shut a socket
or deregister other than through a registry-issued handle), and it notes that
the §4.2b `oneshot`/seventh-`select!`-arm design was rejected in favour of the
socket wake precisely so `tcp.rs` stays untouched.

**What this changes, and what it does not:**

- **Unchanged — the answer to question (2).** Stage C still does not *force*
  stage 4: per-connection teardown was self-sufficient before, and remains so.
  The conclusion was correct at the HEAD it was measured at.
- **Changed — the premise.** "Stage 4 is future work" is false in the working
  tree. Once that work commits, stage C is no longer *choosing* CA's
  drain-on-disconnect semantic; it must **own a `ConnRegistry`**, because the
  7th parameter makes registration non-optional by type. The §1.1 C1 row must
  add `ConnRegistry` to `BlockingPvaServer`'s fields, and C5's shutdown becomes
  two operations: set the accept flag + self-connect (CA's shape, `:234-243`)
  **and** `ConnRegistry::stop()` to end live connections.
- **Improved — the caveat above is retired.** The "3 threads held until process
  exit for a silent peer" gap that §2.3 flagged is exactly what
  `ConnRegistry::stop` closes. Stage C written against the working tree ships
  *better* stop semantics than CA does today.
- **Execution-order impact.** §5.4 lists stage 4 last. If the in-flight work
  lands first, stage C should be written directly against `ConnRegistry`
  rather than written to CA's weaker semantic and then retro-fitted.
  Re-verify the parameter list at the actual commit before starting stage C —
  it moved once already during a single investigation.

---

## 3. The 4-warning residue as a completion criterion

Baseline (measured, exit 0):

| # | Warning | Site |
|---|---|---|
| W1 | use of deprecated `fetch_update` | `tcp.rs:1404` |
| W2 | associated function `PeerEntry::new` is never used | `peers.rs:141` |
| W3 | methods `insert` and `remove` are never used (`PeerRegistry`) | `peers.rs:209`, `:214` |
| W4 | fields `reply_addr`, `reply_port`, `unicast`, `protocols`, `consumed` are never read (`SearchRequest`) | `search.rs:53` |

**Correction to the brief's wording:** it is `PeerEntry::new` +
`PeerRegistry::{insert, remove}`. `PeerRegistry::new` at `peers.rs:205` is
`pub` and is therefore *not* dead — the brief's `PeerRegistry::{new,insert,remove}`
overstates by one.

### 3.1 Stage C retires W2 and W3

The only production callers today are host-only: `PeerEntry::new` at
`accept.rs:240`, `.insert` at `accept.rs:241`, `.remove` at `accept.rs:338`
(all inside the `cfg(not(rtems))` module). Uses at `blocking.rs:654` and
`:911` are past the `:467` `#[cfg(test)]` boundary and do not count on a `--lib`
check. A stage-C accept loop that mirrors `accept.rs:240-241` and removes the
entry on connection exit retires both warnings **by construction** — if stage C
lands and W2/W3 persist, the accept loop is not tracking peers, which is a
defect, not noise.

### 3.2 Stage D retires W4 — but the first move is an extraction

All five fields are read only inside `udp.rs`, which is gated (`mod.rs:58-59`):

- `reply_addr` / `reply_port` — `udp.rs:1009-1011`, `:1041-1042`, `:1065`
- `unicast` — `udp.rs:846`, `:997`
- `protocols` — `udp.rs:1395-1397` (the protocol gate) and `udp.rs:1770-1771`
  (the forward-frame re-encoder)
- `consumed` — `udp.rs:1031-1033`, `:1108`

`queries` is *not* in the dead set because `search.rs:197-198`
(`matched_cids_for_requester`) reads it, and `tcp.rs:5908` calls that.

Note a **documentation defect found in passing**: `search.rs:34-37` claims
"TCP only consults `queries` and `protocols` (and `seq` for the response
echo)", but the compiler reports `protocols` unread on RTEMS — i.e. the TCP
SEARCH path (`tcp.rs:5866-5913`) does *not* consult `protocols`. Doc and code
disagree; not fixed here (read-only task), recorded under UNFIXED.

**The structural finding.** `aa1af842` made `process_search_datagram`
socket-free — its signature (`udp.rs:954-965`) takes `&DynSource`, a frame
slice, addresses and ports, and returns `Vec<SearchOutput>`; it has exactly one
`.await` and no tokio type. But it, and every helper it needs, is still inside
the gated module:

| Symbol | `udp.rs` line | Gated? |
|---|---|---|
| `process_search_datagram` | `:954` | yes |
| `SearchOutput` | `:911` | yes |
| `Origin` | `:1466` | yes |
| `search_matched_cids` | `:1381` | yes |
| `try_build_forward_frame` | `:844` | yes |
| `filter_inbound` | `:890` | yes |

So stage D is **not** "write a recv loop around an available core" — it is
first an extraction of the socket-free half of `udp.rs` into an un-gated module
(`search_engine.rs` or similar), then a ~60-line blocking loop shaped like C8 +
C10 + C11. This is the identical move `mod.rs:24-27` records for `tcp.rs`:

> the four items that held `tcp` back were fixed at source rather than gated
> around (config and SEARCH protocol lifted out of the host-only modules that
> held them …)

Preferring the extraction over "copy the decode logic into a new RTEMS module"
matters: a copy re-opens the whole family of SEARCH-parity bugs on a second
code path.

### 3.3 Completion criterion

After stage C **and** stage D, the RTEMS `--lib` check must show **exactly 1
warning** (W1, `fetch_update`, unrelated to item 7 and pre-existing). Any of
W2/W3/W4 surviving is a measurable incompleteness, not noise.

---

## 4. Thread priorities — pvxs upstream parity

### 4.0 Reference-source disposition

The brief named `/Users/stevek/codes/epics-modules/pvxs`. That path is
**absent** on this host (it is a macOS path; this is Linux). Per the
reference-source rule the required action is *search locally first, and stop
only if the search finds nothing*. The search **found** the tree at
**`/home/stevek/work/epics-modules/pvxs`** (the fallback path the previous
round already used, at `9348ebc`). The stop condition was therefore not met and
I proceeded. Flagging the path discrepancy explicitly rather than silently
substituting.

### 4.1 pvxs assigns priorities — the answer is not "none"

`epicsThreadPriorityCAServerLow = 20`
(`/home/stevek/work/epics-base/modules/libcom/src/osi/epicsThread.h:82`).

| pvxs thread | Site | Priority expr | Value |
|---|---|---|---|
| TCP acceptor + connection reactor (`PVXTCP`) | `src/server.cpp:388` (`Server::Pvt::Pvt`, member `acceptor_loop`) | `epicsThreadPriorityCAServerLow-2` | **18** |
| UDP search collector (`PVXUDP`) | `src/udp_collector.cpp:93` (`Pvt::Pvt`, member `loop`) | `epicsThreadPriorityCAServerLow-4` | **16** |
| Client TCP loop (`PVXCTCP`) — *client side, not server* | `src/client.cpp:1386` | `epicsThreadPriorityCAServerLow` | 20 |
| `IfMapDaemon` (interface-map refresher) | `src/evhelper.cpp:727` | `epicsThreadPriorityMin` | 0 |
| `SigInt` handler — *not a server thread* | `src/util.cpp:302` | `epicsThreadPriorityMax` | 99 |

### 4.2 Consequences for stage C

1. **item-7 decision 5 (`43fd50bd`) proposed `CaServerLow`/19 for the PVAS-*
   threads. The upstream-parity values are 18 (TCP) and 16 (UDP).** 19 matches
   neither. Recommend re-pointing decision 5 to 18/16 with these citations, or
   recording an explicit deviation with a reason.
2. **pvxs is one thread per role; we are three per connection.** pvxs's single
   `acceptor_loop` does accept + read + decode + write for all connections at
   priority 18. Our reader/operation/writer split has no upstream per-thread
   analogue. The parity-preserving assignment is therefore **all three at 18**,
   because upstream that work is one thread at 18 — splitting the work must not
   silently change its scheduling class. Any deviation from uniform-18 (e.g.
   raising the writer) is a design decision that needs its own justification,
   not a default.
3. **The ordering is deliberate and must be preserved**: UDP search (16) runs
   *below* TCP service (18), which runs *below* CA's `CaServerLow` (20). A
   flood of SEARCH datagrams must not starve established connections. Our
   RTEMS CA server already sits at 20 (`epics-ca-rs/src/server/blocking.rs:200`)
   with its event thread at `Custom(CaServerLow-1)` = 19 (`:669`). Adopting
   18/16 for PVA slots the whole PVA front-end below the whole CA front-end,
   which matches a co-hosted C IOC running both.
4. **This costs nothing today.** Priority application is opt-in
   (`EPICS_RS_ALLOW_RT_PRIORITY`, see `pi-lock-evaluation.md` §0 and
   [[pi-locks-park-on-invisible]]) and on RTEMS `apply_priority_impl` is the
   non-Linux arm returning `Unsupported`. The call sites should still be
   written at stage C — retro-fitting priority calls into three thread spawns
   later is exactly the "assign at stage C not retrofit" position decision 5
   already took. Only the *numbers* in decision 5 need correcting.

---

## 5. The smallest `rtems-pva-ioc` (stage G) with zero bridge dependency

Template: `crates/epics-ca-rs/src/bin/rtems-ca-ioc.rs` (whole file read;
`ioc` module gated `#[cfg(any(target_os = "rtems", feature = "rtems-exec-model"))]`
at `:60`).

### 5.1 What already exists and needs nothing

| Need | Status | Evidence |
|---|---|---|
| A `ChannelSource` backed by the record database | **exists, un-gated** | `PvDatabaseSource` `server/native_source.rs:33`, `new` `:46`, `new_with_acf` `:55`; `impl ChannelSource` `:583`. `server/mod.rs:10` declares `pub mod native_source;` with **no `cfg`**, unlike its neighbours `iocsh` (`:8-9`), `pva_server` (`:11-12`) which are gated |
| DB load from `.db` text on a bare target | exists | `IocBuilder` + `block_on_sync(builder.build())`, `rtems-ca-ioc.rs:92-105` — reusable verbatim, base-side |
| Callback pool / delayed timer / scanOnce before records | exists | `background_init()`, `rtems-ca-ioc.rs:112` |
| Per-connection server driver | exists | `server_native/blocking.rs:384` `serve_connection_blocking` |
| Server config | exists, un-gated | `config.rs` (`mod.rs:39`), `with_env()` `:409` |
| Peer bookkeeping types | exist, un-gated | `peers.rs` (`mod.rs:44`) — currently the source of W2/W3 |
| SEARCH reply builder | exists, un-gated | `search.rs` (`mod.rs:49`): `parse_search_request:82`, `matched_cids_for_requester:197`, `build_search_response_proto:220` |
| RTEMS dep split in the manifest | exists | `epics-pva-rs/Cargo.toml:139` `[target.'cfg(not(target_os="rtems"))'.dependencies]`, `:152` the rtems arm |

### 5.2 What does **not** exist yet — the actual gap list

1. **Stage C**: `BlockingPvaServer` — `bind` / `serve` / `shutdown` /
   `tcp_port` (§1.1 C1-C7), owning a `ConnRegistry` once the in-flight stage-4
   work commits (§2.4). ~150 lines.
2. **Stage D part 1**: extraction of the socket-free SEARCH core out of the
   gated `udp.rs` into an un-gated module (§3.2, 6 symbols).
3. **Stage D part 2**: `bind_udp_search` for PVA + `handle_udp_search_blocking`
   equivalent (§1.1 C8-C11). ~90 lines.
4. **A GUID for the server.** `random_guid()` is `udp.rs:47` — *gated*. The
   SEARCH reply cannot be built without one, so this is a fifth symbol for the
   §3.2 extraction list. (`try_fill_secure` at `udp.rs:62`/`:70` is its
   `cfg`-split helper and comes with it.)
5. **A `[[bin]]` target.** `crates/epics-pva-rs/Cargo.toml:193-221` declares
   six binaries, *every one* `required-features = ["client"]`. There is no
   server-side binary at all. A `rtems-pva-ioc` entry must be added with no
   `client` requirement — otherwise the RTEMS build pulls the entire client
   stack.
6. **A `rtems-exec-model` feature on `epics-pva-rs`.** The CA binary's gate is
   `cfg(any(target_os = "rtems", feature = "rtems-exec-model"))`
   (`rtems-ca-ioc.rs:60`), which is what makes it testable on a host. `rg` of
   `epics-pva-rs/Cargo.toml` shows no such feature; without it the binary is
   RTEMS-target-only and un-testable in CI.

### 5.3 Explicitly NOT needed

- **No bridge dependency.** `PvDatabaseSource` reaches `PvDatabase` directly
  from `epics-base-rs`. This is the zero-bridge path §5 of the previous round
  asserted, now confirmed by the un-gated `server/mod.rs:10`. The
  `bridge-rtems-walls.md` conclusion (bridge blocked by
  `epics-bridge-rs/Cargo.toml:93`'s unconditional `tokio/full`) therefore does
  **not** block stage G.
- **No beacons.** Beacon send is `udp.rs:78` (`bind_beacon_send_v6`) and the
  beacon timer machinery; PVA discovery works on SEARCH/response alone. Leave
  beacons gated for the minimal IOC and record it as a known deviation.
- **No ACF.** `PvDatabaseSource::new` (`:46`) takes no ACF; `new_with_acf`
  (`:55`) is the opt-in. The CA binary makes the same choice
  (`rtems-ca-ioc.rs:130`, permissive default).
- **No `PvaServer` / `runtime.rs`.** §2.1.

### 5.4 Suggested execution order

1. Stage D part 1 (the extraction) — it is pure code movement, retires nothing
   by itself, but unblocks both D-part-2 and the GUID need, and is the change
   most likely to conflict with a concurrent panel.
2. Stage C (`BlockingPvaServer`) — retires W2, W3.
3. Stage D part 2 (the UDP responder) — retires W4. RTEMS `--lib` warnings
   should now read exactly 1.
4. Stage G (`rtems-pva-ioc` binary + `rtems-exec-model` feature).
5. Stage 4 (socket registry) — only now, as the upgrade from CA's
   drain-on-disconnect semantic to prompt termination.

---

## 6. Report

**Tested:**

- `git rev-parse HEAD` on the integration worktree — pass (`44681c76` at start and end, unchanged)
- `git status --porcelain` at start — pass (clean)
- `git status --porcelain` at end — **dirty**: `crates/epics-pva-rs/src/server_native/blocking.rs` modified (+298/−22) by the concurrent panel. Handled, not ignored: citations re-pointed to the `44681c76` blob and every affected conclusion re-verified in §2.4
- `git merge-base --is-ancestor 44681c76 HEAD` / `aa1af842 HEAD` — pass (both ancestors)
- `cargo +nightly check -p epics-pva-rs --lib --no-default-features -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf` — pass (exit 0, 4 warnings, enumerated in §3)
- Locate pvxs reference tree — pass (found at `/home/stevek/work/epics-modules/pvxs` @ `9348ebc`; the brief's `/Users/...` path absent, §4.0)
- pvxs server thread-priority enumeration — pass (5 sites, §4.1)
- `epicsThreadPriorityCAServerLow` numeric value from local epics-base — pass (`epicsThread.h:82` = 20)
- Per-connection teardown path read verbatim (`blocking.rs:440-457`) — pass
- CA accept/shutdown path read verbatim (`blocking.rs:176-243`) — pass
- CA UDP responder path read verbatim (`blocking.rs:446-507`) — pass
- Dead-field read-site enumeration for all 5 `SearchRequest` fields — pass (all in gated `udp.rs`, §3.2)
- `PeerRegistry::new` visibility check — pass (`peers.rs:205` is `pub`, brief's wording corrected)
- `server/mod.rs` gate audit for `native_source` — pass (un-gated, `:10`)
- `epics-pva-rs` `[[bin]]` audit — pass (6 bins, all `required-features = ["client"]`)
- Re-verification of all 7 `blocking.rs` anchors against the dirty working tree — pass (§2.4 table; §2's conclusion survives, its premise does not)

**Failed:** none.

**UNFIXED:**

- **Doc/code disagreement at `search.rs:34-37`.** The comment claims the TCP
  SEARCH path consults `protocols`; the RTEMS compiler reports `protocols`
  never read, and `tcp.rs:5866-5913` confirms it is not consulted. Not fixed —
  this task is read-only. Whoever lands stage D should decide whether TCP
  *should* gate on `protocols` (a behaviour change) or the comment is simply
  stale (a doc fix).
- **item-7 decision 5 (`43fd50bd`) carries the wrong priority number.** It
  proposes `CaServerLow`/19; upstream parity is 18 (TCP) / 16 (UDP) per §4.1.
  Not corrected — that doc lives on `main`, outside this read-only scope.
- **W1 (`fetch_update` deprecation, `tcp.rs:1404`)** is pre-existing and
  unrelated to item 7; left as the expected 1-warning residue.
- **This doc is one commit stale by construction.** The in-flight stage-4 work
  in the working tree (§2.4) is uncommitted, so its final shape — parameter
  list, `ConnRegistry` API, line numbers — can still move before it lands.
  §1.1 C1/C5 and §5.2 item 1 must be re-checked against the actual commit
  before stage C is written. I did not read the uncommitted work beyond the
  anchors needed to re-verify my own claims, because it is another panel's
  in-progress change.

**Fixed:** none — no source file was edited, no commit was made, as instructed.
