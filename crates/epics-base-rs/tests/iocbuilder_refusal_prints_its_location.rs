// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — the test hand-builds its own tokio runtime and passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test iocbuilder_refusal_prints_its_location`.
//! **`IocBuilder::build` locates a refused field value the way `dbLoadRecords`
//! does.** C prints `ERROR:  at or before ')' in path "." file "<f>" line N`
//! and the source-line echo under every `Can't set` line (`yyerror(NULL)`,
//! `dbLexRoutines.c:1409-1417`); the builder printed the `Can't set` line alone.
//!
//! The expected text is `softIoc -d <abs path>/a.db` stderr for the same file,
//! ANSI stripped. Before this the file name was not bound either, and the
//! location read `in standard input line 2`.

#![cfg(unix)]

use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = "record(ai, \"A1\") {\n    field(SCAN, \"bogus\")\n    field(INP, {\"const\":3})\n    field(DESC, \"ok\")\n}\nrecord(longin, \"L1\") { field(SIMM, \"RAW\") }\n";

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = match rest[at..].find('m') {
            Some(end) => &rest[at + end + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

#[test]
fn a_refused_field_names_its_line_through_the_builder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.db");
    std::fs::write(&path, DB).unwrap();

    let sink = tempfile::NamedTempFile::new().unwrap();
    let saved = unsafe { libc::dup(2) };
    {
        use std::os::fd::AsRawFd;
        assert!(unsafe { libc::dup2(sink.as_file().as_raw_fd(), 2) } >= 0);
    }
    let p = path.to_string_lossy().into_owned();
    let ran = std::thread::spawn(move || {
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
    .join();
    unsafe {
        libc::dup2(saved, 2);
        libc::close(saved);
    }
    assert!(ran.expect("builder thread").is_err());

    // `Did you mean` is `epicsPrintf` in C, i.e. the errlog thread, so its
    // place among the `fprintf` lines is a race there; compare without it.
    let got: String = strip_ansi(&std::fs::read_to_string(sink.path()).unwrap())
        .split_inclusive('\n')
        .filter(|l| !l.contains("Did you mean"))
        .collect();
    let want = [
        "ERROR: Can't set 'A1.SCAN' to 'bogus' using menu menuScan : Illegal choice\n",
        &format!(
            "ERROR:  at or before ')' in file \"{}\" line 2\n",
            path.display()
        ),
        "\n",
        " 2 |     field(SCAN, \"bogus\")\n",
        "\n",
        "ERROR: Can't set 'L1.SIMM' to 'RAW' using menu menuYesNo : Illegal choice\n",
        "ERROR: \n",
        " 6 | record(longin, \"L1\") { field(SIMM, \"RAW\") }\n",
        "\n",
    ]
    .concat();
    assert!(got.starts_with(&want), "got:\n{got}\nwant:\n{want}");
}
