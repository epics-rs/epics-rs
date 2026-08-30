//! # C reference pins
//!
//! Every `file.c:NNN` citation in this crate resolves at the tree and revision
//! below, not at whatever that tree's working copy holds today. These trees are
//! checked out on local branches here and run ahead of their pins.
//!
//! | tree | pinned revision |
//! | --- | --- |
//! | `pvxs` | `1.5.1-42-gb568e93` |
//! | `epics-base` | `R7.0.10` |
//!
//! **Resolve by symbol at the pin; the line is a hint.** Find the named
//! function, struct, macro or field first, and treat the line number as a hint
//! that has to land inside that construct. Three cases follow:
//!
//! 1. Construct at the pin, line lands in it — the citation is exact. A
//!    reference checkout ahead of the pin will disagree; that disagreement is
//!    the checkout's, not the citation's.
//! 2. Construct at the pin, line lands outside it — line drift. Keep the
//!    symbol and move the line to the pin's.
//! 3. Construct absent at the pin — the citation means code added after it,
//!    and is NOT moved onto the pin, where it would point at lines that do not
//!    exist. It names the revision it means inline, beside the line span: the
//!    upstream PR and commit, and that both are later than the pin this table
//!    gives. `epics-libcom-rs` already carries that form.
//!
//! Every pin above passes `git merge-base --is-ancestor <pin> origin/<default>`
//! in its own tree, which is the test a pin has to meet. A `git describe`
//! string names an exact commit and is worth as much as a tag; what
//! disqualifies a revision is being reachable only from a fork branch or an
//! unmerged PR, because then it names nothing a reader outside this workspace
//! can fetch.
//!
//! Resolve each citation on its own. One sentence can cite two lines that are
//! right at different revisions, and a check run at either revision then
//! reports a single tidy error while vouching for the very citation the other
//! condemns.
//!
//! A row reading *no settled pin* means no revision has been agreed for that
//! tree: say which revision you read, and do not take its `HEAD` for the pin.
//! Citations into non-EPICS sources (libc, RTEMS, `rtems-libbsd`, VxWorks,
//! vendored third-party) are outside this table and carry no pin.

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use quote::quote;
use syn::{Data, DeriveInput, Fields, ItemFn, Lit, parse_macro_input};

/// Resolve the path to the `runtime` module `#[epics_test]` expands into.
///
/// `runtime::task::test_block_on` is OWNED by `epics-libcom-rs`;
/// `epics_base_rs::runtime` is only `pub use epics_libcom_rs::{net, runtime}`.
/// Probing the owner first is what makes the expansion survive a consumer
/// whose `epics-base-rs` is optional: `proc_macro_crate` reads the manifest,
/// not the resolved feature set, so it finds a disabled optional dependency
/// and names a crate the build did not link. `asyn-rs`'s one `#[epics_test]`
/// was `E0433` under `--no-default-features` — which is that crate's own
/// embedded configuration (`scripts/rtems-check.sh:190-197`) — and no
/// spelling at the call site could fix it, because the crate name is emitted
/// by this function.
///
/// `epics-base-rs` and the `epics-rs` umbrella stay in the chain below for a
/// consumer that has neither `epics-libcom-rs` nor a direct base dependency.
fn epics_runtime_path() -> proc_macro2::TokenStream {
    if let Ok(found) = crate_name("epics-libcom-rs") {
        return match found {
            FoundCrate::Itself => quote!(::epics_libcom_rs),
            FoundCrate::Name(name) => {
                let ident = proc_macro2::Ident::new(&name, proc_macro2::Span::call_site());
                quote!(::#ident)
            }
        };
    }
    epics_base_path()
}

/// Resolve the path to `epics_base_rs`, supporting both direct dependency
/// (`epics-base-rs`) and umbrella crate (`epics-rs`) usage.
///
/// Still base-first, because its remaining caller — [`epics_main`] — needs
/// `__tokio`, which only `epics-base-rs` re-exports. A path that answers two
/// different questions is what let the `runtime` one above go wrong.
fn epics_base_path() -> proc_macro2::TokenStream {
    if let Ok(found) = crate_name("epics-base-rs") {
        match found {
            // `crate` would be wrong everywhere except the library target
            // itself (an integration test or bin under the same package sees
            // `crate` as *its own* crate root), so refer to the library by
            // name; `epics-base-rs`'s `extern crate self as epics_base_rs;`
            // makes that name resolve inside the library target too.
            FoundCrate::Itself => quote!(::epics_base_rs),
            FoundCrate::Name(name) => {
                let ident = proc_macro2::Ident::new(&name, proc_macro2::Span::call_site());
                quote!(::#ident)
            }
        }
    } else if let Ok(found) = crate_name("epics-rs") {
        match found {
            FoundCrate::Itself => quote!(crate::base),
            FoundCrate::Name(name) => {
                let ident = proc_macro2::Ident::new(&name, proc_macro2::Span::call_site());
                quote!(::#ident::base)
            }
        }
    } else if let Ok(found) = crate_name("epics-libcom-rs") {
        // `epics_base_rs::runtime` is a re-export of `epics_libcom_rs::runtime`,
        // so for a consumer that only has the libcom layer — including
        // `epics-libcom-rs`'s own test suite, which cannot depend on
        // `epics-base-rs` without a cycle — the same expansions resolve
        // through the libcom path. Probed last so a crate with both deps
        // keeps the canonical `epics_base_rs` spelling.
        match found {
            FoundCrate::Itself => quote!(::epics_libcom_rs),
            FoundCrate::Name(name) => {
                let ident = proc_macro2::Ident::new(&name, proc_macro2::Span::call_site());
                quote!(::#ident)
            }
        }
    } else {
        quote!(::epics_base_rs)
    }
}

/// Marks an `async fn main()` as an EPICS IOC entry point.
///
/// Builds a multi-threaded tokio runtime (via `epics_base_rs::__tokio`)
/// without requiring the downstream crate to depend on tokio directly.
///
/// # Restrictions
/// - Must be applied to `async fn main()` — no generics, no arguments.
/// - Does not accept attribute arguments (e.g., `#[epics_main(flavor = ...)]` is a compile error).
///
/// # Example
/// ```ignore
/// #[epics_main]
/// async fn main() -> CaResult<()> {
///     // ...
/// }
/// ```
#[proc_macro_attribute]
pub fn epics_main(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[epics_main] does not accept arguments",
        )
        .to_compile_error()
        .into();
    }

    let input = parse_macro_input!(item as ItemFn);
    let sig = &input.sig;

    if sig.asyncness.is_none() {
        return syn::Error::new_spanned(sig.fn_token, "#[epics_main] requires `async fn`")
            .to_compile_error()
            .into();
    }
    if sig.ident != "main" {
        return syn::Error::new_spanned(&sig.ident, "#[epics_main] must be applied to `main`")
            .to_compile_error()
            .into();
    }
    if !sig.inputs.is_empty() {
        return syn::Error::new_spanned(&sig.inputs, "`main` must not take arguments")
            .to_compile_error()
            .into();
    }
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        return syn::Error::new_spanned(&sig.generics, "`main` must not be generic")
            .to_compile_error()
            .into();
    }

    let attrs = &input.attrs;
    let vis = &input.vis;
    let ret = &sig.output;
    let body = &input.block;
    let base = epics_base_path();

    quote! {
        #(#attrs)*
        #vis fn main() #ret {
            #base::__tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(async move #body)
        }
    }
    .into()
}

