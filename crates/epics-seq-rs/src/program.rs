//! The static tables snc-rs generates (`seq_snc.h`, option bits of
//! `seqCom.h`), and the function types the generated code implements.

use crate::state_set::StateSet;
use crate::types::{EfId, PrimType, SeqMask, nwords};
use crate::var::{C_CHAR_KIND, ChanCell, ElemKind, ProgramVars};

/// `MAGIC` (`seq_snc.h`): the table layout version snc and seq agree on.
pub const SEQ_MAGIC: u32 = 2_002_009;

/// `SEQ_EVENT_FUNC`: true when a transition fires, with its number in
/// `ptrn` and the next state's index in `pnst`.
pub type EventFn<V> =
    fn(seqg_env: &StateSet<V>, seqg_ptrn: &mut usize, seqg_pnst: &mut usize) -> bool;

/// `SEQ_TRANS_FUNC`: the action of transition `trn`; it may overwrite
/// `pnst`.
pub type ActionFn<V> = fn(seqg_env: &StateSet<V>, seqg_trn: usize, seqg_pnst: &mut usize);

/// `SEQ_SS_FUNC`: state entry/exit, and program entry/exit.
pub type SsFn<V> = fn(seqg_env: &StateSet<V>);

/// `SEQ_PROG_FUNC`: `seqg_init`, the variable initializers. It runs once
/// per program start in every variable model, on the instance's block
/// (C hands it the program only to reach that block).
pub type InitFn<V> = fn(seqg_var: &V);

/// A state's option bits (`seqCom.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateOptions(SeqMask);

impl StateOptions {
    pub const NORESETTIMERS: SeqMask = 1 << 0;
    pub const DOENTRYFROMSELF: SeqMask = 1 << 1;
    pub const DOEXITTOSELF: SeqMask = 1 << 2;

    pub const fn new(bits: SeqMask) -> Self {
        assert!(
            bits & !(Self::NORESETTIMERS | Self::DOENTRYFROMSELF | Self::DOEXITTOSELF) == 0,
            "not a state option bit"
        );
        StateOptions(bits)
    }

    pub const fn test(self, bit: SeqMask) -> bool {
        self.0 & bit != 0
    }
}

/// A program's option bits (`seqCom.h`), less `OPT_REENT` and `OPT_SAFE`:
/// those are [`VarModel`], so safe mode without reentrancy cannot be
/// written. A table that sets either bit does not compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgOptions(SeqMask);

impl ProgOptions {
    pub const DEBUG: SeqMask = 1 << 0;
    pub const ASYNC: SeqMask = 1 << 1;
    pub const CONN: SeqMask = 1 << 2;
    pub const NEWEF: SeqMask = 1 << 4;

    pub const fn new(bits: SeqMask) -> Self {
        assert!(
            bits & !(Self::DEBUG | Self::ASYNC | Self::CONN | Self::NEWEF) == 0,
            "OPT_REENT and OPT_SAFE are SeqProgram::vars, not option bits"
        );
        ProgOptions(bits)
    }

    pub const fn test(self, bit: SeqMask) -> bool {
        self.0 & bit != 0
    }
}

/// Where a program's variables live: snc's `-r`, `+r` and `+s`.
pub enum VarModel<V: 'static> {
    /// `-r`: one block for every start of the program, as C's statics.
    /// snc-rs emits a `LazyLock<seqg_vars>` and a function returning it.
    Static(fn() -> &'static V),
    /// `+r`: a zeroed block per start.
    Reentrant,
    /// `+s` (implies `+r`): a block per start that only the pv layer
    /// writes, and a copy per state set that only its state set touches.
    Safe,
}

impl<V> VarModel<V> {
    /// `seq_optGet('r')`.
    pub fn reentrant(&self) -> bool {
        !matches!(self, VarModel::Static(_))
    }

    /// `seq_optGet('s')`.
    pub fn safe(&self) -> bool {
        matches!(self, VarModel::Safe)
    }
}

