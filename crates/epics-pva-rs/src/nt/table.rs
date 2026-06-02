//! `epics:nt/NTTable:1.0` builder with column accumulation.
//!
//! Mirrors pvxs nt.cpp `NTTable::add_column()` + `build()`.

use super::meta::{alarm_default, alarm_desc, time_default, time_desc};
use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

#[derive(Clone)]
struct Column {
    code: ScalarType,
    name: String,
    label: String,
}

/// Builder for `NTTable`. Each `add_column` appends one named column;
/// `build()` / `create()` produce a structure with `labels` (string
/// array of all labels) and `value` (struct of column arrays).
pub struct NTTable {
    columns: Vec<Column>,
}

impl NTTable {
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
        }
    }

    pub fn add_column(
        mut self,
        code: ScalarType,
        name: impl Into<String>,
        label: Option<&str>,
    ) -> Self {
        let name = name.into();
        let label = label.unwrap_or(name.as_str()).to_string();
        self.columns.push(Column { code, name, label });
        self
    }

    pub fn build(&self) -> FieldDesc {
        let value_fields: Vec<(String, FieldDesc)> = self
            .columns
            .iter()
            .map(|c| (c.name.clone(), FieldDesc::ScalarArray(c.code)))
            .collect();
        FieldDesc::Structure {
            struct_id: "epics:nt/NTTable:1.0".into(),
            fields: vec![
                ("labels".into(), FieldDesc::ScalarArray(ScalarType::String)),
                (
                    "value".into(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: value_fields,
                    },
                ),
                ("descriptor".into(), FieldDesc::Scalar(ScalarType::String)),
                ("alarm".into(), alarm_desc()),
                ("timeStamp".into(), time_desc()),
            ],
        }
    }

    pub fn create(&self) -> PvField {
        let mut root = PvStructure::new("epics:nt/NTTable:1.0");
        let labels = self
            .columns
            .iter()
            .map(|c| ScalarValue::String(c.label.clone().into()))
            .collect::<Vec<_>>();
        root.fields
            .push(("labels".into(), PvField::ScalarArray(labels)));
        let mut value_struct = PvStructure::new("");
        for c in &self.columns {
            value_struct
                .fields
                .push((c.name.clone(), PvField::ScalarArray(Vec::new())));
        }
        root.fields
            .push(("value".into(), PvField::Structure(value_struct)));
        root.fields.push((
            "descriptor".into(),
            PvField::Scalar(ScalarValue::String(String::new().into())),
        ));
        root.fields.push(("alarm".into(), alarm_default()));
        root.fields.push(("timeStamp".into(), time_default()));
        PvField::Structure(root)
    }
}

impl Default for NTTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nt_table_three_columns() {
        let t = NTTable::new()
            .add_column(ScalarType::Int, "A", Some("Col A"))
            .add_column(ScalarType::String, "C", Some("Col C"))
            .add_column(ScalarType::String, "B", Some("Col B"));

        if let FieldDesc::Structure { fields, .. } = t.build() {
            let value = fields
                .iter()
                .find_map(|(n, d)| if n == "value" { Some(d) } else { None })
                .expect("value");
            if let FieldDesc::Structure { fields: cols, .. } = value {
                assert_eq!(cols.len(), 3);
                let col_names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
                assert_eq!(col_names, vec!["A", "C", "B"]);
                assert!(matches!(cols[0].1, FieldDesc::ScalarArray(ScalarType::Int)));
                assert!(matches!(
                    cols[1].1,
                    FieldDesc::ScalarArray(ScalarType::String)
                ));
            }
        }
    }

    #[test]
    fn nt_table_labels_match_column_order() {
        let v = NTTable::new()
            .add_column(ScalarType::Int, "A", Some("Col A"))
            .add_column(ScalarType::String, "C", Some("Col C"))
            .add_column(ScalarType::String, "B", Some("Col B"))
            .create();
        if let PvField::Structure(root) = v {
            if let Some(PvField::ScalarArray(labels)) = root.get_field("labels") {
                let strs: Vec<String> = labels
                    .iter()
                    .filter_map(|v| {
                        if let ScalarValue::String(s) = v {
                            Some(s.as_str_lossy().into_owned())
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(strs, vec!["Col A", "Col C", "Col B"]);
            }
        }
    }
}
