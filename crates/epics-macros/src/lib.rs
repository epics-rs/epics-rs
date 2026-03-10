use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Data, Fields, Lit};

/// Derive macro that implements the `Record` trait for a struct.
///
/// # Attributes
///
/// - `#[record(type = "ai")]` — sets the record type name
/// - `#[record(type = "ai", crate_path = "my_crate")]` — override crate path
/// - `#[field(type = "Double")]` — sets the DBR type for a field
/// - `#[field(type = "Double", read_only)]` — marks a field as read-only
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
}

struct FieldInfo {
    ident: syn::Ident,
    epics_name: String,
    dbf_type: String,
    read_only: bool,
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
            _ => return Err(syn::Error::new_spanned(input, "EpicsRecord requires named fields")),
        },
        _ => return Err(syn::Error::new_spanned(input, "EpicsRecord can only be derived for structs")),
    };

    let mut field_infos = Vec::new();
    for f in &fields.named {
        let ident = f.ident.as_ref().unwrap().clone();
        let (dbf_type, read_only) = parse_field_attrs(f)?;
        let epics_name = ident.to_string().to_uppercase();
        field_infos.push(FieldInfo {
            ident,
            epics_name,
            dbf_type,
            read_only,
        });
    }

    let field_count = field_infos.len();

    // Generate field_list entries
    let field_descs: Vec<_> = field_infos.iter().map(|fi| {
        let name_str = &fi.epics_name;
        let dbf = dbf_type_ident(&fi.dbf_type);
        let ro = fi.read_only;
        quote! {
            #krate::server::record::FieldDesc {
                name: #name_str,
                dbf_type: #krate::types::DbFieldType::#dbf,
                read_only: #ro,
            }
        }
    }).collect();

    // Generate get_field match arms
    let get_arms: Vec<_> = field_infos.iter().map(|fi| {
        let epics_name = &fi.epics_name;
        let ident = &fi.ident;
        let conversion = value_to_epics(&krate, &fi.dbf_type, quote!(self.#ident));
        quote! {
            #epics_name => Some(#conversion),
        }
    }).collect();

    // Generate put_field match arms
    let put_arms: Vec<_> = field_infos.iter().map(|fi| {
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
    }).collect();

    let expanded = quote! {
        impl #krate::server::record::Record for #name {
            fn record_type(&self) -> &'static str {
                #record_type_str
            }

            fn field_list(&self) -> &'static [#krate::server::record::FieldDesc] {
                static FIELDS: [#krate::server::record::FieldDesc; #field_count] = [
                    #(#field_descs),*
                ];
                &FIELDS
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
        }
    };

    Ok(expanded)
}

fn parse_record_attrs(input: &DeriveInput) -> syn::Result<RecordAttrs> {
    let mut record_type = None;
    let mut crate_path = None;

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
                } else {
                    Err(meta.error("expected `type` or `crate_path`"))
                }
            })?;
        }
    }

    let record_type = record_type.ok_or_else(|| {
        syn::Error::new_spanned(input, "missing #[record(type = \"...\")]")
    })?;

    Ok(RecordAttrs { record_type, crate_path })
}

fn parse_field_attrs(field: &syn::Field) -> syn::Result<(String, bool)> {
    let mut dbf_type = None;
    let mut read_only = false;

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
                } else {
                    Err(meta.error("expected `type` or `read_only`"))
                }
            })?;
        }
    }

    let dbf_type = dbf_type.ok_or_else(|| {
        syn::Error::new_spanned(field, "missing #[field(type = \"...\")]")
    })?;

    Ok((dbf_type, read_only))
}

fn dbf_type_ident(type_str: &str) -> proc_macro2::Ident {
    proc_macro2::Ident::new(type_str, proc_macro2::Span::call_site())
}

fn value_to_epics(krate: &proc_macro2::TokenStream, dbf_type: &str, field_expr: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    match dbf_type {
        "Double" => quote! { #krate::types::EpicsValue::Double(#field_expr) },
        "Float" => quote! { #krate::types::EpicsValue::Float(#field_expr) },
        "Short" => quote! { #krate::types::EpicsValue::Short(#field_expr) },
        "Long" => quote! { #krate::types::EpicsValue::Long(#field_expr) },
        "Char" => quote! { #krate::types::EpicsValue::Char(#field_expr) },
        "Enum" => quote! { #krate::types::EpicsValue::Enum(#field_expr) },
        "String" => quote! { #krate::types::EpicsValue::String(#field_expr.clone()) },
        _ => quote! { compile_error!("unknown field type") },
    }
}

fn value_from_epics(krate: &proc_macro2::TokenStream, dbf_type: &str, field_ident: &syn::Ident) -> proc_macro2::TokenStream {
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

    let variant = match dbf_type {
        "Double" => "Double",
        "Float" => "Float",
        "Short" => "Short",
        "Long" => "Long",
        "Char" => "Char",
        "String" => "String",
        _ => return quote! { compile_error!("unknown field type"); },
    };

    let variant_ident = proc_macro2::Ident::new(variant, proc_macro2::Span::call_site());

    quote! {
        if let #krate::types::EpicsValue::#variant_ident(v) = value {
            self.#field_ident = v;
        } else {
            return Err(#krate::error::CaError::TypeMismatch(
                stringify!(#field_ident).to_uppercase().to_string()
            ));
        }
    }
}
