//! `epics:nt/NTURI:1.0` builder.
//!
//! pvxs uses NTURI for RPC-style argument passing — query: struct of
//! typed fields, plus scheme/authority/path strings.

use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

#[derive(Clone)]
struct UriArg {
    name: String,
    desc: FieldDesc,
}

/// Builder for `NTURI`. Each `arg` adds a named field to the `query`
/// sub-structure with the given type. `build()` returns the FieldDesc
/// shape; `create()` returns a default-initialised value with the
/// scheme/authority/path strings empty.
pub struct NTURI {
    args: Vec<UriArg>,
}

impl NTURI {
    pub fn new() -> Self {
        Self { args: Vec::new() }
    }

    pub fn arg(mut self, name: impl Into<String>, desc: FieldDesc) -> Self {
        self.args.push(UriArg {
            name: name.into(),
            desc,
        });
        self
    }

    pub fn arg_scalar(self, name: impl Into<String>, ty: ScalarType) -> Self {
        self.arg(name, FieldDesc::Scalar(ty))
    }

    pub fn build(&self) -> FieldDesc {
        let query_fields: Vec<(String, FieldDesc)> = self
            .args
            .iter()
            .map(|a| (a.name.clone(), a.desc.clone()))
            .collect();
        FieldDesc::Structure {
            struct_id: "epics:nt/NTURI:1.0".into(),
            fields: vec![
                ("scheme".into(), FieldDesc::Scalar(ScalarType::String)),
                ("authority".into(), FieldDesc::Scalar(ScalarType::String)),
                ("path".into(), FieldDesc::Scalar(ScalarType::String)),
                (
                    "query".into(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: query_fields,
                    },
                ),
            ],
        }
    }

    pub fn create(&self) -> PvField {
        let mut root = PvStructure::new("epics:nt/NTURI:1.0");
        for fld in [("scheme", ""), ("authority", ""), ("path", "")] {
            root.fields.push((
                fld.0.into(),
                PvField::Scalar(ScalarValue::String(fld.1.into())),
            ));
        }
        let mut query = PvStructure::new("");
        for a in &self.args {
            // Route every query member through the shared recursive
            // descriptor→default-value helper so a non-scalar query arg
            // (nested structure, structure array, union, bounded string)
            // gets a descriptor-consistent default instead of a bare
            // `Null`. pvxs `NTURI::create()` delegates to
            // `build().create()` (`src/pvxs/nt.h:170-182`), which fills
            // every member from its type — so a `query.nested` advertised
            // as a structure must materialize as an (empty) structure, not
            // a value that mismatches its own descriptor.
            query.fields.push((
                a.name.clone(),
                crate::pvdata::encode::default_value_for(&a.desc),
            ));
        }
        root.fields
            .push(("query".into(), PvField::Structure(query)));
        PvField::Structure(root)
    }

    /// Build a complete NTURI RPC request — descriptor **and** value —
    /// for the given `scheme` / `path` and string-keyed scalar `query`
    /// members.
    ///
    /// This is the request-side counterpart to [`NTURI::build`] /
    /// [`NTURI::create`] (which produce a type template with empty/
    /// default values): it stamps the concrete `scheme` and `path` and
    /// the supplied query values, while still emitting **all four**
    /// normative members — `scheme`, `authority`, `path`, `query` — that
    /// pvxs `NTURI::NTURI()` defines unconditionally
    /// (`src/nt.cpp:253-263`). `authority` is the empty string, matching
    /// pvxs `pvxcall`, which builds `nt::NTURI({}).build()` and sets only
    /// scheme/path/query (`tools/call.cpp:104-118`).
    ///
    /// Routing the CLI request builders through this single owner makes
    /// the presence of `authority` an invariant by construction: a caller
    /// cannot emit an NTURI that omits it. The value's members are
    /// pushed in descriptor order (`scheme`, `authority`, `path`,
    /// `query`) so the value satisfies its own descriptor.
    pub fn request(
        scheme: &str,
        path: &str,
        query: &[(String, ScalarValue)],
    ) -> (FieldDesc, PvField) {
        // Descriptor: reuse the instance builder so the advertised shape
        // stays single-sourced with `build()`.
        let mut b = NTURI::new();
        for (name, val) in query {
            b = b.arg_scalar(name.clone(), val.scalar_type());
        }
        let desc = b.build();

        let mut root = PvStructure::new("epics:nt/NTURI:1.0");
        root.fields.push((
            "scheme".into(),
            PvField::Scalar(ScalarValue::String(scheme.into())),
        ));
        root.fields.push((
            "authority".into(),
            PvField::Scalar(ScalarValue::String(String::new().into())),
        ));
        root.fields.push((
            "path".into(),
            PvField::Scalar(ScalarValue::String(path.into())),
        ));
        let mut q = PvStructure::new("");
        for (name, val) in query {
            q.fields.push((name.clone(), PvField::Scalar(val.clone())));
        }
        root.fields.push(("query".into(), PvField::Structure(q)));
        (desc, PvField::Structure(root))
    }
}

