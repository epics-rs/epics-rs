use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use asyn_rs::error::{AsynError, AsynResult};
use asyn_rs::param::ParamType;
use asyn_rs::port::{DrvUserInfo, DrvUserRequest, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::user::AsynUser;
use tokio::sync::{Notify, mpsc};

use crate::address::{PayloadFormat, TopicAddress};
use crate::config::{MqttConfig, QoS};
use crate::error::MqttError;
use crate::payload::{DecodedValue, encode_payload, octet_bytes_cstr};

/// Request to publish a message to the MQTT broker.
///
/// `payload` is raw bytes, not a `String`: a FLAT octet write must carry the
/// raw octet bytes (up to the first NUL) on the wire, which need not be valid
/// UTF-8 (C `stringWrite` publishes `std::string(stringData.data())`,
/// drvMqtt.cpp:714-716). Text encodings (`encode_payload`) become bytes via
/// `into_bytes()`.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: QoS,
    pub retained: bool,
}

/// MQTT PortDriver implementation.
///
/// Maps MQTT topics to asyn parameters. Incoming MQTT messages update the param
/// cache and fire I/O Intr callbacks. EPICS writes are published to the broker
/// via an async channel.
/// Parameter index for the MQTT connection status.
pub const PARAM_CONNECTED: &str = "_MQTT_CONNECTED";

/// MQTT topic -> the records (param index + parsed address) bound to it.
pub(crate) type TopicMap = HashMap<String, Vec<(usize, TopicAddress)>>;

/// Shared, lock-guarded [`TopicMap`]: the driver (single-threaded port actor)
/// writes it as records bind in `drv_user_create`; the event loop snapshots it
/// once after iocInit.
pub(crate) type SharedTopicMap = Arc<Mutex<TopicMap>>;

pub struct MqttDriver {
    base: PortDriverBase,
    /// Canonical drvInfo (`TopicAddress::to_drv_info`) -> (param index, address).
    /// Dedups on-demand `drv_user_create` so two records referencing the same
    /// address share one parameter (C: `drvUserCreate` -> `createParam` dedups
    /// by `DeviceAddress::operator==`).
    registry: HashMap<String, (usize, TopicAddress)>,
    /// Per-topic config supplied before bind for fields the drvInfo grammar
    /// cannot express (currently only `normalize_on_off`, set by the Z2M
    /// builders for `/set` controls). Keyed by canonical drvInfo. On-demand
    /// creation consults this overlay so a record link carrying a bare drvInfo
    /// still picks up Z2M's write-side normalization. Empty for generic MQTT.
    overlay: HashMap<String, TopicAddress>,
    /// MQTT topic -> records bound to it. Shared with the event loop, which
    /// snapshots it after iocInit. The driver (single-threaded port actor) is
    /// the sole writer, in `drv_user_create`.
    topic_map: SharedTopicMap,
    /// param index -> topic address (for O(1) lookup on writes)
    reason_to_addr: Vec<Option<TopicAddress>>,
    /// Channel to send publish requests to the event loop
    publish_tx: mpsc::UnboundedSender<PublishRequest>,
    /// Default QoS for publishing
    default_qos: QoS,
    /// Param index for connection status (0=disconnected, 1=connected)
    pub connected_param: usize,
    /// Live broker-connection flag, written only by the event loop.
    ///
    /// C parity: `MqttClient::publish` throws "MQTT client not connected" when
    /// `!is_connected()` (mqttClient.cpp:70-72), which the `*Write` handlers
    /// surface as `asynError` (drvMqtt.cpp:590-595). The write path reads this
    /// flag to fail a publish while the broker is down instead of silently
    /// buffering it on the unbounded channel.
    connected: Arc<AtomicBool>,
    /// Signalled when this driver is dropped, so the event loop can send a
    /// DISCONNECT before it exits.
    ///
    /// C parity (MQ4): `~MqttClient` calls `disconnect()`
    /// (mqttClient.cpp:37-41), which sends a DISCONNECT and waits for it
    /// whenever the session is up (`client_.disconnect()->wait()`,
    /// mqttClient.cpp:51-55). The port actor owns this driver, so dropping it
    /// is the same teardown point C destructs at.
    shutdown: Arc<Notify>,
    /// The event loop's half of the teardown rendezvous, until
    /// [`Self::teardown_ack`] hands it over. While it is still here no event
    /// loop exists, which is exactly when `Drop` has nothing to wait for.
    teardown_tx: Option<std::sync::mpsc::Sender<()>>,
    /// This side of it. `Mutex` only because [`PortDriver`] is `Sync` and a
    /// `Receiver` is not; the port actor owns the driver exclusively, so it is
    /// never contended and `Drop` reads it through `get_mut`.
    teardown_rx: Mutex<std::sync::mpsc::Receiver<()>>,
}

