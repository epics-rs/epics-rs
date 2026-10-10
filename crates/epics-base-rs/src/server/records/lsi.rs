use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDeclaration, MENU_POST, MENU_YES_NO, ProcessOutcome, Record};
use crate::types::{EpicsValue, SharedArray};

/// EPICS `MAX_STRING_SIZE` — DBR_STRING buffers are 40 bytes.
const MAX_STRING_SIZE: usize = 40;

/// `menuPost_Always` — index 1 of the `menuPost` menu
/// (`menuPost.dbd.pod`: `choice(menuPost_OnChange, ...)` = 0,
/// `choice(menuPost_Always, ...)` = 1). MPST/APST default to
/// `menuPost_OnChange` (0).
const MENU_POST_ALWAYS: i16 = 1;

/// C `dbGetLinkLS` (`dbLink.c:497-525`) landing a link read in a SIZV buffer.
/// Any non-char source is a DBR_STRING read through a `MAX_STRING_SIZE`
/// buffer, so it keeps at most 39 bytes whatever SIZV is. `Err` is a value no
/// link read delivers, and leaves `val`.
pub(crate) fn link_ls_read(
    value: EpicsValue,
    sizv: u16,
    val: &mut LongStringVal,
) -> Result<(), EpicsValue> {
    match value {
        EpicsValue::String(s) => {
            let max = MAX_STRING_SIZE.min(sizv as usize).saturating_sub(1);
            val.set_text(&s.as_bytes()[..s.len().min(max)]);
        }
        EpicsValue::CharArray(b) | EpicsValue::UCharArray(b) => link_ls_chars(&b, sizv, val),
        EpicsValue::Char(b) | EpicsValue::UChar(b) => link_ls_chars(&[b], sizv, val),
        other => return Err(other),
    }
    Ok(())
}

/// `dbGetLinkLS`'s char arm (`dbLink.c:507-522`): a CHAR/UCHAR source is
/// read as up to SIZV raw elements and `pbuffer[--len] = 0` then overwrites
/// the last one read, so "hij" with no NUL lands as "hi". The empty read
/// writes `pbuffer[-1]` and leaves the buffer, whose `strlen + 1` is LEN.
fn link_ls_chars(read: &[u8], sizv: u16, val: &mut LongStringVal) {
    let n = read.len().min(sizv as usize);
    match n.checked_sub(1) {
        Some(last) => {
            let text = &read[..last];
            let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
            val.set_text(&text[..end]);
        }
        None => val.terminate(),
    }
}

/// lsi/lso/printf VAL (and lsi/lso OVAL) as the `LEN` elements their
/// `get_array_info` reports (`lsiRecord.c:154`, `lsoRecord.c:196`,
/// `printfRecord.c:432`): none until first written, the text and its NUL
/// after (bar [`Self::set_text_strlen`]). LEN is this length by construction, and every reader lends the
/// elements with their count instead of building them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LongStringVal(SharedArray<u8>);

impl LongStringVal {
    /// `text` and its NUL.
    pub fn from_text(text: &[u8]) -> Self {
        let mut v = Self::default();
        v.set_text(text);
        v
    }

    /// The `LEN` elements.
    pub fn elements(&self) -> &SharedArray<u8> {
        &self.0
    }

