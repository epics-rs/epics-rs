use crate::error::{CaError, CaResult};
use crate::server::record::{FieldMetadataOverride, InputFetchPolicy, ProcessOutcome, Record};
use crate::types::{EpicsValue, PvString};

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

/// `EFLG` (`menu(aSubEFLG)`) — when the record posts output-array DB events.
/// C `aSubRecord.c::monitor` (the `switch (prec->eflg)`).
const EFLG_NEVER: i16 = 0;
const EFLG_ON_CHANGE: i16 = 1;
const EFLG_ALWAYS: i16 = 2;

/// `menu(aSubEFLG)` choice labels (`aSubRecord.dbd.pod`).
const ASUB_EFLG_CHOICES: &[&str] = &["NEVER", "ON CHANGE", "ALWAYS"];

/// `menu(aSubLFLG)` choice labels (`aSubRecord.dbd.pod`): IGNORE=0 (resolve
/// SNAM once at init), READ=1 (re-read the name from the SUBL link every
/// process). The READ behaviour is owned by the processing path
/// (`fetch_values`), which holds the link + subroutine registry.
const ASUB_LFLG_CHOICES: &[&str] = &["IGNORE", "READ"];

/// `LFLG == IGNORE` (`menu(aSubLFLG)` index 0): SNAM is resolved at init and on
/// each SNAM put via `special()`. Only in this mode does C `aSubRecord.c::
/// special` run `registryFunctionFind(prec->snam)` and refuse an unregistered
/// name; in READ mode a SNAM put is not validated.
const LFLG_IGNORE: i16 = 0;

/// The output-array fields whose monitor posting `EFLG` gates: `VALA..VALU`
/// (the output values) and `NEVA..NEVU` (their element counts). C
/// `aSubRecord.c::monitor` posts `vala[i]` and `neva[i]` together inside the
/// `EFLG` switch. Leaked once for the `'static` lifetime the posting hooks
/// require (mirrors `multi_input_links`).
fn asub_output_event_fields() -> &'static [&'static str] {
    use std::sync::OnceLock;
    static FIELDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    FIELDS.get_or_init(|| {
        SUFFIX
            .iter()
            .flat_map(|&c| {
                [
                    Box::leak(format!("VAL{c}").into_boxed_str()) as &'static str,
                    Box::leak(format!("NEV{c}").into_boxed_str()) as &'static str,
                ]
            })
            .collect()
    })
}

/// aSub (array subroutine) record.
///
/// C `aSubRecord.c` exposes 21 input channels `A..U` (fed from
/// `INPA..INPU`) and 21 output channels `VALA..VALU` (driven to
/// `OUTA..OUTU`). Each channel has:
///   * `FTx` / `FTVx` — `menuFtype` element type;
///   * `NOx` / `NOVx` — maximum element count;
///   * `NEx` / `NEVx` — elements actually used;
///
/// so a channel can carry CHAR/SHORT/LONG/FLOAT/DOUBLE/STRING data,
/// not only `DOUBLE`. The per-channel value is stored as an
/// [`EpicsValue`], which represents any scalar or array type — the
/// `FTx` field selects the interpretation. The subroutine itself is
/// invoked by the framework (`RecordInstance::subroutine`).
pub struct ASubRecord {
    /// The subroutine return status, published as VAL (`aSubRecord.c:223`
    /// `prec->val = status`). VAL is `DBF_LONG` (`aSubRecord.dbd.pod:126`), i.e.
    /// `epicsInt32`, so a string caput parses via `epicsParseLong` and an
    /// out-of-i32-range or fractional value is REFUSED — modeling it as i32
    /// keys the put on the integer row instead of accepting any double.
    pub val: i32,
    pub snam: PvString,
    pub inam: PvString,
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
    /// Bad-return severity (`menuAlarmSevr`, default NO_ALARM). C
    /// `aSubRecord.c::do_sub` raises `SOFT_ALARM` at this severity when the
    /// subroutine returns a negative status (applied by the framework's
    /// `run_registered_subroutine`, which also publishes the status as VAL).
    pub brsv: i16,
    /// Display precision `PREC` for VAL. C `aSubRecord.c::get_precision`
    /// returns `prec->prec` as the precision of VAL (and any non-link field);
    /// the per-channel value fields use their link's precision, which the port
    /// does not track.
    pub prec: i16,
    /// Output event flag `EFLG` (`menu(aSubEFLG)`, default ON CHANGE). C
    /// `aSubRecord.c::monitor` gates `VALA..VALU`/`NEVA..NEVU` event posting:
    /// NEVER (never post), ON CHANGE (post on change — the framework default),
    /// ALWAYS (post every process).
    pub eflg: i16,
    /// Subroutine-name input mode `LFLG` (`menu(aSubLFLG)`, default IGNORE). C
    /// `aSubRecord.c::fetch_values`: READ re-reads SNAM from the SUBL link
    /// each process and re-resolves the subroutine when it changes (the
    /// framework's processing path performs the read + re-resolution, as it
    /// owns the link and subroutine registry).
    pub lflg: i16,
    /// Subroutine-name input link `SUBL` (`DBF_INLINK`). C `init_record`
    /// seeds SNAM from a constant SUBL; with LFLG=READ, `fetch_values` reads
    /// the current name from this link every process.
    pub subl: String,
    /// Old subroutine name `ONAM`. C `fetch_values` re-resolves the
    /// subroutine only when SNAM differs from ONAM; ONAM is set to SNAM on a
    /// successful swap.
    pub onam: PvString,
    /// This cycle's C `process` status, delivered by the framework's single
    /// `do_sub` owner through [`Record::set_subroutine_status`]. C pushes every
    /// OUT link only under `if (!status)` (aSubRecord.c:232-239), so this is the
    /// whole gate: `0` drives OUTA..OUTU, anything else drives nothing. A record
    /// that has never processed starts non-zero — no outputs before the first
    /// successful `do_sub`.
    sub_status: i64,
}

