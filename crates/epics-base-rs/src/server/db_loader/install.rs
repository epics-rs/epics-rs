//! The per-record install C's parser performs as it reduces each record:
//! `dbRecordHead`, `dbRecordField`, `dbRecordInfo`, `dbRecordAlias` and
//! `dbAlias` (`dbLexRoutines.c:1123-1530`), shared by every load route —
//! iocsh `dbLoadRecords`/`dbLoadTemplate`/`dbLoadDatabase` and
//! [`crate::server::ioc_builder::IocBuilder`] — so a record reads the same
//! whichever route loaded it.

use crate::runtime::log::{ERL_ERROR, ERL_WARNING};
use crate::server::database::{PvDatabase, RecordLoad};
use crate::server::record::Record;
use crate::types::EpicsValue;

use super::{DbFaults, DbFileAlias, DbRecordDef, ParseAbort};

/// What a load route supplies to the shared install.
pub(crate) trait InstallHost {
    fn db(&self) -> &PvDatabase;
    /// C `dbRecordsOnceOnly` as this route reads it.
    fn records_once_only(&self) -> bool;
    /// C `dbFindRecordType` and `dbAllocRecord` for a name not yet in the
    /// database; `None` when this route has no support for the type.
    fn create_record(&self, record_type: &str) -> Option<Box<dyn Record>>;
    /// One of C's bare `fprintf(stderr, ...)` lines.
    fn warn(&self, line: &str);
}

/// The load's status is non-zero: every diagnostic is already printed.
#[derive(Debug)]
pub(crate) struct LoadRejected;

/// C `dbAlias` (`dbLexRoutines.c:1501-1520`) at its own line: the target is
/// looked up in `savedPdbbase` as it stands there, so a record declared
/// further down the file is as unknown as one never declared.
async fn install_file_alias<H: InstallHost>(
    host: &H,
    file_alias: DbFileAlias,
    faults: &mut DbFaults,
) {
    let (target, alias) = (&file_alias.target, &file_alias.alias);
    faults.read_through(alias.line);
    if host.db().get_record(target).is_none() {
        faults.report_at(
            alias.line,
            super::DbDiagnostic::new(super::unknown_alias_message(&alias.name, target)),
        );
        return;
    }
    super::create_alias(
        host.db(),
        &alias.name,
        target,
        alias.line,
        host.records_once_only(),
        faults,
    )
    .await;
}

/// Put each field the way C's parser does — one element at a time, so a value
/// the applier refuses costs that FIELD and nothing else.
///
/// C reduces one `field(NAME,"value")` at a time, and every failure arm of
/// `dbRecordField` ends in `yyerror(NULL)` and `return`
/// (`dbLexRoutines.c:1405-1416` @`R7.0.10-146-g8f5015b663d764ad75df`):
///
/// ```c
///     status = dbPutString(pdbentry,value);
///     if (status) {
///         ...
///         yyerror(NULL);
///         return;
///     }
/// ```
///
/// so the record stays, its remaining fields still load, and only the file's
/// status goes non-zero. Handing the whole slice to [`super::apply_fields`]
/// made the FIRST refusal cost the record and every field behind it as well:
/// measured against `softIoc` R7.0.10 on a `record(ai,"N1"){field(RSET,"1")}`,
/// C's `dbl` listed `N1` and the port's listed nothing.
///
/// Splitting the call is sound because `apply_fields` is itself a loop over
/// `fields` that carries nothing between iterations — `check_link_syntax` is
/// per field and `common_fields` is append-only — so N one-field calls are the
/// same load as one N-field call, minus the abort.
///
/// That makes the rule uniform: a screened refusal and the applier's own —
/// `DBF_NOACCESS`, an unparsable value, a link the syntax check rejects — each
/// cost the field and keep the record, rather than each apply site deciding for
/// itself how much of the file a bad field costs.
///
/// The screen, the loop and the sentence are
/// [`super::apply_fields_one_at_a_time`]'s; this site only decides where the
/// report goes.
fn apply_fields_one_at_a_time<T: super::LoadTarget + ?Sized>(
    links: &crate::server::record::LinkList,
    record: &mut T,
    record_name: &str,
    fields: &[super::DbFieldDef],
    common_fields: &mut Vec<(String, EpicsValue)>,
    faults: &mut DbFaults,
) {
    super::apply_fields_one_at_a_time(
        links,
        record,
        record_name,
        fields,
        common_fields,
        |f, e, diagnostic| {
            // C's lexer names the line; the parse is over by the time this
            // runs, so the element's own recorded line takes that seat.
            faults.seek(f.line, ")");
            // `dbParseLink` prints while `dbPutString` runs, before
            // `dbRecordField` reports the status it returned.
            if let Some(e) = e {
                faults.defer_put_report(f.line, e);
            }
            faults.report(diagnostic);
        },
    );
}

