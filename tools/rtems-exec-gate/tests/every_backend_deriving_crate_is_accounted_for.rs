//! The exec-backend census, in the configuration that is not the exec backend
//! — which is every configuration anyone runs by default.
//!
//! Each backend-deriving crate already carries its own
//! `tests/rtems_exec_model_gate.rs`, and each of those is
//! `#![cfg(exec_backend)]`. That gating is deliberate and stays:
//! it is what puts the census in the configuration whose reactor it reasons
//! about, and what makes a site vouched for by a bumped `ALLOW(N)` count still
//! have to pass the suite it is vouched for in.
//!
//! But it is also a narrowing, and it narrows the census out of existence in
//! the default build. Measured: two reactor-dependent sites went into
//! `epics-ca-rs/src/repeater.rs` and `cargo nextest run -p epics-ca-rs`
//! reported 927 passed with the census not compiled at all; the breach
//! surfaced only when someone happened to run the exec-backend suite. A guard
//! that disappears with the flag it describes cannot be the thing anyone
//! trusts instead of re-reading the code.
//!
//! So the audit also runs from here, where nothing gates it away. The scan
//! itself needs no feature: `audit_source` evaluates the `cfg` predicates as
//! text — `eval(pred, Truth)` — rather than asking the compiler what is on, so
//! its answer is a property of the source and is the same in both
//! configurations.
//!
//! # Nothing here is a floor
//!
//! Every quantity this file used to defend was a floor under a growing number,
//! and a floor under a growing number goes inert rather than stale: it stops
//! being able to fail long before anyone notices it stopped. `MIN_CRATES = 5`
//! was written when five crates declared the feature; by the time the feature
//! was removed, twenty-three build scripts derived the backend cfg, so
//! eighteen of them could have left the census and the floor would still have
//! held. The per-crate file
//! floors were the same shape — `epics-pva-rs` sat at 156 against a floor of
//! 150.
//!
//! So the crate set is DERIVED and the derivation is checked against a second
//! thing measured from the tree, never against a number:
//!
//! * the members come from the workspace manifest's own globs, so a new member
//!   root is covered on the commit that adds it and an unsupported glob shape
//!   fails rather than silently matching nothing;
//! * a crate is in the set if its `build.rs` emits the `tokio_backend` cfg,
//!   which is the crate actually taking part in the decision the census
//!   reasons about — not, as this used to read, a manifest key that only ever
//!   proved a name existed;
//! * every crate whose own sources gate on `exec_backend`/`tokio_backend` must
//!   be in that set, which is the reverse direction and the one that catches a
//!   crate gating on a cfg nothing emits for it — code compiled away on every
//!   target, silently;
//! * and every crate in the set must carry `CANONICAL_DERIVATION` verbatim, so
//!   23 copies of one rule cannot drift and none can lose its
//!   `cargo::rerun-if-env-changed` line.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every file that takes at least one reactor-dependent site out of the census
/// with a file-level `#![cfg(..)]` or a gated column-0 `mod`, named by the
/// crate it lives in and its path below that crate's manifest.
///
/// Read the name literally: this counts files that CONTAIN a gate, never files
/// a gate removes. A parent's `#[cfg(<gate>)] mod foo;` deletes `foo.rs`
/// entirely while leaving nothing inside `foo.rs` to see, so such a file is
/// absent here even though gating is what removed it. Its reactor-dependent
/// sites still have to carry census markers, so this is not a silent bypass —
/// but anyone totalling what gating costs has to measure that shape separately,
/// and it is a shape a single branch can hold none of while the merged tree
/// holds many.
///
/// Pinned here rather than left to the marker mechanism, because scope gating
/// is the one accounting a diff does not show. A census marker sits beside the
/// sites it vouches for, so adding a site under a stale marker fails and
/// removing one shows up at the marker. Options (1) and (2) are a single `cfg`
/// line that can be an arbitrary distance from the tests it silences: gate a
/// file and delete its marker in the same commit and every check the census
/// makes still passes, with the sites simply gone. That is the shape this table
/// exists to make loud.
///
/// # Why names and not a count
///
/// This was `[("epics-ca-rs", 19), ("epics-pva-rs", 12)]`, measured on the
/// branch that wrote it, and it failed to compose three times. A count is
/// branch-local: two branches that each gate a file of their own produce a
/// merged tree whose true total is neither branch's number, and since one side
/// left the constant alone the merge is clean and warns about nothing. The
/// number to catch also moves downward as often as up, so no floor helps.
///
/// Names compose. Each branch adds its own gated files to this list, the merge
/// is the union, and the union is the correct answer for the merged tree —
/// which is the one property a count cannot have. Both directions are checked
/// below, for the reason the `ioc-spawn-gate` reverse check exists: a list that
/// only fails on the entries it does not have goes quiet the moment a gate is
/// removed, and a gate removed is exactly the case where sites come back into
/// the census unannounced. Duplicates are asserted too, because a merge is the
/// one thing that can produce them and, unlike a wrong total, a repeated entry
/// is detectable.
///
/// # This constant is a merged-tree value
///
/// It is the union of every branch's gated files, so it can only be measured
/// on the merged tree, and only the orchestrator can set it. A branch-local
/// run is EXPECTED to differ — a branch sees its own gates and none of the
/// gates in flight beside it — so a red assertion here on a branch is the
/// check working, not a stale constant. Do not paste your branch's measured
/// set in to make it pass: that overwrites the merged value with a narrower
/// one and hands the next branch the same red with less information. Report
/// the difference and leave the constant alone.
const SCOPE_GATED: &[(&str, &str)] = &[
    ("ad-plugins-rs", "tests/configure_duplicate_port.rs"),
    ("ad-plugins-rs", "tests/ioc_asyn_commands.rs"),
    (
        "ad-plugins-rs",
        "tests/plugin_configure_blocking_callbacks.rs",
    ),
    ("ad-plugins-rs", "tests/plugin_configure_max_threads.rs"),
    ("ad-plugins-rs", "tests/plugin_configure_source.rs"),
    ("ad-plugins-rs", "tests/plugin_configure_stack_size.rs"),
    (
        "ad-plugins-rs",
        "tests/plugin_configure_unknown_array_port.rs",
    ),
    ("ad-plugins-rs", "tests/reference_plugin_asyn_flags.rs"),
    ("ad-plugins-rs", "tests/std_arrays_configure_queue_size.rs"),
    ("asyn-rs", "tests/process_gate_tests.rs"),
    (
        "epics-base-rs",
        "tests/a_band_blocks_on_the_programs_runtime.rs",
    ),
    ("epics-base-rs", "tests/client_server.rs"),
    (
        "epics-bridge-rs",
        "tests/a_bare_ca_link_opens_no_pva_channel.rs",
    ),
    (
        "epics-bridge-rs",
        "tests/connected_pva_link_lset_calls_allocate_nothing.rs",
    ),
    (
        "epics-bridge-rs",
        "tests/gw_upstream_unresponsive_window.rs",
    ),
    (
        "epics-bridge-rs",
        "tests/pva_forward_link_dispatch_alloc.rs",
    ),
    ("epics-bridge-rs", "tests/pva_gateway.rs"),
    (
        "epics-bridge-rs",
        "tests/pva_link_empty_name_processes_invalid.rs",
    ),
    ("epics-bridge-rs", "tests/pva_link_parse_warnings.rs"),
    ("epics-bridge-rs", "tests/pva_link_put_relinks_cp_holder.rs"),
    ("epics-bridge-rs", "tests/pva_link_scan_needs_an_inlink.rs"),
    ("epics-bridge-rs", "tests/pva_local_link_unfulfilled.rs"),
    (
        "epics-bridge-rs",
        "tests/qsrv_put_fails_on_the_process_status.rs",
    ),
    ("epics-bridge-rs", "tests/qsrv_remote_log.rs"),
    ("epics-bridge-rs", "tests/qsrv_rs_resolves_a_ca_link.rs"),
    ("epics-bridge-rs", "tests/qsrv_ts_filter_probe.rs"),
    ("epics-ca-rs", "src/client/sync_group.rs"),
    ("epics-ca-rs", "src/client/transport.rs"),
    ("epics-ca-rs", "src/server/ca_server.rs"),
    ("epics-ca-rs", "src/server/tcp.rs"),
    (
        "epics-ca-rs",
        "tests/a_loopback_interface_binds_two_responder_sockets.rs",
    ),
    ("epics-ca-rs", "tests/access_rights_spc_nomod.rs"),
    ("epics-ca-rs", "tests/acf_host_identity.rs"),
    ("epics-ca-rs", "tests/acf_hot_reload.rs"),
    ("epics-ca-rs", "tests/asg_inp_change_reevaluates_access.rs"),
    ("epics-ca-rs", "tests/beacon_ramp_connect.rs"),
    ("epics-ca-rs", "tests/ca_char_put_is_unsigned.rs"),
    ("epics-ca-rs", "tests/ca_client_char_is_unsigned.rs"),
    ("epics-ca-rs", "tests/ca_client_context_channels.rs"),
    ("epics-ca-rs", "tests/ca_filter_pre_post.rs"),
    ("epics-ca-rs", "tests/ca_out_link_write_access_gate.rs"),
    ("epics-ca-rs", "tests/ca_ts_filter_probe.rs"),
    ("epics-ca-rs", "tests/caget_dbr_type.rs"),
    ("epics-ca-rs", "tests/calink_enum_ctrl_attributes.rs"),
    (
        "epics-ca-rs",
        "tests/calink_out_put_clamps_to_element_count.rs",
    ),
    (
        "epics-ca-rs",
        "tests/caput_cli_string_put_prints_what_c_caput_prints.rs",
    ),
    ("epics-ca-rs", "tests/caput_long_string_readback.rs"),
    ("epics-ca-rs", "tests/caput_old_read_never_aborts.rs"),
    ("epics-ca-rs", "tests/caput_readback_timeout.rs"),
    (
        "epics-ca-rs",
        "tests/caput_sends_the_string_c_caput_sends.rs",
    ),
    ("epics-ca-rs", "tests/channel_field_spelling.rs"),
    ("epics-ca-rs", "tests/channel_filters.rs"),
    ("epics-ca-rs", "tests/cli_cainfo_host.rs"),
    ("epics-ca-rs", "tests/cli_caput_enum_order.rs"),
    ("epics-ca-rs", "tests/cli_client_exception.rs"),
    ("epics-ca-rs", "tests/cli_connect_gate.rs"),
    ("epics-ca-rs", "tests/cli_dbr_type_race.rs"),
    ("epics-ca-rs", "tests/cli_monitor_separator.rs"),
    ("epics-ca-rs", "tests/cli_value_format.rs"),
    ("epics-ca-rs", "tests/dbput_ca_link_target.rs"),
    ("epics-ca-rs", "tests/differential_libca.rs"),
    ("epics-ca-rs", "tests/discovery_dns_update.rs"),
    ("epics-ca-rs", "tests/discovery_mdns.rs"),
    ("epics-ca-rs", "tests/dollar_long_string_record.rs"),
    ("epics-ca-rs", "tests/long_string_dollar_monitor_counts.rs"),
    ("epics-ca-rs", "tests/long_string_plain_monitor_counts.rs"),
    (
        "epics-ca-rs",
        "tests/plain_write_fails_on_the_process_status.rs",
    ),
    (
        "epics-ca-rs",
        "tests/put_failure_status_is_the_layer_not_the_error.rs",
    ),
    ("epics-ca-rs", "tests/put_native_type_conversion.rs"),
    (
        "epics-ca-rs",
        "tests/qtime_tag_monitor_serves_the_raw_stamp.rs",
    ),
    ("epics-ca-rs", "tests/rate_limit_parity.rs"),
    ("epics-ca-rs", "tests/rsrv_error_ctx_is_the_record_name.rs"),
    ("epics-ca-rs", "tests/rsrv_error_frame_matches_softioc.rs"),
    ("epics-ca-rs", "tests/scalar_string_write_needs_a_nul.rs"),
    (
        "epics-ca-rs",
        "tests/scan_thread_monitor_post_allocates_nothing.rs",
    ),
    (
        "epics-ca-rs",
        "tests/softioc_a_ca_put_never_waits_on_the_console.rs",
    ),
    ("epics-ca-rs", "tests/softioc_db_channel_create_line.rs"),
    (
        "epics-ca-rs",
        "tests/softioc_prompt_ioc_init_starts_the_ca_server.rs",
    ),
    (
        "epics-ca-rs",
        "tests/the_ca_server_start_prints_rsrvs_bind_lines.rs",
    ),
    (
        "epics-ca-rs",
        "tests/the_ca_servers_set_tcp_nodelay_on_the_accepted_socket.rs",
    ),
    ("epics-ca-rs", "tests/tls_end_to_end.rs"),
    (
        "epics-ca-rs",
        "tests/zero_length_array_write_is_not_a_short_frame.rs",
    ),
    ("epics-libcom-rs", "src/net/loopback_mcast.rs"),
    ("epics-pva-rs", "src/server/pva_server.rs"),
    ("epics-pva-rs", "tests/create_channel_backpressure.rs"),
    ("epics-pva-rs", "tests/heartbeat_echo.rs"),
    ("epics-pva-rs", "tests/interop_pvxs_mods/access_denied.rs"),
    ("epics-pva-rs", "tests/interop_pvxs_mods/asg_cross_impl.rs"),
    ("epics-pva-rs", "tests/interop_pvxs_mods/complex_types.rs"),
    (
        "epics-pva-rs",
        "tests/interop_pvxs_mods/field_projection.rs",
    ),
    (
        "epics-pva-rs",
        "tests/interop_pvxs_mods/monitor_seed_mask.rs",
    ),
    ("epics-pva-rs", "tests/interop_pvxs_mods/pipeline_r20.rs"),
    (
        "epics-pva-rs",
        "tests/interop_pvxs_mods/rpc_and_get_field.rs",
    ),
    ("epics-pva-rs", "tests/interop_pvxs_mods/search_prefix.rs"),
    (
        "epics-pva-rs",
        "tests/interop_pvxs_mods/server_ports_env.rs",
    ),
    ("epics-pva-rs", "tests/interop_pvxs_mods/tcp_search_r11.rs"),
    ("epics-pva-rs", "tests/interop_pvxs_mods/tls_interop.rs"),
    ("epics-pva-rs", "tests/interop_pvxs_mods/tls_mtls.rs"),
    ("epics-pva-rs", "tests/interop_pvxs_mods/type_cache.rs"),
    (
        "epics-pva-rs",
        "tests/interop_pvxs_mods/udp_v6_port_held.rs",
    ),
    ("epics-pva-rs", "tests/monitor_conn_events.rs"),
    (
        "epics-pva-rs",
        "tests/monitor_decode_fault_resets_circuit.rs",
    ),
    ("epics-pva-rs", "tests/monitor_finish_body.rs"),
    ("epics-pva-rs", "tests/multi_pv_streaming.rs"),
    ("epics-pva-rs", "tests/put_two_phase.rs"),
    ("epics-pva-rs", "tests/report_info.rs"),
    ("epics-pva-rs", "tests/rpc_empty_reply.rs"),
    ("epics-pva-rs", "tests/rpc_pvrequest.rs"),
    ("epics-pva-rs", "tests/server_info.rs"),
    ("epics-pva-rs", "tests/server_lifecycle.rs"),
    ("epics-pva-rs", "tests/set_byte_order_monitor_relatch.rs"),
    ("epics-pva-rs", "tests/set_byte_order_renegotiate.rs"),
    ("epics-pva-rs", "tests/tls.rs"),
    ("epics-pva-rs", "tests/ui25_same_process_discovery.rs"),
    ("mca-rs", "tests/a_remote_siml_is_a_ca_link.rs"),
    ("optics-rs", "tests/integration_tests.rs"),
    ("optics-rs", "tests/table_link_kind.rs"),
    ("regression-ioc", "tests/families.rs"),
    ("regression-ioc", "tests/motor.rs"),
    ("regression-ioc", "tests/smoke.rs"),
    ("scaler-rs", "tests/cnt_abort_cancels_delayed_start.rs"),
    ("scaler-rs", "tests/integration_tests.rs"),
    ("std-rs", "tests/e2e_tests.rs"),
    ("std-rs", "tests/epid_dtyp_put_keeps_the_init_dset.rs"),
    ("std-rs", "tests/integration_tests.rs"),
    ("std-rs", "tests/snl_runners_drive_the_db.rs"),
    ("std-rs", "tests/timestamp_monitor_gate.rs"),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root above tools/rtems-exec-gate")
        .to_path_buf()
}

