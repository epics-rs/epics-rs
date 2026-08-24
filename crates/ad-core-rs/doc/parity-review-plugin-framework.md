# ad-core-rs Plugin Framework Review vs epics-modules/ADCore

> **STATUS (2026-06-28): round-1 RAW review — SUPERSEDED, mostly fixed.**
> Findings below are stated present-tense but are *not* the live open list.
> They were triaged into the dispositioned **ADC-1..ADC-12** inventory in
> `doc/c-parity-review-2026-06-15.md` (round 2) — all **Fixed / signoff /
> N-A** — and closed via the ad-core-rs fix stream. **Spot-verified FIXED in
> current source** (do not re-report): the CRITICAL **B1** reliable-send is now
> drop-on-full `try_send` by default (`channel.rs:179`, C++ `trySend`), with
> never-drop back-pressure demoted to an opt-in; **G1** DroppedArrays now
> increments on a full-queue drop (`channel.rs:184`); **G7** the MaxByteRate
> token-bucket Throttler is ported (`plugin/throttler.rs`); also G2 QueueFree
> (807b07b6), G6 NDArrayAddr (3bf747fc), G8/ADC-5 NDDimensions (600adb66),
> B5/ADC-4 throttle-not-dropped (a1cb7e0c), G5/R2 ProcessPlugin cache
> (e1a4bbc4), G10 live attribute sources (b114c2df). The one genuine
> **keep-Rust** item is **G4 NumThreads** multi-worker: the port runs a single
> per-plugin tokio worker on purpose (array ordering is trivially correct), so
> NumThreads/MaxThreads stay inert by design. Do NOT treat a present-tense
> finding here as open without checking the ADC-N inventory AND current source.
> Kept verbatim for audit provenance.

Scope: `src/plugin/{mod,channel,runtime,wiring,params,file_base,file_controller}.rs`
and `src/ioc/{mod,driver_context,helpers,plugin_manager}.rs` vs
`ADApp/pluginSrc/{NDPluginDriver,NDPluginFile,throttler}.{cpp,h}`.

The Rust port deliberately re-architects the threading model: instead of the
C++ `epicsMessageQueue` + N worker threads dispatched by `driverCallback`, it
uses a single `tokio::mpsc` channel + one per-plugin `current_thread` runtime
(`plugin_data_loop`). Many findings below stem from that re-architecture
silently dropping C++ accounting/semantics rather than re-implementing them.

---

## Feature Gaps

### G1. DroppedArrays counter is never incremented (HIGH)
C++ `driverCallback` (NDPluginDriver.cpp:430-444): when `pToThreadMsgQ_->trySend`
fails because the input queue is full, it increments `NDPluginDriverDroppedArrays`
and releases the array. The Rust port has the `DROPPED_ARRAYS` param
(`params.rs:39`) and initializes it to 0 (`runtime.rs:766`) but **nothing ever
increments it**. `NDArraySender::publish` (channel.rs:118-149) uses
`tx.send(msg).await` — a *reliable* enqueue that applies backpressure to the
producer and never drops. So with a full queue the upstream acquisition task
yields/blocks instead of dropping + counting. `rg "dropped_arrays" src/plugin`
shows the field is write-once-zero only. Consequence: the DroppedArrays_RBV PV
is permanently 0; operators lose the primary "plugin can't keep up" signal, and
a slow plugin now backpressures the detector driver instead of dropping frames
(a behavioral change — see also B1).

### G2. QueueFree / QueueSize counters not updated at runtime (HIGH)
C++ updates `NDPluginDriverQueueFree = queueSize - pToThreadMsgQ_->pending()`
on every callback and every processed array (NDPluginDriver.cpp:431-432,
512-513). Rust sets `QUEUE_SIZE` once at construction (`runtime.rs:765`) and
`QUEUE_FREE` (`queue_use`) once to 0 (`runtime.rs:767`), then never touches
either. `tokio::mpsc` exposes `capacity()`/`max_capacity()` but the data loop
never reads them. Consequence: QueueUse_RBV / QueueFree_RBV are dead PVs;
operators cannot see queue depth. Also note the param is named `QUEUE_FREE`
but the struct field is `queue_use` (params.rs:16,40) — a naming inconsistency
that suggests the semantics were never reconciled.

