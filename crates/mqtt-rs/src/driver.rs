use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use asyn_rs::error::{AsynError, AsynResult};
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::user::AsynUser;
use tokio::sync::mpsc;

use crate::address::TopicAddress;
use crate::config::{MqttConfig, QoS};
use crate::error::MqttError;
use crate::payload::{DecodedValue, encode_payload, octet_cstr};

/// Request to publish a message to the MQTT broker.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub topic: String,
    pub payload: String,
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

pub struct MqttDriver {
    base: PortDriverBase,
    /// drvInfo string -> (param index, address)
    registry: HashMap<String, (usize, TopicAddress)>,
    /// MQTT topic -> list of (param index, address)
    topic_map: HashMap<String, Vec<(usize, TopicAddress)>>,
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
}

impl MqttDriver {
    /// Create a new MQTT driver with pre-declared topic addresses.
    ///
    /// All topics must be declared upfront because `drv_user_create(&self)`
    /// cannot mutate the driver to create new parameters at runtime.
    pub fn new(
        port_name: &str,
        config: &MqttConfig,
        topics: Vec<TopicAddress>,
        publish_tx: mpsc::UnboundedSender<PublishRequest>,
        connected: Arc<AtomicBool>,
    ) -> Self {
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
        let mut registry = HashMap::new();
        let mut topic_map: HashMap<String, Vec<(usize, TopicAddress)>> = HashMap::new();
        let mut reason_to_addr = Vec::new();

        // Create connection status param (0=disconnected, 1=connected)
        let connected_param = base
            .create_param(PARAM_CONNECTED, ParamType::Int32)
            .expect("failed to create connected param");
        base.set_int32_param(connected_param, 0, 0).unwrap();

        for addr in topics {
            let drv_info = addr.to_drv_info();
            let param_type = addr.param_type();
            let idx = base
                .create_param(&drv_info, param_type)
                .expect("failed to create param");

            // Grow reason_to_addr to accommodate this index
            if reason_to_addr.len() <= idx {
                reason_to_addr.resize_with(idx + 1, || None);
            }
            reason_to_addr[idx] = Some(addr.clone());

            topic_map
                .entry(addr.topic.clone())
                .or_default()
                .push((idx, addr.clone()));
            registry.insert(drv_info, (idx, addr));
        }

        Self {
            base,
            registry,
            topic_map,
            reason_to_addr,
            publish_tx,
            default_qos: config.qos,
            connected_param,
            connected,
        }
    }

    /// Get the set of MQTT topics this driver subscribes to.
    pub fn subscribed_topics(&self) -> Vec<String> {
        self.topic_map.keys().cloned().collect()
    }

    /// Get a clone of the topic map for the event loop.
    pub fn topic_map(&self) -> &HashMap<String, Vec<(usize, TopicAddress)>> {
        &self.topic_map
    }

    /// Encode and publish a value for the given parameter reason.
    /// Uses FLAT or JSON encoding depending on the topic address format.
    fn publish_value(&self, reason: usize, value: &DecodedValue) -> AsynResult<()> {
        // C parity: a publish while the broker is down throws
        // "MQTT client not connected" (mqttClient.cpp:70-72), surfaced as
        // asynError by the *Write handlers (drvMqtt.cpp:590-595/632-637) so the
        // output record fails with a WRITE/INVALID alarm. Gate here — before
        // touching the unbounded publish channel — so a disconnected write fails
        // (and is not silently buffered) exactly as in C.
        if !self.connected.load(Ordering::Acquire) {
            return Err(MqttError::NotConnected.into());
        }

        let addr = self
            .reason_to_addr
            .get(reason)
            .and_then(|a| a.as_ref())
            .ok_or_else(|| AsynError::ParamNotFound(format!("reason {reason}")))?;

        let payload = encode_payload(value, addr);

        self.publish_tx
            .send(PublishRequest {
                topic: addr.topic.clone(),
                payload,
                qos: self.default_qos,
                retained: false,
            })
            .map_err(|_| MqttError::PublishChannelClosed)?;

        Ok(())
    }
}

