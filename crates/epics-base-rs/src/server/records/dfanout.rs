use crate::error::{CaError, CaResult};
use crate::server::record::{CycleCell, CycleRead, CycleValue};
use crate::server::record::{ProcessOutcome, Record};
use crate::types::EpicsValue;

/// Choice labels for the SELM link-selection menu, in index order.
/// C `menu(dfanoutSELM)`: 0=All, 1=Specified, 2=Mask.
const DFANOUT_SELM_CHOICES: &[&str] = &["All", "Specified", "Mask"];

/// Data fanout record — distributes VAL to up to 16 output links
/// (OUTA..OUTP) selected by SELM/SELN. Manual `Record` impl so the
/// analog limit alarms, MDEL/ADEL deadband and UDF check from C
/// `dfanoutRecord.c` are available (the derive macro provided none).
pub struct DfanoutRecord {
    pub val: f64,
    pub selm: i16,
    /// Link selection. C declares SELN as `DBF_USHORT`
    /// (dfanoutRecord.dbd.pod:188): unsigned 0..65535, used as a bit
    /// mask in `SELM=Mask`.
    pub seln: u16,
    pub outa: String,
    pub outb: String,
    pub outc: String,
    pub outd: String,
    pub oute: String,
    pub outf: String,
    pub outg: String,
    pub outh: String,
    pub outi: String,
    pub outj: String,
    pub outk: String,
    pub outl: String,
    pub outm: String,
    pub outn: String,
    pub outo: String,
    pub outp: String,
    pub dol: String,
    pub omsl: i16,
    pub sell: String,
    pub ivoa: i16,
    pub ivov: f64,
    // Limit-alarm configuration (C `dfanoutRecord.c::checkAlarms`).
    pub hihi: f64,
    pub high: f64,
    pub low: f64,
    pub lolo: f64,
    pub hhsv: i16,
    pub hsv: i16,
    pub lsv: i16,
    pub llsv: i16,
    pub hyst: f64,
    pub lalm: f64,
    // Monitor deadband state.
    pub mdel: f64,
    pub adel: f64,
    pub mlst: f64,
    pub alst: f64,
}

impl Default for DfanoutRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            selm: 0,
            // C `dfanoutRecord.dbd.pod` `field(SELN,DBF_USHORT){ initial("1") }`:
            // an unset SELN defaults to 1, not 0. For dfanout Specified the
            // selected output is `seln - 1` (dfanoutRecord.c:316-322), so the
            // C default 1 drives OUTA while 0 would drive nothing.
            seln: 1,
            outa: String::new(),
            outb: String::new(),
            outc: String::new(),
            outd: String::new(),
            oute: String::new(),
            outf: String::new(),
            outg: String::new(),
            outh: String::new(),
            outi: String::new(),
            outj: String::new(),
            outk: String::new(),
            outl: String::new(),
            outm: String::new(),
            outn: String::new(),
            outo: String::new(),
            outp: String::new(),
            dol: String::new(),
            omsl: 0,
            sell: String::new(),
            ivoa: 0,
            ivov: 0.0,
            hihi: 0.0,
            high: 0.0,
            low: 0.0,
            lolo: 0.0,
            hhsv: 0,
            hsv: 0,
            lsv: 0,
            llsv: 0,
            hyst: 0.0,
            lalm: 0.0,
            mdel: 0.0,
            adel: 0.0,
            // `dfanoutRecord.dbd` gives MLST/ALST no `initial()`: C's calloc'd
            // record starts both at 0.0, and `caget D:DF.MLST` on a C IOC reads
            // 0. They are ordinary wire-visible fields, not sentinels — the
            // "nothing posted yet" state lives in `CommonFields::{mlst,alst}`
            // (`Option<f64>`), which the deadband owner falls back to when a
            // record does not declare the field at all. Every other analog record
            // in the port already starts them at 0.0; dfanout alone used NaN,
            // which both served `nan` to clients and made its FIRST deadband check
            // fire where C's (delta = |0 - 0| = 0, not > MDEL) does not.
            mlst: 0.0,
            alst: 0.0,
        }
    }
}

