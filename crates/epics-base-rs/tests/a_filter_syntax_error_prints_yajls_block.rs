//! `chf_parse` prints yajl's verbose block on a syntax error
//! (`dbChannel.c:291-296`), and the first refusal left to right is the one
//! printed: an unknown filter name cancels the parse before yajl reaches a
//! later syntax error, and so does a plugin's refusal (`chfPlugin.c`), which
//! prints nothing. softIoc R7.0.10 with `record(ai,"F")`, `caget` of each
//! name below, one IOC per name; the expected text is its console verbatim.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush, errlog_strip_ansi};
use epics_base_rs::server::database::filters::db_channel_create;
use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn each_refusal_prints_cs_text() {
    let heard = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push_str(&errlog_strip_ansi(m)));
    let (db, _) = IocBuilder::new()
        .db_string("record(ai, \"F\") {}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let block = |kind: &str, window: &str| {
        format!("dbChannelCreate: {kind}\n{window}\n                     (right here) ------^\n\n")
    };
    let cases = [
        (
            r#"F.{"dbnd":{"d":1}} junk"#,
            block(
                "parse error: trailing garbage",
                r#"                      {"dbnd":{"d":1}} junk"#,
            ),
        ),
        (
            r#"F.{"ts":{}"#,
            block(
                "parse error: premature EOF",
                r#"                                       {"ts":{}"#,
            ),
        ),
        (
            "F.{]",
            block(
                "lexical error: invalid char in json text.",
                "                                      {]",
            ),
        ),
        (
            r#"F.{"dec":{"n":1},"ts":{} x"#,
            block(
                "lexical error: invalid char in json text.",
                r#"                {"dec":{"n":1},"ts":{} x"#,
            ),
        ),
        (
            r#"F.{"x":1"#,
            "dbChannelCreate: Channel filter 'x' not found\n".to_string(),
        ),
        // chfPlugin's own refusal stops the parse first, and it prints nothing.
        (r#"F.{"dbnd":{"q":1}} junk"#, String::new()),
        (r#"F.{"dbnd":{"d":[1]}} junk"#, String::new()),
        (r#"F.{"dbnd":{"d":null} x"#, String::new()),
        (r#"F.{"dec":{}} x"#, String::new()),
        (r#"F.{"dec":{"n":"x"}} junk"#, String::new()),
    ];
    for (name, want) in cases {
        heard.lock().unwrap().clear();
        assert!(db_channel_create(&db, name).is_err(), "{name}");
        errlog_flush();
        assert_eq!(*heard.lock().unwrap(), want, "{name}");
    }
}
