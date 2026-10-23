use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::Local;
use epics_libcom_rs::cfmt::CFloat;

use super::error::AutosaveResult;
use super::format::END_MARKER;
use super::save_file::{SaveEntry, keep_permissions, write_save_file_with_settings};
use super::settings::SrSettings;

/// A set's own backup policy. The sequence files and dated backups are
/// the IOC's ([`super::settings::SrSettings`]), as C's globals are.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    /// Enable .savB backup (default: true)
    pub enable_savb: bool,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self { enable_savb: true }
    }
}

/// Publish a copy of `src` at `dst`, all or nothing.
///
/// **The single owner of every backup artefact write.** `std::fs::copy`
/// opens the destination CREATE|TRUNCATE before it can fail, so copying
/// onto an existing backup destroys that generation the moment the write
/// runs short of space — the previous good `.savB` is left zero-length and
/// `fopen_and_check` then rejects it for the missing `<END>`. Copying
/// to a sibling temp path and renaming makes that intermediate state
/// unconstructible: `dst` holds its previous complete content or the new
/// copy, never a truncation. C's `fopen(dest, "wb")` truncates in place
/// (dbrestore.c:192), a C defect not copied.
///
/// The temp name is the destination plus `.tmp`, so it is in the same
/// directory (`rename` is only atomic within a filesystem) and cannot
/// collide with the `<base>.tmp` that [`super::save_file`] uses for the
/// `.sav` write itself.
///
/// Power-loss durability is unchanged and deliberately not claimed here:
/// a backup is derived data, and the `.sav` write owns the `fsync`
/// sequence that makes the original durable.
pub(super) async fn publish_copy(
    src: &Path,
    dst: &Path,
    settings: &SrSettings,
) -> AutosaveResult<()> {
    if settings.debug() >= 5 {
        crate::runtime::log::errlog_vprintf(format_args!(
            "../dbrestore.c:myFileCopy copying '{}' to '{}'\n",
            src.display(),
            dst.display()
        ));
    }
    let mut tmp = dst.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let published = crate::runtime::fs::blocking({
        let (src, dst) = (src.to_path_buf(), dst.to_path_buf());
        // `true` with a source that would not open.
        move || {
            Ok(match std::fs::read(&src) {
                Ok(contents) => copy_through(&contents, &dst, &tmp).map_err(|(l, e)| (l, e, false)),
                Err(e) => Err((format!("Can't open file '{}'", src.display()), e, true)),
            })
        }
    })
    .await
    .unwrap_or_else(|e| Err((format!("Can't open file '{}'", dst.display()), e, false)));
    // Announced here rather than by the callers: this is the only place a
    // backup artefact is written, as C's `myFileCopy` is.
    match published {
        Ok(()) => Ok(()),
        Err((line, e, unread)) => {
            if unread {
                settings.note_io_error(false);
            }
            crate::runtime::log::errlog_vprintf(format_args!("../dbrestore.c:myFileCopy {line}\n"));
            Err(e.into())
        }
    }
}

/// `publish_copy`'s blocking half; the error carries C's `myFileCopy` line.
/// C names the source when it cannot be read and the destination for
/// anything after (dbrestore.c:181-216).
fn copy_through(contents: &[u8], dst: &Path, tmp: &Path) -> Result<(), (String, std::io::Error)> {
    use std::io::Write as _;
    let failed = |what: &str, path: &Path| {
        let line = format!("{what} '{}'", path.display());
        move |e| (line, e)
    };
    // C's `fopen("", "wb")`, for `write_save_file`'s empty `backup_file`.
    if dst.as_os_str().is_empty() {
        return Err(failed("Can't open file", dst)(
            std::io::ErrorKind::NotFound.into(),
        ));
    }
    let published = std::fs::File::create(tmp)
        .map_err(failed("Can't open file", dst))
        // C only says "Error closing file" for a failed write-back and still
        // returns OK (dbrestore.c:209-218), so a short backup passes as
        // made; here it fails.
        .and_then(|mut file| {
            // C's `fopen(dest, "wb")` keeps the mode of a file already there.
            keep_permissions(&file, dst);
            file.write_all(contents)
                .map_err(failed("Error closing file", dst))
        })
        // A destination C could not `fopen(dest, "wb")`.
        .and_then(|()| std::fs::rename(tmp, dst).map_err(failed("Can't open file", dst)));
    if published.is_err() {
        // The rename never happened, so this temp file is the only trace of
        // the attempt; leaving it would accumulate one per failed cycle next
        // to the save set.
        let _ = std::fs::remove_file(tmp);
    }
    published
}

