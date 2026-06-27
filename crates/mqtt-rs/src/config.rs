/// MQTT Quality of Service level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QoS {
    AtMostOnce = 0,
    #[default]
    AtLeastOnce = 1,
    ExactlyOnce = 2,
}

impl QoS {
    pub fn from_int(v: i32) -> Self {
        match v {
            0 => Self::AtMostOnce,
            2 => Self::ExactlyOnce,
            _ => Self::AtLeastOnce,
        }
    }
}

impl From<QoS> for rumqttc::v5::mqttbytes::QoS {
    fn from(q: QoS) -> Self {
        use rumqttc::v5::mqttbytes::QoS as V5;
        match q {
            QoS::AtMostOnce => V5::AtMostOnce,
            QoS::AtLeastOnce => V5::AtLeastOnce,
            QoS::ExactlyOnce => V5::ExactlyOnce,
        }
    }
}

/// MQTT driver configuration.
#[derive(Debug, Clone)]
pub struct MqttConfig {
    pub broker_host: String,
    pub broker_port: u16,
    pub client_id: String,
    pub qos: QoS,
    pub keep_alive_secs: u64,
    pub clean_session: bool,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            broker_host: "localhost".into(),
            broker_port: 1883,
            client_id: "epics-mqtt".into(),
            qos: QoS::default(),
            keep_alive_secs: 20,
            clean_session: true,
        }
    }
}

impl MqttConfig {
    /// Parse a broker URL like "mqtt://host:port" or "host:port" or just "host".
    pub fn parse_broker_url(url: &str) -> (String, u16) {
        let stripped = url
            .strip_prefix("mqtt://")
            .or_else(|| url.strip_prefix("tcp://"))
            .unwrap_or(url);

        match stripped.rsplit_once(':') {
            Some((host, port_str)) => {
                let port = port_str.parse().unwrap_or(1883);
                (host.to_string(), port)
            }
            None => (stripped.to_string(), 1883),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_broker_url_full() {
        let (host, port) = MqttConfig::parse_broker_url("mqtt://broker.local:1884");
        assert_eq!(host, "broker.local");
        assert_eq!(port, 1884);
    }

    #[test]
    fn parse_broker_url_no_scheme() {
        let (host, port) = MqttConfig::parse_broker_url("192.168.1.10:1883");
        assert_eq!(host, "192.168.1.10");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_host_only() {
        let (host, port) = MqttConfig::parse_broker_url("localhost");
        assert_eq!(host, "localhost");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_tcp_scheme() {
        let (host, port) = MqttConfig::parse_broker_url("tcp://myhost:9883");
        assert_eq!(host, "myhost");
        assert_eq!(port, 9883);
    }

    #[test]
    fn qos_from_int() {
        assert_eq!(QoS::from_int(0), QoS::AtMostOnce);
        assert_eq!(QoS::from_int(1), QoS::AtLeastOnce);
        assert_eq!(QoS::from_int(2), QoS::ExactlyOnce);
        assert_eq!(QoS::from_int(99), QoS::AtLeastOnce); // fallback
    }
}
