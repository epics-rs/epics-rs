use asyn_rs::param::ParamType;

use crate::error::{MqttError, MqttResult};

/// Payload format: flat single-value or structured JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadFormat {
    Flat,
    Json,
}

/// Expected value type of the MQTT payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Int,
    Float,
    Digital,
    String,
    IntArray,
    FloatArray,
}

/// Parsed topic address from a drvInfo string.
///
/// Format: `"FORMAT:TYPE topic/name [json field]"`
///
/// Grammar follows C `MqttDriver::parseDeviceAddress` (drvMqtt.cpp:64-96):
/// - **FLAT**: everything after `FORMAT:TYPE ` is the topic (spaces allowed),
///   `topicName = arguments`.
/// - **JSON**: the topic is the text before the FIRST whitespace and the JSON
///   field is the entire remaining suffix (`arguments.substr(spacePos + 1)`),
///   so a JSON object key may itself contain spaces. A topic containing
///   spaces is not expressible in the C grammar; this port adds an explicit
///   quoted-topic extension `FORMAT:TYPE "topic with spaces" field` for that
///   case only, leaving the reference grammar for unquoted drvInfo intact.
///   The extension is a strict fallback: it fires only when the quoted topic
///   actually contains whitespace, so `JSON:INT "abc" def` reads C's way —
///   topic `"abc"`, quotes included — rather than being hijacked (MQ18).
///
/// Examples:
/// - `"FLAT:INT test/temperature"`
/// - `"JSON:FLOAT sensors/data humidity"`
/// - `"JSON:STRING device/topic key with spaces"` (field is `key with spaces`)
/// - `"JSON:FLOAT \"zigbee2mqtt/living room plug\" power"` (quoted topic)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicAddress {
    pub format: PayloadFormat,
    pub value_type: ValueType,
    pub topic: String,
    pub json_field: Option<String>,
    /// When true, string values "1"/"on"/"true" → "ON", "0"/"off"/"false" → "OFF".
    /// Set by Z2M builders for state control topics. Generic MQTT leaves this false.
    pub normalize_on_off: bool,
}

impl TopicAddress {
    /// Parse a drvInfo string into a `TopicAddress`.
    ///
    /// See the type docs for the grammar. The unquoted JSON form mirrors C
    /// `parseDeviceAddress` (drvMqtt.cpp:74-92) exactly: first whitespace
    /// separates the topic from the whole JSON field suffix. The quoted JSON
    /// form is a port-only extension for topics that contain spaces.
    pub fn parse(drv_info: &str) -> MqttResult<Self> {
        // Split off FORMAT:TYPE (first token)
        let (format_type_str, rest) =
            drv_info.split_once(char::is_whitespace).ok_or_else(|| {
                MqttError::InvalidAddress(format!(
                    "expected at least 'FORMAT:TYPE topic', got: {drv_info:?}"
                ))
            })?;

        let rest = rest.trim_start();
        if rest.is_empty() {
            return Err(MqttError::InvalidAddress(
                "missing topic after FORMAT:TYPE".into(),
            ));
        }

        let (format, value_type) = Self::parse_format_type(format_type_str)?;

        let (topic, json_field) = match format {
            PayloadFormat::Flat => {
                // Everything remaining is the topic
                (rest.to_string(), None)
            }
            PayloadFormat::Json => {
                // MQ18: the quoted form is a port-only extension and may claim
                // ONLY the inputs the reference grammar cannot express. For
                // everything else — a topic that legitimately begins with `"`
                // included — C's rule stands and the quotes are part of the
                // topic name. Taking the quoted branch first made the two sides
                // SUBSCRIBE to different topics for `JSON:INT "abc" def` (`abc`
                // here, `"abc"` in C), so the record never received C's data.
                if let Some((topic, field)) = Self::parse_quoted_json_topic(rest) {
                    (topic, Some(field))
                } else {
                    // C grammar (drvMqtt.cpp:75,86,92): topic is the text before
                    // the first whitespace; the JSON field is the entire rest of
                    // the suffix and may itself contain spaces.
                    let (topic, field) = rest.split_once(char::is_whitespace).ok_or_else(|| {
                        MqttError::InvalidAddress("JSON format requires 'topic field'".into())
                    })?;
                    if topic.is_empty() {
                        return Err(MqttError::InvalidAddress(
                            "empty topic before JSON field".into(),
                        ));
                    }
                    if field.is_empty() {
                        return Err(MqttError::InvalidAddress("empty JSON field".into()));
                    }
                    (topic.to_string(), Some(field.to_string()))
                }
            }
        };

        Self::validate_topic(&topic)?;

        Ok(Self {
            format,
            value_type,
            topic,
            json_field,
            normalize_on_off: false,
        })
    }