#[test]
fn every_crate_that_derives_the_backend_is_accounted_for() {
    let root = workspace_root();
    let members = rtems_exec_gate::workspace_members(&root);
    assert!(
        !members.is_empty(),
        "no workspace members under {}; the derivation is reading the wrong thing",
        root.display()
    );

    let declaring = rtems_exec_gate::declaring_crates(&root);
    assert!(
        !declaring.is_empty(),
        "no `build.rs` under {} emits `{cfg}`; either the backend selector \
         moved or this anchor needs re-pointing, and until it does the census \
         has nothing to judge",
        root.display(),
        cfg = rtems_exec_gate::BACKEND_CFG
    );
    let derived: BTreeSet<&str> = declaring.iter().map(|(n, _)| n.as_str()).collect();

    // The reverse direction. A crate that gates on the backend cfg without a
    // build script that emits it compiles the gated code away everywhere and
    // reports nothing; only this says so.
    let ungoverned: Vec<String> = members
        .iter()
        .filter(|dir| rtems_exec_gate::names_the_backend_cfg(dir))
        .map(|dir| {
            dir.file_name()
                .expect("named")
                .to_string_lossy()
                .into_owned()
        })
        .filter(|n| n != rtems_exec_gate::RULE_OWNER && !derived.contains(n.as_str()))
        .collect();
    assert!(
        ungoverned.is_empty(),
        "{ungoverned:?} gate on `exec_backend`/`tokio_backend` but their build \
         scripts never emit either, so that code is compiled away on every \
         target and no configuration this workspace builds would notice. Give \
         each one `rtems_exec_gate::CANONICAL_DERIVATION`."
    );

    // And the 23 copies of the one derivation are held against each other.
    let drifted = rtems_exec_gate::derivation_breaches(&root);
    assert!(drifted.is_empty(), "{}", drifted.join("\n\n"));

    // Every crate is judged before any verdict, so one crate's breach cannot
    // hide another's.
    let breaches: Vec<String> = declaring
        .iter()
        .filter_map(|(_, dir)| {
            let dir = dir.to_str().expect("crate path is UTF-8");
            std::panic::catch_unwind(|| rtems_exec_gate::assert_crate_is_accounted_for(dir))
                .err()
                .map(|p| match p.downcast::<String>() {
                    Ok(msg) => *msg,
                    Err(p) => p
                        .downcast_ref::<&str>()
                        .map_or_else(|| format!("{dir}: non-string panic"), |s| s.to_string()),
                })
        })
        .collect();
    assert!(
        breaches.is_empty(),
        "{} crate(s) breach the census:\n\n{}",
        breaches.len(),
        breaches.join("\n\n")
    );
    // Measured through the same function the `scope-gated` binary prints from.
    // Filling this constant in is a merge-time step performed by reading a tree
    // (see that binary's docs), and a reading that could disagree with the
    // assertion here would hand the merge a value this test then rejects.
    let measured = rtems_exec_gate::scope_gated_set(&root);

    // A merge is the one thing that can put the same file in twice, and unlike
    // a wrong total it leaves something to see.
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    let dupes: Vec<&(&str, &str)> = SCOPE_GATED.iter().filter(|e| !seen.insert(**e)).collect();
    assert!(
        dupes.is_empty(),
        "SCOPE_GATED names {dupes:?} more than once. That is what a merge of two \
         branches that gated the same file looks like; keep one entry."
    );

    let pinned: BTreeSet<(String, String)> = SCOPE_GATED
        .iter()
        .map(|(c, f)| ((*c).to_owned(), (*f).to_owned()))
        .collect();

    let unlisted: Vec<&(String, String)> = measured.difference(&pinned).collect();
    let stale: Vec<&(String, String)> = pinned.difference(&measured).collect();
    assert!(
        unlisted.is_empty() && stale.is_empty(),
        "the set of files that gate reactor-dependent sites out of the census \
         has changed.\n\n\
         gated but not listed: {unlisted:?}\n\
         listed but no longer gating: {stale:?}\n\n\
         The second list is as much a failure as the first: a file that stops \
         gating puts its sites back in the census, and nothing else reports \
         that. `cargo run -q -p rtems-exec-gate --bin scope-gated -- <root>` \
         classifies each entry of the second list and prints the same text \
         below without needing a failing run. If the change was the point of \
         the commit, say so in it and paste the measured set:\n\n{}",
        rtems_exec_gate::scope_gated_literal(&measured)
    );
}

/// The binary reads `SCOPE_GATED` out of this file as text, because it reports
/// on trees it is not compiled inside; the test above compares the compiled
/// constant. Two readings of one pin is exactly the shape that drifts, so the
/// two are asserted equal rather than assumed to agree.
#[test]
fn the_text_reading_of_the_pin_matches_the_compiled_one() {
    let pinned: BTreeSet<(String, String)> = SCOPE_GATED
        .iter()
        .map(|(c, f)| ((*c).to_owned(), (*f).to_owned()))
        .collect();
    assert_eq!(
        rtems_exec_gate::pinned_scope_gated(&workspace_root()),
        pinned
    );
}