    /// `LEN`.
    pub fn len(&self) -> u32 {
        self.0.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The elements less the terminating NUL.
    pub fn text(&self) -> &[u8] {
        match self.0.split_last() {
            Some((0, text)) => text,
            _ => &self.0,
        }
    }

    /// C's `strcpy` into VAL and `len = strlen + 1`.
    #[inline]
    pub fn set_text(&mut self, text: &[u8]) {
        // Unchanged text is already in place; rewriting it would only cost
        // the buffer's uniqueness check and the copy.
        if self.0.split_last() == Some((&0, text)) {
            return;
        }
        self.write_text(text);
    }

    #[inline(never)]
    fn write_text(&mut self, text: &[u8]) {
        self.fill(text.len() + 1, |b| {
            b[..text.len()].copy_from_slice(text);
            b[text.len()] = 0;
        });
    }

    /// `devEnviron.c::read_lsi`'s `strncpy` and `len = strlen`: the one
    /// writer whose LEN leaves the NUL out.
    pub fn set_text_strlen(&mut self, text: &[u8]) {
        if self.0[..] != *text {
            self.fill(text.len(), |b| b.copy_from_slice(text));
        }
    }

    /// C's `memcpy(oval, val, len)` and `olen = len`; true when OLEN moved.
    /// Out of line: inlined, the fill's registers are saved on every
    /// `monitor()`, the unchanged cycles that never copy included.
    #[inline(never)]
    pub fn copy_from(&mut self, src: &Self) -> bool {
        let moved = self.0.len() != src.0.len();
        self.fill(src.0.len(), |b| b.copy_from_slice(&src.0));
        moved
    }

    /// `*plen = strlen(pbuffer) + 1` over the buffer as it stands.
    pub fn terminate(&mut self) {
        let buf = self.0.buffer();
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        if self.0.len() != end + 1 {
            let text = buf[..end].to_vec();
            self.set_text(&text);
        }
    }

    /// `dbLoadLinkLS`'s number case: the buffer is untouched and LEN is 1.
    pub fn set_len_one(&mut self) {
        if self.0.is_empty() {
            self.0 = [0].into();
        } else {
            self.0.truncate(1);
        }
    }

    /// A VAL put of up to `capacity` (SIZV - 1) bytes: a DBR_STRING put is
    /// `putStringString`'s `strncpy` into the SIZV-wide field, so only the
    /// source (a CA wire string, a stringout VAL) caps it at
    /// `MAX_STRING_SIZE`; a DBR_CHAR put ends at its NUL.
    pub fn put(&mut self, value: EpicsValue, capacity: usize) -> CaResult<()> {
        let text = match &value {
            EpicsValue::String(s) => s.as_bytes(),
            EpicsValue::CharArray(bytes) => {
                &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())]
            }
            _ => return Err(CaError::TypeMismatch("VAL".into())),
        };
        self.set_text(&text[..text.len().min(capacity)]);
        Ok(())
    }

    /// Cut the text to `max` bytes.
    pub fn cut_text(&mut self, max: usize) {
        if self.text().len() > max {
            self.0.make_mut()[max] = 0;
            self.0.truncate(max + 1);
        }
    }

    /// `n` elements written by `write`, followed by a NUL where the buffer
    /// runs past them (`strncpy`'s padding, which `terminate` reads). In
    /// place while no reader holds the buffer, whatever the count, as C's
    /// SIZV-wide VAL is; otherwise one fresh buffer at the old capacity, the
    /// copy C's `db_field_log` makes for the reader.
    #[inline]
    fn fill(&mut self, n: usize, write: impl FnOnce(&mut [u8])) {
        let capacity = self.0.buffer().len().max(n);
        let (text, rest) = self.0.make_mut_resized(capacity, 0).split_at_mut(n);
        write(text);
        if let Some(pad) = rest.first_mut() {
            *pad = 0;
        }
        self.0.truncate(n);
    }
}

// Long string input record (EPICS 7).
// Native CA type is DBR_CHAR array; SIZV (default 256) sets the max byte count.
// LEN and OLEN are VAL's and OVAL's element counts.
pub struct LsiRecord {
    pub val: LongStringVal,
    pub oval: LongStringVal,
    pub sizv: u16,
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    pub sdly: f64,
    /// `menuPost` Post Value Monitors: `menuPost_OnChange` (0, default)
    /// posts DBE_VALUE only on a real change; `menuPost_Always` (1) posts
    /// DBE_VALUE every write (C `lsiRecord.dbd.pod` MPST, monitor:
    /// `if (mpst == menuPost_Always) events |= DBE_VALUE;`).
    pub mpst: i16,
    /// `menuPost` Post Archive Monitors: same as [`Self::mpst`] for the
    /// DBE_LOG (archive) mask (C `lsiRecord.dbd.pod` APST, monitor:
    /// `if (apst == menuPost_Always) events |= DBE_LOG;`).
    pub apst: i16,
    /// LEN owed by `special()` on a VAL put, or by `monitor()`'s
    /// `len != olen` on the cycle it ran; `take_cycle_posted_fields` takes it.
    len_marked: bool,
}

