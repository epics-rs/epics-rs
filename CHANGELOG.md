# Changelog

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
- Support multiple PV names in CA/PVA CLI tools (caget, camonitor, cainfo, pvaget, etc.)
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
