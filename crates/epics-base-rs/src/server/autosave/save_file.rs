use std::path::Path;

use crate::types::EpicsValue;
use chrono::Local;

use super::error::{AutosaveError, AutosaveResult};
use super::format::{ARRAY_MARKER, CompatMode, END_MARKER, VERSION};

/// A single PV entry in a .sav file.
#[derive(Debug, Clone)]
pub struct SaveEntry {
    pub pv_name: String,
    pub value: String,
    pub connected: bool,
}

/// A line of a `.sav` file that declares no entry this grammar
/// understands — a truncated write, a hand edit, a line whose
/// `@array@` braces do not close.
///
/// Kept rather than dropped: a restore that silently skips such a line
/// reports the PVs it did write and nothing else, so a partially
/// written file looks like a clean restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedLine {
    /// 1-based line number within the file.
    pub line_no: usize,
    /// The line as it appeared, minus its terminator.
    pub text: String,
}

/// Everything a `.sav` file body says: the entries it declares, and the
/// lines it could not turn into one. Every reader gets both halves —
/// there is no entries-only parse that can lose the second.
#[derive(Debug, Clone, Default)]
pub struct SaveFileContents {
    pub entries: Vec<SaveEntry>,
    pub malformed: Vec<MalformedLine>,
}

/// Write a .sav file atomically (tmp -> fsync -> rename).
///
/// Uses the autosave-rs native format ([`CompatMode::Native`]). For a
/// file a C IOC must be able to read, use [`write_save_file_with_mode`]
/// with [`CompatMode::CRead`].
pub async fn write_save_file(path: &Path, entries: &[SaveEntry]) -> AutosaveResult<()> {
    write_save_file_with_mode(path, entries, CompatMode::Native).await
}

/// Write a .sav file atomically (tmp -> fsync -> rename) in the given
/// [`CompatMode`].
///
/// In [`CompatMode::CRead`] the header banner is `save/restore` (the
/// banner a C IOC's `restore.c` / `asVerify` expects) instead of the
/// autosave-rs banner. The per-PV line format (`PVNAME value`,
/// arrays as `PVNAME @array@ { ... }`) is shared by both modes — the
/// `SaveEntry.value` text is already mode-encoded by the caller via
/// [`value_to_save_str`] / [`value_to_save_str_c`]. A C IOC can read
/// a `CRead`-written file because the line grammar and `<END>` marker
/// match C autosave's `dbrestore.c` parser.
pub async fn write_save_file_with_mode(
    path: &Path,
    entries: &[SaveEntry],
    mode: CompatMode,
) -> AutosaveResult<()> {
    let mut content = String::new();

    // Header. C autosave writes `# <banner>\t<datetime>` where the
    // banner begins with `save/restore` — restore.c only checks for a
    // leading `#` comment and skips it, but asVerify and operators
    // expect the canonical banner.
    let now = Local::now();
    let banner = match mode {
        CompatMode::Native => VERSION,
        CompatMode::CRead => "save/restore V1.7",
    };
    content.push_str(&format!(
        "# {}\t{}\n",
        banner,
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

    // Atomic write: open RDWR → write → fsync the SAME fd → rename
    // → fsync parent dir. The previous sequence reopened RDONLY
    // before fsync; POSIX is silent on whether sync_all on a RDONLY
    // fd flushes data, and on some FS (older NFS, FUSE) it's a
    // no-op — silently dropping the write across a power loss.
    //
    // The whole sequence runs in ONE `runtime::fs::blocking` closure rather
    // than four awaits: the ordering above is the durability guarantee, and
    // keeping it in one closure keeps it readable as a sequence and costs one
    // hop through the blocking pool instead of four. It also has to leave
    // `tokio::fs` behind — that is a blocking call dressed as an async one and
    // it panics on any thread that is not a tokio runtime thread, which is
    // every callback thread under `rtems-exec-model`.
    let tmp_path = path.with_extension("tmp");
    let final_path = path.to_path_buf();
    let parent = path.parent().map(|p| p.to_path_buf());
    crate::runtime::fs::blocking(move || {
        use std::io::Write as _;
        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp_path)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp_path, &final_path)?;
        // Sync parent directory to make the rename durable across power loss
        if let Some(parent) = parent
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    })
    .await?;

    Ok(())
}