/// `struct seqChan`: one assigned variable, or one element of a multi-PV
/// array.
pub struct SeqChan<V: 'static> {
    /// The pv name before macro substitution; `""` for `assign x;` or
    /// `assign x to ""`, and `None` for an element of a multi-PV array that
    /// no `assign x[i] to` names. Each waits for `pvAssign`.
    pub ch_name: Option<&'static str>,
    /// The cell this channel transfers, replacing C's `offset`.
    pub var: fn(&V) -> &dyn ChanCell,
    /// The variable name with its subscript, as `seqChanShow` prints it.
    pub var_name: &'static str,
    pub var_type: PrimType,
    pub count: u32,
    pub event_num: u32,
    /// The flag a `sync` / `syncQ` ties the channel to, or
    /// [`crate::NOEVFLAG`].
    pub ef_id: EfId,
    pub monitored: bool,
    /// `syncQ` queue size, 0 when not queued.
    pub queue_size: u32,
    pub queue_index: u32,
}

/// `struct seqState`.
pub struct SeqState<V: 'static> {
    pub state_name: &'static str,
    pub action_func: ActionFn<V>,
    pub event_func: EventFn<V>,
    pub entry_func: Option<SsFn<V>>,
    pub exit_func: Option<SsFn<V>>,
    pub event_mask: &'static [SeqMask],
    pub options: StateOptions,
}

/// `struct seqSS`.
pub struct SeqSs<V: 'static> {
    pub ss_name: &'static str,
    pub states: &'static [SeqState<V>],
}

/// `struct seqProgram`: the one public item a compiled `.st` exports.
pub struct SeqProgram<V: 'static> {
    pub magic: u32,
    pub prog_name: &'static str,
    pub chan: &'static [SeqChan<V>],
    pub ss: &'static [SeqSs<V>],
    pub vars: VarModel<V>,
    /// The `program name("...")` parameters, which `seq`'s macros override.
    pub params: &'static str,
    pub num_ev_flags: u32,
    pub options: ProgOptions,
    pub init_func: InitFn<V>,
    pub entry_func: Option<SsFn<V>>,
    pub exit_func: Option<SsFn<V>>,
    pub num_queues: u32,
}

impl PrimType {
    /// The element kind of the Rust type snc-rs declares for this C type.
    pub fn elem_kind(self) -> ElemKind {
        match self {
            PrimType::Char => C_CHAR_KIND,
            PrimType::UChar | PrimType::UInt8T => ElemKind::U8,
            PrimType::Int8T => ElemKind::I8,
            PrimType::Short | PrimType::Int16T => ElemKind::I16,
            PrimType::UShort | PrimType::UInt16T => ElemKind::U16,
            PrimType::Int | PrimType::Int32T => ElemKind::I32,
            PrimType::UInt | PrimType::UInt32T => ElemKind::U32,
            PrimType::Long => ElemKind::I64,
            PrimType::ULong => ElemKind::U64,
            PrimType::Float => ElemKind::F32,
            PrimType::Double => ElemKind::F64,
            PrimType::String => ElemKind::Str,
        }
    }
}