impl Default for NTURI {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nt_uri_with_two_args_has_query_struct_with_two_fields() {
        let u = NTURI::new()
            .arg_scalar("arg1", ScalarType::UInt)
            .arg_scalar("arg2", ScalarType::String)
            .build();
        if let FieldDesc::Structure { struct_id, fields } = u {
            assert_eq!(struct_id, "epics:nt/NTURI:1.0");
            let query = fields
                .iter()
                .find_map(|(n, d)| if n == "query" { Some(d) } else { None })
                .expect("query");
            if let FieldDesc::Structure {
                fields: qfields, ..
            } = query
            {
                assert_eq!(qfields.len(), 2);
                let names: Vec<&str> = qfields.iter().map(|(n, _)| n.as_str()).collect();
                assert_eq!(names, vec!["arg1", "arg2"]);
            }
        }
    }

    #[test]
    fn nt_uri_create_value_matches_descriptor_for_nonscalar_args() {
        // Pre-fix `create()` filled non-scalar query args with `Null`,
        // which mismatches the advertised descriptor. The shared
        // descriptor→default helper now materializes a structure/array
        // default, so the created value must satisfy `build()`.
        use crate::pvdata::value_matches_descriptor;

        let nested = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![
                ("a".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("b".into(), FieldDesc::Scalar(ScalarType::String)),
            ],
        };
        let sarr = FieldDesc::StructureArray {
            struct_id: String::new(),
            fields: vec![("x".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let uri = NTURI::new()
            .arg("nested", nested)
            .arg("rows", sarr)
            .arg_scalar("flat", ScalarType::UInt);

        let desc = uri.build();
        let value = uri.create();

        // The whole created value must be descriptor-consistent.
        value_matches_descriptor(&value, &desc)
            .expect("NTURI::create value must match its own descriptor");

        // The nested structure arg must be an (empty-defaulted) structure,
        // not a Null.
        let PvField::Structure(root) = &value else {
            panic!("NTURI value must be a structure");
        };
        let Some(PvField::Structure(query)) = root.get_field("query") else {
            panic!("query must be a structure");
        };
        assert!(
            matches!(query.get_field("nested"), Some(PvField::Structure(_))),
            "nested query arg must default to a structure, not Null"
        );
    }

    #[test]
    fn request_emits_all_four_normative_members_with_authority() {
        use crate::pvdata::value_matches_descriptor;

        let (desc, value) = NTURI::request(
            "pva",
            "some:rpc:service",
            &[
                ("op".into(), ScalarValue::String("channels".into())),
                ("gain".into(), ScalarValue::String("2".into())),
            ],
        );

        // Descriptor must declare scheme, authority, path, query — the
        // `authority` member is the whole point of this fix.
        let FieldDesc::Structure { struct_id, fields } = &desc else {
            panic!("NTURI request descriptor must be a structure");
        };
        assert_eq!(struct_id, "epics:nt/NTURI:1.0");
        let top_names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(top_names, vec!["scheme", "authority", "path", "query"]);

        // Value must carry the same four members and satisfy its own
        // descriptor (member order matches).
        value_matches_descriptor(&value, &desc)
            .expect("NTURI request value must match its own descriptor");
        let PvField::Structure(root) = &value else {
            panic!("NTURI request value must be a structure");
        };
        let val_names: Vec<&str> = root.fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(val_names, vec!["scheme", "authority", "path", "query"]);

        // scheme/path carry the supplied values; authority is present and
        // empty (pvxs pvxcall leaves it empty).
        assert!(matches!(
            root.get_field("scheme"),
            Some(PvField::Scalar(ScalarValue::String(s))) if s == "pva"
        ));
        assert!(matches!(
            root.get_field("path"),
            Some(PvField::Scalar(ScalarValue::String(s))) if s == "some:rpc:service"
        ));
        assert!(matches!(
            root.get_field("authority"),
            Some(PvField::Scalar(ScalarValue::String(s))) if s.is_empty()
        ));
    }
}