/// Marks an async function as an EPICS test, driven by whichever backend the
/// build selected.
///
/// Expands to a plain `#[test]` whose body runs through
/// `epics_base_rs::runtime::task::test_block_on`: on the tokio backend that
/// is a fresh current-thread runtime (exactly what `#[tokio::test]` builds);
/// on the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread` / the RTEMS
/// target) the
/// test thread drives the future itself and spawns/sleeps land on the
/// background executor. Use this instead of `#[tokio::test]` for any test
/// whose body sticks to the `runtime::` abstractions — such a test needs no
/// backend gating and no `RTEMS-EXEC-MODEL-ALLOW` census entry. A body that
/// touches `tokio::net`/`tokio::time` directly still needs `#[tokio::test]`
/// plus a backend gate.
///
/// # Restrictions
/// - Must be applied to an `async fn` with no arguments and no generics.
/// - Does not accept attribute arguments.
///
/// # Example
/// ```ignore
/// #[epics_test]
/// async fn test_something() {
///     // ...
/// }
/// ```
#[proc_macro_attribute]
pub fn epics_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[epics_test] does not accept arguments",
        )
        .to_compile_error()
        .into();
    }

    let input = parse_macro_input!(item as ItemFn);
    let sig = &input.sig;

    if sig.asyncness.is_none() {
        return syn::Error::new_spanned(sig.fn_token, "#[epics_test] requires `async fn`")
            .to_compile_error()
            .into();
    }
    if !sig.inputs.is_empty() {
        return syn::Error::new_spanned(&sig.inputs, "test functions must not take arguments")
            .to_compile_error()
            .into();
    }
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        return syn::Error::new_spanned(&sig.generics, "test functions must not be generic")
            .to_compile_error()
            .into();
    }
    if input.attrs.iter().any(|a| a.path().is_ident("test")) {
        return syn::Error::new_spanned(
            input
                .attrs
                .iter()
                .find(|a| a.path().is_ident("test"))
                .unwrap(),
            "#[epics_test] already adds #[test]; remove the duplicate",
        )
        .to_compile_error()
        .into();
    }

    let attrs = &input.attrs;
    let vis = &input.vis;
    let name = &sig.ident;
    let ret = &sig.output;
    let body = &input.block;
    let base = epics_runtime_path();

    quote! {
        #[test]
        #(#attrs)*
        #vis fn #name() #ret {
            #base::runtime::task::test_block_on(async move #body)
        }
    }
    .into()
}

