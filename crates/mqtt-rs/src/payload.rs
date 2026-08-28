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

/// An asyn octet value is a NUL-terminated C-string. Both C octet boundaries
/// terminate the value at the first NUL byte: the inbound store
/// `setStringParam(index, val.c_str())` (drvMqtt.cpp:299) and the outbound
/// `mqttClient.publish(addr.topicName, stringData.data())`
/// (`publish(const std::string&)` from a `char*`, drvMqtt.cpp:716). Return the
/// prefix up to the first NUL so the Rust octet value matches that C-string view.
pub fn octet_cstr(s: &str) -> &str {
    match s.find('\0') {
        Some(i) => &s[..i],
        None => s,
    }
}

/// Byte-level twin of [`octet_cstr`]: the prefix of `data` up to the first NUL
/// byte. C's outbound `stringWrite` publishes `std::string(stringData.data())`
/// (drvMqtt.cpp:716), built from a `char*`, i.e. the raw bytes up to the first
/// NUL with no UTF-8 re-encoding. Returns the slice so a non-UTF-8 octet write
/// reaches the wire byte-for-byte.
pub fn octet_bytes_cstr(data: &[u8]) -> &[u8] {
    match data.iter().position(|&b| b == 0) {
        Some(i) => &data[..i],
        None => data,
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
        // C parity: FLAT scalar float publishes `std::to_string(epicsFloat64)`
        // (drvMqtt.cpp:651), specified as sprintf "%f" — always 6 decimals.
        // Rust's `{:.6}` matches C for every finite value and for ±inf
        // ("inf"/"-inf"), but prints "NaN" where glibc "%f" prints lowercase
        // "nan"; normalise the NaN spelling to match C's wire bytes (the
        // FLOATARRAY path already lowercases via `format_ostream_double`).
        DecodedValue::Float64(v) if v.is_nan() => "nan".to_string(),
        DecodedValue::Float64(v) => format!("{v:.6}"),
        DecodedValue::UInt32(v) => v.to_string(),
        DecodedValue::String(v) => v.clone(),
        DecodedValue::Int32Array(v) => {
            let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
            parts.join(",")
        }
        // C parity: FLAT array float streams each element through a default
        // `std::ostringstream` (drvMqtt.cpp:685, `oss << arrayData[i]`), i.e.
        // printf "%g" at the stream's default 6-significant-digit precision.
        DecodedValue::Float64Array(v) => {
            let parts: Vec<String> = v.iter().map(|x| format_ostream_double(*x)).collect();
            parts.join(",")
        }
    }
}

/// Format a float as C++ default `std::ostream operator<<` does — printf
/// `%g` at the stream's default 6-significant-digit precision
/// (drvMqtt.cpp:685, `oss << arrayData[i]`).
///
/// `%g` chooses fixed or scientific by the post-rounding decimal exponent
/// `e`: fixed when `-4 <= e < 6`, scientific otherwise, with trailing
/// zeros stripped. We round to 6 significant digits via Rust scientific
/// formatting (which yields the post-rounding exponent), then re-render in
/// the selected style.
fn format_ostream_double(v: f64) -> String {
    const SIG: usize = 6;
    if v == 0.0 {
        // `oss << 0.0` => "0"; negative zero keeps its sign.
        return if v.is_sign_negative() {
            "-0".to_string()
        } else {
            "0".to_string()
        };
    }
    if !v.is_finite() {
        // libstdc++ streams non-finite as "nan"/"inf"/"-inf".
        if v.is_nan() {
            return "nan".to_string();
        }
        return if v < 0.0 {
            "-inf".to_string()
        } else {
            "inf".to_string()
        };
    }
    // Scientific render to SIG significant digits gives the rounded exponent.
    let sci = format!("{:.*e}", SIG - 1, v);
    let (mantissa, exp_str) = sci.split_once('e').expect("LowerExp always has 'e'");
    let exp: i32 = exp_str.parse().expect("LowerExp exponent is an integer");
    if (-4..SIG as i32).contains(&exp) {
        // Fixed style: SIG-1-exp fractional digits, then strip trailing zeros.
        let frac = (SIG as i32 - 1 - exp).max(0) as usize;
        strip_trailing_zeros(&format!("{v:.frac$}"))
    } else {
        // Scientific style: strip the mantissa, render a C-style exponent
        // (explicit sign, at least two digits) — e.g. "1.23457e+06".
        let mantissa = strip_trailing_zeros(mantissa);
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    }
}

/// Strip trailing zeros (and a now-bare decimal point) from a fixed-form
/// decimal string, matching printf `%g`'s zero suppression.
fn strip_trailing_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
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

/// C `isBoolean` shortcut (drvMqtt.cpp:388): a payload of exactly "true"
/// or "false" maps to 1 / 0 before any numeric parse, for INT
/// (drvMqtt.cpp:278-281) and DIGITAL (drvMqtt.cpp:290-293) records only.
/// `asynParamFloat64` has no such branch (drvMqtt.cpp:285-287), so floats
/// never take this shortcut.
fn boolean_payload_to_int(s: &str) -> Option<i64> {
    match s {
        "true" => Some(1),
        "false" => Some(0),
        _ => None,
    }
}