    /// Convert this address's value type to the corresponding asyn `ParamType`.
    pub fn param_type(&self) -> ParamType {
        match self.value_type {
            ValueType::Int => ParamType::Int32,
            ValueType::Float => ParamType::Float64,
            ValueType::Digital => ParamType::UInt32Digital,
            ValueType::String => ParamType::Octet,
            ValueType::IntArray => ParamType::Int32Array,
            ValueType::FloatArray => ParamType::Float64Array,
        }
    }

    /// Reconstruct the drvInfo string for use as a parameter name.
    pub fn to_drv_info(&self) -> String {
        let fmt = match self.format {
            PayloadFormat::Flat => "FLAT",
            PayloadFormat::Json => "JSON",
        };
        let typ = match self.value_type {
            ValueType::Int => "INT",
            ValueType::Float => "FLOAT",
            ValueType::Digital => "DIGITAL",
            ValueType::String => "STRING",
            ValueType::IntArray => "INTARRAY",
            ValueType::FloatArray => "FLOATARRAY",
        };
        match &self.json_field {
            // A topic containing spaces is only round-trippable through the
            // quoted-topic extension; an unquoted topic re-parses C-faithfully.
            Some(field) if self.topic.contains(char::is_whitespace) => {
                format!("{fmt}:{typ} \"{}\" {field}", self.topic)
            }
            Some(field) => format!("{fmt}:{typ} {} {field}", self.topic),
            None => format!("{fmt}:{typ} {}", self.topic),
        }
    }

    fn parse_format_type(s: &str) -> MqttResult<(PayloadFormat, ValueType)> {
        let (fmt_str, type_str) = s
            .split_once(':')
            .ok_or_else(|| MqttError::InvalidAddress(format!("missing ':' in {s:?}")))?;

        // C parity: `supportedTopicTypes.find(type)` (drvMqtt.cpp:362-364) is a
        // case-sensitive lookup over the uppercase-only set (drvMqtt.cpp:24-37),
        // so `flat:int` / `Json:Float` are rejected (record device-init fails).
        let format = match fmt_str {
            "FLAT" => PayloadFormat::Flat,
            "JSON" => PayloadFormat::Json,
            _ => {
                return Err(MqttError::UnsupportedType(format!(
                    "unknown format: {fmt_str:?}"
                )));
            }
        };

        let value_type = match type_str {
            "INT" => ValueType::Int,
            "FLOAT" => ValueType::Float,
            "DIGITAL" => ValueType::Digital,
            "STRING" => ValueType::String,
            "INTARRAY" => ValueType::IntArray,
            "FLOATARRAY" => ValueType::FloatArray,
            _ => {
                return Err(MqttError::UnsupportedType(format!(
                    "unknown type: {type_str:?}"
                )));
            }
        };

        Ok((format, value_type))
    }

    /// The quoted-topic extension, or `None` when the reference grammar owns
    /// this input.
    ///
    /// The extension exists for one thing C cannot express: a topic containing
    /// whitespace, because `arguments.find(' ')` reserves the first space for
    /// the topic/field boundary (drvMqtt.cpp:75,86,92). So the quoted reading
    /// is taken only when it is well-formed in full — closing quote, non-empty
    /// topic, non-empty field — *and* the topic actually contains whitespace.
    /// Every other input falls through, which means the extension can neither
    /// re-read a drvInfo C reads differently nor reject one C accepts.
    fn parse_quoted_json_topic(rest: &str) -> Option<(String, String)> {
        let after_open = rest.strip_prefix('"')?;
        let close = after_open.find('"')?;
        let topic = &after_open[..close];
        if !topic.contains(char::is_whitespace) {
            return None;
        }
        let field = after_open[close + 1..].trim();
        if field.is_empty() {
            return None;
        }
        Some((topic.to_string(), field.to_string()))
    }

