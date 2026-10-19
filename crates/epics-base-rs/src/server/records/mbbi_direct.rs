use crate::error::{CaError, CaResult};
use crate::server::record::{
    CycleCell, CycleHooks, CycleMarks, CyclePostMask, CycleRead, CycleValue, uniform_post_table,
};
use crate::server::record::{MENU_SIMM, ProcessOutcome, RawSoftEntry, Record};
use crate::types::EpicsValue;
use crate::types::PvString;

// Multi-bit binary input direct record.
// VAL holds the full unsigned 32-bit value; B0-B1F expose individual bits
// as Char (0/1). C `mbbiDirectRecord.c` defines `NUM_BITS 32`, so the
// bit-field interface spans B0..B1F (32 fields).
// On process: RVAL is shifted right by SHFT and stored in VAL and bit fields.
pub struct MbbiDirectRecord {
    pub val: u32,
    // RVAL/ORAW/MASK are DBF_ULONG (mbbiDirectRecord.dbd.pod:112,116,121) —
    // u32 so high-bit raw/mask values round-trip without sign loss.
    pub rval: u32,
    pub oraw: u32,
    pub mask: u32,
    // SHFT is DBF_USHORT (mbbiDirectRecord.dbd.pod:131). VAL/NOBT/MLST are
    // DBF_LONG/DBF_SHORT (signed) on the Direct variant, unlike mbbi.
    pub shft: u16,
    pub nobt: i16,
    pub mlst: u32,
    pub bits: [u8; 32], // B0-B1F
    /// The bits this cycle's `monitor()` changed, drained by
    /// `take_cycle_posted_fields`.
    bits_changed: u32,
    pub simm: i16,
    pub siml: PvString,
    pub siol: PvString,
    // SVAL is `DBF_LONG` (mbbiDirectRecord.dbd.pod:173) — the BUFFER C's
    // `readValue` reads SIOL into as `DBR_ULONG` (mbbiDirectRecord.c:260), so
    // it holds the u32's bits, before publishing `val = sval`.
    pub sval: i32,
    pub sims: i16,
    pub sdly: f64,
    skip_convert: bool,
    /// DTYP is `Raw Soft Channel`, whose `read_mbbi` returns 0 even when
    /// `dbGetLink` failed (`devMbbiDirectSoftRaw.c:58-65`).
    raw_soft_dset: bool,
}

impl Default for MbbiDirectRecord {
    fn default() -> Self {
        Self {
            val: 0,
            rval: 0,
            oraw: 0,
            mask: 0,
            shft: 0,
            nobt: 0,
            mlst: 0,
            bits: [0; 32],
            bits_changed: 0,
            simm: 0,
            siml: PvString::new(),
            siol: PvString::new(),
            sval: 0,
            sims: 0,
            sdly: -1.0,
            skip_convert: false,
            raw_soft_dset: false,
        }
    }
}

impl MbbiDirectRecord {
    fn val_to_bits(&mut self) {
        for i in 0..32 {
            self.bits[i] = ((self.val >> i) & 1) as u8;
        }
    }
}

/// Bit field names B0..B1F — 32 entries, matching C `NUM_BITS 32`.
pub const BIT_NAMES: [&str; 32] = [
    "B0", "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8", "B9", "BA", "BB", "BC", "BD", "BE", "BF",
    "B10", "B11", "B12", "B13", "B14", "B15", "B16", "B17", "B18", "B19", "B1A", "B1B", "B1C",
    "B1D", "B1E", "B1F",
];

impl Record for MbbiDirectRecord {
    crate::slot_map!(crate::server::record::dbd_generated::slots::mbbiDirect;
        fields:
        RVAL => rval,
        ORAW => oraw;
        custom: VAL, NOBT, MASK, MLST, SHFT, SIOL, SVAL, SIML, SIMM, SIMS, SDLY, B0, B1, B2, B3,
            B4, B5, B6, B7, B8, B9, BA, BB, BC, BD, BE, BF, B10, B11, B12, B13, B14, B15, B16,
            B17, B18, B19, B1A, B1B, B1C, B1D, B1E, B1F;
        spill: INP, OLDSIMM, SSCN, SIMPVT);