/// C `fGetDateStr` (fGetDateStr.c:79).
pub(super) fn date_str() -> String {
    Local::now().format("%y%m%d-%H%M%S").to_string()
}

/// C's `line` between two rows of `stars` `***`s.
pub(super) fn print_banner(stars: usize, line: &str) {
    let row = "*** ".repeat(stars - 1) + "***";
    crate::epics_println!("{row}\n{line}\n{row}");
}

/// Whether the `.savB` backup still holds the *outgoing* generation and
/// so has to be refreshed once the new `.sav` is on disk.
///
/// [`rotate_backups`] produces it before the `.sav` write and
/// [`publish_savb`] consumes it after, which is how C splits the same
/// rule across `write_save_file` (`save_restore.c` at `R6-0-20-g186f467`):
/// the pre-write half guarantees a usable `.savB`, the post-write half
/// advances it, and the post-write half is skipped exactly when the
/// pre-write half already wrote this generation (C's `BS_NEW`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavbState {
    /// `.savB` backups are switched off for this set.
    Disabled,
    /// A valid `.savB` was already on disk, so it still holds the
    /// generation about to be replaced.
    Ok,
    /// It was missing or corrupt and has just been written from the values
    /// about to be saved, so it already holds the new generation.
    Rewritten,
}

/// C `check_file`'s answer (`BS_NONE`, `BS_BAD`, `BS_OK`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileState {
    None,
    Bad,
    Ok,
}

impl FileState {
    /// C's `BS_NONE`, `BS_BAD` and `BS_OK` values (save_restore.c:1637-1639).
    pub(super) fn code(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Bad => 1,
            Self::Ok => 2,
        }
    }
}

/// C `check_file` (save_restore.c:1642-1690): a file that opens is good
/// when `<END>` starts 7 or 6 bytes before its end.
pub(super) async fn check_file(path: &Path, settings: &SrSettings) -> FileState {
    let path = path.to_path_buf();
    let debug = settings.debug();
    crate::runtime::fs::blocking(move || Ok(check_file_blocking(&path, debug)))
        .await
        .unwrap_or(FileState::None)
}

/// [`check_file`] on the calling thread. It reads only the tail, as C's
/// `fseek` and `fgets` do.
pub(super) fn check_file_blocking(path: &Path, debug: i32) -> FileState {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut fd) = std::fs::File::open(path) else {
        return FileState::None;
    };
    for back in [7, 6] {
        if fd.seek(SeekFrom::End(-(back as i64))).is_err() {
            crate::epics_println!("save_restore:check_file: seek failed");
            return FileState::Bad;
        }
        let mut tail = Vec::with_capacity(back);
        let _ = (&mut fd).take(back as u64).read_to_end(&mut tail);
        // C's `fgets` of the rest cannot come back empty here, so its
        // "fgets failed" line has no counterpart.
        let tmpstr = fgets(&tail, back);
        if debug >= 5 {
            crate::epics_println!(
                "save_restore:check_file: tmpstr='{}'",
                String::from_utf8_lossy(tmpstr)
            );
        }
        if tmpstr.starts_with(END_MARKER.as_bytes()) {
            return FileState::Ok;
        }
    }
    FileState::Bad
}