    fn validate_topic(topic: &str) -> MqttResult<()> {
        if topic.is_empty() {
            return Err(MqttError::InvalidTopic("empty topic".into()));
        }
        if topic.contains('#') || topic.contains('+') {
            return Err(MqttError::InvalidTopic(format!(
                "wildcards not allowed in topic address: {topic:?}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flat_int() {
        let addr = TopicAddress::parse("FLAT:INT test/temperature").unwrap();
        assert_eq!(addr.format, PayloadFormat::Flat);
        assert_eq!(addr.value_type, ValueType::Int);
        assert_eq!(addr.topic, "test/temperature");
        assert_eq!(addr.json_field, None);
        assert_eq!(addr.param_type(), ParamType::Int32);
    }

    #[test]
    fn parse_flat_float() {
        let addr = TopicAddress::parse("FLAT:FLOAT sensors/pressure").unwrap();
        assert_eq!(addr.format, PayloadFormat::Flat);
        assert_eq!(addr.value_type, ValueType::Float);
        assert_eq!(addr.topic, "sensors/pressure");
    }

    #[test]
    fn parse_flat_string() {
        let addr = TopicAddress::parse("FLAT:STRING device/status").unwrap();
        assert_eq!(addr.value_type, ValueType::String);
        assert_eq!(addr.param_type(), ParamType::Octet);
    }

    #[test]
    fn parse_flat_arrays() {
        let addr = TopicAddress::parse("FLAT:INTARRAY data/counts").unwrap();
        assert_eq!(addr.value_type, ValueType::IntArray);
        assert_eq!(addr.param_type(), ParamType::Int32Array);

        let addr = TopicAddress::parse("FLAT:FLOATARRAY data/waveform").unwrap();
        assert_eq!(addr.value_type, ValueType::FloatArray);
        assert_eq!(addr.param_type(), ParamType::Float64Array);
    }

    #[test]
    fn parse_json_float() {
        let addr = TopicAddress::parse("JSON:FLOAT sensors/data humidity").unwrap();
        assert_eq!(addr.format, PayloadFormat::Json);
        assert_eq!(addr.value_type, ValueType::Float);
        assert_eq!(addr.topic, "sensors/data");
        assert_eq!(addr.json_field.as_deref(), Some("humidity"));
    }

    #[test]
    fn parse_json_nested_field() {
        let addr = TopicAddress::parse("JSON:INT sensors/data reading.value").unwrap();
        assert_eq!(addr.topic, "sensors/data");
        assert_eq!(addr.json_field.as_deref(), Some("reading.value"));
    }

    // C parity: the JSON field is the entire suffix after the first space, so a
    // key containing spaces is one literal key matched against the payload
    // (drvMqtt.cpp:92). The topic is the first token only.
    #[test]
    fn parse_json_field_with_spaces() {
        let addr = TopicAddress::parse("JSON:STRING device/topic key with spaces").unwrap();
        assert_eq!(addr.format, PayloadFormat::Json);
        assert_eq!(addr.topic, "device/topic");
        assert_eq!(addr.json_field.as_deref(), Some("key with spaces"));
    }

    /// C parity: FORMAT:TYPE is matched case-sensitively against the
    /// uppercase-only `supportedTopicTypes` set (drvMqtt.cpp:24-37,362-364),
    /// so a lowercase form is rejected, not silently canonicalised.
    #[test]
    fn parse_rejects_lowercase_format_type() {
        assert!(TopicAddress::parse("flat:int test/topic").is_err());
        assert!(TopicAddress::parse("Json:Float sensors/data humidity").is_err());
        assert!(TopicAddress::parse("FLAT:int test/topic").is_err());
    }

    #[test]
    fn parse_roundtrip() {
        let original = "FLAT:INT test/temperature";
        let addr = TopicAddress::parse(original).unwrap();
        assert_eq!(addr.to_drv_info(), original);

        let original = "JSON:FLOAT sensors/data humidity";
        let addr = TopicAddress::parse(original).unwrap();
        assert_eq!(addr.to_drv_info(), original);
    }

    #[test]
    fn reject_empty_input() {
        assert!(TopicAddress::parse("").is_err());
    }

    #[test]
    fn reject_missing_topic() {
        assert!(TopicAddress::parse("FLAT:INT").is_err());
    }

    #[test]
    fn reject_missing_colon() {
        assert!(TopicAddress::parse("FLATINT test/topic").is_err());
    }

    #[test]
    fn reject_unknown_format() {
        assert!(TopicAddress::parse("XML:INT test/topic").is_err());
    }

    #[test]
    fn reject_unknown_type() {
        assert!(TopicAddress::parse("FLAT:BOOL test/topic").is_err());
    }

    #[test]
    fn reject_wildcard_topics() {
        assert!(TopicAddress::parse("FLAT:INT test/+/data").is_err());
        assert!(TopicAddress::parse("FLAT:INT test/#").is_err());
    }

    #[test]
    fn reject_json_without_field() {
        assert!(TopicAddress::parse("JSON:FLOAT sensors/data").is_err());
    }

    // --- Topics with spaces (e.g. Z2M Korean device names) ---
    //
    // FLAT topics may contain spaces directly (C `topicName = arguments`).
    // JSON topics with spaces require the quoted-topic extension, because the
    // unquoted C grammar reserves whitespace for the topic/field boundary.

    #[test]
    fn parse_flat_topic_with_spaces() {
        let addr = TopicAddress::parse("FLAT:FLOAT zigbee2mqtt/living room plug").unwrap();
        assert_eq!(addr.format, PayloadFormat::Flat);
        assert_eq!(addr.value_type, ValueType::Float);
        assert_eq!(addr.topic, "zigbee2mqtt/living room plug");
        assert_eq!(addr.json_field, None);
    }

    #[test]
    fn parse_json_quoted_topic_with_spaces() {
        let addr =
            TopicAddress::parse("JSON:FLOAT \"zigbee2mqtt/living room plug\" power").unwrap();
        assert_eq!(addr.format, PayloadFormat::Json);
        assert_eq!(addr.topic, "zigbee2mqtt/living room plug");
        assert_eq!(addr.json_field.as_deref(), Some("power"));
    }

    #[test]
    fn parse_json_quoted_topic_nested_field() {
        let addr =
            TopicAddress::parse("JSON:FLOAT \"zigbee2mqtt/desk light\" update.installed_version")
                .unwrap();
        assert_eq!(addr.topic, "zigbee2mqtt/desk light");
        assert_eq!(addr.json_field.as_deref(), Some("update.installed_version"));
    }

    // The unquoted JSON form must NOT absorb topic spaces — that is the C
    // behavior this port restores: first token is the topic, the rest is field.
    #[test]
    fn parse_json_unquoted_splits_at_first_space() {
        let addr = TopicAddress::parse("JSON:FLOAT zigbee2mqtt/living room plug power").unwrap();
        assert_eq!(addr.topic, "zigbee2mqtt/living");
        assert_eq!(addr.json_field.as_deref(), Some("room plug power"));
    }

    // MQ18: an unterminated quote is not an error — C has no quote handling at
    // all, so `arguments.find(' ')` splits at the first space and the stray
    // quote simply stays in the topic name (drvMqtt.cpp:75,86,92). This test
    // used to pin the extension's rejection, which C never performs.
    #[test]
    fn json_unterminated_quote_falls_through_to_the_c_grammar() {
        let addr = TopicAddress::parse("JSON:FLOAT \"zigbee2mqtt/living room power").unwrap();
        assert_eq!(addr.topic, "\"zigbee2mqtt/living");
        assert_eq!(addr.json_field.as_deref(), Some("room power"));
    }

    /// MQ18: for a drvInfo whose topic literally begins with `"`, the port used
    /// to strip the quotes and SUBSCRIBE to a different topic than C, so the
    /// record never received the data C delivers. C's grammar owns this input.
    #[test]
    fn json_quoted_extension_does_not_hijack_a_c_valid_quoted_topic() {
        let addr = TopicAddress::parse("JSON:INT \"abc\" def").unwrap();
        assert_eq!(
            addr.topic, "\"abc\"",
            "C keeps the quotes in the topic name"
        );
        assert_eq!(addr.json_field.as_deref(), Some("def"));

        // The extension still owns the one case C cannot express.
        let addr = TopicAddress::parse("JSON:INT \"a b\" def").unwrap();
        assert_eq!(addr.topic, "a b");
        assert_eq!(addr.json_field.as_deref(), Some("def"));

        // A quoted topic with no field is C's "topic then field" split too, not
        // the extension's error: topic `"a`, field `b"`.
        let addr = TopicAddress::parse("JSON:INT \"a b\"").unwrap();
        assert_eq!(addr.topic, "\"a");
        assert_eq!(addr.json_field.as_deref(), Some("b\""));
    }

    #[test]
    fn parse_flat_topic_with_multiple_spaces() {
        let addr = TopicAddress::parse("FLAT:STRING zigbee2mqtt/my cool device name").unwrap();
        assert_eq!(addr.topic, "zigbee2mqtt/my cool device name");
    }

    #[test]
    fn roundtrip_topic_with_spaces() {
        let original = "FLAT:FLOAT zigbee2mqtt/living room plug";
        let addr = TopicAddress::parse(original).unwrap();
        assert_eq!(addr.to_drv_info(), original);

        // A spaced JSON topic round-trips through the quoted-topic extension.
        let original = "JSON:INT \"zigbee2mqtt/bedroom plug\" device_temperature";
        let addr = TopicAddress::parse(original).unwrap();
        assert_eq!(addr.topic, "zigbee2mqtt/bedroom plug");
        assert_eq!(addr.json_field.as_deref(), Some("device_temperature"));
        assert_eq!(addr.to_drv_info(), original);

        // A JSON field containing spaces round-trips unquoted (C grammar).
        let original = "JSON:STRING device/topic key with spaces";
        let addr = TopicAddress::parse(original).unwrap();
        assert_eq!(addr.to_drv_info(), original);
    }
}
