use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::runtime::log::errlog_vprintf;
use crate::types::EpicsValue;
use chrono::Local;

use super::error::{AutosaveError, AutosaveResult};
use super::format::{ARRAY_MARKER, END_MARKER};
use super::settings::SrSettings;
use epics_libcom_rs::cfmt::CFloat;
use epics_libcom_rs::runtime::epics_string::{print_escaped, raw_from_escaped_bytes};

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
    /// How many PVs the file itself declares it could not save, from its
    /// `!` line; 0 when it has none.
    ///
    /// C writes `! <n> channel(s) not connected - or not all gets were
    /// successful` under the header whenever a set saved with unreachable
    /// members (`save_restore.c` `write_it`), and its restore reads that
    /// line back — `dbrestore.c:994-1010` logs it, sets the set's status
    /// to WARN, and with `save_restoreIncompleteSetsOk` cleared refuses
    /// the restore outright. A file that omits the line tells a C IOC the
    /// save set was complete, so this is not decoration.
    pub not_connected: usize,
    /// How many entries precede the `!` line, where C's restore reads it
    /// and with `save_restoreIncompleteSetsOk` cleared stops.
    pub incomplete_at: Option<usize>,
}

/// Write a .sav file atomically (tmp -> fsync -> rename).
pub async fn write_save_file(path: &Path, entries: &[SaveEntry]) -> AutosaveResult<()> {
    write_save_file_with_settings(path, entries, &SrSettings::default()).await
}

/// C's `AUTOSAVE_RELEASE`: the release, then Perl's `localtime` of the
/// build.
static SR_VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let epoch: i64 = env!("EPICS_RS_BUILD_EPOCH").parse().unwrap_or(0);
    let compiled = chrono::DateTime::from_timestamp(epoch, 0)
        .unwrap_or_default()
        .with_timezone(&Local);
    format!(
        "Autosave release 6.1, compiled {}",
        compiled.format("%a %b %e %H:%M:%S %Y")
    )
});

