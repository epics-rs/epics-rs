//! Port driver base and trait.
//!
//! # I/O Model
//!
//! Ports are driven by a `PortActor` running on a dedicated thread.
//! The actor exclusively owns the driver and processes requests from a channel.
//!
//! **Cache path** (default `read_*`/`write_*` methods):
//! - Default implementations operate on the parameter cache (non-blocking).
//! - Background tasks update cache via `set_*_param()` + `call_param_callbacks()`.
//!
//! **Actor path** (requests submitted via [`crate::port_handle::PortHandle`]):
//! - Each port gets a dedicated actor thread that dispatches requests to driver methods.
//! - `can_block` indicates the port may perform blocking I/O.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use std::any::Any;

/// Per-address device state for multi-device ports.
#[derive(Debug, Clone)]
pub struct DeviceState {
    pub connected: bool,
    pub enabled: bool,
    pub auto_connect: bool,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            connected: true,
            enabled: true,
            auto_connect: true,
        }
    }
}

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::{AsynException, ExceptionEvent, ExceptionManager};
use crate::interpose::{EomReason, OctetInterpose, OctetInterposeStack};
use crate::interrupt::{InterruptManager, InterruptValue};
use crate::param::{EnumEntry, InterruptReason, ParamList, ParamType};
use crate::trace::TraceManager;
use crate::user::AsynUser;

/// C asyn `queueRequest` priority. In asyn-rs this exists as compatibility
/// metadata only — there is no actual request queue or priority-based scheduling.
/// Drivers manage their own async tasks directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum QueuePriority {
    Low = 0,
    #[default]
    Medium = 1,
    High = 2,
    /// Connect/disconnect operations — processed even when disabled/disconnected.
    Connect = 3,
}

/// Port configuration flags.
#[derive(Debug, Clone, Copy)]
pub struct PortFlags {
    /// True if port supports multiple sub-addresses (ASYN_MULTIDEVICE).
    pub multi_device: bool,
    /// True if port can block (ASYN_CANBLOCK).
    ///
    /// When `true`, the port gets a dedicated worker thread that serializes I/O via a
    /// priority queue (matching C asyn's per-port thread model).
    ///
    /// When `false`, requests execute synchronously inline on the caller's thread
    /// (no worker thread is spawned). This is appropriate for non-blocking drivers
    /// whose `io_*` methods return immediately (e.g., cache-based parameter access).
    pub can_block: bool,
    /// True if port can be destroyed via shutdown_port (ASYN_DESTRUCTIBLE).
    pub destructible: bool,
}

impl Default for PortFlags {
    fn default() -> Self {
        // `destructible: false` is the C asyn convention — see
        // asynDriver.h:97 (`#define ASYN_DESTRUCTIBLE 0x0004`) — the
        // attribute is opt-in via `pasynManager->registerPort(..., attr)`
        // and `asynManager::shutdownPort` refuses to act on ports
        // that did not opt in. Defaulting to `true` here over-applied
        // shutdown rights to every driver that built PortFlags via
        // `..PortFlags::default()`.
        Self {
            multi_device: false,
            can_block: false,
            destructible: false,
        }
    }
}

/// Base state shared by all port drivers.
/// Contains the parameter library, interrupt manager, and connection state.
///
/// # Interpose concurrency
///
/// `interpose_octet` requires `&mut self` for all operations (both `push` and
/// `dispatch_*`). Since `PortDriverBase` is always behind `Arc<Mutex<dyn PortDriver>>`,
/// any access to `interpose_octet` requires the port lock. This naturally
/// serializes interpose modifications with I/O dispatch — no additional
/// synchronization is needed. **Callers must never modify the interpose stack
/// without holding the port lock.**
pub struct PortDriverBase {
    pub port_name: String,
    pub max_addr: usize,
    pub flags: PortFlags,
    pub params: ParamList,
    pub interrupts: InterruptManager,
    pub connected: bool,
    pub enabled: bool,
    pub auto_connect: bool,
    /// `defunct` — set by [`Self::shutdown_lifecycle`] when a
    /// destructible port is torn down via `shutdown_port`. Once true,
    /// the port refuses every new request through [`Self::check_ready`].
    /// Mirrors the `dpCommon.defunct` flag at C asynManager.c:2284
    /// — once defunct, the port cannot be re-enabled.
    pub defunct: bool,
    /// Exception sink injected by [`crate::manager::PortManager`] on registration.
    pub exception_sink: Option<Arc<ExceptionManager>>,
    pub options: HashMap<String, String>,
    /// Input EOS sequence (max 2 bytes). Used by EOS interpose and drivers.
    pub input_eos: Vec<u8>,
    /// Output EOS sequence (max 2 bytes). Used by EOS interpose and drivers.
    pub output_eos: Vec<u8>,
    pub interpose_octet: OctetInterposeStack,
    pub trace: Option<Arc<TraceManager>>,
    /// Per-address device state for multi-device ports.
    pub device_states: HashMap<i32, DeviceState>,
    /// Timestamp source callback for custom timestamps.
    pub timestamp_source: Option<Arc<dyn Fn() -> SystemTime + Send + Sync>>,
}

impl PortDriverBase {
    pub fn new(port_name: &str, max_addr: usize, flags: PortFlags) -> Self {
        Self {
            port_name: port_name.to_string(),
            max_addr: max_addr.max(1),
            flags,
            params: ParamList::new(max_addr, flags.multi_device),
            interrupts: InterruptManager::new(256),
            connected: true,
            enabled: true,
            auto_connect: true,
            defunct: false,
            exception_sink: None,
            options: HashMap::new(),
            input_eos: Vec::new(),
            output_eos: Vec::new(),
            interpose_octet: OctetInterposeStack::new(),
            trace: None,
            device_states: HashMap::new(),
            timestamp_source: None,
        }
    }

    /// Announce an exception through the global exception manager (if injected).
    pub fn announce_exception(&self, exception: AsynException, addr: i32) {
        if let Some(ref sink) = self.exception_sink {
            sink.announce(&ExceptionEvent {
                port_name: self.port_name.clone(),
                exception,
                addr,
            });
        }
    }

    /// Query whether the port is connected.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Single owner-API for the port-level `connected` transition.
    ///
    /// C parity: `exceptionConnect` (asynManager.c:2151-2160) and
    /// `exceptionDisconnect` (:2174-2185) fire
    /// `asynExceptionConnect` only when the state actually changes.
    /// All driver code that toggles connection state MUST go through
    /// this helper — directly assigning `base.connected = ...`
    /// followed by `announce_exception(Connect, -1)` bypasses the
    /// edge guard and fans spurious duplicates out to listeners
    /// (CA gateway shadow tasks, asynRecord, monitor relays).
    ///
    /// Returns `true` if the state actually changed (a fan-out
    /// happened); `false` if the call was a no-op.
    pub fn set_connected(&mut self, connected: bool) -> bool {
        if self.connected == connected {
            return false;
        }
        self.connected = connected;
        self.announce_exception(AsynException::Connect, -1);
        true
    }

    /// Per-address variant — for multi-device ports. Same edge
    /// guarantee as [`Self::set_connected`].
    pub fn set_addr_connected(&mut self, addr: i32, connected: bool) -> bool {
        let was = self.device_state(addr).connected;
        if was == connected {
            return false;
        }
        self.device_state(addr).connected = connected;
        self.announce_exception(AsynException::Connect, addr);
        true
    }

