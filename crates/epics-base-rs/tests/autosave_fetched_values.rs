//! C keeps each list's last `get_channel_values` in the list: its
//! `not_connected` count (save_restore.c:1001, :1217, :1376) and the values.
//! The expected lines are the C oracle IOC's for each st.cmd.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_FETCHED_VALUES_DIR";

#[epics_macros_rs::epics_test]
async fn child_ioc() {
    let Some(dir) = std::env::var_os(CHILD).map(std::path::PathBuf::from) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(dir.join("st.cmd").to_str().unwrap())
        // The script's lines after iocInit run beside this; returning ends
        // the IOC, so it waits for the script's last put.
        .run(|config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            for _ in 0..200 {
                if config.db.get_pv("P:DONE").is_ok_and(|v| v.text_g() == "1") {
                    break;
                }
                epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        })
        .await
        .unwrap();
}

const RECORDS: &str = "record(ao, \"A\") { field(VAL, \"3\") }\n\
    record(waveform, \"W\") { field(NELM, \"3\") field(FTVL, \"LONG\") }\n\
    record(stringout, \"S\") { field(VAL, \"a b\") }\n\
    record(bo, \"P:DONE\") {}\n\
    record(longout, \"X:SR_heartbeat\") { field(DISP, \"1\") }\n\
    record(stringout, \"X:SR_recentlyStr\") {}\n\
    record(lso, \"L\") { field(SIZV, \"80\") }\n\
    alias(\"A\", \"AL\")\n";

/// The child's stdout, then its stderr, for `script`, run in a directory
/// holding `a.db` ([`RECORDS`]), `a.req` (A), `s.req` (A, W, S), `n.req`
/// (A and a PV no record serves), `f.req` (A.DESC, AL.DESC, AL) and `k.sav`
/// (AL, S, W and L's long string), and the directory.
fn run(script: &str) -> (Vec<String>, tempfile::TempDir) {
    let (mut lines, stderr, dir) = run_split(script);
    lines.extend(stderr);
    (lines, dir)
}

/// [`run`]'s stdout and stderr apart: errlog's lines land on stderr, where
/// no `echo` marker places them.
fn run_split(script: &str) -> (Vec<String>, Vec<String>, tempfile::TempDir) {
    run_with(script, &[])
}