/// Make sure a valid `.savB` exists *before* the `.sav` is overwritten.
///
/// C pivots this decision on the `.savB` itself, not on the `.sav`
/// ("Ensure that backup is ok before we overwrite .sav file."): a `.savB`
/// that is missing or does not end in `<END>` is rewritten from the values
/// about to be saved, and a corrupt one is kept aside as
/// `<name>.savB_SBAD_<yymmdd-HHMMSS>` first. Pivoting on the `.sav`
/// instead — which is what this module used to do — skips all of it
/// whenever the `.sav` is the file that is missing or corrupt, so the one
/// cycle that most needs a backup is the one that leaves a single usable
/// file behind.
///
/// A `.savB` that cannot be written aborts the cycle with the `.sav`
/// untouched, so a set never trades its last good generation for a
/// backup it failed to make.
async fn ensure_savb(
    sav_path: &Path,
    config: &BackupConfig,
    entries: &[SaveEntry],
    settings: &SrSettings,
) -> AutosaveResult<SavbState> {
    if !config.enable_savb {
        return Ok(SavbState::Disabled);
    }
    let savb = sav_path.with_extension("savB");
    let state = check_file(&savb, settings).await;
    if state == FileState::Ok {
        return Ok(SavbState::Ok);
    }

    let datetime = date_str();
    crate::epics_println!(
        "save_restore:write_save_file: Backup file ({}) bad or not found.  Writing a new one. [{datetime}]",
        savb.display()
    );
    // Only a `.savB` that is there has content worth keeping for
    // diagnosis, and losing that copy is not worth failing the cycle over.
    if state == FileState::Bad {
        let mut aside = savb.as_os_str().to_os_string();
        aside.push(format!("_SBAD_{datetime}"));
        let aside = Path::new(&aside);
        if publish_copy(&savb, aside, settings).await.is_err() {
            crate::epics_println!(
                "save_restore:write_save_file: myFileCopy() failed for '{}' -> '{}'",
                savb.display(),
                aside.display()
            );
        }
    }
    if let Err(e) = write_save_file_with_settings(
        &savb,
        entries,
        settings,
        &sav_path.file_name().unwrap_or_default().to_string_lossy(),
    )
    .await
    {
        print_banner(
            19,
            &format!("save_restore:write_save_file: Can't write new backup file. [{datetime}]"),
        );
        return Err(e);
    }
    Ok(SavbState::Rewritten)
}

/// Guarantee a usable `.savB` before writing a new `.sav` file. The
/// sequence files and dated backups are the save task's
/// ([`super::save_set::SaveSet::do_seq`],
/// [`super::save_set::SaveSet::do_periodic_dated_backup`]), as in C.
///
/// A failed rotation is reported rather than swallowed, and the caller
/// must not overwrite the `.sav` it could not preserve: the whole point
/// of the rotation is that the generation about to be replaced survives
/// somewhere else first.
///
/// The returned [`SavbState`] must be handed to [`publish_savb`] after the
/// `.sav` write; together they hold the invariant that a completed save
/// cycle always ends with a valid `.savB` alongside the new `.sav`.
pub async fn rotate_backups(
    sav_path: &Path,
    config: &BackupConfig,
    entries: &[SaveEntry],
    settings: &SrSettings,
) -> AutosaveResult<SavbState> {
    ensure_savb(sav_path, config, entries, settings).await
}

/// Advance `.savB` to the generation just written, which is the other
/// half of the rule [`rotate_backups`] starts.
///
/// Skipped precisely when the pre-write half already wrote this
/// generation into `.savB` (C's `backup_state != BS_NEW` guard), so the
/// file is never written twice in one cycle.
pub async fn publish_savb(
    sav_path: &Path,
    savb_state: SavbState,
    settings: &SrSettings,
) -> AutosaveResult<()> {
    if savb_state != SavbState::Ok {
        return Ok(());
    }
    publish_copy(sav_path, &sav_path.with_extension("savB"), settings).await
}

/// What C `fgets(buf, n, fp)` reads from `bytes`: at most `n - 1` bytes,
/// through the first newline.
fn fgets(bytes: &[u8], n: usize) -> &[u8] {
    let max = bytes.len().min(n.saturating_sub(1));
    let end = bytes[..max]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(max, |at| at + 1);
    &bytes[..end]
}

/// Modification time of `path`, or `None` when the filesystem will not
/// say. A candidate whose age is unknown sorts oldest — `None < Some(_)`
/// — so it is still selectable when it is the only one left.
async fn modified_time(path: &Path) -> Option<SystemTime> {
    let path = path.to_path_buf();
    crate::runtime::fs::blocking(move || std::fs::metadata(&path)?.modified())
        .await
        .ok()
}