impl Default for LsiRecord {
    fn default() -> Self {
        Self {
            val: LongStringVal::default(),
            oval: LongStringVal::default(),
            // Intentional deviation from C `lsiRecord.dbd.pod`
            // `field(SIZV,DBF_USHORT){ initial("41") }`: the port defaults to a
            // larger 256-byte long-string buffer rather than C's 41. Kept by
            // design (a .db that needs C's size sets SIZV explicitly); not a
            // parity bug.
            sizv: 256,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            sdly: -1.0,
            mpst: 0,
            apst: 0,
            len_marked: false,
        }
    }
}

impl LsiRecord {
    pub fn new(val: &str) -> Self {
        let mut rec = Self::default();
        // An empty initial value leaves LEN=0 (C: no value present yet).
        if !val.is_empty() {
            rec.val = LongStringVal::from_text(&val.as_bytes()[..val.len().min(rec.capacity())]);
        }
        rec
    }

    /// C `devEnviron.c::read_lsi` (:54-67): a set variable's text with
    /// `len = strlen`, an unset one as the empty string with `len = 1`.
    pub fn read_environ(&mut self, text: Option<&[u8]>) {
        match text {
            Some(t) => self.val.set_text_strlen(&t[..t.len().min(self.capacity())]),
            None => self.val.set_text(b""),
        }
    }

    /// VAL's byte capacity: SIZV less the NUL. VAL never exceeds it — every
    /// write, and a SIZV put, cuts to it.
    fn capacity(&self) -> usize {
        (self.sizv as usize).saturating_sub(1)
    }
}

