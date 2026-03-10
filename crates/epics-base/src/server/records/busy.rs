use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, Record, RecordProcessResult};
use crate::types::{DbFieldType, EpicsValue};

/// Busy record (synApps busy module).
///
/// Like bo but tracks asynchronous operation state.
/// VAL=1 means busy, VAL=0 means done.
/// Forward links fire only when `val == 0 || oval == 0`,
/// suppressing FLNK during sustained busy state (1→1).
pub struct BusyRecord {
    pub val: u16,
    pub oval: u16,
    pub znam: String,
    pub onam: String,
    pub zsv: i16,
    pub osv: i16,
    pub cosv: i16,
    pub mlst: u16,
    pub ivoa: i16,
    pub ivov: u16,
    pub omsl: i16,
    pub dol: String,
    pub rval: u32,
    pub mask: u32,
}

impl Default for BusyRecord {
    fn default() -> Self {
        Self {
            val: 0,
            oval: 0,
            znam: String::new(),
            onam: String::new(),
            zsv: 0,
            osv: 0,
            cosv: 0,
            mlst: 0,
            ivoa: 0,
            ivov: 0,
            omsl: 0,
            dol: String::new(),
            rval: 0,
            mask: 0,
        }
    }
}

static BUSY_FIELDS: &[FieldDesc] = &[
    FieldDesc { name: "VAL", dbf_type: DbFieldType::Enum, read_only: false },
    FieldDesc { name: "OVAL", dbf_type: DbFieldType::Enum, read_only: true },
    FieldDesc { name: "ZNAM", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "ONAM", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "ZSV", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "OSV", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "COSV", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "MLST", dbf_type: DbFieldType::Enum, read_only: true },
    FieldDesc { name: "IVOA", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "IVOV", dbf_type: DbFieldType::Enum, read_only: false },
    FieldDesc { name: "OMSL", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "DOL", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "RVAL", dbf_type: DbFieldType::Long, read_only: true },
    FieldDesc { name: "MASK", dbf_type: DbFieldType::Long, read_only: false },
];

impl Record for BusyRecord {
    fn record_type(&self) -> &'static str { "busy" }

    fn process(&mut self) -> CaResult<RecordProcessResult> {
        // VAL → RVAL conversion
        if self.mask != 0 {
            self.rval = if self.val == 0 { 0 } else { self.mask };
        } else {
            self.rval = self.val as u32;
        }

        // Save current VAL to OVAL (for FLNK decision)
        self.oval = self.val;

        // Update MLST for monitor tracking
        self.mlst = self.val;

        Ok(RecordProcessResult::Complete)
    }

    fn should_fire_forward_link(&self) -> bool {
        // Suppress FLNK during sustained busy (1→1)
        self.val == 0 || self.oval == 0
    }

    fn can_device_write(&self) -> bool {
        true
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Enum(self.val)),
            "OVAL" => Some(EpicsValue::Enum(self.oval)),
            "ZNAM" => Some(EpicsValue::String(self.znam.clone())),
            "ONAM" => Some(EpicsValue::String(self.onam.clone())),
            "ZSV" => Some(EpicsValue::Short(self.zsv)),
            "OSV" => Some(EpicsValue::Short(self.osv)),
            "COSV" => Some(EpicsValue::Short(self.cosv)),
            "MLST" => Some(EpicsValue::Enum(self.mlst)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Enum(self.ivov)),
            "OMSL" => Some(EpicsValue::Short(self.omsl)),
            "DOL" => Some(EpicsValue::String(self.dol.clone())),
            "RVAL" => Some(EpicsValue::Long(self.rval as i32)),
            "MASK" => Some(EpicsValue::Long(self.mask as i32)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = match value {
                    EpicsValue::Enum(v) => v,
                    EpicsValue::Long(v) => v as u16,
                    EpicsValue::Short(v) => v as u16,
                    EpicsValue::Double(v) => v as u16,
                    EpicsValue::String(ref s) => {
                        if s.eq_ignore_ascii_case(&self.znam) {
                            0
                        } else if s.eq_ignore_ascii_case(&self.onam) {
                            1
                        } else {
                            s.parse::<u16>().unwrap_or(0)
                        }
                    }
                    _ => return Err(CaError::TypeMismatch("VAL".into()))
                };
                Ok(())
            }
            "ZNAM" => match value { EpicsValue::String(v) => { self.znam = v; Ok(()) } _ => Err(CaError::TypeMismatch("ZNAM".into())) },
            "ONAM" => match value { EpicsValue::String(v) => { self.onam = v; Ok(()) } _ => Err(CaError::TypeMismatch("ONAM".into())) },
            "ZSV" => match value { EpicsValue::Short(v) => { self.zsv = v; Ok(()) } _ => Err(CaError::TypeMismatch("ZSV".into())) },
            "OSV" => match value { EpicsValue::Short(v) => { self.osv = v; Ok(()) } _ => Err(CaError::TypeMismatch("OSV".into())) },
            "COSV" => match value { EpicsValue::Short(v) => { self.cosv = v; Ok(()) } _ => Err(CaError::TypeMismatch("COSV".into())) },
            "IVOA" => match value { EpicsValue::Short(v) => { self.ivoa = v; Ok(()) } _ => Err(CaError::TypeMismatch("IVOA".into())) },
            "IVOV" => match value {
                EpicsValue::Enum(v) => { self.ivov = v; Ok(()) }
                EpicsValue::Short(v) => { self.ivov = v as u16; Ok(()) }
                _ => Err(CaError::TypeMismatch("IVOV".into()))
            },
            "OMSL" => match value { EpicsValue::Short(v) => { self.omsl = v; Ok(()) } _ => Err(CaError::TypeMismatch("OMSL".into())) },
            "DOL" => match value { EpicsValue::String(v) => { self.dol = v; Ok(()) } _ => Err(CaError::TypeMismatch("DOL".into())) },
            "MASK" => match value { EpicsValue::Long(v) => { self.mask = v as u32; Ok(()) } _ => Err(CaError::TypeMismatch("MASK".into())) },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] { BUSY_FIELDS }
}