/// C `do_seq`'s first slot for a list (save_restore.c:2162-2179): the
/// first one `stat` cannot see, else the one modified longest ago.
pub(super) async fn oldest_seq_slot(sav_path: &Path, num_seq_files: usize) -> usize {
    let mut oldest = (0, None);
    for i in 0..num_seq_files {
        let Some(mtime) = modified_time(&sav_path.with_extension(format!("sav{i}"))).await else {
            return i;
        };
        if oldest.1.is_none_or(|t| mtime < t) {
            oldest = (i, Some(mtime));
        }
    }
    oldest.0
}

/// `name` with `suffix` appended, as C `strncat`s onto a file name.
fn appended(name: &Path, suffix: &str) -> PathBuf {
    let mut s = name.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// C `fopen_and_check` (dbrestore.c:1236-1311): the file a boot restore
/// reads — `fname`, then `fname` + `B`, then the sequence files — and
/// whether it is a sequence file (C's `*status = 1`), under `settings`'
/// `save_restoreNumSeqFiles` and `save_restoreDatedBackupFiles`.
///
/// C tries the slot closest in time to now and then the slots after it in
/// index order, wrapping, which after a bad newest slot is the OLDEST one,
/// since `do_seq` writes round-robin. That is a C defect: here the slots are
/// tried closest first, all of them.
pub async fn fopen_and_check(fname: &Path, settings: &SrSettings) -> Option<(PathBuf, bool)> {
    let log = |line: String| {
        crate::runtime::log::errlog_vprintf(format_args!("../dbrestore.c:fopen_and_check {line}\n"))
    };
    let num_seq_files = settings.seq_files();
    let first = check_restore_file(fname, settings).await;
    if settings.debug() >= 1 {
        // C prints the `FILE *`; any address stands for a file that opened.
        let fd = if first {
            format!("{:p}", fname.as_os_str().as_encoded_bytes().as_ptr())
        } else {
            "(nil)".to_string()
        };
        crate::epics_println!("fopen_and_check: checkFile returned {fd}");
    }
    if first {
        return Some((fname.to_path_buf(), false));
    }
    let savb = appended(fname, "B");
    log(format!("Trying backup file '{}'", savb.display()));
    if check_restore_file(&savb, settings).await {
        return Some((savb, false));
    }
    // Every slot is tried, as in C; the ones `stat` dates younger than C's
    // 1e9 s horizon (:1265, :1281) first, closest to now first. C tries the
    // closest and then the next numbers round (:1296-1304), which after
    // the newest is the oldest; that C defect is not kept.
    let now = SystemTime::now();
    let mut slots = Vec::new();
    for i in 0..num_seq_files {
        let path = appended(fname, &i.to_string());
        let mtime = modified_time(&path).await;
        if let Some(mtime) = mtime
            && settings.debug() >= 5
        {
            // `fabs(difftime(currTime, st_mtime))`, both whole seconds.
            let secs = |t: SystemTime| {
                t.duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64)
            };
            let ctime = chrono::DateTime::<Local>::from(mtime).format("%a %b %e %H:%M:%S %Y\n");
            log(format!("'{}' modified at {ctime}", path.display()));
            log(format!(
                "'{}' is {:.6} seconds old",
                path.display(),
                CFloat((secs(now) - secs(mtime)).abs() as f64)
            ));
        }
        let age = mtime
            .map(|mtime| now.duration_since(mtime).unwrap_or_else(|e| e.duration()))
            .filter(|age| *age < Duration::from_secs(1_000_000_000));
        slots.push((age.is_none(), age, path));
    }
    slots.sort();
    let mut last = appended(fname, "0");
    if slots.first().is_none_or(|(undated, ..)| *undated) {
        log("Can't figure out which seq file is most recent,".to_string());
        log(format!(
            "so I'm just going to start with '{}'.",
            last.display()
        ));
    }
    for (.., path) in slots {
        log(format!("Trying backup file '{}'", path.display()));
        if check_restore_file(&path, settings).await {
            return Some((path, true));
        }
        last = path;
    }
    log("Can't find a file to restore from...".to_string());
    log(format!("...last tried '{}'. I give up.", last.display()));
    crate::epics_println!("save_restore: **********************************\n");
    None
}

