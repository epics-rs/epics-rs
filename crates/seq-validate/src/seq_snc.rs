//! What `seq_snc.h` and the headers it includes declare to every generated
//! C program, for the validate programs that name it bare.

use std::ffi::c_int;

use epics_seq_rs::{SeqThreadId, seq_show};

pub use self::{epics_thread_get_id_self as epicsThreadGetIdSelf, seq_show_thread as seqShow};
pub use epics_seq_rs::seq_stop as seqStop;

/// epicsTypes.h `MAX_STRING_SIZE`, an `int` as C's macro is.
pub const MAX_STRING_SIZE: c_int = epics_seq_rs::MAX_STRING_SIZE as c_int;

/// `epicsThreadGetIdSelf`.
pub fn epics_thread_get_id_self() -> SeqThreadId {
    SeqThreadId::current()
}

/// `seqShow`.
pub fn seq_show_thread(tid: SeqThreadId) {
    seq_show(Some(tid));
}
