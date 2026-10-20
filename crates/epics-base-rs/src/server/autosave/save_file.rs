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
    /// Where each `fprintf` C `SR_write_array_data` writes an array with
    /// ends in `value` (see [`save_text`]); empty for a value C writes
    /// with one `fprintf`.
    pub array_fprintfs: Vec<usize>,
}

/// A line of a `.sav` file that declares no entry this grammar
/// understands — a truncated write, a hand edit.
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
    let save_file = path.file_name().unwrap_or_default().to_string_lossy();
    write_save_file_with_settings(path, entries, &SrSettings::default(), &save_file).await
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
/// `SaveEntry.value` is already [`value_to_save_str`] text. `save_file`
/// is the set's own file name (C `plist->save_file`), which C's failure
/// line names whatever file is being written.
pub async fn write_save_file_with_settings(
    path: &Path,
    entries: &[SaveEntry],
    settings: &SrSettings,
    save_file: &str,
) -> AutosaveResult<()> {
    let datetime = super::backup::date_str();
    let mut content = String::new();
    // Where each of C's checked `fprintf`s ends in `content`, and its site.
    let mut pieces = Vec::new();

    // C `write_it`'s header (save_restore.c:1803), whose SRversion is
    // AUTOSAVE_RELEASE (configure/CONFIG:31, autosave_release.pl:7). It
    // carries no `R` or `V`, so `checkFile` skips the `<END>` check for
    // these files exactly as for C's (dbrestore.c:1162-1180).
    content.push_str(&format!(
        "# {}\tAutomatically generated - DO NOT MODIFY - {datetime}\n",
        *SR_VERSION
    ));
    pieces.push((content.len(), Fprintf::Header));

    // C's incomplete-set declaration, in C's exact wording because a C
    // IOC parses it: `dbrestore.c` takes the count from the text after
    // the `!` and refuses the whole restore when the site has cleared
    // `save_restoreIncompleteSetsOk`.
    let not_connected = entries.iter().filter(|e| !e.connected).count();
    if not_connected > 0 {
        content.push_str(&format!(
            "! {not_connected} channel(s) not connected - or not all gets were successful\n"
        ));
        pieces.push((content.len(), Fprintf::NotConnected));
    }

    for entry in entries {
        if entry.connected {
            content.push_str(&entry.pv_name);
            content.push(' ');
            pieces.push((content.len(), Fprintf::Name));
            let start = content.len();
            pieces.extend(
                entry
                    .array_fprintfs
                    .iter()
                    .map(|end| (start + end, Fprintf::Element)),
            );
            content.push_str(&entry.value);
            if !entry.array_fprintfs.is_empty() {
                // `}\n`, the array's last `fprintf` (dbrestore.c:1447).
                content.push('\n');
                pieces.push((content.len(), Fprintf::ArrayEnd));
                continue;
            }
        } else if entry.pv_name.ends_with('$') {
            // C `write_it` writes a `$` member from its char buffer, which
            // no get has filled (save_restore.c:1827-1830, :1844-1848).
            content.push_str(&format!("#{} ", entry.pv_name));
            pieces.push((content.len(), Fprintf::Name));
        } else {
            // C `write_it`'s `#name value` for a channel that never
            // connected (save_restore.c:1296, :1835), which `asVerify` counts.
            // A long string writes its buffer whatever `valid` is, empty
            // for one that never connected (:1844-1849).
            content.push_str(&format!("#{} ", entry.pv_name));
            pieces.push((content.len(), Fprintf::Name));
            if !entry.pv_name.ends_with('$') {
                content.push_str("Search Issued");
            }
        }
        content.push('\n');
        pieces.push((content.len(), Fprintf::Value));
    }

    content.push_str(END_MARKER);
    content.push('\n');
    pieces.push((content.len(), Fprintf::End));

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
    // A file that fails C's post-write checks is not renamed over the
    // previous one, where C has already truncated it: C opens the `.sav` in
    // place with O_TRUNC (save_restore.c:1771, `fopen "w"` at :1794), so a
    // failed write loses the previous file, a C defect not copied.
    let tmp_path = path.with_extension("tmp");
    let final_path = path.to_path_buf();
    let parent = path.parent().map(|p| p.to_path_buf());
    let permissions = settings.file_permissions.load(Ordering::Relaxed);
    let must_set = settings.must_set_permissions.load(Ordering::Relaxed) != 0;
    let debug = settings.debug();
    let save_file = save_file.to_string();
    let open_failed = Arc::new(AtomicBool::new(false));
    let flag = open_failed.clone();
    let written = crate::runtime::fs::blocking(move || {
        let unopened = |e: &std::io::Error| {
            flag.store(true, Ordering::Relaxed);
            crate::epics_println!(
                "save_restore:write_it - unable to open file '{}' [{datetime}]",
                final_path.display()
            );
            my_print_errno("write_it", "../save_restore.c", 1774, e);
        };
        let failed = |e: std::io::Error| {
            let _ = std::fs::remove_file(&tmp_path);
            e
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
            if let Err((site, n, e)) =
                write_as_stdio(&mut file, content.as_bytes(), &pieces, &datetime)
            {
                let (line, dot) = site.c_site();
                if dot {
                    crate::epics_println!("save_restore:write_it: fprintf returned {n}. [{datetime}]");
                } else {
                    crate::epics_println!("save_restore:write_it: fprintf returned {n} [{datetime}].");
                }
                my_print_errno("write_it", "../save_restore.c", line, &e);
                errlog_vprintf(format_args!(
                    "../save_restore.c:write_it Giving up on this attempt to write '{save_file}'. [{}]\n",
                    super::backup::date_str()
                ));
                return Err(failed(e));
            }
            if let Some(e) = file.sync_all().err().filter(fsync_failed) {
                errlog_vprintf(format_args!(
                    "../save_restore.c:write_it fsync returned -1 [{datetime}]\n"
                ));
                my_print_errno("write_it", "../save_restore.c", 1925, &e);
            }
            if let Err(e) = close(file) {
                errlog_vprintf(format_args!(
                    "../save_restore.c:write_it fclose returned -1 [{datetime}]\n"
                ));
                my_print_errno("write_it", "../save_restore.c", 1935, &e);
                errlog_vprintf(format_args!(
                    "../save_restore.c:write_it Giving up on this attempt to write '{save_file}'. [{}]\n",
                    super::backup::date_str()
                ));
                return Err(failed(e));
            }
        }
        // C's post-write checks (save_restore.c:1940-1955).
        let state = super::backup::check_file_blocking(&tmp_path, debug);
        if state != super::backup::FileState::Ok {
            errlog_vprintf(format_args!(
                "../save_restore.c:write_it file-check failure [{datetime}], check_file={}\n",
                state.code()
            ));
            return Err(failed(std::io::Error::other("file-check failure")));
        }
        let meta = std::fs::metadata(&tmp_path).map_err(failed)?;
        if meta.len() == 0 {
            errlog_vprintf(format_args!(
                "../save_restore.c:write_it unphysical file size [{datetime}], size=0\n"
            ));
            return Err(failed(std::io::Error::other("unphysical file size")));
        }
        if let Ok(modified) = meta.modified()
            && let Some(delta) = file_time_difference(std::time::SystemTime::now(), modified)
            && delta > 10.0
        {
            crate::epics_println!(
                "Warning: save_restore:write_it: file time is different from IOC time [{datetime}], difference={:.6}s",
                CFloat(delta)
            );
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
    match &written {
        Ok(()) => settings.note_write_ok(),
        Err(_) if open_failed.load(Ordering::Relaxed) => settings.note_io_error(true),
        Err(_) => {}
    }
    written?;
    Ok(())
}

/// C `write_it`'s `difftime(time(NULL), fileStat.st_mtime)`
/// (save_restore.c:1957), of whole seconds.
fn file_time_difference(now: std::time::SystemTime, mtime: std::time::SystemTime) -> Option<f64> {
    let secs = |t: std::time::SystemTime| {
        t.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as f64)
    };
    Some(secs(now)? - secs(mtime)?)
}

/// `file` closed as C `write_it`'s `fclose` (save_restore.c:1931), whose
/// failure — a deferred NFS write error, say — fails the write, where
/// dropping the `File` would ignore it.
fn close(file: std::fs::File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let fd = std::os::fd::IntoRawFd::into_raw_fd(file);
        // SAFETY: `fd` came out of the `File`, so it is open and closed once.
        if unsafe { libc::close(fd) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(unix))]
    drop(file);
    Ok(())
}

/// The `fprintf` of C `write_it` a piece of the file is.
#[derive(Clone, Copy)]
enum Fprintf {
    Header,
    NotConnected,
    Name,
    Value,
    /// One of an array's `fprintf`s, whose return C adds up.
    Element,
    /// An array's last `fprintf`, where C checks the sum.
    ArrayEnd,
    End,
}

impl Fprintf {
    /// The site's `myPrintErrno` line, and whether its message puts the
    /// `.` before the date (save_restore.c:1804-1889).
    fn c_site(self) -> (u32, bool) {
        match self {
            Self::Header => (1806, true),
            Self::NotConnected => (1816, true),
            Self::Name => (1838, true),
            Self::Value | Self::Element | Self::ArrayEnd => (1870, false),
            Self::End => (1889, true),
        }
    }
}

/// `content` written as glibc's `FILE` writes C's `fprintf`s: a full buffer
/// of `st_blksize` (at most `BUFSIZ`) goes out when a piece overflows it,
/// and the rest at C's `fflush` (save_restore.c:1895-1900), whose failure
/// C only prints. A failed write empties the buffer and drops the rest of
/// its `fprintf`. `pieces` ends each `fprintf` at its offset in `content`.
///
/// An array's `fprintf`s only add to `SR_write_array_data`'s return, -1
/// for a failed one, and `write_it` gives up when that sum is not
/// positive (dbrestore.c:1385-1448, save_restore.c:1866-1868); the error
/// carries the `fprintf` return C prints.
fn write_as_stdio(
    file: &mut std::fs::File,
    content: &[u8],
    pieces: &[(usize, Fprintf)],
    datetime: &str,
) -> Result<(), (Fprintf, i64, std::io::Error)> {
    use std::io::Write as _;
    const BUFSIZ: usize = 8192;
    #[cfg(unix)]
    let block = file
        .metadata()
        .map(|m| std::os::unix::fs::MetadataExt::blksize(&m) as usize)
        .ok()
        .filter(|&b| b > 0 && b < BUFSIZ)
        .unwrap_or(BUFSIZ);
    #[cfg(not(unix))]
    let block = BUFSIZ;
    let mut flushed = 0;
    let mut start = 0;
    // `SR_write_array_data`'s `n`, and its last failure.
    let mut sum = 0i64;
    let mut failed = None;
    for &(end, site) in pieces {
        let mut written = Ok(());
        while end - flushed > block {
            written = file.write_all(&content[flushed..flushed + block]);
            if written.is_err() {
                break;
            }
            flushed += block;
        }
        match (site, written) {
            (Fprintf::Element | Fprintf::ArrayEnd, Ok(())) => sum += (end - start) as i64,
            (Fprintf::Element | Fprintf::ArrayEnd, Err(e)) => {
                sum -= 1;
                flushed = end;
                failed = Some(e);
            }
            (_, written) => written.map_err(|e| (site, -1, e))?,
        }
        if let Fprintf::ArrayEnd = site {
            if sum <= 0
                && let Some(e) = failed.take()
            {
                return Err((site, sum, e));
            }
            sum = 0;
            failed = None;
        }
        start = end;
    }
    if let Err(e) = file.write_all(&content[flushed..]) {
        crate::epics_println!("save_restore:write_it: fflush returned -1 [{datetime}]");
        my_print_errno("write_it", "../save_restore.c", 1899, &e);
    }
    Ok(())
}

/// C `write_it`'s `fchmod` after `save_restoreSet_FilePermissions`
/// (save_restore.c:1777-1786), which does not stop the write.
fn set_permissions(file: &std::fs::File, permissions: u32, path: &Path) {
    #[cfg(unix)]
    if let Err(e) = file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(permissions))
    {
        crate::epics_println!(
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
    crate::epics_println!("Error {err} - {short}\n{long}");
}

/// Whether C `write_it` reports this `fsync` error (save_restore.c:1912-
/// 1922): ENOTSUP is success, as EINVAL and EROFS are on RTEMS. Windows'
/// `_commit` status is not checked (:1910).
fn fsync_failed(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        let ok: &[i32] = if cfg!(target_os = "rtems") {
            &[libc::ENOTSUP, libc::EINVAL, libc::EROFS]
        } else {
            &[libc::ENOTSUP]
        };
        !e.raw_os_error().is_some_and(|n| ok.contains(&n))
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

/// C `myPrintErrno` (dbrestore.c:114-117), called at `file`:`line`; C calls
/// it only when `errno` is set.
pub(super) fn my_print_errno(s: &str, file: &str, line: u32, e: &std::io::Error) {
    if let Some(errno) = e.raw_os_error() {
        print_errno(s, file, line, errno, &crate::server::iocsh::c_strerror(e));
    }
}

/// [`my_print_errno`]'s line for a C `errno` and its `strerror` `text`.
pub(super) fn print_errno(s: &str, file: &str, line: u32, errno: i32, text: &str) {
    errlog_vprintf(format_args!("{file}({line}): [0x{errno:x}]={s}:{text}\n"));
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
    let content = read_save_text(path).await?;
    Ok(PartialSaveFile {
        complete: has_end_marker(&content),
        contents: parse_save_content(&content),
    })
}

/// A `.sav` file's text; a file that is there but cannot be read is
/// corrupt.
pub(super) async fn read_save_text(path: &Path) -> AutosaveResult<String> {
    crate::runtime::fs::read_to_string(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            e.into()
        } else {
            AutosaveError::CorruptSaveFile {
                path: path.display().to_string(),
                message: e.to_string(),
            }
        }
    })
}

/// Quick check if a .sav file has a valid `<END>` marker.
pub async fn validate_save_file(path: &Path) -> AutosaveResult<bool> {
    let content = crate::runtime::fs::read_to_string(path).await?;
    Ok(has_end_marker(&content))
}

pub(super) fn has_end_marker(content: &str) -> bool {
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
                        array_fprintfs: Vec::new(),
                    });
                }
            }
            continue;
        }

        // Normal line: PV_NAME<space>VALUE. A value that begins with the
        // marker is an array, as C's `strncmp` decides (dbrestore.c:913),
        // whose restore reads on to its `}` (`ArrayWalk`).
        match line.split_once(' ') {
            Some((pv_name, value)) if !pv_name.is_empty() => {
                out.entries.push(SaveEntry {
                    pv_name: pv_name.to_string(),
                    value: value.to_string(),
                    connected: true,
                    array_fprintfs: Vec::new(),
                });
            }
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

/// The elements of an `@array@ { "e1" "e2" }` text as C's walk reads them;
/// `None` when it has no `{` or ends before its `}`.
fn c_array_elements(text: &str) -> Option<Vec<String>> {
    let walk = super::save_set::ArrayWalk::read(None, text, &mut || None, u32::MAX, false, 0);
    (walk.begin && !walk.eof).then(|| walk.texts())
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
    save_text(value).0
}

/// [`value_to_save_str`]'s text, and for an array where each of the
/// `fprintf`s C `SR_write_array_data` writes it with ends, but its last
/// (dbrestore.c:1385-1447).
pub fn save_text(value: &EpicsValue) -> (String, Vec<usize>) {
    /// The `fprintf`s of an element: one for an integer, the quotes and
    /// the number for a float, and a string's every escape and byte.
    #[derive(Clone, Copy)]
    enum Element {
        Integer,
        Floating,
        Text,
    }
    fn c_array<T, I>(kind: Element, iter: I) -> (String, Vec<usize>)
    where
        I: IntoIterator<Item = T>,
        T: ToString,
    {
        let mut text = format!("{ARRAY_MARKER} {{ ");
        let mut ends = vec![text.len()];
        for v in iter {
            let v = v.to_string();
            match kind {
                Element::Integer => text.push_str(&format!("\"{v}\" ")),
                Element::Floating => {
                    text.push('"');
                    ends.push(text.len());
                    text.push_str(&v);
                    ends.push(text.len());
                    text.push_str("\" ");
                }
                // The DBF_STRING element (dbrestore.c:1391-1405). C counts
                // each escape against the element's 39 characters, so an
                // element that fills its field loses a character from its
                // end per escape (oracle: `\"…XYZ` saved as `\"…XY`); a C
                // defect, not copied.
                Element::Text => {
                    text.push('"');
                    ends.push(text.len());
                    for c in v.chars() {
                        if matches!(c, '"' | '\\') {
                            text.push('\\');
                            ends.push(text.len());
                        }
                        match c {
                            '\n' => text.push_str("\\n"),
                            '\r' => text.push_str("\\r"),
                            c => {
                                ends.extend((1..c.len_utf8()).map(|i| text.len() + i));
                                text.push(c);
                            }
                        }
                        ends.push(text.len());
                    }
                    text.push_str("\" ");
                }
            }
            ends.push(text.len());
        }
        text.push('}');
        (text, ends)
    }
    use Element::{Floating, Integer, Text};
    // C `write_it` writes a member of at most one current element as a
    // scalar (save_restore.c:1853-1860): element 0, or for an empty array
    // what its get leaves. The `DBR_STRING` get of an empty string, LONG,
    // SHORT, CHAR or UCHAR array is "", as is a USHORT one's DBF_CHAR
    // branch (:1850); the others print the zero `ca_array_get` pads with.
    if value.is_array() && value.count() <= 1 {
        let text = match value.first_element() {
            Some(first) => value_to_save_str(&first),
            None if matches!(
                value,
                EpicsValue::StringArray(_)
                    | EpicsValue::LongArray(_)
                    | EpicsValue::ShortArray(_)
                    | EpicsValue::CharArray(_)
                    | EpicsValue::UCharArray(_)
                    | EpicsValue::UShortArray(_)
            ) =>
            {
                String::new()
            }
            None => "0".to_string(),
        };
        return (text, Vec::new());
    }
    let text = match value {
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
        // defect, not copied: every waveform of more than one element is
        // written as an array.
        EpicsValue::DoubleArray(arr) => {
            return c_array(Floating, arr.iter().map(|v| double_text(*v)));
        }
        EpicsValue::FloatArray(arr) => {
            return c_array(Floating, arr.iter().map(|v| float_text(*v)));
        }
        EpicsValue::LongArray(arr) => return c_array(Integer, arr.iter()),
        EpicsValue::CharArray(arr) => return c_array(Integer, arr.iter()),
        EpicsValue::ShortArray(arr) => return c_array(Integer, arr.iter()),
        EpicsValue::EnumArray(arr) => return c_array(Integer, arr.iter()),
        // C's `SR_write_array_data` has no INT64 or UINT64 case: it writes
        // `@array@ { }` with a "field_type 7 not handled." line per element
        // (dbrestore.c:1445), losing the values; a C defect, not copied.
        EpicsValue::Int64Array(arr) => return c_array(Integer, arr.iter()),
        EpicsValue::UInt64(v) => v.to_string(),
        EpicsValue::UInt64Array(arr) => return c_array(Integer, arr.iter()),
        EpicsValue::UShort(v) => v.to_string(),
        EpicsValue::ULong(v) => v.to_string(),
        EpicsValue::UShortArray(arr) => return c_array(Integer, arr.iter()),
        EpicsValue::ULongArray(arr) => return c_array(Integer, arr.iter()),
        EpicsValue::UChar(v) => v.to_string(),
        EpicsValue::UCharArray(arr) => return c_array(Integer, arr.iter()),
        EpicsValue::StringArray(arr) => return c_array(Text, arr.iter().cloned()),
    };
    (text, Vec::new())
}

/// Parse a save file value string back to EpicsValue, using template for type.
pub fn parse_save_value(raw: &str, template: &EpicsValue) -> Option<EpicsValue> {
    let s = raw.trim();
    // [`value_to_save_str`]'s own array text, handed over without a file.
    if s.starts_with(ARRAY_MARKER) && !template.is_array() {
        return parse_save_value(&encode_array_text(c_array_elements(s)?), template);
    }
    let elements = || array_element_texts(raw);
    // C unescapes every value before it puts it (dbrestore.c:235,
    // save_restore.c:3362).
    let text = String::from_utf8_lossy(&raw_from_escaped_bytes(s.as_bytes())).into_owned();
    match template {
        // Untrimmed: C `dbrestore` hands everything after the separator to
        // `dbPutString`, so a scalar string's spaces are part of its value.
        // C `scalar_restore`'s `epicsStrnRawFromEscaped` (dbrestore.c:235).
        EpicsValue::String(_) => Some(EpicsValue::String(
            raw_from_escaped_bytes(raw.as_bytes()).into(),
        )),
        EpicsValue::Double(_) => text.parse::<f64>().ok().map(EpicsValue::Double),
        EpicsValue::Float(_) => text.parse::<f32>().ok().map(EpicsValue::Float),
        EpicsValue::Long(_) => text.parse::<i32>().ok().map(EpicsValue::Long),
        EpicsValue::Int64(_) => text.parse::<i64>().ok().map(EpicsValue::Int64),
        EpicsValue::UInt64(_) => text.parse::<u64>().ok().map(EpicsValue::UInt64),
        EpicsValue::Short(_) => text.parse::<i16>().ok().map(EpicsValue::Short),
        EpicsValue::Enum(_) | EpicsValue::EnumWithChoices { .. } => {
            text.parse::<u16>().ok().map(EpicsValue::Enum)
        }
        EpicsValue::Char(_) => text.parse::<u8>().ok().map(EpicsValue::Char),
        EpicsValue::UChar(_) => text.parse::<u8>().ok().map(EpicsValue::UChar),
        EpicsValue::DoubleArray(_) => parse_array_str(&elements()?, |v| v.parse::<f64>().ok())
            .map(|v| EpicsValue::DoubleArray(v.into())),
        EpicsValue::LongArray(_) => parse_array_str(&elements()?, |v| v.parse::<i32>().ok())
            .map(|v| EpicsValue::LongArray(v.into())),
        EpicsValue::CharArray(_) => parse_array_str(&elements()?, |v| v.parse::<u8>().ok())
            .map(|v| EpicsValue::CharArray(v.into())),
        EpicsValue::ShortArray(_) => parse_array_str(&elements()?, |v| v.parse::<i16>().ok())
            .map(|v| EpicsValue::ShortArray(v.into())),
        EpicsValue::FloatArray(_) => parse_array_str(&elements()?, |v| v.parse::<f32>().ok())
            .map(|v| EpicsValue::FloatArray(v.into())),
        EpicsValue::EnumArray(_) => parse_array_str(&elements()?, |v| v.parse::<u16>().ok())
            .map(|v| EpicsValue::EnumArray(v.into())),
        EpicsValue::Int64Array(_) => parse_array_str(&elements()?, |v| v.parse::<i64>().ok())
            .map(|v| EpicsValue::Int64Array(v.into())),
        EpicsValue::UInt64Array(_) => parse_array_str(&elements()?, |v| v.parse::<u64>().ok())
            .map(|v| EpicsValue::UInt64Array(v.into())),
        EpicsValue::UShort(_) => text.parse::<u16>().ok().map(EpicsValue::UShort),
        EpicsValue::ULong(_) => text.parse::<u32>().ok().map(EpicsValue::ULong),
        EpicsValue::UShortArray(_) => parse_array_str(&elements()?, |v| v.parse::<u16>().ok())
            .map(|v| EpicsValue::UShortArray(v.into())),
        EpicsValue::ULongArray(_) => parse_array_str(&elements()?, |v| v.parse::<u32>().ok())
            .map(|v| EpicsValue::ULongArray(v.into())),
        EpicsValue::UCharArray(_) => parse_array_str(&elements()?, |v| v.parse::<u8>().ok())
            .map(|v| EpicsValue::UCharArray(v.into())),
        EpicsValue::StringArray(_) if s.starts_with(ARRAY_MARKER) => Some(EpicsValue::StringArray(
            c_array_elements(s)?.into_iter().map(Into::into).collect(),
        )),
        // Any other text is one element, and an empty one none.
        EpicsValue::StringArray(_) => Some(EpicsValue::StringArray(if raw.is_empty() {
            Vec::new().into()
        } else {
            vec![raw_from_escaped_bytes(raw.as_bytes()).into()].into()
        })),
    }
}

fn parse_array_str<T, F>(elements: &[String], parse_elem: F) -> Option<Vec<T>>
where
    F: Fn(&str) -> Option<T>,
{
    elements.iter().map(|v| parse_elem(v)).collect()
}

/// A numeric array's element texts: C's `@array@` list, the `[..]` list
/// an older autosave-rs wrote, or another text as the one element C reads
/// it as, unescaped first (dbrestore.c:513, save_restore.c:2996), an empty
/// one as a zero (dbrestore.c:478-536, save_restore.c:2973-3010).
fn array_element_texts(raw: &str) -> Option<Vec<String>> {
    let s = raw.trim();
    Some(if s.starts_with(ARRAY_MARKER) {
        c_array_elements(s)?
    } else if s.starts_with('[') {
        decode_array_text(s)
    } else if raw.is_empty() {
        vec!["0".to_string()]
    } else {
        vec![String::from_utf8_lossy(&raw_from_escaped_bytes(s.as_bytes())).into_owned()]
    })
}

/// Whose conversion an array member's element texts take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrayRestore {
    /// `SR_put_array_values`, by the field's DBF type (dbrestore.c:520-536,
    /// :620-645).
    Boot,
    /// `manual_array_restore`, by the channel's CA type, which a ULONG or
    /// 64-bit field has as `DBR_DOUBLE` (save_restore.c:3002-3007,
    /// :3102-3107).
    Manual,
}