/// Derive macro that implements the `Record` trait for a struct.
///
/// # Attributes
///
/// - `#[record(type = "ai")]` — sets the record type name
/// - `#[record(type = "ai", crate_path = "my_crate")]` — override crate path
/// - `#[record(type = "seq", init = seq_init_record)]` — emit
///   `Record::init_record` delegating to the named free function
///   (`fn(&mut Self, u8) -> CaResult<()>`), for a record type whose C
///   `init_record` does real work; omitted, the trait's no-op applies
/// - `#[record(type = "fanout", no_value_monitor)]` — emit
///   `Record::process_posts_value_monitor` returning `false`, for a "trigger"
///   record (fanout/seq) whose C `process()` posts VAL only with alarm events;
///   omitted, the trait default `true` applies
/// - `#[record(type = "longin", dset_owns_udf_on_computed)]` — emit
///   `Record::rederives_udf_on_computed_read` returning `false`, for a record
///   whose C `process()` keeps `prec->udf = FALSE` inside `if (status == 0)`
///   and folds `2` into `0` only afterwards (or not at all); omitted, the
///   trait default `true` applies
/// - `#[field(type = "Double")]` — sets the DBR type for a field
/// - `#[field(type = "Double", read_only)]` — marks a field as read-only
/// - `#[field(type = "Short", menu_choices = SELM_CHOICES)]` — a
///   `DBF_MENU` field served as `DBR_ENUM`; `SELM_CHOICES` resolves to a
///   `&'static [&'static str]` choice table and is emitted through
///   `Record::menu_field_choices` (the framework promotes the stored menu
///   index to `EpicsValue::Enum` and attaches these labels).
///
/// Field names are converted from snake_case to UPPER_CASE for EPICS field names.
#[proc_macro_derive(EpicsRecord, attributes(record, field))]
pub fn derive_epics_record(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match impl_epics_record(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

struct RecordAttrs {
    record_type: String,
    crate_path: Option<String>,
    /// `#[record(constant_init = "SELL:SELN,DOL0:DO0")]` — the record's C
    /// `recGblInitConstantLink` table, as `LINK:TARGET` pairs. Emitted as
    /// `Record::constant_init_links`, which the init-seed owner
    /// (`PvDatabase::rec_gbl_init_constant_links`) applies; a record whose
    /// `Record` impl is hand-written declares the same table by overriding that
    /// method directly.
    constant_init: Vec<(String, String)>,
    /// `#[record(init = some_fn)]` — the record's C `init_record`. The derive
    /// emits no `init_record` by default (the trait's no-op applies); a record
    /// type whose C `init_record` does real work (e.g. `seq`'s
    /// `prec->oldn = prec->seln`) names a free function `fn(&mut Self, u8) ->
    /// CaResult<()>` here, and the derive emits `Record::init_record`
    /// delegating to it. Kept a free function, not an inherent method, so the
    /// derive need not assume a method exists.
    init: Option<syn::Path>,
    /// `#[record(metadata_override = some_fn)]` — the record type's C rset
    /// lists fields its `get_control_double` / `get_graphic_double` /
    /// `get_units` / `get_precision` answers itself, beyond what the
    /// framework's per-field routing derives. Point this at a free function
    /// `fn(&Self, &str) -> Option<FieldMetadataOverride>` and the derive emits
    /// `Record::field_metadata_override` delegating to it. Same shape as
    /// [`RecordAttrs::init`]: a free function, so the derive need not assume an
    /// inherent method exists.
    metadata_override: Option<syn::Path>,
    /// `#[record(link_metadata_field = some_fn)]` — the record type's C rset
    /// answers some fields' `get_units` / `get_precision` /
    /// `get_graphic_double` / `get_alarm_double` from one of the record's own
    /// LINK fields rather than from the record (C's `get_linkNumber` /
    /// `get_dol` shape). Point this at a free function
    /// `fn(&Self, &str) -> Option<String>` returning the backing link field's
    /// name, and the derive emits `Record::link_backed_metadata_field`
    /// delegating to it. Same free-function shape as
    /// [`RecordAttrs::metadata_override`].
    link_metadata_field: Option<syn::Path>,
    /// `#[record(no_value_monitor)]` — the record's process cycle posts no VAL
    /// value monitor (`Record::process_posts_value_monitor` → `false`). Set for
    /// the "trigger" records `fanout`/`seq`, whose C `process()` posts VAL only
    /// with alarm events, never `DBE_VALUE`/`DBE_LOG`. Default `false` (the
    /// trait default `true` applies), so ordinary value records are unaffected.
    no_value_monitor: bool,
    /// `#[record(dset_owns_udf_on_computed)]` — the record's C `process()` does
    /// NOT re-derive `udf` after a device read that wrote VAL directly (C
    /// `return 2`), because the `else if (status == 2) status = 0;` fold sits
    /// AFTER the UDF assignment (`biRecord.c:136-141`) or is absent entirely
    /// (`longinRecord.c:148`, `int64inRecord.c:144`). Emitted as
    /// `Record::rederives_udf_on_computed_read` → `false`. Default `false` (the
    /// trait default `true` applies), which is `aiRecord.c:158-161`'s shape and
    /// the majority.
    dset_owns_udf_on_computed: bool,
}

struct FieldInfo {
    ident: syn::Ident,
    epics_name: String,
    dbf_type: String,
    read_only: bool,
    /// Choice-table expression for a `DBF_MENU` field served as
    /// `DBR_ENUM` (`#[field(menu_choices = SOME_CONST)]`), or `None` for a
    /// non-menu field. `SOME_CONST` must resolve to a
    /// `&'static [&'static str]` in the record's module.
    menu_choices: Option<syn::Expr>,
}

fn impl_epics_record(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let attrs = parse_record_attrs(input)?;
    let record_type_str = &attrs.record_type;

    // Determine crate path
    let krate: proc_macro2::TokenStream = match &attrs.crate_path {
        Some(p) => p.parse().unwrap(),
        None => quote! { crate },
    };

    // Parse fields
    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => fields,
            _ => {
                return Err(syn::Error::new_spanned(
                    input,
                    "EpicsRecord requires named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                input,
                "EpicsRecord can only be derived for structs",
            ));
        }
    };

    let mut field_infos = Vec::new();
    for f in &fields.named {
        let ident = f.ident.as_ref().unwrap().clone();
        let (dbf_type, read_only, menu_choices) = parse_field_attrs(f)?;
        let epics_name = ident.to_string().to_uppercase();
        field_infos.push(FieldInfo {
            ident,
            epics_name,
            dbf_type,
            read_only,
            menu_choices,
        });
    }

    // NOTE: the derive deliberately emits NO field declaration. A record type
    // is declared by its `.dbd` and by nothing else; the generated table in
    // `dbd_generated` is what `FieldDeclaration::field_list` serves. Deriving
    // the declaration from the Rust struct member instead is what typed
    // `longin.ADEL` `DBF_DOUBLE` (the member is an `f64`) where
    // `longinRecord.dbd` says `DBF_LONG`, and typed every `DBF_MENU` field as
    // a bare integer with no `menu()`. The member types the *storage*; only the
    // `.dbd` types the *field*.

    // Generate get_field match arms
    let get_arms: Vec<_> = field_infos
        .iter()
        .map(|fi| {
            let epics_name = &fi.epics_name;
            let ident = &fi.ident;
            let conversion = value_to_epics(&krate, &fi.dbf_type, quote!(self.#ident));
            quote! {
                #epics_name => Some(#conversion),
            }
        })
        .collect();

    // Generate put_field match arms
    let put_arms: Vec<_> = field_infos
        .iter()
        .map(|fi| {
            let epics_name = &fi.epics_name;
            let ident = &fi.ident;
            if fi.read_only {
                quote! {
                    #epics_name => {
                        return Err(#krate::error::CaError::ReadOnlyField(
                            #epics_name.to_string()
                        ));
                    }
                }
            } else {
                let extraction = value_from_epics(&krate, &fi.dbf_type, ident);
                quote! {
                    #epics_name => { #extraction }
                }
            }
        })
        .collect();

    // Generate menu_field_choices for any `#[field(menu_choices = ...)]`
    // fields, so a DBF_MENU field served as DBR_ENUM carries its menu()
    // choice labels (see `Record::menu_field_choices`). Omitted entirely
    // when no field declares a menu, so the trait default applies.
    let menu_arms: Vec<_> = field_infos
        .iter()
        .filter_map(|fi| {
            fi.menu_choices.as_ref().map(|expr| {
                let epics_name = &fi.epics_name;
                quote! { #epics_name => Some(#expr), }
            })
        })
        .collect();
    let menu_method = if menu_arms.is_empty() {
        quote! {}
    } else {
        quote! {
            fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
                match field {
                    #(#menu_arms)*
                    _ => None,
                }
            }
        }
    };

    // `#[record(constant_init = "LINK:TARGET,...")]` — the record's C
    // `recGblInitConstantLink` table. Omitted entirely when the record declares
    // none, so the trait default (no constant seeds) applies.
    let constant_init_method = if attrs.constant_init.is_empty() {
        quote! {}
    } else {
        let seeds: Vec<_> = attrs
            .constant_init
            .iter()
            .map(|(link, target)| {
                quote! {
                    #krate::server::record::ConstantInitLink::new(#link, #target)
                }
            })
            .collect();
        quote! {
            fn constant_init_links(&self) -> Vec<#krate::server::record::ConstantInitLink> {
                vec![#(#seeds),*]
            }
        }
    };

    // `#[record(init = some_fn)]` — emit `Record::init_record` delegating to
    // the named free function. Omitted entirely when the record declares no
    // init, so the trait default (no-op) applies.
    let init_method = match &attrs.init {
        Some(path) => quote! {
            fn init_record(&mut self, pass: u8) -> #krate::error::CaResult<()> {
                #path(self, pass)
            }
        },
        None => quote! {},
    };

    // `#[record(metadata_override = some_fn)]` — emit
    // `Record::field_metadata_override` delegating to the named free function.
    // Omitted entirely when the record declares none, so the trait default
    // (`None` — the framework's routing owns every field) applies.
    let metadata_override_method = match &attrs.metadata_override {
        Some(path) => quote! {
            fn field_metadata_override(
                &self,
                field: &str,
            ) -> Option<#krate::server::record::FieldMetadataOverride> {
                #path(self, field)
            }
        },
        None => quote! {},
    };

    // `#[record(link_metadata_field = some_fn)]` — emit
    // `Record::link_backed_metadata_field` delegating to the named free
    // function. Omitted entirely when the record declares none, so the trait
    // default (`None` — no field's metadata comes from a link) applies.
    let link_metadata_field_method = match &attrs.link_metadata_field {
        Some(path) => quote! {
            fn link_backed_metadata_field(&self, field: &str) -> Option<String> {
                #path(self, field)
            }
        },
        None => quote! {},
    };

    // `#[record(no_value_monitor)]` — emit `Record::process_posts_value_monitor`
    // returning `false` (fanout/seq trigger-VAL). Omitted when the flag is
    // absent, so the trait default (`true`) applies to every value record.
    let value_monitor_method = if attrs.no_value_monitor {
        quote! {
            fn process_posts_value_monitor(&self) -> bool {
                false
            }
        }
    } else {
        quote! {}
    };

    // `#[record(dset_owns_udf_on_computed)]` — emit
    // `Record::rederives_udf_on_computed_read` returning `false` for the five
    // records whose C `process()` leaves `udf` to the dset on a `return 2`.
    let computed_udf_method = if attrs.dset_owns_udf_on_computed {
        quote! {
            fn rederives_udf_on_computed_read(&self) -> bool {
                false
            }
        }
    } else {
        quote! {}
    };

    let expanded = quote! {
        impl #krate::server::record::Record for #name {
            #constant_init_method
            #init_method
            #value_monitor_method
            #computed_udf_method
            #metadata_override_method
            #link_metadata_field_method

            fn record_type(&self) -> &'static str {
                #record_type_str
            }

            fn get_field(&self, name: &str) -> Option<#krate::types::EpicsValue> {
                match name {
                    #(#get_arms)*
                    _ => None,
                }
            }

            fn put_field(&mut self, name: &str, value: #krate::types::EpicsValue) -> #krate::error::CaResult<()> {
                self.validate_put(name, &value)?;
                match name {
                    #(#put_arms)*
                    _ => {
                        return Err(#krate::error::CaError::FieldNotFound(name.to_string()));
                    }
                }
                self.on_put(name);
                Ok(())
            }

            #menu_method
        }
    };

    Ok(expanded)
}

