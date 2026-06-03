use crate::address::{PayloadFormat, TopicAddress, ValueType};
use crate::error::{MqttError, MqttResult};

/// Decoded value from an MQTT payload, ready for param store.
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedValue {
    Int32(i32),
    Float64(f64),
    UInt32(u32),
    String(String),
    Int32Array(Vec<i32>),
    Float64Array(Vec<f64>),
}

/// Decode an MQTT payload according to the topic address.
pub fn decode_payload(raw: &str, addr: &TopicAddress) -> MqttResult<DecodedValue> {
    match addr.format {
        PayloadFormat::Flat => decode_flat(raw, addr.value_type),
        PayloadFormat::Json => {
            let field = addr.json_field.as_deref().ok_or_else(|| {
                MqttError::InvalidAddress("JSON format requires a field path".into())
            })?;
            decode_json(raw, addr.value_type, field)
        }
    }
}

/// Encode a value for publishing according to the topic address format.
///
/// If `addr.normalize_on_off` is true, string values are normalized
/// ("1"/"on"/"true" → "ON", "0"/"off"/"false" → "OFF") before encoding.
pub fn encode_payload(value: &DecodedValue, addr: &TopicAddress) -> String {
    let value = if addr.normalize_on_off {
        normalize_value(value)
    } else {
        value.clone()
    };
    match addr.format {
        PayloadFormat::Flat => encode_flat(&value),
        PayloadFormat::Json => {
            let field = addr.json_field.as_deref().unwrap_or("value");
            encode_json(&value, field)
        }
    }
}

/// Encode a value as a flat string.
pub fn encode_flat(value: &DecodedValue) -> String {
    match value {
        DecodedValue::Int32(v) => v.to_string(),
        DecodedValue::Float64(v) => v.to_string(),
        DecodedValue::UInt32(v) => v.to_string(),
        DecodedValue::String(v) => v.clone(),
        DecodedValue::Int32Array(v) => {
            let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
            parts.join(",")
        }
        DecodedValue::Float64Array(v) => {
            let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
            parts.join(",")
        }
    }
}

/// Normalize a string: "1"/"on"/"true" → "ON", "0"/"off"/"false" → "OFF".
/// Other values pass through unchanged.
pub fn normalize_on_off(s: &str) -> String {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" => "ON".to_string(),
        "0" | "off" | "false" => "OFF".to_string(),
        _ => s.to_string(),
    }
}

/// Apply ON/OFF normalization to a DecodedValue (strings only).
fn normalize_value(value: &DecodedValue) -> DecodedValue {
    match value {
        DecodedValue::String(s) => DecodedValue::String(normalize_on_off(s)),
        other => other.clone(),
    }
}

/// Encode a value as JSON with a dot-separated field path.
fn encode_json(value: &DecodedValue, field_path: &str) -> String {
    let json_value = match value {
        DecodedValue::Int32(v) => serde_json::Value::from(*v),
        DecodedValue::Float64(v) => serde_json::Value::from(*v),
        DecodedValue::UInt32(v) => serde_json::Value::from(*v),
        DecodedValue::String(v) => serde_json::Value::from(v.as_str()),
        DecodedValue::Int32Array(v) => serde_json::Value::from(v.as_slice()),
        DecodedValue::Float64Array(v) => serde_json::Value::from(v.as_slice()),
    };

    // Build nested JSON object from dot-separated path
    let keys: Vec<&str> = field_path.split('.').collect();
    let mut result = json_value;
    for key in keys.iter().rev() {
        let mut obj = serde_json::Map::new();
        obj.insert((*key).to_string(), result);
        result = serde_json::Value::Object(obj);
    }

    result.to_string()
}

