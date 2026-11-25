//! `instant_add_duration`: a deadline built by `Instant + Duration`.
//!
//! `std`'s and `tokio`'s `Instant + Duration` panic when the sum leaves the
//! clock's range, and a timeout from the environment, a record field or a tool
//! flag reaches it (`CONN_TMO=1e300`, `pvget -w 1e19`). A deadline is built by
//! `deadline_after` / `deadline_from_now` (`epics_libcom_rs::runtime::time`
//! for `std`'s clock, `runtime::task` for the backend's), which saturate to
//! `FAR_FUTURE`. `+`, `+=`, `.add(..)` and `.add_assign(..)` of a `Duration`
//! on either `Instant` are flagged, whatever the operand: clippy's
//! `disallowed_methods` cannot see an operator.
//!
//! A sum that cannot overflow and has no owner to call (the owners' own
//! fallback, a crate without epics-libcom-rs) names its enclosing function in
//! `dylint.toml` with a reason; an entry that matches nothing in a checked
//! file fails too.

#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_middle;
extern crate rustc_span;

use std::path::Path;

use rustc_errors::DiagDecorator;
use rustc_hir::def::DefKind;
use rustc_hir::{Expr, ExprKind, Node};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::ty::{self, Ty};
use rustc_span::def_id::{DefId, LOCAL_CRATE};
use rustc_span::{FileName, Span};
use serde::Deserialize;

dylint_linting::impl_late_lint! {
    /// ### What it does
    /// Flags `Instant + Duration`, `+=`, `.add(..)` and `.add_assign(..)` on
    /// `std::time::Instant` or `tokio::time::Instant`.
    ///
    /// ### Why is this bad?
    /// The add panics when the sum is past the clock's range, which a
    /// configured timeout of `1e300` seconds reaches.
    ///
    /// ### Example
    /// ```rust,ignore
    /// let deadline = Instant::now() + timeout;
    /// ```
    /// Use instead:
    /// ```rust,ignore
    /// let deadline = deadline_from_now(timeout);
    /// ```
    pub INSTANT_ADD_DURATION,
    Deny,
    "a deadline built by `Instant + Duration`, which panics on overflow",
    InstantAddDuration::new()
}

/// `dylint.toml`'s `[instant_add_duration]` table.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    allow: Vec<Allow>,
}

/// Every sum in function `function` of `path` (relative to the workspace
/// root), and why it cannot overflow.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Allow {
    path: String,
    #[serde(rename = "fn")]
    function: String,
    #[expect(dead_code, reason = "read by people, not the lint")]
    reason: String,
}

struct InstantAddDuration {
    allow: Vec<Allow>,
    used: Vec<bool>,
}

impl InstantAddDuration {
    fn new() -> Self {
        let config: Config = dylint_linting::config_or_default(env!("CARGO_PKG_NAME"));
        let used = vec![false; config.allow.len()];
        Self {
            allow: config.allow,
            used,
        }
    }
}

impl<'tcx> LateLintPass<'tcx> for InstantAddDuration {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let Some(clock) = instant_plus_duration(cx, expr) else {
            return;
        };
        // The call of `Add::add(t, d)` is reported, not its callee path.
        let span = if let Node::Expr(parent) = cx.tcx.parent_hir_node(expr.hir_id)
            && let ExprKind::Call(callee, _) = parent.kind
            && callee.hir_id == expr.hir_id
        {
            parent.span
        } else {
            expr.span
        };
        let sm = cx.sess().source_map();
        let file = sm.lookup_source_file(span.lo());
        if file.cnum != LOCAL_CRATE {
            return;
        }
        let Some(path) = local_path(&file.name) else {
            return;
        };
        let tcx = cx.tcx;
        let owner = tcx.typeck_root_def_id(tcx.hir_enclosing_body_owner(expr.hir_id).to_def_id());
        let function = tcx.opt_item_name(owner);
        let mut allowed = false;
        for (entry, used) in self.allow.iter().zip(self.used.iter_mut()) {
            if path.ends_with(&entry.path)
                && function.is_some_and(|name| name.as_str() == entry.function)
            {
                *used = true;
                allowed = true;
            }
        }
        if allowed {
            return;
        }
        let text = sm.span_to_snippet(span).unwrap_or_default();
        cx.emit_span_lint(
            INSTANT_ADD_DURATION,
            span,
            DiagDecorator(|diag| {
                diag.primary_message(format!(
                    "`{text}` adds a `Duration` to a `{clock}`, which panics past the clock's range"
                ));
                diag.help(
                    "take the deadline from `deadline_after` / `deadline_from_now`, or allow \
                     the function in dylint.toml with the reason the sum cannot overflow",
                );
            }),
        );
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        let sm = cx.sess().source_map();
        let files: Vec<_> = sm
            .files()
            .iter()
            .filter(|file| file.cnum == LOCAL_CRATE)
            .filter_map(|file| local_path(&file.name).map(Path::to_path_buf))
            .collect();
        for (entry, used) in self.allow.iter().zip(&self.used) {
            if !used && files.iter().any(|file| file.ends_with(&entry.path)) {
                cx.opt_span_lint(
                    INSTANT_ADD_DURATION,
                    None::<Span>,
                    DiagDecorator(|diag| {
                        diag.primary_message(format!(
                            "dylint.toml allows `fn {}` in {}, which adds no `Duration` to an \
                             `Instant` there",
                            entry.function, entry.path
                        ));
                    }),
                );
            }
        }
    }
}

