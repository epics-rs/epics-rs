//! The `scanparm` record — port of `scanparmRecord.c`.
//!
//! A scanparm holds one positioner's scan parameters and, on LOAD or GO,
//! writes them through its output links into an sscan record (`OSP` to
//! `P1SP`, `OSC` to `EXSC`, ...). IACT reads the sscan's BUSY so a running
//! scan is never re-loaded; IMP reads its MPTS to clamp NP.

use std::sync::atomic::{AtomicI32, Ordering};

use super::dbd_generated;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::AsyncDbHandle;
use epics_base_rs::server::record::{
    ConstantInitLink, CycleHooks, CycleMarks, CyclePostMask, CycleValue, DeclaredFields, FieldDesc,
    FieldIx, FieldMetadataOverride, InputLinkRequest, LinkRead, LinkReadAs, PostStage,
    ProcessAction, ProcessActions, ProcessOutcome, ProcessStatusRule, Record, ReturnPact,
    StampPoint,
};
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString, RecordName};

/// iocsh `scanparmRecordDebug`: non-zero prints `process()`'s three traces.
pub static SCANPARM_RECORD_DEBUG: AtomicI32 = AtomicI32::new(0);

/// The iocsh variable `sscanSupport.dbd` declares for the record.
pub(crate) fn iocsh_vars() -> Vec<epics_base_rs::server::iocsh::vars::VarDef> {
    use epics_base_rs::server::iocsh::vars::{VarAccess, VarDef};
    vec![VarDef {
        name: "scanparmRecordDebug",
        access: VarAccess::Int {
            get: || i64::from(SCANPARM_RECORD_DEBUG.load(Ordering::Relaxed)),
            set: |v| SCANPARM_RECORD_DEBUG.store(v as i32, Ordering::Relaxed),
        },
    }]
}

fn debug() -> bool {
    SCANPARM_RECORD_DEBUG.load(Ordering::Relaxed) != 0
}

/// C `#define VERSION 3.0`, stored into VERS by `init_record` pass 0
/// (`scanparmRecord.c:117-120`).
const VERSION: f32 = 3.0;

/// The NP posts `process()` makes ahead of its link writes
/// (`scanparmRecord.c:145-152`).
static PROCESS_POSTS: [(FieldIx, CyclePostMask); 1] =
    epics_base_rs::server::record::declared_post_table(
        dbd_generated::SCANPARM_FIELDS,
        [("NP", CyclePostMask::Value)],
    );

/// `monitor()`'s one post: STEP with `recGblResetAlarms`' mask | DBE_VALUE,
/// only when it moved (`scanparmRecord.c:274-283`).
static MONITOR_POSTS: [(FieldIx, CyclePostMask); 1] =
    epics_base_rs::server::record::declared_post_table(
        dbd_generated::SCANPARM_FIELDS,
        [("STEP", CyclePostMask::MonitorValue)],
    );

/// The parameter writes LOAD and GO make, in C order
/// (`scanparmRecord.c:157-200`): `(link field, value field)`.
const LOAD_LINKS: [(&str, &str); 11] = [
    ("OPRE", "PRE"),
    ("OPPV", "PPV"),
    ("ORPV", "RPV"),
    ("OTPV", "TPV"),
    ("ODPV", "DPV"),
    ("OSM", "SM"),
    ("OSP", "SP"),
    ("OEP", "EP"),
    ("ONP", "NP"),
    ("OAR", "AR"),
    ("OAFT", "AFT"),
];

/// The writes only GO makes, after [`LOAD_LINKS`] (`scanparmRecord.c:208-214`).
const GO_LINKS: [(&str, &str); 2] = [("OAQT", "AQT"), ("OSC", "SC")];

