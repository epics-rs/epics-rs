//! `dbStaticLib.c` dumps of std's contributions, against `oracleStdIoc`
//! (`~/work/epics-modules/oracle-ioc`: base + std + calc, R7.0.10.1-DEV).
//! Addresses are masked on both sides; `(nil)` is compared.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

mod common;
use common::masked;

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — all 8 ran and passed on
// the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// std-rs --all-features --test dbstatic_dumps_match_oracle`). The runtime only
// feeds `BlockingBridge::capture` for the shell; no CA server is built.
fn shell() -> IocShell {
    std_rs::register_std_dbd();
    // devTimeOfDay.dbd's ai/stringin lines arrive when its supports are
    // registered.
    let mut builder = epics_base_rs::server::ioc_builder::IocBuilder::new();
    for (record_type, devsup, factory) in std_rs::std_device_supports() {
        builder = builder.register_device_support(record_type, devsup, factory);
    }
    drop(builder);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    std::mem::forget(rt);
    IocShell::new(Arc::new(PvDatabase::new()), bridge)
}

fn run(shell: &IocShell, line: &str) -> String {
    let out = tempfile::NamedTempFile::new().unwrap();
    shell
        .execute_line(format!("{line} > \"{}\"", out.path().display()))
        .unwrap();
    std::fs::read_to_string(out.path()).unwrap()
}

/// oracleStdIoc's `dbDumpDevice` block for `record_type`.
fn c_device_block(record_type: &str) -> String {
    let head = format!("recordtype({record_type})\n");
    blocks(include_str!("oracle/dbDumpDevice.txt"), "recordtype(")
        .into_iter()
        .find(|b| b.starts_with(&head))
        .unwrap()
        .to_string()
}

#[test]
fn std_device_rows_print_c_s_devsup_blocks_dbstaticlib_c_3444() {
    let shell = shell();
    for record_type in ["ai", "stringin", "epid"] {
        assert_eq!(
            masked(&run(&shell, &format!("dbDumpDevice pdbbase {record_type}"))),
            masked(&c_device_block(record_type)),
            "{record_type}"
        );
    }
}

/// oracleIoc's block, since oracleStdIoc does not load the asyn record.
#[test]
fn the_asyn_record_device_prints_c_s_devsup_block_dbstaticlib_c_3444() {
    let shell = shell();
    assert_eq!(
        masked(&run(&shell, "dbDumpDevice pdbbase asyn")),
        "recordtype(asyn)
    device name:   asynRecordDevice
\tchoice:    asynRecordDevice
\tlink_type: 12
\tpdset:     %p
\t    number: 5
\t    func 0: (nil) - report()
\t    func 1: (nil) - init()
\t    func 2: (nil) - init_record()
\t    func 3: %p - get_ioint_info()
\t    func 4: (nil)
\tpdsxt:     (nil)
"
    );
}

