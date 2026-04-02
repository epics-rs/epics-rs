use std::path::Path;

use crate::types::EpicsValue;
use chrono::Local;

use super::error::{AutosaveError, AutosaveResult};
use super::format::{ARRAY_MARKER, END_MARKER, VERSION};

/// A single PV entry in a .sav file.
#[derive(Debug, Clone)]
pub struct SaveEntry {
    pub pv_name: String,
    pub value: String,
    pub connected: bool,
}

/// Write a .sav file atomically (tmp -> fsync -> rename).
pub async fn write_save_file(path: &Path, entries: &[SaveEntry]) -> AutosaveResult<()> {
    let mut content = String::new();

    // Header
    let now = Local::now();
    content.push_str(&format!(
        "# {}\t{}\n",
        VERSION,
        now.format("%Y-%m-%d %H:%M:%S")
    ));

    for entry in entries {
        if entry.connected {
            content.push_str(&entry.pv_name);
            content.push(' ');
            content.push_str(&entry.value);
            content.push('\n');
        } else {
            content.push_str(&format!("#{}\t(not connected)\n", entry.pv_name));
        }
    }

    content.push_str(END_MARKER);
    content.push('\n');

    // Atomic write: tmp -> fsync -> rename
    let tmp_path = path.with_extension("tmp");
    tokio::fs::write(&tmp_path, content.as_bytes()).await?;
    // fsync the file
    let file = tokio::fs::File::open(&tmp_path).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp_path, path).await?;

    Ok(())
}

/// Read a .sav file and validate `<END>` marker.
/// Returns None for corrupt files (no END marker).
pub async fn read_save_file(path: &Path) -> AutosaveResult<Option<Vec<SaveEntry>>> {
    let content = tokio::fs::read_to_string(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            e.into()
        } else {
            AutosaveError::CorruptSaveFile {
                path: path.display().to_string(),
                message: e.to_string(),
            }
        }
    })?;

    if !has_end_marker(&content) {
        return Ok(None);
    }

    let entries = parse_save_content(&content);
    Ok(Some(entries))
}

/// Quick check if a .sav file has a valid `<END>` marker.
pub async fn validate_save_file(path: &Path) -> AutosaveResult<bool> {
    let content = tokio::fs::read_to_string(path).await?;
    Ok(has_end_marker(&content))
}

fn has_end_marker(content: &str) -> bool {
    for line in content.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return trimmed == END_MARKER;
    }
    false
}

fn parse_save_content(content: &str) -> Vec<SaveEntry> {
    let mut entries = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == END_MARKER {
            break;
        }
        // Header/comment lines
        if line.starts_with('#') {
            // Check for disconnected PV: #PVNAME\t(not connected)
            let inner = &line[1..];
            if inner.contains("(not connected)") {
                let pv_name = inner.split(['\t', ' ']).next().unwrap_or("").trim();
                if !pv_name.is_empty() {
                    entries.push(SaveEntry {
                        pv_name: pv_name.to_string(),
                        value: String::new(),
                        connected: false,
                    });
                }
            }
            continue;
        }

        // C autosave @array@ format
        if line.contains(ARRAY_MARKER) {
            if let Some(entry) = parse_c_array_line(line, content) {
                entries.push(entry);
                continue;
            }
        }

        // Normal line: PV_NAME<space>VALUE
        if let Some(space_pos) = line.find(' ') {
            let pv_name = &line[..space_pos];
            let value = &line[space_pos + 1..];
            entries.push(SaveEntry {
                pv_name: pv_name.to_string(),
                value: value.to_string(),
                connected: true,
            });
        }
    }

    entries
}

/// Parse a C autosave @array@ line.
fn parse_c_array_line(line: &str, _full_content: &str) -> Option<SaveEntry> {
    // Format: PV_NAME @array@ { "e1" "e2" "e3" }
    let marker_pos = line.find(ARRAY_MARKER)?;
    let pv_name = line[..marker_pos].trim();
    let rest = line[marker_pos + ARRAY_MARKER.len()..].trim();

    if !rest.starts_with('{') || !rest.ends_with('}') {
        return None;
    }

    let inner = rest[1..rest.len() - 1].trim();
    let elements = parse_c_array_elements(inner);
    let value = format!("[{}]", elements.join(","));

    Some(SaveEntry {
        pv_name: pv_name.to_string(),
        value,
        connected: true,
    })
}

