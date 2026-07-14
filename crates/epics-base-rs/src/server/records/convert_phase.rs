//! `INIT` — "is the next conversion the initial one?" — for the analog records.
//!
//! C keeps this as `prec->init`, a `DBF_SHORT` served read-only (`SPC_NOMOD`)
//! on `ai` and `ao`. Its lifecycle is identical in both:
//!
//! * `init_record` sets it TRUE (`aiRecord.c:114`, `aoRecord.c:120`);
//! * `process` clears it FALSE on the way out (`aiRecord.c:170`,
//!   `aoRecord.c:237`) — every process, not just the first;
//! * an `SPC_LINCONV` special (a put to `LINR` / `EGUF` / `EGUL`) sets it TRUE
//!   again (`aiRecord.c:187`, `aoRecord.c:254`).
//!
//! While TRUE it means the next conversion has no history to build on:
//! `cvtRawToEngBpt` re-resolves the breakpoint table instead of reusing the
//! cached interval, and `ai`'s SMOO filter takes `prec->val = val` as its
//! initial condition rather than smoothing against a stale VAL.
//!
//! It is a phase, not a "primed" bit, and the two readings have opposite
//! polarity — which is how the port came to serve `INIT = 0` on a freshly
//! initialised `ai` where C serves 1 (measured: `record(ai,"P:AI"){}` on the
//! compiled softIoc, `caget -t P:AI.INIT` -> `1`). Naming the two states
//! removes the polarity from the call sites: there is no `init = false` to read
//! backwards, only `ConvertPhase::Initial` and `ConvertPhase::Converted`.

use crate::types::EpicsValue;

/// C `prec->init` on `ai`/`ao`, as a named phase rather than a bare flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConvertPhase {
    /// C `init == TRUE`: the record has been initialised (or its conversion
    /// re-tuned through `SPC_LINCONV`) and has not processed since, so the next
    /// conversion is the initial one.
    Initial,
    /// C `init == FALSE`: the record has processed at least once since it was
    /// initialised, so the next conversion continues from that history.
    #[default]
    Converted,
}

impl ConvertPhase {
    /// Is the next conversion the initial one? (C `if (prec->init)`.)
    pub fn is_initial(self) -> bool {
        matches!(self, Self::Initial)
    }

    /// The `INIT` field as a client reads it: `DBF_SHORT`, 1 while initial.
    pub fn as_field(self) -> EpicsValue {
        EpicsValue::Short(if self.is_initial() { 1 } else { 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C callocs the record, so an `INIT` nobody has initialised reads 0 — the
    /// same zero `.dbd` gives every field with no `initial()`.
    #[test]
    fn r21_the_zero_of_the_phase_is_converted() {
        assert_eq!(ConvertPhase::default(), ConvertPhase::Converted);
        assert_eq!(ConvertPhase::default().as_field(), EpicsValue::Short(0));
        assert!(!ConvertPhase::default().is_initial());
    }

    #[test]
    fn r21_the_initial_phase_serves_one() {
        assert_eq!(ConvertPhase::Initial.as_field(), EpicsValue::Short(1));
        assert!(ConvertPhase::Initial.is_initial());
    }
}