> **G2 addendum (2026-08-23).** 807b07b6 added the runtime update but left the
> constructor writing `QUEUE_FREE = 0`, so an idle plugin published a full
> queue from `iocInit` until its first array arrived. The constructor block is
> now the whole of C `NDPluginDriver.cpp:152-160`, which also closes
> `NDARRAY_ADDR`, `DROPPED_OUTPUT_ARRAYS`, `MAX_THREADS` and `NUM_THREADS` —
> none of which were in G2's or G6's scope, and `MaxThreads_RBV` had no writer
> on either side, so it read UDF/INVALID for the life of the IOC. G4 stays a
> deliberate keep-Rust deviation: the port still runs one worker, and
> `MAX_THREADS`/`NUM_THREADS` now *publish* that 1 instead of publishing
> nothing.

### G3. Compression-aware input gating missing (MEDIUM)
C++ `driverCallback` (NDPluginDriver.cpp:383-394): if the plugin is not
`compressionAware_` and the array has a non-empty `codec`, the array is dropped
and `DroppedArrays` incremented. The Rust port has no `compression_aware`
concept anywhere in the plugin framework; a compressed `NDArray` is passed
straight into `process_array`. Consequence: a non-compression-aware plugin
(e.g. an ROI/stats plugin) will operate on compressed bytes as if raw, producing
garbage instead of cleanly dropping.

### G4. NumThreads / MaxThreads multi-worker model not implemented (MEDIUM)
C++ creates `numThreads` worker threads sharing one input queue
(`createCallbackThreads`, NDPluginDriver.cpp:940-1011) and recreates them when
`NumThreads`/`QueueSize` change (`writeInt32`, lines 730-733). Rust always spawns
exactly one `plugin-data-{port}` thread (`runtime.rs:1155`). `MAX_THREADS` /
`NUM_THREADS` params exist (params.rs:19-20) but are never read, never
validated, and writing them does nothing — there is no `createCallbackThreads`
equivalent. Consequence: plugins that benefit from parallel array processing
(e.g. codec/HDF5) are single-threaded; the NumThreads PV is inert. (For
order-sensitive plugins single-threading is arguably safer, but the C++ API
contract is silently broken.)

### G5. ProcessPlugin (re-process last array) not implemented (MEDIUM)
C++ `writeInt32` for `NDPluginDriverProcessPlugin` (NDPluginDriver.cpp:739-746)
re-injects `pPrevInputArray_` through `driverCallback`. Rust registers the
`PROCESS_PLUGIN` param (params.rs:54) but the data loop (`plugin_data_loop`,
runtime.rs:1241-1336) has no branch for `process_plugin` reason and never caches
the last *input* array (`pPrevInputArray_` has no analogue). Consequence:
writing ProcessPlugin does nothing; the common "tweak a parameter then
reprocess the last frame" workflow is unavailable.

### G6. NDArrayAddr connection / multi-address source not implemented (MEDIUM)
C++ `connectToArrayPort` connects to `(NDArrayPort, NDArrayAddr)` and
`writeInt32` for `NDPluginDriverArrayAddr` triggers a reconnect
(NDPluginDriver.cpp:724-728). Rust wiring (`WiringRegistry`) is keyed by port
*name* only; `NDARRAY_ADDR` param exists (params.rs:16) but is never read and
writing it does nothing. Consequence: plugins cannot select a non-zero source
address of a multi-address driver.

### G7. MaxByteRate / Throttler output throttling not implemented (HIGH)
C++ `endProcessCallbacks` calls `throttled(pArrayOut)` (NDPluginDriver.cpp:287-294)
which uses a token-bucket `Throttler` (throttler.cpp); arrays exceeding
`MaxByteRate` are dropped and counted into `DroppedOutputArrays`.
`writeFloat64` resets the throttler (NDPluginDriver.cpp:788-790). The Rust port
registers `MAX_BYTE_RATE` (params.rs:29) but there is **no Throttler type, no
`throttled()` call, and `max_byte_rate` is never read** in the data loop.
Consequence: output byte-rate limiting is completely absent; the MaxByteRate PV
is inert; `DroppedOutputArrays` only counts sort-buffer overflow, not throttle
drops.