fn decode_flat(raw: &str, value_type: ValueType) -> MqttResult<DecodedValue> {
    let trimmed = raw.trim();
    match value_type {
        ValueType::Int => {
            let v: i32 = trimmed
                .parse()
                .map_err(|e| MqttError::ValueConversion(format!("INT parse: {e}")))?;
            Ok(DecodedValue::Int32(v))
        }
        ValueType::Float => {
            let v: f64 = trimmed
                .parse()
                .map_err(|e| MqttError::ValueConversion(format!("FLOAT parse: {e}")))?;
            Ok(DecodedValue::Float64(v))
        }
        ValueType::Digital => {
            let v: u32 = trimmed
                .parse()
                .map_err(|e| MqttError::ValueConversion(format!("DIGITAL parse: {e}")))?;
            Ok(DecodedValue::UInt32(v))
        }
        ValueType::String => Ok(DecodedValue::String(trimmed.to_string())),
        ValueType::IntArray => {
            let v = parse_int_array(trimmed)?;
            Ok(DecodedValue::Int32Array(v))
        }
        ValueType::FloatArray => {
            let v = parse_float_array(trimmed)?;
            Ok(DecodedValue::Float64Array(v))
        }
    }
}

fn decode_json(raw: &str, value_type: ValueType, field_path: &str) -> MqttResult<DecodedValue> {
    let json: serde_json::Value = serde_json::from_str(raw)?;
    let value = extract_json_field(&json, field_path)
        .ok_or_else(|| MqttError::JsonFieldNotFound(field_path.to_string()))?;

    match value_type {
        ValueType::Int => {
            let v = value.as_i64().ok_or_else(|| {
                MqttError::ValueConversion(format!(
                    "expected integer at '{field_path}', got {value}"
                ))
            })?;
            Ok(DecodedValue::Int32(v as i32))
        }
        ValueType::Float => {
            let v = value.as_f64().ok_or_else(|| {
                MqttError::ValueConversion(format!(
                    "expected number at '{field_path}', got {value}"
                ))
            })?;
            Ok(DecodedValue::Float64(v))
        }
        ValueType::Digital => {
            let v = value.as_u64().ok_or_else(|| {
                MqttError::ValueConversion(format!(
                    "expected unsigned at '{field_path}', got {value}"
                ))
            })?;
            Ok(DecodedValue::UInt32(v as u32))
        }
        ValueType::String => {
            let v = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Ok(DecodedValue::String(v))
        }
        ValueType::IntArray => {
            let arr = value.as_array().ok_or_else(|| {
                MqttError::ValueConversion(format!("expected array at '{field_path}', got {value}"))
            })?;
            let v = arr
                .iter()
                .map(|e| {
                    e.as_i64().map(|n| n as i32).ok_or_else(|| {
                        MqttError::ValueConversion(format!(
                            "expected integer array element at '{field_path}', got {e}"
                        ))
                    })
                })
                .collect::<MqttResult<Vec<i32>>>()?;
            Ok(DecodedValue::Int32Array(v))
        }
        ValueType::FloatArray => {
            let arr = value.as_array().ok_or_else(|| {
                MqttError::ValueConversion(format!("expected array at '{field_path}', got {value}"))
            })?;
            let v = arr
                .iter()
                .map(|e| {
                    e.as_f64().ok_or_else(|| {
                        MqttError::ValueConversion(format!(
                            "expected number array element at '{field_path}', got {e}"
                        ))
                    })
                })
                .collect::<MqttResult<Vec<f64>>>()?;
            Ok(DecodedValue::Float64Array(v))
        }
    }
}

