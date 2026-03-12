use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::error::{CaError, CaResult};
use crate::server::record::Record;
use crate::types::EpicsValue;

/// Factory function that creates a record instance.
pub type RecordFactory = Box<dyn Fn() -> Box<dyn Record> + Send + Sync>;

/// Global registry of external record type factories.
/// External crates (e.g., asyn-rs) can register their own record types
/// to override built-in stubs.
static RECORD_FACTORY_REGISTRY: OnceLock<Mutex<HashMap<String, RecordFactory>>> = OnceLock::new();

fn get_registry() -> &'static Mutex<HashMap<String, RecordFactory>> {
    RECORD_FACTORY_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register an external record type factory.
/// This allows external crates to override built-in record stubs.
/// The factory is checked FIRST in `create_record()`, so it takes priority.
pub fn register_record_type(name: &str, factory: RecordFactory) {
    let mut reg = get_registry().lock().unwrap();
    reg.insert(name.to_string(), factory);
}

/// A record definition parsed from a .db file.
pub struct DbRecordDef {
    pub record_type: String,
    pub name: String,
    pub fields: Vec<(String, String)>,
}

/// Parse an EPICS .db file with macro substitution.
pub fn parse_db(input: &str, macros: &HashMap<String, String>) -> CaResult<Vec<DbRecordDef>> {
    let expanded = substitute_macros(input, macros);
    let mut records = Vec::new();
    let chars: Vec<char> = expanded.chars().collect();
    let mut pos = 0;
    let mut line = 1;
    let mut col = 1;

    while pos < chars.len() {
        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        if pos >= chars.len() {
            break;
        }

        // Expect "record" keyword
        let word = read_word(&chars, &mut pos, &mut col);
        if word.is_empty() {
            pos += 1;
            col += 1;
            continue;
        }
        if word != "record" && word != "grecord" {
            return Err(CaError::DbParseError {
                line,
                column: col,
                message: format!("expected 'record', got '{word}'"),
            });
        }

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        expect_char(&chars, &mut pos, &mut col, '(', line)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        let rec_type = read_word(&chars, &mut pos, &mut col);

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        expect_char(&chars, &mut pos, &mut col, ',', line)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        let name = read_quoted_string(&chars, &mut pos, &mut line, &mut col)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        expect_char(&chars, &mut pos, &mut col, ')', line)?;

        skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
        expect_char(&chars, &mut pos, &mut col, '{', line)?;

        let mut fields = Vec::new();
        loop {
            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            if pos >= chars.len() {
                return Err(CaError::DbParseError {
                    line,
                    column: col,
                    message: "unexpected end of file in record body".into(),
                });
            }
            if chars[pos] == '}' {
                pos += 1;
                col += 1;
                break;
            }

            let kw = read_word(&chars, &mut pos, &mut col);
            if kw != "field" && kw != "info" && kw != "alias" {
                return Err(CaError::DbParseError {
                    line,
                    column: col,
                    message: format!("expected 'field', got '{kw}'"),
                });
            }

            if kw == "info" || kw == "alias" {
                // Skip info/alias: consume until matching ')'
                skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
                if pos < chars.len() && chars[pos] == '(' {
                    let mut depth = 1;
                    pos += 1;
                    col += 1;
                    while pos < chars.len() && depth > 0 {
                        match chars[pos] {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            '\n' => { line += 1; col = 0; }
                            _ => {}
                        }
                        pos += 1;
                        col += 1;
                    }
                }
                continue;
            }

            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, '(', line)?;

            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let field_name = read_word(&chars, &mut pos, &mut col);

            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, ',', line)?;

            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            let field_value = read_field_value(&chars, &mut pos, &mut line, &mut col)?;

            skip_whitespace_and_comments(&chars, &mut pos, &mut line, &mut col);
            expect_char(&chars, &mut pos, &mut col, ')', line)?;

            fields.push((field_name, field_value));
        }

        records.push(DbRecordDef {
            record_type: rec_type,
            name,
            fields,
        });
    }

    Ok(records)
}

/// Configuration for file-based DB loading with include support.
pub struct DbLoadConfig {
    pub include_paths: Vec<PathBuf>,
    pub max_include_depth: usize,
}

impl Default for DbLoadConfig {
    fn default() -> Self {
        Self {
            include_paths: Vec::new(),
            max_include_depth: 32,
        }
    }
}

/// File-based entry point: expand includes, substitute macros, parse records.
pub fn parse_db_file(
    path: &Path,
    macros: &HashMap<String, String>,
    config: &DbLoadConfig,
) -> CaResult<Vec<DbRecordDef>> {
    let content = expand_includes(path, macros, config)?;
    parse_db(&content, macros)
}