impl PortDriver for MqttDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn drv_user_create(&self, drv_info: &str) -> AsynResult<usize> {
        // Check topic registry first, then fall back to param name lookup
        // (for internal params like _MQTT_CONNECTED)
        if let Some((idx, _)) = self.registry.get(drv_info) {
            return Ok(*idx);
        }
        self.base()
            .params
            .find_param(drv_info)
            .ok_or_else(|| AsynError::ParamNotFound(drv_info.to_string()))
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.publish_value(user.reason, &DecodedValue::Int32(value))?;
        self.base.params.set_int32(user.reason, user.addr, value)?;
        self.base.call_param_callbacks(user.addr)
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.publish_value(user.reason, &DecodedValue::Float64(value))?;
        self.base
            .params
            .set_float64(user.reason, user.addr, value)?;
        self.base.call_param_callbacks(user.addr)
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
        // asyn octet values are NUL-terminated C-strings: C stringWrite publishes
        // std::string(stringData.data()), terminating the payload at the first NUL
        // (drvMqtt.cpp:716). Truncate at the first NUL so the published payload
        // (and the cached value, which holds the same octet) matches.
        let full = String::from_utf8_lossy(data).into_owned();
        let s = octet_cstr(&full).to_string();
        self.publish_value(user.reason, &DecodedValue::String(s.clone()))?;
        self.base.params.set_string(user.reason, user.addr, s)?;
        self.base.call_param_callbacks(user.addr)
    }

    fn write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        // C parity: MqttDriver::digitalWrite (drvMqtt.cpp:608-622) refuses a
        // partial-mask digital write until the current full value is known.
        // It reads getUIntDigitalParam(idx, &cur, 0xFFFFFFFF); if that returns
        // asynParamUndefined it throws "Masked write attempted on uninitialized
        // value" and returns asynError, because publishing only the masked bits
        // would overwrite the unknown bits with an assumed zero. A full-mask
        // (0xFFFFFFFF) write supplies every bit, so it may proceed with no prior
        // value. The lower asyn param library otherwise starts an undefined
        // UInt32Digital from zero (set_uint32), which is exactly what this guard
        // must prevent for a masked publish.
        if mask != 0xFFFF_FFFF {
            // Surfaces ParamUndefined as an error (C asynParamUndefined),
            // gating the start-from-zero merge below.
            self.base.params.get_uint32_strict(user.reason, user.addr)?;
        }
        // Device write interface: no forced interrupt mask (interrupt_mask = 0).
        self.base
            .params
            .set_uint32(user.reason, user.addr, value, mask, 0)?;
        let full_val = self
            .base
            .params
            .get_uint32(user.reason, user.addr)
            .unwrap_or(value & mask);
        self.publish_value(user.reason, &DecodedValue::UInt32(full_val))?;
        self.base.call_param_callbacks(user.addr)
    }

    fn write_int32_array(&mut self, user: &AsynUser, data: &[i32]) -> AsynResult<()> {
        self.publish_value(user.reason, &DecodedValue::Int32Array(data.to_vec()))?;
        self.base
            .params
            .set_int32_array(user.reason, user.addr, data.to_vec())?;
        self.base.call_param_callbacks(user.addr)
    }

    fn read_int32_array(&mut self, user: &AsynUser, buf: &mut [i32]) -> AsynResult<usize> {
        let data = self.base.params.get_int32_array(user.reason, user.addr)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    fn write_float64_array(&mut self, user: &AsynUser, data: &[f64]) -> AsynResult<()> {
        self.publish_value(user.reason, &DecodedValue::Float64Array(data.to_vec()))?;
        self.base
            .params
            .set_float64_array(user.reason, user.addr, data.to_vec())?;
        self.base.call_param_callbacks(user.addr)
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
        let addrs: Vec<TopicAddress> = topics
            .iter()
            .map(|s| TopicAddress::parse(s).unwrap())
            .collect();
        // These tests exercise the publish path, so start connected; the event
        // loop owns this flag at runtime. The disconnected gate is covered by
        // `write_while_disconnected_fails_and_does_not_publish`.
        let connected = Arc::new(AtomicBool::new(true));
        let driver = MqttDriver::new("TEST", &config, addrs, tx, connected);
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
        let addrs = vec![TopicAddress::parse("FLAT:INT test/int_topic").unwrap()];
        let mut driver = MqttDriver::new("TEST", &config, addrs, tx, connected.clone());
        let reason = driver.drv_user_create("FLAT:INT test/int_topic").unwrap();
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
        assert_eq!(rx.try_recv().unwrap().payload, "42");
    }

    #[test]
    fn drv_user_create_finds_registered_topics() {
        let (driver, _rx) = make_driver(&[
            "FLAT:INT test/int_topic",
            "FLAT:FLOAT test/float_topic",
            "JSON:FLOAT sensors/data humidity",
        ]);

        assert!(driver.drv_user_create("FLAT:INT test/int_topic").is_ok());
        assert!(
            driver
                .drv_user_create("FLAT:FLOAT test/float_topic")
                .is_ok()
        );
        assert!(
            driver
                .drv_user_create("JSON:FLOAT sensors/data humidity")
                .is_ok()
        );
    }

    #[test]
    fn drv_user_create_rejects_unknown() {
        let (driver, _rx) = make_driver(&["FLAT:INT test/topic"]);
        assert!(driver.drv_user_create("FLAT:FLOAT other/topic").is_err());
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
        let reason = driver.drv_user_create("FLAT:INT test/int_topic").unwrap();
        let mut user = AsynUser::new(reason);

        driver.write_int32(&mut user, 42).unwrap();

        let req = rx.try_recv().unwrap();
        assert_eq!(req.topic, "test/int_topic");
        assert_eq!(req.payload, "42");
    }

    #[test]
    fn write_float64_sends_publish_request() {
        let (mut driver, mut rx) = make_driver(&["FLAT:FLOAT test/float_topic"]);
        let reason = driver
            .drv_user_create("FLAT:FLOAT test/float_topic")
            .unwrap();
        let mut user = AsynUser::new(reason);

        driver.write_float64(&mut user, 3.15).unwrap();

        let req = rx.try_recv().unwrap();
        assert_eq!(req.topic, "test/float_topic");
        // C `std::to_string(double)` = "%f", fixed 6 decimals (drvMqtt.cpp:651).
        assert_eq!(req.payload, "3.150000");
    }

    #[test]
    fn write_octet_sends_publish_request() {
        let (mut driver, mut rx) = make_driver(&["FLAT:STRING test/str_topic"]);
        let reason = driver
            .drv_user_create("FLAT:STRING test/str_topic")
            .unwrap();
        let mut user = AsynUser::new(reason);

        driver.write_octet(&mut user, b"hello").unwrap();

        let req = rx.try_recv().unwrap();
        assert_eq!(req.topic, "test/str_topic");
        assert_eq!(req.payload, "hello");
    }

    /// C parity: stringWrite publishes std::string(stringData.data()), which
    /// terminates at the first NUL (drvMqtt.cpp:716). An embedded-NUL octet
    /// write must publish only the bytes up to that NUL, not the full buffer.
    #[test]
    fn write_octet_truncates_published_payload_at_first_nul() {
        let (mut driver, mut rx) = make_driver(&["FLAT:STRING test/str_topic"]);
        let reason = driver
            .drv_user_create("FLAT:STRING test/str_topic")
            .unwrap();
        let mut user = AsynUser::new(reason);

        driver.write_octet(&mut user, b"hi\0there").unwrap();

        let req = rx.try_recv().unwrap();
        assert_eq!(req.topic, "test/str_topic");
        assert_eq!(req.payload, "hi");
    }

    /// C parity: a partial-mask DIGITAL write before any current value is
    /// known must be rejected (MqttDriver::digitalWrite throws on
    /// asynParamUndefined, drvMqtt.cpp:613-614) and must not publish — the
    /// unknown bits cannot be safely overwritten with an assumed zero.
    #[test]
    fn masked_digital_write_rejected_when_value_uninitialized() {
        let (mut driver, mut rx) = make_driver(&["FLAT:DIGITAL test/bits"]);
        let reason = driver.drv_user_create("FLAT:DIGITAL test/bits").unwrap();
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

    /// Once the current value is known (here via a full-mask write, which C
    /// allows with no prior value), a partial-mask write merges the masked
    /// bits into it and publishes the composite — matching C's
    /// auxVal |= (value & mask); auxVal &= (value | ~mask) (drvMqtt.cpp:620-621).
    #[test]
    fn masked_digital_write_merges_after_current_value_known() {
        let (mut driver, mut rx) = make_driver(&["FLAT:DIGITAL test/bits"]);
        let reason = driver.drv_user_create("FLAT:DIGITAL test/bits").unwrap();
        let mut user = AsynUser::new(reason);

        // Full mask supplies every bit → allowed with no prior value.
        driver
            .write_uint32_digital(&mut user, 0x00f0, 0xffff_ffff)
            .unwrap();
        let req = rx.try_recv().unwrap();
        assert_eq!(req.topic, "test/bits");
        assert_eq!(req.payload, "240"); // 0x00f0

        // Partial mask now merges into the known 0x00f0:
        // (0x00f0 & ~0x000f) | (0x0005 & 0x000f) = 0x00f5 = 245.
        driver
            .write_uint32_digital(&mut user, 0x0005, 0x000f)
            .unwrap();
        let req = rx.try_recv().unwrap();
        assert_eq!(req.payload, "245");
    }

    #[test]
    fn topic_map_groups_by_mqtt_topic() {
        let (driver, _rx) = make_driver(&[
            "FLAT:INT test/shared",
            "FLAT:FLOAT test/shared",
            "FLAT:STRING test/other",
        ]);

        assert_eq!(driver.topic_map()["test/shared"].len(), 2);
        assert_eq!(driver.topic_map()["test/other"].len(), 1);
    }
}