### G8. NDDimensions int32-array callback on dimension change missing (MEDIUM)
C++ `beginProcessCallbacks` (NDPluginDriver.cpp:220-231) tracks `dimsPrev_` and
fires `doCallbacksInt32Array(dimsPrev_, ND_ARRAY_MAX_DIMS, NDDimensions, 0)` when
dimensions change; `readInt32Array` serves `NDDimensions` from `dimsPrev_`
(lines 862-866). Rust `build_publish_batch` sets `n_dimensions`, `array_size_x/y/z`
scalars but never maintains a `dimsPrev_` array, never registers/serves an
`NDDimensions` waveform, and never emits an int32-array interrupt on dim change.
Consequence: the `Dimensions` waveform PV (used by CSS/areaDetector GUIs) is not
populated by plugins.

### G9. NDPluginFile attribute-driven filename/destination logic missing (MEDIUM)
C++ NDPluginFile implements a family of attribute hooks:
`attrIsProcessingRequired` (FILEPLUGIN_DESTINATION — routes a frame to a
specific file plugin), `attrFileNameSet`/`attrFileNameCheck` (FilePluginFileName
/ FilePluginFileNumber override path and force file reopen mid-stream),
`attrFileCloseCheck` (FilePluginClose attribute forces a close). The Rust
`FilePluginController` / `NDPluginFileBase` implement none of these — only the
`DriverFileName` deletion attribute is honored (file_base.rs:216, 249).
Consequence: multi-file-plugin destination routing and HDF5/Nexus per-frame file
switching driven by detector attributes do not work.

### G10. NDPluginFile does not increment ArrayCounter only on saved frames (LOW)
C++ `processCallbacks` (NDPluginFile.cpp:730-789) deliberately does *not* let the
base bump `NDArrayCounter` for every callback — it saves the counter, lets the
base increment, then only re-applies `arrayCounter++` for frames it actually
saved. The Rust `FilePluginController::process_array` returns through the generic
`build_publish_batch` which increments `array_counter` for *every* array
regardless of whether it was saved (runtime.rs:456-458; file plugins go through
the same path). Consequence: ArrayCounter_RBV on a file plugin counts callbacks,
not saved frames — minor parity drift, affects ArrayRate display.

### G11. `EXECUTION_TIME` is the only timing param; no `ArrayRate` source-of-truth,
`callParamCallbacks` addr semantics differ (LOW)
C++ does per-address `callParamCallbacks(addr)`. Rust `io_write_*`
(runtime.rs:962-991) calls `call_param_callbacks(addr)` correctly, but the
data-thread path flushes via `set_params_and_notify` per addr bucket
(`ParamBatch::flush`, runtime.rs:712-723) — functionally equivalent. Noted only
because the `extra` HashMap path is untested by any unit test.

### G12. File-plugin `isFrameValid` uses dims+dtype but not bytesPerElement edge,
and Single/Capture modes skip validation (LOW)
C++ `isFrameValid` is consulted in Capture mode (`numCaptured < numCapture &&
isFrameValid`) and Stream mode. Rust validates frame dims/dtype only in Stream
mode (file_controller.rs:144-159); Capture mode (`file_base.rs:227-233`,
`file_controller.rs:118-141`) appends every array with no validation.
Consequence: a Capture-mode file with frames of varying size will buffer
mismatched arrays and hand them to a multi-array writer that C++ would have
rejected.

---

## Bugs

### CRITICAL

