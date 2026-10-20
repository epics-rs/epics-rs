//! `raw_float_format`: a float reaching a std formatting trait raw.
//!
//! `{}`, `{:?}`, `{:e}` and `{:E}` of an `f32` / `f64` print Rust's spelling —
//! `NaN` with its sign dropped, shortest round-trip digits, `1e5` — where C
//! `printf` prints `nan` / `-nan`, `%g` digits and `1e+05`. A call is flagged
//! when its callee's bounds, instantiated with the call's arguments, ask
//! `Display`, `Debug`, `LowerExp`, `UpperExp` or `ToString` of a float (through
//! references and std's delegating wrappers such as `Box`). That one rule sees
//! `format_args!`'s argument constructors, `x.to_string()`, a `fmt` delegated
//! to a float and a generic `T: Display` instantiated with one. A site that is
//! not C text names its argument in `dylint.toml` with a reason; an entry that
//! matches nothing in a checked file fails too.

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
use rustc_middle::ty;
use rustc_span::def_id::{DefId, LOCAL_CRATE};
use rustc_span::{FileName, Span, SyntaxContext};
use serde::Deserialize;

dylint_linting::impl_late_lint! {
    /// ### What it does
    /// Flags a call that needs `Display`, `Debug`, `LowerExp`, `UpperExp` or
    /// `ToString` of an `f32` / `f64` (or a reference or `Box` of one).
    ///
    /// ### Why is this bad?
    /// The text differs from the C IOC's `printf` for NaN, infinities,
    /// exponents and `%g` digits.
    ///
    /// ### Example
    /// ```rust,ignore
    /// format!("{}", x)
    /// ```
    /// Use instead:
    /// ```rust,ignore
    /// format!("{}", CFloat(x).g())
    /// ```
    pub RAW_FLOAT_FORMAT,
    Deny,
    "a float formatted by Rust's float spelling instead of C printf's",
    RawFloatFormat::new()
}

/// `dylint.toml`'s `[raw_float_format]` table.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    allow: Vec<Allow>,
}

/// A site that is not C text: `arg` as written in `path` (relative to the
/// workspace root), and why.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Allow {
    path: String,
    arg: String,
    #[expect(dead_code, reason = "read by people, not the lint")]
    reason: String,
}

struct RawFloatFormat {
    allow: Vec<Allow>,
    used: Vec<bool>,
}

impl RawFloatFormat {
    fn new() -> Self {
        let config: Config = dylint_linting::config_or_default(env!("CARGO_PKG_NAME"));
        let used = vec![false; config.allow.len()];
        Self {
            allow: config.allow,
            used,
        }
    }
}

/// The traits whose `fmt` / `to_string` spell a float Rust's way.
const FORMAT_TRAITS: &[&str] = &["Display", "Debug", "LowerExp", "UpperExp", "ToString"];

/// std types whose formatting impls hand the value straight to the inner
/// type's, so `Box<f64>` spells its float as `f64` does.
const DELEGATING: &[&str] = &[
    "Box",
    "Rc",
    "Arc",
    "Cow",
    "Ref",
    "RefMut",
    "MutexGuard",
    "RwLockReadGuard",
    "RwLockWriteGuard",
    "Wrapping",
    "Saturating",
    "Pin",
];

impl<'tcx> LateLintPass<'tcx> for RawFloatFormat {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        let Some(formatted) = float_bound(cx, expr) else {
            return;
        };
        let (span, text) = named(cx, expr);
        // `format_args!` lowers the argument at its source range but in the
        // macro's context, where rustc drops lints from an external macro.
        let span = span.with_ctxt(SyntaxContext::root());
        let sm = cx.sess().source_map();
        let file = sm.lookup_source_file(span.lo());
        if file.cnum != LOCAL_CRATE {
            return;
        }
        let Some(path) = local_path(&file.name) else {
            return;
        };
        let text = text.unwrap_or_else(|| {
            let snippet = sm.span_to_snippet(span).unwrap_or_default();
            // A captured argument's range is its whole placeholder, `{x:.3}`.
            match snippet.strip_prefix('{') {
                Some(captured) => captured.split([':', '}']).next().unwrap_or_default(),
                None => snippet.strip_prefix('&').unwrap_or(&snippet).trim(),
            }
            .to_owned()
        });
        let mut allowed = false;
        for (entry, used) in self.allow.iter().zip(self.used.iter_mut()) {
            if path.ends_with(&entry.path) && entry.arg == text {
                *used = true;
                allowed = true;
            }
        }
        if allowed {
            return;
        }
        cx.emit_span_lint(
            RAW_FLOAT_FORMAT,
            span,
            DiagDecorator(|diag| {
                diag.primary_message(format!(
                    "`{text}` is a `{formatted}` formatted with Rust's float spelling"
                ));
                diag.help(
                    "format `CFloat(..)` (`.g()` for %g) as the C text does, or allow the \
                     argument in dylint.toml with the C reason",
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
                    RAW_FLOAT_FORMAT,
                    None::<Span>,
                    DiagDecorator(|diag| {
                        diag.primary_message(format!(
                            "dylint.toml allows `{}` in {}, which formats no float there",
                            entry.arg, entry.path
                        ));
                    }),
                );
            }
        }
    }
}