/// [`run_split`] with `files` (name, contents) also in the directory.
fn run_with(script: &str, files: &[(&str, &str)]) -> (Vec<String>, Vec<String>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for (name, contents) in files {
        std::fs::write(d.join(name), contents).unwrap();
    }
    std::fs::write(d.join("a.db"), RECORDS).unwrap();
    std::fs::write(d.join("a.req"), "A\n").unwrap();
    std::fs::write(d.join("s.req"), "A\nW\nS\n").unwrap();
    std::fs::write(d.join("n.req"), "A\nNOSUCH\n").unwrap();
    std::fs::write(d.join("f.req"), "A.DESC\nAL.DESC\nAL\n").unwrap();
    std::fs::write(
        d.join("k.sav"),
        "# h\nAL 9\nS zz\nW @array@ { \"1\" \"2\" \"3\" }\nL.VAL$ hello long\n<END>\n",
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         {script}\
         dbpf P:DONE 1\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_ioc", "--nocapture", "--test-threads=1"])
        .env(CHILD, d)
        .current_dir(d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let lines = |bytes: &[u8]| -> Vec<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(|l| l.strip_prefix("test child_ioc ... ").unwrap_or(l))
            .filter(|l| !l.starts_with('\u{1b}'))
            .map(|l| l.trim_end().to_string())
            .collect()
    };
    (lines(&out.stdout), lines(&out.stderr), dir)
}

/// The lines after `marker` that report the list's `not_connected`.
fn not_connected_after(lines: &[String], marker: &str) -> String {
    lines
        .iter()
        .skip_while(|l| *l != marker)
        .find(|l| l.contains("not connected (or ca_get failed)"))
        .cloned()
        .unwrap_or_default()
}

/// A manual set's first pass, before `iocInit`, connects nothing; a manual
/// save after it fetches every member, and the list's count says so.
#[test]
fn a_manual_save_updates_the_lists_not_connected() {
    let (lines, _dir) = run("create_manual_set(\"a.req\")\n\
         epicsThreadSleep 2\n\
         iocInit\n\
         epicsThreadSleep 2\n\
         echo ==show1\n\
         save_restoreShow\n\
         manual_save(\"a.req\")\n\
         epicsThreadSleep 2\n\
         echo ==show2\n\
         save_restoreShow\n");
    assert_eq!(
        not_connected_after(&lines, "==show1"),
        "    1 channel  not connected (or ca_get failed)",
        "{lines:#?}"
    );
    assert_eq!(
        not_connected_after(&lines, "==show2"),
        "    0 channels not connected (or ca_get failed)",
        "{lines:#?}"
    );
}

/// The lines a `fdbrestore` of a list's last save file prints, and the
/// values after it: the C oracle IOC restores the values the save fetched,
/// with the file on disk rewritten to `A 9`, reads no file and writes no
/// `.bu`. C counts the array it restored as an error (status 1 and
/// "SR_put_array_values() to 'W' failed with 0."), a C defect not copied.
#[test]
fn a_restore_of_the_lists_last_save_uses_the_fetched_values() {
    let (lines, dir) = run("save_restoreSet_IncompleteSetsOk(1)\n\
         create_manual_set(\"s.req\")\n\
         iocInit\n\
         dbpf W.VAL \"[1,2,3]\"\n\
         epicsThreadSleep 2\n\
         manual_save(\"s.req\")\n\
         epicsThreadSleep 2\n\
         dbpf A 7\n\
         dbpf W.VAL \"[4,5]\"\n\
         dbpf S \"x\"\n\
         eraseFile(\"s.sav\")\n\
         appendToFile(\"s.sav\", \"# h\")\n\
         appendToFile(\"s.sav\", \"A 9\")\n\
         appendToFile(\"s.sav\", \"<END>\")\n\
         echo ==mem\n\
         save_restoreSet_Debug(5)\n\
         fdbrestore(\"s.sav\")\n\
         epicsThreadSleep 1\n\
         save_restoreSet_Debug(0)\n\
         echo ==values\n\
         dbgf A\n\
         dbgf W\n\
         dbgf S\n\
         echo ==end\n");
    let mem: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .skip_while(|l| *l != "==mem")
        .take_while(|l| *l != "==values")
        .filter(|l| l.contains("restore") && !l.contains("Set_Debug"))
        .collect();
    assert_eq!(
        mem,
        [
            "save_restore:fdbrestore:entry",
            "save_restore:request_manual_restore: entry",
            "save_restore task: calling do_manual_restore('s.sav')",
            "save_restore:do_manual_restore: entry for file 's.sav'",
            "save_restore: manual restore status=0 (0==success)",
        ],
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l
                == "../dbrestore.c:SR_put_array_values restoring 3 values to W (max_elements=3)"),
        "{lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("failed with")),
        "{lines:#?}"
    );
    let values: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .skip_while(|l| *l != "==values")
        .take_while(|l| *l != "==end")
        .filter(|l| l.starts_with("DBF_"))
        .collect();
    assert_eq!(
        values,
        [
            "DBF_DOUBLE:         3",
            "DBF_LONG[3]:        1 = 0x1   2 = 0x2   3 = 0x3",
            "DBF_STRING:         \"a b\"",
        ],
        "{lines:#?}"
    );
    assert!(!dir.path().join("s.sav.bu").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("s.sav")).unwrap(),
        "# h\nA 9\n<END>\n"
    );
}