/// How long `Drop` waits for the event loop to finish the teardown it just
/// asked for.
///
/// C does not bound this at all — `client_.disconnect()->wait()`
/// (mqttClient.cpp:51-55) — because paho's own I/O thread writes the packet and
/// needs nothing from the caller. Ours needs the event-loop task to be
/// scheduled and to get its param updates into the port actor's inbox, and the
/// actor is this very thread; a full inbox would therefore never drain. So the
/// wait is bounded, and it sits under asyn's five-second
/// `PORT_EXIT_STOP_TIMEOUT` so the driver reports its own failure rather than
/// being cut off mid-wait by the port-level one.
const TEARDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// C parity (MQ4): `~MqttClient` disconnects (mqttClient.cpp:37-41). The port
/// actor owns the driver, so this is the teardown point; the event loop turns
/// the signal into the DISCONNECT packet because it owns the rumqttc client.
///
/// Raising the signal is not the teardown — C's `disconnect()` *waits* for the
/// packet (`->wait()`, mqttClient.cpp:51-55), on the thread that is exiting.
/// Without that wait this `Drop` returns the instant it has asked, asyn's
/// `stop_port_actor` sees the actor stop, `call_at_exits` returns and the
/// process exits out from under the event-loop task that was going to write the
/// packet: measured on this box, the DISCONNECT reached the broker 22 times out
/// of 30. So `Drop` returning must mean the broker has been told, which is what
/// it means for every other asyn driver — `DrvAsynSerialPort::drop` calls
/// `disconnect` and it completes before `drop` returns.
impl Drop for MqttDriver {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        if self.teardown_tx.is_some() {
            // Nobody took the event loop's half, so no event loop exists: a
            // unit-test driver, or a `mqttDriverConfigure` that failed before
            // it could spawn one. Nothing is coming.
            return;
        }
        let rx = self
            .teardown_rx
            .get_mut()
            .unwrap_or_else(|e| e.into_inner());
        match rx.recv_timeout(TEARDOWN_TIMEOUT) {
            // The loop ended properly (`Ok`), or its task died with the runtime
            // before it could say so (`Disconnected`). Either way nothing more
            // is coming and there is nothing left to wait for.
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            // Loud, and on stderr, for the reason asyn's own timeout is: this
            // is the report that the broker was NOT told, and it happens on the
            // way out of a process whose `tracing` subscriber may already be
            // gone.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => eprintln!(
                "mqtt: port '{}' did not get its DISCONNECT out within \
                 {TEARDOWN_TIMEOUT:?}; the broker was not told",
                self.base.port_name
            ),
        }
    }
}