#### B1. Reliable-send model converts frame drops into detector backpressure
`NDArraySender::publish` (channel.rs:140 `tx.send(msg).await`) is reliable: when
the downstream queue is full the producing future suspends until space frees.
The C++ design is explicitly drop-on-full at the *input* queue
(`trySend`, NDPluginDriver.cpp:430). The doc comment in channel.rs:90-102
acknowledges this as intentional ("Neither mode drops arrays"). But the
consequence is a correctness regression: a single slow plugin (e.g. a stalled
HDF5 writer) now applies backpressure all the way up the pipeline to the
detector acquisition loop, which in C++ would instead keep running and drop
frames at that plugin. This changes the fundamental data-flow contract of the
plugin framework and can stall acquisition. At minimum the input side should
offer a bounded `try_send` path that drops + increments DroppedArrays to match
C++ (ties to G1). Severity CRITICAL because it can deadlock/stall a live
detector, not just misreport a counter.

### HIGH

#### B2. Sort mode buffers ALL arrays, ignoring the in-order fast path
C++ `endProcessCallbacks` (NDPluginDriver.cpp:295-328): even with
`callbacksSorted` set, an array whose `uniqueId == prevUniqueId_(+1)` is emitted
*immediately* via `doCallbacksGenericPointer`; only out-of-order arrays go into
`sortedNDArrayList_`. The Rust `process_and_publish` (runtime.rs:364-383)
unconditionally inserts every output array into `sort_buffer` whenever
`sort_mode != 0`, then relies on the periodic `sort_flush_interval` tick to
drain. Consequence: with sort mode on, *every* array is delayed by up to
`sort_time` seconds even when frames arrive perfectly in order — a large,
unnecessary latency penalty. The integration test `test_sort_mode_runtime_integration`
(runtime.rs:1866) actually depends on this wrong behavior (it asserts arrays are
NOT received until sort mode is disabled).

#### B3. Sort buffer release is purely time-bucketed; C++ uses order + deadline
C++ `sortingTask` (NDPluginDriver.cpp:619-670) drains the multiset element by
element: it emits the head while `(!firstOutputArray_ && orderOK) || deltaTime >
sortTime` — i.e. it releases as soon as the next expected uniqueId is present,
and uses `sortTime` only as a per-element staleness deadline. Rust
`SortBuffer::drain_all` (runtime.rs:300-306) is all-or-nothing: on every flush
tick it dumps the *entire* buffer regardless of contiguity or per-array
insertion age. Consequence: (a) no early release of an already-contiguous run
between ticks (extra latency); (b) a single late array does not get its own
`sortTime` grace — it is flushed with everything else on the next tick;
(c) `last_emitted_id` is set to the max key drained (runtime.rs:302-304), so a
later-arriving lower id is then mis-counted as disordered even though sort mode
"handled" it. The Rust sort implementation is a different algorithm, not a port.

#### B4. `disordered_arrays` counting diverges from C++ and double-purposes
C++ counts a disordered array at *emission* time when `!orderOK`
(NDPluginDriver.cpp:317-325, 648-657), in both the sorted and unsorted paths.
Rust only counts disorder inside `SortBuffer::insert` when `unique_id <
last_emitted_id` (runtime.rs:285-287), i.e. **only when sort mode is on**. In
unsorted mode (`sort_mode == 0`) disordered arrays are never detected or counted
at all — `build_publish_batch` has no `prevUniqueId_` tracking. Consequence:
`DisorderedArrays_RBV` is always 0 unless sort mode is enabled; the normal
out-of-order detection that C++ provides for every plugin is missing.

#### B5. `last_process_time` updated before throttle decision is committed,
plus throttled arrays are silently lost with no accounting
`SharedProcessorInner::process_and_publish` (runtime.rs:355-362): `should_throttle()`
is checked, and if true the function returns `None` and the array is dropped.
This is the `MinCallbackTime` analogue of C++ `driverCallback`
(NDPluginDriver.cpp:405). Two divergences: (1) C++ updates `lastProcessTime_`
*before* dispatch only when it decides to process (line 418); Rust sets
`last_process_time = Some(t0)` (runtime.rs:362) *after* a successful process,
which is fine — but on the throttled path it does not record anything, so timing
is consistent. The real bug is (2): a throttled array is dropped with **no
counter** — C++ at least leaves DroppedArrays semantics intact because
MinCallbackTime drops happen pre-queue; here the array was already delivered
through the reliable channel and is now silently discarded inside the data loop.
There is no DroppedArrays increment and the upstream `publish().await` already
returned success. Consequence: silent frame loss invisible to operators.

