//! `.db` text generation and loading for this crate's runtime record builders.
//!
//! Every `.db` line `z2m` and `ioc` produce goes through here. They
//! interpolate caller-supplied text — an MQTT topic named in `st.cmd`, an
//! asyn port name — into a *quoted field value*, and the loader reads that as
//! a `jsonSTRING` ([`db_loader::read_field_value`] →
//! `read_json_string`), which ends at the first unescaped `"`. A raw quote
//! there does not corrupt one field: the parse fails, `parse_db` returns
//! `Err` for the whole string, and not one record of the device is created.
//!
//! Record NAMES are deliberately not escaped here. They are `tokenSTRING`
//! (`read_quoted_string`), where C's lexer emits escape sequences verbatim
//! rather than translating them, so escaping a name would corrupt it.

use std::collections::HashMap;
use std::fmt::Write;

use epics_base_rs::server::database::RecordLoad;
use epics_base_rs::server::db_loader;
use epics_base_rs::server::iocsh::registry::CommandContext;

/// Escape `value` for use inside a double-quoted `.db` field value.
///
/// The inverse of `db_loader::read_json_string`, whose `escapedchar` is a
/// backslash followed by anything outside `[ux1-9]`: `\` and `"` are the two
/// characters that change the parse, and the fixed-width `\xHH` / `\uHHHH`
/// forms are never emitted except for the control characters that rule
/// rejects outright.
pub(crate) fn escape_field_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // `read_json_string` refuses any other unescaped control
            // character; `\xHH` reads exactly two hex digits on the way back
            // in, so a hex digit following it in the topic is unambiguous.
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// One record's `.db` text, appended to a shared buffer.
///
/// Exists so that no builder in this crate writes a `field(...)` line by hand
/// — [`Self::field`] is the only way to add one, and it escapes.
pub(crate) struct RecordWriter<'a> {
    out: &'a mut String,
}

impl<'a> RecordWriter<'a> {
    /// Open `record(<record_type>, "<name>")`. `name` is written raw: see the
    /// module note on record names.
    pub(crate) fn new(out: &'a mut String, record_type: &str, name: &str) -> Self {
        let _ = writeln!(out, "record({record_type}, \"{name}\") {{");
        Self { out }
    }

    pub(crate) fn field(&mut self, name: &str, value: &str) -> &mut Self {
        let _ = writeln!(
            self.out,
            "    field({name}, \"{}\")",
            escape_field_value(value)
        );
        self
    }

    pub(crate) fn end(self) {
        let _ = writeln!(self.out, "}}");
    }
}

