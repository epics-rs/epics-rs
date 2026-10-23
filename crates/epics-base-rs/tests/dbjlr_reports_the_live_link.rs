//! `dbjlr` calls each JSON link type's `report` on the record's own link,
//! so the lines carry what the link last did. The lnk*.c reports print with
//! plain `printf`, on the console. Expected blocks are the C oracle's
//! (`oracleStdIoc`, the same database and script).

use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "DBJLR_REPORTS_THE_LIVE_LINK_SCRIPT";

#[epics_macros_rs::epics_test]
async fn child_script() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .startup_script(script.to_str().unwrap())
        .run(|_| async { Ok(()) })
        .await
        .unwrap();
}

/// Runs `db` and `script` (`{d}` is the directory); returns the console
/// and each named file.
fn run(db: &str, script: &str, names: &[&str]) -> (String, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    // iocsh reads an unquoted `\` as an escape, as C `split()` does.
    let d = dir.path().display().to_string().replace('\\', "/");
    std::fs::write(format!("{d}/a.db"), db).unwrap();
    std::fs::write(
        format!("{d}/st.cmd"),
        format!(
            "dbLoadRecords(\"{d}/a.db\")\niocInit\n{}",
            script.replace("{d}", &d)
        ),
    )
    .unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_script", "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{d}/st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let files = names
        .iter()
        .map(|name| std::fs::read_to_string(format!("{d}/{name}")).unwrap())
        .collect();
    (String::from_utf8_lossy(&out.stdout).into_owned(), files)
}

/// lnkConst.c:286-347 over what its yajl callbacks parsed: `true` is an
/// integer, a mixed array doubles, `[]` the scalar arm's `default:`.
#[test]
fn a_const_link_reports_the_value_it_parsed() {
    let (console, files) = run(
        "record(ai, \"B\") { field(INP, {const:true}) }\n\
         record(waveform, \"E\") { field(NELM, 2) field(INP, {const:[]}) }\n\
         record(waveform, \"M\") { field(NELM, 3) field(FTVL, DOUBLE) \
         field(INP, {const:[1, 2.5, true]}) }\n\
         record(stringin, \"Q\") { field(INP, {const:\"a\\\"b\"}) }\n",
        "dbjlr B 2 > {d}/f.txt\n\
         dbjlr E 2\n\
         dbjlr M 2\n\
         dbjlr Q 2\n",
        &["f.txt"],
    );
    assert_eq!(files, [""]);
    for block in [
        "  ai record 'B':\n    Link field 'INP':\n      'const': integer 1\n",
        "  waveform record 'E':\n    Link field 'INP':\n      'const': bug -- type=4\n",
        "  waveform record 'M':\n    Link field 'INP':\n      'const': array of 3 doubles\n        [1, 2.5, 1]\n",
        "  stringin record 'Q':\n    Link field 'INP':\n      'const': string \"a\"b\"\n",
    ] {
        assert!(console.contains(block), "{block}\n{console}");
    }
}

/// lnkCalc.c:377-425, after one evaluation.
#[test]
fn a_calc_link_reports_its_last_evaluation() {
    let (console, files) = run(
        "record(ai, \"X\") { field(INP, {calc:{expr:\"A+B\", args:[2, {const:3}], \
         major:\"A>1\", units:\"mm\", prec:3, time:\"b\"}}) }\n",
        "dbpf X.PROC 1\n\
         dbjlr X 2 > {d}/two.txt\n\
         dbjlr X 0 > {d}/zero.txt\n\
         echo END\n",
        &["two.txt", "zero.txt"],
    );
    assert_eq!(files, ["", ""]);
    let two = concat!(
        "JSON links in record 'X'\n\n",
        "  ai record 'X':\n",
        "    Link field 'INP':\n",
        "      'calc': \"A+B\" = 5 mm\n",
        "        Alarm: MAJOR, LINK, \"post_major error\"\n",
        "        Major expression: \"A>1\"\n",
        "        Timestamp input B: <undefined>\n",
        "        Input A: 2\n",
        "        Input B: 3\n",
        "          'const': integer 3\n",
    );
    let zero = concat!(
        "JSON links in record 'X'\n\n",
        "  ai record 'X':\n",
        "    Link field 'INP':\n",
        "      'calc': \"A+B\" = 5 mm\n",
    );
    assert!(console.contains(two), "{console}");
    assert!(console.contains(&format!("{zero}\u{1b}")), "{console}");
}

/// lnkState.c:99-105: the bit the link last read (`invert ^ state`) or was
/// last asked to write, `FALSE` before either.
#[test]
fn a_state_link_reports_its_last_value() {
    let (console, files) = run(
        "record(bi, \"S\") { field(INP, {state:\"!foo\"}) }\n\
         record(bi, \"U\") { field(INP, {state:\"!foo\"}) }\n\
         record(bo, \"W\") { field(OUT, {state:\"!bar\"}) }\n",
        "dbpf S.PROC 1\n\
         dbpf W 1\n\
         dbjlr S > {d}/f.txt\n\
         dbjlr U\n\
         dbjlr W\n",
        &["f.txt"],
    );
    assert_eq!(files, [""]);
    for block in [
        "  bi record 'S':\n    Link field 'INP':\n      'state': \"foo\" = ! TRUE\n",
        "  bi record 'U':\n    Link field 'INP':\n      'state': \"foo\" = ! FALSE\n",
        "  bo record 'W':\n    Link field 'OUT':\n      'state': \"bar\" = ! TRUE\n",
    ] {
        assert!(console.contains(block), "{block}\n{console}");
    }
}
