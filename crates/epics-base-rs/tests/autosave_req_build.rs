// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_req_build` (7/7).
//! C `makeAutosaveFileFromDbInfo(fileBaseName, info_name)` and
//! `makeAutosaveFiles()` (dbrestore.c:1460-1554, :1739-1743) write a request
//! file from the records' info items; `eraseFile` and `appendToFile`
//! (:1557-1592) build one by hand, and `autosaveBuild` (:1612-1734) from
//! each `dbLoadRecords` through `dbLoadRecordsHook`. The expected files and lines are the
//! C oracle IOC's for the same database without `N`: C never clears its
//! `long_string` flag, so it would refuse `N.PREC` as not found after
//! `N.OUT$`, a C defect the port does not copy.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const CHILD: &str = "AUTOSAVE_REQ_BUILD_DIR";
const LINE: &str = "AUTOSAVE_REQ_BUILD_LINE";

const DB: &str = r#"record(stringout, "S") {
    info(autosaveFields, "VAL$ OUT$  DESC")
}
record(ao, "A") {
    alias("AA")
    info(autosaveFields, "  PREC DESC NOPE ")
}
record(ao, "L") {
    info(autosaveFields, "VAL$")
}
record(ao, "N") {
    info(autosaveFields, "OUT$ PREC")
}
record(bo, "B") {
    info(autosaveFields_pass0, "VAL")
    info(autosaveFields, "ZNAM")
}
"#;

/// The child's stdout lines naming the command, and its errlog lines naming
/// dbrestore.c, for `line` run in `dir`.
fn outputs_of(dir: &Path, line: &str) -> (Vec<String>, Vec<String>) {
    let name = "child_req_build";
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .current_dir(dir)
        .env(CHILD, dir)
        .env(LINE, line)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let pick = |bytes: &[u8], needle: &str| -> Vec<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .filter_map(|l| l.find(needle).map(|at| l[at..].to_owned()))
            .collect()
    };
    (
        pick(&out.stdout, "makeAutosaveFileFromDbInfo:")
            .into_iter()
            .chain(pick(&out.stdout, "autosaveBuild:"))
            .chain(pick(&out.stdout, "myDbLoadRecordsHook:"))
            .collect(),
        pick(&out.stderr, "../dbrestore.c"),
    )
}

/// `$LINE`, `;`-separated, in an iocsh that has loaded [`DB`].
#[test]
fn child_req_build() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let db_file = dir.join("t.db");
    std::fs::write(&db_file, DB).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = Arc::new(PvDatabase::new());
    let line = std::env::var(LINE).unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let holder = Arc::new(std::sync::Mutex::new(AutosaveStartupConfig::new()));
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(db, bridge);
        for cmd in AutosaveStartupConfig::register_startup_commands(holder) {
            shell.register(cmd);
        }
        shell
            .execute_line(format!("dbLoadRecords(\"{}\")", db_file.display()))
            .unwrap();
        for line in line.split(';') {
            shell.execute_line(line).unwrap();
        }
    })
    .join()
    .unwrap();
    epics_base_rs::runtime::log::errlog_flush();
}

const SETTINGS: &str = "A.PREC\nA.DESC\nN.OUT$\nN.PREC\nB.ZNAM\nS.VAL$\nS.OUT$\nS.DESC\n";

/// Records by type, then load order, an alias once; a field the record
/// lacks, or a `$` on a field that is neither a string nor a link, is
/// named and left out; ".req" is appended.
#[test]
fn a_request_file_from_the_info_items() {
    let dir = tempfile::tempdir().unwrap();
    let (out, errlog) = outputs_of(
        dir.path(),
        "makeAutosaveFileFromDbInfo(\"x\", \"autosaveFields\")",
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("x.req")).unwrap(),
        SETTINGS
    );
    assert_eq!(
        out,
        [
            "makeAutosaveFileFromDbInfo: A.NOPE not found",
            "makeAutosaveFileFromDbInfo: L.VAL$ not found",
        ]
    );
    assert_eq!(errlog, Vec::<String>::new());
}

#[test]
fn make_autosave_files_writes_settings_and_positions() {
    let dir = tempfile::tempdir().unwrap();
    outputs_of(dir.path(), "makeAutosaveFiles");
    let read = |f: &str| std::fs::read_to_string(dir.path().join(f)).unwrap();
    assert_eq!(read("info_settings.req"), SETTINGS);
    assert_eq!(read("info_positions.req"), "B.VAL\n");
}

