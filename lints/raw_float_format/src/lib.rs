//! `raw_float_format`: a float reaching a std formatting trait raw.
//!
//! `{}`, `{:?}`, `{:e}` and `{:E}` of an `f32` / `f64` print Rust's spelling —
//! `NaN` with its sign dropped, shortest round-trip digits, `1e5` — where C
//! `printf` prints `nan` / `-nan`, `%g` digits and `1e+05`. A call is flagged
//! when its callee's bounds, instantiated with the call's arguments, ask
//! `Display`, `Debug`, `LowerExp`, `UpperExp` or `ToString` of a float, or of
//! a std type whose impl of that trait asks it of a float: `Vec<T>: Debug`
//! needs `T: Debug`, `Box<T>: Display` needs `T: Display`. That one rule sees
//! `format_args!`'s argument constructors, `x.to_string()`, a `fmt` delegated
//! to a float, a generic `T: Display` instantiated with one and `{:?}` of a
//! `Vec<f64>`, an `Option<f32>` or a `(u8, f64)`; such a value coerced to a
//! `dyn` formatting trait is flagged too. A type's own `Debug`, derived or
//! written, is developer output and is not looked into.
//!
//! C-comparable text goes through the C formatter, never `{:?}`.
//!
//! A site that is not C text names its argument in `dylint.toml` with a
//! reason; an entry that matches nothing in a checked file fails too.

#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_infer;
extern crate rustc_middle;
extern crate rustc_span;
extern crate rustc_trait_selection;

use std::path::Path;

use rustc_errors::DiagDecorator;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::{Expr, ExprKind, LetStmt, Node, QPath};
use rustc_infer::infer::TyCtxtInferExt;
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::traits::ObligationCause;
use rustc_middle::ty::adjustment::{Adjust, PointerCoercion};
use rustc_middle::ty::{self, Ty, TypeVisitableExt};
use rustc_span::def_id::{DefId, LOCAL_CRATE};
use rustc_span::{FileName, Span, SyntaxContext};
use rustc_trait_selection::traits::{ImplSource, Obligation, SelectionContext};
use serde::Deserialize;

dylint_linting::impl_late_lint! {
    /// ### What it does
    /// Flags a call that needs `Display`, `Debug`, `LowerExp`, `UpperExp` or
    /// `ToString` of an `f32` / `f64`, or of a type that formats one (a
    /// reference, `Box`, `Vec`, `Option`, tuple or derived struct of one).
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

impl<'tcx> LateLintPass<'tcx> for RawFloatFormat {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) {
        if in_debug_impl(cx, expr) {
            return;
        }
        let (formatted, (mut span, text)) = if let Some(formatted) = float_bound(cx, expr) {
            (formatted, named(cx, expr, formatted))
        } else if let Some(formatted) = float_coercion(cx, expr) {
            (formatted, (expr.span, None))
        } else {
            return;
        };
        let sm = cx.sess().source_map();
        // An argument another crate's macro builds (`vec![x]`) spans that
        // macro's source; report its call here.
        let file = loop {
            let file = sm.lookup_source_file(span.lo());
            if file.cnum == LOCAL_CRATE {
                break file;
            }
            if !span.from_expansion() {
                return;
            }
            span = span.parent_callsite().unwrap_or(span);
        };
        // `format_args!` lowers the argument at its source range but in the
        // macro's context, where rustc drops lints from an external macro.
        let span = span.with_ctxt(SyntaxContext::root());
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

/// Whether `expr` is in the body of an `impl Debug`, derived or written.
fn in_debug_impl(cx: &LateContext<'_>, expr: &Expr<'_>) -> bool {
    let tcx = cx.tcx;
    let body = tcx.typeck_root_def_id(tcx.hir_enclosing_body_owner(expr.hir_id).to_def_id());
    tcx.trait_impl_of_assoc(body)
        .map(|imp| tcx.impl_trait_id(imp))
        .is_some_and(|tr| is_std(tcx, tr) && tcx.item_name(tr).as_str() == "Debug")
}

/// The type `expr` holds when it is coerced to a `dyn` std formatting trait that
/// spells a float in it: `&x as &dyn Display`, formatted where no bound sees it.
fn float_coercion<'tcx>(cx: &LateContext<'tcx>, expr: &Expr<'tcx>) -> Option<Ty<'tcx>> {
    let tcx = cx.tcx;
    let results = cx.typeck_results();
    let mut source = results.expr_ty(expr);
    for adjustment in results.expr_adjustments(expr) {
        if let Adjust::Pointer(PointerCoercion::Unsize) = adjustment.kind
            && let Some(target) = adjustment.target.builtin_deref(true)
            && let ty::Dynamic(predicates, ..) = target.kind()
            && let Some(format) = predicates.principal_def_id()
            && is_format_trait(tcx, format)
            && let Some(held) = source.builtin_deref(true)
            && spells_float(cx, ty::TraitRef::new(tcx, format, [held]))
        {
            return Some(held.peel_refs());
        }
        source = adjustment.target;
    }
    None
}

/// The type a std formatting trait is required of by `expr`, a method call or
/// a path to a function, when that trait spells a float in it: the callee's
/// own bounds, instantiated with the call's arguments. `format_args!`'s
/// `Argument::new_display::<f64>`, `x.to_string()`, `Display::fmt(&x, f)` and
/// `render::<f64>` for a `fn render<T: Display>` are all this one bound.
fn float_bound<'tcx>(cx: &LateContext<'tcx>, expr: &Expr<'tcx>) -> Option<Ty<'tcx>> {
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
            (is_format_trait(tcx, bound.def_id()) && spells_float(cx, bound.trait_ref))
                .then(|| bound.self_ty().peel_refs())
        })
}

