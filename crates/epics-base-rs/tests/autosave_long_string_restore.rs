//! Restoring a `field$` member. At boot the field gets the text through
//! dbStatic, a DBF_NOACCESS long string its `DBR_CHAR` put in pass 1
//! (dbrestore.c:935-978, :222-327); `fdbrestore` puts `DBR_CHAR` to the
//! `$` channel (save_restore.c:3373-3397). Either way a value is cut at
//! BUF_SIZE-1 characters. The file is the C oracle IOC's save plus a
//! 250-character line; the expected values and traces are the oracle's.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::{IocApplication, RunOutcome};
use epics_base_rs::types::EpicsValue;

const RESTORE_CHILD: &str = "AUTOSAVE_LONG_STRING_RESTORE_SCRIPT";

/// 250 characters, of which C writes the first 199.
fn long_value() -> String {
    (0..250u8).map(|i| char::from(b'0' + i % 40)).collect()
}

/// The oracle's `s.sav` after its header line.
fn c_body() -> String {
    format!(
        "SO.DESC$ hello world\n\
         SO2.DESC$ a\\\"b\\\\c{{d}}\n\
         C.CALC$ A+B+C+D\n\
         A.INP$ SO.VAL NPP NMS\n\
         LS.VAL$ {}\n\
         E.DESC$ \n\
         SO.DESC hello world\n\
         SO.VAL$ v1\n\
         <END>\n",
        &long_value()[..199]
    )
}

/// The oracle's restore records: every field differs from the file.
const RESTORE_DB: &str = r#"
record(stringout, "SO") { field(DESC, "x") field(VAL, "v0") }
record(stringout, "SO2") { field(DESC, "y") }
record(calcout, "C") { field(CALC, "0") }
record(ai, "A") { field(INP, "") }
record(lsi, "LS") { field(SIZV, "300") }
record(lsi, "LS2") { field(SIZV, "300") }
record(stringout, "E") { field(DESC, "zz") }
"#;