/// Parse a whole payload element the way C's `strtod` does.
///
/// MQ37: C validates a scalar FLOAT with `strtof` (drvMqtt.cpp:399) and array
/// elements go straight through `std::strtod` (drvMqtt.cpp:538). Both take the
/// full C99 grammar, and two of its forms `f64::from_str` rejects outright: a
/// hexadecimal significand (`0x1.8p3`, whose binary exponent `strtod` treats as
/// optional) and a NaN carrying a character sequence (`nan(123)`). Everything
/// else is delegated to `f64::from_str`, whose accept set already matches
/// `strtod`'s decimal form.
///
/// `None` means "not consumed to the end", which is what `isFloat`'s
/// `end == s.c_str() + s.size()` test (drvMqtt.cpp:400) rejects, and what the
/// array walker sees as a separator that is not a separator.
fn parse_c_float(s: &str) -> Option<f64> {
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let magnitude = if body.starts_with("0x") || body.starts_with("0X") {
        parse_hex_float(body)?
    } else if let Some(v) = parse_nan_sequence(body) {
        v
    } else {
        // Decimal (and bare `inf` / `nan`): from_str already matches strtod,
        // including its rejection of leading or trailing junk.
        return s.parse::<f64>().ok();
    };
    Some(if negative { -magnitude } else { magnitude })
}

