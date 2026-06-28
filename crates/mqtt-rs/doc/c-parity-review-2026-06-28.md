# mqtt-rs — C-parity review (2026-06-28)

Codex-style C-parity audit of `crates/mqtt-rs/src` against the upstream EPICS
**mqtt** module (André Favoto, GPL-3.0) at
`/Users/stevek/codes/epics-modules/mqtt/mqttSup/src`:

- `drvMqtt.cpp/.h` — the Autoparam asyn driver (topic parsing, payload
  encode/decode, record I/O, callbacks).
- `mqttClient.cpp/.h` — the Eclipse Paho async-client wrapper (connect /
  subscribe / publish / reconnect).
- `json/json.hpp` — bundled nlohmann JSON (field extraction, `dump()`).
- `drvMqtt.dbd`, `mqttExampleApp/Db/example.db`, `testApp/Db/mqttTest.db`.

Round 1 (round id `01KW6ZZJ`) fanned out to 4 opus reviewers by category with
carved numbering ranges. Read-only sweep; this doc is the inventory. Fixes are a
separate phase (per-finding commits), each marked `cleared` here as it lands.

Reviewer principle 5 governs severity: where the Rust port intentionally declines
to reproduce a C **bug** (the standing steer: "find divergences but do not copy
C's bugs"), that is recorded as an **intentional-divergence aside**, not a
finding.

| Category | Rust | C reference | Range | Findings |
|---|---|---|---|---|
| 1 Client & connection lifecycle | `config.rs`, `event_loop.rs`, `driver.rs` (connect/callbacks) | `mqttClient.cpp/.h`, `drvMqtt.cpp` callbacks | MQ1–MQ15 | MQ1–MQ5 |
| 2 Address / topic parsing & drvUser | `address.rs` | `drvMqtt.cpp` `parseDeviceAddress`/`isValidTopicName`/`supportedTopicTypes` | MQ16–MQ30 | MQ16–MQ19 |
| 3 Payload encode/decode & data-types | `payload.rs` | `drvMqtt.cpp` `*Write`/`onMessageCb`/`is*`/`checkAndParse*Array`/`findJsonField` | MQ31–MQ45 | MQ31–MQ41 |
| 4 Records / IOC / registration | `ioc.rs`, `driver.rs` | `drvMqtt.cpp` ctor + dbd + Db templates | MQ46–MQ60 | MQ46–MQ51 |

**Scope exclusion:** `crates/mqtt-rs/src/z2m.rs` (zigbee2mqtt) has **no C
counterpart** and was not reviewed against C. The `normalize_on_off` /
ON-OFF feature is z2m-only and disabled on every C-comparable path
(`address.rs` never sets `normalize_on_off`).

**Reference gap:** the **Autoparam** base class (`Autoparam::Driver`,
`autoparamDriver.h`/`autoparamHandler.h`) that `drvMqtt.cpp` derives from is a
separate EPICS module and is **not present anywhere under `/Users/stevek/codes`**
(searched the `asyn` checkout and the whole tree). This blocks a definitive
ruling on MQ51 (write-side cache-update parity) and MQ19 (whether autoparam trims
`arguments`). Those two are flagged as verification-blocked, not asserted.

---

## Verified MATCHING (no finding — read both sides)

The behavioural core is faithful where it matters most:

- **QoS / keepAlive / cleanStart defaults** — `config.rs:5-6,42-52` ↔
  `mqttClient.h:16-18`: QoS default 1, keepAlive 20 s, cleanStart true.
- **Publish defaults** — retained=false, qos=configured: `driver.rs:128-131` ↔
  `mqttClient.cpp:70,74-75`.
- **Re-subscribe on every (re)connect** — driven on ConnAck: `event_loop.rs:74-87`
  ↔ `mqttClient.cpp:110-118` (`AUTO_RECONNECT_REASON`), `drvMqtt.cpp:193-214`.
- **Supported type set** — exactly 6 types × 2 formats: `address.rs:169-199` ↔
  `drvMqtt.cpp:24-37` (modulo case, MQ17).
- **FLAT topic = whole remainder; JSON topic/field split on first whitespace** —
  `address.rs:81,105-116` ↔ `drvMqtt.cpp:65,75,86,92`.
- **`isValidTopicName`** (reject empty + `#`/`+`; jsonField not wildcard-checked) —
  `address.rs:201-211` ↔ `drvMqtt.cpp:366-373`.
- **Address equality / subscription dedup** — registry key + topic_map ↔
  `operator==` (`drvMqtt.cpp:42-52`).
- **Boolean shortcut** ("true"→1/"false"→0, INT+DIGITAL only, before numeric
  parse) — `payload.rs:186-192` ↔ `drvMqtt.cpp:388-390`.
- **`findJsonField` recursion** (whole-key, first-match, **sorted** key order) —
  `payload.rs:296-332` (explicit sort) ↔ `drvMqtt.cpp:340-359`.
- **Digital write mask** (reject partial-mask write on undefined value;
  read-modify-write merge; full mask bypasses read) — `driver.rs:185-217` ↔
  `drvMqtt.cpp:600-625`.
- **Scalar INT decode** (boolean shortcut, no whitespace skip, `+`/`-` accepted,
  base-10, overflow rejected) — `payload.rs:202-210` ↔ `drvMqtt.cpp:278-284`.
- **Scalar FLOAT decode** (`trim_start` mirrors `strtof` leading-ws skip; trailing
  garbage rejected) — `payload.rs:211-222` ↔ `drvMqtt.cpp:285-287`.
- **Float-array encode** (`%g` 6-sig, fixed/sci by exponent, trailing-zero strip,
  C-style ≥2-digit exponent, signed zero, lowercase nan/inf) —
  `payload.rs:92-128` ↔ `drvMqtt.cpp:682-687`.
- **Octet NUL-truncation** (value terminates at first NUL on store + publish) —
  `payload.rs:34-39` ↔ `drvMqtt.cpp:299,716`.
- **dbd registration surface** — `registrar` only, no device-support/record-type
  entries; both rely on standard asyn device support — `ioc.rs:275-282` ↔
  `drvMqtt.dbd`.
- **qos omitted-arg iocsh zero-fill** → QoS 0 — `ioc.rs:132-137` ↔
  `drvMqtt.cpp:745,764`.

---

## Open Findings

### MQ1 — Write while disconnected returns asynSuccess (no connection gate)
- **Severity:** DEFECT
- Rust: `driver.rs:116-135` (`publish_value`) + write handlers `driver.rs:159-240`;
  buffered on the unbounded `publish_tx` (`event_loop.rs:134-152`)
- C: `mqttClient.cpp:70-72` (`publish` throws when `!is_connected()`), caught
  `drvMqtt.cpp:590-595/632-637` → `status = asynError`
- Impact: C fails the output record (WRITE/INVALID alarm) when the broker is down.
  Rust pushes onto an **unbounded** channel and every write returns `Ok`, so a
  write while disconnected silently "succeeds" (no alarm) and the value is buffered
  (channel grows without bound). Missing connection gate + missing error routing.

### MQ2 — Subscription/dispatch set = all declared topics, not just I/O-Intr-bound records
- **Severity:** CONCERN — **asyn-rs framework** (folds in MQ50)
- Rust: `driver.rs:104-112`, `event_loop.rs:74-87,154-161,177-184`
- C: `drvMqtt.cpp:123` (`setAutoInterrupts(false)`), `:207-213` (subscribe only
  `getInterruptVariables()`), `:250-255` (dispatch only interrupt vars)
- Impact: C subscribes/dispatches strictly to topics referenced by an
  `SCAN="I/O Intr"` input record; Rust subscribes by topic *registration* and
  fans every inbound message to every param on the topic. An output-only topic is
  subscribed (and its cache updated) under Rust but not C; a Passive-scanned input
  never updates under C but does under Rust. Masked in the bundled templates (every
  topic there has an I/O-Intr input). Requires asyn-rs interrupt-variable tracking
  to close → see "Structural / framework" below.

### MQ3 — Reconnect/retry policy: fixed 1 s vs Paho automatic_reconnect backoff
- **Severity:** NOTE
- Rust: `event_loop.rs:97-111` (flat 1 s retry, no backoff/ceiling)
- C: `mqttClient.cpp:23` (`.automatic_reconnect(true)`, Paho exponential backoff)
- Impact: More reconnect churn / log noise against a persistently-down broker;
  both eventually reconnect and re-subscribe. Functional outcome equivalent.

### MQ4 — No graceful MQTT DISCONNECT on shutdown
- **Severity:** NOTE
- Rust: `event_loop.rs:53-114` (loop never breaks; no DISCONNECT sent)
- C: `mqttClient.cpp:51-55`, `drvMqtt.cpp:182-184` (destructor `disconnect()->wait()`)
- Impact: C sends a clean DISCONNECT on teardown; Rust relies on TCP close /
  keepalive timeout. Neither sets a will; rarely exercised (IOCs run to process
  exit).

### MQ5 — `MqttConfig` library default `client_id` diverges from C
- **Severity:** NOTE
- Rust: `config.rs:47` (`client_id = "epics-mqtt"`)
- C: `mqttClient.h:15` (`clientId = "MqttClient"`)
- Impact: Default differs, but `clientId` is a required `mqttDriverConfigure` arg
  (`ioc.rs:145-157`), so only direct library use sees it. Host/port defaults match.

### MQ16 — JSON topic/field separator: C uses a literal space, Rust any Unicode whitespace
- **Severity:** NOTE (Rust more permissive)
- Rust: `address.rs:105` (`split_once(char::is_whitespace)`)
- C: `drvMqtt.cpp:75` (`find(' ')`, ASCII 0x20 only)
- Impact: `JSON:FLOAT topic\tfield` is rejected by C, accepted by Rust. Rust is the
  more-permissive side; hand-written drvInfo uses single spaces. Keep.

### MQ17 — FORMAT:TYPE accepted case-insensitively; C requires exact-case
- **Severity:** CONCERN
- Rust: `address.rs:174,184` (`to_ascii_uppercase()`); registry keyed by canonical
  `to_drv_info()`
- C: `drvMqtt.cpp:362-364` (`supportedTopicTypes.find(type)`, case-sensitive over
  the uppercase set `:24-37`)
- Impact: `flat:int` / `Json:Float` are rejected by C (record device-init fails)
  but accepted by Rust. Worse, it creates a **Rust-internal inconsistency**:
  `mqttAddTopic` canonicalises the key to uppercase, but `drv_user_create` looks up
  the record's *verbatim* (lowercase) drvInfo → `ParamNotFound`, so the record
  silently fails to bind after a false "success". C never offers the false success.

### MQ18 — Quoted-topic JSON extension can hijack C-valid topics beginning with `"`
- **Severity:** CONCERN (exotic input)
- Rust: `address.rs:84-100` (`strip_prefix('"') …`)
- C: `drvMqtt.cpp:86,366-373` (`"` is a legal topic char; no quote handling)
- Impact: The Rust quoted-topic form is an intentional extension that lifts C's
  "no spaces in a JSON topic" limitation, but it diverts any unquoted JSON drvInfo
  whose remainder begins with `"`. `JSON:INT "abc" def` → C subscribes to literal
  `"abc"`, Rust strips quotes → `abc` (silent wrong subscription). The doc claim
  that the unquoted grammar is left "intact" (`address.rs:34-35`) is inaccurate for
  `"`-leading topics. Topics literally beginning with `"` are extremely rare.

### MQ19 — Rust trims leading whitespace off `arguments`; C parses verbatim (autoparam-dependent)
- **Severity:** NOTE — **verification-blocked** (autoparam source absent)
- Rust: `address.rs:69` (`rest.trim_start()`)
- C: `drvMqtt.cpp:65,86,92` (substr on raw `arguments`)
- Impact: For multi-space input C keeps a space-prefixed topic (FLAT) or rejects
  (JSON empty topic) **if** autoparam hands `parseDeviceAddress` a leading space.
  Whether it does is unverifiable without the autoparam source. Flagged as an
  assumption, not asserted.

### MQ31 — Array decode pre-trims the whole payload; C's state machine does not
- **Severity:** CONCERN (Rust more robust)
- Rust: `payload.rs:195,237,242` (`raw.trim()`)
- C: `drvMqtt.cpp:446-485,531-565`
- Impact: Rust accepts `"1,2,3\n"` / `"1,2,3 "` / `" [1,2]"`; C rejects a trailing
  separator/space. A line-buffered broker that appends `\n` updates under Rust,
  is dropped under C. Rust is the more-robust side (real MQTT lines carry `\n`).

### MQ32 — Array separator: "comma-if-any-else-space" split vs C's first-seen locked separator
- **Severity:** CONCERN (one half is a Rust regression)
- Rust: `payload.rs:338,351` (`if s.contains(',') { ',' } else { ' ' }` then `split`)
- C: `drvMqtt.cpp:469-483` (separator locked to the first seen)
- Impact: `"1  2"` (double space): C accepts `[1,2]` (loop-top space-skip), Rust
  yields an empty split element → reject (**Rust worse**). `"1 ,2"`: C rejects, Rust
  accepts (Rust more lenient). The double-space rejection is the regression to fix
  (collapse empty elements → Rust ≥ C); the `"1 ,2"` leniency is kept.

### MQ33 — Bracket handling: Rust strips `[`/`]` greedily/independently; C requires one balanced pair
- **Severity:** CONCERN (Rust more lenient)
- Rust: `payload.rs:337,350` (`trim_start_matches('[').trim_end_matches(']')`)
- C: `drvMqtt.cpp:439-443,522-526`
- Impact: `"[1,2,3"` / `"1,2,3]"` / `"[[1,2]]"` are rejected by C, accepted by Rust.
  Malformed-bracket payloads update under Rust. Low harm; Rust more lenient. Keep.

### MQ34 — DIGITAL accepts a leading `+` that C rejects
- **Severity:** CONCERN
- Rust: `payload.rs:227` (`raw.parse::<u32>()` accepts a leading `+`)
- C: `drvMqtt.cpp:294` (`isInteger(val, false)` — `isSigned=false` ⇒ `+`/`-` is a
  non-digit ⇒ rejected)
- Impact: `"+5"` on a DIGITAL topic: C keeps the prior value ("Invalid unsigned
  integer"), Rust decodes 5. INT matches (default `isSigned=true`).

### MQ35 — DIGITAL large value: C truncates via `stoul`→cast wrap; Rust rejects
- **Severity:** NOTE — **intentional-divergence aside** (declines C UB)
- Rust: `payload.rs:227` (`parse::<u32>()` errors on overflow)
- C: `drvMqtt.cpp:295` (`static_cast<epicsUInt32>(std::stoul(val))`)
- Impact: `"4294967296"`: C wraps mod 2³² → stores `0`; Rust rejects (record
  unchanged). Rust correctly declines C's silent truncation. Keep.

### MQ36 — FLOAT scalar magnitude overflow: C rejects (`stod` throws), Rust stores ±inf
- **Severity:** NOTE
- Rust: `payload.rs:217-220` (`parse::<f64>()` → `Ok(inf)`)
- C: `drvMqtt.cpp:286-287` (`stod` throws `out_of_range`)
- Impact: `"1e400"`: C keeps prior value, Rust stores `+inf`. Extreme-magnitude
  boundary only; within range both accept. The FLOATARRAY path matches (both → inf).

### MQ37 — FLOAT decode accepts the full `strtod` grammar (hex floats, `nan(seq)`) that Rust rejects
- **Severity:** NOTE — **intentional-divergence aside** (Rust stricter; harmless)
- Rust: `payload.rs:217-220,353-356`
- C: `drvMqtt.cpp:399,538` (`strtof`/`strtod`)
- Impact: C accepts C99 hex floats (`"0x1.8p3"`) and `"nan(123)"`; Rust rejects
  them (accepts `inf`/`nan`). No real MQTT broker emits hex-float payloads. Keep.

### MQ38 — Non-UTF-8 payload dropped for ALL subscribers; C stores raw bytes on STRING topics
- **Severity:** CONCERN — partly **asyn-rs framework** (octet param is String)
- Rust: `event_loop.rs:169-175` (`from_utf8(payload)` → `return`, dropping the
  whole message)
- C: `drvMqtt.cpp:248,299` (`val = payload`; `setStringParam(val.c_str())`)
- Impact: A non-UTF-8 payload drops the message for **every** subscriber on the
  topic, not just the STRING one. The local bug is the *global* drop; per-subscriber
  best-effort handling (numeric subscribers fail to parse anyway, STRING stores the
  bytes) is the fixable part. Full raw-byte cache fidelity is asyn-rs-blocked
  (`ParamSetValue::Octet` is a `String`, not `Vec<u8>`).

### MQ39 — Outbound octet uses `from_utf8_lossy` (U+FFFD); C publishes raw bytes up to the first NUL
- **Severity:** CONCERN
- Rust: `driver.rs:178-179` (`String::from_utf8_lossy(data)`)
- C: `drvMqtt.cpp:714-716` + `mqttClient.cpp:75` (publish raw bytes, NUL-terminated)
- Impact: A binary octet / waveform-CHAR write is lossily re-encoded to U+FFFD
  before publishing, corrupting the wire payload. NUL-truncation itself is faithful;
  only non-UTF-8 byte preservation diverges. Fixable mqtt-rs-locally (carry the raw
  bytes, truncated at the first NUL byte, to the publish path).

### MQ40 — JSON object/array carrier serialized in document order (serde); C `dump()` is sorted-key
- **Severity:** CONCERN
- Rust: `payload.rs:271-274` (`other.to_string()`)
- C: `drvMqtt.cpp:265` (`fieldAddr->dump()`, nlohmann `std::map` ⇒ sorted keys)
- Impact: A JSON field resolving to an object/array on a STRING topic stores
  `{"a":2,"b":1}` under C (sorted) but `{"b":1,"a":2}` under Rust (doc order, the
  IOC build links serde `preserve_order`) — different bytes. The field *search*
  order is already matched (explicit sort, `payload.rs:310-311`); only the carrier
  *serialization* diverges. Fixable by sorting keys when serializing a composite
  carrier.

### MQ41 — Scalar FLOAT encode of non-finite: `"%f"` spelling differs (`NaN` vs `nan`)
- **Severity:** NOTE
- Rust: `payload.rs:66` (`format!("{v:.6}")` → `"NaN"`)
- C: `drvMqtt.cpp:651` (`std::to_string`, `"%f"` → `"nan"`)
- Impact: A NaN VAL publishes `"NaN"` (Rust) vs `"nan"` (C). `inf`/`-inf` match;
  the FLOATARRAY path already emits lowercase (`format_ostream_double`). Only the
  scalar-float NaN spelling diverges.

### MQ46 — Driver registers `can_block: true`; C explicitly sets `setBlocking(false)`
- **Severity:** DEFECT
- Rust: `driver.rs:58-61` (`PortFlags { can_block: true, .. }`)
- C: `drvMqtt.cpp:122` (`.setBlocking(false)`)
- Impact: `can_block` ⇒ `ASYN_CANBLOCK`, which defers `performIO` two-phase
  (PACT=1, async completion) where a non-blocking port completes inline. C declared
  the port non-blocking, and the Rust write path *is* non-blocking (only an mpsc
  `send`), so `true` is opposite to C: `ao`/`stringout`/`aao` writes go async (PACT
  transient, FLNK/caput completion deferred) where C completes them synchronously.

### MQ47 — Record-binding contract inverted: explicit `mqttAddTopic` pre-reg vs C autoparam lazy creation
- **Severity:** CONCERN — **asyn-rs framework**
- Rust: `ioc.rs:14-40,45-80`, `driver.rs:47-91,147-157` (`drv_user_create(&self)`
  only looks up)
- C: `drvMqtt.cpp:54-107` (Autoparam creates the param on demand from the record's
  resolved `INP/OUT`); `st.cmd` runs only `mqttDriverConfigure` + `dbLoadRecords`
- Impact: In C the `INP/OUT` link *is* the topic declaration (driver born empty,
  records auto-create params). In Rust every drvInfo must be pre-registered via the
  **non-C** `mqttAddTopic` command before `mqttDriverConfigure`; an unregistered
  record fails to bind (`ParamNotFound`, only an `eprintln`). The unmodified C
  `st.cmd` / `example.db` / `mqttTest.db` will not bind without inserting
  `mqttAddTopic` lines. Root cause is asyn-rs: `drv_user_create(&self)` cannot
  create params on demand. Same framework gap as **modbus R34/R52/R54**.

### MQ48 — `mqttDriverConfigure` arg signature: 5 args (qos optional + extra `connPvName`) vs C 4
- **Severity:** NOTE
- Rust: `ioc.rs:87-123` (`portName, brokerUrl, clientId, qos?, connPvName?`)
- C: `drvMqtt.cpp:737-765` (4 fixed args)
- Impact: Command name matches; omitted-qos path is C-faithful (zero-fill → QoS 0).
  The appended optional 5th arg `connPvName` is a Rust-only extension (MQ49); 4-arg
  callers are unaffected.

### MQ49 — Connection-status param/record is a Rust-only extension
- **Severity:** NOTE — **intentional divergence / aside**
- Rust: `driver.rs:29,68-71` (`_MQTT_CONNECTED` always created),
  `ioc.rs:159-162,201-249` (optional synthesized `bi` record)
- C: none (connection state only `asynPrint`-logged)
- Impact: One extra always-present asyn param + an optional injected record.
  Additive (no C behaviour violated); recorded rather than silently carried.

### MQ50 — (DUPLICATE of MQ2) subscribe/dispatch keyed on all topics, not I/O-Intr vars
- Folded into **MQ2**. Same divergence, found independently by the IOC reviewer.

### MQ51 — Write handlers update the param cache + fire callbacks; C `*Write` never `setParam`
- **Severity:** CONCERN — **verification-blocked** (autoparam source absent)
- Rust: `driver.rs:159-240` (each `write_*` calls `set_*` then `call_param_callbacks`)
- C: `drvMqtt.cpp:573-733` (`*Write` publish only; none call `setParam`), with
  `setAutoInterrupts(false)` (`:123`)
- Impact: Whether Rust's eager write-side cache update + callback matches C depends
  on `Autoparam::Driver`'s post-handler commit/post behaviour, and the autoparam
  source is **not present under `/Users/stevek/codes`**. If autoparam commits +
  posts after a successful write, Rust matches; if not, Rust posts an
  output-readback C does not. Left as an open verification item pending the
  autoparam source path.

---

## Disposition summary

- **Fix (Rust wrong / C right, mqtt-rs-local):** MQ1, MQ17, MQ34, MQ41, MQ46,
  MQ39, MQ40, and the double-space half of MQ32.
- **Intentional-divergence aside (Rust declines a C bug / is more robust — keep):**
  MQ31, MQ33, MQ35, MQ37, the `"1 ,2"` half of MQ32, the quoted-topic extension of
  MQ18.
- **NOTE (low-priority documented divergence):** MQ3, MQ4, MQ5, MQ16, MQ36, MQ48,
  MQ49.
- **Structural / framework — asyn-rs contract change, design sign-off:** MQ2
  (=MQ50) interrupt-variable subscription set; MQ47 on-demand param creation
  (`drv_user_create(&self)`); the cache-side of MQ38 (octet param is `String`).
  These are the **same asyn-rs gap** flagged by **modbus R34/R52/R54**.
- **Verification-blocked (autoparam source absent):** MQ51, the definitive ruling
  on MQ19.

Wire/observable path is faithful for the common case (numeric/text payloads,
QoS/retain/keepAlive, topic grammar, JSON field search, digital mask, NUL
truncation). The defects cluster in connection-state gating (MQ1), the
record-processing model (MQ46), and accept/reject + byte-encoding boundaries.