/// How C classifies a per-record `.db` failure. `dbRecordHead` and
/// `dbRecordField` (`dbLexRoutines.c:1123-1197`, `:1199-1380`) reach for
/// `yyerror(NULL)` when the record can be skipped and the rest of the file
/// still read, and for `yyerrorAbort` only when the parse cannot continue
/// — an unknown record type or a `dbCreateRecord` that failed for any
/// other reason. The port had one class, so the first skippable record
/// discarded every record after it.
enum RecordFault {
    /// C `yyerror(NULL)`: report, skip this record, keep loading.
    Recoverable(String),
    /// C `yyerrorAbort`: stop the load here.
    Fatal(String),
}

/// Install each parsed record definition into the database through the
/// one routine every load route shares: duplicate-name merge, field
/// application, the creation sink's load-then-init ordering, alias
/// registration, and the post-load link / SIMM / watchdog init passes.
/// `Err` is the load's non-zero status, every diagnostic already printed
/// (epics-base 144f975).
pub(crate) async fn install_records<H: InstallHost>(
    host: &H,
    defs: Vec<DbRecordDef>,
    file_aliases: Vec<DbFileAlias>,
    aborted: Option<ParseAbort>,
    breaktable_registry: &crate::server::cvt_bpt::BreakTableRegistry,
    faults: &mut DbFaults,
) -> Result<(), LoadRejected> {
    // Non-fatal per-record failures (alias reject, merge-field put) join
    // whatever the parse itself recovered from, in the one owner that
    // decides what recoverable means: the load continues past them, but
    // the command must still end in Err so `on error` fires — C's
    // dbLoadRecords returns non-zero after its parser recovered and kept
    // going (epics-base#498 / UI-105).
    let mut file_aliases = file_aliases.into_iter().peekable();
    for (index, mut def) in defs.into_iter().enumerate() {
        while let Some(file_alias) = file_aliases.next_if(|a| a.after <= index) {
            install_file_alias(host, file_alias, faults).await;
        }
        // C's lexer has read up to this record's head, and with it every
        // line before: their notices and refusals print now, in line order.
        // A fault about the record itself is C's `dbRecordHead`'s, parked on
        // the head's `)`.
        faults.read_through(def.line);
        faults.seek(def.line, ")");
        // C `dbRecordHead` (`dbLexRoutines.c:1136-1157`) reads two record
        // types as instructions rather than types: `*` modifies the record
        // already in the database and `#` deletes it. Both skip the body
        // when the name is unknown (`duplicate = TRUE`), and they report
        // it differently — `*` with `yyerror(NULL)`, `#` with a bare
        // WARNING that leaves the load's status clean. The port passed
        // both to the record factory, so every such block failed as an
        // unknown record type and took the load with it.
        if def.record_type == "#" {
            if !host.db().remove_record(&def.name).await {
                // C also names the file and line here; the port does not
                // carry either as far as the install loop.
                host.warn(&format!(
                    "{ERL_WARNING}: Record '{}' not found, can't delete",
                    def.name
                ));
            }
            continue;
        }
        let modify_block = def.record_type == "*";
        let existing_type = host
            .db()
            .get_record(&def.name)
            .map(|rec| rec.read().record.record_type().to_string());
        // A duplicate of another type is refused with `yyerror(NULL)` but,
        // unlike every other refusal in `dbRecordHead`, without `duplicate =
        // TRUE` (`dbLexRoutines.c:1171-1177`): `pdbentry` is left on the
        // existing record, so the body is applied to it.
        let mut retyped = false;
        match existing_type {
            // Continue as the type already there: from here on this is the
            // same field merge a same-type reload performs, which is what C's
            // pdbentry does after `dbFindRecord` / `dbCreateRecord`.
            Some(t) if t != def.record_type => {
                if !modify_block {
                    faults.recoverable(format!(
                        "{ERL_ERROR}: {} record '{}' already exists, can't load {t} record",
                        def.record_type, def.name
                    ));
                    retyped = true;
                }
                def.record_type = t;
            }
            Some(_) => {}
            None if modify_block => {
                faults.recoverable(format!("{ERL_ERROR}: Record '{}' not found", def.name));
                continue;
            }
            None => {}
        }

        // Resolve a `LINR` field naming a loaded breakpoint table to its
        // menuConvert index (shared with the IocBuilder load path).
        super::resolve_linr_breaktable_names(
            &def.record_type,
            &mut def.fields,
            breaktable_registry,
        );

        // C creates the record and only THEN puts each field
        // (`dbCreateRecord` at `dbLexRoutines.c:1172`, `dbPutString` at
        // `:1405`), so a value `dbPutString` refuses costs that FIELD its
        // value and nothing else: the record stays and keeps the default,
        // its other fields load, the rest of the file is read, and the
        // load's status goes non-zero only at the end (`yyerror`, `:1415`).
        // `apply_fields_one_at_a_time` screens each field before applying
        // it, on both the fresh and the merge arm below.
        let added: Result<(), RecordFault> = async {
            // C-parity (dbLexRoutines.c:1170-1188): the SAME record name
            // re-loaded merges fields into the existing instance (the
            // standard ADCore convention — simDetector.template overrides
            // ColorMode menu choices declared by its included
            // NDArrayBase.template). A duplicate of another type was
            // reported and retyped above.
            let existing = if let Some(rec) = host.db().get_record(&def.name) {
                // `dbRecordsOnceOnly` (`dbLexRoutines.c:1181-1187`) refuses
                // the SECOND declaration once the type matched, and sets
                // `duplicate` so the body is dropped whole — fields, info
                // tags and aliases alike. Returning here is that drop.
                //
                // A `record("*", …)` block is exempt because C's `*` arm
                // returns from `dbRecordHead` before `dbCreateRecord`
                // (`:1136-1144`), so it never reaches the flag: modifying
                // an existing record is what `*` is FOR, and refusing it
                // would make the knob mean two different things.
                //
                // The mismatched-type refusal is the `if` this flag's check is
                // the `else` of, so a retyped body is not refused twice.
                if host.records_once_only() && !modify_block && !retyped {
                    return Err(RecordFault::Recoverable(format!(
                        "{ERL_ERROR}: Record '{}' already defined; dbRecordsOnceOnly is set,\n  \
                         so can't modify record.",
                        def.name
                    )));
                }
                Some(rec)
            } else {
                None
            };

            let mut common_fields = Vec::new();
            let is_merge = existing.is_some();
            let rec_arc = if let Some(rec_arc) = existing {
                // Merge: apply field overrides directly to the
                // existing record instance.
                {
                    let mut inst = rec_arc.write();
                    apply_fields_one_at_a_time(
                        &host.db().link_list(),
                        &mut *inst,
                        &def.name.to_str_lossy(),
                        &def.fields,
                        &mut common_fields,
                        faults,
                    );
                }
                rec_arc
            } else {
                // C `dbFindRecordType` failing is `yyerrorAbort`
                // (`dbLexRoutines.c:1162-1167`) — an unknown record type
                // stops the load on both sides.
                //
                // The sentence is built HERE because C builds it here, out
                // of the two things only this site holds: the type and the
                // RECORD NAME. The factory sees neither, so forwarding its
                // error put `DB parse error at line 0, column 0: unknown
                // record type: 'nosuchtype'` on the stream — this port's own
                // words, and a position it did not have. The factory's
                // `register_record_type` hint is dropped with it.
                //
                // A registered factory is C's `.dbd` and its
                // `registerRecordDeviceDriver` together, so a type without
                // one is a type no `.dbd` declared: never `rec_size = 0`.
                let mut record = host.create_record(&def.record_type).ok_or_else(|| {
                    RecordFault::Fatal(format!(
                        "{ERL_ERROR}: Record type '{}' for record '{}' not found",
                        def.record_type, def.name
                    ))
                })?;
                // The breakpoint-table registry is installed by the
                // creation sink; apply_fields only needs the LINR
                // index, already resolved above.
                apply_fields_one_at_a_time(
                    &host.db().link_list(),
                    &mut record,
                    &def.name.to_str_lossy(),
                    &def.fields,
                    &mut common_fields,
                    faults,
                );
                // Record + its whole loaded field set in one call: the
                // sink applies the common fields and info tags, THEN
                // runs C's `iocInit` passes, so the initial UDF
                // severity sees the `.db`'s final UDF (a
                // `field(VAL,…)` clears it — `dbStaticLib.c:2653`).
                let load = RecordLoad {
                    common_fields: std::mem::take(&mut common_fields),
                    info_tags: def.info_tags.clone(),
                };
                if host
                    .db()
                    .add_loaded_record(&def.name, record, load)
                    .await
                    .is_err()
                {
                    // C `dbCreateRecord` failing for anything other than
                    // `S_dbLib_recExists` is `yyerrorAbort`
                    // (`dbLexRoutines.c:1189-1194`), with the same
                    // site-built shape as the type miss above.
                    return Err(RecordFault::Fatal(format!(
                        "{ERL_ERROR}: Can't create {} record '{}'",
                        def.record_type, def.name
                    )));
                }
                host.db().get_record(&def.name).ok_or_else(|| {
                    // No C counterpart: this is the port's own invariant
                    // (the creation sink installed it) failing. It wears
                    // the `ERROR:` prefix because it now leaves by the same
                    // exit as the two above.
                    RecordFault::Fatal(format!(
                        "{ERL_ERROR}: dbLoadRecords: '{}' vanished between add_record and get_record",
                        def.name
                    ))
                })?
            };

            // Register any aliases declared in the record body
            // (epics-base PR #336). Failures are reported but
            // don't abort the load — the record is already in.
            // For a merge, aliases declared in the new block
            // are also registered (C parser appends).
            for alias in &def.aliases {
                super::create_alias(
                    host.db(),
                    &alias.name,
                    &def.name,
                    alias.line,
                    host.records_once_only(),
                    faults,
                )
                .await;
            }

            // A record created during THIS load — fresh, or a merge whose
            // original was created during the load — has its whole init deferred
            // to C's `initDatabase` pass (`drain_deferred_record_inits` at
            // `iocInit`), so none of the init tail below runs here for it: doing
            // so would bind the dset against ports not yet configured and out of
            // C's order. Only a merge into a record created BEFORE the load
            // (programmatically, already eager-inited) re-inits in place.
            let init_deferred = host.db().record_init_deferred(&def.name);

            // A MERGE re-applies the new block's fields to a record that
            // is already in the database and already initialised, so
            // the load-then-init ordering the creation sink guarantees
            // has to be re-created by hand here: fields first, passes
            // after. A fresh record took that ordering from the sink
            // and must not run the passes twice.
            if is_merge {
                {
                    let mut instance = rec_arc.write();
                    // info(key, value) directives — last write
                    // wins. Populated before common-field application
                    // so device support seeing `init_record` can
                    // observe info tags.
                    for (k, v) in &def.info_tags {
                        instance.set_info(k, v);
                    }
                }
                for (name, value) in common_fields {
                    use crate::server::record::CommonFieldPutResult;
                    // `.db` load: C's loader converter, whose menu
                    // bound differs from a runtime dbPut's
                    // (`dbStaticRun.c::dbPutStringNum`).
                    // The record data lock is scoped per field so it is
                    // down before `update_scan_index` re-enters the
                    // database — the same rule as `field_io`'s put path;
                    // the data lock is not the processing-exclusion
                    // mechanism, so the per-field release is a bounded,
                    // `.await`-free window.
                    let mut link =
                        crate::server::record::PreparedCommonLink::for_put(&name, &value);
                    let put = rec_arc
                        .write()
                        .put_common_field_db_load_prepared(&name, value, &mut link);
                    drop(link);
                    match put {
                        Ok(CommonFieldPutResult::ScanChanged {
                            old_scan,
                            new_scan,
                            phas,
                        }) => {
                            host.db()
                                .update_scan_index(def.name.as_bytes(), old_scan, new_scan, phas, phas);
                        }
                        Ok(CommonFieldPutResult::PhasChanged {
                            scan,
                            old_phas,
                            new_phas,
                        }) => {
                            host.db()
                                .update_scan_index(def.name.as_bytes(), scan, scan, old_phas, new_phas);
                        }
                        Ok(CommonFieldPutResult::NoChange) => {}
                        Err(e) => {
                            faults.recoverable(format!(
                                "put_common_field({name}) failed for {}: {e}",
                                def.name
                            ));
                        }
                    }
                }
                // C `iocInit` calls init_record once per record AFTER all
                // dbLoadRecords blocks, so init_record always sees the final
                // merged field set. A merge into a record created during this
                // load is left for that barrier pass (`init_deferred`); a merge
                // into a record created BEFORE the load re-runs init in place so
                // the new field values (LINR / ESLO / ZRST / ...) take effect in
                // convert routines and post-init derived state. The cost of the
                // in-place re-run: stateful records (compress accum,
                // first_output_done) get re-initialised — still better than
                // silently ignoring field overrides that affect init.
                if !init_deferred {
                    let input = crate::server::builtin_devices::soft_callback::add_record_input_of(
                        host.db(),
                        &rec_arc,
                        None,
                    );
                    let reached = {
                        let mut inst = rec_arc.write();
                        let reached = inst.run_init_pass0(&def.name.to_str_lossy());
                        if reached {
                            inst.run_add_record(&def.name.to_str_lossy(), input);
                        }
                        reached
                    };
                    // Pass 1's link reads take other records' locks, so they
                    // run with this record's lock down, as the barrier's do.
                    let mut reads = reached.then(|| host.db().read_init_links(&rec_arc));
                    let mut inst = rec_arc.write();
                    if let Some(reads) = &mut reads {
                        inst.run_init_pass1(&def.name.to_str_lossy(), reads);
                    }
                    rec_arc.settle_addr_facts(&inst);
                }
            }
            // C's `init_record` checkLinks (`init_links`) and the constant-INP
            // seed / `dbLoadLinkArray` (`rec_gbl_init_constant_links`). For a
            // record whose init is deferred to `iocInit`, the barrier pass owns
            // both; only an in-place re-init (pre-load record merged into) runs
            // them here.
            if !init_deferred {
                {
                    let mut instance = rec_arc.write();
                    let inst = &mut *instance;
                    inst.record.init_links(&inst.common);
                }
                host.db().rec_gbl_init_constant_links(&rec_arc);
                if is_merge {
                    // A merge re-ran the init passes above against the new
                    // field set, so the rest of pass 1 owes a re-run too: a
                    // second block may have introduced SIML/SIOL or moved SDEL.
                    // A FRESH record takes both from the creation sink
                    // (`PvDatabase::add_loaded_record`) and must not repeat them
                    // here — `arm_watchdog` would spawn a second task and
                    // supersede the first for nothing.
                    host.db().rec_gbl_init_simm(&rec_arc);
                    host.db().arm_watchdog(def.name.as_bytes());
                }
            }
            Ok(())
        }
        .await;
        match added {
            Ok(()) => {}
            // The load's status still goes non-zero (epics-base 144f975:
            // the `iocshSetError` equivalent, so a startup script fails),
            // but the records already installed stay and the ones after
            // this record are still read.
            Err(RecordFault::Recoverable(msg)) => {
                faults.report_at(def.line, super::DbDiagnostic::new(msg))
            }
            Err(RecordFault::Fatal(msg)) => {
                // C `yyerrorAbort(NULL)` (`dbYacc.y:385-389`) is
                // `yyerror(NULL)` plus `yyAbort = TRUE`: the sentence is
                // the site's own `fprintf`, and the abort adds exactly the
                // bare `ERROR: ` and the position that a RECOVERED fault
                // gets. So the two arms print through the SAME owner and
                // differ only in whether the loop goes on — which is what
                // makes it impossible for an abort to reach the operator
                // without the file, line and source echo the rest carry.
                // This arm used to `ctx.println` the message instead:
                // stdout, unframed, and invisible to a `2>` startup log.
                faults.abort_at(def.line, super::DbDiagnostic::new(msg));
                faults.stop_reading();
                return Err(LoadRejected);
            }
        }
    }
    for file_alias in file_aliases {
        install_file_alias(host, file_alias, faults).await;
    }
    // C reports a parse abort from inside `yyparse`, after every record it
    // had reduced is in `pdbbase`.
    if let Some(abort) = aborted {
        faults.abort_parse(&abort);
        return Err(LoadRejected);
    }
    faults.read_to_end();

    if faults.is_empty() {
        Ok(())
    } else {
        Err(LoadRejected)
    }
}