impl DfanoutRecord {
    pub fn new(val: f64) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }

    /// Get all non-empty output link targets.
    pub fn output_links(&self) -> Vec<&str> {
        [
            &self.outa, &self.outb, &self.outc, &self.outd, &self.oute, &self.outf, &self.outg,
            &self.outh, &self.outi, &self.outj, &self.outk, &self.outl, &self.outm, &self.outn,
            &self.outo, &self.outp,
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str())
        .collect()
    }

    fn out_slot_mut(&mut self, name: &str) -> Option<&mut String> {
        Some(match name {
            "OUTA" => &mut self.outa,
            "OUTB" => &mut self.outb,
            "OUTC" => &mut self.outc,
            "OUTD" => &mut self.outd,
            "OUTE" => &mut self.oute,
            "OUTF" => &mut self.outf,
            "OUTG" => &mut self.outg,
            "OUTH" => &mut self.outh,
            "OUTI" => &mut self.outi,
            "OUTJ" => &mut self.outj,
            "OUTK" => &mut self.outk,
            "OUTL" => &mut self.outl,
            "OUTM" => &mut self.outm,
            "OUTN" => &mut self.outn,
            "OUTO" => &mut self.outo,
            "OUTP" => &mut self.outp,
            _ => return None,
        })
    }

    fn out_slot(&self, name: &str) -> Option<&str> {
        Some(match name {
            "OUTA" => &self.outa,
            "OUTB" => &self.outb,
            "OUTC" => &self.outc,
            "OUTD" => &self.outd,
            "OUTE" => &self.oute,
            "OUTF" => &self.outf,
            "OUTG" => &self.outg,
            "OUTH" => &self.outh,
            "OUTI" => &self.outi,
            "OUTJ" => &self.outj,
            "OUTK" => &self.outk,
            "OUTL" => &self.outl,
            "OUTM" => &self.outm,
            "OUTN" => &self.outn,
            "OUTO" => &self.outo,
            "OUTP" => &self.outp,
            _ => return None,
        })
    }
}

