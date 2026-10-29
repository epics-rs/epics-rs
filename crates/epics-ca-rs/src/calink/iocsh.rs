//! iocsh command for `calink` — `dbcaxr`.
//!
//! The CA-link counterpart of the bridge `pvalink::iocsh`: dump CA-link
//! state for a record, after C `dbCa.c`'s `dbcar` debug surface. There is
//! no CA pre-warm command: each record link opens its own channel at
//! `dbCaAddLink`, so a channel opened by name would serve no link.

// RTEMS-EXEC-MODEL-ALLOW(1): the flavored test drives the CA client stack,
// which needs a multi-thread tokio runtime. These run and pass in the
// exec-backend suite on the tokio driver.
use epics_base_rs::server::database::{LinkRef, LinkSet};
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

use super::resolver::CaLinkResolver;

/// `dbcaxr [<recordName>]` — print CA-link debug info. With no
/// argument prints resolver-level stats (open-link count); with a
/// record name walks every link-shaped String field on that record
/// and dumps connection / value / alarm / time state for each
/// ` CA`-modified or non-local link via the registered
/// [`epics_base_rs::server::database::LinkSet`]. The CA-link
/// counterpart of `dbpvxr`.
///
/// `FLNK` is included, and must be: C `dbcar` walks
/// `pdbRecordType->link_ind[j]` — every link field of the record type, with
/// no `dbfType` filter — and prints each one whose `plink->type` is
/// `CA_LINK` (`dbCaTest.c:88-136`). An `FLNK="OTHER.PROC CA"` is such a
/// link (`dbLink.c:118-136` reaches `dbCaAddLink` for `DBF_FWDLINK` too), so
/// hiding it here would under-report exactly the links `dbcar` exists to
/// show.
pub fn db_dbcaxr_command(resolver: CaLinkResolver) -> CommandDef {
    CommandDef::new(
        "dbcaxr",
        vec![ArgDesc {
            name: "record",
            arg_type: ArgType::String,
        }],
        "dbcaxr [<recordName>]",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let target = match args.first() {
                Some(ArgValue::String(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            };
            ctx.println(&format!(
                "dbcaxr: {} cached CA link(s)",
                resolver.link_count()
            ));
            if let Some(rec) = target {
                let links = ctx.db().record_link_fields(&rec);
                if links.is_empty() {
                    ctx.println(&format!(
                        "  '{rec}': no link fields found (or record missing)"
                    ));
                } else {
                    ctx.println(&format!("  '{rec}': {} link field(s)", links.len()));
                    for (field, raw, parsed) in links {
                        if let epics_base_rs::server::record::ParsedLink::Ca(ca) = parsed {
                            // The record's held parse: its link is the one
                            // whose channel is reported.
                            let link = LinkRef::new(ca.pv.as_bytes(), &ca.open);
                            let name = &ca.pv;
                            // A report: the value is peeked, so the link is
                            // neither opened nor monitored by reading it.
                            let r = &resolver;
                            let connected = <CaLinkResolver as LinkSet>::is_connected(r, link);
                            let alarm = <CaLinkResolver as LinkSet>::alarm_severity(r, link);
                            let ts = <CaLinkResolver as LinkSet>::time_stamp(r, link);
                            let value = r.peek_value(link);
                            ctx.println(&format!(
                                "    {field}={raw:?}  {name}  connected={connected}"
                            ));
                            if let Some(v) = value {
                                ctx.println(&format!("        value={}", v.text_g()));
                            }
                            if let Some(sev) = alarm {
                                ctx.println(&format!("        alarmSeverity={sev}"));
                            }
                            if let Some((s, n, _)) = ts {
                                ctx.println(&format!("        timeStamp={s}.{n:09}"));
                            }
                        }
                    }
                }
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Convenience: build the full `calink` iocsh command set bound to
/// `resolver`. Drop the result into
/// [`epics_base_rs::server::ioc_app::IocRunConfig::shell_commands`].
pub fn register_calink_commands(resolver: CaLinkResolver) -> Vec<CommandDef> {
    vec![db_dbcaxr_command(resolver)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_resolver() -> CaLinkResolver {
        CaLinkResolver::new(epics_base_rs::runtime::task::Reactor::current().expect("test runtime"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn register_calink_commands_returns_dbcaxr() {
        let r = dummy_resolver();
        let cmds = register_calink_commands(r);
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["dbcaxr"]);
    }
}