/// The list's last fetch missed NOSUCH: with incomplete sets OK, C puts the
/// rest and fails the put to NOSUCH (ECA_DISCONN); without, it refuses.
#[test]
fn a_restore_of_an_incomplete_fetch_puts_or_refuses_as_c() {
    let (lines, _dir) = run("save_restoreSet_IncompleteSetsOk(1)\n\
         create_manual_set(\"n.req\")\n\
         iocInit\n\
         epicsThreadSleep 2\n\
         manual_save(\"n.req\")\n\
         epicsThreadSleep 2\n\
         dbpf A 7\n\
         echo ==ok\n\
         fdbrestore(\"n.sav\")\n\
         epicsThreadSleep 1\n\
         dbgf A\n\
         echo ==set\n\
         dbpf A 7\n\
         save_restoreSet_IncompleteSetsOk(0)\n\
         echo ==refused\n\
         fdbrestore(\"n.sav\")\n\
         epicsThreadSleep 1\n\
         dbgf A\n\
         echo ==end\n");
    let section = |from: &str, to: &str| -> Vec<String> {
        lines
            .iter()
            .skip_while(|l| *l != from)
            .take_while(|l| *l != to)
            .filter(|l| l.contains("not connected") || l.starts_with("DBF_"))
            .cloned()
            .collect()
    };
    assert_eq!(
        section("==ok", "==set"),
        [
            "save_restore:do_manual_restore: 1 channel(s) not connected or fetched",
            "DBF_DOUBLE:         3",
        ],
        "{lines:#?}"
    );
    assert_eq!(
        section("==refused", "==end"),
        [
            "save_restore:do_manual_restore: 1 channel(s) not connected or fetched",
            "DBF_DOUBLE:         7",
        ],
        "{lines:#?}"
    );
    for line in [
        "do_manual_restore:ca_put() to 'NOSUCH' failed with 192.",
        "../save_restore.c:do_manual_restore aborting restore",
        "do_manual_restore:failed because some PVs not connected",
    ] {
        assert_eq!(
            lines.iter().filter(|l| *l == line).count(),
            1,
            "{line}: {lines:#?}"
        );
    }
}

/// The libca exception blocks in `lines`, each without its time line.
fn write_exceptions(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .skip_while(|l| *l != "==mem")
        .filter(|l| l.starts_with("    Context:") || l.starts_with("    Warning:"))
        .cloned()
        .collect()
}

/// A put the record refuses (DISP) reaches C as libca's write exception,
/// one block per channel; the C oracle IOC leaves A 7 and S "a b". W, saved
/// empty, is a `DBR_STRING` put of "" the waveform cannot convert.
#[test]
fn a_refused_restore_put_prints_cs_write_exception() {
    let (lines, _dir) = run("create_manual_set(\"a.req\")\n\
         create_manual_set(\"s.req\")\n\
         iocInit\n\
         epicsThreadSleep 2\n\
         manual_save(\"s.req\")\n\
         epicsThreadSleep 2\n\
         dbpf A 7\n\
         dbpf A.DISP 1\n\
         dbpf S.DISP 1\n\
         echo ==mem\n\
         fdbrestore(\"s.sav\")\n\
         epicsThreadSleep 1\n\
         echo ==values\n\
         dbgf A\n\
         dbgf S\n\
         echo ==end\n");
    assert_eq!(
        write_exceptions(&lines),
        [
            "    Warning: \"Channel write request failed\"",
            "    Context: \"op=1, channel=A, type=DBR_STRING, count=1, ctx=\"A\"\"",
            "    Warning: \"Channel write request failed\"",
            "    Context: \"op=1, channel=W, type=DBR_STRING, count=1, ctx=\"W\"\"",
            "    Warning: \"Channel write request failed\"",
            "    Context: \"op=1, channel=S, type=DBR_STRING, count=1, ctx=\"S\"\"",
        ],
        "{lines:#?}"
    );
    let values: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .skip_while(|l| *l != "==values")
        .take_while(|l| *l != "==end")
        .filter(|l| l.starts_with("DBF_"))
        .collect();
    assert_eq!(
        values,
        ["DBF_DOUBLE:         7", "DBF_STRING:         \"a b\""],
        "{lines:#?}"
    );
}

/// rsrv's ctx is `RECORD_NAME(dbch)` (camessage.c:56), the record the
/// channel resolved to: the C oracle IOC prints ctx="A" for A.DESC, for
/// A's alias AL and for AL.DESC.
#[test]
fn a_refused_put_names_the_channels_record_in_ctx() {
    let (lines, _dir) = run("create_manual_set(\"f.req\")\n\
         iocInit\n\
         epicsThreadSleep 2\n\
         dbpf A.DESC hello\n\
         manual_save(\"f.req\")\n\
         epicsThreadSleep 2\n\
         dbpf A.DISP 1\n\
         echo ==mem\n\
         fdbrestore(\"f.sav\")\n\
         epicsThreadSleep 1\n");
    assert_eq!(
        write_exceptions(&lines),
        [
            "    Warning: \"Channel write request failed\"",
            "    Context: \"op=1, channel=A.DESC, type=DBR_STRING, count=1, ctx=\"A\"\"",
            "    Warning: \"Channel write request failed\"",
            "    Context: \"op=1, channel=AL.DESC, type=DBR_STRING, count=1, ctx=\"A\"\"",
            "    Warning: \"Channel write request failed\"",
            "    Context: \"op=1, channel=AL, type=DBR_STRING, count=1, ctx=\"A\"\"",
        ],
        "{lines:#?}"
    );
}