/// The float a std formatting trait is required of by `expr`, a method call
/// or a path to a function: the callee's own bounds, instantiated with the
/// call's arguments. `format_args!`'s `Argument::new_display::<f64>`,
/// `x.to_string()`, `Display::fmt(&x, f)` and `render::<f64>` for a
/// `fn render<T: Display>` are all this one bound.
fn float_bound<'tcx>(cx: &LateContext<'tcx>, expr: &Expr<'tcx>) -> Option<ty::Ty<'tcx>> {
    let tcx = cx.tcx;
    let results = cx.typeck_results();
    let def = match expr.kind {
        ExprKind::MethodCall(..) => results.type_dependent_def_id(expr.hir_id)?,
        ExprKind::Path(ref qpath) => cx.qpath_res(qpath, expr.hir_id).opt_def_id()?,
        _ => return None,
    };
    if !matches!(tcx.def_kind(def), DefKind::Fn | DefKind::AssocFn) {
        return None;
    }
    let args = results.node_args(expr.hir_id);
    if args.len() != tcx.generics_of(def).count() {
        return None;
    }
    tcx.clauses_of(def)
        .instantiate(tcx, args)
        .clauses
        .into_iter()
        .find_map(|clause| {
            let bound = clause.skip_normalization().as_trait_clause()?.skip_binder();
            let format = bound.def_id();
            if !is_std(tcx, format) || !FORMAT_TRAITS.contains(&tcx.item_name(format).as_str()) {
                return None;
            }
            let formatted = delegated(tcx, bound.self_ty());
            matches!(formatted.kind(), ty::Float(_)).then_some(formatted)
        })
}

/// `ty` with the references and [`DELEGATING`] wrappers around it removed.
fn delegated<'tcx>(tcx: ty::TyCtxt<'tcx>, mut ty: ty::Ty<'tcx>) -> ty::Ty<'tcx> {
    loop {
        ty = ty.peel_refs();
        match ty.kind() {
            ty::Adt(adt, args)
                if is_std(tcx, adt.did())
                    && DELEGATING.contains(&tcx.item_name(adt.did()).as_str()) =>
            {
                match args.types().next() {
                    Some(inner) => ty = inner,
                    None => return ty,
                }
            }
            _ => return ty,
        }
    }
}

/// Where to report `expr` and, unless it is the span's own text, what the
/// allow-list names it by: a method call's receiver; a called path's first
/// float argument (`format_args!`'s argument), else the path.
fn named<'tcx>(cx: &LateContext<'tcx>, expr: &Expr<'tcx>) -> (Span, Option<String>) {
    if let ExprKind::MethodCall(_, receiver, _, _) = expr.kind {
        return (expr.span, Some(snippet(cx, receiver.span)));
    }
    if let Node::Expr(parent) = cx.tcx.parent_hir_node(expr.hir_id)
        && let ExprKind::Call(callee, args) = parent.kind
        && callee.hir_id == expr.hir_id
    {
        let results = cx.typeck_results();
        return match args
            .iter()
            .find(|arg| matches!(delegated(cx.tcx, results.expr_ty(arg)).kind(), ty::Float(_)))
        {
            Some(arg) => (arg.span, None),
            None => (parent.span, Some(snippet(cx, expr.span))),
        };
    }
    (expr.span, None)
}

fn snippet(cx: &LateContext<'_>, span: Span) -> String {
    let span = span.with_ctxt(SyntaxContext::root());
    cx.sess()
        .source_map()
        .span_to_snippet(span)
        .unwrap_or_default()
}

fn is_std(tcx: ty::TyCtxt<'_>, def: DefId) -> bool {
    matches!(tcx.crate_name(def.krate).as_str(), "core" | "alloc" | "std")
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
            r#"[raw_float_format]
allow = [
    { path = "ui_allow/main.rs", arg = "x", reason = "test" },
    { path = "ui_allow/main.rs", arg = "z", reason = "test" },
]"#,
        )
        .run();
}
