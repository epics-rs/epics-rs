//! Derives this crate's own copy of the `exec_backend` / `tokio_backend` cfg.
//!
//! The seam itself lives in `epics-libcom-rs` (whose `build.rs` is the
//! original of this one). This crate still needs the predicate because code
//! *above* the seam gates on it too — `server::scan` sizes its periodic-scan
//! threads differently on the reactor-free backend — and a cfg set by a
//! dependency's build script is not visible here. Each crate that gates on the
//! pair deriving its own cfg from the same two inputs is the existing pattern
//! in this workspace (`epics-ca-rs`, `epics-pva-rs`).
//!
//! Two copies of a predicate is two chances to disagree, so they are pinned:
//! `lib.rs` carries
//! `const _: () = assert!(epics_libcom_rs::EXEC_BACKEND == cfg!(exec_backend))`,
//! which fails to compile if either build script misses
//! `EPICS_RS_BUILD_EXEC_BACKEND`.
//!
//! * `exec_backend` — the std-thread background executor. The only option on
//!   `epics_embedded_target` (RTEMS or VxWorks — no tokio reactor on
//!   either), and — via `EPICS_RS_BUILD_EXEC_BACKEND=thread` — the
//!   host-selectable RTEMS execution model.
//! * `tokio_backend` — the tokio runtime. The hosted default (variable unset
//!   or `tokio`, non-embedded target).
//!
//! Exactly one of the two is set for any build.
//!
//! It also stamps the `epicsVCS.h` pair — see [`stamp_vcs_version`] — which
//! is a build-time job for the same reason C makes it one: the values name
//! the checkout that produced the binary, so nothing in the source tree can
//! hold them.

fn main() {
    stamp_vcs_version();

    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");
    println!("cargo::rustc-check-cfg=cfg(epics_embedded_target)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let embedded_target = matches!(target_os.as_str(), "rtems" | "vxworks");
    if embedded_target {
        println!("cargo::rustc-cfg=epics_embedded_target");
    }

    // Build-time backend selection, from the environment rather than from a
    // cargo feature: a feature that flips a backend is not additive, so
    // `--all-features` turned the reactor off and no single invocation meant
    // "everything on". `epics-libcom-rs`'s module docs carry the reasoning;
    // `tools/rtems-exec-gate` holds every copy of this block against that
    // crate's, so 23 derivations of one rule cannot drift apart.
    println!("cargo::rerun-if-env-changed=EPICS_RS_BUILD_EXEC_BACKEND");
    let requested = std::env::var_os("EPICS_RS_BUILD_EXEC_BACKEND").unwrap_or_default();
    let host_exec_backend = match requested.to_string_lossy().as_ref() {
        "thread" => true,
        "" | "tokio" => false,
        bad => panic!(
            "EPICS_RS_BUILD_EXEC_BACKEND={bad}: the exec backend is `thread` \
             (reactor-free std threads) or `tokio` (the host default, which an \
             unset or empty variable also selects)"
        ),
    };
    if embedded_target || host_exec_backend {
        println!("cargo::rustc-cfg=exec_backend");
    } else {
        println!("cargo::rustc-cfg=tokio_backend");
    }
}

/// Stamp C's `epicsVCS.h` pair into the compilation environment, where
/// `coreRelease`'s banner reads them with `env!` as C's banner reads the
/// macros.
///
/// C generates that header with `src/tools/genVersionHeader.pl`, which has
/// exactly two outcomes and no empty one. With a VCS checkout
/// (`genVersionHeader.pl:88-101` for the Git arm) it takes
/// `git describe --always --tags --dirty --abbrev=20` for `EPICS_VCS_VERSION`
/// and `<vcs>: ` + `git show -s --format=%ci HEAD` for
/// `EPICS_VCS_VERSION_DATE`. With no VCS directory (`:130-139`) and the empty
/// `-V` that `RULES_BUILD:453` passes, it names the source `build date/time`
/// and puts the build timestamp in the version.
///
/// So the pair is produced here by one function with no path that can yield
/// an empty string: [`from_git`] answers `None` rather than an empty value,
/// and [`build_timestamp`] is pure arithmetic that cannot fail. That is the
/// invariant, and `misc_commands.rs` asserts it again at compile time.
///
/// Cargo re-runs this script when a file in this package changes, where C's
/// `FORCE` rule regenerates `epicsVCS.h` on every make, so a commit that
/// touches nothing here leaves the previous stamp until this crate is built
/// again.
///
/// The one place this reads differently from C: C leaves the date's tail
/// unset on the no-VCS arm (`$cv` is never assigned there), so its own
/// fallback renders `Rev. Date build date/time: ` and stops. This puts the
/// timestamp C computed for that arm on both lines instead, because a field
/// that renders a label and nothing else is the defect being closed.
fn stamp_vcs_version() {
    let (version, date) = from_git().unwrap_or_else(build_timestamp);
    println!("cargo::rustc-env=EPICS_VCS_VERSION={version}");
    println!("cargo::rustc-env=EPICS_VCS_VERSION_DATE={date}");
}

/// The Git arm. `None` when git is absent, the checkout is not one (a
/// published `.crate` has no `.git`), or either command answers nothing —
/// an empty answer is a missing one, not a value.
fn from_git() -> Option<(String, String)> {
    let version = git(&["describe", "--always", "--tags", "--dirty", "--abbrev=20"])?;
    let date = git(&["show", "-s", "--format=%ci", "HEAD"])?;
    Some((version, format!("Git: {date}")))
}

fn git(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(std::env::var_os("CARGO_MANIFEST_DIR")?)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The no-VCS arm, in C's `%Y-%m-%dT%H:%M%z` shape. UTC, so the zone is
/// `+0000` where C's `localtime` prints the builder's own — the alternative
/// was shelling out to `date`, which cannot be the fallback for a missing
/// program.
fn build_timestamp() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let time = secs.rem_euclid(86_400);
    let now = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}+0000",
        time / 3600,
        (time % 3600) / 60
    );
    (now.clone(), format!("build date/time: {now}"))
}

/// Days since the epoch to a civil date — Howard Hinnant's `civil_from_days`,
/// the algorithm every `date` implementation uses. Written out because the
/// std library has no calendar and this crate takes no build-dependency.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}