impl MqttDriver {
    /// Create a new MQTT driver.
    ///
    /// C parity: `Autoparam::Driver` is born with no topic parameters; each is
    /// created on demand as a record binds (`drvUserCreate` -> `createParam`).
    /// This driver mirrors that — only the internal `_MQTT_CONNECTED` param
    /// exists at construction; topic params are created in `drv_user_create`.
    ///
    /// `overlay_topics` carries pre-bind per-topic configuration the drvInfo
    /// grammar cannot express (the Z2M builders' `normalize_on_off`). These are
    /// **not** created here; they are stored as an overlay that on-demand
    /// creation consults. Generic MQTT passes an empty vector.
    pub fn new(
        port_name: &str,
        config: &MqttConfig,
        overlay_topics: Vec<TopicAddress>,
        publish_tx: mpsc::UnboundedSender<PublishRequest>,
        connected: Arc<AtomicBool>,
    ) -> Self {
        let (teardown_tx, teardown_rx) = std::sync::mpsc::channel();
        // C parity: drvMqtt sets `.setBlocking(false)` (drvMqtt.cpp:122) — the
        // MQTT port is non-blocking. The Rust write path only `send`s on an
        // mpsc channel (publish_value) and reads serve from the param cache, so
        // nothing blocks; declaring ASYN_CANBLOCK would needlessly defer record
        // I/O two-phase (PACT) where C completes inline.
        let flags = PortFlags {
            can_block: false,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new(port_name, 1, flags);

        // Create connection status param (0=disconnected, 1=connected)
        let connected_param = base
            .create_param(PARAM_CONNECTED, ParamType::Int32)
            .expect("failed to create connected param");
        base.set_int32_param(connected_param, 0, 0).unwrap();

        let overlay = overlay_topics
            .into_iter()
            .map(|addr| (addr.to_drv_info(), addr))
            .collect();

        Self {
            base,
            registry: HashMap::new(),
            overlay,
            topic_map: Arc::new(Mutex::new(HashMap::new())),
            reason_to_addr: Vec::new(),
            publish_tx,
            default_qos: config.qos,
            connected_param,
            connected,
            shutdown: Arc::new(Notify::new()),
            teardown_tx: Some(teardown_tx),
            teardown_rx: Mutex::new(teardown_rx),
        }
    }

    /// The teardown signal the event loop waits on — see the `shutdown` field.
    /// Take a clone before handing the driver to the port runtime, exactly as
    /// [`Self::topic_map`] is taken.
    pub fn shutdown_signal(&self) -> Arc<Notify> {
        self.shutdown.clone()
    }

    /// Take the event loop's half of the teardown rendezvous.
    ///
    /// The event loop owns this sender for its whole life, so *every* way that
    /// task can end — returning after the DISCONNECT, returning because there
    /// was no session to close, or being dropped with the tokio runtime —
    /// releases the `Drop` waiting on the other end. That is what keeps the
    /// wait bounded by the loop's own lifetime rather than by
    /// `TEARDOWN_TIMEOUT`, and it is why no exit path added to the loop later
    /// can leave a shutting-down IOC waiting out the full timeout.
    ///
    /// Handed over exactly once, before the port actor takes the driver.
    pub fn teardown_ack(&mut self) -> std::sync::mpsc::Sender<()> {
        self.teardown_tx.take().expect(
            "the event loop's teardown half is handed over exactly once, \
             before the port actor takes the driver",
        )
    }

    /// Get the set of MQTT topics created so far (those a record has bound).
    pub fn subscribed_topics(&self) -> Vec<String> {
        self.topic_map.lock().unwrap().keys().cloned().collect()
    }

    /// A handle to the shared topic map for the event loop, which snapshots it
    /// after iocInit (every record has bound by then; records are never created
    /// at runtime in EPICS, so the map is final).
    pub fn topic_map(&self) -> SharedTopicMap {
        Arc::clone(&self.topic_map)
    }

    /// Gate every write while the broker is down.
    ///
    /// C parity: a publish while disconnected throws "MQTT client not
    /// connected" (mqttClient.cpp:70-72), surfaced as asynError by the *Write
    /// handlers (drvMqtt.cpp:590-595/632-637) so the output record fails with a
    /// WRITE/INVALID alarm. This is the single owner of the gate condition; a
    /// write handler MUST reach it before producing any observable side effect
    /// (param-cache commit, callback, channel send) so a disconnected write
    /// fails cleanly and is never silently buffered — exactly as in C.
    fn ensure_connected(&self) -> AsynResult<()> {
        if self.connected.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(MqttError::NotConnected.into())
        }
    }

    /// Gate, resolve the topic for `reason`, and enqueue a publish with the
    /// already-encoded `payload` bytes. Single owner of the connection gate and
    /// the channel send for every write handler.
    fn publish_bytes(&self, reason: usize, payload: Vec<u8>) -> AsynResult<()> {
        // Gate before touching the unbounded publish channel. The five
        // commit-after-publish handlers reach the gate here first; the digital
        // handler (which commits its cache before publishing) gates explicitly
        // at its top.
        self.ensure_connected()?;

        let topic = self
            .reason_to_addr
            .get(reason)
            .and_then(|a| a.as_ref())
            .ok_or_else(|| AsynError::ParamNotFound(format!("reason {reason}")))?
            .topic
            .clone();

        self.publish_tx
            .send(PublishRequest {
                topic,
                payload,
                qos: self.default_qos,
                retained: false,
            })
            .map_err(|_| MqttError::PublishChannelClosed)?;

        Ok(())
    }

    /// Encode and publish a value for the given parameter reason.
    /// Uses FLAT or JSON encoding depending on the topic address format.
    fn publish_value(&self, reason: usize, value: &DecodedValue) -> AsynResult<()> {
        let addr = self
            .reason_to_addr
            .get(reason)
            .and_then(|a| a.as_ref())
            .ok_or_else(|| AsynError::ParamNotFound(format!("reason {reason}")))?;
        // encode_payload is pure (no observable side effect), so resolving the
        // addr and encoding before the gate inside publish_bytes is safe.
        let payload = encode_payload(value, addr).into_bytes();
        self.publish_bytes(reason, payload)
    }
}

impl PortDriver for MqttDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn drv_user_create(&mut self, req: &DrvUserRequest) -> AsynResult<DrvUserInfo> {
        // The MQTT drvInfo names the payload type (`FLAT:INT`, `JSON:STRING`, …),
        // so the on-demand parameter type comes from the drvInfo, not from the
        // bound record's interface (`req.iface`).
        let drv_info = req.drv_info.as_str();
        // C parity: `Autoparam::Driver::drvUserCreate` parses the reason into a
        // device address and creates the parameter on demand, reusing the
        // existing one for an equal address (`createParam` dedup by
        // `DeviceAddress::operator==`). A reason that is not a topic address
        // (e.g. the internal `_MQTT_CONNECTED` param) falls through to a plain
        // parameter-name lookup.
        let parsed = match TopicAddress::parse(drv_info) {
            Ok(addr) => addr,
            Err(_) => {
                let reason = self
                    .base()
                    .params
                    .find_param(drv_info)
                    .ok_or_else(|| AsynError::ParamNotFound(drv_info.to_string()))?;
                return Ok(DrvUserInfo::from_reason(reason));
            }
        };