/// Write a .sav file atomically (tmp -> fsync -> rename) in the file
/// format C `write_it` writes, so a C IOC restores it. Each
/// `SaveEntry.value` is already [`value_to_save_str`] text.
pub async fn write_save_file_with_settings(
    path: &Path,
    entries: &[SaveEntry],
    settings: &SrSettings,
) -> AutosaveResult<()> {
    let mut content = String::new();

    // C `write_it`'s header (save_restore.c:1803), whose SRversion is
    // AUTOSAVE_RELEASE (configure/CONFIG:31, autosave_release.pl:7). It
    // carries no `R` or `V`, so `checkFile` skips the `<END>` check for
    // these files exactly as for C's (dbrestore.c:1162-1180).
    content.push_str(&format!(
        "# {}\tAutomatically generated - DO NOT MODIFY - {}\n",
        *SR_VERSION,
        Local::now().format("%y%m%d-%H%M%S")
    ));

    // C's incomplete-set declaration, in C's exact wording because a C
    // IOC parses it: `dbrestore.c` takes the count from the text after
    // the `!` and refuses the whole restore when the site has cleared
    // `save_restoreIncompleteSetsOk`.
    let not_connected = entries.iter().filter(|e| !e.connected).count();
    if not_connected > 0 {
        content.push_str(&format!(
            "! {not_connected} channel(s) not connected - or not all gets were successful\n"
        ));
    }

    for entry in entries {
        if entry.connected {
            content.push_str(&entry.pv_name);
            content.push(' ');
            content.push_str(&entry.value);
            content.push('\n');
        } else {
            // C `write_it`'s `#name value` for a channel that never
            // connected (save_restore.c:1296, :1835), which `asVerify` counts.
            content.push_str(&format!("#{} Search Issued\n", entry.pv_name));
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
    // every callback thread under `exec_backend`.
    //
    // Each failure prints what C `write_it` prints for the step it stands
    // for (save_restore.c:1773-1981). C writes the file in place, so an
    // unopenable temp file and a refused rename are both its failed `open`.
    let tmp_path = path.with_extension("tmp");
    let final_path = path.to_path_buf();
    let parent = path.parent().map(|p| p.to_path_buf());
    let permissions = settings.file_permissions.load(Ordering::Relaxed);
    let must_set = settings.must_set_permissions.load(Ordering::Relaxed) != 0;
    let open_failed = Arc::new(AtomicBool::new(false));
    let flag = open_failed.clone();
    let written = crate::runtime::fs::blocking(move || {
        use std::io::Write as _;
        let datetime = super::backup::date_str();
        let unopened = |e: &std::io::Error| {
            flag.store(true, Ordering::Relaxed);
            println!(
                "save_restore:write_it - unable to open file '{}' [{datetime}]",
                final_path.display()
            );
            my_print_errno("write_it", "../save_restore.c", 1774, e);
        };
        {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).truncate(true).write(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, permissions);
            let mut file = options.open(&tmp_path).inspect_err(unopened)?;
            if must_set {
                set_permissions(&file, permissions, &final_path);
            } else {
                keep_permissions(&file, &final_path);
            }
            file.write_all(content.as_bytes()).inspect_err(|e| {
                // A short write surfaces at C's `fclose`.
                errlog_vprintf(format_args!(
                    "../save_restore.c:write_it fclose returned -1 [{datetime}]\n"
                ));
                my_print_errno("write_it", "../save_restore.c", 1935, e);
                errlog_vprintf(format_args!(
                    "../save_restore.c:write_it Giving up on this attempt to write '{}'. [{}]\n",
                    final_path.file_name().unwrap_or_default().display(),
                    super::backup::date_str()
                ));
            })?;
            file.sync_all().inspect_err(|e| {
                errlog_vprintf(format_args!(
                    "../save_restore.c:write_it fsync returned -1 [{datetime}]\n"
                ));
                my_print_errno("write_it", "../save_restore.c", 1925, e);
            })?;
        }
        std::fs::rename(&tmp_path, &final_path).inspect_err(unopened)?;
        // Sync parent directory to make the rename durable across power loss
        if let Some(parent) = parent
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    })
    .await;
    // `save_restoreIoErrors` (save_restore.c:1962, :1987).
    match &written {
        Ok(()) => settings.io_errors.store(0, Ordering::Relaxed),
        Err(_) if open_failed.load(Ordering::Relaxed) => {
            settings.io_errors.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {}
    }
    written?;
    Ok(())
}

/// C `write_it`'s `fchmod` after `save_restoreSet_FilePermissions`
/// (save_restore.c:1777-1786), which does not stop the write.
fn set_permissions(file: &std::fs::File, permissions: u32, path: &Path) {
    #[cfg(unix)]
    if let Err(e) = file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(permissions))
    {
        println!(
            "write_it - when changing {} file permission:",
            path.display()
        );
        print_chmod_error(e.raw_os_error().unwrap_or(0));
    }
    #[cfg(not(unix))]
    let _ = (file, permissions, path);
}

/// The mode C's in-place `open(O_TRUNC)` leaves on a file that is already
/// there, which a temp file renamed over it would otherwise replace.
pub(super) fn keep_permissions(file: &std::fs::File, path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = file.set_permissions(meta.permissions());
    }
}

/// C `print_chmod_error` (save_restore.c:1696-1740).
#[cfg(unix)]
fn print_chmod_error(err: i32) {
    let (short, long) = match err {
        libc::EBADF => (
            "EBADF: Descriptor is not valid.".to_owned(),
            "A file descriptor argument was out of range, referred to a file that was not open, or a read or write request was made to a file that is not open for that operation.",
        ),
        libc::EPERM => (
            "EPERM: The operation is not permitted.".to_owned(),
            "You must have appropriate privileges or be the owner of the object or other resource to do the requested operation.",
        ),
        libc::EROFS => (
            "EROFS: Read-only file system.".to_owned(),
            "You have attempted an update operation in a file system that only supports read operations.",
        ),
        libc::EINTR => (
            "EINTR: Interrupted function call.".to_owned(),
            "The function was interrupted by a signal.",
        ),
        libc::EINVAL => (
            "EINVAL: The value specified for the argument is not correct.".to_owned(),
            "A function was passed incorrect argument values, or an operation was attempted on an object and the operation specified is not supported for that type of object.",
        ),
        n => (format!("Unknown errno {n}"), ""),
    };
    println!("Error {err} - {short}\n{long}");
}