fn parse_record_attrs(input: &DeriveInput) -> syn::Result<RecordAttrs> {
    let mut record_type = None;
    let mut crate_path = None;
    let mut constant_init: Vec<(String, String)> = Vec::new();
    let mut init: Option<syn::Path> = None;
    let mut metadata_override: Option<syn::Path> = None;
    let mut link_metadata_field: Option<syn::Path> = None;
    let mut no_value_monitor = false;
    let mut dset_owns_udf_on_computed = false;

    for attr in &input.attrs {
        if attr.path().is_ident("record") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("type") {
                    let value = meta.value()?;
                    let lit: Lit = value.parse()?;
                    if let Lit::Str(s) = lit {
                        record_type = Some(s.value());
                    }
                    Ok(())
                } else if meta.path.is_ident("crate_path") {
                    let value = meta.value()?;
                    let lit: Lit = value.parse()?;
                    if let Lit::Str(s) = lit {
                        crate_path = Some(s.value());
                    }
                    Ok(())
                } else if meta.path.is_ident("constant_init") {
                    let value = meta.value()?;
                    let lit: Lit = value.parse()?;
                    if let Lit::Str(s) = lit {
                        for pair in s.value().split(',').filter(|p| !p.trim().is_empty()) {
                            let (link, target) = pair.trim().split_once(':').ok_or_else(|| {
                                meta.error("constant_init entries are `LINK:TARGET` pairs")
                            })?;
                            constant_init
                                .push((link.trim().to_string(), target.trim().to_string()));
                        }
                    }
                    Ok(())
                } else if meta.path.is_ident("init") {
                    init = Some(meta.value()?.parse()?);
                    Ok(())
                } else if meta.path.is_ident("metadata_override") {
                    metadata_override = Some(meta.value()?.parse()?);
                    Ok(())
                } else if meta.path.is_ident("link_metadata_field") {
                    link_metadata_field = Some(meta.value()?.parse()?);
                    Ok(())
                } else if meta.path.is_ident("no_value_monitor") {
                    // Bare flag, like a field's `read_only`: no `= value`.
                    no_value_monitor = true;
                    Ok(())
                } else if meta.path.is_ident("dset_owns_udf_on_computed") {
                    // Bare flag.
                    dset_owns_udf_on_computed = true;
                    Ok(())
                } else {
                    Err(meta.error(
                        "expected `type`, `crate_path`, `constant_init`, `init`, \
                         `metadata_override`, `link_metadata_field`, \
                         `no_value_monitor` or `dset_owns_udf_on_computed`",
                    ))
                }
            })?;
        }
    }

    let record_type = record_type
        .ok_or_else(|| syn::Error::new_spanned(input, "missing #[record(type = \"...\")]"))?;

    Ok(RecordAttrs {
        record_type,
        crate_path,
        constant_init,
        init,
        metadata_override,
        link_metadata_field,
        no_value_monitor,
        dset_owns_udf_on_computed,
    })
}

fn parse_field_attrs(field: &syn::Field) -> syn::Result<(String, bool, Option<syn::Expr>)> {
    let mut dbf_type = None;
    let mut read_only = false;
    let mut menu_choices = None;

    for attr in &field.attrs {
        if attr.path().is_ident("field") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("type") {
                    let value = meta.value()?;
                    let lit: Lit = value.parse()?;
                    if let Lit::Str(s) = lit {
                        dbf_type = Some(s.value());
                    }
                    Ok(())
                } else if meta.path.is_ident("read_only") {
                    read_only = true;
                    Ok(())
                } else if meta.path.is_ident("menu_choices") {
                    // `menu_choices = SOME_CONST`: a path/expression that
                    // resolves to a `&'static [&'static str]` choice table
                    // for a DBF_MENU field served as DBR_ENUM.
                    let value = meta.value()?;
                    menu_choices = Some(value.parse::<syn::Expr>()?);
                    Ok(())
                } else {
                    Err(meta.error("expected `type`, `read_only`, or `menu_choices`"))
                }
            })?;
        }
    }

    let dbf_type = dbf_type
        .ok_or_else(|| syn::Error::new_spanned(field, "missing #[field(type = \"...\")]"))?;

    Ok((dbf_type, read_only, menu_choices))
}