/// `OUTA..OUTU` -> `VALA..VALU`, the pairs C's push loop walks
/// (`dbPutLink(&(&prec->outa)[i], ..., (&prec->vala)[i], (&prec->neva)[i])`).
fn asub_output_links() -> &'static [(&'static str, &'static str)] {
    use std::sync::OnceLock;
    static PAIRS: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    PAIRS.get_or_init(|| {
        SUFFIX
            .iter()
            .map(|&c| {
                let link: &'static str = Box::leak(format!("OUT{c}").into_boxed_str());
                let val: &'static str = Box::leak(format!("VAL{c}").into_boxed_str());
                (link, val)
            })
            .collect()
    })
}

impl Default for ASubRecord {
    fn default() -> Self {
        Self {
            val: 0,
            snam: PvString::new(),
            inam: PvString::new(),
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
            brsv: 0,
            prec: 0,
            eflg: EFLG_ON_CHANGE,
            lflg: 0,
            subl: String::new(),
            onam: PvString::new(),
            // Never processed: C's `status` is only 0 after a `do_sub` that
            // returned 0, so nothing is driven out before then.
            sub_status: SUBROUTINE_NOT_RUN,
        }
    }
}

/// The `sub_status` of a record that has not yet completed a `do_sub` — any
/// non-zero value; C's `status` is a `long` that only a successful `do_sub`
/// leaves at 0.
const SUBROUTINE_NOT_RUN: i64 = -1;

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