/// The same refusal from a file the list did not save: C's `ca_search`
/// channels print nothing for it.
#[test]
fn a_refused_put_from_a_file_prints_nothing() {
    let (lines, _dir) = run("create_manual_set(\"a.req\")\n\
         iocInit\n\
         epicsThreadSleep 2\n\
         dbpf S.DISP 1\n\
         eraseFile(\"f.sav\")\n\
         appendToFile(\"f.sav\", \"# h\")\n\
         appendToFile(\"f.sav\", \"A 9\")\n\
         appendToFile(\"f.sav\", \"S y\")\n\
         appendToFile(\"f.sav\", \"<END>\")\n\
         echo ==mem\n\
         fdbrestore(\"f.sav\")\n\
         epicsThreadSleep 1\n\
         echo ==values\n\
         dbgf A\n\
         dbgf S\n\
         echo ==end\n");
    let section: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .skip_while(|l| *l != "==mem")
        .take_while(|l| *l != "==end")
        .filter(|l| l.contains("failed") || l.contains("Exception") || l.starts_with("DBF_"))
        .collect();
    assert_eq!(
        section,
        ["DBF_DOUBLE:         9", "DBF_STRING:         \"a b\""],
        "{lines:#?}"
    );
}

/// C's `TRY_TO_PUT` of a status PV the record refuses: the C oracle IOC
/// prints libca's write exception for `X:SR_heartbeat` on each pass.
#[test]
fn a_refused_status_put_prints_cs_write_exception() {
    let (lines, _dir) = run("save_restoreSet_status_prefix(\"X:\")\n\
         create_manual_set(\"a.req\")\n\
         iocInit\n\
         echo ==mem\n\
         epicsThreadSleep 3\n");
    let blocks = write_exceptions(&lines);
    assert!(
        blocks.len() >= 2
            && blocks.chunks(2).all(|b| b
                == [
                    "    Warning: \"Channel write request failed\"",
                    "    Context: \"op=1, channel=X:SR_heartbeat, type=DBR_SHORT, count=1, ctx=\"X:SR_heartbeat\"\"",
                ]),
        "{lines:#?}"
    );
}

/// The refused-put lines on stdout from `==mem` to `==end` and on stderr,
/// and the SR_recentlyStr `dbgf` reads, each libca block cut to its
/// Context line, `X:SR_heartbeat`'s left out, sorted: errlog and stdout
/// interleave in no fixed order.
fn refusals(stdout: &[String], stderr: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = stdout
        .iter()
        .skip_while(|l| *l != "==mem")
        .take_while(|l| *l != "==end")
        .chain(stderr)
        .filter(|l| {
            (l.contains("failed") || l.contains("returned") || l.starts_with("    Context:"))
                && !l.starts_with("    Warning:")
                && !l.contains("X:SR_heartbeat")
                || l.contains("Restore of")
        })
        .cloned()
        .collect();
    lines.sort();
    lines
}