/// Expand `include "..."` directives recursively.
pub fn expand_includes(
    path: &Path,
    macros: &HashMap<String, String>,
    config: &DbLoadConfig,
) -> CaResult<String> {
    let canonical = path.canonicalize().map_err(|e| CaError::DbParseError {
        line: 0,
        column: 0,
        message: format!("cannot resolve '{}': {}", path.display(), e),
    })?;
    let mut stack = Vec::new();
    expand_includes_inner(&canonical, macros, config, &mut stack)
}

fn expand_includes_inner(
    path: &Path,
    macros: &HashMap<String, String>,
    config: &DbLoadConfig,
    stack: &mut Vec<PathBuf>,
) -> CaResult<String> {
    // Circular include detection
    if stack.iter().any(|p| p == path) {
        let chain: Vec<String> = stack.iter().map(|p| p.display().to_string()).collect();
        return Err(CaError::DbParseError {
            line: 0,
            column: 0,
            message: format!(
                "circular include: {} -> {}",
                chain.join(" -> "),
                path.display()
            ),
        });
    }

    // Depth limit
    if stack.len() >= config.max_include_depth {
        return Err(CaError::DbParseError {
            line: 0,
            column: 0,
            message: format!(
                "include depth limit ({}) exceeded at '{}'",
                config.max_include_depth,
                path.display()
            ),
        });
    }

    let content = std::fs::read_to_string(path).map_err(|e| CaError::DbParseError {
        line: 0,
        column: 0,
        message: format!("cannot read '{}': {}", path.display(), e),
    })?;

    let parent_dir = path.parent().unwrap_or(Path::new("."));
    stack.push(path.to_path_buf());

    // Local macro overrides from `substitute` directives.
    // These override the caller-provided macros for subsequent includes.
    let mut local_macros = macros.clone();

    let mut output = String::with_capacity(content.len());
    for line in content.lines() {
        if let Some(subst_str) = parse_substitute_directive(line) {
            // Apply substitute overrides to local macros
            for pair in subst_str.split(',') {
                if let Some((k, v)) = pair.split_once('=') {
                    let expanded_v = substitute_macros(v.trim(), &local_macros);
                    local_macros.insert(k.trim().to_string(), expanded_v);
                }
            }
        } else if let Some(filename) = parse_include_directive(line) {
            let expanded_filename = substitute_macros(&filename, &local_macros);
            let include_path =
                resolve_include_path(&expanded_filename, parent_dir, &config.include_paths)?;
            let canonical = include_path
                .canonicalize()
                .map_err(|e| CaError::DbParseError {
                    line: 0,
                    column: 0,
                    message: format!("cannot resolve '{}': {}", include_path.display(), e),
                })?;
            let included = expand_includes_inner(&canonical, &local_macros, config, stack)?;
            output.push_str(&included);
            output.push('\n');
        } else {
            // Apply current macros (including substitute overrides) to content lines
            let expanded_line = substitute_macros(line, &local_macros);
            output.push_str(&expanded_line);
            output.push('\n');
        }
    }

    stack.pop();
    Ok(output)
}

/// Parse an include directive line. Returns the filename if the line is an include directive.
fn parse_include_directive(line: &str) -> Option<String> {
    let trimmed = line.trim();
    // Comment lines are not include directives
    if trimmed.starts_with('#') {
        return None;
    }
    if !trimmed.starts_with("include") {
        return None;
    }
    // Must have whitespace or quote after "include"
    let rest = &trimmed["include".len()..];
    if rest.is_empty() {
        return None;
    }
    let first = rest.chars().next().unwrap();
    if !first.is_whitespace() && first != '"' {
        return None;
    }
    // Extract quoted filename
    let quote_start = rest.find('"')?;
    let after_quote = &rest[quote_start + 1..];
    let quote_end = after_quote.find('"')?;
    Some(after_quote[..quote_end].to_string())
}

/// Parse a `substitute` directive line.
///
/// EPICS DB files use `substitute "NAME=VALUE"` to override macros for subsequent
/// `include` directives. Returns the quoted content if the line is a substitute directive.
fn parse_substitute_directive(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return None;
    }
    if !trimmed.starts_with("substitute") {
        return None;
    }
    let rest = &trimmed["substitute".len()..];
    if rest.is_empty() {
        return None;
    }
    let first = rest.chars().next().unwrap();
    if !first.is_whitespace() && first != '"' {
        return None;
    }
    // Extract quoted content
    let quote_start = rest.find('"')?;
    let after_quote = &rest[quote_start + 1..];
    let quote_end = after_quote.find('"')?;
    Some(after_quote[..quote_end].to_string())
}