        // Canonical form is the parameter key and the dedup identity.
        let canonical = parsed.to_drv_info();
        if let Some((idx, _)) = self.registry.get(&canonical) {
            return Ok(DrvUserInfo::from_reason(*idx));
        }

        // An overlay entry (Z2M `normalize_on_off`) supersedes the parsed
        // address; otherwise the parsed address is used verbatim.
        let addr = self.overlay.get(&canonical).cloned().unwrap_or(parsed);
        let param_type = addr.param_type();
        let idx = self.base_mut().create_param(&canonical, param_type)?;

        if self.reason_to_addr.len() <= idx {
            self.reason_to_addr.resize_with(idx + 1, || None);
        }
        self.reason_to_addr[idx] = Some(addr.clone());

        self.topic_map
            .lock()
            .unwrap()
            .entry(addr.topic.clone())
            .or_default()
            .push((idx, addr.clone()));
        self.registry.insert(canonical, (idx, addr));

        Ok(DrvUserInfo::from_reason(idx))
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        // C parity (MQ51): integerWrite (drvMqtt.cpp:573-598) is publish-only.
        // drvMqtt runs setAutoInterrupts(false) (drvMqtt.cpp:123) and its write
        // handlers leave processInterrupts at DEFAULT, so shouldProcessInterrupts
        // is false (autoparamDriver.cpp:1033-1036,437-442): the base class skips
        // setParam + callParamCallbacks. The cache + I/O Intr readback come only
        // from the broker echo (onMessageCb), never from the write itself.
        self.publish_value(user.reason, &DecodedValue::Int32(value))
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        // C parity (MQ51): floatWrite (drvMqtt.cpp:642-658) is publish-only — see
        // `write_int32`. No cache commit, no post; readback is broker-echo-only.
        self.publish_value(user.reason, &DecodedValue::Float64(value))
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        // asyn octet values are NUL-terminated C-strings: C stringWrite publishes
        // std::string(stringData.data()), terminating the payload at the first
        // NUL (drvMqtt.cpp:714-716). Take the raw bytes up to that NUL.
        let raw = octet_bytes_cstr(data);
        // Bytes transferred = the caller's full byte count, not the published
        // length. `Autoparam::Driver::writeOctet` sets `*nActual = nChars`
        // unconditionally (autoparamDriver.cpp:1495, v2.1.0 `2159559`) — commented
        // "Only complete writes are supported" (:1494) — before it ever dispatches
        // to `writeOctetData` (:1496), so the handler cannot influence the count.
        // The payload still stops at the NUL above; only the count is the whole
        // buffer. (Contrast modbus, whose writeOctet derives *nActual from what
        // fit the register buffer, drvModbusAsyn.cpp:1548-1552.)
        let nbytes = data.len();
        // Copy the format (Copy enum) so the immutable reason_to_addr borrow is
        // dropped before the mutable cache store below.
        let format = self
            .reason_to_addr
            .get(user.reason)
            .and_then(|a| a.as_ref())
            .ok_or_else(|| AsynError::ParamNotFound(format!("reason {}", user.reason)))?
            .format;
        match format {
            // FLAT publishes the raw octet bytes verbatim — they need not be
            // valid UTF-8, and C does no re-encoding. Earlier the path forced
            // them through String::from_utf8_lossy, corrupting a binary /
            // waveform-CHAR write to U+FFFD on the wire (MQ39).
            PayloadFormat::Flat => self.publish_bytes(user.reason, raw.to_vec())?,
            // JSON octet write is unimplemented in C (stringWrite throws
            // logic_error, drvMqtt.cpp:720-722) and a JSON string value must be
            // UTF-8 anyway, so this path keeps the String encoding.
            PayloadFormat::Json => {
                let s = String::from_utf8_lossy(raw).into_owned();
                self.publish_value(user.reason, &DecodedValue::String(s))?;
            }
        }
        // C parity (MQ51): stringWrite (drvMqtt.cpp:700-733) is publish-only —
        // setAutoInterrupts(false) means a successful write neither commits the
        // param cache nor posts; the octet readback comes only from the broker
        // echo (onMessageCb → setStringParam). See `write_int32`.
        Ok(nbytes)
    }

    fn write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        // C parity (MQ51): MqttDriver::digitalWrite (drvMqtt.cpp:600-639) is
        // publish-only and merges a partial-mask write against the *current
        // cached value* — which is populated solely by inbound broker messages,
        // since a write never commits the cache (setAutoInterrupts(false), see
        // `write_int32`). It reads getUIntDigitalParam(idx, &cur, 0xFFFFFFFF) and,
        // if that value was never received (asynParamUndefined), throws "Masked
        // write attempted on uninitialized value" rather than overwriting the
        // unknown bits with an assumed zero. A full-mask write supplies every bit,
        // so it needs no prior value. The merge reads but never writes the cache,
        // and nothing is posted — the readback is the broker echo. The read
        // precedes the publish, so (as in C) the uninitialized error surfaces
        // before the connection gate inside `publish_value`.
        let full_val = if mask == 0xFFFF_FFFF {
            value
        } else {
            // Surfaces ParamUndefined as an error (C asynParamUndefined); the
            // merge must not assume zero for the unknown bits.
            let current = self.base.params.get_uint32_strict(user.reason, user.addr)?;
            // C: auxVal |= (value & mask); auxVal &= (value | ~mask)
            //  ≡ (current & ~mask) | (value & mask) (drvMqtt.cpp:620-621).
            (current & !mask) | (value & mask)
        };
        self.publish_value(user.reason, &DecodedValue::UInt32(full_val))
    }

    fn write_int32_array(&mut self, user: &AsynUser, data: &[i32]) -> AsynResult<()> {
        // C parity (MQ51): writeArray (autoparamDriver.cpp:1071-1082) posts only
        // when shouldProcessInterrupts holds, which is false under drvMqtt's
        // setAutoInterrupts(false); the mqtt array write is publish-only. See
        // `write_int32`. No cache commit, no post.
        self.publish_value(user.reason, &DecodedValue::Int32Array(data.to_vec()))
    }

    fn read_int32_array(&mut self, user: &AsynUser, buf: &mut [i32]) -> AsynResult<usize> {
        let data = self.base.params.get_int32_array(user.reason, user.addr)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    fn write_float64_array(&mut self, user: &AsynUser, data: &[f64]) -> AsynResult<()> {
        // C parity (MQ51): publish-only — see `write_int32_array`.
        self.publish_value(user.reason, &DecodedValue::Float64Array(data.to_vec()))
    }

    fn read_float64_array(&mut self, user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize> {
        let data = self.base.params.get_float64_array(user.reason, user.addr)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_driver(topics: &[&str]) -> (MqttDriver, mpsc::UnboundedReceiver<PublishRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let config = MqttConfig::default();
        // These tests exercise the publish path, so start connected; the event
        // loop owns this flag at runtime. The disconnected gate is covered by
        // `write_while_disconnected_fails_and_does_not_publish`.
        let connected = Arc::new(AtomicBool::new(true));
        // Born-empty (no overlay); bind each topic on demand exactly as a record
        // would (`drv_user_create`).
        let mut driver = MqttDriver::new("TEST", &config, Vec::new(), tx, connected);
        for t in topics {
            driver
                .drv_user_create(&DrvUserRequest::new(*t, 0))
                .expect("on-demand create");
        }
        (driver, rx)
    }

    /// C parity: a publish while the broker is down throws "MQTT client not
    /// connected" (mqttClient.cpp:70-72), surfaced as asynError by the *Write
    /// handlers (drvMqtt.cpp:590-595). The write must fail (WRITE alarm) and
    /// must not buffer the value, rather than silently returning asynSuccess.
    #[test]
    fn write_while_disconnected_fails_and_does_not_publish() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let config = MqttConfig::default();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = MqttDriver::new("TEST", &config, Vec::new(), tx, connected.clone());
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:INT test/int_topic", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        let r = driver.write_int32(&mut user, 42);
        assert!(r.is_err(), "disconnected write must fail, got {r:?}");
        assert!(
            rx.try_recv().is_err(),
            "a failed write must not enqueue a publish"
        );

        // Once the event loop marks the session up, the same write publishes.
        connected.store(true, Ordering::Release);
        driver.write_int32(&mut user, 42).unwrap();
        assert_eq!(rx.try_recv().unwrap().payload, b"42");
    }

    /// C parity (MQ51): a write never commits the param cache — only an inbound
    /// broker message does. This is most visible on the digital path's masked
    /// guard: a disconnected full-mask write fails (and, like every write, would
    /// not commit even if it succeeded), so a later masked write still hits the
    /// uninitialized guard rather than merging against a phantom value.
    #[test]
    fn disconnected_digital_write_does_not_commit_cache() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let config = MqttConfig::default();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = MqttDriver::new("TEST", &config, Vec::new(), tx, connected.clone());
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:DIGITAL test/digital_topic", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        // Disconnected full-mask write: must fail, must not publish, must not
        // commit the param cache.
        let r = driver.write_uint32_digital(&mut user, 0x00f0, 0xFFFF_FFFF);
        assert!(
            r.is_err(),
            "disconnected digital write must fail, got {r:?}"
        );
        assert!(
            rx.try_recv().is_err(),
            "a failed digital write must not enqueue a publish"
        );

        // Reconnect, then attempt a masked write. If the disconnected write had
        // committed 0x00f0 (the defect), the cache would now be defined and this
        // masked write would succeed by merging. Because the cache stayed
        // untouched, the uninitialized guard fires and the masked write fails.
        connected.store(true, Ordering::Release);
        let masked = driver.write_uint32_digital(&mut user, 0x0005, 0x000f);
        assert!(
            masked.is_err(),
            "masked write must fail on an uninitialized value — a phantom commit \
             from the disconnected write would have let it merge, got {masked:?}"
        );
        assert!(
            rx.try_recv().is_err(),
            "the rejected masked write must not enqueue a publish"
        );

        // A full-mask write supplies every bit, so it proceeds and publishes.
        driver
            .write_uint32_digital(&mut user, 0x00f0, 0xFFFF_FFFF)
            .unwrap();
        assert_eq!(rx.try_recv().unwrap().payload, b"240");
    }

    #[test]
    fn drv_user_create_dedups_repeated_binds() {
        // C parity: two records referencing the same address share one parameter
        // (`createParam` dedup by `DeviceAddress::operator==`). Re-binding the
        // same drvInfo returns the same reason; a different address gets its own.
        let (mut driver, _rx) = make_driver(&[]);
        let a1 = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:INT test/int_topic", 0))
            .unwrap()
            .reason;
        let a2 = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:INT test/int_topic", 0))
            .unwrap()
            .reason;
        assert_eq!(a1, a2, "same address must reuse the same parameter");

        let b = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:FLOAT test/float_topic", 0))
            .unwrap()
            .reason;
        assert_ne!(a1, b, "a distinct address gets its own parameter");
    }

    #[test]
    fn drv_user_create_creates_on_demand_and_rejects_invalid() {
        // Born-empty driver: a valid topic never pre-registered is created on
        // demand when a record binds (C: `Autoparam::Driver` lazy `createParam`).
        let (mut driver, _rx) = make_driver(&[]);
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("FLAT:FLOAT other/topic", 0))
                .is_ok()
        );
        // A drvInfo that is neither a valid topic address nor an internal param
        // name is rejected.
        assert!(
            driver
                .drv_user_create(&DrvUserRequest::new("not a topic address", 0))
                .is_err()
        );
    }

    #[test]
    fn on_demand_create_applies_overlay_config() {
        // A Z2M `/set` control pre-registers `normalize_on_off` through the
        // overlay; the record link carries only the bare drvInfo (the grammar
        // cannot express the flag), so on-demand creation must recover it from
        // the overlay rather than from the parsed address.
        let (tx, _rx) = mpsc::unbounded_channel();
        let config = MqttConfig::default();
        let connected = Arc::new(AtomicBool::new(true));
        let mut overlaid = TopicAddress::parse("JSON:STRING zigbee/x state").unwrap();
        overlaid.normalize_on_off = true;
        let mut driver = MqttDriver::new("TEST", &config, vec![overlaid], tx, connected);

        let on_demand = driver
            .drv_user_create(&DrvUserRequest::new("JSON:STRING zigbee/x state", 0))
            .unwrap()
            .reason;
        assert!(
            driver.reason_to_addr[on_demand]
                .as_ref()
                .unwrap()
                .normalize_on_off,
            "on-demand create must apply the overlay's normalize_on_off flag"
        );

        // A topic with no overlay entry uses the parsed address verbatim.
        let plain = driver
            .drv_user_create(&DrvUserRequest::new("JSON:STRING zigbee/y state", 0))
            .unwrap()
            .reason;
        assert!(
            !driver.reason_to_addr[plain]
                .as_ref()
                .unwrap()
                .normalize_on_off
        );
    }

    #[test]
    fn subscribed_topics_returns_unique_mqtt_topics() {
        let (driver, _rx) = make_driver(&[
            "FLAT:INT test/topic",
            "FLAT:FLOAT test/topic",
            "FLAT:STRING other/topic",
        ]);

        let mut topics = driver.subscribed_topics();
        topics.sort();
        assert_eq!(topics, vec!["other/topic", "test/topic"]);
    }

    #[test]
    fn write_int32_sends_publish_request() {
        let (mut driver, mut rx) = make_driver(&["FLAT:INT test/int_topic"]);
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:INT test/int_topic", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        driver.write_int32(&mut user, 42).unwrap();

        let req = rx.try_recv().unwrap();
        assert_eq!(req.topic, "test/int_topic");
        assert_eq!(req.payload, b"42");
    }

    #[test]
    fn write_float64_sends_publish_request() {
        let (mut driver, mut rx) = make_driver(&["FLAT:FLOAT test/float_topic"]);
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:FLOAT test/float_topic", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        driver.write_float64(&mut user, 3.15).unwrap();

        let req = rx.try_recv().unwrap();
        assert_eq!(req.topic, "test/float_topic");
        // C `std::to_string(double)` = "%f", fixed 6 decimals (drvMqtt.cpp:651).
        assert_eq!(req.payload, b"3.150000");
    }

    #[test]
    fn write_octet_sends_publish_request() {
        let (mut driver, mut rx) = make_driver(&["FLAT:STRING test/str_topic"]);
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:STRING test/str_topic", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        driver.write_octet(&mut user, b"hello").unwrap();

        let req = rx.try_recv().unwrap();
        assert_eq!(req.topic, "test/str_topic");
        assert_eq!(req.payload, b"hello");
    }

    /// C parity: stringWrite publishes std::string(stringData.data()), which
    /// terminates at the first NUL (drvMqtt.cpp:716). An embedded-NUL octet
    /// write must publish only the bytes up to that NUL, not the full buffer.
    #[test]
    fn write_octet_truncates_published_payload_at_first_nul() {
        let (mut driver, mut rx) = make_driver(&["FLAT:STRING test/str_topic"]);
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:STRING test/str_topic", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        driver.write_octet(&mut user, b"hi\0there").unwrap();

        let req = rx.try_recv().unwrap();
        assert_eq!(req.topic, "test/str_topic");
        assert_eq!(req.payload, b"hi");
    }
    /// `*nbytesTransfered` for an embedded-NUL octet write.
    ///
    /// C, at the declared autoparam pin (`v2.1.0`, `2159559`,
    /// `/home/stevek/work/epics-modules/autoparamDriver`):
    /// `Autoparam::Driver::writeOctet` (`autoparamDriver.cpp:1484-1500`) sets
    /// `*nActual = nChars` **unconditionally** at `:1495`, commented "Only
    /// complete writes are supported" (`:1494`), *before* it dispatches to the
    /// handler via `writeOctetData` (`:1496`). So C reports the full count asyn
    /// handed down — 8 for `b"hi\0there"` — whatever the payload contains, and
    /// `stringWrite` never gets to influence it.
    ///
    /// `write_octet` returned `raw.len()` — the length up to the first NUL, 2 —
    /// until this was corrected to the caller's count. `raw.len()` is the
    /// semantically defensible number, being what actually reached the wire, but
    /// it is not what C reports and asyn callers read the count, not the wire.
    ///
    /// The wire payload is *not* in dispute: both sides publish `b"hi"` only
    /// (`drvMqtt.cpp:714-716`, covered by
    /// `write_octet_truncates_published_payload_at_first_nul` above). Whoever
    /// takes this decision must move the count without moving the payload,
    /// which is why this test asserts both.
    ///
    /// Both halves are asserted here because they move independently: the count
    /// is the caller's buffer, the payload stops at the NUL.
    #[test]
    fn write_octet_reports_c_s_full_nbytes_transfered() {
        let (mut driver, mut rx) = make_driver(&["FLAT:STRING test/str_topic"]);
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:STRING test/str_topic", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        let buf = b"hi\0there";
        let nbytes = driver.write_octet(&mut user, buf).unwrap();

        // C: *nActual = nChars (autoparamDriver.cpp:1495).
        assert_eq!(
            nbytes,
            buf.len(),
            "C reports the full nChars it was handed, not the pre-NUL length"
        );
        // ... while the published payload stays truncated on both sides.
        assert_eq!(rx.try_recv().unwrap().payload, b"hi");
    }

    /// MQ39: a non-UTF-8 FLAT octet write reaches the wire byte-for-byte. C
    /// publishes std::string(stringData.data()) — raw bytes up to the first NUL
    /// with no re-encoding (drvMqtt.cpp:714-716). The old from_utf8_lossy path
    /// turned each invalid byte into U+FFFD (0xEF 0xBF 0xBD), corrupting a
    /// binary / waveform-CHAR payload.
    #[test]
    fn write_octet_flat_publishes_raw_non_utf8_bytes() {
        let (mut driver, mut rx) = make_driver(&["FLAT:STRING test/bin"]);
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:STRING test/bin", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        // 0xFF 0xFE 0x01 is not valid UTF-8; it must pass through unchanged.
        driver.write_octet(&mut user, &[0xFF, 0xFE, 0x01]).unwrap();
        let req = rx.try_recv().unwrap();
        assert_eq!(req.payload, vec![0xFF, 0xFE, 0x01]);

        // NUL truncation still applies to the raw bytes.
        driver.write_octet(&mut user, &[0xFF, 0x00, 0xFE]).unwrap();
        assert_eq!(rx.try_recv().unwrap().payload, vec![0xFF]);
    }

    /// C parity: a partial-mask DIGITAL write before any current value is
    /// known must be rejected (MqttDriver::digitalWrite throws on
    /// asynParamUndefined, drvMqtt.cpp:613-614) and must not publish — the
    /// unknown bits cannot be safely overwritten with an assumed zero.
    #[test]
    fn masked_digital_write_rejected_when_value_uninitialized() {
        let (mut driver, mut rx) = make_driver(&["FLAT:DIGITAL test/bits"]);
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:DIGITAL test/bits", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        let r = driver.write_uint32_digital(&mut user, 0x0005, 0x000f);
        assert!(
            matches!(r, Err(AsynError::ParamUndefined(_))),
            "masked write on uninitialized value must be rejected, got {r:?}"
        );
        assert!(
            rx.try_recv().is_err(),
            "a rejected masked write must not publish"
        );
    }

    /// Once the current value is known — populated by an inbound broker message,
    /// the ONLY path that sets the digital cache (C: a write is publish-only,
    /// setAutoInterrupts(false)) — a partial-mask write merges the masked bits
    /// into it and publishes the composite, matching C's
    /// auxVal |= (value & mask); auxVal &= (value | ~mask) (drvMqtt.cpp:620-621).
    #[test]
    fn masked_digital_write_merges_against_inbound_value() {
        let (mut driver, mut rx) = make_driver(&["FLAT:DIGITAL test/bits"]);
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:DIGITAL test/bits", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        // Simulate the broker echo arriving on the subscribed topic — the inbound
        // path (onMessageCb → setUIntDigitalParam) is what populates the cache. A
        // full-mask *write* does not, so the merge below reads this value only.
        driver
            .base
            .params
            .set_uint32(reason, 0, 0x00f0, 0xffff_ffff, 0)
            .unwrap();

        // Partial mask now merges into the inbound 0x00f0:
        // (0x00f0 & ~0x000f) | (0x0005 & 0x000f) = 0x00f5 = 245.
        driver
            .write_uint32_digital(&mut user, 0x0005, 0x000f)
            .unwrap();
        let req = rx.try_recv().unwrap();
        assert_eq!(req.topic, "test/bits");
        assert_eq!(req.payload, b"245");
    }

    /// C parity (MQ51): a *successful* write is publish-only — it does NOT commit
    /// the param cache (setAutoInterrupts(false) → shouldProcessInterrupts false,
    /// no setParam/callParamCallbacks; autoparamDriver.cpp:1033-1036,437-442).
    /// A full-mask write that publishes thus leaves the cache undefined, so a
    /// following masked write hits the uninitialized guard exactly as in C —
    /// where under the old optimistic-post behaviour it would have merged.
    #[test]
    fn successful_write_does_not_commit_cache() {
        let (mut driver, mut rx) = make_driver(&["FLAT:DIGITAL test/bits"]);
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:DIGITAL test/bits", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        // Connected full-mask write: publishes, but must not commit the cache.
        driver
            .write_uint32_digital(&mut user, 0x00f0, 0xffff_ffff)
            .unwrap();
        assert_eq!(rx.try_recv().unwrap().payload, b"240");

        // Cache still undefined → the masked merge hits the guard (it would have
        // merged against 0x00f0 under the old post-on-write behaviour).
        let masked = driver.write_uint32_digital(&mut user, 0x0005, 0x000f);
        assert!(
            matches!(masked, Err(AsynError::ParamUndefined(_))),
            "a write must not populate the cache; the masked merge must hit the \
             uninitialized guard, got {masked:?}"
        );
        assert!(rx.try_recv().is_err());
    }

    /// C parity (MQ51): the scalar handlers are publish-only too — a successful
    /// `write_int32` publishes but leaves the param cache undefined (the readback
    /// arrives only via the broker echo). Representative of the int32/float64/
    /// octet/array family, which all drop the write-side commit + post.
    #[test]
    fn successful_scalar_write_does_not_commit_cache() {
        let (mut driver, mut rx) = make_driver(&["FLAT:INT test/int_topic"]);
        let reason = driver
            .drv_user_create(&DrvUserRequest::new("FLAT:INT test/int_topic", 0))
            .unwrap()
            .reason;
        let mut user = AsynUser::new(reason);

        driver.write_int32(&mut user, 42).unwrap();
        assert_eq!(rx.try_recv().unwrap().payload, b"42");
        assert!(
            matches!(
                driver.base.params.get_int32_strict(reason, 0),
                Err(AsynError::ParamUndefined(_))
            ),
            "a successful scalar write must not commit the param cache"
        );
    }

    #[test]
    fn topic_map_groups_by_mqtt_topic() {
        let (driver, _rx) = make_driver(&[
            "FLAT:INT test/shared",
            "FLAT:FLOAT test/shared",
            "FLAT:STRING test/other",
        ]);

        let topic_map = driver.topic_map();
        let topic_map = topic_map.lock().unwrap();
        assert_eq!(topic_map["test/shared"].len(), 2);
        assert_eq!(topic_map["test/other"].len(), 1);
    }
}
