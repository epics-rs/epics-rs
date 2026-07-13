use crate::types::EpicsValue;

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
    value.to_f64().map(crate::types::c_cast::f64_to_i32)
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