fn is_format_trait(tcx: ty::TyCtxt<'_>, def: DefId) -> bool {
    is_std(tcx, def) && FORMAT_TRAITS.contains(&tcx.item_name(def).as_str())
}

/// Whether proving `bound` (`T: Debug`, ...) formats a float: `T` is one, or
/// the std impl it selects asks a formatting trait of one through its own
/// bounds (`Vec<T>: Debug` needs `T: Debug`). A `PhantomData<f64>` or
/// `Weak<f64>` asks nothing of its `f64` and prints none. The walk is the
/// proof of a bound that holds, so it ends.
fn spells_float<'tcx>(cx: &LateContext<'tcx>, bound: ty::TraitRef<'tcx>) -> bool {
    let tcx = cx.tcx;
    let self_ty = bound.self_ty();
    if matches!(self_ty.kind(), ty::Float(_)) {
        return true;
    }
    let infcx = tcx.infer_ctxt().build(cx.typing_mode());
    let obligation = Obligation::new(tcx, ObligationCause::dummy(), cx.param_env, bound);
    let Ok(Some(ImplSource::UserDefined(selected))) =
        SelectionContext::new(&infcx).select(&obligation)
    else {
        return false;
    };
    if !is_std(tcx, selected.impl_def_id) {
        return false;
    }
    let args =
        tcx.erase_and_anonymize_regions(infcx.deeply_resolve_ignoring_regions(selected.args));
    if args.has_non_region_infer() {
        return false;
    }
    tcx.clauses_of(selected.impl_def_id)
        .instantiate(tcx, args)
        .clauses
        .into_iter()
        .filter_map(|clause| {
            tcx.try_normalize_erasing_regions(cx.typing_env(), clause)
                .ok()?
                .as_trait_clause()
        })
        .map(|clause| clause.skip_binder().trait_ref)
        .filter(|asked| is_format_trait(tcx, asked.def_id))
        .any(|asked| spells_float(cx, asked))
}

/// Where to report `expr` and, unless it is the span's own text, what the
/// allow-list names it by: a method call's receiver; a called path's argument
/// that is the `formatted` value (`format_args!`'s argument), else the path.
fn named<'tcx>(
    cx: &LateContext<'tcx>,
    expr: &Expr<'tcx>,
    formatted: Ty<'tcx>,
) -> (Span, Option<String>) {
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
            .find(|arg| results.expr_ty(arg).peel_refs() == formatted)
        {
            Some(arg) => (lowered_from(cx, arg).span, None),
            None => (parent.span, Some(snippet(cx, expr.span))),
        };
    }
    (expr.span, None)
}

/// The expression `format_args!` lowered its argument `arg` (`args.0`) from:
/// that element of its `super let args = (&arg0, ..)`. Only that one keeps the
/// context of a macro that built it, `vec![x]`.
fn lowered_from<'tcx>(cx: &LateContext<'tcx>, arg: &'tcx Expr<'tcx>) -> &'tcx Expr<'tcx> {
    if let ExprKind::Field(base, index) = arg.kind
        && let ExprKind::Path(QPath::Resolved(None, path)) = base.kind
        && let Res::Local(local) = path.res
        && let Node::LetStmt(LetStmt {
            init: Some(init), ..
        }) = cx.tcx.parent_hir_node(local)
        && let ExprKind::Tup(elements) = init.kind
        && let Ok(index) = index.as_str().parse::<usize>()
        && let Some(&Expr {
            kind: ExprKind::AddrOf(_, _, inner),
            ..
        }) = elements.get(index)
    {
        inner
    } else {
        arg
    }
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