    /// Query whether the port is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Query whether auto-connect is enabled.
    pub fn is_auto_connect(&self) -> bool {
        self.auto_connect
    }

    /// Toggle the auto-connect flag at runtime.
    ///
    /// C parity: `autoConnectAsyn` (asynManager.c:2310-2324) always
    /// fires `asynExceptionAutoConnect` regardless of prior state
    /// (no state-change guard). Mirror that — every call announces.
    /// Driver constructors that initialise `base.auto_connect`
    /// directly during `PortDriver::new()` keep the silent path
    /// (the port is not yet registered, so no listeners exist).
    pub fn set_auto_connect(&mut self, yes: bool) {
        self.auto_connect = yes;
        self.announce_exception(AsynException::AutoConnect, -1);
    }

    /// Per-address variant — for multi-device ports. C parity:
    /// `autoConnectAsyn` walks dpCommon via findDpCommon so a per-
    /// device pasynUser hits the device's dpc, otherwise the port's
    /// dpc (asynManager.c:2314 + findDpCommon).
    pub fn set_auto_connect_addr(&mut self, addr: i32, yes: bool) {
        self.device_state(addr).auto_connect = yes;
        self.announce_exception(AsynException::AutoConnect, addr);
    }

    /// Query whether the port has been marked defunct via
    /// [`Self::shutdown_lifecycle`] — once true the port is gone for
    /// good, mirroring C asynManager.c:2266-2269.
    pub fn is_defunct(&self) -> bool {
        self.defunct
    }

    /// Check that the port is enabled, connected, and not defunct.
    /// Returns `Err(Disabled)`, `Err(Disconnected)`, or `Err(Disabled)`
    /// (defunct => permanently disabled) otherwise.
    pub fn check_ready(&self) -> AsynResult<()> {
        // C asyn parity: a defunct port short-circuits queueRequest
        // (asynManager.c:2283 comment). Reject *before* the enabled
        // check so the error message names the lifecycle phase, not
        // just "disabled".
        if self.defunct {
            return Err(AsynError::Status {
                status: AsynStatus::Disabled,
                message: format!("port {} has been shut down (defunct)", self.port_name),
            });
        }
        if !self.enabled {
            return Err(AsynError::Status {
                status: AsynStatus::Disabled,
                message: format!("port {} is disabled", self.port_name),
            });
        }
        if !self.connected {
            return Err(AsynError::Status {
                status: AsynStatus::Disconnected,
                message: format!("port {} is disconnected", self.port_name),
            });
        }
        Ok(())
    }