#### B6. `enable_callbacks` race: array dropped at sender, but disabling does not
release the cached input array
C++ `writeInt32` for `EnableCallbacks==0` (NDPluginDriver.cpp:712-722) releases
`pPrevInputArray_`. Rust `NDArraySender::publish` checks `enabled` and returns
early (channel.rs:119-121) — good — but the `enabled` flag is stored on the
*sender* and toggled from the data loop (runtime.rs:1245). There is a window:
the param-change message and array messages race on the same `tokio::select!`;
an array already admitted to the channel before `enabled` flips will still be
processed. More importantly, the check is on the *sender* side, so a plugin that
is wired as downstream of multiple upstreams has its `enabled` flag shared
correctly, but the C++ semantic of "unregister interrupt + drop cached input" is
only half-present (no cached input exists — see G5). Severity HIGH because
EnableCallbacks=0 is the standard way to quiesce a plugin and the quiesce is not
synchronous.

### MEDIUM

#### B7. Stream-mode `num_capture == 0` (capture-forever) handling differs
C++ Stream mode stops when `numCaptured == numCapture` (NDPluginFile.cpp:780);
with `numCapture == 0` it never auto-stops (the equality is never met for
positive numCaptured). Rust `FilePluginController::process_array` Stream branch
(file_controller.rs:162) guards with `target > 0 && num_captured >= target` —
so `num_capture == 0` also streams forever. Equivalent. **However**
`NDPluginFileBase::flush_capture` / Capture mode in `file_base.rs:230`
(`num_captured >= num_capture`) and `set_num_capture` clamps to `.max(1)`
(file_controller.rs:239). C++ Capture mode with `numCapture == 0` would buffer
indefinitely; Rust forces minimum 1, so a single frame triggers an immediate
flush. Minor behavior divergence.

#### B8. Capture-mode flush leaves `capture_active` inconsistent when auto_save off
`FilePluginController::process_array` Capture branch (file_controller.rs:118-141):
when the buffer fills and `auto_save` is false, it sets `capture_active = false`
but does **not** clear the capture buffer or reset `num_captured`. C++ keeps the
buffer until `freeCaptureBuffer` (explicit FreeCapture or next capture start).
Rust matches that — but `num_captured` (file_base.rs:49) still reflects the full
buffer and `NUM_CAPTURED` PV is pushed (line 121) before the buffer-full check;
after `capture_active=false` a subsequent `WriteFile` will `flush_capture` the
stale buffer. This is *roughly* C++ behavior, but the state machine has no single
owner: `capture_active`, `file_base.capture_buffer`, `num_captured`, and the
`CAPTURE` PV are mutated in 4 places (file_controller.rs:129,133,319,334,359)
with no invariant tying them together. Recommend a single `stop_capture()` /
`start_capture()` owner. Flagged as a latent state-machine hazard.

#### B9. Stream first-frame open is duplicated and `lazy_open` branch is dead-ish
`NDPluginFileBase::process_array` Stream arm (file_base.rs:234-246) has two
nearly identical blocks: `if !is_open && !lazy_open { open }` then
`if lazy_open && !is_open { open }`. These collapse to "open if not open" — the
`lazy_open` flag has no observable effect on *when* the file opens (both open on
first frame). C++ `doCapture` opens eagerly at capture-start for non-lazy
(`NDPluginFile.cpp:478-479`) and defers to first `writeFileBase` for lazy
(`doLazyOpen = lazyOpen && numCaptured==0`, line 333). The Rust port never opens
at capture-start, so non-lazy and lazy are identical. Consequence: `FileLazyOpen`
PV is effectively inert; eager-open error reporting (a bad path is reported at
Capture=1 in C++, not at first frame) is lost.