/// The clock `expr` adds a `Duration` to: an overloaded `+` / `+=`, or a
/// method call or path to `Add::add` / `AddAssign::add_assign`, whose
/// `Self` is `std`'s or `tokio`'s `Instant` and whose `Rhs` is `Duration`.
fn instant_plus_duration<'tcx>(cx: &LateContext<'tcx>, expr: &Expr<'tcx>) -> Option<&'static str> {
    let tcx = cx.tcx;
    let results = cx.typeck_results();
    let def = match expr.kind {
        ExprKind::Binary(..) | ExprKind::AssignOp(..) | ExprKind::MethodCall(..) => {
            results.type_dependent_def_id(expr.hir_id)?
        }
        ExprKind::Path(ref qpath) => cx.qpath_res(qpath, expr.hir_id).opt_def_id()?,
        _ => return None,
    };
    if tcx.def_kind(def) != DefKind::AssocFn {
        return None;
    }
    // An operator, method call or path resolves to the trait's own `add`; an
    // inherent method's parent is an impl, which has no name.
    let op = tcx.parent(def);
    if tcx.def_kind(op) != DefKind::Trait
        || !is_crate(tcx, op, "core")
        || !matches!(tcx.item_name(op).as_str(), "Add" | "AddAssign")
    {
        return None;
    }
    let args = results.node_args(expr.hir_id);
    if args.len() != 2 {
        return None;
    }
    let clock = clock(tcx, args.type_at(0))?;
    is_duration(tcx, args.type_at(1)).then_some(clock)
}

fn clock(tcx: ty::TyCtxt<'_>, ty: Ty<'_>) -> Option<&'static str> {
    let ty::Adt(adt, _) = ty.kind() else {
        return None;
    };
    if tcx.item_name(adt.did()).as_str() != "Instant" {
        return None;
    }
    match tcx.crate_name(adt.did().krate).as_str() {
        "std" => Some("std::time::Instant"),
        "tokio" => Some("tokio::time::Instant"),
        _ => None,
    }
}

fn is_duration(tcx: ty::TyCtxt<'_>, ty: Ty<'_>) -> bool {
    matches!(ty.kind(), ty::Adt(adt, _)
        if is_crate(tcx, adt.did(), "core") && tcx.item_name(adt.did()).as_str() == "Duration")
}

fn is_crate(tcx: ty::TyCtxt<'_>, def: DefId, name: &str) -> bool {
    tcx.crate_name(def.krate).as_str() == name
}

fn local_path(name: &FileName) -> Option<&Path> {
    match name {
        FileName::Real(real) => real.local_path(),
        _ => None,
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}

#[test]
fn ui_allow() {
    dylint_testing::ui::Test::src_base(env!("CARGO_PKG_NAME"), "ui_allow")
        .dylint_toml(
            r#"[instant_add_duration]
allow = [
    { path = "ui_allow/main.rs", fn = "owner", reason = "test" },
    { path = "ui_allow/main.rs", fn = "absent", reason = "test" },
]"#,
        )
        .run();
}