#[derive(Default)]
pub struct ScanparmRecord {
    pub vers: f32,
    pub val: f64,
    pub prec: i16,
    pub pre: i16,
    pub opre: PvString,
    pub sm: u16,
    pub osm: PvString,
    pub ar: u16,
    pub oar: PvString,
    pub aft: u16,
    pub oaft: PvString,
    pub ppv: PvString,
    pub rpv: PvString,
    pub dpv: PvString,
    pub tpv: PvString,
    pub oppv: PvString,
    pub orpv: PvString,
    pub odpv: PvString,
    pub otpv: PvString,
    pub sp: f64,
    pub osp: PvString,
    pub ep: f64,
    pub oep: PvString,
    pub np: i32,
    pub onp: PvString,
    pub sc: i16,
    pub osc: PvString,
    pub aqt: f64,
    pub oaqt: PvString,
    pub mp: i32,
    pub imp: PvString,
    pub act: i16,
    pub iact: PvString,
    pub load: i16,
    pub oload: PvString,
    pub go: i16,
    pub ogo: PvString,
    pub step: f64,
    pub lstp: f64,

    /// Both of `process()`'s `dbGetLink`s succeeded this cycle; C returns
    /// their status ahead of everything else when either fails (`:136-142`).
    reads_ok: bool,
    /// `prec->name`, for the debug traces.
    name: String,
    process_posts: u128,
    monitor_posts: u128,
}

impl ScanparmRecord {
    fn link_text(&self, link: &str) -> &PvString {
        match link {
            "OPRE" => &self.opre,
            "OSM" => &self.osm,
            "OAR" => &self.oar,
            "OAFT" => &self.oaft,
            "OPPV" => &self.oppv,
            "ORPV" => &self.orpv,
            "ODPV" => &self.odpv,
            "OTPV" => &self.otpv,
            "OSP" => &self.osp,
            "OEP" => &self.oep,
            "ONP" => &self.onp,
            "OSC" => &self.osc,
            "OAQT" => &self.oaqt,
            "OLOAD" => &self.oload,
            "OGO" => &self.ogo,
            "IMP" => &self.imp,
            "IACT" => &self.iact,
            _ => unreachable!("{link} is not a scanparm link"),
        }
    }

    /// C's `if (plink->value.pv_link.pvname && pvname[0]) dbPutLink(..)`:
    /// the value goes out only through a link that names something.
    fn put_link(&self, actions: &mut Vec<ProcessAction>, link: &'static str, value: EpicsValue) {
        if !self.link_text(link).as_ref().is_empty() {
            actions.push(ProcessAction::WriteDbLink {
                link_field: link,
                value,
            });
        }
    }

    /// One of `process()`'s put groups, then C's `if (status) return(status);`
    /// (`scanparmRecord.c:206`, `:219`): `status` is the last put the group
    /// made, so only that one returns, and it returns ahead of `pact = TRUE`.
    fn put_group(&self, actions: &mut Vec<ProcessAction>, links: &[(&'static str, &str)]) {
        let mut puts = links
            .iter()
            .filter(|(link, _)| !self.link_text(link).as_ref().is_empty())
            .map(|&(link, field)| {
                let value = self.get_field(field).expect("a declared scanparm field");
                (link, value)
            })
            .peekable();
        while let Some((link_field, value)) = puts.next() {
            actions.push(if puts.peek().is_some() {
                ProcessAction::WriteDbLink { link_field, value }
            } else {
                ProcessAction::WriteDbLinkOrReturn {
                    link_field,
                    value,
                    pact: ReturnPact::Clear,
                }
            });
        }
    }

    /// `(ep - sp) / (np - 1)`, the step size `init_record` and `process()`
    /// both derive.
    fn step_size(&self) -> f64 {
        (self.ep - self.sp) / f64::from(self.np - 1)
    }
}

fn short(name: &str, value: EpicsValue) -> CaResult<i16> {
    match value.convert_to(DbFieldType::Short) {
        EpicsValue::Short(v) => Ok(v),
        _ => Err(CaError::TypeMismatch(name.into())),
    }
}

fn long(name: &str, value: EpicsValue) -> CaResult<i32> {
    match value.convert_to(DbFieldType::Long) {
        EpicsValue::Long(v) => Ok(v),
        _ => Err(CaError::TypeMismatch(name.into())),
    }
}

fn double(name: &str, value: EpicsValue) -> CaResult<f64> {
    value
        .to_f64()
        .ok_or_else(|| CaError::TypeMismatch(name.into()))
}

fn menu(name: &str, value: EpicsValue) -> CaResult<u16> {
    match value.convert_to(DbFieldType::Enum) {
        EpicsValue::Enum(v) => Ok(v),
        _ => Err(CaError::TypeMismatch(name.into())),
    }
}

fn string(name: &str, value: EpicsValue) -> CaResult<PvString> {
    match value {
        EpicsValue::String(v) => Ok(v),
        _ => Err(CaError::TypeMismatch(name.into())),
    }
}

impl Record for ScanparmRecord {
    fn record_type(&self) -> &'static str {
        "scanparm"
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.vers = VERSION;
            return Ok(());
        }
        // A constant IMP is seeded into MP by `constant_init_links` (:124-126).
        // No monitor can exist yet, so C's NP post here has no reader.
        if self.np < 2 {
            self.np = 2;
        }
        self.lstp = self.step_size();
        Ok(())
    }