impl Record for ASubRecord {
    fn record_type(&self) -> &'static str {
        "aSub"
    }

    /// `aSubRecord.c::process` has no unconditional UDF re-derive: UDF is
    /// cleared only inside `do_sub` when the subroutine actually ran and
    /// returned `>= 0` (`aSubRecord.c:469-470` `prec->udf = FALSE`). The
    /// framework performs that clear in `run_registered_subroutine` (C
    /// `do_sub`'s home); a process cycle that runs no subroutine — e.g. a
    /// `caput .UDF 1` on a Passive record with no SNAM — sources nothing and
    /// must keep the client's UDF put. Opt out of the per-cycle blanket
    /// re-derive.
    fn clears_udf(&self) -> bool {
        false
    }

    /// `aSubRecord.c` has NO `recGblCheckUdf` anywhere — an undefined aSub
    /// raises NO alarm from UDF (softIoc: a fresh `record(aSub,"X"){}` reports
    /// UDF 1 with STAT/SEVR = NO_ALARM, unlike `sub` which raises UDF/INVALID).
    /// With `clears_udf` false, UDF can now legitimately stay 1 on a cycle that
    /// sourced nothing (or on a negative `do_sub` status), so this MUST be false
    /// or the framework's `rec_gbl_check_udf` would invent an alarm C never
    /// raises. See [`Record::raises_udf_alarm`]; same shape as `event`/`swait`.
    fn raises_udf_alarm(&self) -> bool {
        false
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Subroutine is invoked externally via RecordInstance.subroutine.
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => return Some(EpicsValue::Long(self.val)),
            "SNAM" => return Some(EpicsValue::String(self.snam.clone())),
            "INAM" => return Some(EpicsValue::String(self.inam.clone())),
            "BRSV" => return Some(EpicsValue::Short(self.brsv)),
            "PREC" => return Some(EpicsValue::Short(self.prec)),
            "EFLG" => return Some(EpicsValue::Short(self.eflg)),
            "LFLG" => return Some(EpicsValue::Short(self.lflg)),
            "SUBL" => return Some(EpicsValue::String(self.subl.clone().into())),
            "ONAM" => return Some(EpicsValue::String(self.onam.clone())),
            _ => {}
        }
        let (prefix, idx) = parse_channel(name)?;
        Some(match prefix {
            "" => self.a[idx].clone(), // input value A..U
            "INP" => EpicsValue::String(self.inp[idx].clone().into()),
            "VAL" => channel_get(&self.vala[idx]),
            "OUT" => EpicsValue::String(self.out[idx].clone().into()),
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
                // VAL is DBF_LONG; the string→native put parse already rejects a
                // fractional / out-of-i32 caput in `coerce_put_value` (target is
                // DBF_LONG from `get_field`). Here VAL is also the status field
                // that subroutine closures and the framework assign directly with
                // whatever numeric they hold — C `prec->val = status` truncates
                // any arithmetic value to epicsInt32, so accept any numeric and
                // project it onto DBF_LONG rather than rejecting a Double writer.
                self.val = value
                    .to_dbf_i32()
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
            "BRSV" => {
                self.brsv = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("BRSV".into()))?
                    as i16;
                return Ok(());
            }
            "PREC" => {
                self.prec = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("PREC".into()))?
                    as i16;
                return Ok(());
            }
            "EFLG" => {
                self.eflg = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("EFLG".into()))?
                    as i16;
                return Ok(());
            }
            "LFLG" => {
                self.lflg = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LFLG".into()))?
                    as i16;
                return Ok(());
            }
            "SUBL" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.subl = s.as_str_lossy().into_owned();
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                };
            }
            "ONAM" => {
                return match value {
                    EpicsValue::String(s) => {
                        self.onam = s;
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
                    EpicsValue::String(s) => s.as_str_lossy().into_owned(),
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

    /// C `aSubRecord.c::special` (SPC_MOD on SNAM) resolves the name via
    /// `registryFunctionFind` and returns `S_db_BadSub` for a non-empty
    /// unregistered name — but ONLY when `LFLG == IGNORE` (`:557-558`). In
    /// READ mode the name is read from the SUBL link at process time
    /// (`fetch_values`) and a SNAM put is not validated. The registry lookup
    /// itself is performed by the put owner; see
    /// [`Record::is_subroutine_name_field`].
    fn is_subroutine_name_field(&self, field: &str) -> bool {
        field == "SNAM" && self.lflg == LFLG_IGNORE
    }

    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        // EFLG is `menu(aSubEFLG)`, LFLG is `menu(aSubLFLG)`; serve their
        // labels so a `.db` `field(EFLG,"ON CHANGE")` / `field(LFLG,"READ")`
        // resolves and a client reads them as DBR_ENUM.
        match field {
            "EFLG" => Some(ASUB_EFLG_CHOICES),
            "LFLG" => Some(ASUB_LFLG_CHOICES),
            _ => None,
        }
    }

    fn force_posted_fields(&self) -> &'static [&'static str] {
        // EFLG=ALWAYS: C `monitor()` posts every `vala[i]`/`neva[i]` each
        // process regardless of change — the framework's force-post path.
        if self.eflg == EFLG_ALWAYS {
            asub_output_event_fields()
        } else {
            &[]
        }
    }

    fn event_posted_fields(&self) -> &'static [&'static str] {
        // EFLG=NEVER: C `monitor()` posts no `vala[i]`/`neva[i]` events.
        // Excluding them from generic change-detection suppresses the post.
        // (VAL, the scalar return status, is not in this set — C posts it on
        // change independent of EFLG.)
        if self.eflg == EFLG_NEVER {
            asub_output_event_fields()
        } else {
            &[]
        }
    }

    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        // C `aSubRecord.c::get_precision` reports `prec->prec` as VAL's display
        // precision. (The per-channel value fields use their link's precision
        // in C; the port does not model per-link precision, so only VAL is
        // served here.)
        if field.eq_ignore_ascii_case("VAL") {
            return Some(FieldMetadataOverride {
                precision: Some(self.prec),
                ..Default::default()
            });
        }
        None
    }

    /// C `aSubRecord.c:126-136`: `recGblInitConstantLink(&prec->subl,
    /// DBF_STRING, prec->snam)` followed by the `dbLoadLinkArray` loop over
    /// INPA..INPU. Both are init-only: at process `dbGetLink` on a constant
    /// delivers nothing, so SNAM and A..U keep what was seeded (or what a
    /// client later put).
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        let mut seeds = vec![crate::server::record::ConstantInitLink::new("SUBL", "SNAM")];
        seeds.extend(crate::server::record::seed_input_links(
            self.multi_input_links(),
        ));
        seeds
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

    /// C `aSubRecord.c::fetch_values` (277-289) returns on the first failed
    /// `dbGetLink` and `process` (216-218) then skips `do_sub`.
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::AbortOnFirstFailure
    }

    /// The cycle status from the framework's single `do_sub` owner. Stored
    /// verbatim; [`Self::multi_output_links`] is its only reader.
    fn set_subroutine_status(&mut self, status: i64) {
        self.sub_status = status;
    }

    /// C `aSubRecord.c::process` (232-239) pushes EVERY output link once the
    /// cycle's status is 0:
    ///
    /// ```c
    /// if (!status) {
    ///     for (i = 0; i < NUM_ARGS; i++)
    ///         dbPutLink(&(&prec->outa)[i], (&prec->ftva)[i], (&prec->vala)[i],
    ///             (&prec->neva)[i]);
    /// }
    /// ```
    ///
    /// The port stored OUTA..OUTU and never wrote them, so a subroutine's
    /// results reached VALA..VALU (and CA monitors on them) but no downstream
    /// record. `status` is 0 only when `fetch_values` succeeded AND `do_sub` ran
    /// and returned 0, so a failed input link, an unresolved SNAM and a
    /// subroutine that reported failure each suppress every push — the gate is
    /// the status itself, not a per-link condition. The framework's generic
    /// `multi_output_links` dispatch skips an empty link name, which is C's
    /// `dbPutLink` on an unset link (a no-op).
    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        if self.sub_status == 0 {
            asub_output_links()
        } else {
            &[]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// input/output channels M..U exist (C `NUM_ARGS == 21`).
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

    /// a non-double channel keeps its native type. `FTVA=STRING`
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

    /// PREC round-trips and is served as VAL's display precision (C
    /// `aSubRecord.c::get_precision` returns `prec->prec` for VAL).
    #[test]
    fn prec_round_trips_and_serves_val_precision() {
        let mut rec = ASubRecord::default();
        assert_eq!(rec.get_field("PREC"), Some(EpicsValue::Short(0)));
        assert_eq!(
            rec.field_metadata_override("VAL").and_then(|o| o.precision),
            Some(0)
        );

        rec.put_field("PREC", EpicsValue::Short(4)).unwrap();
        assert_eq!(rec.get_field("PREC"), Some(EpicsValue::Short(4)));
        assert_eq!(
            rec.field_metadata_override("VAL").and_then(|o| o.precision),
            Some(4),
            "PREC must be served as VAL display precision"
        );
        // Non-VAL fields carry no precision override.
        assert!(rec.field_metadata_override("SNAM").is_none());
    }

    /// EFLG gates output-array posting. C `aSubRecord.c::monitor`:
    /// NEVER posts no `VALx`/`NEVx`; ON CHANGE (default) leaves them to the
    /// framework's change-detection; ALWAYS force-posts them every process.
    #[test]
    fn eflg_drives_output_event_posting_policy() {
        let mut rec = ASubRecord::default();
        // Default ON CHANGE: neither force-post nor suppress.
        assert_eq!(rec.eflg, EFLG_ON_CHANGE);
        assert!(rec.force_posted_fields().is_empty());
        assert!(rec.event_posted_fields().is_empty());

        // ALWAYS: VALx/NEVx force-posted every cycle (21 + 21 = 42 fields).
        rec.put_field("EFLG", EpicsValue::Short(EFLG_ALWAYS))
            .unwrap();
        assert_eq!(rec.get_field("EFLG"), Some(EpicsValue::Short(2)));
        assert_eq!(rec.force_posted_fields().len(), NUM_ARGS * 2);
        assert!(rec.force_posted_fields().contains(&"VALA"));
        assert!(rec.force_posted_fields().contains(&"NEVU"));
        assert!(rec.event_posted_fields().is_empty());

        // NEVER: VALx/NEVx excluded from posting; VAL scalar not in the set.
        rec.put_field("EFLG", EpicsValue::Short(EFLG_NEVER))
            .unwrap();
        assert!(rec.force_posted_fields().is_empty());
        assert_eq!(rec.event_posted_fields().len(), NUM_ARGS * 2);
        assert!(rec.event_posted_fields().contains(&"VALA"));
        assert!(!rec.event_posted_fields().contains(&"VAL"));
    }

    /// EFLG loads from a `.db` menu label (C `field(EFLG,"ALWAYS")`).
    #[test]
    fn eflg_loads_from_menu_label() {
        use crate::server::db_loader::{apply_fields, create_record};
        let mut rec = create_record("aSub").unwrap();
        let mut common = Vec::new();
        apply_fields(&mut rec, &[("EFLG".into(), "ALWAYS".into())], &mut common)
            .expect("EFLG menu label must load");
        assert_eq!(rec.get_field("EFLG"), Some(EpicsValue::Short(2)));
    }

    /// BRSV round-trips as a `menuAlarmSevr` index (C `aSubRecord.c` BRSV,
    /// the severity used when the subroutine returns a negative status).
    #[test]
    fn brsv_round_trips() {
        let mut rec = ASubRecord::default();
        assert_eq!(rec.get_field("BRSV"), Some(EpicsValue::Short(0)));
        rec.put_field("BRSV", EpicsValue::Short(3)).unwrap();
        assert_eq!(rec.get_field("BRSV"), Some(EpicsValue::Short(3)));
    }

    /// LFLG/SUBL/ONAM round-trip and default to IGNORE / empty / empty
    /// (C `aSubRecord.c` defaults; ONAM init-copies the empty SNAM).
    #[test]
    fn lflg_subl_onam_round_trip() {
        let mut rec = ASubRecord::default();
        assert_eq!(rec.get_field("LFLG"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("SUBL"), Some(EpicsValue::String("".into())));
        assert_eq!(rec.get_field("ONAM"), Some(EpicsValue::String("".into())));

        rec.put_field("LFLG", EpicsValue::Short(1)).unwrap();
        rec.put_field("SUBL", EpicsValue::String("HOLDER".into()))
            .unwrap();
        rec.put_field("ONAM", EpicsValue::String("prev".into()))
            .unwrap();
        assert_eq!(rec.get_field("LFLG"), Some(EpicsValue::Short(1)));
        assert_eq!(
            rec.get_field("SUBL"),
            Some(EpicsValue::String("HOLDER".into()))
        );
        assert_eq!(
            rec.get_field("ONAM"),
            Some(EpicsValue::String("prev".into()))
        );
    }

    /// C `aSubRecord.c::special` runs `registryFunctionFind(prec->snam)` ONLY
    /// when `LFLG == IGNORE`; a SNAM put in READ mode is not validated, and no
    /// other field is a subroutine name.
    #[test]
    fn snam_is_a_subroutine_name_field_only_in_ignore_mode() {
        let mut rec = ASubRecord::default();
        assert_eq!(rec.lflg, LFLG_IGNORE);
        assert!(rec.is_subroutine_name_field("SNAM"));
        assert!(!rec.is_subroutine_name_field("INAM"));
        assert!(!rec.is_subroutine_name_field("VAL"));

        rec.put_field("LFLG", EpicsValue::Short(1)).unwrap(); // READ
        assert!(
            !rec.is_subroutine_name_field("SNAM"),
            "READ mode reads the name from SUBL at process time; a SNAM put is not validated"
        );
    }

    /// `field(LFLG,"READ")` resolves via the `menu(aSubLFLG)` labels;
    /// `field(SUBL,"...")` loads even though SUBL is `special(SPC_NOMOD)`
    /// (the loader writes through `put_field`, bypassing the runtime gate).
    #[test]
    fn lflg_subl_load_from_db() {
        use crate::server::db_loader::{apply_fields, create_record};
        let mut rec = create_record("aSub").unwrap();
        let mut common = Vec::new();
        apply_fields(
            &mut rec,
            &[
                ("LFLG".into(), "READ".into()),
                ("SUBL".into(), "NAME_HOLDER".into()),
            ],
            &mut common,
        )
        .expect("LFLG menu label and SUBL link must load");
        assert_eq!(rec.get_field("LFLG"), Some(EpicsValue::Short(1)));
        assert_eq!(
            rec.get_field("SUBL"),
            Some(EpicsValue::String("NAME_HOLDER".into()))
        );
    }
}
