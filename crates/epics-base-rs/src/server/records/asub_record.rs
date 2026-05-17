use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Number of aSub channels. C `aSubRecord.c`: `NUM_ARGS == 21`
/// (inputs `A..U`, outputs `VALA..VALU`).
const NUM_ARGS: usize = 21;

/// `menuFtype` index for `DOUBLE`. C `aSubRecord` initialises every
/// `FTx`/`FTVx` to `DOUBLE`.
const FTYPE_DOUBLE: i16 = 10;

/// The per-channel suffix letters `A..U`.
const SUFFIX: [char; NUM_ARGS] = [
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S',
    'T', 'U',
];

/// aSub (array subroutine) record.
///
/// C `aSubRecord.c` exposes 21 input channels `A..U` (fed from
/// `INPA..INPU`) and 21 output channels `VALA..VALU` (driven to
/// `OUTA..OUTU`). Each channel has:
///   * `FTx` / `FTVx` — `menuFtype` element type (H-5);
///   * `NOx` / `NOVx` — maximum element count;
///   * `NEx` / `NEVx` — elements actually used;
/// so a channel can carry CHAR/SHORT/LONG/FLOAT/DOUBLE/STRING data,
/// not only `DOUBLE`. The per-channel value is stored as an
/// [`EpicsValue`], which represents any scalar or array type — the
/// `FTx` field selects the interpretation. The subroutine itself is
/// invoked by the framework (`RecordInstance::subroutine`).
pub struct ASubRecord {
    pub val: f64,
    pub snam: String,
    pub inam: String,
    /// Input links `INPA..INPU`.
    pub inp: [String; NUM_ARGS],
    /// Input values `A..U` — native typed via `fta`.
    pub a: [EpicsValue; NUM_ARGS],
    /// Output values `VALA..VALU` — native typed via `ftva`.
    pub vala: [EpicsValue; NUM_ARGS],
    /// Output links `OUTA..OUTU`.
    pub out: [String; NUM_ARGS],
    /// Input element-type menu `FTA..FTU` (`menuFtype`).
    pub fta: [i16; NUM_ARGS],
    /// Output element-type menu `FTVA..FTVU` (`menuFtype`).
    pub ftva: [i16; NUM_ARGS],
    /// Input max-element counts `NOA..NOU`.
    pub noa: [i32; NUM_ARGS],
    /// Output max-element counts `NOVA..NOVU`.
    pub nova: [i32; NUM_ARGS],
    /// Input elements-used `NEA..NEU`.
    pub nea: [i32; NUM_ARGS],
    /// Output elements-used `NEVA..NEVU`.
    pub neva: [i32; NUM_ARGS],
}

impl Default for ASubRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            snam: String::new(),
            inam: String::new(),
            inp: std::array::from_fn(|_| String::new()),
            a: std::array::from_fn(|_| EpicsValue::Double(0.0)),
            vala: std::array::from_fn(|_| EpicsValue::DoubleArray(Vec::new())),
            out: std::array::from_fn(|_| String::new()),
            fta: [FTYPE_DOUBLE; NUM_ARGS],
            ftva: [FTYPE_DOUBLE; NUM_ARGS],
            noa: [1; NUM_ARGS],
            nova: [1; NUM_ARGS],
            nea: [1; NUM_ARGS],
            neva: [1; NUM_ARGS],
        }
    }
}

/// Parse a per-channel field name into `(prefix, channel index)`.
/// E.g. `"INPC"` -> `("INP", 2)`, `"VALU"` -> `("VAL", 20)`,
/// `"M"` -> `("", 12)`.
fn parse_channel(name: &str) -> Option<(&'static str, usize)> {
    const PREFIXES: [&str; 7] = ["INP", "OUT", "VAL", "FTV", "FT", "NOV", "NO"];
    // Single-letter input value field A..U.
    if name.len() == 1 {
        let c = name.chars().next().unwrap();
        return SUFFIX.iter().position(|&s| s == c).map(|i| ("", i));
    }
    // NEA..NEU and NEVA..NEVU.
    if let Some(rest) = name.strip_prefix("NEV") {
        if rest.len() == 1 {
            let c = rest.chars().next().unwrap();
            return SUFFIX.iter().position(|&s| s == c).map(|i| ("NEV", i));
        }
        return None;
    }
    if let Some(rest) = name.strip_prefix("NE") {
        if rest.len() == 1 {
            let c = rest.chars().next().unwrap();
            return SUFFIX.iter().position(|&s| s == c).map(|i| ("NE", i));
        }
        return None;
    }
    for prefix in PREFIXES {
        if let Some(rest) = name.strip_prefix(prefix) {
            if rest.len() == 1 {
                let c = rest.chars().next().unwrap();
                if let Some(i) = SUFFIX.iter().position(|&s| s == c) {
                    return Some((prefix, i));
                }
            }
        }
    }
    None
}