fn value_to_epics(
    krate: &proc_macro2::TokenStream,
    dbf_type: &str,
    field_expr: proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    match dbf_type {
        "Double" => quote! { #krate::types::EpicsValue::Double(#field_expr) },
        "Float" => quote! { #krate::types::EpicsValue::Float(#field_expr) },
        "Short" => quote! { #krate::types::EpicsValue::Short(#field_expr) },
        "Long" => quote! { #krate::types::EpicsValue::Long(#field_expr) },
        "Int64" => quote! { #krate::types::EpicsValue::Int64(#field_expr) },
        "Char" => quote! { #krate::types::EpicsValue::Char(#field_expr) },
        "Enum" => quote! { #krate::types::EpicsValue::Enum(#field_expr) },
        "UShort" => quote! { #krate::types::EpicsValue::UShort(#field_expr) },
        "String" => quote! { #krate::types::EpicsValue::String(#field_expr.clone().into()) },
        // Byte-faithful DBF_STRING storage (`PvString`): serve the bytes
        // verbatim with no lossy text conversion.
        "PvStr" => quote! { #krate::types::EpicsValue::String(#field_expr.clone()) },
        _ => quote! { compile_error!("unknown field type") },
    }
}

fn value_from_epics(
    krate: &proc_macro2::TokenStream,
    dbf_type: &str,
    field_ident: &syn::Ident,
) -> proc_macro2::TokenStream {
    // Enum fields accept Enum, Long, and Short values (common in asyn drivers)
    if dbf_type == "Enum" {
        return quote! {
            match value {
                #krate::types::EpicsValue::Enum(v) => { self.#field_ident = v; }
                #krate::types::EpicsValue::Long(v) => { self.#field_ident = v as u16; }
                #krate::types::EpicsValue::Short(v) => { self.#field_ident = v as u16; }
                _ => {
                    return Err(#krate::error::CaError::TypeMismatch(
                        stringify!(#field_ident).to_uppercase().to_string()
                    ));
                }
            }
        };
    }

    // DBF_USHORT fields accept UShort plus Short/Long: C `dbPut`
    // converts any source DBR into the native unsigned 16-bit field,
    // and internal link reads (e.g. SELL/NVL -> SELN) still pass Short.
    if dbf_type == "UShort" {
        return quote! {
            match value {
                #krate::types::EpicsValue::UShort(v) => { self.#field_ident = v; }
                #krate::types::EpicsValue::Long(v) => { self.#field_ident = v as u16; }
                #krate::types::EpicsValue::Short(v) => { self.#field_ident = v as u16; }
                _ => {
                    return Err(#krate::error::CaError::TypeMismatch(
                        stringify!(#field_ident).to_uppercase().to_string()
                    ));
                }
            }
        };
    }

    let variant = match dbf_type {
        "Double" => "Double",
        "Float" => "Float",
        "Short" => "Short",
        "Long" => "Long",
        "Int64" => "Int64",
        "Char" => "Char",
        "String" => "String",
        // `PvStr` fields carry the DBF_STRING payload (`EpicsValue::String`)
        // but store it byte-for-byte in a `PvString`.
        "PvStr" => "String",
        _ => return quote! { compile_error!("unknown field type"); },
    };

    let variant_ident = proc_macro2::Ident::new(variant, proc_macro2::Span::call_site());

    // `String` fields are declared as Rust `String` but the
    // `EpicsValue::String` variant wraps `PvString`, so convert on assign.
    // `PvStr` fields already store a `PvString`, so the wire bytes move in
    // verbatim — a non-UTF-8 value is preserved.
    let assign = if dbf_type == "String" {
        quote! { self.#field_ident = v.as_str_lossy().into_owned(); }
    } else {
        quote! { self.#field_ident = v; }
    };

    quote! {
        if let #krate::types::EpicsValue::#variant_ident(v) = value {
            #assign
        } else {
            return Err(#krate::error::CaError::TypeMismatch(
                stringify!(#field_ident).to_uppercase().to_string()
            ));
        }
    }
}

// ── PVA Typed NT + service framework ─────────────────────────────────

/// Resolve the path to `epics_pva_rs` crate. Mirrors
/// [`epics_base_path`] for the PVA macros below.
fn epics_pva_path() -> proc_macro2::TokenStream {
    if let Ok(found) = crate_name("epics-pva-rs") {
        match found {
            FoundCrate::Itself => quote!(crate),
            FoundCrate::Name(name) => {
                let ident = proc_macro2::Ident::new(&name, proc_macro2::Span::call_site());
                quote!(::#ident)
            }
        }
    } else if let Ok(found) = crate_name("epics-rs") {
        match found {
            FoundCrate::Itself => quote!(crate::pva),
            FoundCrate::Name(name) => {
                let ident = proc_macro2::Ident::new(&name, proc_macro2::Span::call_site());
                quote!(::#ident::pva)
            }
        }
    } else {
        quote!(::epics_pva_rs)
    }
}

/// `#[derive(NTScalar)]` — generate a `TypedNT` impl for the
/// annotated struct. The struct must have:
///
/// - exactly one field named `value` of a primitive type (`f64`,
///   `i32`, `String`, etc.) — encoded as the NTScalar `value` slot
/// - any number of additional fields. Fields tagged
///   `#[nt(meta)]` get encoded as their type's
///   `TypedNT` impl (`Alarm`, `TimeStamp`, custom meta structs).
///
/// The wrapper structure id is taken from the `value` type's own
/// `TypedNT` descriptor (`epics:nt/NTScalar:1.0`,
/// `epics:nt/NTScalarArray:1.0`, or `epics:nt/NTEnum:1.0`). The
/// mandatory metadata members for that id — `alarm` and `timeStamp`
/// (and `display` for NTEnum) — are always present in the generated
/// descriptor and value: a `#[nt(meta)]` field with the matching name
/// supplies its own type/value, otherwise a default is filled in.
/// A derived type therefore cannot claim a normative structure id while
/// omitting the members that id requires.
///
/// ```ignore
/// use epics_pva_rs::nt::{Alarm, TimeStamp};
///
/// #[derive(epics_macros_rs::NTScalar)]
/// struct MotorPos {
///     value: f64,
///     #[nt(meta)] alarm: Alarm,
///     #[nt(meta)] timestamp: TimeStamp,
/// }
/// ```
#[proc_macro_derive(NTScalar, attributes(nt))]
pub fn derive_nt_scalar(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let krate = epics_pva_path();
    let name = &input.ident;

    // The derive does not yet handle generic / lifetime params —
    // `descriptor()` would need to resolve every `T: TypedNT`
    // bound at expansion time, and the runtime helpers don't know
    // how to wrap a borrow. Reject up front with a clear message
    // instead of emitting code that fails with a confusing trait
    // resolution error 200 lines later.
    if !input.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &input.generics,
            "NTScalar derive does not support generic or lifetime \
             parameters; implement TypedNT manually for parameterised types",
        )
        .to_compile_error()
        .into();
    }

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return syn::Error::new_spanned(
                    name,
                    "NTScalar derive requires a struct with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(name, "NTScalar derive only works on structs")
                .to_compile_error()
                .into();
        }
    };

    let mut value_field: Option<(syn::Ident, syn::Type)> = None;
    let mut meta_fields: Vec<(syn::Ident, syn::Type)> = Vec::new();
    for field in fields {
        let Some(ident) = field.ident.clone() else {
            continue;
        };
        let is_meta = field.attrs.iter().any(|a| {
            a.path().is_ident("nt")
                && a.parse_nested_meta(|m| {
                    if m.path.is_ident("meta") {
                        Ok(())
                    } else {
                        Err(m.error("unknown #[nt(...)] arg"))
                    }
                })
                .is_ok()
        });
        if is_meta {
            meta_fields.push((ident, field.ty.clone()));
        } else if ident == "value" {
            value_field = Some((ident, field.ty.clone()));
        } else {
            // Other fields are forbidden so the generated descriptor
            // stays predictable. Operators that need richer NT shapes
            // can still implement TypedNT manually.
            return syn::Error::new_spanned(
                ident,
                "NTScalar derive: only `value` and `#[nt(meta)]`-tagged fields allowed",
            )
            .to_compile_error()
            .into();
        }
    }

    let Some((value_ident, value_ty)) = value_field else {
        return syn::Error::new_spanned(name, "NTScalar derive requires a `value` field")
            .to_compile_error()
            .into();
    };

    let meta_field_names: Vec<String> = meta_fields.iter().map(|(i, _)| i.to_string()).collect();
    let meta_field_idents: Vec<&syn::Ident> = meta_fields.iter().map(|(i, _)| i).collect();
    let meta_field_tys: Vec<&syn::Type> = meta_fields.iter().map(|(_, t)| t).collect();

    let value_ty_path = quote!(<#value_ty as #krate::nt::TypedNT>::descriptor());
    let value_to_field =
        quote!(<#value_ty as #krate::nt::TypedNT>::to_pv_field(&self.#value_ident));
    let value_from_field = quote! {
        <#value_ty as #krate::nt::TypedNT>::from_pv_field(__field)
            .map_err(|e| __rt::wrong_type("value", &e.to_string()))?
    };

    // Extract the inner `value` PvField from the parent wrapper
    // structure and pass it to the value type's TypedNT impl. The
    // primitive impls (and Vec<T>, EnumValue, ...) all expect to
    // see their full-wrapper shape (e.g. `epics:nt/NTScalar:1.0` for
    // f64, `epics:nt/NTScalarArray:1.0` for Vec<f64>), so we
    // re-wrap the raw `value` field in the inner type's expected
    // wrapper before forwarding.
    let value_extract = quote! {
        {
            let raw = __s
                .get_field("value")
                .ok_or_else(|| __rt::missing("value"))?;
            let __wrap_sid = match #value_ty_path {
                __rt::FieldDesc::Structure { struct_id, .. } => struct_id,
                _ => "epics:nt/NTScalar:1.0".to_string(),
            };
            let mut __wrap = __rt::PvStructure::new(&__wrap_sid);
            __wrap
                .fields
                .push(("value".into(), raw.clone()));
            let __field = __rt::PvField::Structure(__wrap);
            let __field = &__field;
            #value_from_field
        }
    };

    let expanded = quote! {
        impl #krate::nt::TypedNT for #name {
            fn descriptor() -> #krate::pvdata::FieldDesc {
                use #krate::nt::typed::__rt;
                let mut __fields: ::std::vec::Vec<(::std::string::String, __rt::FieldDesc)> =
                    ::std::vec::Vec::new();
                // The `value` slot's wire type is taken from the
                // user's `value` field type's TypedNT::descriptor —
                // that descriptor is itself an NTScalar wrapper, so
                // we pull out its `value` field. Falling back to
                // Variant when the wrapper looks unexpected keeps
                // user-defined nested NT compositions working.
                // I-2: derive the wrapper struct_id from the inner
                // value type's descriptor so the same derive macro
                // covers NTScalar / NTScalarArray / NTEnum.
                let __inner = #value_ty_path;
                let (__sid, __value_field) = match __inner {
                    __rt::FieldDesc::Structure { struct_id, fields } => {
                        let __value_field = fields.into_iter()
                            .find(|(n, _)| n == "value")
                            .map(|(_, f)| f)
                            .unwrap_or(__rt::FieldDesc::Variant);
                        (struct_id, __value_field)
                    }
                    other => ("epics:nt/NTScalar:1.0".to_string(), other),
                };
                __fields.push(("value".into(), __value_field));
                #(
                    __fields.push((
                        #meta_field_names.into(),
                        <#meta_field_tys as #krate::nt::TypedNT>::descriptor(),
                    ));
                )*
                // Guarantee the mandatory NT metadata members for the
                // resolved structure ID are present (alarm/timeStamp, plus
                // display for NTEnum) even when the user declared none —
                // pvxs `NTScalar`/`NTEnum::build()` always emit them
                // (nt.cpp:44-53, :121-131), so a type claiming the ID must.
                let __fields = __rt::ensure_nt_meta_desc(&__sid, __fields);
                __rt::FieldDesc::Structure {
                    struct_id: __sid,
                    fields: __fields,
                }
            }

            fn to_pv_field(&self) -> #krate::pvdata::PvField {
                use #krate::nt::typed::__rt;
                let __sid = match #value_ty_path {
                    __rt::FieldDesc::Structure { struct_id, .. } => struct_id,
                    _ => "epics:nt/NTScalar:1.0".to_string(),
                };
                let mut __s = __rt::PvStructure::new(&__sid);
                // Inner TypedNT impl may return either a bare scalar
                // or an NTScalar wrapper struct. Unwrap to grab the
                // `value` slot so the parent struct stays
                // single-level.
                let __inner_field = #value_to_field;
                let __value_slot = match __inner_field {
                    __rt::PvField::Structure(inner) => {
                        inner.fields
                            .into_iter()
                            .find(|(n, _)| n == "value")
                            .map(|(_, f)| f)
                            .unwrap_or(__rt::PvField::Scalar(__rt::ScalarValue::Int(0)))
                    }
                    other => other,
                };
                __s.fields.push(("value".into(), __value_slot));
                #(
                    __s.fields.push((
                        #meta_field_names.into(),
                        <#meta_field_tys as #krate::nt::TypedNT>::to_pv_field(&self.#meta_field_idents),
                    ));
                )*
                // Mirror descriptor(): fill any mandatory NT metadata member
                // the user omitted with its default so the value matches the
                // advertised normative structure ID.
                __s.fields = __rt::ensure_nt_meta_value(&__sid, __s.fields);
                __rt::PvField::Structure(__s)
            }

            fn from_pv_field(
                __field: &#krate::pvdata::PvField,
            ) -> ::std::result::Result<Self, #krate::nt::TypedNTError> {
                use #krate::nt::typed::__rt;
                let __s = match __field {
                    __rt::PvField::Structure(s) => s,
                    _ => return Err(__rt::wrong_type("<root>", "expected NTScalar wrapper")),
                };
                // I-2: accept any wrapper id, including
                // NTScalar / NTScalarArray / NTEnum / empty,
                // since the derive emits whatever the value's
                // TypedNT impl declared. Concrete shape mismatch
                // surfaces inside `from_pv_field` for the value
                // type via WrongType.
                let __expected_sid = match #value_ty_path {
                    __rt::FieldDesc::Structure { struct_id, .. } => struct_id,
                    _ => "epics:nt/NTScalar:1.0".to_string(),
                };
                if !(__s.struct_id.is_empty() || __s.struct_id == __expected_sid) {
                    return Err(__rt::wrong_struct_id(&__expected_sid, &__s.struct_id));
                }
                let __value: #value_ty = #value_extract;
                #(
                    let #meta_field_idents: #meta_field_tys = {
                        let raw = __s
                            .get_field(#meta_field_names)
                            .ok_or_else(|| __rt::missing(#meta_field_names))?;
                        <#meta_field_tys as #krate::nt::TypedNT>::from_pv_field(raw)?
                    };
                )*
                Ok(Self {
                    #value_ident: __value,
                    #( #meta_field_idents, )*
                })
            }
        }
    };

    expanded.into()
}

/// `#[pva_service]` — turn an `impl Block` for a service struct
/// into a `PvaService` (in `epics_pva_rs::service`). Every async
/// method becomes a wire-callable RPC; positional parameters are
/// extracted from the request struct's named fields.
///
/// Return-value contract:
/// - a method returning `Result<T, E>` (`T: IntoServiceResponse`,
///   `E: Display`) is the idiomatic form: `Ok(T)` is encoded as the
///   success response, and `Err(e)` becomes an **RPC operation error**
///   (wire `Status::error`), so the client's call resolves to an error
///   — matching pvxs `op->error(...)` (`sharedpv.cpp:162-180`,
///   `test/testrpc.cpp:193-209`). An app that wants an explicit
///   non-error status payload returns `Ok(Status::error(...))`.
/// - a method returning a plain `T: IntoServiceResponse` is always a
///   success response.
///
/// The `Result` vs non-`Result` decision is made by the type system via
/// `IntoServiceOutcome`, not by inspecting how the return type is
/// spelled, so a return type that is a *type alias* for `Result`
/// (`type RpcResult<T> = Result<T, String>`, `anyhow::Result<T>`, …)
/// routes its `Err` to the operation-error path just like a literal
/// `Result`.
///
/// Restrictions:
/// - methods must be `&self` async
/// - parameters must implement `ServiceArg`
/// - the success type must implement `IntoServiceResponse`
///
/// ```ignore
/// struct MotorService { driver: Arc<Driver> }
///
/// #[epics_macros_rs::pva_service]
/// impl MotorService {
///     async fn r#move(&self, target: f64, velocity: f64) -> Result<f64, String> {
///         self.driver.start(target, velocity).await
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn pva_service(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let krate = epics_pva_path();
    let input = parse_macro_input!(item as syn::ItemImpl);
    let self_ty = &input.self_ty;

    let mut method_arms = Vec::new();
    for item in &input.items {
        let syn::ImplItem::Fn(m) = item else {
            continue;
        };
        // Only async fn(&self, ...) methods are exposed.
        if m.sig.asyncness.is_none() {
            continue;
        }
        let mut iter = m.sig.inputs.iter();
        match iter.next() {
            Some(syn::FnArg::Receiver(_)) => {}
            _ => continue, // skip non-method functions
        }
        let method_name = m.sig.ident.to_string();
        let method_ident = &m.sig.ident;

        let mut arg_names: Vec<String> = Vec::new();
        let mut arg_idents: Vec<syn::Ident> = Vec::new();
        let mut arg_tys: Vec<syn::Type> = Vec::new();
        for arg in iter {
            let syn::FnArg::Typed(pat_ty) = arg else {
                continue;
            };
            let syn::Pat::Ident(pat_ident) = &*pat_ty.pat else {
                continue;
            };
            arg_names.push(pat_ident.ident.to_string());
            arg_idents.push(pat_ident.ident.clone());
            arg_tys.push((*pat_ty.ty).clone());
        }

        // The return value's dispatch outcome is decided by the type
        // system, not by syntactically testing whether the return type
        // spells `Result`: `IntoServiceOutcome` has a blanket impl for
        // `T: IntoServiceResponse` (always a success response) and a
        // `Result<T, E>` impl that routes `Err` to the RPC
        // operation-error path (`ServiceError::Method` → wire
        // `Status::error`, pvxs `op->error`). A proc-macro cannot resolve
        // a return type that is a *type alias* for `Result` (e.g.
        // `RpcResult<T>` / `anyhow::Result<T>`), but the compiler resolves
        // it before selecting the impl, so an aliased `Result` behaves
        // identically to a literal one. The `Result` arm never lives in an
        // `IntoServiceResponse for Result` impl, so an `Err` can never be
        // silently encoded as a success NTRPCStatus payload.
        let return_handling = quote! {
            #krate::service::types::IntoServiceOutcome::into_service_outcome(__out)
        };

        let dispatch_arm = quote! {
            {
                let __svc: ::std::sync::Arc<#self_ty> = self.clone();
                #krate::service::ServiceMethod {
                    name: #method_name.into(),
                    dispatch: ::std::sync::Arc::new(move |__req: #krate::pvdata::PvField| {
                        let __svc = __svc.clone();
                        ::std::boxed::Box::pin(async move {
                            let __args = #krate::service::Args::from_pv_field(&__req);
                            #(
                                let #arg_idents: #arg_tys = __args
                                    .get_named::<#arg_tys>(#arg_names)?;
                            )*
                            let __out = __svc.#method_ident(#( #arg_idents ),*).await;
                            #return_handling
                        })
                    }),
                }
            }
        };
        method_arms.push(dispatch_arm);
    }

    let impl_block = &input;
    let expanded = quote! {
        #impl_block

        impl #krate::service::PvaService for #self_ty {
            fn methods(self: ::std::sync::Arc<Self>) -> ::std::vec::Vec<#krate::service::ServiceMethod> {
                ::std::vec![ #( #method_arms ),* ]
            }
        }
    };
    expanded.into()
}

