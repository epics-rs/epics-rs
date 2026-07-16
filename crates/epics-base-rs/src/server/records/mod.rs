use crate::error::{CaError, CaResult};
use crate::types::{DbFieldType, EpicsValue};

/// The `RVAL` store behind every `DTYP="Raw Soft Channel"` INPUT dset — ai,
/// bi, mbbi, mbbiDirect.
///
/// C reads the link straight into the raw word: `dbGetLink(&prec->inp,
/// DBR_LONG|DBR_ULONG, &prec->rval, ...)`. `dbGetLink` picks its conversion
/// routine from BOTH ends (`dbFastGetConvertRoutine`, `dbConvert.c:1571-1638`),
/// so what happens to an out-of-range value depends on the SOURCE type: an
/// integer source reaches `getLongUlong` and converts MODULO 2^32, which C
/// defines (C17 6.3.1.3p2) — `0xdeadbeef` read from a signed `DBF_LONG` lands in
/// RVAL as `0xdeadbeef` — while a float source reaches `getDoubleUlong` and hits
/// the UB cast that CBUG-E2 saturates.
///
/// [`EpicsValue::convert_to`] is the owner of that split. Reaching for
/// `c_cast::f64_to_*(value.to_f64())` instead — as all four dsets once did —
/// forces the FLOAT rule onto integer sources and saturates what C wraps.
///
/// `convert_to` coerces rather than fails, so the not-numeric rejection has to
/// stay explicit: without this gate a string INP would land in RVAL as a silent
/// 0 instead of C's `S_db_badDbrtype`.
fn raw_soft_rval(record: &str, value: &EpicsValue, rval_type: DbFieldType) -> CaResult<EpicsValue> {
    if value.to_f64().is_none() {
        return Err(CaError::TypeMismatch(
            format!("{record} Raw Soft Channel: INP value not numeric").into(),
        ));
    }
    Ok(value.convert_to(rval_type))
}

/// [`raw_soft_rval`] for the dsets whose `RVAL` is `epicsInt32` (ai).
pub(crate) fn raw_soft_rval_i32(record: &str, value: &EpicsValue) -> CaResult<i32> {
    match raw_soft_rval(record, value, DbFieldType::Long)? {
        EpicsValue::Long(v) => Ok(v),
        other => Err(CaError::TypeMismatch(
            format!("{record} Raw Soft Channel: INP value not a scalar ({other:?})").into(),
        )),
    }
}

/// [`raw_soft_rval`] for the dsets whose `RVAL` is `epicsUInt32` (bi, mbbi,
/// mbbiDirect).
pub(crate) fn raw_soft_rval_u32(record: &str, value: &EpicsValue) -> CaResult<u32> {
    match raw_soft_rval(record, value, DbFieldType::ULong)? {
        EpicsValue::ULong(v) => Ok(v),
        other => Err(CaError::TypeMismatch(
            format!("{record} Raw Soft Channel: INP value not a scalar ({other:?})").into(),
        )),
    }
}

/// A put into one of the array records' bookkeeping counters — waveform/aai/aao
/// NELM, subArray NELM/MALM/INDX, compress NSAM/N, histogram NELM/MDEL — which C
/// declares `DBF_ULONG` (or `DBF_SHORT`/`DBF_USHORT` on histogram).
///
/// `dbPutField` converts the client's DBR type to the FIELD's type before
/// storing (the `dbConvert.c` put table), so a counter takes any numeric put —
/// which is what the port's coercion owner delivers here (a wire put arrives
/// already converted to the declared `ULong`, a direct `put_field` from the
/// framework or a unit test may carry `Long`/`Short`). `None` means the value is
/// not numeric at all, i.e. C's `S_db_badDbrtype`.
///
/// The counters are stored `i32` — each record's own arithmetic is signed (INDX
/// offsets, `nord - indx` slice lengths, the compress ring cursor) — and every
/// put arm floors what it stores, so a stored counter is never negative and the
/// `as u32` on the read side is exact.
pub(crate) fn count_put(value: &EpicsValue) -> Option<i32> {
    // Through the coercion owner, NOT `c_cast::f64_to_i32(v.to_f64())`: `to_f64`
    // erases the source's integer-ness, and C picks its conversion routine BY the
    // source type. A `ULong(0x8000_0000)` put would otherwise take the float rule
    // and saturate to `i32::MAX`, where C's `putUlongLong` wraps.
    value.to_dbf_i32()
}

pub mod acalcout;
pub mod ai;
pub mod alarm_filter;
pub mod ao;
pub mod asub_record;
pub mod asyn_record;
pub mod bi;
pub mod bo;
pub mod busy;
pub mod calc;
pub(crate) mod calc_compile;
pub mod calcout;
pub mod compress;
pub mod convert_phase;
pub mod dfanout;
pub mod event;
pub mod fanout;
pub mod histogram;
pub mod int64in;
pub mod int64out;
pub mod link_status;
pub mod longin;
pub mod longout;
pub mod lsi;
pub mod lso;
pub mod mbbi;
pub mod mbbi_direct;
pub mod mbbo;
pub mod mbbo_direct;
pub mod permissive;
pub mod printf;
pub mod scalcout;
pub mod sel;
pub mod seq;
pub mod sseq;
pub mod state;
pub mod stringin;
pub mod stringout;
pub mod sub_record;
pub mod swait;
pub mod transform;
pub mod waveform;