impl Record for LsiRecord {
    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        crate::server::record::CycleHooks::WRITE_POSTED
    }

    fn record_type(&self) -> &'static str {
        "lsi"
    }

    /// C reads INP (`devLsiSoft.c:32`) and SIOL (`lsiRecord.c:244`) through
    /// `dbGetLinkLS` (`dbLink.c:497-505`), whose switch is on the SOURCE
    /// class: a `DBF_CHAR`/`DBF_UCHAR` source is read as the bytes it
    /// spells (capped at SIZV), anything else as `DBR_STRING` — so an
    /// ENUM/MENU source delivers its state label (epics-base#183).
    ///
    /// Deliberate deviation: `dbGetLinkLS` returns 0 — success, nothing
    /// written — when the source type is unknown (`dtyp < 0`, a
    /// disconnected CA source; `dbLink.c:504-505`), so C keeps stale VAL
    /// with no alarm. That silent success is the same wart family as
    /// `putStringUlong`'s accepted-but-writes-nothing (`c_parse.rs`); the
    /// port keeps its failed-read LINK alarm instead.
    fn input_link_request(&self, link_field: &str) -> crate::server::record::InputLinkRequest {
        use crate::server::record::{InputLinkRequest, LinkReadAs};
        match link_field {
            "INP" | "SIOL" => InputLinkRequest::FromSource,
            _ => InputLinkRequest::As(LinkReadAs::Native),
        }
    }

    fn input_link_read_as_from_source(
        &self,
        _link_field: &str,
        source: &crate::server::record::OutTarget,
    ) -> Option<crate::server::record::LinkReadAs> {
        use crate::server::record::LinkReadAs;
        use crate::types::DbFieldType;
        Some(match source.field_type {
            Some(DbFieldType::Char | DbFieldType::UChar) => LinkReadAs::Native,
            _ => LinkReadAs::String,
        })
    }

    /// `DBF_MENU` fields, served as `DBR_ENUM` (`lsiRecord.dbd.pod`): `SIMM`
    /// is `menu(menuYesNo)` (two-choice NO/YES). `MPST`/`APST` are
    /// `menu(menuPost)` (On Change, Always) — unlike the array records
    /// (`aai`/`aao`/`waveform`) whose POST menus reverse that order, so they
    /// are resolved here rather than globally. `SIMS`/`OLDSIMM` are shared
    /// menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SIMM" => Some(MENU_YES_NO),
            "MPST" | "APST" => Some(MENU_POST),
            _ => None,
        }
    }

    /// C `cvt_dbaddr` serves each as one `DBF_STRING` (`no_elements = 1`,
    /// `lsiRecord.c:141`), whatever LEN holds.
    fn dbaddr_capacity(&self, field: &str) -> Option<u32> {
        self.field_is_long_string(field).then_some(1)
    }

    fn uses_monitor_deadband(&self) -> bool {
        false
    }

    /// `lsiRecord.c::process` has no unconditional UDF re-derive; UDF is
    /// cleared only by the init-time `dbLoadLinkLS` when it loads a value
    /// (`lsiRecord.c:85-89`, applied by the init-seed owner) and by the
    /// simulated SIOL read (`:245`). `devLsiSoft.c::readLocked` never touches
    /// UDF, so a soft INP read keeps it (see [`Self::soft_read_clears_udf`]).
    fn clears_udf(&self) -> bool {
        false
    }

    fn soft_read_clears_udf(&self) -> bool {
        false
    }

    /// `lsiRecord.c::process` never re-derives UDF after a DEVICE read that
    /// wrote VAL directly (C `return 2`): its two `prec->udf = FALSE` sites
    /// are `init_record` (`:88`) and the simulated SIOL read (`:245`). Same
    /// asyn octet consequence as `stringin` — a partial read on a transport
    /// error stores VAL but must leave UDF alone
    /// (`devAsynOctet.c::callbackSiRead:918-932`).
    fn rederives_udf_on_computed_read(&self) -> bool {
        false
    }

    /// `lsiRecord.c` has NO `recGblCheckUdf` / `UDF_ALARM` (unlike
    /// `lsoRecord.c:118`): an undefined lsi raises no alarm from UDF (softIoc:
    /// `record(lsi,"X"){}` → UDF 1, STAT/SEVR = NO_ALARM). With `clears_udf`
    /// false, UDF can now legitimately stay 1, so this MUST be false or
    /// `rec_gbl_check_udf` would invent an alarm C never raises.
    fn raises_udf_alarm(&self) -> bool {
        false
    }

    /// C `devLsiSoft.c:24` — the soft input support's
    /// `dbLoadLinkLS(&prec->inp, prec->val, prec->sizv, &prec->len)`. The
    /// load runs in the init-seed owner, which gates it on the soft DTYP the
    /// way C gates it on which device support is bound.
    fn constant_ls_link(&self) -> Option<&'static str> {
        Some("INP")
    }

    /// The `dbLoadLinkLS` sink plus C's init tail (`lsiRecord.c:85-88`).
    fn apply_ls_load(&mut self, load: crate::server::record::LsLoad) -> u32 {
        match load {
            crate::server::record::LsLoad::Text(s) => {
                self.val
                    .set_text(&s.as_bytes()[..s.len().min(self.capacity())]);
            }
            crate::server::record::LsLoad::LenOnly => self.val.set_len_one(),
        }
        self.oval.copy_from(&self.val);
        self.val.len()
    }

    /// C `lsiRecord.c:206-210` `monitor()`: `if (prec->len != prec->olen ||
    /// memcmp(prec->oval, prec->val, prec->len)) { events |= DBE_VALUE |
    /// DBE_LOG; memcpy(prec->oval, prec->val, prec->len); }` — compared and
    /// committed HERE, at C's position. (`olen` follows from the separate
    /// `if (prec->len != prec->olen)` two lines down, which also posts LEN.)
    ///
    /// For `lso` this is what makes the IVOA = Set_output_to_IVOV write post
    /// on its OWN cycle: the framework's IVOA owner runs after
    /// `process()` returns, so a flag captured in `process()` would be the
    /// verdict on the PRE-arm value.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let changed = self.oval != self.val;
        if changed {
            // Unchanged bytes have an unchanged length, and the take that
            // `marked` guarantees has already cleared the mark.
            self.len_marked = self.oval.copy_from(&self.val);
        }
        Some(changed)
    }

    /// C `lsiRecord.c:212-214`: `if (len != olen) { olen = len; db_post_events(&len,
    /// DBE_VALUE | DBE_LOG); }`, marked by `monitor_value_changed` as it
    /// commits OVAL, whose length is OLEN.
    fn monitor_value_classes(
        &mut self,
        _gate: crate::server::record::MonitorValueGate,
        _last_posted: &mut [f64; 2],
    ) -> crate::server::record::MonitorValueClasses {
        let changed = self.monitor_value_changed() == Some(true);
        let (val_always, archive_always) = self.monitor_always_post();
        crate::server::record::MonitorValueClasses {
            value: changed || val_always,
            archive: changed || archive_always,
            hash_changed: false,
            marked: self.len_marked,
        }
    }

    /// C `special()` sets `len` and posts it on every VAL put (lsiRecord.c:197),
    /// changed or not; `monitor()` posts it again only when `len != olen`.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if after && field == "VAL" {
            self.len_marked = true;
        }
        Ok(())
    }

    fn take_cycle_posted_fields(&mut self) -> crate::server::record::CycleMarks {
        use crate::server::record::{CycleMarks, CyclePostMask};
        static LEN_POST: [(&str, CyclePostMask); 1] = [("LEN", CyclePostMask::ValueLog)];
        CycleMarks::of(&LEN_POST, std::mem::take(&mut self.len_marked) as u64)
    }

    /// LEN posts from `special()` and from `monitor()`'s `olen` compare, never
    /// against what a subscriber was last sent.
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &["LEN"]
    }

    /// C `monitor()` posts VAL and LEN alone (lsiRecord.c:204-223); the
    /// OVAL/OLEN copy it makes is never posted.
    fn process_posted_fields(&self) -> Option<&'static [&'static str]> {
        Some(&["VAL", "LEN"])
    }

    fn monitor_always_post(&self) -> (bool, bool) {
        // C `lsiRecord.c` monitor: `if (mpst == menuPost_Always) events |=
        // DBE_VALUE; if (apst == menuPost_Always) events |= DBE_LOG;`.
        (self.mpst == MENU_POST_ALWAYS, self.apst == MENU_POST_ALWAYS)
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    /// For the getenv dset's [`Self::read_environ`].
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::CharArray(self.val.elements().clone()))
    }

    /// [`Self::val`]'s elements, borrowed.
    fn monitor_value_view(&self) -> Option<crate::server::record::ValueView<'_>> {
        Some(crate::server::record::ValueView::Char(self.val.elements()))
    }

    /// `devLsiSoft.c::readLocked` is `dbGetLinkLS(pinp, prec->val, prec->sizv,
    /// &prec->len)`: the read (DBR_STRING, or a char source's bytes up to
    /// SIZV) lands in VAL through [`link_ls_read`], and LEN is `strlen + 1`. The read is
    /// already VAL's text, so it skips the generic put's shape lookups and
    /// its 40-byte DBR_STRING put cap, which C's char-source read does not have.
    fn soft_input_read(&mut self, value: Option<EpicsValue>) -> CaResult<()> {
        let Some(value) = value else { return Ok(()) };
        match link_ls_read(value, self.sizv, &mut self.val) {
            Ok(()) => Ok(()),
            Err(other) => self.set_val(other),
        }
    }

    /// The simulated SIOL read is the same `dbGetLinkLS` (`lsiRecord.c:244`).
    fn land_simulated_value(&mut self, value: EpicsValue) -> CaResult<()> {
        self.soft_input_read(Some(value))
    }

    fn link_text_ref(&self, link_field: &str) -> Option<&str> {
        match link_field {
            "SIML" => Some(&self.siml),
            "SIOL" => Some(&self.siol),
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

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => self.val(),
            "OVAL" => Some(EpicsValue::CharArray(self.oval.elements().clone())),
            "SIZV" => Some(EpicsValue::UShort(self.sizv)),
            "LEN" => Some(EpicsValue::ULong(self.val.len())),
            "OLEN" => Some(EpicsValue::ULong(self.oval.len())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "SDLY" => Some(EpicsValue::Double(self.sdly)),
            "MPST" => Some(EpicsValue::Short(self.mpst)),
            "APST" => Some(EpicsValue::Short(self.apst)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                let capacity = self.capacity();
                self.val.put(value, capacity)?;
            }
            "SIZV" => {
                // SIZV is DBF_USHORT (lsiRecord.dbd.pod:75): a client put
                // arrives as UShort, internal callers may still pass Short.
                let raw = match value {
                    EpicsValue::UShort(v) => v as i32,
                    EpicsValue::Short(v) => v as i32,
                    _ => return Err(CaError::TypeMismatch("SIZV".into())),
                };
                // C `lsiRecord.c:46-55`: SIZV clamps to [16, 0x7fff].
                self.sizv = raw.clamp(16, 0x7fff) as u16;
                // C sizes the buffer once (SPC_NOMOD); the port takes SIZV
                // after a .db VAL, so the cut lands here.
                let capacity = self.capacity();
                self.val.cut_text(capacity);
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
                    self.siml = v.as_str_lossy().into_owned();
                } else {
                    return Err(CaError::TypeMismatch("SIML".into()));
                }
            }
            "SIOL" => {
                if let EpicsValue::String(v) = value {
                    self.siol = v.as_str_lossy().into_owned();
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
            "MPST" => {
                if let EpicsValue::Short(v) = value {
                    self.mpst = v;
                } else {
                    return Err(CaError::TypeMismatch("MPST".into()));
                }
            }
            "APST" => {
                if let EpicsValue::Short(v) = value {
                    self.apst = v;
                } else {
                    return Err(CaError::TypeMismatch("APST".into()));
                }
            }
            _ => return Err(CaError::FieldNotFound(name.to_string())),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dbLSConvertJSON`'s number case leaves the buffer and sets `LEN = 1`,
    /// so a written VAL keeps only its first element in view.
    #[test]
    fn set_len_one_keeps_one_element_of_a_written_val() {
        let mut v = LongStringVal::from_text(b"abc");
        v.set_len_one();
        assert_eq!(v.len(), 1);
        assert_eq!(v.elements()[..], *b"a");

        let mut empty = LongStringVal::default();
        empty.set_len_one();
        assert_eq!(empty.elements()[..], [0]);
    }

    /// A shorter write lands in the same buffer, NUL-padded as `strncpy`
    /// pads it, so the empty read's `strlen` stops at the new text.
    #[test]
    fn a_shorter_write_reuses_the_buffer_and_pads_it() {
        let mut v = LongStringVal::from_text(b"abcdef");
        let p = v.elements().as_ptr();
        v.set_text_strlen(b"ab");
        assert_eq!(v.elements().as_ptr(), p);
        v.terminate();
        assert_eq!((v.text(), v.len()), (&b"ab"[..], 3));
        v.set_text(b"abcd");
        assert_eq!(v.elements().as_ptr(), p);
    }

    /// A buffer a reader still holds is left to it; the write takes one copy.
    #[test]
    fn a_write_under_a_reader_copies_once() {
        let mut v = LongStringVal::from_text(b"abc");
        let reader = v.elements().clone();
        v.set_text(b"xy");
        assert!(!v.elements().ptr_eq(&reader));
        assert_eq!(
            (&reader[..], &v.elements()[..]),
            (&b"abc\0"[..], &b"xy\0"[..])
        );
    }
}