/// C99 hexadecimal floating form, unsigned: `0x` h+ [`.` h*] [(`p`|`P`) \[sign\]
/// d+]. The exponent is optional here because `strtod` accepts `0x1.8`; when a
/// `p` is present its digits are required, and anything left over makes the
/// whole element unconsumed. An exponent large enough to overflow yields an
/// infinity, which the scalar arm's finiteness guard then refuses exactly as
/// `std::stod`'s `out_of_range` does.
fn parse_hex_float(body: &str) -> Option<f64> {
    let rest = body
        .strip_prefix("0x")
        .or_else(|| body.strip_prefix("0X"))?;
    let (digits, exponent) = match rest.find(['p', 'P']) {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let (int_digits, frac_digits) = match digits.split_once('.') {
        Some((a, b)) => (a, b),
        None => (digits, ""),
    };
    if int_digits.is_empty() && frac_digits.is_empty() {
        return None;
    }

    let mut mantissa = 0.0f64;
    for c in int_digits.chars() {
        mantissa = mantissa * 16.0 + f64::from(c.to_digit(16)?);
    }
    let mut place = 1.0f64 / 16.0;
    for c in frac_digits.chars() {
        mantissa += f64::from(c.to_digit(16)?) * place;
        place /= 16.0;
    }

    let exponent = match exponent {
        None => 0i32,
        Some(e) => {
            let (sign, digits) = match e.strip_prefix('-') {
                Some(rest) => (-1i32, rest),
                None => (1i32, e.strip_prefix('+').unwrap_or(e)),
            };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // A silly-large exponent saturates rather than wrapping; powi then
            // gives 0 or inf, which is what strtod reports for it.
            sign * digits.parse::<i32>().unwrap_or(i32::MAX / 2)
        }
    };

    Some(mantissa * 2f64.powi(exponent))
}

/// C99 `nan(n-char-sequence)`, unsigned. The sequence is
/// implementation-defined; glibc accepts alphanumerics and `_`, and an empty
/// sequence is legal. A bare `nan` is *not* matched here — `f64::from_str`
/// already takes it.
fn parse_nan_sequence(body: &str) -> Option<f64> {
    if !body.get(..3)?.eq_ignore_ascii_case("nan") {
        return None;
    }
    let inner = body[3..].strip_prefix('(')?.strip_suffix(')')?;
    if !inner
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    Some(f64::NAN)
}

/// Does the payload text itself name an infinity or a NaN?
///
/// MQ36: C converts a scalar FLOAT with `std::stod` (drvMqtt.cpp:287), which
/// throws `out_of_range` only when the *value* leaves the double range — a
/// literal `inf` / `nan` is consumed by `strtod` with no range error, so
/// `stod` returns it and C stores it. The finiteness guard must therefore let
/// a payload that names a non-finite value through and reject only a
/// finite-looking one that overflowed. The accept set is `f64::from_str`'s,
/// since that is what produced the value.
fn text_names_non_finite(s: &str) -> bool {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    body.eq_ignore_ascii_case("inf")
        || body.eq_ignore_ascii_case("infinity")
        || body.eq_ignore_ascii_case("nan")
        || parse_nan_sequence(body).is_some()
}

fn decode_flat(raw: &str, value_type: ValueType) -> MqttResult<DecodedValue> {
    let trimmed = raw.trim();
    match value_type {
        // INT/DIGITAL parse the RAW payload: C's isInteger (drvMqtt.cpp:376-387)
        // is a hand-rolled digit scan with no whitespace skip, so a payload like
        // " 42" or "42\n" is rejected (the prior value is kept), not silently
        // accepted as 42. Rust's integer FromStr rejects surrounding whitespace
        // identically. FLOAT differs (C uses strtof) -- see its arm below.
        ValueType::Int => {
            if let Some(b) = boolean_payload_to_int(raw) {
                return Ok(DecodedValue::Int32(b as i32));
            }
            let v: i32 = raw
                .parse()
                .map_err(|e| MqttError::ValueConversion(format!("INT parse: {e}")))?;
            Ok(DecodedValue::Int32(v))
        }
        ValueType::Float => {
            // C validates floats with isFloat -> strtof (drvMqtt.cpp:393-401),
            // which SKIPS leading whitespace but requires the parse to reach
            // end-of-string (trailing whitespace/garbage rejected). So " 4.2"
            // is a valid 4.2 in C while "4.2 " is refused. trim_start() before
            // parse() mirrors that: accept leading ws, still reject trailing.
            let text = raw.trim_start();
            let v: f64 = parse_c_float(text).ok_or_else(|| {
                MqttError::ValueConversion(format!("FLOAT parse: {text:?} is not a C float"))
            })?;
            // MQ36: C's isFloat accepts "1e400" because strtof consumes the
            // whole string (drvMqtt.cpp:393-401), and the conversion that
            // follows is `std::stod`, which throws `out_of_range` for a
            // magnitude no double can hold (drvMqtt.cpp:287). The throw is
            // caught (drvMqtt.cpp:327-331) with no setParam, so the record
            // keeps its prior value. `f64::from_str` returns +/-inf instead, so
            // a client read `inf` where C read the previous number. Reject only
            // when the text does not itself name the non-finite value -- see
            // `text_names_non_finite`.
            if !v.is_finite() && !text_names_non_finite(text) {
                return Err(MqttError::ValueConversion(format!(
                    "FLOAT parse: {text:?} is out of range for a double \
                     (C std::stod throws out_of_range)"
                )));
            }
            Ok(DecodedValue::Float64(v))
        }
        ValueType::Digital => {
            if let Some(b) = boolean_payload_to_int(raw) {
                return Ok(DecodedValue::UInt32(b as u32));
            }
            // C parity: DIGITAL parses with `isInteger(val, isSigned=false)`
            // (drvMqtt.cpp:294,376-387), which treats a leading '+'/'-' as a
            // non-digit and rejects it (only all-digit payloads pass). Rust's
            // `u32::from_str` rejects '-' but *accepts* a leading '+', so guard
            // it explicitly to match C's accept set.
            if raw.starts_with('+') {
                return Err(MqttError::ValueConversion(
                    "DIGITAL parse: leading '+' not allowed (unsigned)".into(),
                ));
            }
            let v: u32 = raw
                .parse()
                .map_err(|e| MqttError::ValueConversion(format!("DIGITAL parse: {e}")))?;
            Ok(DecodedValue::UInt32(v))
        }
        // STRING stores the raw payload verbatim: C setStringParam(val) writes
        // the unmodified payload (drvMqtt.cpp:297-299), so "  hello  " keeps its
        // surrounding whitespace rather than being trimmed to "hello".
        ValueType::String => Ok(DecodedValue::String(raw.to_string())),
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

    // C's onMessageCb (drvMqtt.cpp:256-265) collapses the matched JSON
    // field to ONE string carrier and then runs the *same* type switch it
    // uses for a non-JSON (flat) payload: a missing field or an explicit
    // JSON `null` is "not found"; a JSON string yields its raw content
    // (`fieldAddr->get<std::string>()`); every other type yields its
    // compact serialization (`fieldAddr->dump()`). The numeric/array/bool
    // conversion (isBoolean/isInteger/isFloat/checkAndParse*Array) is then
    // identical to the flat path. Mirror that exactly — build the carrier,
    // then delegate to [`decode_flat`] — so JSON and flat share one parser
    // instead of decode_json re-deriving conversion with serde's
    // `as_i64`/`as_f64`/`as_array`. That divergence rejected the string
    // carrier a broker that encodes every value as text emits:
    // `{"v":"42"}` (INT), `{"v":"3.14"}` (FLOAT), and `{"v":"[1,2,3]"}`
    // (INTARRAY) all failed where C accepts them, and `dump()` likewise
    // lets a lone JSON number satisfy an *array* topic (checkAndParseInt
    // Array accepts a single value, drvMqtt.cpp:426-487).
    if value.is_null() {
        return Err(MqttError::JsonFieldNotFound(field_path.to_string()));
    }
    let carrier = match value {
        serde_json::Value::String(s) => s.clone(),
        other => dump_sorted(other),
    };

    // Octet/String is C's one non-parsing arm: `setStringParam(val)` uses
    // the carrier verbatim (no trim, no parse), so emit it directly rather
    // than routing through decode_flat's whitespace-trimming string arm.
    if value_type == ValueType::String {
        return Ok(DecodedValue::String(carrier));
    }
    decode_flat(&carrier, value_type)
}

/// Serialize a JSON value the way C's `fieldAddr->dump()` does (drvMqtt.cpp:265):
/// compact, with object keys in **sorted** order at every level, because
/// nlohmann's `json` is backed by a `std::map` (drvMqtt.h:19). serde_json with
/// the `preserve_order` feature — which the shipped IOC build links transitively
/// via `epics-base-rs` — would otherwise emit keys in document order, so a JSON
/// object/array carrier on a STRING topic stored different bytes than C. Rebuild
/// the value with sorted keys first, then serialize. Scalars are unaffected (the
/// rebuild clones them), so only composite carriers change.
fn dump_sorted(value: &serde_json::Value) -> String {
    fn sort_value(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                // With `preserve_order`, `Map` is an `IndexMap` that keeps
                // insertion order, so inserting in sorted order yields sorted
                // output; without it `Map` is a `BTreeMap` (already sorted).
                let sorted: serde_json::Map<String, serde_json::Value> = entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), sort_value(v)))
                    .collect();
                serde_json::Value::Object(sorted)
            }
            serde_json::Value::Array(elems) => {
                serde_json::Value::Array(elems.iter().map(sort_value).collect())
            }
            other => other.clone(),
        }
    }
    sort_value(value).to_string()
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
/// and keys are visited in SORTED order to mirror C's iteration (see the
/// `Object` arm).
fn extract_json_field<'a>(
    json: &'a serde_json::Value,
    target_key: &str,
) -> Option<&'a serde_json::Value> {
    match json {
        serde_json::Value::Object(map) => {
            // C `findJsonField` iterates an `nlohmann::json` object whose
            // default backing is a sorted `std::map` (drvMqtt.h:19
            // `using json = nlohmann::json`), so keys are visited in sorted
            // order, each checked before descending into its value. serde_json's
            // `Map` is an `IndexMap` (document order) whenever `preserve_order`
            // is enabled — which the shipped IOC build is, transitively via
            // `epics-base-rs` — so iterate a sorted key view to reproduce C's
            // order deterministically, independent of the linked feature set.
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (key, value) in entries {
                if key.as_str() == target_key {
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
    let out: MqttResult<Vec<i32>> = if s.contains(',') {
        // First-seen comma locks the comma separator (C drvMqtt.cpp:473); a
        // comma skips trailing spaces (`:481-483`), so trim each element. A
        // double comma leaves an empty element that fails to parse — rejected,
        // matching C. The `"1 ,2"` leniency (space-then-comma) is kept.
        s.split(',')
            .map(|part| {
                part.trim()
                    .parse::<i32>()
                    .map_err(|e| MqttError::ValueConversion(format!("INTARRAY element: {e}")))
            })
            .collect()
    } else {
        // Space separator: C skips a *run* of whitespace between numbers
        // (loop-top `while isspace` skip, drvMqtt.cpp:447), so `"1  2"` is
        // `[1,2]`. `split_whitespace` collapses consecutive whitespace and
        // ignores leading/trailing, reproducing that (vs `split(' ')`, which
        // emitted an empty element on a double space and rejected the value).
        s.split_whitespace()
            .map(|part| {
                part.parse::<i32>()
                    .map_err(|e| MqttError::ValueConversion(format!("INTARRAY element: {e}")))
            })
            .collect()
    };
    let out = out?;
    if out.is_empty() {
        // C rejects an empty / all-whitespace / `"[]"` payload
        // (drvMqtt.cpp:428,444,448); never yield an empty array.
        return Err(MqttError::ValueConversion("INTARRAY: empty value".into()));
    }
    Ok(out)
}

/// Parse a comma-separated or space-separated list of floats.
fn parse_float_array(s: &str) -> MqttResult<Vec<f64>> {
    let s = s.trim_start_matches('[').trim_end_matches(']');
    let out: MqttResult<Vec<f64>> = if s.contains(',') {
        // First-seen comma locks the comma separator (C drvMqtt.cpp:550); see
        // `parse_int_array` for the comma/double-comma/`"1 ,2"` rationale.
        s.split(',')
            .map(|part| {
                let part = part.trim();
                // MQ37: C walks array elements with `std::strtod`
                // (drvMqtt.cpp:538), so they take the same C99 grammar the
                // scalar arm does -- hex significands included.
                parse_c_float(part).ok_or_else(|| {
                    MqttError::ValueConversion(format!("FLOATARRAY element: {part:?}"))
                })
            })
            .collect()
    } else {
        // Space separator collapses whitespace runs (C drvMqtt.cpp:532); see
        // `parse_int_array`.
        s.split_whitespace()
            .map(|part| {
                parse_c_float(part).ok_or_else(|| {
                    MqttError::ValueConversion(format!("FLOATARRAY element: {part:?}"))
                })
            })
            .collect()
    };
    let out = out?;
    if out.is_empty() {
        // C rejects an empty / all-whitespace / `"[]"` payload
        // (drvMqtt.cpp:511,527,533); never yield an empty array.
        return Err(MqttError::ValueConversion("FLOATARRAY: empty value".into()));
    }
    Ok(out)
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
    fn decode_flat_int_whitespace_rejected() {
        // C isInteger (drvMqtt.cpp:376-385) rejects any non-digit char, so a
        // payload with surrounding whitespace/newline is refused and the prior
        // VAL is kept -- it is NOT trimmed to 100.
        let addr = TopicAddress::parse("FLAT:INT test/t").unwrap();
        assert!(decode_payload("  100  \n", &addr).is_err());
        assert!(decode_payload("42\n", &addr).is_err());
        // A clean integer still parses.
        assert_eq!(
            decode_payload("100", &addr).unwrap(),
            DecodedValue::Int32(100)
        );
    }

    #[test]
    fn decode_flat_float_leading_ws_ok_trailing_rejected() {
        // C isFloat -> strtof (drvMqtt.cpp:393-401) skips leading whitespace
        // but requires whole-string consumption: " 4.2" is a valid 4.2, while
        // "4.2\n"/"4.2 " (trailing) are refused and the prior VAL kept.
        let f = TopicAddress::parse("FLAT:FLOAT test/t").unwrap();
        assert_eq!(
            decode_payload(" 4.2", &f).unwrap(),
            DecodedValue::Float64(4.2)
        );
        assert!(decode_payload("4.2\n", &f).is_err());
        assert!(decode_payload("4.2 ", &f).is_err());
        // DIGITAL uses isInteger (no ws skip), so any surrounding ws is refused.
        let d = TopicAddress::parse("FLAT:DIGITAL test/t").unwrap();
        assert!(decode_payload("7 ", &d).is_err());
        assert!(decode_payload(" 7", &d).is_err());
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

    /// C parity: DIGITAL uses `isInteger(val, isSigned=false)` (drvMqtt.cpp:294),
    /// so a leading sign is a non-digit and the payload is rejected. Rust's
    /// `u32::from_str` accepts a leading '+', so it must be guarded out.
    #[test]
    fn decode_flat_digital_rejects_leading_plus() {
        let addr = TopicAddress::parse("FLAT:DIGITAL test/t").unwrap();
        assert!(decode_payload("+5", &addr).is_err());
        // '-5' is rejected by u32::from_str already; confirm it stays rejected.
        assert!(decode_payload("-5", &addr).is_err());
        // A plain unsigned digit string still decodes.
        assert_eq!(decode_payload("5", &addr).unwrap(), DecodedValue::UInt32(5));
    }

    #[test]
    fn decode_flat_string() {
        let addr = TopicAddress::parse("FLAT:STRING test/t").unwrap();
        let val = decode_payload("hello world", &addr).unwrap();
        assert_eq!(val, DecodedValue::String("hello world".into()));
    }

    #[test]
    fn decode_flat_string_preserves_surrounding_whitespace() {
        // C setStringParam stores the raw payload verbatim (drvMqtt.cpp:297-299).
        let addr = TopicAddress::parse("FLAT:STRING test/t").unwrap();
        let val = decode_payload("  hello  ", &addr).unwrap();
        assert_eq!(val, DecodedValue::String("  hello  ".into()));
    }

    #[test]
    fn octet_cstr_truncates_at_first_nul() {
        // asyn octet values are NUL-terminated C-strings (drvMqtt.cpp:299,716).
        assert_eq!(octet_cstr("hi\0there"), "hi");
        assert_eq!(octet_cstr("\0rest"), "");
        // No NUL: full string, surrounding whitespace preserved.
        assert_eq!(octet_cstr("  hello  "), "  hello  ");
        assert_eq!(octet_cstr(""), "");
    }

    #[test]
    fn decode_flat_int_array_comma() {
        let addr = TopicAddress::parse("FLAT:INTARRAY test/t").unwrap();
        let val = decode_payload("1,2,3,4", &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32Array(vec![1, 2, 3, 4]));
    }

    /// MQ36: a FLOAT payload that overflows the double range must leave the
    /// record at its prior value, as C's `std::stod` out_of_range does
    /// (drvMqtt.cpp:286-287,327-331) -- not store an infinity.
    #[test]
    fn decode_flat_float_rejects_overflow_but_keeps_a_named_infinity() {
        let addr = TopicAddress::parse("FLAT:FLOAT t/f").unwrap();
        assert!(
            decode_payload("1e400", &addr).is_err(),
            "overflow must fail"
        );
        assert!(
            decode_payload("-1e400", &addr).is_err(),
            "negative overflow must fail"
        );

        // C stores a payload that names the value: strtof consumes "inf" with
        // no range error, so stod returns HUGE_VAL and never throws.
        assert_eq!(
            decode_payload("inf", &addr).unwrap(),
            DecodedValue::Float64(f64::INFINITY)
        );
        assert_eq!(
            decode_payload("-Infinity", &addr).unwrap(),
            DecodedValue::Float64(f64::NEG_INFINITY)
        );
        match decode_payload("nan", &addr).unwrap() {
            DecodedValue::Float64(v) => assert!(v.is_nan()),
            other => panic!("expected Float64(NaN), got {other:?}"),
        }

        // The ARRAY path keeps C's range behaviour: it parses elements with
        // bare `std::strtod` and ignores ERANGE (drvMqtt.cpp:538), so C stores
        // an infinity there and so must this port.
        let arr = TopicAddress::parse("FLAT:FLOATARRAY t/a").unwrap();
        assert_eq!(
            decode_payload("1e400 2", &arr).unwrap(),
            DecodedValue::Float64Array(vec![f64::INFINITY, 2.0])
        );
    }

    #[test]
    fn decode_flat_float_accepts_the_c99_strtod_grammar() {
        let addr = TopicAddress::parse("FLAT:FLOAT t/f").unwrap();

        // MQ37: `strtof`/`std::stod` take a hexadecimal significand, and the
        // binary exponent is optional. `f64::from_str` takes none of these.
        assert_eq!(
            decode_payload("0x1.8p3", &addr).unwrap(),
            DecodedValue::Float64(12.0)
        );
        assert_eq!(
            decode_payload("-0X1P-1", &addr).unwrap(),
            DecodedValue::Float64(-0.5)
        );
        assert_eq!(
            decode_payload("0x1.8", &addr).unwrap(),
            DecodedValue::Float64(1.5)
        );

        // ... and a NaN carrying a character sequence, empty sequence included.
        for text in ["nan(123)", "NAN(_ab9)", "nan()"] {
            match decode_payload(text, &addr).unwrap() {
                DecodedValue::Float64(v) => assert!(v.is_nan(), "{text} must be NaN"),
                other => panic!("expected Float64(NaN) for {text}, got {other:?}"),
            }
        }

        // Full consumption is still required: `isFloat` (drvMqtt.cpp:400)
        // compares strtof's end pointer against the end of the payload.
        for text in ["0x1.8p3junk", "0x1.8p", "0xg", "0x"] {
            assert!(
                decode_payload(text, &addr).is_err(),
                "{text} is not consumed to the end and must be refused"
            );
        }

        // The array walker uses `std::strtod` on each element (drvMqtt.cpp:538),
        // so it takes the same grammar in both separator styles.
        let arr = TopicAddress::parse("FLAT:FLOATARRAY t/a").unwrap();
        assert_eq!(
            decode_payload("0x1.8p3 2", &arr).unwrap(),
            DecodedValue::Float64Array(vec![12.0, 2.0])
        );
        assert_eq!(
            decode_payload("0x1p1, 0x1p2", &arr).unwrap(),
            DecodedValue::Float64Array(vec![2.0, 4.0])
        );
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

    /// MQ32: a space separator collapses a run of whitespace between numbers,
    /// matching C's loop-top `while isspace` skip (drvMqtt.cpp:447). `split(' ')`
    /// previously emitted an empty element on a double space and rejected the
    /// value — a Rust-worse-than-C regression.
    #[test]
    fn decode_flat_int_array_double_space() {
        let addr = TopicAddress::parse("FLAT:INTARRAY test/t").unwrap();
        assert_eq!(
            decode_payload("1  2", &addr).unwrap(),
            DecodedValue::Int32Array(vec![1, 2])
        );
        // Inside brackets too, after the bracket strip.
        assert_eq!(
            decode_payload("[1   2   3]", &addr).unwrap(),
            DecodedValue::Int32Array(vec![1, 2, 3])
        );
    }

    #[test]
    fn decode_flat_float_array_double_space() {
        let addr = TopicAddress::parse("FLAT:FLOATARRAY test/t").unwrap();
        assert_eq!(
            decode_payload("1.5   2.5", &addr).unwrap(),
            DecodedValue::Float64Array(vec![1.5, 2.5])
        );
    }

    /// MQ32: collapsing whitespace must NOT turn an empty / all-whitespace /
    /// `"[]"` payload into an empty array — C rejects those (drvMqtt.cpp:428,
    /// 444,448 for ints; :511,527,533 for floats).
    #[test]
    fn decode_flat_array_empty_is_rejected() {
        let int_addr = TopicAddress::parse("FLAT:INTARRAY test/t").unwrap();
        assert!(decode_payload("", &int_addr).is_err());
        assert!(decode_payload("   ", &int_addr).is_err());
        assert!(decode_payload("[]", &int_addr).is_err());
        assert!(decode_payload("[ ]", &int_addr).is_err());
        let flt_addr = TopicAddress::parse("FLAT:FLOATARRAY test/t").unwrap();
        assert!(decode_payload("", &flt_addr).is_err());
        assert!(decode_payload("[]", &flt_addr).is_err());
    }

    /// MQ32: the comma branch is unchanged — a double comma still rejects
    /// (empty element fails to parse, matching C's strictness), so the fix did
    /// not loosen the comma path.
    #[test]
    fn decode_flat_int_array_double_comma_still_rejected() {
        let addr = TopicAddress::parse("FLAT:INTARRAY test/t").unwrap();
        assert!(decode_payload("1,,2", &addr).is_err());
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
    fn decode_json_field_search_is_sorted_order() {
        // C `findJsonField` iterates a sorted `std::map` (drvMqtt.h:19), so a
        // key that recurs at two depths resolves in C's sorted-DFS order, NOT
        // document order. For target `value` in `{"value":1,"data":{"value":2}}`
        // the sorted top-level keys are [`data`, `value`]; C descends `data`
        // first and returns the nested 2 before ever reaching the top `value`.
        // Document-order iteration (serde_json `preserve_order`, the IOC build)
        // would wrongly return 1 — this pins the build-independent sorted order.
        let addr = TopicAddress::parse("JSON:INT dev/data value").unwrap();
        let val = decode_payload(r#"{"value": 1, "data": {"value": 2}}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32(2));
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

    /// MQ40: a JSON field resolving to an object/array on a STRING topic is
    /// stored as C's `dump()` does — compact with object keys SORTED at every
    /// level (nlohmann std::map backing, drvMqtt.cpp:265) — not in serde
    /// document order.
    #[test]
    fn decode_json_object_carrier_keys_sorted() {
        let addr = TopicAddress::parse("JSON:STRING dev/data obj").unwrap();
        // Document order is b,a but the stored carrier must be sorted a,b.
        let val = decode_payload(r#"{"obj": {"b": 1, "a": 2}}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::String(r#"{"a":2,"b":1}"#.into()));
    }

    #[test]
    fn decode_json_nested_object_carrier_keys_sorted() {
        let addr = TopicAddress::parse("JSON:STRING dev/data obj").unwrap();
        // Sorting is recursive: the nested object's keys sort too.
        let val = decode_payload(r#"{"obj": {"b": {"d": 1, "c": 2}, "a": 3}}"#, &addr).unwrap();
        assert_eq!(
            val,
            DecodedValue::String(r#"{"a":3,"b":{"c":2,"d":1}}"#.into())
        );
    }

    #[test]
    fn decode_json_array_of_objects_carrier_keys_sorted() {
        let addr = TopicAddress::parse("JSON:STRING dev/data arr").unwrap();
        // Objects nested inside an array carrier are sorted too; array element
        // order is preserved.
        let val =
            decode_payload(r#"{"arr": [{"y": 1, "x": 2}, {"b": 3, "a": 4}]}"#, &addr).unwrap();
        assert_eq!(
            val,
            DecodedValue::String(r#"[{"x":2,"y":1},{"a":4,"b":3}]"#.into())
        );
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

    // --- Boolean shortcut (C isBoolean, drvMqtt.cpp:278-293) ---

    #[test]
    fn decode_flat_int_boolean() {
        // C maps "true"/"false" to 1/0 before the integer parse for INT
        // (drvMqtt.cpp:278-281).
        let addr = TopicAddress::parse("FLAT:INT test/t").unwrap();
        assert_eq!(
            decode_payload("true", &addr).unwrap(),
            DecodedValue::Int32(1)
        );
        assert_eq!(
            decode_payload("false", &addr).unwrap(),
            DecodedValue::Int32(0)
        );
    }

    #[test]
    fn decode_flat_digital_boolean() {
        // Same shortcut for DIGITAL (drvMqtt.cpp:290-293).
        let addr = TopicAddress::parse("FLAT:DIGITAL test/t").unwrap();
        assert_eq!(
            decode_payload("true", &addr).unwrap(),
            DecodedValue::UInt32(1)
        );
        assert_eq!(
            decode_payload("false", &addr).unwrap(),
            DecodedValue::UInt32(0)
        );
    }

    #[test]
    fn decode_flat_float_rejects_boolean() {
        // C has no isBoolean branch for asynParamFloat64 (drvMqtt.cpp:285-287):
        // a boolean payload on a float topic is a parse error, not 1.0.
        let addr = TopicAddress::parse("FLAT:FLOAT test/t").unwrap();
        assert!(decode_payload("true", &addr).is_err());
    }

    #[test]
    fn decode_json_int_boolean() {
        // C dumps a JSON boolean field to "true"/"false" then runs the same
        // isBoolean shortcut (drvMqtt.cpp:265,278).
        let addr = TopicAddress::parse("JSON:INT dev/data flag").unwrap();
        assert_eq!(
            decode_payload(r#"{"flag": true}"#, &addr).unwrap(),
            DecodedValue::Int32(1)
        );
        assert_eq!(
            decode_payload(r#"{"flag": false}"#, &addr).unwrap(),
            DecodedValue::Int32(0)
        );
    }

    #[test]
    fn decode_json_digital_boolean() {
        let addr = TopicAddress::parse("JSON:DIGITAL dev/data flag").unwrap();
        assert_eq!(
            decode_payload(r#"{"flag": true}"#, &addr).unwrap(),
            DecodedValue::UInt32(1)
        );
    }

    #[test]
    fn decode_json_int_string_boolean() {
        // A JSON string field "true" is fetched via get<std::string>() and
        // takes the same isBoolean shortcut in C (drvMqtt.cpp:263,278).
        let addr = TopicAddress::parse("JSON:INT dev/data flag").unwrap();
        assert_eq!(
            decode_payload(r#"{"flag": "true"}"#, &addr).unwrap(),
            DecodedValue::Int32(1)
        );
    }

    #[test]
    fn decode_json_float_rejects_boolean() {
        // No isBoolean for float (drvMqtt.cpp:285-287); a JSON boolean on a
        // float topic stays an error.
        let addr = TopicAddress::parse("JSON:FLOAT dev/data flag").unwrap();
        assert!(decode_payload(r#"{"flag": true}"#, &addr).is_err());
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

    /// A scalar JSON number on an INTARRAY topic is NOT rejected: C dumps
    /// it to "42" and `checkAndParseIntArray` accepts a lone value as a
    /// one-element array — it breaks right after the first number with
    /// `asynSuccess` (drvMqtt.cpp:465-487). The carrier delegation
    /// reproduces that through `parse_int_array`. (The earlier
    /// `as_array()` path rejected it, diverging from C.)
    #[test]
    fn decode_json_scalar_number_is_single_element_int_array() {
        let addr = TopicAddress::parse("JSON:INTARRAY sensors/data readings").unwrap();
        let val = decode_payload(r#"{"readings": 42}"#, &addr).unwrap();
        assert_eq!(val, DecodedValue::Int32Array(vec![42]));
    }

    /// Regression: C builds a string
    /// carrier from the matched JSON field — `get<std::string>()` for a
    /// JSON string, `dump()` otherwise (drvMqtt.cpp:262-265) — then runs
    /// the same flat numeric/array parsers. A broker that encodes every
    /// value as a JSON string must therefore still drive INT/FLOAT/
    /// DIGITAL/INTARRAY/FLOATARRAY records. The earlier per-type
    /// `as_i64`/`as_f64`/`as_array` path rejected every one of these.
    #[test]
    fn decode_json_string_carried_numeric_and_array() {
        let int = TopicAddress::parse("JSON:INT dev/d v").unwrap();
        assert_eq!(
            decode_payload(r#"{"v": "42"}"#, &int).unwrap(),
            DecodedValue::Int32(42)
        );

        let float = TopicAddress::parse("JSON:FLOAT dev/d v").unwrap();
        assert_eq!(
            decode_payload(r#"{"v": "12.5"}"#, &float).unwrap(),
            DecodedValue::Float64(12.5)
        );

        let digital = TopicAddress::parse("JSON:DIGITAL dev/d v").unwrap();
        assert_eq!(
            decode_payload(r#"{"v": "7"}"#, &digital).unwrap(),
            DecodedValue::UInt32(7)
        );

        let int_arr = TopicAddress::parse("JSON:INTARRAY dev/d v").unwrap();
        assert_eq!(
            decode_payload(r#"{"v": "[1,2,3]"}"#, &int_arr).unwrap(),
            DecodedValue::Int32Array(vec![1, 2, 3])
        );

        let float_arr = TopicAddress::parse("JSON:FLOATARRAY dev/d v").unwrap();
        assert_eq!(
            decode_payload(r#"{"v": "[1.5, 2.5]"}"#, &float_arr).unwrap(),
            DecodedValue::Float64Array(vec![1.5, 2.5])
        );
    }

    /// An explicit JSON `null` for a field is treated as "not found", as
    /// C does (`!fieldAddr || fieldAddr->is_null()`, drvMqtt.cpp:260).
    #[test]
    fn decode_json_explicit_null_is_not_found() {
        let addr = TopicAddress::parse("JSON:INT dev/d v").unwrap();
        assert!(decode_payload(r#"{"v": null}"#, &addr).is_err());
    }

    /// BUG 5 — the FLAT INTARRAY decode path (already producing
    /// `Int32Array`) stays intact; the event loop now delivers it via
    /// `ParamSetValue::Value` carrying a `ParamValue::Int32Array`.
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
        // C `std::to_string(double)` = sprintf "%f" → fixed 6 decimals
        // (drvMqtt.cpp:651), not the shortest round-trip "3.15".
        assert_eq!(encode_flat(&DecodedValue::Float64(3.15)), "3.150000");
        assert_eq!(encode_flat(&DecodedValue::Float64(22.5)), "22.500000");
        assert_eq!(encode_flat(&DecodedValue::UInt32(255)), "255");
        assert_eq!(encode_flat(&DecodedValue::String("hello".into())), "hello");
    }

    #[test]
    fn encode_flat_arrays() {
        assert_eq!(
            encode_flat(&DecodedValue::Int32Array(vec![1, 2, 3])),
            "1,2,3"
        );
        // FLAT float arrays stream through default ostream = "%g"
        // 6-significant-digit (drvMqtt.cpp:685); short values are unchanged.
        assert_eq!(
            encode_flat(&DecodedValue::Float64Array(vec![1.1, 2.2])),
            "1.1,2.2"
        );
        // ...but more-than-6-significant-digit / large-magnitude values are
        // rounded by "%g" where the old shortest-round-trip kept full form.
        assert_eq!(
            encode_flat(&DecodedValue::Float64Array(vec![
                std::f64::consts::PI,
                1234567.0
            ])),
            "3.14159,1.23457e+06"
        );
    }

    #[test]
    fn format_ostream_double_matches_g6() {
        // Fixed-style branch (-4 <= exp < 6), trailing zeros stripped.
        assert_eq!(format_ostream_double(22.5), "22.5");
        assert_eq!(format_ostream_double(1.0), "1");
        assert_eq!(format_ostream_double(100.0), "100");
        assert_eq!(format_ostream_double(0.1), "0.1");
        assert_eq!(format_ostream_double(std::f64::consts::PI), "3.14159");
        assert_eq!(format_ostream_double(0.0001234567), "0.000123457");
        assert_eq!(format_ostream_double(999999.0), "999999");
        assert_eq!(format_ostream_double(-22.5), "-22.5");
        // Scientific-style branch (exp >= 6 or exp < -4), C exponent form.
        assert_eq!(format_ostream_double(1234567.0), "1.23457e+06");
        assert_eq!(format_ostream_double(1000000.0), "1e+06");
        assert_eq!(format_ostream_double(0.00001234567), "1.23457e-05");
        // Post-rounding exponent carry (9999999 -> 1.00000e7).
        assert_eq!(format_ostream_double(9999999.0), "1e+07");
        // Zero and signed zero.
        assert_eq!(format_ostream_double(0.0), "0");
        assert_eq!(format_ostream_double(-0.0), "-0");
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