/// C `myPrintErrno` (dbrestore.c:114-117), called at `file`:`line`; C calls
/// it only when `errno` is set.
pub(super) fn my_print_errno(s: &str, file: &str, line: u32, e: &std::io::Error) {
    if let Some(errno) = e.raw_os_error() {
        errlog_vprintf(format_args!(
            "{file}({line}): [0x{errno:x}]={s}:{}\n",
            crate::server::iocsh::c_strerror(e)
        ));
    }
}

/// Read a .sav file and validate `<END>` marker.
/// Returns None for corrupt files (no END marker).
pub async fn read_save_file(path: &Path) -> AutosaveResult<Option<SaveFileContents>> {
    let read = read_partial_save_file(path).await?;
    Ok(read.complete.then_some(read.contents))
}

/// A `.sav` body and whether the file that carried it was complete.
#[derive(Debug, Clone, Default)]
pub struct PartialSaveFile {
    pub contents: SaveFileContents,
    /// `false` when the file does not end in `<END>`, i.e. it was
    /// truncated or is still being written.
    pub complete: bool,
}

/// Parse a `.sav` body whether or not the file is complete, and say
/// which it was.
///
/// [`read_save_file`] is this with the incomplete case turned into
/// `None`, which is what restore needs: a truncated file must not be
/// pushed into the database as if it were the whole save set.
pub async fn read_partial_save_file(path: &Path) -> AutosaveResult<PartialSaveFile> {
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

    Ok(PartialSaveFile {
        complete: has_end_marker(&content),
        contents: parse_save_content(&content),
    })
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

pub(super) fn parse_save_content(content: &str) -> SaveFileContents {
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

        // The file's own count of what it could not save. C emits it as
        // `! <n> channel(s) not connected - ...` and reads the number
        // back with `atol` on the text after the `!`, so a leading `!`
        // never names a PV — parsing it as one produced an entry called
        // `!` that the restore then looked for in the database.
        if let Some(rest) = framing.strip_prefix('!') {
            let digits: String = rest
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            out.not_connected = digits.parse().unwrap_or(0);
            out.incomplete_at.get_or_insert(out.entries.len());
            continue;
        }

        // Header/comment lines
        if framing.starts_with('#') {
            // A PV that had no value to save. `Search Issued` / `Search
            // Failed` are what C's `write_it` leaves in `pchannel->value`
            // for a channel that never connected, and C's own `asVerify`
            // counts the first of them; `(not connected)` is what an older
            // autosave-rs wrote. Matching them keeps the unsaved PVs
            // visible here instead of passing as ordinary comments.
            let inner = &framing[1..];
            if inner.contains("(not connected)")
                || inner.contains("Search Issued")
                || inner.contains("Search Failed")
            {
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
pub(super) fn decode_array_text(s: &str) -> Vec<String> {
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
    Some(SaveEntry {
        pv_name: line[..marker_pos].trim().to_string(),
        value: c_array_value(&line[marker_pos..])?,
        connected: true,
    })
}

/// `@array@ { "e1" "e2" }` as the `[..]` text the parse holds.
fn c_array_value(text: &str) -> Option<String> {
    let rest = text.strip_prefix(ARRAY_MARKER)?.trim();
    let inner = rest.strip_prefix('{')?.strip_suffix('}')?.trim();
    Some(encode_array_text(parse_c_array_elements(inner)))
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
            // Quoted string element, decoded once with C's
            // `epicsStrnRawFromEscaped` (dbrestore.c:623). C first takes the
            // `\` off `\"` and `\\` (:583-586) and so decodes `\\q` as `q`;
            // that double decode is a C defect, not kept.
            chars.next(); // skip opening quote
            let mut elem = String::new();
            loop {
                match chars.next() {
                    Some('\\') => {
                        elem.push('\\');
                        if let Some(c) = chars.next() {
                            elem.push(c);
                        }
                    }
                    Some('"') => break,
                    Some(c) => elem.push(c),
                    None => break,
                }
            }
            let raw = raw_from_escaped_bytes(elem.as_bytes());
            elements.push(String::from_utf8_lossy(&raw).into_owned());
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

/// C's DOUBLE_FMT (save_restore.h:45).
fn double_text(v: f64) -> String {
    format!("{:.14}", CFloat(v).g())
}

/// C's FLOAT_FMT (save_restore.h:44).
fn float_text(v: f32) -> String {
    format!("{:.7}", CFloat(f64::from(v)).g())
}

/// An `EpicsValue` as its `.sav` text, so a C IOC can read the file.
///
/// * Scalar strings are written unquoted and escaped — C autosave's
///   `dbrestore.c` takes everything after the first space on a line as
///   the value and decodes its escapes.
/// * Arrays use C's `@array@ { "v" "v" ... }` form, with every element
///   double-quoted and `"`/`\` escaped.
pub fn value_to_save_str(value: &EpicsValue) -> String {
    // C `SR_write_array_data`'s DBF_STRING element (dbrestore.c:1391-1405).
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
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
        // Scalars: plain printf form, a string through C `write_it`'s
        // `epicsStrPrintEscaped` (save_restore.c:1861).
        EpicsValue::String(s) => print_escaped(s.as_bytes()),
        EpicsValue::Double(v) => double_text(*v),
        EpicsValue::Float(v) => float_text(*v),
        EpicsValue::Short(v) => v.to_string(),
        EpicsValue::Long(v) => v.to_string(),
        EpicsValue::Int64(v) => v.to_string(),
        EpicsValue::Enum(v) => v.to_string(),
        // NTEnum carrier never reaches an autosave write (it is consumed
        // at the link-write boundary), but serialize the index for
        // exhaustiveness, identical to `Enum`.
        EpicsValue::EnumWithChoices { index, .. } => index.to_string(),
        EpicsValue::Char(v) => v.to_string(),
        // Arrays: C `@array@ { "v" "v" }` form. C's escaped-string branch
        // for a CHAR waveform (save_restore.c:1850) compares a dbAddr type
        // against CA's DBF_CHAR (4, dbFld USHORT): C writes a CHAR waveform
        // as this array and a USHORT one as a string it cannot restore. A C
        // defect, not copied: every waveform is written as an array.
        EpicsValue::DoubleArray(arr) => c_array(arr.iter().map(|v| double_text(*v))),
        EpicsValue::FloatArray(arr) => c_array(arr.iter().map(|v| float_text(*v))),
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
pub fn parse_save_value(raw: &str, template: &EpicsValue) -> Option<EpicsValue> {
    let s = raw.trim();
    // [`value_to_save_str`]'s own array text, handed over without a file.
    if s.starts_with(ARRAY_MARKER) {
        return parse_save_value(&c_array_value(s)?, template);
    }
    match template {
        // Untrimmed: C `dbrestore` hands everything after the separator to
        // `dbPutString`, so a scalar string's spaces are part of its value.
        // C `scalar_restore`'s `epicsStrnRawFromEscaped` (dbrestore.c:235).
        EpicsValue::String(_) => Some(EpicsValue::String(
            raw_from_escaped_bytes(raw.as_bytes()).into(),
        )),
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
            parse_array_str(s, |v| v.parse::<f64>().ok()).map(|v| EpicsValue::DoubleArray(v.into()))
        }
        EpicsValue::LongArray(_) => {
            parse_array_str(s, |v| v.parse::<i32>().ok()).map(|v| EpicsValue::LongArray(v.into()))
        }
        EpicsValue::CharArray(_) => {
            parse_array_str(s, |v| v.parse::<u8>().ok()).map(|v| EpicsValue::CharArray(v.into()))
        }
        EpicsValue::ShortArray(_) => {
            parse_array_str(s, |v| v.parse::<i16>().ok()).map(|v| EpicsValue::ShortArray(v.into()))
        }
        EpicsValue::FloatArray(_) => {
            parse_array_str(s, |v| v.parse::<f32>().ok()).map(|v| EpicsValue::FloatArray(v.into()))
        }
        EpicsValue::EnumArray(_) => {
            parse_array_str(s, |v| v.parse::<u16>().ok()).map(|v| EpicsValue::EnumArray(v.into()))
        }
        EpicsValue::Int64Array(_) => {
            parse_array_str(s, |v| v.parse::<i64>().ok()).map(|v| EpicsValue::Int64Array(v.into()))
        }
        EpicsValue::UInt64Array(_) => {
            parse_array_str(s, |v| v.parse::<u64>().ok()).map(|v| EpicsValue::UInt64Array(v.into()))
        }
        EpicsValue::UShort(_) => s.parse::<u16>().ok().map(EpicsValue::UShort),
        EpicsValue::ULong(_) => s.parse::<u32>().ok().map(EpicsValue::ULong),
        EpicsValue::UShortArray(_) => {
            parse_array_str(s, |v| v.parse::<u16>().ok()).map(|v| EpicsValue::UShortArray(v.into()))
        }
        EpicsValue::ULongArray(_) => {
            parse_array_str(s, |v| v.parse::<u32>().ok()).map(|v| EpicsValue::ULongArray(v.into()))
        }
        EpicsValue::UCharArray(_) => {
            parse_array_str(s, |v| v.parse::<u8>().ok()).map(|v| EpicsValue::UCharArray(v.into()))
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
    use crate::types::EpicsValue;

    /// M6: a C-format scalar string is written UNQUOTED (C autosave
    /// treats everything after the first space as the literal value).
    #[test]
    fn c_format_scalar_string_unquoted() {
        let v = EpicsValue::String("hello world".to_string().into());
        assert_eq!(value_to_save_str(&v), "hello world");
    }

    /// C `get_channel_values` and `SR_write_array_data` print a double with
    /// DOUBLE_FMT and a float with FLOAT_FMT (save_restore.h:44-45).
    #[test]
    fn floating_values_are_written_with_c_formats() {
        assert_eq!(value_to_save_str(&EpicsValue::Double(1.5)), "1.5");
        assert_eq!(value_to_save_str(&EpicsValue::Double(1e20)), "1e+20");
        assert_eq!(value_to_save_str(&EpicsValue::Double(0.1 + 0.2)), "0.3");
        assert_eq!(value_to_save_str(&EpicsValue::Float(0.1)), "0.1");
        assert_eq!(
            value_to_save_str(&EpicsValue::DoubleArray(vec![1.25, -2.0].into())),
            "@array@ { \"1.25\" \"-2\" }"
        );
        assert_eq!(
            value_to_save_str(&EpicsValue::FloatArray(vec![1.0e-5].into())),
            "@array@ { \"1e-05\" }"
        );
    }

    /// C `dbrestore` reads a scalar line as `%80s%c%[^\n\r]`: the value is
    /// everything after the one separator, spaces at either end included.
    #[test]
    fn a_string_value_keeps_its_spaces_on_restore() {
        let template = EpicsValue::String(Default::default());
        for text in [" MF", "MF ", "   "] {
            let v = EpicsValue::String(text.to_string().into());
            let written = value_to_save_str(&v);
            let contents = parse_save_content(&format!("X:post {written}\n"));
            let entry = &contents.entries[0];
            assert_eq!(
                parse_save_value(&entry.value, &template),
                Some(v.clone()),
                "{written:?}"
            );
        }
        // A number still ignores the spaces around it.
        assert_eq!(
            parse_save_value(" 7 ", &EpicsValue::Long(0)),
            Some(EpicsValue::Long(7))
        );
    }

    /// M6: a C-format array uses the `@array@ { "v" "v" }` form a C
    /// IOC can parse — not the native `[v,v,v]` form.
    #[test]
    fn c_format_array_uses_at_array_form() {
        let v = EpicsValue::LongArray(vec![1, 2, 3].into());
        assert_eq!(value_to_save_str(&v), "@array@ { \"1\" \"2\" \"3\" }");
    }

    /// M6: a written `.sav` carries C `write_it`'s header line and is
    /// still readable by the Rust reader — the array form
    /// round-trips through `parse_c_array_line`.
    #[epics_macros_rs::epics_test]
    async fn c_compat_save_file_has_c_banner_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sav");

        let entries = vec![
            SaveEntry {
                pv_name: "PV:SCALAR".to_string(),
                value: value_to_save_str(&EpicsValue::Long(42)),
                connected: true,
            },
            SaveEntry {
                pv_name: "PV:ARRAY".to_string(),
                value: value_to_save_str(&EpicsValue::LongArray(vec![10, 20].into())),
                connected: true,
            },
        ];
        write_save_file(&path, &entries).await.unwrap();

        let content = crate::runtime::fs::read_to_string(&path).await.unwrap();
        assert!(
            content.starts_with("# Autosave release 6.1, compiled "),
            "{content}"
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
        let parsed = parse_save_value(&arr.value, &EpicsValue::LongArray(vec![].into())).unwrap();
        assert_eq!(parsed, EpicsValue::LongArray(vec![10, 20].into()));
    }
}
