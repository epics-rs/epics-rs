//! The program instance list (`seq_prog.c`), kept in the registry of
//! [`traverse_sequencer_programs`](crate::cmd::traverse_sequencer_programs)
//! and walked under its lock.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::cmd::traverse_sequencer_programs;
use crate::pvt::{ProgInstance, Program};
use crate::seq::SeqThreadId;
use crate::var::ProgramVars;

/// `seqAddProg`: append `prog` to its program's instances, numbering it
/// one past the highest instance there ([`Program::instance`]).
pub fn seq_add_prog<V: ProgramVars>(prog: &Arc<Program<V>>) {
    traverse_sequencer_programs(|reg, instances| {
        if reg.prog_name() != prog.table.prog_name {
            return false;
        }
        let instance = instances.iter().map(|p| p.instance() + 1).max();
        prog.instance
            .store(instance.unwrap_or(0), Ordering::Relaxed);
        instances.push(Arc::clone(prog) as Arc<dyn ProgInstance>);
        true
    });
}

/// `seqDelProg`: remove `prog` (by address) from its program's instances.
pub fn seq_del_prog(prog: &dyn ProgInstance) {
    traverse_sequencer_programs(|reg, instances| {
        if reg.prog_name() != prog.prog_name() {
            return false;
        }
        let Some(at) = instances
            .iter()
            .position(|p| std::ptr::addr_eq(Arc::as_ptr(p), prog))
        else {
            return false;
        };
        instances.remove(at);
        true
    });
}

/// `seqFindStateSet`: the instance with a state set running on `tid`,
/// and that state set's index.
pub fn seq_find_state_set(tid: SeqThreadId) -> Option<(Arc<dyn ProgInstance>, usize)> {
    let mut found = None;
    seq_traverse_prog(|p| {
        found = (0..p.num_ss())
            .find(|&ss| p.thread_id(ss) == Some(tid))
            .map(|ss| (Arc::clone(p), ss));
        found.is_some()
    });
    found
}

/// `seqFindProg`.
pub fn seq_find_prog(tid: SeqThreadId) -> Option<Arc<dyn ProgInstance>> {
    seq_find_state_set(tid).map(|(p, _)| p)
}

/// `seqTraverseProg`: `f` on every instance, newest registered program
/// first and each program's instances in start order, until `f` returns
/// true.
pub fn seq_traverse_prog(mut f: impl FnMut(&Arc<dyn ProgInstance>) -> bool) {
    traverse_sequencer_programs(|_, instances| instances.iter().any(&mut f));
}