    /// C seeds a constant IMP with `recGblInitConstantLink(&imp, DBF_SHORT,
    /// &mp)` (`scanparmRecord.c:125`), two bytes into the DBF_LONG MP; the
    /// port loads it at MP's own type.
    fn constant_init_links(&self) -> Vec<ConstantInitLink> {
        vec![ConstantInitLink::new("IMP", "MP")]
    }

    /// IACT first; IMP is read from [`Self::link_read_result`] only once IACT
    /// read cleanly (`scanparmRecord.c:136-141`).
    fn pre_process_actions(&mut self) -> ProcessActions {
        ProcessActions::from(vec![ProcessAction::GetLink { link_field: "IACT" }])
    }

    /// `dbGetLink(&iact, DBR_SHORT, ..)` and `dbGetLink(&imp, DBR_LONG, ..)`
    /// (`scanparmRecord.c:136`, `:141`).
    fn input_link_request(&self, link_field: &str) -> InputLinkRequest {
        InputLinkRequest::As(match link_field {
            "IMP" => LinkReadAs::Long,
            _ => LinkReadAs::Short,
        })
    }

    fn link_read_result(&mut self, link_field: &'static str, read: LinkRead) -> ProcessActions {
        let ok = !matches!(read, LinkRead::Failed);
        if let LinkRead::Value(v) = read {
            match link_field {
                "IACT" => self.act = short(link_field, v).unwrap_or(self.act),
                _ => self.mp = long(link_field, v).unwrap_or(self.mp),
            }
        }
        // C prints ACT only past its status check and MP ahead of it
        // (`scanparmRecord.c:137-146`).
        if debug() && link_field == "IACT" && ok {
            println!("scanparm({}):process:act={}", self.name, self.act);
        }
        if debug() && link_field == "IMP" {
            println!("scanparm({}):process:mp={}", self.name, self.mp);
        }
        if link_field == "IACT" && ok {
            return ProcessActions::from(vec![ProcessAction::GetLink { link_field: "IMP" }]);
        }
        self.reads_ok = ok && link_field == "IMP";
        ProcessActions::new()
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // `if (status) return(status);` after either read: no write, no
        // clear, no monitor and no forward link.
        if !std::mem::take(&mut self.reads_ok) {
            return Ok(ProcessOutcome::complete_no_emit());
        }

        if self.np > self.mp {
            self.np = self.mp;
            self.process_posts = 1;
        }
        if self.np < 2 {
            self.np = 2;
            self.process_posts = 1;
        }
        self.step = self.step_size();

        let mut actions = Vec::new();
        if self.act == 0 && (self.load != 0 || self.go != 0) {
            self.put_group(&mut actions, &LOAD_LINKS);
            if self.go != 0 {
                // After the LOAD group, so a group that returns skips it (:206-211).
                if debug() {
                    actions.push(ProcessAction::Trace {
                        prefix: "scanparm(",
                        suffix: "):process:starting scan",
                    });
                }
                self.put_group(&mut actions, &GO_LINKS);
            }
        }

        // Each of GO and LOAD clears right after its own put, posted with a
        // bare DBE_VALUE, so OLOAD's target reads GO already 0 (:248-262).
        if self.go != 0 {
            self.put_link(&mut actions, "OGO", EpicsValue::Short(self.go));
            actions.push(ProcessAction::PutInLinkStage {
                field: "GO",
                value: CycleValue::Short(0),
                post: PostStage::Output,
            });
        }
        if self.load != 0 {
            self.put_link(&mut actions, "OLOAD", EpicsValue::Short(self.load));
            actions.push(ProcessAction::PutInLinkStage {
                field: "LOAD",
                value: CycleValue::Short(0),
                post: PostStage::Output,
            });
        }

        Ok(ProcessOutcome::complete_with(actions))
    }