/// Read a .sav file and validate `<END>` marker.
/// Returns None for corrupt files (no END marker).
pub async fn read_save_file(path: &Path) -> AutosaveResult<Option<SaveFileContents>> {
    let content = crate::runtime::fs::read_to_string(path)
        .await
        .map_err(|e| {
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

    Ok(Some(parse_save_content(&content)))
}

/// Quick check if a .sav file has a valid `<END>` marker.
pub async fn validate_save_file(path: &Path) -> AutosaveResult<bool> {
    let content = crate::runtime::fs::read_to_string(path).await?;
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

fn parse_save_content(content: &str) -> SaveFileContents {
    let mut out = SaveFileContents::default();

    for (idx, raw) in content.lines().enumerate() {
        // Leading indentation is framing; a TRAILING space is not. The
        // writer emits `PVNAME<space>VALUE`, so the value of a PV whose
        // value is the empty string is the empty tail after that space —
        // trimming the line first is what used to erase the separator and
        // drop the entry.
        let line = raw.trim_start();
        let framing = line.trim_end();
        if framing.is_empty() {
            continue;
        }
        if framing == END_MARKER {
            break;
        }
        let malformed = |out: &mut SaveFileContents| {
            out.malformed.push(MalformedLine {
                line_no: idx + 1,
                text: raw.to_string(),
            });
        };

        // Header/comment lines
        if framing.starts_with('#') {
            // Check for disconnected PV: #PVNAME\t(not connected)
            let inner = &framing[1..];
            if inner.contains("(not connected)") {
                let pv_name = inner.split(['\t', ' ']).next().unwrap_or("").trim();
                if !pv_name.is_empty() {
                    out.entries.push(SaveEntry {
                        pv_name: pv_name.to_string(),
                        value: String::new(),
                        connected: false,
                    });
                }
            }
            continue;
        }

        // C autosave @array@ format. The marker settles what the line is,
        // so a line carrying it either parses as an array or is malformed —
        // falling through to the scalar rule would turn `PV @array@ { 1 2`
        // into a PV whose value is the literal text `@array@ { 1 2`.
        if framing.contains(ARRAY_MARKER) {
            match parse_c_array_line(framing) {
                Some(entry) => out.entries.push(entry),
                None => malformed(&mut out),
            }
            continue;
        }

        // Normal line: PV_NAME<space>VALUE
        match line.split_once(' ') {
            Some((pv_name, value)) if !pv_name.is_empty() => out.entries.push(SaveEntry {
                pv_name: pv_name.to_string(),
                value: value.to_string(),
                connected: true,
            }),
            _ => malformed(&mut out),
        }
    }

    out
}

/// The native array text `[e1,e2,e3]` — the one owner of that form, in
/// both directions.
///
/// An element is written verbatim unless it would be ambiguous against
/// the punctuation of the form itself — it contains `,`, `]`, `"` or
/// `\`, has surrounding whitespace, or is empty — in which case it is
/// double-quoted with `"` and `\` backslash-escaped. Numbers never
/// qualify, so numeric arrays keep the text they always had.
/// [`decode_array_text`] undoes exactly this, so any element survives
/// the round trip. Nothing else in this module may join or split array
/// text: an element carrying a separator is precisely the case an
/// ad-hoc `join(",")` / `split(',')` pair loses.
fn encode_array_text<I, S>(elements: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = String::from("[");
    for (i, elem) in elements.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let elem = elem.as_ref();
        let ambiguous = elem.is_empty()
            || elem.contains([',', ']', '"', '\\'])
            || elem.starts_with(char::is_whitespace)
            || elem.ends_with(char::is_whitespace);
        if ambiguous {
            out.push('"');
            for c in elem.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        } else {
            out.push_str(elem);
        }
    }
    out.push(']');
    out
}

/// Read back the text [`encode_array_text`] writes, unquoting and
/// unescaping the elements that were quoted. Unquoted elements keep the
/// old tolerance for surrounding whitespace, so hand-written `[1, 2]`
/// still reads as two elements.
fn decode_array_text(s: &str) -> Vec<String> {
    let inner = s.strip_prefix('[').unwrap_or(s);
    // `strip_suffix`, not `trim_end_matches`: only the form's own
    // closing bracket comes off, so `]` inside a quoted element cannot
    // be eaten from the end of the text.
    let inner = inner.strip_suffix(']').unwrap_or(inner);

    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            None if out.is_empty() => break,
            // A trailing separator still declares a final empty element.
            None => {
                out.push(String::new());
                break;
            }
            Some('"') => {
                chars.next();
                let mut elem = String::new();
                loop {
                    match chars.next() {
                        Some('\\') => {
                            if let Some(c) = chars.next() {
                                elem.push(c);
                            }
                        }
                        Some('"') | None => break,
                        Some(c) => elem.push(c),
                    }
                }
                out.push(elem);
                // Skip to the separator that follows the closing quote.
                while chars.peek().is_some_and(|&c| c != ',') {
                    chars.next();
                }
            }
            Some(_) => {
                let mut elem = String::new();
                while chars.peek().is_some_and(|&c| c != ',') {
                    elem.push(chars.next().unwrap());
                }
                out.push(elem.trim_end().to_string());
            }
        }
        match chars.next() {
            Some(',') => continue,
            _ => break,
        }
    }
    out
}

