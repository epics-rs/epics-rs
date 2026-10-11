//! C's `.db` lexer expands a line as `db_yyinput` reads it
//! (`dbLexRoutines.c:375-391`), so a line's macLib notice and `WARNING` come
//! out after the diagnostics of the line before and ahead of its own. Every
//! byte below is `softIoc` R7.0.10-146-g8f5015b66 loading this exact file with
//! `EPICS_DB_INCLUDE_PATH` at its directory, ANSI stripped.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const SC_DB: &str = "\
record(calc, \"A\") { field(CALC, \"A+\") field(DESC, \"$(X)\") }
record(calc, \"B\") { field(CALC, \"B+\") field(DESC, \"$(Y)\") }
";

const WANT: &str = r#"macLib: macro X is undefined (expanding string record(calc, "A") { field(CALC, "A+") field(DESC, "$(X)") }
)
WARNING: 'sc.db' line 1 has undefined macros
Incomplete expression, operand missing in CALC expression 'A+'
ERROR: Can't set 'A.CALC' to 'A+'  : Bad Field value
ERROR:  at or before ')' in path "{DIR}"  file "sc.db" line 1

 1 | record(calc, "A") { field(CALC, "A+") field(DESC, "$(X,undefined)") }

A.DESC Has unexpanded macro
ERROR: Can't set 'A.DESC' to '$(X,undefined)'  : Bad Field value
ERROR: 
 1 | record(calc, "A") { field(CALC, "A+") field(DESC, "$(X,undefined)") }

macLib: macro Y is undefined (expanding string record(calc, "B") { field(CALC, "B+") field(DESC, "$(Y)") }
)
WARNING: 'sc.db' line 2 has undefined macros
Incomplete expression, operand missing in CALC expression 'B+'
ERROR: Can't set 'B.CALC' to 'B+'  : Bad Field value
ERROR: 
 2 | record(calc, "B") { field(CALC, "B+") field(DESC, "$(Y,undefined)") }

B.DESC Has unexpanded macro
ERROR: Can't set 'B.DESC' to '$(Y,undefined)'  : Bad Field value
ERROR: 
 2 | record(calc, "B") { field(CALC, "B+") field(DESC, "$(Y,undefined)") }

ERROR: Failed to load 'sc.db'
"#;

/// Run one iocsh line with fd 2 pointed at a file: macLib's notice goes
/// through errlog and the loader's lines through stderr, and the process
/// console is the one place their order is the operator's.
fn stderr_of(line: &str) -> String {
    let sink = tempfile::NamedTempFile::new().expect("capture file");
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0, "dup(2) failed");
    let fd = {
        use std::os::fd::AsRawFd;
        sink.as_file().as_raw_fd()
    };
    assert!(unsafe { libc::dup2(fd, 2) } >= 0, "dup2 onto fd 2 failed");
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let owned = line.to_string();
    let db = Arc::new(PvDatabase::new());
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_line(&owned)).join();
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };
    ran.expect("the shell thread").ok();
    let raw = std::fs::read_to_string(sink.path()).expect("read the capture");
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw.as_str();
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = rest[at..].find('m').map_or("", |end| &rest[at + end + 1..]);
    }
    out.push_str(rest);
    out
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn each_line_s_macro_warning_prints_where_c_reads_the_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("sc.db"), SC_DB).expect("write the .db");
    let dir_text = dir.path().display().to_string();
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", &dir_text) };
    let got = stderr_of("dbLoadRecords(\"sc.db\")");
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
    let want = WANT.replace("{DIR}", &dir_text);
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}