#[test]
fn an_unopenable_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (_, errlog) = outputs_of(
        dir.path(),
        "makeAutosaveFileFromDbInfo(\"no/x.req\", \"autosaveFields\")",
    );
    assert_eq!(
        errlog,
        ["../dbrestore.c:makeAutosaveFileFromDbInfo unable to open file 'no/x.req'"]
    );
}

/// Each line on a line of its own after what was there; an erased file is
/// empty; a file that cannot be opened is named.
#[test]
fn append_to_file_and_erase_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("b.txt"), "old\n").unwrap();
    let (_, errlog) = outputs_of(
        dir.path(),
        "appendToFile(\"a.txt\", \"hi there\");appendToFile(\"a.txt\", \"2\");\
         eraseFile(\"b.txt\");appendToFile(\"nodir/a\", \"x\");eraseFile(\"nodir/a\")",
    );
    let read = |f: &str| std::fs::read_to_string(dir.path().join(f)).unwrap();
    assert_eq!(read("a.txt"), "hi there\n2\n");
    assert_eq!(read("b.txt"), "");
    assert_eq!(
        errlog,
        ["../dbrestore.c:appendToFile Can't open file 'nodir/a'"]
    );
}

/// `a.db`, `b.template` and `c.txt`, with request files for `a`'s settings
/// and `b`'s settings and positions, and a stale `built.req`.
fn build_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::create_dir(p.join("req")).unwrap();
    std::fs::write(p.join("a.db"), "record(ao, \"$(P)A\") {}\n").unwrap();
    std::fs::write(p.join("b.template"), "record(ao, \"$(P)B$(N)\") {}\n").unwrap();
    std::fs::write(p.join("c.txt"), "record(ao, \"$(P)C\") {}\n").unwrap();
    for req in ["a_settings", "b_settings", "b_positions"] {
        std::fs::write(p.join("req").join(format!("{req}.req")), "x\n").unwrap();
    }
    std::fs::write(p.join("built.req"), "stale\n").unwrap();
    std::fs::write(
        p.join("b.substitutions"),
        "file \"b.template\" {\npattern {P, N, M}\n{t:, 1, \"m$(N)\"}\n}\n",
    )
    .unwrap();
    dir
}

/// The C oracle IOC's `built.req` and lines for the same script: a row's
/// definitions expanded among themselves, a request file not on the path
/// skipped, a disabled suffix skipped.
#[test]
fn autosave_build_writes_a_file_line_per_load() {
    let dir = build_dir();
    let (out, _) = outputs_of(
        dir.path(),
        "set_requestfile_path(\"req\");autosaveBuild(\"\", \"_settings.req\", 1);\
         autosaveBuild(\"built.req\", \"\", 1);autosaveBuild(\"built.req\", \"_settings.req\", 1);\
         autosaveBuild(\"built.req\", \"_positions.req\", 1);dbLoadRecords(\"a.db\", \"P=x:\");\
         dbLoadTemplate(\"b.substitutions\", \"G=g\");dbLoadRecords(\"c.txt\", \"P=c:\");\
         autosaveBuild(\"built.req\", \"_positions.req\", 0);\
         dbLoadRecords(\"b.template\", \"P=y:,N=2\")",
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("built.req")).unwrap(),
        "file a_settings.req P=x:\n\
         file b_settings.req G=g,P=t:,N=1,M=\"m1\"\n\
         file b_positions.req G=g,P=t:,N=1,M=\"m1\"\n\
         file b_settings.req P=y:,N=2\n"
    );
    assert_eq!(
        out,
        [
            "autosaveBuild: bad filename",
            "autosaveBuild: bad suffix",
            "myDbLoadRecordsHook: Can't make request-file name from 'c.txt'",
        ]
    );
}

/// C toggles only the first suffix a `*` matches; each is toggled here.
#[test]
fn autosave_build_star_toggles_every_suffix() {
    let dir = build_dir();
    outputs_of(
        dir.path(),
        "set_requestfile_path(\"req\");autosaveBuild(\"built.req\", \"_settings.req\", 1);\
         autosaveBuild(\"built.req\", \"_positions.req\", 1);autosaveBuild(\"built.req\", \"*\", 0);\
         dbLoadRecords(\"b.template\", \"P=y:,N=2\")",
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("built.req")).unwrap(),
        ""
    );
}