    /// `recGblGetTimeStamp` follows the OGO and OLOAD puts (`scanparmRecord.c:263`),
    /// so a cycle that returns at a failed put keeps the previous TIME.
    fn stamp_point(&self) -> StampPoint {
        StampPoint::AfterLinkWrites
    }

    /// `return(status)` (:271) is the last of the OGO and OLOAD puts
    /// (:250-258); a returning group put leaves through its own action.
    fn set_async_context(&mut self, name: RecordName, _db: AsyncDbHandle) {
        self.name = String::from_utf8_lossy(name.as_bytes()).into_owned();
    }

    fn process_status_rule(&self) -> ProcessStatusRule {
        ProcessStatusRule::ZERO.with_status_links(&["OGO", "OLOAD"])
    }

    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::CYCLE_POSTED
            .union(CycleHooks::PROCESS_POSTED)
            .union(CycleHooks::MONITOR_RESET)
    }

    /// `monitor()` resets the alarms and posts STEP only when the step moved
    /// (`scanparmRecord.c:280-283`), on a cycle that got that far.
    fn monitor_resets_alarms(&mut self) -> bool {
        let moved = self.step != self.lstp;
        self.monitor_posts = u128::from(moved);
        if moved {
            self.lstp = self.step;
        }
        moved
    }

    fn take_process_posted_fields(&mut self) -> CycleMarks {
        CycleMarks::of(&PROCESS_POSTS, std::mem::take(&mut self.process_posts))
    }

    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        CycleMarks::of(&MONITOR_POSTS, std::mem::take(&mut self.monitor_posts))
    }

    /// C posts NP, GO, LOAD and STEP from a process cycle, and nothing else.
    fn process_posted_fields(&self) -> Option<DeclaredFields> {
        Some(
            &const {
                epics_base_rs::server::record::declared_set(
                    dbd_generated::SCANPARM_FIELDS,
                    ["NP", "GO", "LOAD", "STEP"],
                )
            },
        )
    }

    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &["NP", "STEP"]
    }

    /// VAL is never posted: `monitor()` posts STEP alone.
    fn process_posts_value_monitor(&self) -> bool {
        false
    }

    /// `get_precision` answers 2 for VERS and PREC everywhere else
    /// (`scanparmRecord.c:286-297`).
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        field
            .eq_ignore_ascii_case("VERS")
            .then(|| FieldMetadataOverride {
                precision: Some(2),
                ..Default::default()
            })
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        let s = |v: &PvString| Some(EpicsValue::String(v.clone()));
        match name {
            "VERS" => Some(EpicsValue::Float(self.vers)),
            "VAL" => Some(EpicsValue::Double(self.val)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "PRE" => Some(EpicsValue::Short(self.pre)),
            "SM" => Some(EpicsValue::Enum(self.sm)),
            "AR" => Some(EpicsValue::Enum(self.ar)),
            "AFT" => Some(EpicsValue::Enum(self.aft)),
            "PPV" => s(&self.ppv),
            "RPV" => s(&self.rpv),
            "DPV" => s(&self.dpv),
            "TPV" => s(&self.tpv),
            "SP" => Some(EpicsValue::Double(self.sp)),
            "EP" => Some(EpicsValue::Double(self.ep)),
            "NP" => Some(EpicsValue::Long(self.np)),
            "SC" => Some(EpicsValue::Short(self.sc)),
            "AQT" => Some(EpicsValue::Double(self.aqt)),
            "MP" => Some(EpicsValue::Long(self.mp)),
            "ACT" => Some(EpicsValue::Short(self.act)),
            "LOAD" => Some(EpicsValue::Short(self.load)),
            "GO" => Some(EpicsValue::Short(self.go)),
            "STEP" => Some(EpicsValue::Double(self.step)),
            "LSTP" => Some(EpicsValue::Double(self.lstp)),
            "OPRE" | "OSM" | "OAR" | "OAFT" | "OPPV" | "ORPV" | "ODPV" | "OTPV" | "OSP" | "OEP"
            | "ONP" | "OSC" | "OAQT" | "IMP" | "IACT" | "OLOAD" | "OGO" => s(self.link_text(name)),
            _ => None,
        }
    }

    /// Every declared field stores; the `SPC_NOMOD` ones (VERS, the links,
    /// MP, ACT, STEP, LSTP) are refused to clients by the framework's
    /// `check_no_mod` before they get here.
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VERS" => self.vers = double(name, value)? as f32,
            "VAL" => self.val = double(name, value)?,
            "PREC" => self.prec = short(name, value)?,
            "PRE" => self.pre = short(name, value)?,
            "SM" => self.sm = menu(name, value)?,
            "AR" => self.ar = menu(name, value)?,
            "AFT" => self.aft = menu(name, value)?,
            "PPV" => self.ppv = string(name, value)?,
            "RPV" => self.rpv = string(name, value)?,
            "DPV" => self.dpv = string(name, value)?,
            "TPV" => self.tpv = string(name, value)?,
            "SP" => self.sp = double(name, value)?,
            "EP" => self.ep = double(name, value)?,
            "NP" => self.np = long(name, value)?,
            "SC" => self.sc = short(name, value)?,
            "AQT" => self.aqt = double(name, value)?,
            "MP" => self.mp = long(name, value)?,
            "ACT" => self.act = short(name, value)?,
            "LOAD" => self.load = short(name, value)?,
            "GO" => self.go = short(name, value)?,
            "STEP" => self.step = double(name, value)?,
            "LSTP" => self.lstp = double(name, value)?,
            "OPRE" => self.opre = string(name, value)?,
            "OSM" => self.osm = string(name, value)?,
            "OAR" => self.oar = string(name, value)?,
            "OAFT" => self.oaft = string(name, value)?,
            "OPPV" => self.oppv = string(name, value)?,
            "ORPV" => self.orpv = string(name, value)?,
            "ODPV" => self.odpv = string(name, value)?,
            "OTPV" => self.otpv = string(name, value)?,
            "OSP" => self.osp = string(name, value)?,
            "OEP" => self.oep = string(name, value)?,
            "ONP" => self.onp = string(name, value)?,
            "OSC" => self.osc = string(name, value)?,
            "OAQT" => self.oaqt = string(name, value)?,
            "IMP" => self.imp = string(name, value)?,
            "IACT" => self.iact = string(name, value)?,
            "OLOAD" => self.oload = string(name, value)?,
            "OGO" => self.ogo = string(name, value)?,
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        dbd_generated::SCANPARM_FIELDS
    }

    fn declared_dbaddr_fields(&self) -> Option<&'static [&'static str]> {
        Some(dbd_generated::SCANPARM_DBADDR)
    }

    fn declared_noaccess_fields(&self) -> &'static [&'static str] {
        dbd_generated::SCANPARM_NOACCESS
    }
}