impl Record for DfanoutRecord {
    // The menus are stored as `i16` and served as ENUM, so they stay General.
    crate::slot_map!(crate::server::record::dbd_generated::slots::dfanout;
        VAL => val,
        SELN => seln,
        HIHI => hihi,
        LOLO => lolo,
        HIGH => high,
        LOW => low,
        HYST => hyst,
        ADEL => adel,
        MDEL => mdel,
        IVOV => ivov;
        read_only:
        LALM => lalm,
        ALST => alst,
        MLST => mlst);

    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        crate::server::record::CycleHooks::NONE
    }

    /// `monitor()` posts VAL alone (`dfanoutRecord.c:283-298`).
    fn process_posted_fields(&self) -> Option<&'static [&'static str]> {
        Some(&["VAL"])
    }

    /// C `dfanoutRecord.c::init_record` (:96-111) ends without touching
    /// MLST/ALST/LALM, which is the other half of the note on the MLST/ALST
    /// field declarations above: `caget D:DF.MLST` on a C IOC reads 0 even
    /// when the record was loaded with a nonzero VAL.
    fn seed_deadband_tracking(&mut self, _dset_init: crate::server::device_support::DsetInit) {}

    fn has_array_monitor_post(&self) -> bool {
        false
    }

    fn record_type(&self) -> &'static str {
        "dfanout"
    }

    /// C `dfanoutRecord.c:116-122`: the scalar `dbGetLink(&prec->dol, ..., &prec->val, 0, 0)`
    /// under `dol.type != CONSTANT && omsl == menuOmslclosed_loop`.
    fn fetches_dol_closed_loop(&self) -> bool {
        true
    }

    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SELM" => Some(DFANOUT_SELM_CHOICES),
            _ => None,
        }
    }

    fn uses_monitor_deadband(&self) -> bool {
        true
    }

    /// C `dfanoutRecord.c:102-105`:
    ///
    /// ```c
    /// recGblInitConstantLink(&prec->sell, DBF_USHORT, &prec->seln);
    /// if (recGblInitConstantLink(&prec->dol, DBF_DOUBLE, &prec->val))
    ///     prec->udf = FALSE;
    /// ```
    ///
    /// Both links are init-only — at process `dbGetLink` on a constant
    /// delivers nothing — so this table is the only way a constant SELL or DOL
    /// reaches SELN / VAL.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        vec![
            crate::server::record::ConstantInitLink::new("SELL", "SELN"),
            // C `dfanoutRecord.c:105-106`: a successful DOL load also DEFINES
            // the record — `prec->udf = isnan(prec->val)`.
            crate::server::record::ConstantInitLink::dol_to_val("DOL", "VAL"),
        ]
    }

    /// dfanout has no value computation in `process()` itself — VAL is
    /// either set by a CA/DB put or fetched from DOL by the framework's
    /// closed-loop binding. The output distribution to OUTA..OUTP is
    /// handled by the framework's multi-output dispatch, and the IVOA
    /// policy is applied there once `check_alarms` raises INVALID.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    /// C `dfanoutRecord.c::process` sets `udf = isnan(val)` after a DOL
    /// fetch; a NaN VAL keeps UDF set so the framework raises UDF_ALARM.
    fn value_is_undefined(&self) -> bool {
        self.val.is_nan()
    }

    /// ...and that DOL fetch is the ONLY thing in dfanout's `process()` that
    /// touches UDF (`dfanoutRecord.c:116-122`). With no DOL — or a constant one,
    /// which `dbGetLink` never re-delivers — C leaves UDF alone, so a
    /// `record(dfanout,"DF1")` stays UDF=1 and `checkAlarms` (`:233-234`) raises
    /// UDF_ALARM at UDFS every single cycle. softIoc: `dbpf DF1.PROC 1` →
    /// UDF=1, SEVR INVALID, STAT UDF. The port's blanket per-cycle clear made it
    /// NO_ALARM — an alarm-visible divergence, not a cosmetic one.
    ///
    /// The closed-loop DOL read still defines the record: the framework applies
    /// `udf = value_is_undefined()` at the DOL-apply site, which is exactly
    /// where C's `:121` sits (softIoc: `DF2` with `DOL="SRC"` → UDF=0).
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `dfanoutRecord.c::checkAlarms` — UDF alarm plus analog limit
    /// alarms (HIHI/HIGH/LOW/LOLO) with per-level hysteresis.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        if common.udf != 0 {
            recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::UDF_ALARM,
                AlarmSeverity::from_u16(common.udfs as u16),
            );
            return;
        }

        let val = self.val;
        let hyst = self.hyst;
        let lalm = self.lalm;

        let hhsv = AlarmSeverity::from_u16(self.hhsv as u16);
        if hhsv != AlarmSeverity::NoAlarm
            && (val >= self.hihi || (lalm == self.hihi && val >= self.hihi - hyst))
        {
            if recgbl::rec_gbl_set_sevr(common, alarm_status::HIHI_ALARM, hhsv) {
                self.lalm = self.hihi;
            }
            return;
        }
        let llsv = AlarmSeverity::from_u16(self.llsv as u16);
        if llsv != AlarmSeverity::NoAlarm
            && (val <= self.lolo || (lalm == self.lolo && val <= self.lolo + hyst))
        {
            if recgbl::rec_gbl_set_sevr(common, alarm_status::LOLO_ALARM, llsv) {
                self.lalm = self.lolo;
            }
            return;
        }
        let hsv = AlarmSeverity::from_u16(self.hsv as u16);
        if hsv != AlarmSeverity::NoAlarm
            && (val >= self.high || (lalm == self.high && val >= self.high - hyst))
        {
            if recgbl::rec_gbl_set_sevr(common, alarm_status::HIGH_ALARM, hsv) {
                self.lalm = self.high;
            }
            return;
        }
        let lsv = AlarmSeverity::from_u16(self.lsv as u16);
        if lsv != AlarmSeverity::NoAlarm
            && (val <= self.low || (lalm == self.low && val <= self.low + hyst))
        {
            if recgbl::rec_gbl_set_sevr(common, alarm_status::LOW_ALARM, lsv) {
                self.lalm = self.low;
            }
            return;
        }
        self.lalm = val;
    }

    fn cycle_cell(&self, cell: CycleCell) -> CycleRead {
        match cell {
            CycleCell::Val => CycleRead::Value(CycleValue::Double(self.val)),
            CycleCell::Mdel => CycleRead::Value(CycleValue::Double(self.mdel)),
            CycleCell::Adel => CycleRead::Value(CycleValue::Double(self.adel)),
            CycleCell::Mlst => CycleRead::Value(CycleValue::Double(self.mlst)),
            CycleCell::Alst => CycleRead::Value(CycleValue::Double(self.alst)),
            CycleCell::Hyst => CycleRead::Value(CycleValue::Double(self.hyst)),
            CycleCell::Lalm => CycleRead::Value(CycleValue::Double(self.lalm)),
            _ => CycleRead::Absent,
        }
    }

    fn put_cycle_cell(&mut self, cell: CycleCell, value: CycleValue) -> CaResult<()> {
        match (cell, value) {
            (CycleCell::Lalm, CycleValue::Double(v)) => self.lalm = v,
            (CycleCell::Alst, CycleValue::Double(v)) => self.alst = v,
            (CycleCell::Mlst, CycleValue::Double(v)) => self.mlst = v,
            (cell, value) => return self.put_field(cell.name(), value.into()),
        }
        Ok(())
    }

    fn link_text_ref(&self, link_field: &str) -> Option<&str> {
        match link_field {
            "DOL" => Some(&self.dol),
            "SELL" => Some(&self.sell),
            // OUTA..OUTP, which the output row reads every cycle.
            _ => {
                let [b'O', b'U', b'T', slot] = *link_field.as_bytes() else {
                    return None;
                };
                Some(match slot {
                    b'A' => &self.outa,
                    b'B' => &self.outb,
                    b'C' => &self.outc,
                    b'D' => &self.outd,
                    b'E' => &self.oute,
                    b'F' => &self.outf,
                    b'G' => &self.outg,
                    b'H' => &self.outh,
                    b'I' => &self.outi,
                    b'J' => &self.outj,
                    b'K' => &self.outk,
                    b'L' => &self.outl,
                    b'M' => &self.outm,
                    b'N' => &self.outn,
                    b'O' => &self.outo,
                    b'P' => &self.outp,
                    _ => return None,
                })
            }
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            // SELM is DBF_MENU (dfanoutRecord.dbd.pod): served as DBR_ENUM,
            // labels from menu_field_choices.
            "SELM" => Some(EpicsValue::Enum(self.selm as u16)),
            "SELN" => Some(EpicsValue::UShort(self.seln)),
            "DOL" => Some(EpicsValue::String(self.dol.clone().into())),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "SELL" => Some(EpicsValue::String(self.sell.clone().into())),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
            "HIHI" => Some(EpicsValue::Double(self.hihi)),
            "HIGH" => Some(EpicsValue::Double(self.high)),
            "LOW" => Some(EpicsValue::Double(self.low)),
            "LOLO" => Some(EpicsValue::Double(self.lolo)),
            "HHSV" => Some(EpicsValue::Short(self.hhsv)),
            "HSV" => Some(EpicsValue::Short(self.hsv)),
            "LSV" => Some(EpicsValue::Short(self.lsv)),
            "LLSV" => Some(EpicsValue::Short(self.llsv)),
            "HYST" => Some(EpicsValue::Double(self.hyst)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            _ => self
                .out_slot(name)
                .map(|s| EpicsValue::String(s.to_string().into())),
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" | "IVOV" | "HIHI" | "HIGH" | "LOW" | "LOLO" | "HYST" | "LALM" | "MDEL"
            | "ADEL" | "MLST" | "ALST" => {
                let v = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                match name {
                    "VAL" => self.val = v,
                    "IVOV" => self.ivov = v,
                    "HIHI" => self.hihi = v,
                    "HIGH" => self.high = v,
                    "LOW" => self.low = v,
                    "LOLO" => self.lolo = v,
                    "HYST" => self.hyst = v,
                    "LALM" => self.lalm = v,
                    "MDEL" => self.mdel = v,
                    "ADEL" => self.adel = v,
                    "MLST" => self.mlst = v,
                    "ALST" => self.alst = v,
                    _ => unreachable!(),
                }
                Ok(())
            }
            // SELN is `DBF_USHORT` (dfanoutRecord.dbd.pod:188): client
            // puts arrive converted to UShort; internal SELL link reads
            // still pass a Short. Cast both into the unsigned field
            // (C `dbPut` truncation).
            "SELN" => match value {
                EpicsValue::UShort(v) => {
                    self.seln = v;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.seln = v as u16;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            // SELM is DBF_MENU (dfanoutRecord.dbd.pod): a client put arrives
            // converted to Enum; internal callers may still pass a Short
            // index. Store the menu index either way.
            "SELM" => match value {
                EpicsValue::Enum(v) => {
                    self.selm = v as i16;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.selm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELM".into())),
            },
            "OMSL" | "IVOA" | "HHSV" | "HSV" | "LSV" | "LLSV" => match value {
                EpicsValue::Short(v) => {
                    match name {
                        "OMSL" => self.omsl = v,
                        "IVOA" => self.ivoa = v,
                        "HHSV" => self.hhsv = v,
                        "HSV" => self.hsv = v,
                        "LSV" => self.lsv = v,
                        "LLSV" => self.llsv = v,
                        _ => unreachable!(),
                    }
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DOL" | "SELL" => match value {
                EpicsValue::String(s) => {
                    match name {
                        "DOL" => self.dol = s.as_str_lossy().into_owned(),
                        "SELL" => self.sell = s.as_str_lossy().into_owned(),
                        _ => unreachable!(),
                    }
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            _ => match (self.out_slot_mut(name), value) {
                (Some(slot), EpicsValue::String(s)) => {
                    *slot = s.as_str_lossy().into_owned();
                    Ok(())
                }
                (Some(_), _) => Err(CaError::TypeMismatch(name.into())),
                (None, _) => Err(CaError::FieldNotFound(name.to_string())),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OUTn lands and reads back through the fallback arm; a non-string put
    /// to it is a type mismatch and an unknown name is not found, as before
    /// the named fields moved ahead of it. softIoc: `dbpf D.OUTC SINK` reads
    /// back `SINK NPP NMS`.
    #[test]
    fn out_links_and_named_fields_resolve_through_one_match() {
        let mut r = DfanoutRecord::default();
        r.put_field("OUTC", EpicsValue::String("SINK".into()))
            .unwrap();
        r.put_field("VAL", EpicsValue::Double(2.0)).unwrap();
        assert_eq!(r.get_field("OUTC"), Some(EpicsValue::String("SINK".into())));
        assert_eq!(r.get_field("VAL"), Some(EpicsValue::Double(2.0)));
        assert!(matches!(
            r.put_field("OUTC", EpicsValue::Double(1.0)),
            Err(CaError::TypeMismatch(_))
        ));
        assert!(matches!(
            r.put_field("NOPE", EpicsValue::Double(1.0)),
            Err(CaError::FieldNotFound(_))
        ));
        assert_eq!(r.get_field("NOPE"), None);
    }
}