/// Parse a C autosave @array@ line.
fn parse_c_array_line(line: &str) -> Option<SaveEntry> {
    // Format: PV_NAME @array@ { "e1" "e2" "e3" }
    let marker_pos = line.find(ARRAY_MARKER)?;
    let pv_name = line[..marker_pos].trim();
    let rest = line[marker_pos + ARRAY_MARKER.len()..].trim();

    if !rest.starts_with('{') || !rest.ends_with('}') {
        return None;
    }

    let inner = rest[1..rest.len() - 1].trim();
    let elements = parse_c_array_elements(inner);
    let value = encode_array_text(&elements);

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
        EpicsValue::String(s) => format!(
            "\"{}\"",
            s.as_str_lossy().replace('\\', "\\\\").replace('"', "\\\"")
        ),
        EpicsValue::Double(v) => format!("{:.14e}", v),
        EpicsValue::Float(v) => format!("{:.7e}", v),
        EpicsValue::Short(v) => v.to_string(),
        EpicsValue::Long(v) => v.to_string(),
        EpicsValue::Int64(v) => v.to_string(),
        EpicsValue::Enum(v) => v.to_string(),
        // NTEnum carrier never reaches an autosave write (it is consumed
        // at the link-write boundary), but serialize the index for
        // exhaustiveness, identical to `Enum`.
        EpicsValue::EnumWithChoices { index, .. } => index.to_string(),
        EpicsValue::Char(v) => v.to_string(),
        EpicsValue::DoubleArray(arr) => {
            encode_array_text(arr.iter().map(|v| format!("{:.14e}", v)))
        }
        EpicsValue::LongArray(arr) => encode_array_text(arr.iter().map(|v| v.to_string())),
        EpicsValue::CharArray(arr) => encode_array_text(arr.iter().map(|v| v.to_string())),
        EpicsValue::ShortArray(arr) => encode_array_text(arr.iter().map(|v| v.to_string())),
        EpicsValue::FloatArray(arr) => encode_array_text(arr.iter().map(|v| format!("{:.7e}", v))),
        EpicsValue::EnumArray(arr) => encode_array_text(arr.iter().map(|v| v.to_string())),
        EpicsValue::Int64Array(arr) => encode_array_text(arr.iter().map(|v| v.to_string())),
        EpicsValue::UInt64(v) => v.to_string(),
        EpicsValue::UInt64Array(arr) => encode_array_text(arr.iter().map(|v| v.to_string())),
        EpicsValue::UShort(v) => v.to_string(),
        EpicsValue::ULong(v) => v.to_string(),
        EpicsValue::UShortArray(arr) => encode_array_text(arr.iter().map(|v| v.to_string())),
        EpicsValue::ULongArray(arr) => encode_array_text(arr.iter().map(|v| v.to_string())),
        EpicsValue::UChar(v) => v.to_string(),
        EpicsValue::UCharArray(arr) => encode_array_text(arr.iter().map(|v| v.to_string())),
        EpicsValue::StringArray(arr) => {
            encode_array_text(arr.iter().map(|s| s.as_str_lossy().into_owned()))
        }
    }
}

