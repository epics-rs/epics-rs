//! The long-string constant load must read the SAME dialect every other JSON
//! link reader reads.
//!
//! C never sees the raw `.db` bytes here: `dbParseLink` hands the brace body
//! to `dbJLinkParse` (`dbStaticLib.c:2280-2282` at R7.0.10), which calls
//! `yajl_alloc` + `yajl_parse` (`dbJLink.c:402-406`) with the default flags
//! `yajl_allow_json5 | yajl_allow_comments` (`yajl.c:77`). So by the time
//! `lnkConst_loadLS` (`lnkConst.c:199`) receives a value, comments are gone
//! and the string is decoded.
//!
//! `load_link_ls` read the raw text instead, so a comment inside the link body
//! made the whole const load vanish — LEN stayed at the empty-string 1 and no
//! diagnostic said why.
//!
//! The comment forms are handed to `load_link_ls` directly: they cannot come
//! from a `.db`, whose `JSON` start condition refuses `/` before
//! `dbJLinkParse` sees the text (`dbLex.l:143-155`; see
//! `json_state_invalid_character_is_a_lexer_error`).

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{LsLoad, load_link_ls};
use epics_base_rs::types::EpicsValue;

async fn val_len(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> (String, i64) {
    let inst = db.get_record(rec).unwrap_or_else(|| panic!("{rec}"));
    let inst = inst.read();
    let v = match inst.record.get_field("VAL") {
        Some(EpicsValue::CharArray(b)) => {
            String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or_default()).into_owned()
        }
        Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
        other => panic!("{rec}.VAL: {other:?}"),
    };
    let len = match inst.record.get_field("LEN") {
        Some(EpicsValue::ULong(n)) => n as i64,
        Some(EpicsValue::Long(n)) => n as i64,
        other => panic!("{rec}.LEN: {other:?}"),
    };
    (v, len)
}

#[epics_macros_rs::epics_test]
async fn a_comment_inside_a_const_link_does_not_erase_the_long_string_load() {
    let db_text = concat!(
        r#"record(lsi,"LS:PLAIN") { field(INP,{const:"hi there"}) }"#,
        "\n",
    );
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let plain = val_len(&db, "LS:PLAIN").await;
    assert_eq!(
        plain,
        ("hi there".to_string(), 9),
        "measured on softIoc 7.0.10: VAL \"hi there\", LEN 9"
    );
    for text in [
        r#"{const:/*which*/"hi there"}"#,
        r#"{/*t*/const:"hi there"}"#,
    ] {
        assert_eq!(
            load_link_ls(text),
            Some(LsLoad::Text("hi there".into())),
            "a comment is whitespace to yajl, so the value is the same string: {text}"
        );
    }
}
