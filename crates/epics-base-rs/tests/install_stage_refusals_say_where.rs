// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — the test hand-builds its own tokio runtime and passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test install_stage_refusals_say_where`.
//! **A refusal raised after the parse — an unknown record type, a name that is
//! already another type, an alias C will not create — is located the way
//! `yyerror` locates it, on both load paths.**
//!
//! Each is `yyerror(NULL)` / `yyerrorAbort(NULL)` inside the grammar's action,
//! so C prints `ERROR:  at or before ')'`, the file and line the lexer is
//! parked on, and the source line. An abort is followed by the grammar's own
//! `syntax error` at the EOF it makes of the next line read.
//!
//! The expected bytes are `softIoc -d` stderr (R7.0.10) for these exact files,
//! ANSI stripped; `dbLoadRecords` names the search path and the file, an
//! `IocBuilder::db_file` names the file as written. C's `WARNING: dbReadCOM:
//! Parser stack dirty` is not reproduced and is not in the expectations.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;

/// Run `f` with fd 2 pointed at a file, and give back what was written to it
/// without C's SGR sequences.
fn stderr_of<T>(f: impl FnOnce() -> T) -> (T, String) {
    let sink = tempfile::NamedTempFile::new().expect("capture file");
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0, "dup(2) failed");
    {
        use std::os::fd::AsRawFd;
        assert!(unsafe { libc::dup2(sink.as_file().as_raw_fd(), 2) } >= 0);
    }
    let ran = f();
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };
    let mut out = String::new();
    let mut rest = std::fs::read_to_string(sink.path()).expect("read the capture");
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = match rest[at..].find('m') {
            Some(end) => rest[at + end + 1..].to_string(),
            None => String::new(),
        };
    }
    out.push_str(&rest);
    (ran, out)
}

/// (file, text, diagnostics). A diagnostic is (message, line, source line);
/// the first quotes ` at or before ')'`, and an abort adds `syntax error`.
type Diagnostic = (&'static str, u32, &'static str);
type Case = (&'static str, &'static str, &'static [Diagnostic], bool);

const CASES: &[Case] = &[
    (
        "unknown_type.db",
        "record(nosuch, \"R1\") {\n}\n",
        &[(
            "Record type 'nosuch' for record 'R1' not found",
            1,
            "record(nosuch, \"R1\") {",
        )],
        true,
    ),
    (
        "other_type.db",
        "record(ai, \"R2\") {\n}\nrecord(bo, \"R2\") {\n}\n",
        &[(
            "bo record 'R2' already exists, can't load ai record",
            3,
            "record(bo, \"R2\") {",
        )],
        false,
    ),
    (
        "alias_is_record.db",
        "record(ai, \"R4\") {\n}\nrecord(ai, \"R5\") {\n    alias(\"R4\")\n}\n",
        &[(
            "Alias 'R4' for record 'R5' is already a record name.",
            4,
            "    alias(\"R4\")",
        )],
        false,
    ),
    (
        "alias_unknown_target.db",
        "record(ai, \"R4\") {\n}\nalias(\"R9\", \"R8\")\n",
        &[(
            "Alias 'R8' names an unknown record 'R9'",
            3,
            "alias(\"R9\", \"R8\")",
        )],
        false,
    ),
];

fn expected(location: &str, diagnostics: &[Diagnostic], abort: bool, tail: &str) -> String {
    let mut want = String::new();
    for (i, (message, line, source)) in diagnostics.iter().enumerate() {
        want += &format!("ERROR: {message}\n");
        if i == 0 {
            want += &format!("ERROR:  at or before ')'{location} line {line}\n");
        }
        want += &format!("\n {line} | {source}\n\n");
    }
    if abort {
        let (_, line, source) = diagnostics.last().unwrap();
        want += &format!("ERROR: syntax error\n\n {line} | {source}\n\n");
    }
    want + tail
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn dbloadrecords_locates_install_stage_refusals() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, text, ..) in CASES {
        std::fs::write(dir.path().join(name), text).expect("write the .db");
    }
    let found_under = dir.path().display().to_string();
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under) };
    for (name, _, diagnostics, abort) in CASES {
        let line = format!("dbLoadRecords(\"{name}\")");
        let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
        let (_, got) = stderr_of(|| {
            let db = Arc::new(PvDatabase::new());
            std::thread::spawn(move || IocShell::new(db, bridge).execute_line(&line))
                .join()
                .ok()
        });
        let want = expected(
            &format!(" in path \"{found_under}\"  file \"{name}\""),
            diagnostics,
            *abort,
            &format!("ERROR: Failed to load '{name}'\n"),
        );
        assert_eq!(
            got, want,
            "{name}\n--- got ---\n{got}\n--- want ---\n{want}"
        );
    }
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
}

#[test]
fn iocbuilder_locates_install_stage_refusals() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, text, diagnostics, abort) in CASES {
        let path = dir.path().join(name);
        std::fs::write(&path, text).expect("write the .db");
        let p = path.to_string_lossy().into_owned();
        let (built, got) = stderr_of(|| {
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        IocBuilder::new()
                            .db_file(&p, &Default::default())
                            .unwrap()
                            .build()
                            .await
                            .map(|_| ())
                    })
            })
            .join()
            .expect("builder thread")
        });
        assert!(built.is_err(), "{name}: build() is the load's status");
        let want = expected(
            &format!(" in file \"{}\"", path.display()),
            diagnostics,
            *abort,
            "",
        );
        assert_eq!(
            got, want,
            "{name}\n--- got ---\n{got}\n--- want ---\n{want}"
        );
    }
}