/// Convert an `EpicsValue` to a **C-autosave wire-compatible** save
/// string, so a C IOC (or `asVerify` in a C IOC) can read the file.
///
/// Differences from [`value_to_save_str`] (autosave-rs native):
///
/// * Scalar strings are written **unquoted** — C autosave's
///   `dbrestore.c` treats everything after the first space on a line
///   as the literal value; it does not strip quotes from a scalar.
/// * Scalar numbers are plain (same as native).
/// * Arrays use C's `@array@ { "v" "v" ... }` form (the form the
///   native reader already accepts via `parse_c_array_line`), with
///   every element double-quoted and `"`/`\` escaped — instead of the
///   native `[v,v,v]` form a C IOC cannot parse.
pub fn value_to_save_str_c(value: &EpicsValue) -> String {
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }
    fn c_array<T, I>(iter: I) -> String
    where
        I: IntoIterator<Item = T>,
        T: ToString,
    {
        let parts: Vec<String> = iter
            .into_iter()
            .map(|v| format!("\"{}\"", esc(&v.to_string())))
            .collect();
        format!("{ARRAY_MARKER} {{ {} }}", parts.join(" "))
    }
    match value {
        // Scalars: plain printf form, strings unquoted.
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        EpicsValue::Double(v) => format!("{:.14e}", v),
        EpicsValue::Float(v) => format!("{:.7e}", v),
        EpicsValue::Short(v) => v.to_string(),
        EpicsValue::Long(v) => v.to_string(),
        EpicsValue::Int64(v) => v.to_string(),
        EpicsValue::Enum(v) => v.to_string(),
        // NTEnum carrier never reaches an autosave write (it is consumed
        // at the link-write boundary), but serialize the index for
        // exhaustiveness, identical to `Enum`.
        EpicsValue::EnumWithChoices { index, .. } => index.to_string(),
        EpicsValue::Char(v) => v.to_string(),
        // Arrays: C `@array@ { "v" "v" }` form.
        EpicsValue::DoubleArray(arr) => c_array(arr.iter().map(|v| format!("{:.14e}", v))),
        EpicsValue::FloatArray(arr) => c_array(arr.iter().map(|v| format!("{:.7e}", v))),
        EpicsValue::LongArray(arr) => c_array(arr.iter()),
        EpicsValue::CharArray(arr) => c_array(arr.iter()),
        EpicsValue::ShortArray(arr) => c_array(arr.iter()),
        EpicsValue::EnumArray(arr) => c_array(arr.iter()),
        EpicsValue::Int64Array(arr) => c_array(arr.iter()),
        EpicsValue::UInt64(v) => v.to_string(),
        EpicsValue::UInt64Array(arr) => c_array(arr.iter()),
        EpicsValue::UShort(v) => v.to_string(),
        EpicsValue::ULong(v) => v.to_string(),
        EpicsValue::UShortArray(arr) => c_array(arr.iter()),
        EpicsValue::ULongArray(arr) => c_array(arr.iter()),
        EpicsValue::UChar(v) => v.to_string(),
        EpicsValue::UCharArray(arr) => c_array(arr.iter()),
        EpicsValue::StringArray(arr) => c_array(arr.iter().cloned()),
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
                Some(EpicsValue::String(unescaped.into()))
            } else {
                Some(EpicsValue::String(s.to_string().into()))
            }
        }
        EpicsValue::Double(_) => s.parse::<f64>().ok().map(EpicsValue::Double),
        EpicsValue::Float(_) => s.parse::<f32>().ok().map(EpicsValue::Float),
        EpicsValue::Long(_) => s.parse::<i32>().ok().map(EpicsValue::Long),
        EpicsValue::Int64(_) => s.parse::<i64>().ok().map(EpicsValue::Int64),
        EpicsValue::UInt64(_) => s.parse::<u64>().ok().map(EpicsValue::UInt64),
        EpicsValue::Short(_) => s.parse::<i16>().ok().map(EpicsValue::Short),
        EpicsValue::Enum(_) | EpicsValue::EnumWithChoices { .. } => {
            s.parse::<u16>().ok().map(EpicsValue::Enum)
        }
        EpicsValue::Char(_) => s.parse::<u8>().ok().map(EpicsValue::Char),
        EpicsValue::UChar(_) => s.parse::<u8>().ok().map(EpicsValue::UChar),
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
        EpicsValue::Int64Array(_) => {
            parse_array_str(s, |v| v.parse::<i64>().ok()).map(EpicsValue::Int64Array)
        }
        EpicsValue::UInt64Array(_) => {
            parse_array_str(s, |v| v.parse::<u64>().ok()).map(EpicsValue::UInt64Array)
        }
        EpicsValue::UShort(_) => s.parse::<u16>().ok().map(EpicsValue::UShort),
        EpicsValue::ULong(_) => s.parse::<u32>().ok().map(EpicsValue::ULong),
        EpicsValue::UShortArray(_) => {
            parse_array_str(s, |v| v.parse::<u16>().ok()).map(EpicsValue::UShortArray)
        }
        EpicsValue::ULongArray(_) => {
            parse_array_str(s, |v| v.parse::<u32>().ok()).map(EpicsValue::ULongArray)
        }
        EpicsValue::UCharArray(_) => {
            parse_array_str(s, |v| v.parse::<u8>().ok()).map(EpicsValue::UCharArray)
        }
        EpicsValue::StringArray(_) => Some(EpicsValue::StringArray(
            decode_array_text(s).into_iter().map(Into::into).collect(),
        )),
    }
}