impl<V: ProgramVars> SeqProgram<V> {
    /// Check what C trusts snc for: the table indices agree with each
    /// other, and each channel's cell is the type and length the table
    /// says. `seq` refuses a program that fails.
    pub fn validate(&self) -> Result<(), String> {
        let fail = |what: String| Err(format!("program {}: {what}", self.prog_name));
        if self.magic != SEQ_MAGIC {
            return fail(format!("magic {} is not {SEQ_MAGIC}", self.magic));
        }
        if self.ss.is_empty() {
            return fail("no state sets".into());
        }
        let vars = V::zeroed();
        for (n, ch) in self.chan.iter().enumerate() {
            let at = |what: String| fail(format!("channel {n} ({}): {what}", ch.var_name));
            let expect = self.num_ev_flags + 1 + n as u32;
            if ch.event_num != expect {
                return at(format!("event number {} is not {expect}", ch.event_num));
            }
            if ch.ef_id.0 > self.num_ev_flags {
                return at(format!(
                    "event flag {} past {}",
                    ch.ef_id.0, self.num_ev_flags
                ));
            }
            // snc's `#error`: CA has no integer wider than 32 bits, and
            // `long` is 64-bit on every target here.
            if matches!(ch.var_type, PrimType::Long | PrimType::ULong) {
                return at(format!(
                    "{} cannot be assigned to a PV",
                    ch.var_type.c_name()
                ));
            }
            let cell = (ch.var)(&vars);
            if cell.kind() != ch.var_type.elem_kind() {
                return at(format!(
                    "cell holds {:?}, table says {}",
                    cell.kind(),
                    ch.var_type.c_name()
                ));
            }
            if cell.count() != ch.count {
                return at(format!(
                    "cell holds {} elements, table says {}",
                    cell.count(),
                    ch.count
                ));
            }
            if ch.queue_size > 0 && ch.queue_index >= self.num_queues {
                return at(format!("queue {} past {}", ch.queue_index, self.num_queues));
            }
        }
        let words = nwords(self.num_ev_flags + self.chan.len() as u32);
        for ss in self.ss {
            if ss.states.is_empty() {
                return fail(format!("state set {} has no states", ss.ss_name));
            }
            for st in ss.states {
                if st.event_mask.len() != words {
                    return fail(format!(
                        "state {}.{}: event mask has {} words, not {words}",
                        ss.ss_name,
                        st.state_name,
                        st.event_mask.len()
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NOEVFLAG;
    use crate::var::{SeqVar, Zeroed};

    #[derive(Clone)]
    struct Vars {
        n: SeqVar<i32>,
        w: SeqVar<[f64; 4]>,
        l: SeqVar<i64>,
    }

    impl ProgramVars for Vars {
        fn zeroed() -> Self {
            Vars {
                n: Zeroed::zeroed(),
                w: Zeroed::zeroed(),
                l: Zeroed::zeroed(),
            }
        }
    }

    fn event(_: &StateSet<Vars>, _: &mut usize, _: &mut usize) -> bool {
        false
    }

    fn action(_: &StateSet<Vars>, _: usize, _: &mut usize) {}

    fn init(_: &Vars) {}

    fn state(mask: &'static [SeqMask]) -> SeqState<Vars> {
        SeqState {
            state_name: "s",
            action_func: action,
            event_func: event,
            entry_func: None,
            exit_func: None,
            event_mask: mask,
            options: StateOptions::new(0),
        }
    }

    fn n_chan(event_num: u32) -> SeqChan<Vars> {
        SeqChan {
            ch_name: Some("{P}n"),
            var: |v| &v.n,
            var_name: "n",
            var_type: PrimType::Int,
            count: 1,
            event_num,
            ef_id: NOEVFLAG,
            monitored: true,
            queue_size: 0,
            queue_index: 0,
        }
    }

    fn w_chan(event_num: u32) -> SeqChan<Vars> {
        SeqChan {
            var: |v| &v.w,
            var_name: "w",
            var_type: PrimType::Double,
            count: 4,
            ..n_chan(event_num)
        }
    }

    /// Two event flags and two channels: events 1..=4, one mask word.
    fn prog(chan: Vec<SeqChan<Vars>>, words: usize) -> SeqProgram<Vars> {
        let mask: &'static [SeqMask] = Box::leak(vec![0; words].into_boxed_slice());
        let states = Box::leak(Box::new([state(mask)]));
        let ss = Box::leak(Box::new([SeqSs {
            ss_name: "ss",
            states,
        }]));
        SeqProgram {
            magic: SEQ_MAGIC,
            prog_name: "t",
            chan: Box::leak(chan.into_boxed_slice()),
            ss,
            vars: VarModel::Reentrant,
            params: "",
            num_ev_flags: 2,
            options: ProgOptions::new(ProgOptions::CONN),
            init_func: init,
            entry_func: None,
            exit_func: None,
            num_queues: 0,
        }
    }

    fn valid() -> SeqProgram<Vars> {
        prog(vec![n_chan(3), w_chan(4)], 1)
    }

    #[test]
    fn a_consistent_table_passes() {
        assert_eq!(valid().validate(), Ok(()));
    }

    #[test]
    fn a_foreign_magic_is_refused() {
        let p = SeqProgram {
            magic: SEQ_MAGIC - 1,
            ..valid()
        };
        assert!(p.validate().unwrap_err().contains("magic"));
    }

    #[test]
    fn an_event_number_out_of_channel_order_is_refused() {
        let p = prog(vec![n_chan(3), w_chan(5)], 1);
        assert!(
            p.validate()
                .unwrap_err()
                .contains("event number 5 is not 4")
        );
    }

    #[test]
    fn the_last_event_flag_is_a_sync_target_and_the_next_is_not() {
        let synced = |ef| SeqChan {
            ef_id: EfId(ef),
            ..n_chan(3)
        };
        assert_eq!(prog(vec![synced(2)], 1).validate(), Ok(()));
        assert!(
            prog(vec![synced(3)], 1)
                .validate()
                .unwrap_err()
                .contains("event flag 3 past 2")
        );
    }

    #[test]
    fn a_long_channel_is_refused_as_snc_s_error() {
        let l: SeqChan<Vars> = SeqChan {
            var: |v| &v.l,
            var_name: "l",
            var_type: PrimType::Long,
            ..n_chan(3)
        };
        assert!(
            prog(vec![l], 1)
                .validate()
                .unwrap_err()
                .contains("long cannot be assigned to a PV")
        );
    }

    #[test]
    fn a_cell_of_another_type_is_refused() {
        let short = SeqChan {
            var_type: PrimType::Short,
            ..n_chan(3)
        };
        assert!(
            prog(vec![short], 1)
                .validate()
                .unwrap_err()
                .contains("table says short")
        );
    }

    #[test]
    fn a_cell_of_another_length_is_refused() {
        let w = SeqChan {
            count: 3,
            ..w_chan(3)
        };
        assert!(
            prog(vec![w], 1)
                .validate()
                .unwrap_err()
                .contains("4 elements, table says 3")
        );
    }

    /// Bit 0 is unused, so 31 events fit one word and 32 need two.
    #[test]
    fn the_mask_takes_a_second_word_at_event_32() {
        let chans = |n: u32| (0..n).map(|i| n_chan(3 + i)).collect::<Vec<_>>();
        assert_eq!(prog(chans(29), 1).validate(), Ok(()));
        assert!(prog(chans(30), 1).validate().unwrap_err().contains("not 2"));
        assert_eq!(prog(chans(30), 2).validate(), Ok(()));
    }

    #[test]
    fn a_queue_past_the_declared_ones_is_refused() {
        let q = SeqChan {
            queue_size: 5,
            queue_index: 0,
            ..n_chan(3)
        };
        assert!(
            prog(vec![q], 1)
                .validate()
                .unwrap_err()
                .contains("queue 0 past 0")
        );
    }

    #[test]
    #[should_panic(expected = "OPT_REENT and OPT_SAFE")]
    fn reent_is_not_an_option_bit() {
        ProgOptions::new(1 << 3);
    }

    #[test]
    fn opt_r_and_opt_s_follow_the_variable_model() {
        static BLOCK: std::sync::LazyLock<Vars> = std::sync::LazyLock::new(Vars::zeroed);
        let static_model: VarModel<Vars> = VarModel::Static(|| &BLOCK);
        assert!(!static_model.reentrant() && !static_model.safe());
        assert!(VarModel::<Vars>::Reentrant.reentrant());
        assert!(!VarModel::<Vars>::Reentrant.safe());
        assert!(VarModel::<Vars>::Safe.reentrant() && VarModel::<Vars>::Safe.safe());
    }
}