/// Surface an output array channel: empty -> scalar 0.0, single
/// element -> scalar, otherwise the array (matching the legacy
/// access shape clients expect).
fn channel_get(v: &EpicsValue) -> EpicsValue {
    match v {
        EpicsValue::DoubleArray(a) if a.is_empty() => EpicsValue::Double(0.0),
        EpicsValue::DoubleArray(a) if a.len() == 1 => EpicsValue::Double(a[0]),
        other => other.clone(),
    }
}

impl ASubRecord {
    /// Build the static field-descriptor table (21 of each per-channel
    /// field plus VAL/SNAM/INAM).
    fn descriptors() -> Vec<FieldDesc> {
        let mut v = vec![
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
            FieldDesc {
                name: "INAM",
                dbf_type: DbFieldType::String,
                read_only: false,
            },
        ];
        // Per-channel field families. Names are leaked to obtain the
        // 'static lifetime FieldDesc requires; the table is built once.
        for &c in SUFFIX.iter() {
            let mk = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
            v.push(FieldDesc {
                name: mk(format!("INP{c}")),
                dbf_type: DbFieldType::String,
                read_only: false,
            });
            v.push(FieldDesc {
                name: mk(c.to_string()),
                dbf_type: DbFieldType::Double,
                read_only: false,
            });
            v.push(FieldDesc {
                name: mk(format!("VAL{c}")),
                dbf_type: DbFieldType::Double,
                read_only: false,
            });
            v.push(FieldDesc {
                name: mk(format!("OUT{c}")),
                dbf_type: DbFieldType::String,
                read_only: false,
            });
            v.push(FieldDesc {
                name: mk(format!("FT{c}")),
                dbf_type: DbFieldType::Short,
                read_only: false,
            });
            v.push(FieldDesc {
                name: mk(format!("FTV{c}")),
                dbf_type: DbFieldType::Short,
                read_only: false,
            });
            v.push(FieldDesc {
                name: mk(format!("NO{c}")),
                dbf_type: DbFieldType::Long,
                read_only: false,
            });
            v.push(FieldDesc {
                name: mk(format!("NOV{c}")),
                dbf_type: DbFieldType::Long,
                read_only: false,
            });
            v.push(FieldDesc {
                name: mk(format!("NE{c}")),
                dbf_type: DbFieldType::Long,
                read_only: true,
            });
            v.push(FieldDesc {
                name: mk(format!("NEV{c}")),
                dbf_type: DbFieldType::Long,
                read_only: true,
            });
        }
        v
    }
}