fn parse_array_str<T, F>(s: &str, parse_elem: F) -> Option<Vec<T>>
where
    F: Fn(&str) -> Option<T>,
{
    decode_array_text(s).iter().map(|v| parse_elem(v)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::autosave::format::CompatMode;
    use crate::types::EpicsValue;

    /// M6: a C-format scalar string is written UNQUOTED (C autosave
    /// treats everything after the first space as the literal value).
    #[test]
    fn c_format_scalar_string_unquoted() {
        let v = EpicsValue::String("hello world".to_string().into());
        assert_eq!(value_to_save_str_c(&v), "hello world");
        // Native quotes it.
        assert_eq!(value_to_save_str(&v), "\"hello world\"");
    }

    /// M6: a C-format array uses the `@array@ { "v" "v" }` form a C
    /// IOC can parse — not the native `[v,v,v]` form.
    #[test]
    fn c_format_array_uses_at_array_form() {
        let v = EpicsValue::LongArray(vec![1, 2, 3]);
        assert_eq!(value_to_save_str_c(&v), "@array@ { \"1\" \"2\" \"3\" }");
        assert_eq!(value_to_save_str(&v), "[1,2,3]");
    }

    /// M6: a `.sav` written in `CompatMode::CRead` carries the
    /// `save/restore` banner (the banner a C IOC expects) and is
    /// still readable by the Rust reader — the array form
    /// round-trips through `parse_c_array_line`.
    #[epics_macros_rs::epics_test]
    async fn c_compat_save_file_has_c_banner_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sav");

        let entries = vec![
            SaveEntry {
                pv_name: "PV:SCALAR".to_string(),
                value: value_to_save_str_c(&EpicsValue::Long(42)),
                connected: true,
            },
            SaveEntry {
                pv_name: "PV:ARRAY".to_string(),
                value: value_to_save_str_c(&EpicsValue::LongArray(vec![10, 20])),
                connected: true,
            },
        ];
        write_save_file_with_mode(&path, &entries, CompatMode::CRead)
            .await
            .unwrap();

        let content = crate::runtime::fs::read_to_string(&path).await.unwrap();
        assert!(
            content.starts_with("# save/restore"),
            "C-compat file must carry the save/restore banner, got: {content}"
        );
        assert!(content.contains("PV:ARRAY @array@ { \"10\" \"20\" }"));

        // Reader accepts the C-format file.
        let read = read_save_file(&path)
            .await
            .unwrap()
            .expect("valid file")
            .entries;
        assert_eq!(read.len(), 2);
        let arr = read.iter().find(|e| e.pv_name == "PV:ARRAY").unwrap();
        assert_eq!(arr.value, "[10,20]");
        let parsed = parse_save_value(&arr.value, &EpicsValue::LongArray(vec![])).unwrap();
        assert_eq!(parsed, EpicsValue::LongArray(vec![10, 20]));
    }

    /// M6: native mode still writes the autosave-rs banner.
    #[epics_macros_rs::epics_test]
    async fn native_save_file_keeps_native_banner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native.sav");
        write_save_file(
            &path,
            &[SaveEntry {
                pv_name: "PV1".to_string(),
                value: "1".to_string(),
                connected: true,
            }],
        )
        .await
        .unwrap();
        let content = crate::runtime::fs::read_to_string(&path).await.unwrap();
        assert!(content.starts_with("# autosave-rs"));
    }
}