/// C `checkFile` (dbrestore.c:1137-1234): whether `file` opens and,
/// unless its header names a version below 1.8 (:1171-1180), ends in
/// `<END>`; a file that does not is copied to `file` + `_RBAD_[date]`.
async fn check_restore_file(file: &Path, settings: &SrSettings) -> bool {
    let log = |line: String| {
        crate::runtime::log::errlog_vprintf(format_args!("../dbrestore.c:checkFile {line}\n"))
    };
    let debug = settings.debug();
    if debug >= 2 {
        crate::epics_println!("checkFile: entry");
    }
    let path = file.to_path_buf();
    let Ok(bytes) = crate::runtime::fs::blocking(move || std::fs::read(path)).await else {
        log(format!("Can't open file '{}'.", file.display()));
        return false;
    };
    // C's stream: `fgets` from `pos`, which a failed `fseek` leaves alone.
    let mut pos = 0;
    let gets = |pos: &mut usize, n: usize| -> Option<&[u8]> {
        let line = fgets(bytes.get(*pos..).filter(|b| !b.is_empty())?, n);
        *pos += line.len();
        Some(line)
    };
    // An empty file C gives up on without a word (:1159-1161).
    // C's `PATH_SIZE + 50` buffer (save_restore.h:85).
    let Some(header) = gets(&mut pos, 255 + 50) else {
        return false;
    };
    // The number after the header's first `R`, else its first `V`.
    let tag = header
        .iter()
        .position(|&b| b == b'R')
        .or_else(|| header.iter().position(|&b| b == b'V'));
    // C trusts a header with no version (dbrestore.c:1165-1169), and since
    // autosave 8804347 the "Autosave release" header it writes has none, so
    // it restores a torn .sav over a good .savB. A C regression, not
    // copied: only an explicit version below 1.8 skips the `<END>` test.
    if let Some(at) = tag {
        let version = if header.get(at + 1).is_some_and(u8::is_ascii_digit) {
            crate::runtime::stdlib::strtod(&header[at + 1..]).value
        } else {
            0.0
        };
        if debug >= 2 {
            crate::epics_println!("checkFile: version={:.6}", CFloat(version));
        }
        if version < 1.8 {
            return true;
        }
    }
    let len = bytes.len();
    for (back, line) in [(6, 1183), (7, 1202)] {
        match len.checked_sub(back) {
            Some(start) => pos = start,
            // `fseek`'s EINVAL, a C library errno and not an OS error: 22
            // and this text in glibc, musl, newlib and the Windows CRT.
            None => super::save_file::print_errno(
                "checkFile: fseek error ",
                "../dbrestore.c",
                line,
                22,
                "Invalid argument",
            ),
        }
        // A file too short for the marker C gives up on without a word.
        let Some(tail) = gets(&mut pos, back) else {
            return false;
        };
        if debug >= 5 {
            crate::epics_println!(
                "checkFile: files ends with '{}'",
                String::from_utf8_lossy(tail)
            );
        }
        if tail.starts_with(END_MARKER.as_bytes()) {
            return true;
        }
    }
    log(format!("File '{}' is not trusted.", file.display()));
    let rbad = appended(
        file,
        &format!(
            "_RBAD_{}",
            if settings.dated_backup_files() {
                date_str()
            } else {
                String::new()
            }
        ),
    );
    if publish_copy(file, &rbad, settings).await.is_err() {
        log(format!(
            "myFileCopy() failed for '{}' -> '{}'",
            file.display(),
            rbad.display()
        ));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `myFileCopy` counts a source it cannot open as an I/O error, and
    /// no other failure of the copy (dbrestore.c:181-195).
    #[crate::epics_test]
    async fn only_an_unopened_copy_source_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let settings = SrSettings::default();
        let missing = dir.path().join("missing");
        let _ = publish_copy(&missing, &dir.path().join("b"), &settings).await;
        assert_eq!(settings.io_errors(), 1);
        let src = dir.path().join("a");
        std::fs::write(&src, "x").unwrap();
        let _ = publish_copy(&src, &missing.join("b"), &settings).await;
        assert_eq!(settings.io_errors(), 1);
    }
}