/// The values the restore left, as the oracle's `dbgf`s read them, in the
/// child only.
#[epics_macros_rs::epics_test]
async fn child_restore() {
    let Some(script) = std::env::var_os(RESTORE_CHILD) else {
        return;
    };
    let outcome = IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run_outcome(|config| async move {
            let db = config.db.clone();
            // iocInit returns, and a manual script reaches fdbrestore.
            epics_base_rs::server::db_server::announce_serving("probe");
            let deadline = Instant::now() + fact_budget::FACT_BUDGET;
            while db.get_pv("LS2.LEN").unwrap().to_f64() != Some(200.0) {
                assert!(Instant::now() < deadline, "LS2 never restored");
                sleep(Duration::from_millis(20)).await;
            }
            let text = |name: &str| match db.get_pv(name).unwrap() {
                EpicsValue::String(s) => s.to_string(),
                other => panic!("{name}: {other:?}"),
            };
            assert_eq!(text("SO.DESC"), "hello world");
            assert_eq!(text("SO2.DESC"), "a\"b\\c{d}");
            assert_eq!(text("C.CALC"), "A+B+C+D");
            assert_eq!(text("A.INP"), "SO.VAL NPP NMS");
            assert_eq!(text("E.DESC"), "");
            assert_eq!(text("SO.VAL"), "v1");
            for ls in ["LS", "LS2"] {
                let len = db.get_pv(format!("{ls}.LEN")).unwrap().to_f64();
                assert_eq!(len, Some(200.0), "{ls}");
                let val = match db.get_pv(format!("{ls}.VAL")).unwrap() {
                    EpicsValue::CharArray(c) => c.to_vec(),
                    EpicsValue::String(s) => s.as_bytes().to_vec(),
                    other => panic!("{ls}: {other:?}"),
                };
                let text: Vec<u8> = val.into_iter().take_while(|&c| c != 0).collect();
                assert_eq!(text, long_value().as_bytes()[..199], "{ls}");
            }
            config.autosave_manager.unwrap().shutdown();
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(outcome, RunOutcome::Served);
}

fn restore(before_init: &str, after_init: &str) -> std::process::Output {
    restore_with(before_init, after_init, "hello world")
}

/// [`restore`] with `SO.DESC$` holding `so_desc`.
fn restore_with(before_init: &str, after_init: &str, so_desc: &str) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), RESTORE_DB).unwrap();
    let body = c_body()
        .replace("<END>\n", &format!("LS2.VAL$ {}\n<END>\n", long_value()))
        .replace("SO.DESC$ hello world\n", &format!("SO.DESC$ {so_desc}\n"));
    std::fs::write(
        d.join("sav/s.sav"),
        format!("# Autosave release 6.1\n{body}"),
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         {before_init}\
         create_manual_set(\"\")\n\
         iocInit\n\
         {after_init}",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_restore",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(RESTORE_CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    out
}

/// The oracle's Debug-10 lines for each `$` member, in each pass.
#[test]
fn a_boot_restore_puts_the_long_string_back() {
    let out = restore(
        "save_restoreSet_Debug(10)\n\
         set_pass0_restoreFile(\"s.sav\")\n\
         set_pass1_restoreFile(\"s.sav\")\n",
        "",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("Treating as array.") || l.contains("reach end of line"))
        .collect();
    let mut pass = Vec::new();
    for (pv, n) in [
        ("SO.DESC$", 41),
        ("SO2.DESC$", 41),
        ("C.CALC$", 160),
        ("A.INP$", 1024),
        ("LS.VAL$", 300),
        ("E.DESC$", 41),
        ("SO.VAL$", 40),
        ("LS2.VAL$", 300),
    ] {
        pass.push(format!(
            "reboot_restore: PV '{pv}' is scalar in .sav file, but has {n} elements.  Treating as array."
        ));
        if pv.starts_with("LS") {
            pass.push("reboot_restore: did not reach end of line for long-string PV".to_string());
        }
    }
    let expected: Vec<String> = [pass.clone(), pass].concat();
    assert_eq!(lines, expected, "{stdout}");
}

/// The oracle's Debug-5 lines for each `$` member: C reads the element
/// count off the `$` address, and shows the value only as far as the
/// first `fgets` buffer reached.
#[test]
fn a_manual_restore_puts_the_long_string_back() {
    let out = restore("", "save_restoreSet_Debug(5)\nfdbrestore(\"s.sav\")\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("Treating as array.") || l.contains("is long string"))
        .collect();
    let long = long_value();
    let mut expected = Vec::new();
    for (pv, n, value) in [
        ("SO.DESC$", 41, "hello world"),
        ("SO2.DESC$", 41, "a\\\"b\\\\c{d}"),
        ("C.CALC$", 160, "A+B+C+D"),
        ("A.INP$", 1024, "SO.VAL NPP NMS"),
        ("LS.VAL$", 300, &long[..191]),
        ("E.DESC$", 41, ""),
        ("SO.VAL$", 40, "v1"),
        ("LS2.VAL$", 300, &long[..190]),
    ] {
        expected.push(format!(
            "save_restore:do_manual_restore: PV '{pv}' is scalar in .sav file, but has {n} elements.  Treating as array."
        ));
        expected.push(format!(
            "save_restore:do_manual_restore: PV '{pv}' is long string; value='{value}'."
        ));
    }
    assert_eq!(lines, expected, "{stdout}");
}

/// reviewer3's oracle run: 50 characters and their NUL are more than
/// `SO.DESC$`'s 41 elements, so C's `ca_array_put` fails (ECA_BADCOUNT),
/// prints its line and counts the error (save_restore.c:3394-3397). The
/// file's later `SO.DESC` line then sets the field as before.
#[test]
fn a_manual_long_string_longer_than_the_field_is_refused() {
    let value: String = "abcdefghij".repeat(5);
    let out = restore_with(
        "",
        "save_restoreSet_Debug(2)\nfdbrestore(\"s.sav\")\n",
        &value,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let refused =
        format!("save_restore:do_manual_restore: ca_array_put of '{value}' to 'SO.DESC$' failed");
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("ca_array_put") || l.contains("manual restore status"))
        .collect();
    assert_eq!(
        lines,
        [
            refused.as_str(),
            "save_restore: manual restore status=1 (0==success)"
        ],
        "{stdout}"
    );
}

const EDGE_CHILD: &str = "AUTOSAVE_LONG_STRING_EDGE_SCRIPT";

/// Boots the script and stops, in the child only.
#[epics_macros_rs::epics_test]
async fn child_edge() {
    let Some(script) = std::env::var_os(EDGE_CHILD) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run(|_| async move { Ok(()) })
        .await
        .unwrap();
}

/// The Debug-10 "did not reach end of line" lines a boot restore of an
/// `LS.VAL$` line of `line_len` characters, its newline counted, prints.
fn boot_reach_lines(line_len: usize) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), RESTORE_DB).unwrap();
    let value = &long_value()[..line_len - "LS.VAL$ \n".len()];
    std::fs::write(
        d.join("sav/s.sav"),
        format!("# Autosave release 6.1\nLS.VAL$ {value}\n<END>\n"),
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_Debug(10)\n\
         set_pass0_restoreFile(\"s.sav\")\n\
         set_pass1_restoreFile(\"s.sav\")\n\
         iocInit\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_edge", "--nocapture", "--test-threads=1"])
        .env(EDGE_CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("did not reach end of line"))
        .count()
}

/// C's first `fgets(buffer, BUF_SIZE)` holds 199 characters, so a
/// 200-character line is the first one it does not end (dbrestore.c:942-944);
/// one line per pass.
#[test]
fn a_boot_line_of_buf_size_characters_is_not_ended_by_one_read() {
    assert_eq!(boot_reach_lines(199), 0);
    assert_eq!(boot_reach_lines(200), 2);
}

/// 40 characters and their NUL fill `SO.DESC$`'s 41 elements exactly, so
/// the put is not refused.
#[test]
fn a_manual_long_string_that_fills_the_field_is_put() {
    let value: String = "abcdefghij".repeat(4);
    let out = restore_with(
        "",
        "save_restoreSet_Debug(2)\nfdbrestore(\"s.sav\")\n",
        &value,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("ca_array_put") || l.contains("manual restore status"))
        .collect();
    assert_eq!(
        lines,
        ["save_restore: manual restore status=0 (0==success)"],
        "{stdout}"
    );
}