#### B10. `create_file_name` printf emulation mishandles `%-` and width-only specs
`NDPluginFileBase::create_file_name` (file_base.rs:88-152): the format-spec
scanner collects digits, `.`, `-` (line 96) but the `%d` formatter only honors
`width.max(precision)` zero-padding (lines 130-139). A template like `%-3d`
(left-justify) or `%5d` (space pad) is rendered as zero-padded, and `%-3d`
parses `width` from `"-3"` via `parse::<usize>()` which fails → `unwrap_or(0)` →
no padding at all. C++ uses real `epicsSnprintf`. Consequence: non-default
`FileTemplate` values that use space-padding or left-justification produce wrong
filenames. Most sites use `%3.3d` which works, so MEDIUM.

#### B11. `process_and_publish` color_mode is inferred from dims, not the array
`build_publish_batch` (runtime.rs:510): `color_mode = if dims.len() <= 2 {0} else {2}`
— Mono if ≤2 dims else RGB1. C++ `beginProcessCallbacks` reads the actual
`ColorMode` *attribute* off the NDArray (NDPluginDriver.cpp:201-203) and also
`BayerPattern`. Rust ignores attributes entirely here and never sets
`NDBayerPattern`. Consequence: a 3-D RGB2/RGB3 array is reported as RGB1; a
2-D Bayer array loses its pattern; `ColorMode_RBV`/`BayerPattern_RBV` are wrong
for anything but plain mono/RGB1.

#### B12. `array_counter` lives on `SharedProcessorInner`, not the param library;
ProcessPlugin / reset path cannot reset it
`array_counter` (runtime.rs:323) is a plain field incremented in
`build_publish_batch`. C++ keeps `NDArrayCounter` in the param library and
`beginProcessCallbacks` does `getIntegerParam`/`setIntegerParam`
(NDPluginDriver.cpp:206-208) — so writing the ArrayCounter PV resets it. In Rust
the field is write-only-from-data-thread; a control-plane write of
`ARRAY_COUNTER` updates the param library but the next array overwrites it from
the stale internal counter. Consequence: ArrayCounter is not resettable and can
desync from the PV.

#### B13. `wire_downstream` / `connect_downstream` bypass the WiringRegistry
`wire_downstream` (runtime.rs:1354-1356) and `GenericDriverContext::connect_downstream`
(driver_context.rs:42-44) push a sender directly into an `NDArrayOutput` without
recording the edge in `WiringRegistry`. Later a `rewire_by_name` for that sender
will `take()` it from the registry-tracked output — but if the initial edge was
added via `connect_downstream` to a driver output that *is* registered
(driver_context.rs:31 registers it), it works; if added via `wire_downstream` to
a plugin output, the registry was populated by `PluginManager::add_plugin`
(plugin_manager.rs:64-66) so it also works. The hazard: `create_plugin_runtime_with_output`
(runtime.rs:1359) takes a pre-built `NDArrayOutput` that may never be registered,
and tests use it. Not a production bug today, but the "registry is the single
source of truth for wiring" invariant is not enforced — any output created
outside `add_plugin`/`GenericDriverContext::new` is invisible to rewiring.

### LOW

#### B14. `param_change_tx` uses `try_send` — control-plane param changes can be
silently dropped
`io_write_int32/float64/octet` (runtime.rs:964-989) all use
`param_change_tx.try_send(...)` on a bounded channel of capacity 64
(runtime.rs:1081). If 64 param writes queue up faster than the data loop drains
them (e.g. autosave restoring hundreds of PVs at IOC init), writes are dropped
and the data-plane processor never sees them. The param library is still
updated, so the PV reads correct, but `on_param_change` / the file controller
state never updates. C++ applies param actions synchronously inside `writeInt32`.
Consequence: rare, but at IOC startup with save/restore a file plugin could miss
its FilePath/FileName/WriteMode and write to the wrong place.

#### B15. `flush_sort_buffer` on sort-mode-off does not re-check ordering /
disordered count
When `sort_mode` is set to 0 (runtime.rs:1262-1267) the buffer is drained via
`flush_sort_buffer` → `drain_all`, emitting everything in key order. Fine. But
`build_sort_params_batch` (runtime.rs:413-435) reports `disordered_arrays` and
`dropped_output_arrays` as cumulative counters that are never reset — there is no
analogue to C++ resetting these. Minor: counters are monotonic and never
clearable from the PV.

