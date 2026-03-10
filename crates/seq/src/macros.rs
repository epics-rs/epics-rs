use std::collections::HashMap;

/// Parse macro string in "key=value,key2=value2" format.
pub fn parse_macros(input: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if input.is_empty() {
        return map;
    }
    for pair in input.split(',') {
        if let Some((key, val)) = pair.split_once('=') {
            map.insert(key.trim().to_string(), val.trim().to_string());
        }
    }
    map
}

/// Substitute macros in a PV name template.
/// Replaces `{key}` and `$(key)` patterns with values from the map.
pub fn substitute(template: &str, macros: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, val) in macros {
        result = result.replace(&format!("{{{key}}}"), val);
        result = result.replace(&format!("$({key})"), val);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_macros() {
        let m = parse_macros("P=TEST:,R=ai1");
        assert_eq!(m.get("P").unwrap(), "TEST:");
        assert_eq!(m.get("R").unwrap(), "ai1");
    }

    #[test]
    fn test_substitute() {
        let m = parse_macros("P=IOC:");
        assert_eq!(substitute("{P}voltage", &m), "IOC:voltage");
        assert_eq!(substitute("$(P)voltage", &m), "IOC:voltage");
    }
}