/// `#[derive(NTTable)]` — generate a `TypedNT` impl for a
/// table-shaped struct. Every field of the annotated struct must
/// be a `Vec<T>` where `T: TypedNT` (the column type). Field
/// names become column labels.
///
/// Wire shape:
/// ```text
/// epics:nt/NTTable:1.0 {
///     labels: Vec<String>,    // column names (declared field order)
///     value: {                // one Vec<T> per column
///         col1: Vec<T1>,
///         col2: Vec<T2>,
///         ...
///     },
///     descriptor: String,     // normative NTTable metadata, always
///     alarm: alarm_t,         // emitted (pvxs NTTable::build, nt.cpp)
///     timeStamp: time_t,
/// }
/// ```
///
/// ```ignore
/// use epics_pva_rs::nt::derive::NTTable;
///
/// #[derive(NTTable)]
/// struct ScanResult {
///     timestamp: Vec<f64>,
///     position:  Vec<f64>,
///     intensity: Vec<f64>,
/// }
/// ```
#[proc_macro_derive(NTTable)]
pub fn derive_nt_table(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let krate = epics_pva_path();
    let name = &input.ident;

    if !input.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &input.generics,
            "NTTable derive does not support generic or lifetime parameters; \
             implement TypedNT manually for parameterised tables",
        )
        .to_compile_error()
        .into();
    }

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return syn::Error::new_spanned(
                    name,
                    "NTTable derive requires a struct with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(name, "NTTable derive only works on structs")
                .to_compile_error()
                .into();
        }
    };

    let mut col_idents: Vec<syn::Ident> = Vec::new();
    let mut col_tys: Vec<syn::Type> = Vec::new();
    for field in fields {
        let Some(ident) = field.ident.clone() else {
            continue;
        };
        col_idents.push(ident);
        col_tys.push(field.ty.clone());
    }
    let col_names: Vec<String> = col_idents.iter().map(|i| i.to_string()).collect();

    if col_idents.is_empty() {
        return syn::Error::new_spanned(name, "NTTable derive requires at least one column field")
            .to_compile_error()
            .into();
    }

    // Each column is encoded as Vec<T>; we extract the wire-format
    // descriptor's `value` slot from <Vec<T> as TypedNT>::descriptor()
    // and wrap into the inner `value` struct of NTTable.
    let expanded = quote! {
        impl #krate::nt::TypedNT for #name {
            fn descriptor() -> #krate::pvdata::FieldDesc {
                use #krate::nt::typed::__rt;
                let mut __value_fields: ::std::vec::Vec<(::std::string::String, __rt::FieldDesc)> =
                    ::std::vec::Vec::new();
                #(
                    let __inner = <#col_tys as #krate::nt::TypedNT>::descriptor();
                    let __slot = match __inner {
                        __rt::FieldDesc::Structure { fields, .. } => {
                            fields.into_iter()
                                .find(|(n, _)| n == "value")
                                .map(|(_, f)| f)
                                .unwrap_or(__rt::FieldDesc::Variant)
                        }
                        other => other,
                    };
                    __value_fields.push((#col_names.into(), __slot));
                )*
                let __fields: ::std::vec::Vec<(::std::string::String, __rt::FieldDesc)> = ::std::vec![
                    ("labels".into(), __rt::FieldDesc::ScalarArray(__rt::ScalarType::String)),
                    ("value".into(), __rt::FieldDesc::Structure {
                        struct_id: "".into(),
                        fields: __value_fields,
                    }),
                ];
                // pvxs `NTTable::build()` (nt.cpp:170-176) always carries
                // `descriptor`, `alarm`, and `timeStamp` after `labels` +
                // `value`; inject them so the derived type matches the
                // normative NTTable shape rather than a two-field stub.
                let __fields = __rt::ensure_nt_meta_desc("epics:nt/NTTable:1.0", __fields);
                __rt::FieldDesc::Structure {
                    struct_id: "epics:nt/NTTable:1.0".into(),
                    fields: __fields,
                }
            }

            fn to_pv_field(&self) -> #krate::pvdata::PvField {
                use #krate::nt::typed::__rt;
                let mut __value_struct = __rt::PvStructure::new("");
                #(
                    let __col_field = <#col_tys as #krate::nt::TypedNT>::to_pv_field(&self.#col_idents);
                    let __slot = match __col_field {
                        __rt::PvField::Structure(inner) => {
                            inner.fields.into_iter()
                                .find(|(n, _)| n == "value")
                                .map(|(_, f)| f)
                                .unwrap_or(__rt::PvField::Scalar(__rt::ScalarValue::Int(0)))
                        }
                        other => other,
                    };
                    __value_struct.fields.push((#col_names.into(), __slot));
                )*
                let mut __labels: ::std::vec::Vec<__rt::ScalarValue> = ::std::vec::Vec::new();
                #(
                    __labels.push(__rt::ScalarValue::String(#col_names.into()));
                )*
                let mut __root = __rt::PvStructure::new("epics:nt/NTTable:1.0");
                __root.fields.push((
                    "labels".into(),
                    __rt::PvField::ScalarArray(__labels),
                ));
                __root.fields.push((
                    "value".into(),
                    __rt::PvField::Structure(__value_struct),
                ));
                // Mirror descriptor(): fill the normative NTTable metadata
                // (descriptor/alarm/timeStamp) with defaults.
                __root.fields = __rt::ensure_nt_meta_value("epics:nt/NTTable:1.0", __root.fields);
                __rt::PvField::Structure(__root)
            }

            fn from_pv_field(
                __field: &#krate::pvdata::PvField,
            ) -> ::std::result::Result<Self, #krate::nt::TypedNTError> {
                use #krate::nt::typed::__rt;
                let __s = match __field {
                    __rt::PvField::Structure(s) => s,
                    _ => return Err(__rt::wrong_type("<root>", "expected NTTable wrapper")),
                };
                if !(__s.struct_id.is_empty() || __s.struct_id == "epics:nt/NTTable:1.0") {
                    return Err(__rt::wrong_struct_id("epics:nt/NTTable:1.0", &__s.struct_id));
                }
                let __value_struct = match __s.get_field("value") {
                    ::std::option::Option::Some(__rt::PvField::Structure(v)) => v,
                    _ => return Err(__rt::missing("value")),
                };
                #(
                    let #col_idents: #col_tys = {
                        let raw = __value_struct
                            .get_field(#col_names)
                            .ok_or_else(|| __rt::missing(#col_names))?;
                        // Wrap the raw column value in the inner type's
                        // expected NTScalarArray wrapper.
                        let __wrap_sid = match <#col_tys as #krate::nt::TypedNT>::descriptor() {
                            __rt::FieldDesc::Structure { struct_id, .. } => struct_id,
                            _ => "epics:nt/NTScalarArray:1.0".to_string(),
                        };
                        let mut __wrap = __rt::PvStructure::new(&__wrap_sid);
                        __wrap.fields.push(("value".into(), raw.clone()));
                        let __wrapped = __rt::PvField::Structure(__wrap);
                        <#col_tys as #krate::nt::TypedNT>::from_pv_field(&__wrapped)
                            .map_err(|e| __rt::wrong_type(#col_names, &e.to_string()))?
                    };
                )*
                Ok(Self {
                    #( #col_idents, )*
                })
            }
        }
    };
    expanded.into()
}
