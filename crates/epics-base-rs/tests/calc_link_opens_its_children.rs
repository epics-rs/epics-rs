//! `lnkCalc_open` (`lnkCalc.c:449-466`) opens each child link at `iocInit`
//! and `dbLoadLink`s its value into `arg[i]`, then opens `out`. Expected
//! values are the C oracle's (`oracleStdIoc`, the same database and script).

use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "CALC_LINK_OPENS_ITS_CHILDREN_SCRIPT";

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

/// A const child's value is the one its open loaded, element 0 of an array;
/// a state child's state exists from `iocInit`, so a set before the first
/// read is seen.
#[test]
fn the_children_s_values_are_loaded_at_open() {
    let (_, files) = run(
        "record(ai, \"A1\") { field(INP, {calc:{expr:\"B\", args:[6, {const:[1, 2]}]}}) }\n\
         record(ai, \"S1\") { field(INP, {calc:{expr:\"A\", args:[{state:\"x\"}]}}) }\n",
        "dbStateSet x\n\
         dbpf A1.PROC 1\n\
         dbgf A1 > {d}/a1.txt\n\
         dbpf S1.PROC 1\n\
         dbgf S1 > {d}/s1.txt\n",
        &["a1.txt", "s1.txt"],
    );
    assert_eq!(
        files,
        [
            "DBF_DOUBLE:         1         \n",
            "DBF_DOUBLE:         1         \n"
        ]
    );
}

/// Before any evaluation the report shows what the open loaded, in
/// `lnkConst_loadScalar`'s conversions: a string, element 0 of a string
/// array, `true` as 1, and nothing from `[]`. A nested calc's state child and
/// the `out` link's state are created by the opens too.
#[test]
fn the_report_before_the_first_evaluation_shows_the_loaded_values() {
    let (console, _) = run(
        "record(ai, \"C1\") { field(INP, {calc:{expr:\"A+B\", args:[6, {const:3}]}}) }\n\
         record(ai, \"Q1\") { field(INP, {calc:{expr:\"A+B+C+D\", \
         args:[{const:\"2.5\"}, {const:[\"7\"]}, {const:true}, {const:[]}]}}) }\n\
         record(ao, \"O1\") { field(OUT, {calc:{expr:\"A\", out:{state:\"z\"}, \
         args:[{calc:{expr:\"1\", args:[{state:\"y\"}]}}]}}) }\n",
        "dbjlr C1 1\n\
         dbjlr Q1 1\n\
         dbStateShow y\n\
         dbStateShow z\n",
        &[],
    );
    for block in [
        "  ai record 'C1':\n    Link field 'INP':\n      'calc': \"A+B\" = 0 \n\
         \x20       Input A: 6\n        Input B: 3\n          'const': integer 3\n",
        "  ai record 'Q1':\n    Link field 'INP':\n      'calc': \"A+B+C+D\" = 0 \n\
         \x20       Input A: 2.5\n          'const': string \"2.5\"\n\
         \x20       Input B: 7\n          'const': array of 1 string\n\
         \x20       Input C: 1\n          'const': integer 1\n\
         \x20       Input D: 0\n          'const': bug -- type=4\n",
    ] {
        assert!(console.contains(block), "{block}\n{console}");
    }
    // One `FALSE` per `dbStateShow`: both states exist.
    assert_eq!(
        console.lines().filter(|l| *l == "FALSE").count(),
        2,
        "{console}"
    );
}

/// `dbPutFieldLink`'s `dbAddLink` opens the link a put stores, as `iocInit`
/// opens a loaded one.
#[test]
fn a_link_put_opens_the_link_it_stores() {
    let (console, _) = run(
        "record(ai, \"P1\") {}\nrecord(ai, \"P2\") {}\n",
        "dbpf P1.INP '{calc:{expr:\"A\",args:[{const:5}]}}'\n\
         dbjlr P1 1\n\
         dbpf P2.INP '{state:\"w\"}'\n\
         dbStateShow w\n\
         dbStateShow v\n",
        &[],
    );
    let block = "  ai record 'P1':\n    Link field 'INP':\n      'calc': \"A\" = 0 \n\
                 \x20       Input A: 5\n          'const': integer 5\n";
    assert!(console.contains(block), "{block}\n{console}");
    assert_eq!(
        console.lines().filter(|l| *l == "FALSE").count(),
        1,
        "{console}"
    );
}