/// Parse C array elements: `"e1" "e2" "e3"` or `1.0 2.0 3.0`
fn parse_c_array_elements(s: &str) -> Vec<String> {
    let mut elements = Vec::new();
    let mut chars = s.chars().peekable();

    loop {
        // Skip whitespace
        while chars.peek().map_or(false, |c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }

        if chars.peek() == Some(&'"') {
            // Quoted string element
            chars.next(); // skip opening quote
            let mut elem = String::new();
            loop {
                match chars.next() {
                    Some('\\') => {
                        if let Some(c) = chars.next() {
                            elem.push(c);
                        }
                    }
                    Some('"') => break,
                    Some(c) => elem.push(c),
                    None => break,
                }
            }
            elements.push(elem);
        } else {
            // Unquoted element (number)
            let mut elem = String::new();
            while chars.peek().map_or(false, |c| !c.is_whitespace()) {
                elem.push(chars.next().unwrap());
            }
            if !elem.is_empty() {
                elements.push(elem);
            }
        }
    }

    elements
}

/// Convert an EpicsValue to a save file string.
pub fn value_to_save_str(value: &EpicsValue) -> String {
    match value {
        EpicsValue::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        EpicsValue::Double(v) => format!("{:.14e}", v),
        EpicsValue::Float(v) => format!("{:.7e}", v),
        EpicsValue::Short(v) => v.to_string(),
        EpicsValue::Long(v) => v.to_string(),
        EpicsValue::Enum(v) => v.to_string(),
        EpicsValue::Char(v) => v.to_string(),
        EpicsValue::DoubleArray(arr) => {
            let parts: Vec<_> = arr.iter().map(|v| format!("{:.14e}", v)).collect();
            format!("[{}]", parts.join(","))
        }
        EpicsValue::LongArray(arr) => {
            let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
            format!("[{}]", parts.join(","))
        }
        EpicsValue::CharArray(arr) => {
            let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
            format!("[{}]", parts.join(","))
        }
        EpicsValue::ShortArray(arr) => {
            let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
            format!("[{}]", parts.join(","))
        }
        EpicsValue::FloatArray(arr) => {
            let parts: Vec<_> = arr.iter().map(|v| format!("{:.7e}", v)).collect();
            format!("[{}]", parts.join(","))
        }
        EpicsValue::EnumArray(arr) => {
            let parts: Vec<_> = arr.iter().map(|v| v.to_string()).collect();
            format!("[{}]", parts.join(","))
        }
    }
}

/// Parse a save file value string back to EpicsValue, using template for type.
pub fn parse_save_value(s: &str, template: &EpicsValue) -> Option<EpicsValue> {
    let s = s.trim();
    match template {
        EpicsValue::String(_) => {
            if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
                let inner = &s[1..s.len() - 1];
                let unescaped = inner.replace("\\\"", "\"").replace("\\\\", "\\");
                Some(EpicsValue::String(unescaped))
            } else {
                Some(EpicsValue::String(s.to_string()))
            }
        }
        EpicsValue::Double(_) => s.parse::<f64>().ok().map(EpicsValue::Double),
        EpicsValue::Float(_) => s.parse::<f32>().ok().map(EpicsValue::Float),
        EpicsValue::Long(_) => s.parse::<i32>().ok().map(EpicsValue::Long),
        EpicsValue::Short(_) => s.parse::<i16>().ok().map(EpicsValue::Short),
        EpicsValue::Enum(_) => s.parse::<u16>().ok().map(EpicsValue::Enum),
        EpicsValue::Char(_) => s.parse::<u8>().ok().map(EpicsValue::Char),
        EpicsValue::DoubleArray(_) => {
            parse_array_str(s, |v| v.parse::<f64>().ok()).map(EpicsValue::DoubleArray)
        }
        EpicsValue::LongArray(_) => {
            parse_array_str(s, |v| v.parse::<i32>().ok()).map(EpicsValue::LongArray)
        }
        EpicsValue::CharArray(_) => {
            parse_array_str(s, |v| v.parse::<u8>().ok()).map(EpicsValue::CharArray)
        }
        EpicsValue::ShortArray(_) => {
            parse_array_str(s, |v| v.parse::<i16>().ok()).map(EpicsValue::ShortArray)
        }
        EpicsValue::FloatArray(_) => {
            parse_array_str(s, |v| v.parse::<f32>().ok()).map(EpicsValue::FloatArray)
        }
        EpicsValue::EnumArray(_) => {
            parse_array_str(s, |v| v.parse::<u16>().ok()).map(EpicsValue::EnumArray)
        }
    }
}

fn parse_array_str<T, F>(s: &str, parse_elem: F) -> Option<Vec<T>>
where
    F: Fn(&str) -> Option<T>,
{
    let inner = s.trim_start_matches('[').trim_end_matches(']');
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner.split(',').map(|v| parse_elem(v.trim())).collect()
}