/// Resolve an include filename to a path.
/// Search order: current file's directory → config.include_paths.
fn resolve_include_path(
    filename: &str,
    current_dir: &Path,
    include_paths: &[PathBuf],
) -> CaResult<PathBuf> {
    let file_path = Path::new(filename);

    // Absolute path: use directly
    if file_path.is_absolute() {
        if file_path.exists() {
            return Ok(file_path.to_path_buf());
        }
        return Err(CaError::DbParseError {
            line: 0,
            column: 0,
            message: format!("include file not found: '{filename}'"),
        });
    }

    // Relative: search current dir first
    let candidate = current_dir.join(file_path);
    if candidate.exists() {
        return Ok(candidate);
    }

    // Then search include paths
    for dir in include_paths {
        let candidate = dir.join(file_path);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(CaError::DbParseError {
        line: 0,
        column: 0,
        message: format!("include file not found: '{filename}' (searched: {}, {})",
            current_dir.display(),
            include_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")),
    })
}

/// Override DTYP fields on records that already have a DTYP field.
pub fn override_dtyp(records: &mut [DbRecordDef], dtyp: &str) {
    for rec in records.iter_mut() {
        for (name, value) in rec.fields.iter_mut() {
            if name == "DTYP" {
                *value = dtyp.to_string();
            }
        }
    }
}

fn substitute_macros(input: &str, macros: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == '$' && (chars[i + 1] == '(' || chars[i + 1] == '{') {
            let close = if chars[i + 1] == '(' { ')' } else { '}' };
            // Find matching close bracket, respecting nested $() / ${}
            let start = i + 2;
            let mut depth = 1usize;
            let mut j = start;
            while j < chars.len() && depth > 0 {
                if j + 1 < chars.len() && chars[j] == '$' && (chars[j + 1] == '(' || chars[j + 1] == '{') {
                    depth += 1;
                    j += 2;
                    continue;
                }
                // Only match the corresponding bracket type at the outermost level
                if (depth == 1 && chars[j] == close) || (depth > 1 && (chars[j] == ')' || chars[j] == '}')) {
                    depth -= 1;
                    if depth == 0 { break; }
                }
                j += 1;
            }
            if depth == 0 {
                let macro_content: String = chars[start..j].iter().collect();
                let (name, default) = if let Some(eq_pos) = macro_content.find('=') {
                    (&macro_content[..eq_pos], Some(&macro_content[eq_pos + 1..]))
                } else {
                    (macro_content.as_str(), None)
                };

                if let Some(val) = macros.get(name) {
                    result.push_str(val);
                } else if let Some(def) = default {
                    // Strip outer quotes from default: $(NAME="value") → value
                    // Matches C EPICS macLib behavior
                    let def = if def.starts_with('"') && def.ends_with('"') && def.len() >= 2 {
                        &def[1..def.len() - 1]
                    } else {
                        def
                    };
                    // Recursively expand macros within the default value
                    // e.g. $(TS_PORT=$(PORT)_TS) with PORT=ATTR1 → ATTR1_TS
                    let expanded = substitute_macros(def, macros);
                    result.push_str(&expanded);
                } else {
                    // Leave macro unexpanded
                    result.push_str(&format!("$({macro_content})"));
                }
                i = j + 1;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }

    result
}

fn skip_whitespace_and_comments(chars: &[char], pos: &mut usize, line: &mut usize, col: &mut usize) {
    while *pos < chars.len() {
        match chars[*pos] {
            ' ' | '\t' | '\r' => { *pos += 1; *col += 1; }
            '\n' => { *pos += 1; *line += 1; *col = 1; }
            '#' => {
                // Line comment
                while *pos < chars.len() && chars[*pos] != '\n' {
                    *pos += 1;
                }
            }
            _ => break,
        }
    }
}

fn read_word(chars: &[char], pos: &mut usize, col: &mut usize) -> String {
    let mut word = String::new();
    while *pos < chars.len() && (chars[*pos].is_ascii_alphanumeric() || chars[*pos] == '_') {
        word.push(chars[*pos]);
        *pos += 1;
        *col += 1;
    }
    word
}

fn read_quoted_string(chars: &[char], pos: &mut usize, line: &mut usize, col: &mut usize) -> CaResult<String> {
    if *pos >= chars.len() || chars[*pos] != '"' {
        return Err(CaError::DbParseError {
            line: *line,
            column: *col,
            message: "expected '\"'".into(),
        });
    }
    *pos += 1;
    *col += 1;

    let mut s = String::new();
    while *pos < chars.len() && chars[*pos] != '"' {
        if chars[*pos] == '\\' && *pos + 1 < chars.len() {
            *pos += 1;
            *col += 1;
            match chars[*pos] {
                '"' => s.push('"'),
                '\\' => s.push('\\'),
                'n' => s.push('\n'),
                other => { s.push('\\'); s.push(other); }
            }
        } else if chars[*pos] == '\n' {
            *line += 1;
            *col = 0;
            s.push('\n');
        } else {
            s.push(chars[*pos]);
        }
        *pos += 1;
        *col += 1;
    }

    if *pos >= chars.len() {
        return Err(CaError::DbParseError {
            line: *line,
            column: *col,
            message: "unterminated string".into(),
        });
    }
    *pos += 1; // skip closing "
    *col += 1;
    Ok(s)
}

fn read_field_value(chars: &[char], pos: &mut usize, line: &mut usize, col: &mut usize) -> CaResult<String> {
    if *pos < chars.len() && chars[*pos] == '"' {
        return read_quoted_string(chars, pos, line, col);
    }

    // Unquoted value: read until ')' or ','
    let mut s = String::new();
    while *pos < chars.len() && chars[*pos] != ')' && chars[*pos] != ',' {
        if chars[*pos] == '\n' {
            *line += 1;
            *col = 0;
        }
        s.push(chars[*pos]);
        *pos += 1;
        *col += 1;
    }
    Ok(s.trim().to_string())
}

fn expect_char(chars: &[char], pos: &mut usize, col: &mut usize, expected: char, line: usize) -> CaResult<()> {
    if *pos >= chars.len() || chars[*pos] != expected {
        let got = if *pos < chars.len() { chars[*pos].to_string() } else { "EOF".to_string() };
        return Err(CaError::DbParseError {
            line,
            column: *col,
            message: format!("expected '{expected}', got '{got}'"),
        });
    }
    *pos += 1;
    *col += 1;
    Ok(())
}

/// Create a record from a type name.
/// Checks the external factory registry first, then falls back to built-in types.
pub fn create_record(record_type: &str) -> CaResult<Box<dyn Record>> {
    // Check external registry first (allows overrides from e.g. asyn-rs)
    if let Ok(reg) = get_registry().lock() {
        if let Some(factory) = reg.get(record_type) {
            return Ok(factory());
        }
    }

    use crate::server::records::*;

    match record_type {
        "ai" => Ok(Box::new(ai::AiRecord::default())),
        "ao" => Ok(Box::new(ao::AoRecord::default())),
        "bi" => Ok(Box::new(bi::BiRecord::default())),
        "bo" => Ok(Box::new(bo::BoRecord::default())),
        "busy" => Ok(Box::new(busy::BusyRecord::default())),
        "stringin" => Ok(Box::new(stringin::StringinRecord::default())),
        "asyn" => Ok(Box::new(asyn_record::AsynRecord::default())),
        "stringout" => Ok(Box::new(stringout::StringoutRecord::default())),
        "longin" => Ok(Box::new(longin::LonginRecord::default())),
        "longout" => Ok(Box::new(longout::LongoutRecord::default())),
        "mbbi" => Ok(Box::new(mbbi::MbbiRecord::default())),
        "mbbo" => Ok(Box::new(mbbo::MbboRecord::default())),
        "waveform" | "subArray" => Ok(Box::new(waveform::WaveformRecord::default())),
        "calc" => Ok(Box::new(calc::CalcRecord::default())),
        "fanout" => Ok(Box::new(fanout::FanoutRecord::default())),
        "seq" => Ok(Box::new(seq::SeqRecord::default())),
        "sseq" => Ok(Box::new(sseq::SseqRecord::default())),
        "scalcout" => Ok(Box::new(scalcout::ScalcoutRecord::default())),
        "transform" => Ok(Box::new(transform::TransformRecord::default())),
        "calcout" => Ok(Box::new(calcout::CalcoutRecord::default())),
        "dfanout" => Ok(Box::new(dfanout::DfanoutRecord::default())),
        "compress" => Ok(Box::new(compress::CompressRecord::default())),
        "histogram" => Ok(Box::new(histogram::HistogramRecord::default())),
        "sel" => Ok(Box::new(sel::SelRecord::default())),
        "sub" => Ok(Box::new(sub_record::SubRecord::default())),
        _ => Err(CaError::DbParseError {
            line: 0,
            column: 0,
            message: format!("unknown record type: '{record_type}'"),
        }),
    }
}

/// Apply fields from a DbRecordDef to a record.
/// Returns the record along with any common field values.
pub fn apply_fields(
    record: &mut Box<dyn Record>,
    fields: &[(String, String)],
    common_fields: &mut Vec<(String, EpicsValue)>,
) -> CaResult<()> {
    for (name, value_str) in fields {
        let upper_name = name.to_uppercase();

        // Try record-specific field first
        let field_desc = record
            .field_list()
            .iter()
            .find(|f| f.name == upper_name.as_str());

        if let Some(desc) = field_desc {
            let value = EpicsValue::parse(desc.dbf_type, value_str)?;
            record.put_field(&upper_name, value)?;
        } else {
            // Store as common field for RecordInstance to handle
            common_fields.push((upper_name, EpicsValue::String(value_str.clone())));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_db() {
        let input = r#"
record(ai, "TEMP") {
    field(DESC, "Temperature")
    field(SCAN, "1 second")
    field(HOPR, "100")
    field(LOPR, "0")
}
"#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, "ai");
        assert_eq!(records[0].name, "TEMP");
        assert_eq!(records[0].fields.len(), 4);
        assert_eq!(records[0].fields[0], ("DESC".into(), "Temperature".into()));
    }

    #[test]
    fn test_macro_substitution() {
        let input = r#"
record(ai, "$(P)TEMP") {
    field(DESC, "$(D=Default Desc)")
}
"#;
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "IOC:".to_string());

        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records[0].name, "IOC:TEMP");
        assert_eq!(records[0].fields[0].1, "Default Desc");
    }

    #[test]
    fn test_multiple_records() {
        let input = r#"
record(ai, "TEMP1") {
    field(VAL, "25.0")
}
record(bo, "SWITCH") {
    field(VAL, "1")
    field(ZNAM, "Off")
    field(ONAM, "On")
}
"#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].record_type, "ai");
        assert_eq!(records[1].record_type, "bo");
    }

    #[test]
    fn test_comments() {
        let input = r#"
# This is a comment
record(ai, "TEMP") {
    # Another comment
    field(VAL, "25.0")
}
"#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn test_unknown_record_type() {
        let result = create_record("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_quoted_string_escape() {
        let input = r#"
record(stringin, "TEST") {
    field(VAL, "hello \"world\"")
}
"#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records[0].fields[0].1, "hello \"world\"");
    }

    #[test]
    fn test_macro_with_quoted_default_in_string() {
        // C EPICS macLib treats quotes inside $(...) as literal characters.
        // e.g. $(XPOS="") means "default to empty-string pair".
        let input = r#"
record(longout, "$(P)$(R)PositionXLink") {
    field(DOL, "$(XPOS="") CP MS")
}
"#;
        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "SIM1:".to_string());
        macros.insert("R".to_string(), "Over1:1:".to_string());
        macros.insert("XPOS".to_string(), "SIM1:ROI1:MinX_RBV".to_string());
        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records[0].fields[0].1, "SIM1:ROI1:MinX_RBV CP MS");
    }

    #[test]
    fn test_macro_with_quoted_default_unset() {
        // When XPOS is not set, $(XPOS="") should expand to "" (literal quotes)
        let input = r#"
record(longout, "TEST:Link") {
    field(DOL, "$(XPOS="") CP MS")
}
"#;
        let macros = HashMap::new();
        let records = parse_db(input, &macros).unwrap();
        // With undefined macro and default="", the field gets the raw default
        assert!(records[0].fields[0].1.contains("CP MS"));
    }

    #[test]
    fn test_recursive_macro_default() {
        // $(TS_PORT=$(PORT)_TS) with PORT=ATTR1 → ATTR1_TS
        let input = r#"
record(stringin, "TEST") {
    field(VAL, "$(TS_PORT=$(PORT)_TS)")
}
"#;
        let mut macros = HashMap::new();
        macros.insert("PORT".to_string(), "ATTR1".to_string());
        let records = parse_db(input, &macros).unwrap();
        assert_eq!(records[0].fields[0].1, "ATTR1_TS");
    }

    #[test]
    fn test_substitute_directive_in_expand() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create a simple child template
        let child = dir.path().join("child.db");
        let mut f = std::fs::File::create(&child).unwrap();
        writeln!(f, r#"record(ai, "$(P)$(R)Val") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "$(ADDR)")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        // Create parent with substitute + include
        let parent = dir.path().join("parent.db");
        let mut f = std::fs::File::create(&parent).unwrap();
        writeln!(f, r#"substitute "R=A:,ADDR=0""#).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();
        writeln!(f, r#"substitute "R=B:,ADDR=1""#).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "IOC:".to_string());
        let config = DbLoadConfig { include_paths: vec![], max_include_depth: 10 };
        let records = parse_db_file(&parent, &macros, &config).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "IOC:A:Val");
        assert_eq!(records[0].fields[0].1, "0");
        assert_eq!(records[1].name, "IOC:B:Val");
        assert_eq!(records[1].fields[0].1, "1");
    }

    #[test]
    fn test_empty_string_numeric_parse() {
        // C EPICS treats empty VAL as 0 for numeric record types
        let input = r#"
record(longin, "TEST:Int") {
    field(VAL, "")
}
"#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        // Should parse without error — empty string → 0
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn test_calcout_process() {
        use crate::server::records::calcout::CalcoutRecord;
        use crate::server::record::Record;

        let mut rec = CalcoutRecord::default();
        rec.put_field("CALC", EpicsValue::String("A+B".into())).unwrap();
        rec.put_field("A", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(4.0)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 7.0).abs() < 1e-10),
            other => panic!("expected Double(7.0), got {:?}", other),
        }
    }

    #[test]
    fn test_calcout_oopt() {
        use crate::server::records::calcout::CalcoutRecord;
        use crate::server::record::Record;

        let mut rec = CalcoutRecord::default();
        rec.put_field("CALC", EpicsValue::String("A".into())).unwrap();
        rec.put_field("OOPT", EpicsValue::Short(1)).unwrap(); // On Change
        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();

        // First process — value changes from 0 to 5
        rec.process().unwrap();
        assert!((rec.oval - 5.0).abs() < 1e-10);

        // Second process — same value, OVAL should not update (but val still computes)
        rec.process().unwrap();
        // OVAL is still 5.0 since val didn't change
    }

    #[test]
    fn test_calcout_dopt() {
        use crate::server::records::calcout::CalcoutRecord;
        use crate::server::record::Record;

        let mut rec = CalcoutRecord::default();
        rec.put_field("CALC", EpicsValue::String("A+B".into())).unwrap();
        rec.put_field("OCAL", EpicsValue::String("A*B".into())).unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use OCAL
        rec.put_field("A", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(4.0)).unwrap();
        rec.process().unwrap();

        // VAL = A+B = 7, OVAL = A*B = 12
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 7.0).abs() < 1e-10),
            other => panic!("expected Double(7.0), got {:?}", other),
        }
        match rec.get_field("OVAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 12.0).abs() < 1e-10),
            other => panic!("expected Double(12.0), got {:?}", other),
        }
    }

    #[test]
    fn test_dfanout_basic() {
        use crate::server::records::dfanout::DfanoutRecord;
        use crate::server::record::Record;

        let mut rec = DfanoutRecord::default();
        rec.put_field("VAL", EpicsValue::Double(42.0)).unwrap();
        assert_eq!(rec.record_type(), "dfanout");
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
    }

    #[test]
    fn test_dfanout_output_links() {
        use crate::server::records::dfanout::DfanoutRecord;
        use crate::server::record::Record;

        let mut rec = DfanoutRecord::default();
        rec.put_field("OUTA", EpicsValue::String("REC_A".into())).unwrap();
        rec.put_field("OUTB", EpicsValue::String("REC_B".into())).unwrap();
        let links = rec.output_links();
        assert_eq!(links.len(), 2);
    }

    #[test]
    fn test_compress_circular_buffer() {
        use crate::server::records::compress::CompressRecord;
        use crate::server::record::Record;

        let mut rec = CompressRecord::new(5, 3); // nsam=5, alg=Circular Buffer
        for i in 0..7 {
            rec.push_value(i as f64);
        }
        // Buffer should have last 5 values wrapped around
        match rec.get_field("VAL") {
            Some(EpicsValue::DoubleArray(arr)) => {
                assert_eq!(arr.len(), 5);
                // offset=7 → indices written: 0,1,2,3,4,5(=0),6(=1)
                assert!((arr[2] - 2.0).abs() < 1e-10); // unchanged
            }
            other => panic!("expected DoubleArray, got {:?}", other),
        }
    }

    #[test]
    fn test_compress_n_to_1_mean() {
        use crate::server::records::compress::CompressRecord;
        use crate::server::record::Record;

        let mut rec = CompressRecord::new(10, 2); // alg=Mean
        rec.put_field("N", EpicsValue::Long(3)).unwrap();
        rec.push_value(3.0);
        rec.push_value(6.0);
        rec.push_value(9.0); // mean = 6.0
        match rec.get_field("VAL") {
            Some(EpicsValue::DoubleArray(arr)) => {
                assert!((arr[0] - 6.0).abs() < 1e-10);
            }
            other => panic!("expected DoubleArray, got {:?}", other),
        }
    }

    #[test]
    fn test_histogram_bucket_count() {
        use crate::server::records::histogram::HistogramRecord;

        let mut rec = HistogramRecord::new(10, 0.0, 10.0);
        rec.add_sample(2.5);  // bucket 2
        rec.add_sample(2.7);  // bucket 2
        rec.add_sample(7.0);  // bucket 7
        assert_eq!(rec.val[2], 2);
        assert_eq!(rec.val[7], 1);
    }

    #[test]
    fn test_histogram_out_of_range() {
        use crate::server::records::histogram::HistogramRecord;

        let mut rec = HistogramRecord::new(10, 0.0, 10.0);
        rec.add_sample(-1.0);  // below range
        rec.add_sample(10.0);  // at upper limit (excluded)
        rec.add_sample(15.0);  // above range
        let total: i32 = rec.val.iter().sum();
        assert_eq!(total, 0);
    }

    #[test]
    fn test_sel_specified() {
        use crate::server::records::sel::SelRecord;
        use crate::server::record::Record;

        let mut rec = SelRecord::default();
        rec.put_field("SELM", EpicsValue::Short(0)).unwrap(); // Specified
        rec.put_field("SELN", EpicsValue::Short(2)).unwrap(); // Select C
        rec.put_field("C", EpicsValue::Double(99.0)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 99.0).abs() < 1e-10),
            other => panic!("expected Double(99.0), got {:?}", other),
        }
    }

    #[test]
    fn test_sel_high_low_median() {
        use crate::server::records::sel::SelRecord;
        use crate::server::record::Record;

        let mut rec = SelRecord::default();
        rec.put_field("A", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(30.0)).unwrap();
        rec.put_field("C", EpicsValue::Double(20.0)).unwrap();

        // High
        rec.put_field("SELM", EpicsValue::Short(1)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 30.0).abs() < 1e-10),
            other => panic!("expected Double(30.0), got {:?}", other),
        }

        // Low
        rec.put_field("SELM", EpicsValue::Short(2)).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!(v <= 0.001), // min of all 12 values (most are 0)
            other => panic!("expected near 0.0, got {:?}", other),
        }
    }

    #[test]
    fn test_sub_record_register_and_call() {
        use crate::server::records::sub_record::SubRecord;
        use crate::server::record::{Record, RecordInstance, SubroutineFn};
        use std::sync::Arc;

        let mut rec = SubRecord::default();
        rec.put_field("SNAM", EpicsValue::String("double_val".into())).unwrap();
        rec.put_field("VAL", EpicsValue::Double(5.0)).unwrap();

        let mut instance = RecordInstance::new("TEST_SUB".into(), rec);
        let sub_fn: SubroutineFn = Box::new(|record: &mut dyn Record| {
            if let Some(EpicsValue::Double(v)) = record.get_field("VAL") {
                record.put_field("VAL", EpicsValue::Double(v * 2.0))?;
            }
            Ok(())
        });
        instance.subroutine = Some(Arc::new(sub_fn));

        instance.process_local().unwrap();

        match instance.record.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!((v - 10.0).abs() < 1e-10),
            other => panic!("expected Double(10.0), got {:?}", other),
        }
    }

    #[test]
    fn test_new_record_types_in_db() {
        let input = r#"
record(calcout, "TEST_CO") {
    field(CALC, "A+1")
}
record(dfanout, "TEST_DF") {
    field(VAL, "5.0")
}
record(compress, "TEST_CMP") {
    field(DESC, "test compress")
}
record(histogram, "TEST_HIST") {
    field(DESC, "test hist")
}
record(sel, "TEST_SEL") {
    field(SELM, "0")
}
record(sub, "TEST_SUB") {
    field(SNAM, "my_sub")
}
"#;
        let records = parse_db(input, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 6);
        // Verify they can all be created
        for def in &records {
            create_record(&def.record_type).unwrap();
        }
    }

    // ===== include / parse_db_file tests =====

    #[test]
    fn test_parse_include_directive() {
        // Normal include
        assert_eq!(
            parse_include_directive(r#"include "foo.template""#),
            Some("foo.template".to_string())
        );
        // With leading whitespace
        assert_eq!(
            parse_include_directive(r#"  include "bar.db""#),
            Some("bar.db".to_string())
        );
        // With trailing comment
        assert_eq!(
            parse_include_directive(r#"include "baz.template" # a comment"#),
            Some("baz.template".to_string())
        );
        // No quote — not an include
        assert_eq!(parse_include_directive("include something"), None);
        // Comment line
        assert_eq!(parse_include_directive(r#"# include "ignored.db""#), None);
        // Not an include keyword
        assert_eq!(parse_include_directive("record(ai, \"X\") {"), None);
        // "includes" is not "include"
        assert_eq!(parse_include_directive(r#"includes "nope.db""#), None);
    }

    #[test]
    fn test_commented_include_ignored() {
        assert_eq!(parse_include_directive(r#"# include "file.db""#), None);
        assert_eq!(parse_include_directive(r#"  # include "file.db""#), None);
    }

    #[test]
    fn test_expand_includes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create child.db
        let child_path = dir.path().join("child.db");
        let mut f = std::fs::File::create(&child_path).unwrap();
        writeln!(f, r#"record(ai, "CHILD") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "1.0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        // Create parent.db that includes child.db
        let parent_path = dir.path().join("parent.db");
        let mut f = std::fs::File::create(&parent_path).unwrap();
        writeln!(f, r#"record(ao, "PARENT") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "2.0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        let config = DbLoadConfig::default();
        let result = expand_includes(&parent_path, &HashMap::new(), &config).unwrap();
        assert!(result.contains(r#"record(ao, "PARENT")"#));
        assert!(result.contains(r#"record(ai, "CHILD")"#));

        // Verify it parses correctly
        let records = parse_db(&result, &HashMap::new()).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn test_circular_include_error() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let a_path = dir.path().join("a.template");
        let b_path = dir.path().join("b.template");

        let mut fa = std::fs::File::create(&a_path).unwrap();
        writeln!(fa, r#"include "b.template""#).unwrap();

        let mut fb = std::fs::File::create(&b_path).unwrap();
        writeln!(fb, r#"include "a.template""#).unwrap();

        let config = DbLoadConfig::default();
        let result = expand_includes(&a_path, &HashMap::new(), &config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("circular include"), "error was: {err}");
    }

    #[test]
    fn test_duplicate_include_allowed() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let shared_path = dir.path().join("shared.db");
        let mut f = std::fs::File::create(&shared_path).unwrap();
        writeln!(f, r#"record(ai, "SHARED") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        // main.db includes shared.db twice (not circular, just duplicate)
        let main_path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main_path).unwrap();
        writeln!(f, r#"include "shared.db""#).unwrap();
        writeln!(f, r#"include "shared.db""#).unwrap();

        let config = DbLoadConfig::default();
        let result = expand_includes(&main_path, &HashMap::new(), &config).unwrap();
        // shared.db content appears twice
        assert_eq!(result.matches(r#"record(ai, "SHARED")"#).count(), 2);
    }

    #[test]
    fn test_include_depth_limit() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        // Create a chain: file0 -> file1 -> file2 -> ... -> file33
        for i in 0..34 {
            let path = dir.path().join(format!("file{i}.db"));
            let mut f = std::fs::File::create(&path).unwrap();
            if i < 33 {
                writeln!(f, r#"include "file{}.db""#, i + 1).unwrap();
            } else {
                writeln!(f, r#"record(ai, "DEEP") {{"#).unwrap();
                writeln!(f, r#"    field(VAL, "0")"#).unwrap();
                writeln!(f, r#"}}"#).unwrap();
            }
        }

        let config = DbLoadConfig {
            include_paths: vec![],
            max_include_depth: 32,
        };
        let result = expand_includes(&dir.path().join("file0.db"), &HashMap::new(), &config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("depth limit"), "error was: {err}");
    }

    #[test]
    fn test_include_not_found_error() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"include "nonexistent.db""#).unwrap();

        let config = DbLoadConfig::default();
        let result = expand_includes(&path, &HashMap::new(), &config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"), "error was: {err}");
    }

    #[test]
    fn test_include_with_macro_filename() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir).unwrap();

        let child_path = subdir.join("child.db");
        let mut f = std::fs::File::create(&child_path).unwrap();
        writeln!(f, r#"record(ai, "CHILD") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let main_path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main_path).unwrap();
        writeln!(f, r#"include "$(DIR)/child.db""#).unwrap();

        let mut macros = HashMap::new();
        macros.insert("DIR".to_string(), subdir.to_string_lossy().to_string());

        let config = DbLoadConfig::default();
        let result = expand_includes(&main_path, &macros, &config).unwrap();
        assert!(result.contains(r#"record(ai, "CHILD")"#));
    }

    #[test]
    fn test_include_search_order() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let inc_dir = dir.path().join("inc");
        std::fs::create_dir(&inc_dir).unwrap();

        // Put file in include path only (not in current dir)
        let child_path = inc_dir.join("child.db");
        let mut f = std::fs::File::create(&child_path).unwrap();
        writeln!(f, r#"record(ai, "FROM_INC") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let main_path = dir.path().join("main.db");
        let mut f = std::fs::File::create(&main_path).unwrap();
        writeln!(f, r#"include "child.db""#).unwrap();

        let config = DbLoadConfig {
            include_paths: vec![inc_dir.clone()],
            max_include_depth: 32,
        };
        let result = expand_includes(&main_path, &HashMap::new(), &config).unwrap();
        assert!(result.contains(r#"record(ai, "FROM_INC")"#));

        // Now also put a file in current dir — it should take priority
        let local_child = dir.path().join("child.db");
        let mut f = std::fs::File::create(&local_child).unwrap();
        writeln!(f, r#"record(ai, "FROM_LOCAL") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let result = expand_includes(&main_path, &HashMap::new(), &config).unwrap();
        assert!(result.contains(r#"record(ai, "FROM_LOCAL")"#));
    }

    #[test]
    fn test_dtyp_override_existing_only() {
        let mut records = vec![
            DbRecordDef {
                record_type: "ai".to_string(),
                name: "REC_WITH_DTYP".to_string(),
                fields: vec![
                    ("DTYP".to_string(), "oldDtyp".to_string()),
                    ("VAL".to_string(), "0".to_string()),
                ],
            },
            DbRecordDef {
                record_type: "ao".to_string(),
                name: "REC_WITHOUT_DTYP".to_string(),
                fields: vec![("VAL".to_string(), "1".to_string())],
            },
        ];

        override_dtyp(&mut records, "newDtyp");

        // Record with DTYP: value replaced
        assert_eq!(records[0].fields[0], ("DTYP".to_string(), "newDtyp".to_string()));
        // Record without DTYP: unchanged (no DTYP added)
        assert_eq!(records[1].fields.len(), 1);
        assert!(!records[1].fields.iter().any(|(n, _)| n == "DTYP"));
    }

    #[test]
    fn test_parse_db_file_no_includes() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();

        let path = dir.path().join("simple.db");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"record(ai, "$(P)TEMP") {{"#).unwrap();
        writeln!(f, r#"    field(VAL, "25.0")"#).unwrap();
        writeln!(f, r#"}}"#).unwrap();

        let mut macros = HashMap::new();
        macros.insert("P".to_string(), "IOC:".to_string());

        let config = DbLoadConfig::default();
        let records = parse_db_file(&path, &macros, &config).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "IOC:TEMP");
    }
}