/// An array member's value as C's restore makes it from `raw`: each
/// element's `atol`, `strtoul` or `atof`, which take what number the text
/// starts with, cast to the element type. A 64-bit field at boot takes
/// `epicsParseInt64`, whose refusal leaves the zeroed element. Any other
/// `template` is [`parse_save_value`]'s.
pub fn parse_array_value(
    raw: &str,
    template: &EpicsValue,
    from: ArrayRestore,
) -> Option<EpicsValue> {
    if matches!(template, EpicsValue::StringArray(_)) || !template.is_array() {
        return parse_save_value(raw, template);
    }
    Some(array_of_texts(&array_element_texts(raw)?, template, from))
}

/// An array member's element `texts` as `from` converts them to the
/// element type of `template` ([`parse_array_value`]), an array of them
/// for a one-element field as much as for an array.
pub(super) fn array_of_texts(
    texts: &[String],
    template: &EpicsValue,
    from: ArrayRestore,
) -> EpicsValue {
    use crate::types::c_cast::{f64_to_i64, f64_to_u32, f64_to_u64};
    use crate::types::c_parse::{NumericField, atol, parse_base10_units_null, strtoul_auto};
    fn each<T>(texts: &[String], f: impl Fn(&str) -> T) -> Vec<T> {
        texts.iter().map(|t| f(t)).collect()
    }
    let atof = |t: &str| crate::runtime::stdlib::strtod(t.as_bytes()).value;
    let manual = from == ArrayRestore::Manual;
    match template {
        EpicsValue::StringArray(_) | EpicsValue::String(_) => {
            EpicsValue::StringArray(texts.iter().map(|t| t.as_str().into()).collect())
        }
        EpicsValue::CharArray(_) | EpicsValue::Char(_) => {
            EpicsValue::CharArray(each(texts, |t| atol(t) as u8).into())
        }
        EpicsValue::UCharArray(_) | EpicsValue::UChar(_) => {
            EpicsValue::UCharArray(each(texts, |t| atol(t) as u8).into())
        }
        EpicsValue::ShortArray(_) | EpicsValue::Short(_) => {
            EpicsValue::ShortArray(each(texts, |t| atol(t) as i16).into())
        }
        EpicsValue::UShortArray(_) | EpicsValue::UShort(_) => {
            EpicsValue::UShortArray(each(texts, |t| atol(t) as u16).into())
        }
        EpicsValue::EnumArray(_) | EpicsValue::Enum(_) | EpicsValue::EnumWithChoices { .. } => {
            EpicsValue::EnumArray(each(texts, |t| atol(t) as u16).into())
        }
        EpicsValue::LongArray(_) | EpicsValue::Long(_) => {
            EpicsValue::LongArray(each(texts, |t| atol(t) as i32).into())
        }
        EpicsValue::ULongArray(_) | EpicsValue::ULong(_) if manual => {
            EpicsValue::ULongArray(each(texts, |t| f64_to_u32(atof(t))).into())
        }
        EpicsValue::ULongArray(_) | EpicsValue::ULong(_) => {
            EpicsValue::ULongArray(each(texts, |t| strtoul_auto(t) as u32).into())
        }
        EpicsValue::Int64Array(_) | EpicsValue::Int64(_) if manual => {
            EpicsValue::Int64Array(each(texts, |t| f64_to_i64(atof(t))).into())
        }
        EpicsValue::Int64Array(_) | EpicsValue::Int64(_) => EpicsValue::Int64Array(
            each(texts, |t| {
                match parse_base10_units_null(NumericField::Int64, t) {
                    Some(EpicsValue::Int64(v)) => v,
                    _ => 0,
                }
            })
            .into(),
        ),
        EpicsValue::UInt64Array(_) | EpicsValue::UInt64(_) if manual => {
            EpicsValue::UInt64Array(each(texts, |t| f64_to_u64(atof(t))).into())
        }
        EpicsValue::UInt64Array(_) | EpicsValue::UInt64(_) => EpicsValue::UInt64Array(
            each(texts, |t| {
                match parse_base10_units_null(NumericField::UInt64, t) {
                    Some(EpicsValue::UInt64(v)) => v,
                    _ => 0,
                }
            })
            .into(),
        ),
        EpicsValue::FloatArray(_) | EpicsValue::Float(_) => EpicsValue::FloatArray(
            each(texts, |t| super::verify::safe_double_to_float(atof(t))).into(),
        ),
        EpicsValue::DoubleArray(_) | EpicsValue::Double(_) => {
            EpicsValue::DoubleArray(each(texts, atof).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EpicsValue;

    #[cfg(unix)]
    #[test]
    fn enotsup_from_fsync_is_success() {
        let errno = std::io::Error::from_raw_os_error;
        assert!(!fsync_failed(&errno(libc::ENOTSUP)));
        assert!(fsync_failed(&errno(libc::EIO)));
    }

    /// glibc flushes only a piece that overflows the buffer: one that ends
    /// exactly on `st_blksize` stays buffered for the `fflush`, whose
    /// failure is only printed (save_restore.c:1895-1900).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_piece_filling_the_buffer_waits_for_the_fflush() {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let blksize = std::os::unix::fs::MetadataExt::blksize(&file.metadata().unwrap()) as usize;
        let block = if blksize > 0 && blksize < 8192 {
            blksize
        } else {
            8192
        };
        let content = vec![b'x'; block + 1];
        let full = write_as_stdio(
            &mut file,
            &content[..block],
            &[(block, Fprintf::Value)],
            "D",
        );
        assert!(full.is_ok());
        let over = write_as_stdio(&mut file, &content, &[(block + 1, Fprintf::Value)], "D");
        assert!(matches!(over, Err((Fprintf::Value, -1, _))));
    }

    /// C takes the difference of whole seconds: 10.2 s across 11 second
    /// boundaries is 11, and 10.9 s within 10 of them is 10.
    #[test]
    fn the_file_time_difference_is_of_whole_seconds() {
        let at = |s: f64| std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(s);
        assert_eq!(file_time_difference(at(20.1), at(9.9)), Some(11.0));
        assert_eq!(file_time_difference(at(20.95), at(10.05)), Some(10.0));
        assert_eq!(file_time_difference(at(5.0), at(7.0)), Some(-2.0));
    }

    /// `SR_write_array_data`'s `fprintf`s: one per integer element, three
    /// per float, and a string's quote, escapes and bytes each.
    #[test]
    fn an_array_is_written_with_cs_fprintfs() {
        let (text, ends) = save_text(&EpicsValue::DoubleArray(vec![1.5, 2.0].into()));
        assert_eq!(text, "@array@ { \"1.5\" \"2\" }");
        assert_eq!(ends, [10, 11, 14, 16, 17, 18, 20]);
        let (_, ends) = save_text(&EpicsValue::LongArray(vec![1, 22].into()));
        assert_eq!(ends, [10, 14, 19]);
        let (text, ends) = save_text(&EpicsValue::StringArray(
            vec!["a\"\n".into(), "é".into()].into(),
        ));
        assert_eq!(text, "@array@ { \"a\\\"\\n\" \"é\" }");
        assert_eq!(ends, [10, 11, 12, 13, 14, 16, 18, 19, 20, 21, 23]);
        assert!(
            save_text(&EpicsValue::DoubleArray(vec![1.5].into()))
                .1
                .is_empty()
        );
    }

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
            value_to_save_str(&EpicsValue::FloatArray(vec![1.0e-5, 2.0].into())),
            "@array@ { \"1e-05\" \"2\" }"
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
    /// An escaped scalar is its unescaped text, as C's manual restore puts
    /// it (save_restore.c:3362; oracle: `M \x36` on an ao restores 6).
    #[test]
    fn a_scalar_value_is_unescaped_first() {
        assert_eq!(
            parse_save_value("\\x34", &EpicsValue::Double(0.0)),
            Some(EpicsValue::Double(4.0))
        );
        assert_eq!(
            parse_save_value("\\x35", &EpicsValue::Long(0)),
            Some(EpicsValue::Long(5))
        );
    }

    /// An `@array@` string element is unescaped once, by itself
    /// (dbrestore.c:623), not again with the line.
    #[test]
    fn an_array_string_element_is_unescaped_once() {
        assert_eq!(
            parse_save_value(
                r#"@array@ { "\\x36" }"#,
                &EpicsValue::StringArray(vec![].into())
            ),
            Some(EpicsValue::StringArray(vec!["\\x36".into()].into()))
        );
    }

    /// Oracle d26: `W \x35` on a LONG waveform restores 5.
    #[test]
    fn a_value_without_the_array_marker_is_unescaped_first() {
        let template = EpicsValue::LongArray(vec![0; 4].into());
        for from in [ArrayRestore::Boot, ArrayRestore::Manual] {
            assert_eq!(
                parse_array_value("\\x35", &template, from),
                Some(EpicsValue::LongArray(vec![5].into())),
                "{from:?}"
            );
        }
    }

    #[test]
    fn c_format_array_uses_at_array_form() {
        let v = EpicsValue::LongArray(vec![1, 2, 3].into());
        assert_eq!(value_to_save_str(&v), "@array@ { \"1\" \"2\" \"3\" }");
    }

    /// M6: a written `.sav` carries C `write_it`'s header line and is
    /// still readable by the Rust reader — the array form
    /// round-trips.
    #[epics_macros_rs::epics_test]
    async fn c_compat_save_file_has_c_banner_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sav");

        let entries = vec![
            SaveEntry {
                pv_name: "PV:SCALAR".to_string(),
                value: value_to_save_str(&EpicsValue::Long(42)),
                connected: true,
                array_fprintfs: Vec::new(),
            },
            SaveEntry {
                pv_name: "PV:ARRAY".to_string(),
                value: value_to_save_str(&EpicsValue::LongArray(vec![10, 20].into())),
                connected: true,
                array_fprintfs: Vec::new(),
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
        assert_eq!(arr.value, "@array@ { \"10\" \"20\" }");
        let parsed = parse_save_value(&arr.value, &EpicsValue::LongArray(vec![].into())).unwrap();
        assert_eq!(parsed, EpicsValue::LongArray(vec![10, 20].into()));
    }
}