/// Recursively search for `target_key` anywhere in the JSON value,
/// depth-first, first match wins.
///
/// C parity: `findJsonField` (drvMqtt.cpp:340-359) descends into every
/// nested object and array element and returns the first value whose key
/// equals `target_key`. The key is matched WHOLE — a configured field
/// like `a.b` is one literal key (C parses jsonField as the entire
/// arguments-remainder, drvMqtt.cpp:92), never a dot-separated path. At
/// each object level the key is checked before recursing into its value,
/// so traversal order mirrors C; serde_json's default `Map` is a sorted
/// `BTreeMap`, matching nlohmann's default sorted `std::map` iteration.
fn extract_json_field<'a>(
    json: &'a serde_json::Value,
    target_key: &str,
) -> Option<&'a serde_json::Value> {
    match json {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key == target_key {
                    return Some(value);
                }
                if let Some(found) = extract_json_field(value, target_key) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(elems) => {
            for elem in elems {
                if let Some(found) = extract_json_field(elem, target_key) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

/// Parse a comma-separated or space-separated list of integers.
/// Also handles bracket-wrapped arrays like `[1,2,3]`.
fn parse_int_array(s: &str) -> MqttResult<Vec<i32>> {
    let s = s.trim_start_matches('[').trim_end_matches(']');
    let separator = if s.contains(',') { ',' } else { ' ' };
    s.split(separator)
        .map(|part| {
            part.trim()
                .parse::<i32>()
                .map_err(|e| MqttError::ValueConversion(format!("INTARRAY element: {e}")))
        })
        .collect()
}

/// Parse a comma-separated or space-separated list of floats.
fn parse_float_array(s: &str) -> MqttResult<Vec<f64>> {
    let s = s.trim_start_matches('[').trim_end_matches(']');
    let separator = if s.contains(',') { ',' } else { ' ' };
    s.split(separator)
        .map(|part| {
            part.trim()
                .parse::<f64>()
                .map_err(|e| MqttError::ValueConversion(format!("FLOATARRAY element: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::TopicAddress;

    // --- Flat decode ---

    #[test]
    fn decode_flat_int() {
        let addr = TopicAddress::parse("FLAT:INT test/t").unwrap();
        let val = decode_payload("42", &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32(42));
    }

    #[test]
    fn decode_flat_int_negative() {
        let addr = TopicAddress::parse("FLAT:INT test/t").unwrap();
        let val = decode_payload("-7", &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32(-7));
    }

    #[test]
    fn decode_flat_int_whitespace() {
        let addr = TopicAddress::parse("FLAT:INT test/t").unwrap();
        let val = decode_payload("  100  \n", &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32(100));
    }

    #[test]
    fn decode_flat_float() {
        let addr = TopicAddress::parse("FLAT:FLOAT test/t").unwrap();
        let val = decode_payload("3.15", &addr).unwrap();
        assert_eq!(val, DecodedValue::Float64(3.15));
    }

    #[test]
    fn decode_flat_digital() {
        let addr = TopicAddress::parse("FLAT:DIGITAL test/t").unwrap();
        let val = decode_payload("255", &addr).unwrap();
        assert_eq!(val, DecodedValue::UInt32(255));
    }

    #[test]
    fn decode_flat_string() {
        let addr = TopicAddress::parse("FLAT:STRING test/t").unwrap();
        let val = decode_payload("hello world", &addr).unwrap();
        assert_eq!(val, DecodedValue::String("hello world".into()));
    }

    #[test]
    fn decode_flat_int_array_comma() {
        let addr = TopicAddress::parse("FLAT:INTARRAY test/t").unwrap();
        let val = decode_payload("1,2,3,4", &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32Array(vec![1, 2, 3, 4]));
    }

    #[test]
    fn decode_flat_int_array_brackets() {
        let addr = TopicAddress::parse("FLAT:INTARRAY test/t").unwrap();
        let val = decode_payload("[10, 20, 30]", &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32Array(vec![10, 20, 30]));
    }

    #[test]
    fn decode_flat_float_array() {
        let addr = TopicAddress::parse("FLAT:FLOATARRAY test/t").unwrap();
        let val = decode_payload("1.1,2.2,3.3", &addr).unwrap();
        assert_eq!(val, DecodedValue::Float64Array(vec![1.1, 2.2, 3.3]));
    }

    #[test]
    fn decode_flat_invalid_int() {
        let addr = TopicAddress::parse("FLAT:INT test/t").unwrap();
        assert!(decode_payload("not_a_number", &addr).is_err());
    }

    // --- JSON decode ---

    #[test]
    fn decode_json_float() {
        let addr = TopicAddress::parse("JSON:FLOAT sensors/data humidity").unwrap();
        let val = decode_payload(r#"{"humidity": 65.5}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::Float64(65.5));
    }

    #[test]
    fn decode_json_nested() {
        // C `findJsonField` recurses into nested objects (drvMqtt.cpp:347):
        // a whole-key match found anywhere wins, so `value` resolves even
        // though it sits under the `reading` parent object.
        let addr = TopicAddress::parse("JSON:INT sensors/data value").unwrap();
        let val = decode_payload(r#"{"reading": {"value": 42}}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32(42));
    }

    #[test]
    fn decode_json_deeply_nested() {
        // Recursion descends arbitrarily deep (drvMqtt.cpp:347).
        let addr = TopicAddress::parse("JSON:FLOAT device/data c").unwrap();
        let val = decode_payload(r#"{"a": {"b": {"c": 9.99}}}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::Float64(9.99));
    }

    #[test]
    fn decode_json_whole_key_with_dot() {
        // C parses jsonField as the whole arguments-remainder
        // (drvMqtt.cpp:92), so a key that legally contains a dot is matched
        // literally as one key, never split into a path.
        let addr = TopicAddress::parse("JSON:INT dev/data a.b").unwrap();
        let val = decode_payload(r#"{"a.b": 7}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32(7));
    }

    #[test]
    fn decode_json_dotted_field_is_not_path_split() {
        // Anti-regression for the old dot-split traversal: `reading.value`
        // is one literal key (absent here), so C returns not-found rather
        // than walking `reading` then `value`.
        let addr = TopicAddress::parse("JSON:INT sensors/data reading.value").unwrap();
        assert!(decode_payload(r#"{"reading": {"value": 42}}"#, &addr).is_err());
    }

    #[test]
    fn decode_json_recurses_into_array_elements() {
        // C recurses into array elements as well as objects
        // (drvMqtt.cpp:352-356).
        let addr = TopicAddress::parse("JSON:FLOAT dev/data temp").unwrap();
        let val = decode_payload(r#"{"sensors": [{"id": 1}, {"temp": 2.5}]}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::Float64(2.5));
    }

    #[test]
    fn decode_json_string() {
        let addr = TopicAddress::parse("JSON:STRING device/data status").unwrap();
        let val = decode_payload(r#"{"status": "OK"}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::String("OK".into()));
    }

    #[test]
    fn decode_json_string_non_string_value() {
        let addr = TopicAddress::parse("JSON:STRING device/data count").unwrap();
        let val = decode_payload(r#"{"count": 42}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::String("42".into()));
    }

    #[test]
    fn decode_json_field_not_found() {
        let addr = TopicAddress::parse("JSON:FLOAT sensors/data missing").unwrap();
        assert!(decode_payload(r#"{"other": 1.0}"#, &addr).is_err());
    }

    #[test]
    fn decode_json_type_mismatch() {
        let addr = TopicAddress::parse("JSON:INT sensors/data value").unwrap();
        assert!(decode_payload(r#"{"value": "not_a_number"}"#, &addr).is_err());
    }

    #[test]
    fn decode_json_invalid_json() {
        let addr = TopicAddress::parse("JSON:INT sensors/data value").unwrap();
        assert!(decode_payload("not json at all", &addr).is_err());
    }

    // --- BUG 5: inbound integer-array support ---

    /// BUG 5 regression — a JSON-format INTARRAY topic must decode to
    /// `Int32Array` instead of returning `UnsupportedType`, so a record
    /// bound to an integer-array MQTT topic receives data.
    #[test]
    fn decode_json_int_array() {
        let addr = TopicAddress::parse("JSON:INTARRAY sensors/data readings").unwrap();
        let val = decode_payload(r#"{"readings": [10, 20, 30]}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32Array(vec![10, 20, 30]));
    }

    /// BUG 5 regression — a nested JSON INTARRAY field decodes too, reached
    /// by C's recursive whole-key search (drvMqtt.cpp:347).
    #[test]
    fn decode_json_int_array_nested() {
        let addr = TopicAddress::parse("JSON:INTARRAY dev/data b").unwrap();
        let val = decode_payload(r#"{"a": {"b": [1, 2, 3, 4]}}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32Array(vec![1, 2, 3, 4]));
    }

    /// BUG 5 — a JSON FLOATARRAY field decodes to `Float64Array`.
    #[test]
    fn decode_json_float_array() {
        let addr = TopicAddress::parse("JSON:FLOATARRAY sensors/data temps").unwrap();
        let val = decode_payload(r#"{"temps": [1.5, 2.5]}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::Float64Array(vec![1.5, 2.5]));
    }

    /// BUG 5 — a non-array JSON value for an INTARRAY topic is a clean
    /// conversion error, not a panic.
    #[test]
    fn decode_json_int_array_type_mismatch() {
        let addr = TopicAddress::parse("JSON:INTARRAY sensors/data readings").unwrap();
        assert!(decode_payload(r#"{"readings": 42}"#, &addr).is_err());
    }

    /// BUG 5 — the FLAT INTARRAY decode path (already producing
    /// `Int32Array`) stays intact; the event loop now delivers it via
    /// `ParamSetValue::Int32Array`.
    #[test]
    fn decode_flat_int_array_for_inbound_delivery() {
        let addr = TopicAddress::parse("FLAT:INTARRAY test/arr").unwrap();
        let val = decode_payload("[5, 6, 7]", &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32Array(vec![5, 6, 7]));
    }

    // --- Encode ---

    #[test]
    fn encode_flat_values() {
        assert_eq!(encode_flat(&DecodedValue::Int32(42)), "42");
        assert_eq!(encode_flat(&DecodedValue::Float64(3.15)), "3.15");
        assert_eq!(encode_flat(&DecodedValue::UInt32(255)), "255");
        assert_eq!(encode_flat(&DecodedValue::String("hello".into())), "hello");
    }

    #[test]
    fn encode_flat_arrays() {
        assert_eq!(
            encode_flat(&DecodedValue::Int32Array(vec![1, 2, 3])),
            "1,2,3"
        );
        assert_eq!(
            encode_flat(&DecodedValue::Float64Array(vec![1.1, 2.2])),
            "1.1,2.2"
        );
    }

    // --- JSON encode ---

    #[test]
    fn encode_json_string() {
        let addr = TopicAddress::parse("JSON:STRING zigbee2mqtt/plug/set state").unwrap();
        let result = encode_payload(&DecodedValue::String("ON".into()), &addr);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["state"], "ON");
    }

    #[test]
    fn encode_json_int() {
        let addr = TopicAddress::parse("JSON:INT zigbee2mqtt/light/set brightness").unwrap();
        let result = encode_payload(&DecodedValue::Int32(128), &addr);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["brightness"], 128);
    }

    #[test]
    fn encode_json_float() {
        let addr = TopicAddress::parse("JSON:FLOAT device/set temperature").unwrap();
        let result = encode_payload(&DecodedValue::Float64(22.5), &addr);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["temperature"], 22.5);
    }

    #[test]
    fn encode_json_nested() {
        let addr = TopicAddress::parse("JSON:INT device/set settings.brightness").unwrap();
        let result = encode_payload(&DecodedValue::Int32(200), &addr);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["settings"]["brightness"], 200);
    }

    #[test]
    fn encode_payload_flat_passthrough() {
        let addr = TopicAddress::parse("FLAT:INT test/t").unwrap();
        assert_eq!(encode_payload(&DecodedValue::Int32(42), &addr), "42");
    }

    // --- ON/OFF normalization (opt-in via normalize_on_off flag) ---

    fn addr_with_normalize(drv_info: &str) -> TopicAddress {
        let mut addr = TopicAddress::parse(drv_info).unwrap();
        addr.normalize_on_off = true;
        addr
    }

    #[test]
    fn normalize_on_variants() {
        let addr = addr_with_normalize("JSON:STRING device/set state");
        for input in &["1", "on", "On", "ON", "true", "TRUE", "True"] {
            let result = encode_payload(&DecodedValue::String(input.to_string()), &addr);
            let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(parsed["state"], "ON", "input: {input}");
        }
    }

    #[test]
    fn normalize_off_variants() {
        let addr = addr_with_normalize("JSON:STRING device/set state");
        for input in &["0", "off", "Off", "OFF", "false", "FALSE", "False"] {
            let result = encode_payload(&DecodedValue::String(input.to_string()), &addr);
            let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(parsed["state"], "OFF", "input: {input}");
        }
    }

    #[test]
    fn no_normalize_without_flag() {
        // Default: normalize_on_off = false — values pass through as-is
        let addr = TopicAddress::parse("JSON:STRING device/set state").unwrap();
        let result = encode_payload(&DecodedValue::String("1".into()), &addr);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["state"], "1"); // NOT "ON"
    }

    #[test]
    fn no_normalize_other_strings() {
        let addr = addr_with_normalize("JSON:STRING device/set mode");
        let result = encode_payload(&DecodedValue::String("auto".into()), &addr);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["mode"], "auto");
    }

    #[test]
    fn no_normalize_integers() {
        let addr = addr_with_normalize("JSON:INT device/set brightness");
        let result = encode_payload(&DecodedValue::Int32(0), &addr);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["brightness"], 0);

        let result = encode_payload(&DecodedValue::Int32(1), &addr);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["brightness"], 1);
    }
}