    /// `mbbiDirectRecord.c:109` loads a constant SIOL as `DBF_ULONG` into the LONG SVAL.
    fn siol_load_type(&self) -> Option<crate::types::DbFieldType> {
        Some(crate::types::DbFieldType::ULong)
    }

    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::CYCLE_POSTED
    }

    /// UDF belongs to the dset here, not to `process()`. C
    /// `mbbiDirectRecord.c:152-166` assigns `prec->udf = FALSE` at `:163`,
    /// INSIDE `if (status == 0)`, and folds `2` into `0` only at `:165-166`,
    /// so a device support that wrote VAL never reaches the assignment.
    ///
    /// That is not a hole: the C dset that returns 2 writes `prec->udf`
    /// itself first — `devMbbiDirectSoft.c:55-60` — which the port states as
    /// `DeviceUdf::Defined`.
    fn refused_async_read_keeps_udf(&self) -> bool {
        true
    }

    fn rederives_udf_on_computed_read(&self) -> bool {
        false
    }

    /// The Raw dset's status 0 on a dead INP reaches `:163`'s
    /// `prec->udf = FALSE`; every other dset's failure misses it.
    fn derives_udf_on_read_failure(&self) -> bool {
        self.raw_soft_dset
    }

    /// C `mbbiDirectRecord.c:128-129` seeds MLST and ORAW after the dset init
    /// has loaded VAL/RVAL, whatever its status, and derives B0-B1F from VAL
    /// only on a dset `init_record` that returned 0 (`:115-124`). MLST is
    /// `special(SPC_NOMOD)` so it has no `put_field` arm for the trait default
    /// to go through.
    fn seed_deadband_tracking(&mut self, dset_init: crate::server::device_support::DsetInit) {
        if dset_init == crate::server::device_support::DsetInit::Ok {
            self.val_to_bits();
        }
        self.mlst = self.val;
        self.oraw = self.rval;
    }

    fn record_type(&self) -> &'static str {
        "mbbiDirect"
    }

    /// devmbbiDirectStream.c reads and writes the `mbbiDirectRecord` members itself.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    /// devMbbiDirectSoft.c returns `dbGetLink`'s status; devMbbiDirectSoftRaw.c returns 0.
    fn process_status_rule(&self) -> crate::server::record::ProcessStatusRule {
        crate::server::record::ProcessStatusRule::io(crate::server::record::SoftStatus {
            plain: true,
            raw: false,
            async_: false,
        })
    }

    /// `SIMM` is `DBF_MENU menu(menuSimm)` (`mbbiDirectRecord.dbd.pod`): the
    /// three-choice NO/YES/RAW simulation menu. Served as `DBR_ENUM` with
    /// these labels. `SIMS`/`OLDSIMM` are shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_SIMM),
            _ => None,
        }
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    /// `devMbbiDirectSoft.c:51`, `devMbbiDirectSoftRaw.c:54` and
    /// `devMbbiDirectSoftCallback.c:62` all read INP as `DBR_ULONG`.
    fn soft_input_request(
        &self,
        _soft: crate::server::device_support::SoftDtyp,
    ) -> crate::server::record::InputLinkRequest {
        crate::server::record::InputLinkRequest::As(crate::server::record::LinkReadAs::ULong)
    }

    /// The Plain and Async dsets' `DBR_ULONG` read lands in VAL bit for bit.
    fn soft_input_read_u32(
        &mut self,
        value: u32,
        _dpvt: &mut crate::server::device_support::Dpvt,
    ) -> CaResult<()> {
        self.val = value;
        Ok(())
    }

    /// C `devMbbiDirectSoftRaw` — `recGblInitConstantLink(&prec->inp,
    /// DBF_ULONG, &prec->rval)` at init (`devMbbiDirectSoftRaw.c:42`, unmasked),
    /// and per read `dbGetLink(&prec->inp, DBR_ULONG, &prec->rval, 0, 0)` then
    /// `prec->rval &= prec->mask` (`:60-61`). `read_mbbi` returns 0, so the
    /// record's `RVAL >> SHFT` → VAL/bit-field convert runs.
    /// `devXxxSoftRaw::init_record`'s mask (see `raw_soft_dset_mask`).
    fn init_soft_dset(&mut self, soft: crate::server::device_support::SoftDtyp) {
        self.raw_soft_dset = soft == crate::server::device_support::SoftDtyp::Raw;
        if self.raw_soft_dset {
            self.mask = super::raw_soft_dset_mask(self.mask, self.nobt == 0, self.shft);
        }
    }

    /// `mbbiDirectRecord.c:260` `dbGetLink(&prec->siol, DBR_ULONG, &prec->sval)`.
    fn input_link_request(&self, link_field: &str) -> crate::server::record::InputLinkRequest {
        crate::server::record::InputLinkRequest::As(match link_field {
            "SIOL" => crate::server::record::LinkReadAs::ULong,
            _ => crate::server::record::LinkReadAs::Native,
        })
    }

    fn raw_soft_input(&mut self, entry: RawSoftEntry, value: EpicsValue) -> Option<CaResult<()>> {
        self.rval = match super::raw_soft_rval_u32("mbbiDirect", &value) {
            Ok(rval) => rval,
            Err(e) => return Some(Err(e)),
        };
        if entry == RawSoftEntry::Read {
            self.rval &= self.mask;
        }
        Some(Ok(()))
    }

    /// C `mbbiDirectRecord.c:228-231` `monitor()`: `if (prec->mlst != prec->val) { events |=
    /// DBE_VALUE | DBE_LOG; prec->mlst = prec->val; }` — compared and
    /// committed HERE, at C's position, never captured during `process()`.
    /// `:218-226` first derives B0-B1F from VAL, against the bit each held.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let mut bits_changed = 0;
        for (i, b) in self.bits.iter_mut().enumerate() {
            let new = ((self.val >> i) & 1) as u8;
            if *b != new {
                *b = new;
                bits_changed |= 1 << i;
            }
        }
        self.bits_changed = bits_changed;
        let changed = self.mlst != self.val;
        if changed {
            self.mlst = self.val;
        }
        Some(changed)
    }

    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        static BIT_POSTS: [(crate::server::record::FieldIx, CyclePostMask); 32] =
            uniform_post_table(
                crate::server::record::dbd_generated::MBBI_DIRECT_FIELDS,
                &BIT_NAMES,
                CyclePostMask::MonitorValueLog,
            );
        CycleMarks::of(&BIT_POSTS, std::mem::take(&mut self.bits_changed) as u128)
    }

    /// C `:221-224` posts a bit on its own change against the record's bit or
    /// with the alarm events, never on a compare against what a client was
    /// last sent: a Bx put is posted by `dbPut` and monitor() then restores it.
    /// RVAL likewise answers from the record's own `oraw`.
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        static CLAIMED: [&str; 33] = [
            "RVAL", "B0", "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8", "B9", "BA", "BB", "BC",
            "BD", "BE", "BF", "B10", "B11", "B12", "B13", "B14", "B15", "B16", "B17", "B18", "B19",
            "B1A", "B1B", "B1C", "B1D", "B1E", "B1F",
        ];
        &CLAIMED
    }

    fn alarm_cycle_monitored_fields(&self) -> crate::server::record::DeclaredFields {
        &const {
            crate::server::record::declared_set(
                crate::server::record::dbd_generated::MBBI_DIRECT_FIELDS,
                BIT_NAMES,
            )
        }
    }

    /// C `mbbiDirectRecord.c:168-169` raises UDF with the literal `INVALID_ALARM`, not
    /// `prec->udfs`:
    ///
    /// ```c
    /// if (prec->udf)
    ///     recGblSetSevr(prec, UDF_ALARM, INVALID_ALARM);
    /// ```
    ///
    /// so `UDFS` cannot weaken or silence this record's UDF alarm. `mbboDirect`, the other half of the Direct pair, passes
    /// `prec->udfs` (`mbboDirectRecord.c:191`), and so does `mbbi`
    /// (`mbbiRecord.c:302`) — the split is per-record, not per-family.
    fn udf_alarm_severity(&self) -> Option<crate::server::record::AlarmSeverity> {
        Some(crate::server::record::AlarmSeverity::Invalid)
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 {
            // C `mbbiDirectRecord.c:112-113`, after the pass-0 return — MASK
            // from NOBT, NOBT may span 1..32 (NUM_BITS 32).
            if self.mask == 0 && self.nobt > 0 && self.nobt <= 32 {
                self.mask = ((1i64 << self.nobt) - 1) as u32;
            }
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if !self.skip_convert {
            // C `mbbiDirectRecord.c:155-163` is the whole convert:
            //
            //     epicsUInt32 rval = prec->rval;
            //     if (prec->shft > 0) rval >>= prec->shft;
            //     prec->val = rval;
            //
            // MASK is not read. It belongs to device support, which positions
            // it (`prec->mask <<= prec->shft`, devMbbiDirectSoftRaw.c:48) and
            // applies it to RVAL before the record ever sees it (`:55`), or —
            // under asyn — overwrites MASK with the driver's already-positioned
            // hardware mask (devAsynUInt32Digital.c:1008-1009). Masking here
            // with the record's own UNSHIFTED NOBT mask destroyed the value it
            // was meant to protect: NOBT=4 SHFT=4 RVAL=240 gives `240 & 0x0F`
            // = 0 instead of 15.
            let mut raw = self.rval;
            if self.shft > 0 {
                // DELIBERATE DEVIATION, and the rule for every SHFT site in
                // this module. `SHFT` is `DBF_USHORT`, so a client can put 40
                // into it; C shifts with the bare operator under the same
                // `shft > 0` guard and nothing else, which at or past the
                // width of the promoted type is undefined. It is not a
                // no-crash constant either. SHFT is a runtime value, so the
                // form that matters is a register-controlled shift, and
                // executed with gcc 13.3.0 -O2 on x86_64 that gives
                // `15u << 40` == 3840 and `240u >> 40` == 0 -- one rule, the
                // count masked to five bits (`15 << 8`, `240 >> 8`), not two
                // directions disagreeing. Written with a literal 40 the same
                // gcc folds both to 0, and clang 18.1.3 targeting
                // aarch64-linux-gnu or armv7-none-eabi emits a function that
                // returns its argument unshifted. Three answers for one
                // expression, so there is no C behaviour to match and the port
                // saturates to zero -- the one answer that does not make RVAL
                // depend on the build target.
                raw = raw.checked_shr(self.shft as u32).unwrap_or(0);
            }
            self.val = raw;
        }
        self.skip_convert = false;
        Ok(ProcessOutcome::complete())
    }

    /// C `mbbiDirectRecord.c:235-238` posts RVAL with `vl_events` outside
    /// the `if (events)` that gates VAL.
    fn fields_posted_with_value_mask(
        &self,
    ) -> &'static [(&'static str, crate::server::record::ValuePostGate)] {
        &[("RVAL", crate::server::record::ValuePostGate::OnChangeForced)]
    }

    /// `monitor()` posts VAL, RVAL and B0-B1F alone
    /// (`mbbiDirectRecord.c:208-239`).
    fn process_posted_fields(&self) -> Option<crate::server::record::DeclaredFields> {
        static POSTED: [crate::server::record::FieldIx; 34] = crate::server::record::declared_set(
            crate::server::record::dbd_generated::MBBI_DIRECT_FIELDS,
            [
                "VAL", "RVAL", "B0", "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8", "B9", "BA",
                "BB", "BC", "BD", "BE", "BF", "B10", "B11", "B12", "B13", "B14", "B15", "B16",
                "B17", "B18", "B19", "B1A", "B1B", "B1C", "B1D", "B1E", "B1F",
            ],
        );
        Some(&POSTED)
    }

    /// C `if (prec->oraw != prec->rval) { ...; prec->oraw = prec->rval; }`
    /// (`mbbiDirectRecord.c:235-238`), minus the post. Outside init,
    /// `monitor()` is the only writer of `oraw`.
    fn take_secondary_value_changes(
        &mut self,
        _value_mask: crate::server::recgbl::EventMask,
    ) -> crate::server::record::SecondaryTake {
        crate::server::record::SecondaryTake::unguarded(u64::from(
            crate::server::record::SecondaryTake::advance(&mut self.oraw, self.rval),
        ))
    }

    fn link_text_ref(&self, link_field: &str) -> Option<&[u8]> {
        match link_field {
            "SIML" => Some(self.siml.as_bytes()),
            "SIOL" => Some(self.siol.as_bytes()),
            _ => None,
        }
    }

    fn simm_cells(&self) -> Option<crate::server::record::SimmCells> {
        Some(crate::server::record::SimmCells {
            simm: self.simm,
            sims: self.sims,
            sdly: self.sdly,
        })
    }

    fn cycle_cell(&self, cell: CycleCell) -> CycleRead {
        match cell {
            CycleCell::Val => CycleRead::Value(CycleValue::Long(self.val as i32)),
            CycleCell::Mlst => CycleRead::Value(CycleValue::Long(self.mlst as i32)),
            _ => CycleRead::Absent,
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val as i32)),
            "RVAL" => Some(EpicsValue::ULong(self.rval)),
            "ORAW" => Some(EpicsValue::ULong(self.oraw)),
            "MASK" => Some(EpicsValue::ULong(self.mask)),
            "SHFT" => Some(EpicsValue::UShort(self.shft)),
            "NOBT" => Some(EpicsValue::Short(self.nobt)),
            "MLST" => Some(EpicsValue::Long(self.mlst as i32)),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SVAL" => Some(EpicsValue::Long(self.sval)),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            // B0..B1F are DBF_UCHAR (mbbiDirectRecord.dbd.pod). Serve the bit as
            // the native `UChar` (unsigned) — it still projects to `DBR_CHAR` on
            // the wire (`dbr.rs`: `UChar -> Char`), so a client's `caget` reports
            // DBF_CHAR exactly as C does, but the put-coercion target derived
            // from this value (`dbput_request`) is now the unsigned 0..=255 range
            // instead of signed i8. C `dbFastPutConvert[DBR_STRING][DBF_UCHAR]`
            // accepts `caput .Bn 255`; the prior `Char` representation made the
            // port refuse everything above i8-max (128..=255).
            _ => BIT_NAMES
                .iter()
                .position(|&n| n == name)
                .map(|idx| EpicsValue::UChar(self.bits[idx])),
        }
    }

    /// The `SPC_NOMOD` MLST that `put_field` refuses, stored raw from a
    /// `.db` as `dbPutString` does.
    fn load_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match (name, value) {
            ("MLST", EpicsValue::Long(v)) => {
                self.mlst = v as u32;
                Ok(())
            }
            (name, value) => self.put_field(name, value),
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Long(v) => self.val = v as u32,
                EpicsValue::Short(v) => self.val = v as u32,
                EpicsValue::Char(v) => self.val = v as u32,
                _ => return Err(CaError::TypeMismatch("VAL".into())),
            },
            // RVAL/MASK are DBF_ULONG: accept the native ULong and tolerate
            // the legacy signed Long (device support / autosave).
            "RVAL" => {
                self.rval = match value {
                    EpicsValue::ULong(v) => v,
                    EpicsValue::Long(v) => v as u32,
                    _ => return Err(CaError::TypeMismatch("RVAL".into())),
                };
            }
            "MASK" => {
                self.mask = match value {
                    EpicsValue::ULong(v) => v,
                    EpicsValue::Long(v) => v as u32,
                    _ => return Err(CaError::TypeMismatch("MASK".into())),
                };
            }
            // SHFT is DBF_USHORT: accept UShort, tolerate Enum/Short.
            "SHFT" => {
                self.shft = match value {
                    EpicsValue::UShort(v) => v,
                    EpicsValue::Enum(v) => v,
                    EpicsValue::Short(v) => v as u16,
                    _ => return Err(CaError::TypeMismatch("SHFT".into())),
                };
            }
            "NOBT" => {
                if let EpicsValue::Short(v) = value {
                    self.nobt = v;
                } else {
                    return Err(CaError::TypeMismatch("NOBT".into()));
                }
            }
            "SIMM" => {
                if let EpicsValue::Short(v) = value {
                    self.simm = v;
                } else {
                    return Err(CaError::TypeMismatch("SIMM".into()));
                }
            }
            "SIML" => {
                if let EpicsValue::String(v) = value {
                    self.siml = v;
                } else {
                    return Err(CaError::TypeMismatch("SIML".into()));
                }
            }
            "SIOL" => {
                if let EpicsValue::String(v) = value {
                    self.siol = v;
                } else {
                    return Err(CaError::TypeMismatch("SIOL".into()));
                }
            }
            "SIMS" => {
                if let EpicsValue::Short(v) = value {
                    self.sims = v;
                } else {
                    return Err(CaError::TypeMismatch("SIMS".into()));
                }
            }
            "SDLY" => {
                if let EpicsValue::Double(v) = value {
                    self.sdly = v;
                } else {
                    return Err(CaError::TypeMismatch("SDLY".into()));
                }
            }
            "SVAL" => {
                if let EpicsValue::Long(v) = value {
                    self.sval = v;
                } else {
                    return Err(CaError::TypeMismatch("SVAL".into()));
                }
            }
            _ => {
                if let Some(idx) = BIT_NAMES.iter().position(|&n| n == name) {
                    // B0..B1F are DBF_UCHAR: C stores the cast byte, and only
                    // `monitor()`'s re-derive (`mbbiDirectRecord.c:221`)
                    // normalises it.
                    let bit = match value {
                        EpicsValue::UChar(v) => v,
                        EpicsValue::Char(v) => v as u8,
                        EpicsValue::Short(v) => v as u8,
                        EpicsValue::Long(v) => v as u8,
                        _ => return Err(CaError::TypeMismatch(name.into())),
                    };
                    // INPUT record: Bx is DERIVED from VAL, so a Bx put must
                    // NOT fold back into VAL (unlike mbboDirect). Store the raw
                    // bit transiently; the `pp(TRUE)` put processes the record,
                    // and process() re-derives every bit from the unchanged VAL,
                    // overwriting this (C `mbbiDirectRecord.c` has no `special`
                    // for Bx and never updates VAL on a bit put — VAL stays put).
                    self.bits[idx] = bit;
                } else {
                    return Err(CaError::FieldNotFound(name.to_string()));
                }
            }
        }
        Ok(())
    }

    fn set_device_did_compute(&mut self, did: bool) {
        self.skip_convert = did;
    }

    /// asyn device readback (asynUInt32Digital only — `asynMbbiDirectUInt32Digital`
    /// is the sole mbbiDirect dset, devAsynUInt32Digital.c:165): `processMbbiDirect`
    /// sets `pr->rval = value & mask` (devAsynUInt32Digital.c:1031) and returns 0,
    /// so mbbiDirectRecord's `rval -> (>> SHFT) -> VAL` convert resolves VAL; the
    /// bits are derived later in `process()`, C `monitor()`'s one derivation
    /// (`mbbiDirectRecord.c:218-226`). The hook runs that convert inline and returns `true` so the framework's
    /// `set_device_did_compute(true)` makes `process()` skip the (identical) forward
    /// pass. Without it the raw word lands verbatim in VAL via the default `set_val`
    /// (no MASK, no SHFT, wrong bits). Device-distinct entry — the Soft Channel path
    /// stays on `set_val` (C `devMbbiDirectSoft` returns 2). Input twin of
    /// `mbbo_direct::apply_raw_readback`.
    fn apply_raw_readback(&mut self, raw: i32) -> bool {
        self.rval = raw as u32;
        self.val = if self.shft > 0 {
            self.rval.checked_shr(self.shft as u32).unwrap_or(0)
        } else {
            self.rval
        };
        true
    }

    /// `mbbiDirect` has an `RVAL → VAL` `convert()` step. A `Soft
    /// Channel` `mbbiDirect` must skip it — C `devMbbiDirectSoft.c`
    /// `read_mbbiDirect` returns 2.
    fn soft_channel_skips_convert(&self) -> bool {
        true
    }
}
