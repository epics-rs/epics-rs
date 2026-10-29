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

use epics_base_rs::server::database::char_view::{ChannelAddr, DollarView};
use epics_base_rs::server::database::filters::FilterChain;
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

impl ChannelShape {
    /// The shape of a channel whose filter chain leaves `final_elements`.
    pub fn of_count(final_elements: usize) -> Self {
        if final_elements == 1 {
            ChannelShape::Scalar
        } else {
            ChannelShape::Array
        }
    }

    /// Of the channel addressed `addr` through its filter chain `filters`,
    /// whose `arr` reshapes the count as its `channel_report` does.
    ///
    /// A long-string field is never a scalar channel. C's `cvt_dbaddr` gives
    /// it `no_elements = 1`, but pvxs reshapes a `DBF_STRING` wider than
    /// `MAX_STRING_SIZE` into a `SIZV`-element `DBF_CHAR` channel
    /// (`ioc/channel.cpp:58-67`) and reaches its long-string arms
    /// (`putLongString`, `iocsource.cpp:602-603`) BECAUSE of that, so the
    /// count is not what can answer here; the declaration is.
    pub fn of_addr(addr: &ChannelAddr, filters: &FilterChain) -> Self {
        if addr.long_string() {
            return ChannelShape::Array;
        }
        Self::of_count(filters.final_element_count(addr.no_elements()))
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
pub(crate) fn zero_of(dbf: DbFieldType) -> EpicsValue {
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

/// pvxs `getChannelValueType` (`ioc/iocsource.cpp:619-643`) for a record
/// channel without `$`: the one answer its read, its descriptor and its put
/// take, in both servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelValue {
    /// `fromDbrType(final_field_type)` in this shape.
    Shaped(ChannelShape),
    /// Read through its char address: a channel pvxs re-viewed
    /// (`ioc/channel.cpp:56-73`), or a `DBF_CHAR` array whose format is
    /// `"String"`. [`DollarView::served`] is what it serves: the buffer's
    /// text (`:633`, `:124-136`), or whatever `type` and count the chain
    /// left. A string put to an array of it is `putLongString`
    /// (`:601-603`).
    Text(DollarView),
    /// A long string with no room past `MAX_STRING_SIZE + 1`, which pvxs
    /// leaves `DBF_STRING` (`ioc/channel.cpp:58-60`): the buffer read to its
    /// NUL.
    LongStringField,
}

impl ChannelValue {
    /// Of the channel addressed `addr` on `field_upper` through `filters`.
    pub fn of_addr(addr: &ChannelAddr, field_upper: &str, filters: &FilterChain) -> Self {
        let view = addr.re_view();
        // Still `DBR_STRING`, whatever its `Q:form`.
        if view.is_none() && addr.long_string() {
            return Self::LongStringField;
        }
        // The reshaped chars, whatever the chain leaves of them.
        if let Some(view) = view {
            return Self::Text(DollarView::of_text_channel(
                Some(view),
                view.no_elements(),
                filters,
            ));
        }
        let shape = ChannelShape::of_count(filters.final_element_count(addr.no_elements()));
        // A `DBF_CHAR` field — the signed one, as pvxs tests.
        if addr.value_type() == DbFieldType::Char
            && shape.is_array()
            && addr.form_string(field_upper, None, false)
        {
            return Self::Text(DollarView::of_text_channel(
                None,
                addr.no_elements(),
                filters,
            ));
        }
        Self::Shaped(shape)
    }
}