/// Parse `db_string` and create every record it declares.
///
/// The single owner of the generate-then-load step for this crate. Both
/// `mqttZ2mXxx` and `mqttDriverConfigure` build `.db` text from `st.cmd`
/// arguments, and both must answer a failure the same way: print it, keep
/// going so the remaining records still land, and end in `Err` so `on error`
/// in `st.cmd` fires instead of the IOC booting while `dbl` lists nothing
/// (epics-base#498 / UI-105). `what` prefixes the messages with the iocsh
/// command the operator typed.
pub(crate) fn load_generated_db(
    what: &str,
    db_string: &str,
    ctx: &CommandContext,
) -> Result<(), String> {
    let macros = HashMap::new();
    let mut deferred: Vec<String> = Vec::new();
    let defs = db_loader::parse_db(db_string, &macros)
        .map_err(|e| format!("{what}: parse_db failed: {e}"))?;
    for def in defs {
        match db_loader::create_record(&def.record_type) {
            Ok(mut record) => {
                let mut common_fields = Vec::new();
                if let Err(e) =
                    db_loader::apply_fields(&mut record, &def.fields, &mut common_fields)
                {
                    let msg = format!("{what}: apply_fields for {}: {e}", def.name);
                    eprintln!("{msg}");
                    deferred.push(msg);
                    continue;
                }
                // Record + loaded common fields in one call: the creation sink
                // runs the `iocInit` passes against the FINAL field set (the
                // SCAN="I/O Intr" the .db declares, and the UDF a field(VAL,…)
                // clears), not a pre-load one.
                let load = RecordLoad::from_common_fields(common_fields);
                ctx.block_on(async {
                    if let Err(e) = ctx.db().add_loaded_record(&def.name, record, load).await {
                        let msg = format!("{what}: register '{}' skipped: {e}", def.name);
                        eprintln!("{msg}");
                        deferred.push(msg);
                    }
                });
            }
            Err(e) => {
                let msg = format!("{what}: create_record({}): {e}", def.record_type);
                eprintln!("{msg}");
                deferred.push(msg);
            }
        }
    }
    match deferred.len() {
        0 => Ok(()),
        1 => Err(deferred.remove(0)),
        n => Err(format!("{} (+{} more)", deferred[0], n - 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::db_loader;

    /// The escaper is only correct if the loader gives the text back
    /// unchanged, so assert against the real parser rather than against an
    /// expected escape string.
    fn round_trip(value: &str) -> String {
        let mut db = String::new();
        let mut w = RecordWriter::new(&mut db, "ai", "TEST:RT");
        w.field("INP", value);
        w.end();
        let defs = db_loader::parse_db(&db, &std::collections::HashMap::new())
            .unwrap_or_else(|e| panic!("parse failed for {value:?}: {e}\n{db}"));
        let (_, v) = defs[0]
            .fields
            .iter()
            .find(|(k, _)| k == "INP")
            .expect("INP field");
        v.to_string()
    }

    fn make_ctx() -> (tokio::runtime::Runtime, CommandContext) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = std::sync::Arc::new(epics_base_rs::server::database::PvDatabase::new());
        let bridge = {
            let _guard = rt.enter();
            epics_base_rs::runtime::task::BlockingBridge::capture()
        };
        let ctx = CommandContext::new(db, bridge);
        (rt, ctx)
    }

    /// `mqttDriverConfigure` used to drop every failure on this path and print
    /// its success line regardless, so an IOC booted reporting a Connected PV
    /// that `dbl` never listed. The rule is the one `mqttZ2mXxx` already
    /// followed: a load failure ends the command in `Err`.
    #[test]
    fn test_load_generated_db_reports_a_rejected_record_name() {
        let (_rt, ctx) = make_ctx();

        let mut good = String::new();
        let mut w = RecordWriter::new(&mut good, "bi", "TEST:MQTT:Conn");
        w.field("ZNAM", "Disconnected");
        w.field("ONAM", "Connected");
        w.end();
        load_generated_db("mqttDriverConfigure", &good, &ctx).unwrap();
        assert_eq!(
            ctx.block_on(async { ctx.db().all_record_names().await }),
            vec!["TEST:MQTT:Conn".to_string()]
        );

        // The documented trigger: a space in `connPvName`, which
        // `db_loader::validate_name` rejects.
        let mut bad = String::new();
        let mut w = RecordWriter::new(&mut bad, "bi", "TEST:MQTT Conn");
        w.field("ZNAM", "Disconnected");
        w.end();
        let err = load_generated_db("mqttDriverConfigure", &bad, &ctx).unwrap_err();
        assert!(err.starts_with("mqttDriverConfigure:"), "{err}");
        // Nothing new landed.
        assert_eq!(
            ctx.block_on(async { ctx.db().all_record_names().await })
                .len(),
            1
        );
    }

    #[test]
    fn test_field_value_round_trips_through_the_loader() {
        for value in [
            "@asyn(MQTT1) JSON:FLOAT \"zigbee2mqtt/living room plug\" power",
            "plain",
            "back\\slash",
            "both \" and \\ together",
            "trailing backslash \\",
            "quote at end \"",
            "tab\there",
            "\u{1}control",
        ] {
            assert_eq!(round_trip(value), value, "value {value:?}");
        }
    }
}