#### B16. `register_noop_commands` silently ignores `callbackSetQueueSize`
helpers.rs:96 registers `callbackSetQueueSize` as a no-op. In C this sizes the
asyn callback queue; ignoring it is acceptable for the Rust model but means a
cmd file tuning that value gets no effect and no warning. LOW / documented.

#### B17. `NDPluginFileBase::close_stream` increments `file_number` but
`process_array` Single also does — double-increment risk across mode switch
Single mode increments `file_number` per write (file_base.rs:223-225); Stream
`close_stream` increments once on close (file_base.rs:323-325); Capture
`flush_capture` increments per file or once (file_base.rs:285-302). Each path is
individually C-correct, but switching `WriteMode` mid-capture (allowed —
`on_param_change` sets mode unconditionally, file_controller.rs:234-236) with a
stream still open can leave `is_open=true` and a later Single write will not
close the stream. No single owner gates the mode transition. LOW (operator
error path) but it is a real state-machine gap.

---

## Notes

- Threading re-architecture: the entire C++ `epicsMessageQueue` + worker-pool +
  `From/ToThreadMessage` handshake (`createCallbackThreads`,
  `deleteCallbackThreads`, `startCallbackThreads`, `processTask`) is replaced by
  one `tokio::select!` loop. This is a legitimate design choice and is cleaner,
  but it dropped the accounting (G1, G2), the multi-worker option (G4), the
  drop-on-full semantics (B1), and the ProcessPlugin re-injection (G5) along the
  way. These are not "C is weird" cases — they are user-visible PVs and
  documented behaviors.

- NDArray refcounting: C++ uses explicit `reserve()`/`release()` with a finite
  `NDArrayPool`; Rust uses `Arc<NDArray>` so refcount discipline is automatic and
  the whole class of C refcount bugs (forgotten release on early return, etc.)
  cannot occur. `ArrayMessage::Drop` (channel.rs:79-88) cleanly handles the
  completion-signal + queued-counter decrement. This part of the port is solid —
  no refcount bugs found.

- `QueuedArrayCounter` (channel.rs:9-60) is a genuine improvement: it gives
  drivers a clean `wait_until_zero` for end-of-acquisition draining that C++ does
  ad-hoc. Correct use of condvar + atomic. No bug.

- `blocking_callbacks`: the Rust semantics (channel.rs:90-149) are *not* the C++
  semantics. C++ `blockingCallbacks=1` means "run processCallbacks in the
  detector's own thread, no queue at all". Rust `blocking_callbacks=1` means
  "still go through the channel, but the producer awaits downstream *completion*".
  The observable effect (producer blocked until processing done) is similar, but
  the Rust version still hops threads and still enqueues. Documented in the code;
  flagged here only so reviewers don't assume parity.

- Parameter coverage: `PluginBaseParams` (params.rs) registers the full
  NDPluginBase.template name set. The gaps are not missing *params* but missing
  *behavior* behind them (MAX_THREADS, NUM_THREADS, MAX_BYTE_RATE, NDARRAY_ADDR,
  PROCESS_PLUGIN, DROPPED_ARRAYS, QUEUE_FREE all exist as inert PVs).

- File plugins: `NDPluginFileBase` + `FilePluginController` cover Single /
  Capture / Stream, AutoSave, AutoIncrement, temp-suffix rename, CreateDir,
  FreeCapture, ReadFile. The missing pieces are all attribute-driven
  (G9) plus the lazy-open / eager-open distinction (B9) and frame validation in
  Capture mode (G12).

- No tests exercise: DroppedArrays, QueueFree, MaxByteRate, MinCallbackTime
  drop accounting, ProcessPlugin, or the `ParamBatch.extra` (non-zero addr)
  path. The sort-mode integration test (runtime.rs:1866) encodes the B2 bug as
  expected behavior and will need updating when B2 is fixed.
