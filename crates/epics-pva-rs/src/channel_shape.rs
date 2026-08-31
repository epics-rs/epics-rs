//! The one owner of pvxs's `bool isArray = dbChannelFinalElements(chan) != 1`
//! (`ioc/iocsource.cpp:631`) — the predicate that decides whether a QSRV
//! channel is served as `NTScalar`/`NTScalarArray`, whether its PUT goes
//! through `putScalar` (`:599-601`), and what a `+type:"any"` member's
//! payload holds (`ioc/field.cpp:38-45`).
//!
//! It lives here, below both servers, because both of them serve the same
//! `dbChannel` namespace and each used to answer the question for itself off
//! the FTVL storage variant. That is not the same predicate: an `aai` with
//! `NELM=1` stores a one-element `DoubleArray`, so both servers advertised
//! `NTScalarArray` where C serves `NTScalar` — visible on the wire as 20 PVA
//! oracle defects (`aai`/`aao` VAL, `acalcout` AVAL/AA..LL/OAV, `compress`
//! VAL) and as `pvxput` refusing the drive with "Unable to assign string[]
//! with String".

use epics_base_rs::server::record::{FieldDeclaration, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

/// The shape a channel serves, from its element count alone.
///
/// pvxs takes the ELEMENT TYPE from `fromDbrType(dbChannelFinalFieldType)`
/// and the SHAPE from this count; the two are independent, so a channel
/// backed by an array buffer can still be a scalar and nothing about the
/// stored variant may say otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelShape {
    /// `dbChannelFinalElements(chan) == 1` — one element, no `arrayOf()`.
    Scalar,
    /// `dbChannelFinalElements(chan) != 1`.
    Array,
}

/// `dbChannelFinalElements(chan)` for a channel bound to `field_upper`.
///
/// `dbNameToAddr` seeds `paddr->no_elements = 1` and nothing but a
/// `special(SPC_DBADDR)` field's `cvt_dbaddr` ever raises it, which is
/// exactly the population [`FieldDeclaration::field_native_count`] answers
/// for. Its `None` means "the channel count is the value's own count" — the
/// answer for a plain scalar field (one) and for a `SPC_DBADDR` field whose
/// record declares no `cvt_dbaddr` capacity (`histogram` VAL, whose buffer is
/// always `NELM` long).
/// Whether `field_upper` is one of the record's long-string fields
/// (`lsi`/`lso` VAL+OVAL, `printf` VAL), matched as
/// [`Record::long_string_fields`] specifies.
fn is_long_string(record: &dyn Record, field_upper: &str) -> bool {
    record
        .long_string_fields()
        .iter()
        .any(|f| f.eq_ignore_ascii_case(field_upper))
}

pub fn channel_final_elements(
    record: &dyn Record,
    field_upper: &str,
    resolved: Option<&EpicsValue>,
) -> u32 {
    record
        .field_native_count(field_upper)
        .unwrap_or_else(|| resolved.map(EpicsValue::count).unwrap_or(1))
}

impl ChannelShape {
    /// Classify the channel bound to `record.field_upper`. `resolved` is the
    /// value that channel serves (`client_field_value`, i.e. already
    /// projected onto the field's declared type).
    pub fn of_channel(
        record: &dyn Record,
        field_upper: &str,
        resolved: Option<&EpicsValue>,
    ) -> Self {
        // A long-string field is never a scalar channel. C's `cvt_dbaddr`
        // gives it `no_elements = SIZV` (`lsiRecord.c:127-134`), never 1, and
        // pvxs reaches its long-string arms (`putLongString`,
        // `iocsource.cpp:602-603`) precisely BECAUSE the channel is not
        // scalar-shaped. This port models that storage as a `String` — count
        // 1 — so the count is not what can answer here; the declaration is.
        if is_long_string(record, field_upper) {
            return ChannelShape::Array;
        }
        if channel_final_elements(record, field_upper, resolved) == 1 {
            ChannelShape::Scalar
        } else {
            ChannelShape::Array
        }
    }

    /// [`Self::of_channel`] for a live record, resolving the field's value
    /// only when the count actually needs it.
    ///
    /// `field_native_count` answers every field the `.dbd` declares
    /// `SPC_DBADDR` with a capacity, and every field it does not with the
    /// scalar `None`; only a `SPC_DBADDR` field whose record declares no
    /// capacity falls through to the value's own count. Resolving eagerly
    /// would clone a whole waveform buffer on the read path to ask a question
    /// its `NELM` already answered.
    pub fn of_record_channel(
        instance: &epics_base_rs::server::record::RecordInstance,
        field_upper: &str,
    ) -> Self {
        match instance.record.field_native_count(field_upper) {
            Some(1) => ChannelShape::Scalar,
            Some(_) => ChannelShape::Array,
            None => Self::of_channel(
                &*instance.record,
                field_upper,
                instance.client_field_value(field_upper).as_ref(),
            ),
        }
    }

    /// pvxs's `isArray`.
    pub fn is_array(self) -> bool {
        matches!(self, ChannelShape::Array)
    }

    /// `value` re-rendered in this shape, or `None` when it already has it.
    ///
    /// The scalar arm is `getScalarValue` (`iocsource.cpp:69-116`): one
    /// element read out of the buffer, and a zeroed buffer when the channel
    /// has none left (`nReq == 0` — "this was an actual max length 1 array,
    /// which has zero elements now"), which is why an empty `FTVL=STRING`
    /// `NELM=1` buffer serves `""` and not a zero-length array. The array arm
    /// keeps the buffer as `getArrayValue` does.
    ///
    /// `None` rather than a clone so the array channels — every large
    /// waveform on the box — pay nothing for asking.
    pub fn collapsed(self, value: &EpicsValue) -> Option<EpicsValue> {
        if self.is_array() || !value.is_array() {
            return None;
        }
        Some(
            value
                .first_element()
                .unwrap_or_else(|| zero_of(value.db_field_type())),
        )
    }
}

/// The element type's zero — C's `memset(&buf, 0, sizeof(buf))` for the
/// `nReq == 0` read above.
fn zero_of(dbf: DbFieldType) -> EpicsValue {
    match dbf {
        DbFieldType::String => EpicsValue::String(PvString::default()),
        DbFieldType::Short => EpicsValue::Short(0),
        DbFieldType::Float => EpicsValue::Float(0.0),
        DbFieldType::Enum => EpicsValue::Enum(0),
        DbFieldType::Char => EpicsValue::Char(0),
        DbFieldType::Long => EpicsValue::Long(0),
        DbFieldType::Double => EpicsValue::Double(0.0),
        DbFieldType::Int64 => EpicsValue::Int64(0),
        DbFieldType::UInt64 => EpicsValue::UInt64(0),
        DbFieldType::UShort => EpicsValue::UShort(0),
        DbFieldType::ULong => EpicsValue::ULong(0),
        DbFieldType::UChar => EpicsValue::UChar(0),
    }
}

impl ChannelShape {
    /// Put a channel snapshot's value into the shape the channel serves.
    ///
    /// Every renderer downstream of a snapshot — the NT id, the descriptor,
    /// the value leaf, the monitor event — reads the shape off `snap.value`,
    /// so shaping it once here is what stops them from having to agree by
    /// convention.
    pub fn shape(self, snap: &mut epics_base_rs::server::snapshot::Snapshot) {
        if let Some(v) = self.collapsed(&snap.value) {
            snap.value = v;
        }
    }
}