/// The task's CA context, made once as it starts (save_restore.c:802),
/// decides how a refused put reaches the console. A set created before
/// iocInit's `dbServiceIOInit` (dbCa.c:344) gets rsrv's channels: libca's
/// block for the memory restore and the status PV, nothing for a file's
/// `ca_search` channels. After it, `dbChannelIO::write` throws
/// (dbChannelIO.cpp:93): `ca_put` returns 160, printed and counted, and
/// the status PV's is dropped. The lines are the C oracle IOC's for each
/// order.
#[test]
fn a_refused_put_reports_by_the_tasks_ca_context() {
    let script = |create_before: bool| {
        let (before, after) = match create_before {
            true => ("create_manual_set(\"a.req\")\n", ""),
            false => ("", "create_manual_set(\"a.req\")\n"),
        };
        format!(
            "save_restoreSet_status_prefix(\"X:\")\n\
             {before}\
             iocInit\n\
             {after}\
             epicsThreadSleep 2\n\
             dbpf A 4\n\
             manual_save(\"a.req\")\n\
             epicsThreadSleep 2\n\
             dbpf A.DISP 1\n\
             dbpf S.DISP 1\n\
             dbpf W.DISP 1\n\
             dbpf L.DISP 1\n\
             echo ==mem\n\
             fdbrestore(\"a.sav\")\n\
             epicsThreadSleep 6\n\
             dbgf X:SR_recentlyStr\n\
             fdbrestore(\"k.sav\")\n\
             epicsThreadSleep 6\n\
             dbgf X:SR_recentlyStr\n\
             echo ==end\n"
        )
    };
    let heartbeat = "    Context: \"op=1, channel=X:SR_heartbeat, type=DBR_SHORT, count=1, ctx=\"X:SR_heartbeat\"\"";

    let (stdout, stderr, _dir) = run_split(&script(true));
    let lines = [&stdout[..], &stderr[..]].concat();
    assert_eq!(
        refusals(&stdout, &stderr),
        [
            "    Context: \"op=1, channel=A, type=DBR_STRING, count=1, ctx=\"A\"\"",
            "DBF_STRING:         \"Restore of \\'a.sav\\' Succeeded\"",
            "DBF_STRING:         \"Restore of \\'k.sav\\' Succeeded\"",
        ],
        "{lines:#?}"
    );
    assert!(stderr.iter().any(|l| l == heartbeat), "{lines:#?}");

    let (stdout, stderr, _dir) = run_split(&script(false));
    let lines = [&stdout[..], &stderr[..]].concat();
    assert_eq!(
        refusals(&stdout, &stderr),
        [
            "../save_restore.c:manual_array_restore ca_array_put to 'W' failed",
            "DBF_STRING:         \"Restore of \\'a.sav\\' Failed\"",
            "DBF_STRING:         \"Restore of \\'k.sav\\' Failed\"",
            "do_manual_restore:ca_put() to 'A' failed with 160.",
            "save_restore:do_manual_restore: ca_array_put of 'hello long' to 'L.VAL$' failed",
            "save_restore:do_manual_restore: ca_put of 9 to AL failed",
            "save_restore:do_manual_restore: ca_put of zz to S failed",
            "save_restore:do_manual_restore: manual_array_restore() returned -1",
        ],
        "{lines:#?}"
    );
    assert!(!lines.iter().any(|l| l == heartbeat), "{lines:#?}");
}

/// A one-element array member restores `pchannel->value`, the text
/// `get_channel_values` made with DOUBLE_FMT (save_restore.c:1623-1624,
/// :3244-3245), so 0.1+0.2 comes back as `%.14g`'s 0.3.
#[test]
fn a_one_element_array_restores_the_double_fmt_text() {
    let (lines, _, _dir) = run_with(
        "dbLoadRecords(\"d.db\")\n\
         create_manual_set(\"d.req\")\n\
         iocInit\n\
         dbpf WD.VAL \"[0.30000000000000004]\"\n\
         epicsThreadSleep 2\n\
         manual_save(\"d.req\")\n\
         epicsThreadSleep 2\n\
         dbpf WD.VAL \"[4,5]\"\n\
         fdbrestore(\"d.sav\")\n\
         epicsThreadSleep 1\n\
         dbpf EQ.PROC 1\n\
         echo ==values\n\
         dbgf WD.NORD\n\
         dbgf EQ\n\
         echo ==end\n",
        &[
            (
                "d.db",
                "record(waveform, \"WD\") { field(NELM, \"3\") field(FTVL, \"DOUBLE\") }\n\
                 record(calc, \"EQ\") { field(INPA, \"WD NPP\") field(CALC, \"A=0.3\") }\n",
            ),
            ("d.req", "WD\n"),
        ],
    );
    let values: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .skip_while(|l| *l != "==values")
        .skip(1)
        .take_while(|l| *l != "==end")
        .collect();
    assert_eq!(
        values,
        ["DBF_ULONG:          1 = 0x1", "DBF_DOUBLE:         1"],
        "{lines:#?}"
    );
}