impl Record for ASubRecord {
    fn record_type(&self) -> &'static str {
        "aSub"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Subroutine is invoked externally via RecordInstance.subroutine.
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => return Some(EpicsValue::Double(self.val)),
            "SNAM" => return Some(EpicsValue::String(self.snam.clone())),
            "INAM" => return Some(EpicsValue::String(self.inam.clone())),
            _ => {}
        }
        let (prefix, idx) = parse_channel(name)?;
        Some(match prefix {
            "" => self.a[idx].clone(), // input value A..U
            "INP" => EpicsValue::String(self.inp[idx].clone()),
            "VAL" => channel_get(&self.vala[idx]),
            "OUT" => EpicsValue::String(self.out[idx].clone()),
            "FT" => EpicsValue::Short(self.fta[idx]),
            "FTV" => EpicsValue::Short(self.ftva[idx]),
            "NO" => EpicsValue::Long(self.noa[idx]),
            "NOV" => EpicsValue::Long(self.nova[idx]),
            "NE" => EpicsValue::Long(self.nea[idx]),
            "NEV" => EpicsValue::Long(self.neva[idx]),
            _ => return None,
        })
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                return Ok(());
            }
            "SNAM" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.snam = s;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                };
            }
            "INAM" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.inam = s;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                };
            }
            _ => {}
        }
        let (prefix, idx) =
            parse_channel(name).ok_or_else(|| CaError::FieldNotFound(name.to_string()))?;
        match prefix {
            // Input value A..U — store native; track NEx (elements used).
            "" => {
                self.nea[idx] = value.count().max(1) as i32;
                self.a[idx] = value;
            }
            "INP" | "OUT" => {
                let s = match value {
                    EpicsValue::String(s) => s,
                    _ => return Err(CaError::TypeMismatch(name.into())),
                };
                if prefix == "INP" {
                    self.inp[idx] = s;
                } else {
                    self.out[idx] = s;
                }
            }
            // Output value VALA..VALU — store native; track NEVx.
            "VAL" => {
                self.neva[idx] = value.count().max(1) as i32;
                self.vala[idx] = value;
            }
            "FT" | "FTV" | "NO" | "NOV" | "NE" | "NEV" => {
                let v = match value {
                    EpicsValue::Short(v) => v as i32,
                    EpicsValue::Long(v) => v,
                    other => other
                        .to_f64()
                        .map(|f| f as i32)
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?,
                };
                match prefix {
                    "FT" => self.fta[idx] = v as i16,
                    "FTV" => self.ftva[idx] = v as i16,
                    "NO" => self.noa[idx] = v,
                    "NOV" => self.nova[idx] = v,
                    "NE" => self.nea[idx] = v,
                    "NEV" => self.neva[idx] = v,
                    _ => unreachable!(),
                }
            }
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        Ok(())
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        use std::sync::OnceLock;
        static FIELDS: OnceLock<Vec<FieldDesc>> = OnceLock::new();
        FIELDS.get_or_init(ASubRecord::descriptors)
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        use std::sync::OnceLock;
        static PAIRS: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
        PAIRS.get_or_init(|| {
            SUFFIX
                .iter()
                .map(|&c| {
                    let link: &'static str = Box::leak(format!("INP{c}").into_boxed_str());
                    let val: &'static str = Box::leak(c.to_string().into_boxed_str());
                    (link, val)
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H-4: input/output channels M..U exist (C `NUM_ARGS == 21`).
    #[test]
    fn channels_m_through_u_present() {
        let mut rec = ASubRecord::default();
        for name in ["M", "Q", "U"] {
            rec.put_field(name, EpicsValue::Double(3.0)).unwrap();
            assert_eq!(rec.get_field(name), Some(EpicsValue::Double(3.0)));
        }
        for name in ["VALM", "VALU"] {
            rec.put_field(name, EpicsValue::DoubleArray(vec![1.0, 2.0]))
                .unwrap();
            assert_eq!(
                rec.get_field(name),
                Some(EpicsValue::DoubleArray(vec![1.0, 2.0]))
            );
        }
        for name in ["NOM", "NOVU", "INPR", "OUTT"] {
            assert!(rec.get_field(name).is_some(), "channel field must exist");
        }
    }

    /// H-5: a non-double channel keeps its native type. `FTVA=STRING`
    /// with a string output value is represented faithfully.
    #[test]
    fn non_double_channel_keeps_native_type() {
        let mut rec = ASubRecord::default();
        rec.put_field("FTA", EpicsValue::Short(0)).unwrap(); // STRING
        rec.put_field("A", EpicsValue::String("hello".into()))
            .unwrap();
        assert_eq!(rec.get_field("A"), Some(EpicsValue::String("hello".into())));
        assert_eq!(rec.fta[0], 0);

        rec.put_field("FTC", EpicsValue::Short(5)).unwrap(); // LONG
        rec.put_field("C", EpicsValue::LongArray(vec![10, 20, 30]))
            .unwrap();
        assert_eq!(
            rec.get_field("C"),
            Some(EpicsValue::LongArray(vec![10, 20, 30]))
        );
        // NEC tracks elements used.
        assert_eq!(rec.get_field("NEC"), Some(EpicsValue::Long(3)));
    }

    /// All 21 input channels feed `multi_input_links`.
    #[test]
    fn twenty_one_multi_input_links() {
        let rec = ASubRecord::default();
        assert_eq!(rec.multi_input_links().len(), 21);
    }
}