    /// Run the C `shutdownPort` lifecycle (asynManager.c:2251-2308):
    ///
    /// 1. Refuse if the port did not opt into `ASYN_DESTRUCTIBLE`
    ///    (returns `Err(Status::Error)`).
    /// 2. Short-circuit if already defunct (idempotent — returns Ok).
    /// 3. Set `enabled = false`, `defunct = true` — every subsequent
    ///    request through [`Self::check_ready`] fails.
    /// 4. Broadcast `AsynException::Shutdown` so registered observers
    ///    (CA gateways, monitor sinks) tear down their handles.
    ///
    /// Drivers should call this from their own shutdown plumbing and
    /// then release any hardware-owned resources via their
    /// [`PortDriver::shutdown`] implementation. Callers from outside
    /// the runtime can drive the same lifecycle via
    /// [`crate::manager::PortManager::shutdown_port`].
    pub fn shutdown_lifecycle(&mut self) -> AsynResult<()> {
        if self.defunct {
            // Idempotent — C asynManager.c:2266-2269 returns asynSuccess.
            return Ok(());
        }
        if !self.flags.destructible {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "port {} does not support shutting down (ASYN_DESTRUCTIBLE not set)",
                    self.port_name
                ),
            });
        }
        self.enabled = false;
        self.defunct = true;
        self.announce_exception(AsynException::Shutdown, -1);
        Ok(())
    }

    /// Check that port + device address are both ready.
    /// For multi-device ports, checks per-address state in addition to port-level state.
    pub fn check_ready_addr(&self, addr: i32) -> AsynResult<()> {
        self.check_ready()?;
        if self.flags.multi_device {
            if let Some(ds) = self.device_states.get(&addr) {
                if !ds.enabled {
                    return Err(AsynError::Status {
                        status: AsynStatus::Disabled,
                        message: format!("port {} addr {} is disabled", self.port_name, addr),
                    });
                }
                if !ds.connected {
                    return Err(AsynError::Status {
                        status: AsynStatus::Disconnected,
                        message: format!("port {} addr {} is disconnected", self.port_name, addr),
                    });
                }
            }
        }
        Ok(())
    }

    /// Get or create a device state for the given address.
    pub fn device_state(&mut self, addr: i32) -> &mut DeviceState {
        self.device_states.entry(addr).or_default()
    }

    /// Check if a specific device address is connected.
    pub fn is_device_connected(&self, addr: i32) -> bool {
        self.device_states
            .get(&addr)
            .map_or(true, |ds| ds.connected)
    }

    /// Set a specific device address as connected.
    ///
    /// C parity: announce only on actual transition
    /// (asynManager.c:2151-2160 — `exceptionConnect` rejects
    /// already-connected; we keep an Ok return for idempotency but
    /// suppress the duplicate fan-out so subscribers don't see
    /// spurious connect events). Thin wrapper over
    /// [`Self::set_addr_connected`] for callers that prefer the
    /// directional verb.
    pub fn connect_addr(&mut self, addr: i32) {
        self.set_addr_connected(addr, true);
    }

    /// Set a specific device address as disconnected.
    ///
    /// C parity: announce only on actual transition
    /// (asynManager.c:2174-2185). Thin wrapper over
    /// [`Self::set_addr_connected`].
    pub fn disconnect_addr(&mut self, addr: i32) {
        self.set_addr_connected(addr, false);
    }

    /// Enable a specific device address.
    pub fn enable_addr(&mut self, addr: i32) {
        self.device_state(addr).enabled = true;
        self.announce_exception(AsynException::Enable, addr);
    }

    /// Disable a specific device address.
    pub fn disable_addr(&mut self, addr: i32) {
        self.device_state(addr).enabled = false;
        self.announce_exception(AsynException::Enable, addr);
    }

    /// Set a custom timestamp source callback.
    pub fn register_timestamp_source<F>(&mut self, source: F)
    where
        F: Fn() -> SystemTime + Send + Sync + 'static,
    {
        self.timestamp_source = Some(Arc::new(source));
    }

    /// Get current timestamp from the registered source, or SystemTime::now().
    pub fn current_timestamp(&self) -> SystemTime {
        self.timestamp_source
            .as_ref()
            .map_or_else(SystemTime::now, |f| f())
    }

    pub fn create_param(&mut self, name: &str, param_type: ParamType) -> AsynResult<usize> {
        self.params.create_param(name, param_type)
    }

    pub fn find_param(&self, name: &str) -> Option<usize> {
        self.params.find_param(name)
    }

    // --- Convenience param accessors ---

    pub fn set_int32_param(&mut self, index: usize, addr: i32, value: i32) -> AsynResult<()> {
        self.params.set_int32(index, addr, value)
    }

    pub fn get_int32_param(&self, index: usize, addr: i32) -> AsynResult<i32> {
        self.params.get_int32(index, addr)
    }

    /// Strict variant — returns [`AsynError::ParamUndefined`] when the
    /// cache entry has never been set (C parity for `asynParamUndefined`).
    /// See [`crate::param::ParamList::get_int32_strict`].
    pub fn get_int32_param_strict(&self, index: usize, addr: i32) -> AsynResult<i32> {
        self.params.get_int32_strict(index, addr)
    }

    pub fn set_int64_param(&mut self, index: usize, addr: i32, value: i64) -> AsynResult<()> {
        self.params.set_int64(index, addr, value)
    }

    pub fn get_int64_param(&self, index: usize, addr: i32) -> AsynResult<i64> {
        self.params.get_int64(index, addr)
    }

    /// Strict variant — see [`crate::param::ParamList::get_int64_strict`].
    pub fn get_int64_param_strict(&self, index: usize, addr: i32) -> AsynResult<i64> {
        self.params.get_int64_strict(index, addr)
    }

    pub fn set_float64_param(&mut self, index: usize, addr: i32, value: f64) -> AsynResult<()> {
        self.params.set_float64(index, addr, value)
    }

    pub fn get_float64_param(&self, index: usize, addr: i32) -> AsynResult<f64> {
        self.params.get_float64(index, addr)
    }

    /// Strict variant — see [`crate::param::ParamList::get_float64_strict`].
    pub fn get_float64_param_strict(&self, index: usize, addr: i32) -> AsynResult<f64> {
        self.params.get_float64_strict(index, addr)
    }

    pub fn set_string_param(&mut self, index: usize, addr: i32, value: String) -> AsynResult<()> {
        self.params.set_string(index, addr, value)
    }

    pub fn get_string_param(&self, index: usize, addr: i32) -> AsynResult<&str> {
        self.params.get_string(index, addr)
    }

    /// Strict variant — see [`crate::param::ParamList::get_string_strict`].
    pub fn get_string_param_strict(&self, index: usize, addr: i32) -> AsynResult<&str> {
        self.params.get_string_strict(index, addr)
    }

    pub fn set_uint32_param(
        &mut self,
        index: usize,
        addr: i32,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        self.params.set_uint32(index, addr, value, mask)
    }

    pub fn get_uint32_param(&self, index: usize, addr: i32) -> AsynResult<u32> {
        self.params.get_uint32(index, addr)
    }

    /// Strict variant — see [`crate::param::ParamList::get_uint32_strict`].
    pub fn get_uint32_param_strict(&self, index: usize, addr: i32) -> AsynResult<u32> {
        self.params.get_uint32_strict(index, addr)
    }

    pub fn get_enum_param(&self, index: usize, addr: i32) -> AsynResult<(usize, Arc<[EnumEntry]>)> {
        self.params.get_enum(index, addr)
    }

    pub fn set_enum_index_param(
        &mut self,
        index: usize,
        addr: i32,
        value: usize,
    ) -> AsynResult<()> {
        self.params.set_enum_index(index, addr, value)
    }

    pub fn set_enum_choices_param(
        &mut self,
        index: usize,
        addr: i32,
        choices: Arc<[EnumEntry]>,
    ) -> AsynResult<()> {
        self.params.set_enum_choices(index, addr, choices)
    }

    pub fn get_generic_pointer_param(
        &self,
        index: usize,
        addr: i32,
    ) -> AsynResult<Arc<dyn Any + Send + Sync>> {
        self.params.get_generic_pointer(index, addr)
    }

    pub fn set_generic_pointer_param(
        &mut self,
        index: usize,
        addr: i32,
        value: Arc<dyn Any + Send + Sync>,
    ) -> AsynResult<()> {
        self.params.set_generic_pointer(index, addr, value)
    }

    pub fn set_param_timestamp(
        &mut self,
        index: usize,
        addr: i32,
        ts: SystemTime,
    ) -> AsynResult<()> {
        self.params.set_timestamp(index, addr, ts)
    }

    pub fn set_param_status(
        &mut self,
        index: usize,
        addr: i32,
        status: AsynStatus,
        alarm_status: u16,
        alarm_severity: u16,
    ) -> AsynResult<()> {
        self.params
            .set_param_status(index, addr, status, alarm_status, alarm_severity)
    }

    pub fn get_param_status(&self, index: usize, addr: i32) -> AsynResult<(AsynStatus, u16, u16)> {
        self.params.get_param_status(index, addr)
    }

    /// Detailed parameter report matching C asynPortDriver::reportParams.
    pub fn report_params(&self, level: i32) {
        eprintln!("  Number of parameters is {}", self.params.len());
        if level < 1 {
            return;
        }
        for i in 0..self.params.len() {
            let name = self.params.param_name(i).unwrap_or("?");
            let ptype = self
                .params
                .param_type(i)
                .map(|t| format!("{t:?}"))
                .unwrap_or("?".into());
            if level >= 2 {
                for addr in 0..self.max_addr.max(1) {
                    let val = self
                        .params
                        .get_value(i, addr as i32)
                        .map(|v| format!("{v:?}"))
                        .unwrap_or("undefined".into());
                    let (status, alarm_st, alarm_sev) = self
                        .params
                        .get_param_status(i, addr as i32)
                        .unwrap_or((AsynStatus::Success, 0, 0));
                    eprintln!(
                        "  param[{i}] name={name} type={ptype} addr={addr} val={val} status={status:?} alarm=({alarm_st},{alarm_sev})"
                    );
                }
            } else {
                eprintln!("  param[{i}] name={name} type={ptype}");
            }
        }
    }

    /// Push an interpose layer onto the octet I/O stack.
    ///
    /// **Concurrency**: requires `&mut self`, which means the caller must hold
    /// the port lock (`Arc<Mutex<dyn PortDriver>>`). This ensures
    /// interpose modifications are serialized with I/O dispatch.
    pub fn push_octet_interpose(&mut self, layer: Box<dyn OctetInterpose>) {
        self.interpose_octet.push(layer);
    }

    /// Flush changed parameters as interrupt notifications.
    /// Equivalent to C asyn's callParamCallbacks().
    pub fn call_param_callbacks(&mut self, addr: i32) -> AsynResult<()> {
        let changed = self.params.take_changed(addr)?;
        let now = self.current_timestamp();
        for reason in changed {
            let value = self.params.get_value(reason, addr)?.clone();
            let ts = self.params.get_timestamp(reason, addr)?.unwrap_or(now);
            let uint32_mask = self
                .params
                .get_uint32_interrupt_mask(reason, addr)
                .unwrap_or(0);
            // C parity: asynPortDriver.cpp:631-642 sets
            // `pInterrupt->pasynUser->auxStatus/alarmStatus/alarmSeverity`
            // from the param's stored status before invoking each
            // subscriber callback. Pull those here so subscribers see
            // the same triplet C consumers do.
            let (aux_status, alarm_status, alarm_severity) = self
                .params
                .get_param_status(reason, addr)
                .unwrap_or((AsynStatus::Success, 0, 0));
            self.interrupts.notify(InterruptValue {
                reason,
                addr,
                value,
                timestamp: ts,
                uint32_changed_mask: uint32_mask,
                aux_status,
                alarm_status,
                alarm_severity,
            });
        }
        Ok(())
    }

    /// Flush a single parameter's changed flag and notify if dirty.
    /// Use this instead of `call_param_callbacks` when you want to avoid
    /// flushing unrelated parameters (e.g. rapidly-updating CP-linked params).
    pub fn call_param_callback(&mut self, addr: i32, reason: usize) -> AsynResult<()> {
        if self.params.take_changed_single(reason, addr)? {
            let now = self.current_timestamp();
            let value = self.params.get_value(reason, addr)?.clone();
            let ts = self.params.get_timestamp(reason, addr)?.unwrap_or(now);
            let uint32_mask = self
                .params
                .get_uint32_interrupt_mask(reason, addr)
                .unwrap_or(0);
            // C parity: see `call_param_callbacks` above.
            let (aux_status, alarm_status, alarm_severity) = self
                .params
                .get_param_status(reason, addr)
                .unwrap_or((AsynStatus::Success, 0, 0));
            self.interrupts.notify(InterruptValue {
                reason,
                addr,
                value,
                timestamp: ts,
                uint32_changed_mask: uint32_mask,
                aux_status,
                alarm_status,
                alarm_severity,
            });
        }
        Ok(())
    }

    /// Mark a parameter as changed without modifying its value.
    ///
    /// Use this to trigger I/O Intr on params whose data is served via
    /// `read_*_array()` overrides rather than the param cache (e.g. pixel data).
    pub fn mark_param_changed(&mut self, index: usize, addr: i32) -> AsynResult<()> {
        self.params.mark_changed(index, addr)
    }
}