/// C's text without the C struct layout `dbDumpRecordType` prints: the
/// `offset` and `size` columns and the closing `rset * %p rec_size %d` line.
fn without_struct_layout(c: &str) -> String {
    let mut out = String::new();
    for line in c.lines() {
        if line.starts_with("rset ") {
            continue;
        }
        let line = match line.split_once('\t') {
            Some((left, right)) => match left.split_whitespace().collect::<Vec<_>>()[..] {
                ["index", "offset", "size", "name"] => format!("index name\t{right}"),
                [index, _offset, _size, name] => format!("{index:>5} {name}\t{right}"),
                _ => line.to_string(),
            },
            None => line.to_string(),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[test]
fn a_std_record_type_dumps_its_field_table_dbstaticlib_c_3304() {
    let shell = shell();
    assert_eq!(
        run(&shell, "dbDumpRecordType pdbbase epid"),
        without_struct_layout(include_str!("oracle/dbDumpRecordType_epid.txt"))
    );
}

/// C's text without the six `dbDumpField` lines the port has no datum for
/// (the four `.dbd` attributes the emitter drops and the C struct layout) and
/// without the `Attribute:` block: `PvDatabase` seeds RTYP/VERS for base's
/// record types only.
fn without_undeclared_lines(c: &str) -> String {
    c.lines()
        .filter(|line| {
            let key = line.trim_start().split(':').next().unwrap_or("");
            if key == "Attribute" {
                return false;
            }
            !matches!(
                key,
                "prompt" | "extra" | "base" | "promptgroup" | "size" | "offset"
            )
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

#[test]
fn a_std_record_type_dumps_its_fields_dbstaticlib_c_3346() {
    let shell = shell();
    for (field, c) in [
        ("KP", include_str!("oracle/dbDumpField_epid_KP.txt")),
        ("INP", include_str!("oracle/dbDumpField_epid_INP.txt")),
    ] {
        assert_eq!(
            run(&shell, &format!("dbDumpField pdbbase epid {field}")),
            without_undeclared_lines(c),
            "{field}"
        );
    }
}

#[test]
fn std_menus_join_the_menu_dump_in_c_order_dbstaticlib_c_3295() {
    let shell = shell();
    assert_eq!(
        run(&shell, "dbDumpMenu pdbbase epidFeedbackMode"),
        "menu(epidFeedbackMode) {
\tchoice(epidFeedbackMode_PID,\"PID\")
\tchoice(epidFeedbackMode_MaxMin,\"Max/Min\")
}
"
    );
    // rs's base tables also carry asyn's and swait's menus, which
    // oracleStdIoc does not load; the rest is C's whole dump, order included.
    let c = include_str!("oracle/dbDumpMenu.txt");
    let rs: String = run(&shell, "dbDumpMenu pdbbase")
        .split_inclusive("}\n")
        .filter(|block| {
            let name = block.trim_start_matches("menu(").split(')').next().unwrap();
            c.contains(&format!("menu({name}) {{"))
        })
        .collect();
    assert_eq!(rs, c);
}

#[test]
fn a_record_type_without_devices_prints_its_header_dbstaticlib_c_3452() {
    let shell = shell();
    // `printf("recordtype(%s)\n")` precedes the devList walk in C.
    for record_type in ["throttle", "timestamp", "calc", "fanout", "seq"] {
        assert_eq!(
            run(&shell, &format!("dbDumpDevice pdbbase {record_type}")),
            format!("recordtype({record_type})\n")
        );
    }
}

/// `text` cut into one block per record type, each opening with a line that
/// starts with `head`.
fn blocks<'a>(text: &'a str, head: &str) -> Vec<&'a str> {
    let mut starts: Vec<usize> = text.match_indices(head).map(|(i, _)| i).collect();
    starts.retain(|&i| i == 0 || text.as_bytes()[i - 1] == b'\n');
    starts.push(text.len());
    starts.windows(2).map(|w| &text[w[0]..w[1]]).collect()
}

/// rs's blocks for the record types C also loads: rs's base tables carry
/// asyn's `asyn`/`busy` and calc's `swait`, which oracleStdIoc does not load.
fn of_c_types(rs: &str, c: &str, head: &str) -> String {
    let c_heads: Vec<&str> = blocks(c, head)
        .iter()
        .map(|b| b.lines().next().unwrap().split(')').next().unwrap())
        .collect();
    blocks(rs, head)
        .into_iter()
        .filter(|b| c_heads.contains(&b.lines().next().unwrap().split(')').next().unwrap()))
        .collect()
}

#[test]
fn the_whole_device_dump_walks_types_in_c_order_dbstaticlib_c_3444() {
    let shell = shell();
    let c = include_str!("oracle/dbDumpDevice.txt");
    let rs = run(&shell, "dbDumpDevice pdbbase");
    assert_eq!(masked(&of_c_types(&rs, c, "recordtype(")), masked(c));
}

#[test]
fn the_whole_record_type_dump_walks_types_in_c_order_dbstaticlib_c_3304() {
    let shell = shell();
    let c = without_struct_layout(include_str!("oracle/dbDumpRecordType.txt"));
    let rs = run(&shell, "dbDumpRecordType pdbbase");
    assert_eq!(of_c_types(&rs, &c, "name("), c);
}
