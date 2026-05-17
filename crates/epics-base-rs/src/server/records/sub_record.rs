use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Number of subroutine input arguments. C `subRecord.c`:
/// `#define INP_ARG_MAX 21` — fields `A..U` / `INPA..INPU`.
const INP_ARG_MAX: usize = 21;

/// The 21 input value field names `A..U`.
const VAL_NAMES: [&str; INP_ARG_MAX] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U",
];

/// The 21 input link field names `INPA..INPU`.
const INP_NAMES: [&str; INP_ARG_MAX] = [
    "INPA", "INPB", "INPC", "INPD", "INPE", "INPF", "INPG", "INPH", "INPI", "INPJ", "INPK", "INPL",
    "INPM", "INPN", "INPO", "INPP", "INPQ", "INPR", "INPS", "INPT", "INPU",
];

/// (INP link, value field) pairs for the 21 channels.
const INP_VAL_PAIRS: [(&str, &str); INP_ARG_MAX] = [
    ("INPA", "A"),
    ("INPB", "B"),
    ("INPC", "C"),
    ("INPD", "D"),
    ("INPE", "E"),
    ("INPF", "F"),
    ("INPG", "G"),
    ("INPH", "H"),
    ("INPI", "I"),
    ("INPJ", "J"),
    ("INPK", "K"),
    ("INPL", "L"),
    ("INPM", "M"),
    ("INPN", "N"),
    ("INPO", "O"),
    ("INPP", "P"),
    ("INPQ", "Q"),
    ("INPR", "R"),
    ("INPS", "S"),
    ("INPT", "T"),
    ("INPU", "U"),
];

/// Sub (subroutine) record — calls a named subroutine function on
/// process. C `subRecord.c` exposes 21 inputs `A..U` fed from links
/// `INPA..INPU`; the subroutine itself is invoked by the framework
/// (`RecordInstance::subroutine`).
pub struct SubRecord {
    pub val: f64,
    pub snam: String,
    /// Input links `INPA..INPU`.
    pub inp: [String; INP_ARG_MAX],
    /// Input values `A..U`.
    pub a: [f64; INP_ARG_MAX],
}

impl Default for SubRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            snam: String::new(),
            inp: std::array::from_fn(|_| String::new()),
            a: [0.0; INP_ARG_MAX],
        }
    }
}

static SUB_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SNAM",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    // INPA..INPU
    field_str("INPA"),
    field_str("INPB"),
    field_str("INPC"),
    field_str("INPD"),
    field_str("INPE"),
    field_str("INPF"),
    field_str("INPG"),
    field_str("INPH"),
    field_str("INPI"),
    field_str("INPJ"),
    field_str("INPK"),
    field_str("INPL"),
    field_str("INPM"),
    field_str("INPN"),
    field_str("INPO"),
    field_str("INPP"),
    field_str("INPQ"),
    field_str("INPR"),
    field_str("INPS"),
    field_str("INPT"),
    field_str("INPU"),
    // A..U
    field_dbl("A"),
    field_dbl("B"),
    field_dbl("C"),
    field_dbl("D"),
    field_dbl("E"),
    field_dbl("F"),
    field_dbl("G"),
    field_dbl("H"),
    field_dbl("I"),
    field_dbl("J"),
    field_dbl("K"),
    field_dbl("L"),
    field_dbl("M"),
    field_dbl("N"),
    field_dbl("O"),
    field_dbl("P"),
    field_dbl("Q"),
    field_dbl("R"),
    field_dbl("S"),
    field_dbl("T"),
    field_dbl("U"),
];

const fn field_str(name: &'static str) -> FieldDesc {
    FieldDesc {
        name,
        dbf_type: DbFieldType::String,
        read_only: false,
    }
}

const fn field_dbl(name: &'static str) -> FieldDesc {
    FieldDesc {
        name,
        dbf_type: DbFieldType::Double,
        read_only: false,
    }
}

impl Record for SubRecord {
    fn record_type(&self) -> &'static str {
        "sub"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // The subroutine is invoked by the framework via
        // `RecordInstance::subroutine` (it needs the registry of
        // named functions, which the record does not own).
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => return Some(EpicsValue::Double(self.val)),
            "SNAM" => return Some(EpicsValue::String(self.snam.clone())),
            _ => {}
        }
        if let Some(idx) = INP_NAMES.iter().position(|&n| n == name) {
            return Some(EpicsValue::String(self.inp[idx].clone()));
        }
        if let Some(idx) = VAL_NAMES.iter().position(|&n| n == name) {
            return Some(EpicsValue::Double(self.a[idx]));
        }
        None
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                return match value {
                    EpicsValue::Double(v) => {
                        self.val = v;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch("VAL".into())),
                };
            }
            "SNAM" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.snam = s;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch("SNAM".into())),
                };
            }
            _ => {}
        }
        if let Some(idx) = INP_NAMES.iter().position(|&n| n == name) {
            return match value {
                EpicsValue::String(s) => {
                    self.inp[idx] = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            };
        }
        if let Some(idx) = VAL_NAMES.iter().position(|&n| n == name) {
            self.a[idx] = value
                .to_f64()
                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
            return Ok(());
        }
        Err(CaError::FieldNotFound(name.to_string()))
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        SUB_FIELDS
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &INP_VAL_PAIRS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H-3: inputs M..U and INPM..INPU exist (C `INP_ARG_MAX == 21`).
    #[test]
    fn inputs_m_through_u_present() {
        let mut rec = SubRecord::default();
        for name in ["M", "Q", "U"] {
            rec.put_field(name, EpicsValue::Double(2.5)).unwrap();
            assert_eq!(rec.get_field(name), Some(EpicsValue::Double(2.5)));
        }
        for name in ["INPM", "INPR", "INPU"] {
            rec.put_field(name, EpicsValue::String("src".into()))
                .unwrap();
            assert_eq!(rec.get_field(name), Some(EpicsValue::String("src".into())));
        }
    }

    /// All 21 input channels are wired into `multi_input_links`.
    #[test]
    fn twenty_one_multi_input_links() {
        let rec = SubRecord::default();
        assert_eq!(rec.multi_input_links().len(), 21);
    }
}