/// Port driver trait. All methods have default implementations that operate
/// on the parameter cache (no actual I/O).
///
/// Drivers performing real hardware I/O should:
/// 1. Run I/O in a background task (e.g., tokio::spawn)
/// 2. Update parameters via `base_mut().set_*_param()` + `call_param_callbacks()`
/// 3. Let the default `read_*` methods return cached values
///
/// # LockPort/UnlockPort
///
/// C asyn provides `lockPort`/`unlockPort` for direct mutex locking. In asyn-rs,
/// the port is always behind `Arc<Mutex<dyn PortDriver>>`, so callers hold the
/// parking_lot mutex directly. For multi-request exclusive access, use
/// `BlockProcess`/`UnblockProcess` via the worker queue.
pub trait PortDriver: Send + Sync + 'static {
    fn base(&self) -> &PortDriverBase;
    fn base_mut(&mut self) -> &mut PortDriverBase;

    // --- AsynCommon ---

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // Single owner-API: edge-guarded fire is in PortDriverBase::set_connected.
        self.base_mut().set_connected(true);
        Ok(())
    }

    fn disconnect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        self.base_mut().set_connected(false);
        Ok(())
    }

    fn enable(&mut self, _user: &AsynUser) -> AsynResult<()> {
        self.base_mut().enabled = true;
        self.base().announce_exception(AsynException::Enable, -1);
        Ok(())
    }

    fn disable(&mut self, _user: &AsynUser) -> AsynResult<()> {
        self.base_mut().enabled = false;
        self.base().announce_exception(AsynException::Enable, -1);
        Ok(())
    }

    fn connect_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().connect_addr(user.addr);
        Ok(())
    }

    fn disconnect_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().disconnect_addr(user.addr);
        Ok(())
    }

    fn enable_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().enable_addr(user.addr);
        Ok(())
    }

    fn disable_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().disable_addr(user.addr);
        Ok(())
    }

    fn get_option(&self, key: &str) -> AsynResult<String> {
        self.base()
            .options
            .get(key)
            .cloned()
            .ok_or_else(|| AsynError::OptionNotFound(key.to_string()))
    }

    fn set_option(&mut self, key: &str, value: &str) -> AsynResult<()> {
        self.base_mut()
            .options
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn report(&self, level: i32) {
        let base = self.base();
        eprintln!("Port: {}", base.port_name);
        eprintln!(
            "  connected: {}, max_addr: {}, params: {}, options: {}",
            base.connected,
            base.max_addr,
            base.params.len(),
            base.options.len()
        );
        if level >= 1 {
            base.report_params(level.saturating_sub(1));
        }
        if level >= 2 {
            for (k, v) in &base.options {
                eprintln!("  option: {k} = {v}");
            }
        }
    }

    // --- Scalar I/O (cache-based defaults, timeout not applicable) ---

    // Cache-based defaults do NOT check connection state (C parity).
    // The port actor checks check_ready_addr() before dispatching, matching
    // C asyn where asynManager checks connection before calling the driver.

    fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
        self.base().params.get_int32(user.reason, user.addr)
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int32(user.reason, user.addr, value)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_int64(&mut self, user: &AsynUser) -> AsynResult<i64> {
        self.base().params.get_int64(user.reason, user.addr)
    }

    fn write_int64(&mut self, user: &mut AsynUser, value: i64) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int64(user.reason, user.addr, value)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn get_bounds_int32(&self, _user: &AsynUser) -> AsynResult<(i32, i32)> {
        Ok((i32::MIN, i32::MAX))
    }

    fn get_bounds_int64(&self, _user: &AsynUser) -> AsynResult<(i64, i64)> {
        Ok((i64::MIN, i64::MAX))
    }

    fn read_float64(&mut self, user: &AsynUser) -> AsynResult<f64> {
        self.base().params.get_float64(user.reason, user.addr)
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_float64(user.reason, user.addr, value)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        let s = self.base().params.get_string(user.reason, user.addr)?;
        let bytes = s.as_bytes();
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
        let s = String::from_utf8_lossy(data).into_owned();
        self.base_mut()
            .params
            .set_string(user.reason, user.addr, s)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<u32> {
        let val = self.base().params.get_uint32(user.reason, user.addr)?;
        Ok(val & mask)
    }

    fn write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_uint32(user.reason, user.addr, value, mask)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    /// Configure rising / falling interrupt masks for a
    /// UInt32Digital parameter. C parity:
    /// `asynPortDriver::setInterruptUInt32Digital`
    /// (`asynPortDriver.cpp:2346-2369`) → routes to
    /// `paramList::setUInt32Interrupt`. The default delegates to the
    /// param store; drivers that need to push the configuration to
    /// hardware (e.g. real GPIB cards toggling SRQ enable) override
    /// it.
    fn set_interrupt_uint32_digital(
        &mut self,
        user: &AsynUser,
        mask: u32,
        reason: InterruptReason,
    ) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_uint32_interrupt(user.reason, user.addr, mask, reason)
    }

    /// Clear bits from rising AND falling masks. C parity:
    /// `asynPortDriver::clearInterruptUInt32Digital`
    /// (`asynPortDriver.cpp:2392-2415`). Mirrors C — the call does
    /// not take an `interruptReason`; both masks are cleared.
    fn clear_interrupt_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<()> {
        self.base_mut()
            .params
            .clear_uint32_interrupt(user.reason, user.addr, mask)
    }

    /// Read the configured rising / falling / combined mask. C
    /// parity: `asynPortDriver::getInterruptUInt32Digital`
    /// (`asynPortDriver.cpp:2438-2461`).
    fn get_interrupt_uint32_digital(
        &self,
        user: &AsynUser,
        reason: InterruptReason,
    ) -> AsynResult<u32> {
        self.base()
            .params
            .get_uint32_interrupt(user.reason, user.addr, reason)
    }

    // --- Enum I/O (cache-based defaults) ---

    fn read_enum(&mut self, user: &AsynUser) -> AsynResult<(usize, Arc<[EnumEntry]>)> {
        self.base().params.get_enum(user.reason, user.addr)
    }

    fn write_enum(&mut self, user: &mut AsynUser, index: usize) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_enum_index(user.reason, user.addr, index)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn write_enum_choices(
        &mut self,
        user: &mut AsynUser,
        choices: Arc<[EnumEntry]>,
    ) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_enum_choices(user.reason, user.addr, choices)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    // --- GenericPointer I/O (cache-based defaults) ---

    fn read_generic_pointer(&mut self, user: &AsynUser) -> AsynResult<Arc<dyn Any + Send + Sync>> {
        self.base()
            .params
            .get_generic_pointer(user.reason, user.addr)
    }

    fn write_generic_pointer(
        &mut self,
        user: &mut AsynUser,
        value: Arc<dyn Any + Send + Sync>,
    ) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_generic_pointer(user.reason, user.addr, value)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    // --- Array I/O (default: not supported) ---

    fn read_float64_array(&mut self, _user: &AsynUser, _buf: &mut [f64]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynFloat64Array".into()))
    }

    fn write_float64_array(&mut self, user: &AsynUser, data: &[f64]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_float64_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_int32_array(&mut self, _user: &AsynUser, _buf: &mut [i32]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynInt32Array".into()))
    }

    fn write_int32_array(&mut self, user: &AsynUser, data: &[i32]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int32_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_int8_array(&mut self, _user: &AsynUser, _buf: &mut [i8]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynInt8Array".into()))
    }

    fn write_int8_array(&mut self, user: &AsynUser, data: &[i8]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int8_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_int16_array(&mut self, _user: &AsynUser, _buf: &mut [i16]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynInt16Array".into()))
    }

    fn write_int16_array(&mut self, user: &AsynUser, data: &[i16]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int16_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_int64_array(&mut self, _user: &AsynUser, _buf: &mut [i64]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynInt64Array".into()))
    }

    fn write_int64_array(&mut self, user: &AsynUser, data: &[i64]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int64_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_float32_array(&mut self, _user: &AsynUser, _buf: &mut [f32]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynFloat32Array".into()))
    }

    fn write_float32_array(&mut self, user: &AsynUser, data: &[f32]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_float32_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    // --- I/O methods (worker thread calls these) ---
    // Default: delegate to cache-based read_*/write_* for backward compat.
    // Real I/O drivers override these for actual hardware access.

    fn io_read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        self.read_octet(user, buf)
    }

    /// Octet read that also reports the end-of-message reason — C
    /// parity for `asynOctet::read(... int *eomReason)`
    /// (`asynOctet.h:38-40`). The default implementation delegates to
    /// [`Self::io_read_octet`] and reconstructs a synthetic
    /// [`EomReason`]: `CNT` when the buffer filled, `empty` otherwise.
    /// Drivers that have native EOM information
    /// (`asynOctetSyncIO::readRaw`, GPIB END, EOS match) must
    /// override this method so consumers — `asynRecord::EOMR`,
    /// `asynOctetSyncIO::readRaw` mirrors — receive the real flags.
    fn io_read_octet_eom(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        let cap = buf.len();
        let n = self.io_read_octet(user, buf)?;
        let eom = if n >= cap && cap > 0 {
            EomReason::CNT
        } else {
            EomReason::empty()
        };
        Ok((n, eom))
    }

    fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
        self.write_octet(user, data)
    }

    fn io_read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
        self.read_int32(user)
    }

    fn io_write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.write_int32(user, value)
    }

    fn io_read_int64(&mut self, user: &AsynUser) -> AsynResult<i64> {
        self.read_int64(user)
    }

    fn io_write_int64(&mut self, user: &mut AsynUser, value: i64) -> AsynResult<()> {
        self.write_int64(user, value)
    }

    fn io_read_float64(&mut self, user: &AsynUser) -> AsynResult<f64> {
        self.read_float64(user)
    }

    fn io_write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.write_float64(user, value)
    }

    fn io_read_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<u32> {
        self.read_uint32_digital(user, mask)
    }

    fn io_write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        self.write_uint32_digital(user, value, mask)
    }

    fn io_flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        Ok(())
    }

    // --- Octet EOS (delegates to interpose stack by default) ---
    //
    // ## EOS connect-wait policy (C asyn issue #103)
    //
    // C asyn `asynOctetSyncIO::setInputEos` / `setOutputEos`
    // (`asynOctetSyncIO.c:300-321`, 346-367) call `lockPort` ahead of
    // the actual `setInputEos` — `lockPort` waits up to the user's
    // timeout for the port to be connected, by `epicsEventWait`-ing
    // on the connect event published from `connectIt`. On IOC init
    // and exit this serialises EOS configuration against the connect
    // task, but it also means a `setInputEos` issued before the port
    // has ever connected blocks the calling thread (issue #103
    // captured the symptom: IOC startup pauses for the full asyn
    // timeout when the device is off-line).
    //
    // The Rust path here is purely in-memory: `set_input_eos` and
    // `set_output_eos` write the bytes into `PortDriverBase` and the
    // EOS interpose stack reads from those fields at next read/write
    // time. No connect-wait, no lock contention with the connect
    // task — so issue #103's symptom cannot reproduce. If a future
    // refactor introduces a connect-gated EOS path (e.g. a driver
    // that owns the EOS state inside its connect()-allocated
    // resource), authors MUST keep the wait optional / bounded so
    // the connect-wait failure mode doesn't return.

    fn set_input_eos(&mut self, eos: &[u8]) -> AsynResult<()> {
        if eos.len() > 2 {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("illegal eoslen {}", eos.len()),
            });
        }
        self.base_mut().input_eos = eos.to_vec();
        Ok(())
    }

    fn get_input_eos(&self) -> Vec<u8> {
        self.base().input_eos.clone()
    }

    fn set_output_eos(&mut self, eos: &[u8]) -> AsynResult<()> {
        if eos.len() > 2 {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("illegal eoslen {}", eos.len()),
            });
        }
        self.base_mut().output_eos = eos.to_vec();
        Ok(())
    }

    fn get_output_eos(&self) -> Vec<u8> {
        self.base().output_eos.clone()
    }

    // --- Lifecycle ---

    /// Called when the port is being shut down. Drivers override this
    /// to release hardware resources. Matches C asynPortDriver::shutdownPortDriver().
    fn shutdown(&mut self) -> AsynResult<()> {
        Ok(())
    }

    // --- drvUser ---

    /// Resolve a driver info string to a parameter index.
    /// Default: look up by parameter name.
    fn drv_user_create(&self, drv_info: &str) -> AsynResult<usize> {
        self.base()
            .params
            .find_param(drv_info)
            .ok_or_else(|| AsynError::ParamNotFound(drv_info.to_string()))
    }

    // --- Capabilities ---

    /// Declare the capabilities this driver supports.
    /// Default implementation includes all scalar read/write operations.
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        crate::interfaces::default_capabilities()
    }

    /// Check if this driver supports a specific capability.
    fn supports(&self, cap: crate::interfaces::Capability) -> bool {
        self.capabilities().contains(&cap)
    }

    fn init(&mut self) -> AsynResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestDriver {
        base: PortDriverBase,
    }

    impl TestDriver {
        fn new() -> Self {
            let mut base = PortDriverBase::new("test", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.create_param("TEMP", ParamType::Float64).unwrap();
            base.create_param("MSG", ParamType::Octet).unwrap();
            base.create_param("BITS", ParamType::UInt32Digital).unwrap();
            Self { base }
        }
    }

    impl PortDriver for TestDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    #[test]
    fn test_default_read_write_int32() {
        let mut drv = TestDriver::new();
        let mut user = AsynUser::new(0);
        drv.write_int32(&mut user, 42).unwrap();
        let user = AsynUser::new(0);
        assert_eq!(drv.read_int32(&user).unwrap(), 42);
    }

    #[test]
    fn test_default_read_write_float64() {
        let mut drv = TestDriver::new();
        let mut user = AsynUser::new(1);
        drv.write_float64(&mut user, 3.14).unwrap();
        let user = AsynUser::new(1);
        assert!((drv.read_float64(&user).unwrap() - 3.14).abs() < 1e-10);
    }

    #[test]
    fn test_default_read_write_octet() {
        let mut drv = TestDriver::new();
        let mut user = AsynUser::new(2);
        drv.write_octet(&mut user, b"hello").unwrap();
        let user = AsynUser::new(2);
        let mut buf = [0u8; 32];
        let n = drv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn test_default_read_write_uint32() {
        let mut drv = TestDriver::new();
        let mut user = AsynUser::new(3);
        drv.write_uint32_digital(&mut user, 0xFF, 0x0F).unwrap();
        let user = AsynUser::new(3);
        assert_eq!(drv.read_uint32_digital(&user, 0xFF).unwrap(), 0x0F);
    }

    #[test]
    fn test_connect_disconnect() {
        let mut drv = TestDriver::new();
        let user = AsynUser::default();
        assert!(drv.base().connected);
        drv.disconnect(&user).unwrap();
        assert!(!drv.base().connected);
        drv.connect(&user).unwrap();
        assert!(drv.base().connected);
    }

    #[test]
    fn test_drv_user_create() {
        let drv = TestDriver::new();
        assert_eq!(drv.drv_user_create("VAL").unwrap(), 0);
        assert_eq!(drv.drv_user_create("TEMP").unwrap(), 1);
        assert!(drv.drv_user_create("NOPE").is_err());
    }

    #[test]
    fn test_call_param_callbacks() {
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        drv.base_mut().set_int32_param(0, 0, 100).unwrap();
        drv.base_mut().set_float64_param(1, 0, 2.0).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let v1 = rx.try_recv().unwrap();
        assert_eq!(v1.reason, 0);
        let v2 = rx.try_recv().unwrap();
        assert_eq!(v2.reason, 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_call_param_callbacks_propagates_aux_status_and_alarm() {
        // C parity: asynPortDriver.cpp:631-642 writes the param's stored
        // status / alarmStatus / alarmSeverity onto the subscriber's
        // pasynUser before invoking the callback. The Rust port carries
        // those fields on InterruptValue.
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        drv.base_mut().set_int32_param(0, 0, 99).unwrap();
        drv.base_mut()
            .params
            .set_param_status(0, 0, crate::error::AsynStatus::Timeout, 4, 2)
            .unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let iv = rx.try_recv().unwrap();
        assert_eq!(iv.reason, 0);
        assert!(matches!(iv.aux_status, crate::error::AsynStatus::Timeout));
        assert_eq!(iv.alarm_status, 4);
        assert_eq!(iv.alarm_severity, 2);
    }

    #[test]
    fn test_call_param_callback_single_propagates_aux_status() {
        // Mirror for the single-flush path (call_param_callback).
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        drv.base_mut().set_int32_param(0, 0, 1).unwrap();
        drv.base_mut()
            .params
            .set_param_status(0, 0, crate::error::AsynStatus::Disconnected, 7, 3)
            .unwrap();
        drv.base_mut().call_param_callback(0, 0).unwrap();

        let iv = rx.try_recv().unwrap();
        assert!(matches!(
            iv.aux_status,
            crate::error::AsynStatus::Disconnected
        ));
        assert_eq!(iv.alarm_status, 7);
        assert_eq!(iv.alarm_severity, 3);
    }

    #[test]
    fn test_no_callback_for_unchanged() {
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        drv.base_mut().set_int32_param(0, 0, 5).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();
        let _ = rx.try_recv().unwrap(); // consume

        // Set same value — no interrupt
        drv.base_mut().set_int32_param(0, 0, 5).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_array_not_supported_by_default() {
        let mut drv = TestDriver::new();
        let user = AsynUser::new(0);
        let mut buf = [0f64; 10];
        assert!(drv.read_float64_array(&user, &mut buf).is_err());
        assert!(drv.write_float64_array(&user, &[1.0]).is_err());
    }

    #[test]
    fn test_option_set_get() {
        let mut drv = TestDriver::new();
        drv.set_option("baud", "9600").unwrap();
        assert_eq!(drv.get_option("baud").unwrap(), "9600");
        drv.set_option("baud", "115200").unwrap();
        assert_eq!(drv.get_option("baud").unwrap(), "115200");
    }

    #[test]
    fn test_option_not_found() {
        let drv = TestDriver::new();
        let err = drv.get_option("nonexistent").unwrap_err();
        assert!(matches!(err, AsynError::OptionNotFound(_)));
    }

    #[test]
    fn test_report_no_panic() {
        let mut drv = TestDriver::new();
        drv.set_option("testkey", "testval").unwrap();
        drv.base_mut().set_int32_param(0, 0, 42).unwrap();
        for level in 0..=3 {
            drv.report(level);
        }
    }

    #[test]
    fn test_callback_uses_param_timestamp() {
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        let custom_ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        drv.base_mut().set_int32_param(0, 0, 77).unwrap();
        drv.base_mut().set_param_timestamp(0, 0, custom_ts).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let v = rx.try_recv().unwrap();
        assert_eq!(v.reason, 0);
        assert_eq!(v.timestamp, custom_ts);
    }

    #[test]
    fn test_default_read_write_enum() {
        use crate::param::EnumEntry;

        let mut base = PortDriverBase::new("test_enum", 1, PortFlags::default());
        base.create_param("MODE", ParamType::Enum).unwrap();

        struct EnumDriver {
            base: PortDriverBase,
        }
        impl PortDriver for EnumDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = EnumDriver { base };
        let choices: Arc<[EnumEntry]> = Arc::from(vec![
            EnumEntry {
                string: "Off".into(),
                value: 0,
                severity: 0,
            },
            EnumEntry {
                string: "On".into(),
                value: 1,
                severity: 0,
            },
        ]);
        let mut user = AsynUser::new(0);
        drv.write_enum_choices(&mut user, choices).unwrap();
        drv.write_enum(&mut user, 1).unwrap();
        let (idx, ch) = drv.read_enum(&AsynUser::new(0)).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(ch[1].string, "On");
    }

    #[test]
    fn test_enum_callback() {
        use crate::param::{EnumEntry, ParamValue};

        let mut base = PortDriverBase::new("test_enum_cb", 1, PortFlags::default());
        base.create_param("MODE", ParamType::Enum).unwrap();
        let mut rx = base.interrupts.subscribe_async();

        struct EnumDriver {
            base: PortDriverBase,
        }
        impl PortDriver for EnumDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = EnumDriver { base };
        let choices: Arc<[EnumEntry]> = Arc::from(vec![
            EnumEntry {
                string: "A".into(),
                value: 0,
                severity: 0,
            },
            EnumEntry {
                string: "B".into(),
                value: 1,
                severity: 0,
            },
        ]);
        drv.base_mut()
            .set_enum_choices_param(0, 0, choices)
            .unwrap();
        drv.base_mut().set_enum_index_param(0, 0, 1).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let v = rx.try_recv().unwrap();
        assert_eq!(v.reason, 0);
        assert!(matches!(v.value, ParamValue::Enum { index: 1, .. }));
    }

    #[test]
    fn test_default_read_write_generic_pointer() {
        let mut base = PortDriverBase::new("test_gp", 1, PortFlags::default());
        base.create_param("PTR", ParamType::GenericPointer).unwrap();

        struct GpDriver {
            base: PortDriverBase,
        }
        impl PortDriver for GpDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = GpDriver { base };
        let data: Arc<dyn std::any::Any + Send + Sync> = Arc::new(99i32);
        let mut user = AsynUser::new(0);
        drv.write_generic_pointer(&mut user, data).unwrap();
        let val = drv.read_generic_pointer(&AsynUser::new(0)).unwrap();
        assert_eq!(*val.downcast_ref::<i32>().unwrap(), 99);
    }

    #[test]
    fn test_generic_pointer_callback() {
        use crate::param::ParamValue;

        let mut base = PortDriverBase::new("test_gp_cb", 1, PortFlags::default());
        base.create_param("PTR", ParamType::GenericPointer).unwrap();
        let mut rx = base.interrupts.subscribe_async();

        struct GpDriver {
            base: PortDriverBase,
        }
        impl PortDriver for GpDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = GpDriver { base };
        let data: Arc<dyn std::any::Any + Send + Sync> = Arc::new(vec![1, 2, 3]);
        drv.base_mut()
            .set_generic_pointer_param(0, 0, data)
            .unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let v = rx.try_recv().unwrap();
        assert_eq!(v.reason, 0);
        assert!(matches!(v.value, ParamValue::GenericPointer(_)));
    }

    #[test]
    fn test_interpose_push_requires_lock() {
        use crate::interpose::{OctetInterpose, OctetNext, OctetReadResult};
        use parking_lot::Mutex;
        use std::sync::Arc;

        struct NoopInterpose;
        impl OctetInterpose for NoopInterpose {
            fn read(
                &mut self,
                user: &AsynUser,
                buf: &mut [u8],
                next: &mut dyn OctetNext,
            ) -> AsynResult<OctetReadResult> {
                next.read(user, buf)
            }
            fn write(
                &mut self,
                user: &mut AsynUser,
                data: &[u8],
                next: &mut dyn OctetNext,
            ) -> AsynResult<usize> {
                next.write(user, data)
            }
            fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
                next.flush(user)
            }
        }

        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(TestDriver::new()));

        {
            let mut guard = port.lock();
            guard
                .base_mut()
                .push_octet_interpose(Box::new(NoopInterpose));
            assert_eq!(guard.base().interpose_octet.len(), 1);
        }
    }

    #[test]
    fn test_default_read_write_int64() {
        let mut base = PortDriverBase::new("test_i64", 1, PortFlags::default());
        base.create_param("BIG", ParamType::Int64).unwrap();

        struct I64Driver {
            base: PortDriverBase,
        }
        impl PortDriver for I64Driver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = I64Driver { base };
        let mut user = AsynUser::new(0);
        drv.write_int64(&mut user, i64::MAX).unwrap();
        assert_eq!(drv.read_int64(&AsynUser::new(0)).unwrap(), i64::MAX);
    }

    #[test]
    fn test_get_bounds_int64_default() {
        let base = PortDriverBase::new("test_bounds", 1, PortFlags::default());
        struct BoundsDriver {
            base: PortDriverBase,
        }
        impl PortDriver for BoundsDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }
        let drv = BoundsDriver { base };
        let (lo, hi) = drv.get_bounds_int64(&AsynUser::default()).unwrap();
        assert_eq!(lo, i64::MIN);
        assert_eq!(hi, i64::MAX);
    }

    #[test]
    fn test_per_addr_device_state() {
        let mut base = PortDriverBase::new(
            "multi",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();

        // Default: all connected
        assert!(base.is_device_connected(0));
        assert!(base.is_device_connected(1));

        // Disable addr 1
        base.device_state(1).enabled = false;
        assert!(base.check_ready_addr(0).is_ok());
        let err = base.check_ready_addr(1).unwrap_err();
        assert!(format!("{err}").contains("disabled"));

        // Disconnect addr 2
        base.device_state(2).connected = false;
        let err = base.check_ready_addr(2).unwrap_err();
        assert!(format!("{err}").contains("disconnected"));
    }

    #[test]
    fn test_per_addr_single_device_ignored() {
        let mut base = PortDriverBase::new("single", 1, PortFlags::default());
        base.create_param("V", ParamType::Int32).unwrap();
        // For single-device, per-addr check passes even if no device state
        assert!(base.check_ready_addr(0).is_ok());
    }

    #[test]
    fn test_timestamp_source() {
        let mut base = PortDriverBase::new("ts_test", 1, PortFlags::default());
        base.create_param("V", ParamType::Int32).unwrap();

        let fixed_ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(999999);
        base.register_timestamp_source(move || fixed_ts);

        assert_eq!(base.current_timestamp(), fixed_ts);
    }

    #[test]
    fn test_timestamp_source_in_callbacks() {
        let mut base = PortDriverBase::new("ts_cb", 1, PortFlags::default());
        base.create_param("V", ParamType::Int32).unwrap();
        let mut rx = base.interrupts.subscribe_async();

        let fixed_ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(123456);
        base.register_timestamp_source(move || fixed_ts);

        struct TsDriver {
            base: PortDriverBase,
        }
        impl PortDriver for TsDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }
        let mut drv = TsDriver { base };
        drv.base_mut().set_int32_param(0, 0, 42).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let v = rx.try_recv().unwrap();
        // Should use fixed_ts since no per-param timestamp is set
        assert_eq!(v.timestamp, fixed_ts);
    }

    #[test]
    fn test_queue_priority_connect() {
        assert!(QueuePriority::Connect > QueuePriority::High);
    }

    #[test]
    fn test_port_flags_destructible_default_is_opt_in() {
        // C asyn parity: ASYN_DESTRUCTIBLE (0x0004, asynDriver.h:97) is
        // a `registerPort` attribute that callers opt into. Default
        // must be false so drivers don't accidentally accept a
        // shutdownPort call. PortDriver authors that want shutdown
        // support set `destructible: true` explicitly.
        let flags = PortFlags::default();
        assert!(
            !flags.destructible,
            "destructible must be opt-in (C parity)"
        );
    }

    #[test]
    fn shutdown_lifecycle_refuses_non_destructible() {
        let mut base = PortDriverBase::new(
            "p_nondestr",
            1,
            PortFlags {
                multi_device: false,
                can_block: false,
                destructible: false,
            },
        );
        match base.shutdown_lifecycle() {
            Err(AsynError::Status { message, .. }) => {
                assert!(message.contains("ASYN_DESTRUCTIBLE"), "msg={message}");
            }
            other => panic!("expected ASYN_DESTRUCTIBLE refusal, got {other:?}"),
        }
        assert!(
            !base.is_defunct(),
            "non-destructible port must not flip defunct"
        );
        assert!(base.is_enabled(), "non-destructible port must stay enabled");
    }

    #[test]
    fn shutdown_lifecycle_marks_destructible_defunct_and_idempotent() {
        let mut base = PortDriverBase::new(
            "p_destr",
            1,
            PortFlags {
                multi_device: false,
                can_block: false,
                destructible: true,
            },
        );
        assert!(base.is_enabled());
        assert!(!base.is_defunct());
        base.shutdown_lifecycle().unwrap();
        assert!(
            !base.is_enabled(),
            "shutdown_lifecycle must flip enabled=false"
        );
        assert!(
            base.is_defunct(),
            "shutdown_lifecycle must flip defunct=true"
        );
        // Idempotent — second call is Ok and leaves state unchanged.
        base.shutdown_lifecycle().unwrap();
        assert!(base.is_defunct());
        // check_ready surfaces the defunct state for every request.
        match base.check_ready() {
            Err(AsynError::Status { message, .. }) => {
                assert!(message.contains("defunct"), "msg={message}");
            }
            other => panic!("expected defunct error, got {other:?}"),
        }
    }

    // --- Phase 2B: per-addr connect/disconnect/enable/disable ---

    #[test]
    fn test_connect_addr() {
        let mut base = PortDriverBase::new(
            "multi_conn",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();

        base.disconnect_addr(1);
        assert!(!base.is_device_connected(1));
        assert!(base.check_ready_addr(1).is_err());

        base.connect_addr(1);
        assert!(base.is_device_connected(1));
        assert!(base.check_ready_addr(1).is_ok());
    }

    #[test]
    fn test_enable_disable_addr() {
        let mut base = PortDriverBase::new(
            "multi_en",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();

        base.disable_addr(2);
        let err = base.check_ready_addr(2).unwrap_err();
        assert!(format!("{err}").contains("disabled"));

        base.enable_addr(2);
        assert!(base.check_ready_addr(2).is_ok());
    }

    #[test]
    fn test_port_level_overrides_addr() {
        let mut base = PortDriverBase::new(
            "multi_override",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();

        // Port-level disabled overrides addr-level enabled
        base.enabled = false;
        base.enable_addr(0); // addr 0 is enabled, but port is disabled
        let err = base.check_ready_addr(0).unwrap_err();
        assert!(format!("{err}").contains("disabled"));
    }

    #[test]
    fn test_per_addr_exception_announced() {
        use std::sync::atomic::{AtomicI32, Ordering};

        let mut base = PortDriverBase::new(
            "multi_exc",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();

        let exc_mgr = Arc::new(crate::exception::ExceptionManager::new());
        base.exception_sink = Some(exc_mgr.clone());

        let last_addr = Arc::new(AtomicI32::new(-99));
        let last_addr2 = last_addr.clone();
        exc_mgr.add_callback(move |event| {
            last_addr2.store(event.addr, Ordering::Relaxed);
        });

        base.disconnect_addr(3);
        assert_eq!(last_addr.load(Ordering::Relaxed), 3);

        base.enable_addr(2);
        assert_eq!(last_addr.load(Ordering::Relaxed), 2);
    }

    /// C parity (asynManager.c:2151-2160 exceptionConnect,
    /// :2174-2185 exceptionDisconnect): redundant connect/disconnect
    /// on a port already in that state must NOT fan out a duplicate
    /// `asynExceptionConnect`. Subscribers depend on the event
    /// edge — duplicate fan-out causes them to e.g. re-subscribe or
    /// re-arm timers that should fire exactly once per transition.
    #[test]
    fn test_connect_disconnect_announce_only_on_transition() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut base = PortDriverBase::new(
            "edge",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();
        let exc_mgr = Arc::new(crate::exception::ExceptionManager::new());
        base.exception_sink = Some(exc_mgr.clone());

        let connect_hits = Arc::new(AtomicUsize::new(0));
        let hits2 = connect_hits.clone();
        exc_mgr.add_callback(move |event| {
            if event.exception == AsynException::Connect {
                hits2.fetch_add(1, Ordering::Relaxed);
            }
        });

        // device starts connected by DeviceState::default — a redundant
        // connect_addr is a no-op.
        base.connect_addr(2);
        assert_eq!(
            connect_hits.load(Ordering::Relaxed),
            0,
            "redundant connect_addr must not fan out"
        );

        // First transition fires once.
        base.disconnect_addr(2);
        assert_eq!(connect_hits.load(Ordering::Relaxed), 1);

        // Redundant disconnect is silent.
        base.disconnect_addr(2);
        assert_eq!(
            connect_hits.load(Ordering::Relaxed),
            1,
            "redundant disconnect_addr must not fan out"
        );

        // Re-connect fires the transition.
        base.connect_addr(2);
        assert_eq!(connect_hits.load(Ordering::Relaxed), 2);
    }

    /// C parity: `autoConnectAsyn` (asynManager.c:2310-2324) fires
    /// `asynExceptionAutoConnect` unconditionally — even setting the
    /// same value as the current one. Rust mirrors that so observers
    /// can refresh their UI after a re-confirmation, not just an edge.
    #[test]
    fn test_set_auto_connect_fires_unconditionally() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut base = PortDriverBase::new("ac", 1, PortFlags::default());
        let exc_mgr = Arc::new(crate::exception::ExceptionManager::new());
        base.exception_sink = Some(exc_mgr.clone());
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        exc_mgr.add_callback(move |event| {
            if event.exception == AsynException::AutoConnect {
                hits2.fetch_add(1, Ordering::Relaxed);
            }
        });
        // base.auto_connect defaults to true — setting true again
        // still must fire (no state-change guard in C).
        base.set_auto_connect(true);
        base.set_auto_connect(false);
        base.set_auto_connect(false);
        assert_eq!(hits.load(Ordering::Relaxed), 3);
    }

    /// C parity: `asynPortDriver::setInterruptUInt32Digital` /
    /// `clearInterruptUInt32Digital` / `getInterruptUInt32Digital`
    /// (`asynPortDriver.cpp:2346-2461`) route through paramList. The
    /// PortDriver trait default delegates to the param store; we
    /// verify the round-trip end-to-end through the trait surface.
    #[test]
    fn test_port_driver_uint32_interrupt_round_trip() {
        struct UInt32Drv {
            base: PortDriverBase,
        }
        impl PortDriver for UInt32Drv {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut base = PortDriverBase::new("uint32_int", 1, PortFlags::default());
        let idx = base
            .params
            .create_param("BITS", ParamType::UInt32Digital)
            .unwrap();
        let mut drv = UInt32Drv { base };
        let user = AsynUser::new(idx).with_addr(0);

        drv.set_interrupt_uint32_digital(&user, 0xF0, InterruptReason::ZeroToOne)
            .unwrap();
        drv.set_interrupt_uint32_digital(&user, 0x0F, InterruptReason::OneToZero)
            .unwrap();
        assert_eq!(
            drv.get_interrupt_uint32_digital(&user, InterruptReason::Both)
                .unwrap(),
            0xFF
        );
        drv.clear_interrupt_uint32_digital(&user, 0x11).unwrap();
        assert_eq!(
            drv.get_interrupt_uint32_digital(&user, InterruptReason::ZeroToOne)
                .unwrap(),
            0xE0
        );
        assert_eq!(
            drv.get_interrupt_uint32_digital(&user, InterruptReason::OneToZero)
                .unwrap(),
            0x0E
        );
    }
}
