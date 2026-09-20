//! The operand stack every `*Perform` evaluator runs on.
//!
//! C declares it in the evaluator's own frame — `double stack[CALCPERFORM_STACK+1]`
//! (`calcPerform.c:49`), `char stack[SCALC_STACKSIZE][40]` (`sCalcPerform.c`),
//! `double *stack[ACALC_STACKSIZE]` (`aCalcPerform.c`) — sized from the same
//! element-table ceiling the compiler refuses to exceed (`postfix.c:469`,
//! `runtime_depth >= stack_size`). The port had a `Vec` in each of the three, so
//! every `calc` / `scalcout` / `acalcout` process paid one malloc and one free
//! for a buffer whose size was settled when the expression compiled.
//!
//! The slots past `len` are uninitialised, as C's frame array is: filling them
//! with a placeholder cost the numeric evaluator a 640-byte store per process
//! for memory no accessor can reach.

use std::mem::MaybeUninit;

/// A fixed-capacity operand stack, `N` from the flavour's element table.
///
/// Invariant: `buf[..len]` is initialised and owned by the stack; `buf[len..]`
/// is never read or dropped. Every method that lowers `len` moves or drops the
/// slots it gives up, and `len` is lowered before those slots are touched so a
/// panicking destructor cannot leave them reachable.
pub(crate) struct Stack<T, const N: usize> {
    buf: [MaybeUninit<T>; N],
    len: usize,
    /// A push the array could not take. Recorded rather than returned, so the
    /// ~140 push sites across the three evaluators stay infallible statements
    /// as C's `*++ptop =` is; the evaluation fails at its tail instead, which
    /// is where C's own `ptop != stack + 1` test already lives.
    overflowed: bool,
}

impl<T, const N: usize> Stack<T, N> {
    pub(crate) fn new() -> Self {
        Stack {
            buf: [const { MaybeUninit::uninit() }; N],
            len: 0,
            overflowed: false,
        }
    }

    pub(crate) fn push(&mut self, v: T) {
        match self.buf.get_mut(self.len) {
            Some(slot) => {
                slot.write(v);
                self.len += 1;
            }
            None => self.overflowed = true,
        }
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        let len = self.len.checked_sub(1)?;
        self.len = len;
        // SAFETY: `len` was below the old length, so the slot is initialised;
        // it is above the new length, so nothing reads or drops it again.
        Some(unsafe { self.buf[len].assume_init_read() })
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        let old = self.len;
        let len = len.min(old);
        self.len = len;
        for slot in &mut self.buf[len..old] {
            // SAFETY: the slot was below the old length and is above the new
            // one, so this is its only drop.
            unsafe { slot.assume_init_drop() }
        }
    }

    /// The top `n` in stack order, removed. The returned `Vec` is C's
    /// `pop_args`-style scratch and allocates as it did before; what this type
    /// removes is the per-evaluation stack itself, not the per-operator
    /// argument lists.
    pub(crate) fn take_top(&mut self, n: usize) -> Vec<T> {
        let old = self.len;
        let base = old.saturating_sub(n);
        self.len = base;
        self.buf[base..old]
            .iter()
            // SAFETY: as `pop` — each slot was below the old length and is
            // above the new one, so the read is the slot's only move-out.
            .map(|slot| unsafe { slot.assume_init_read() })
            .collect()
    }

    /// The one value C's tail test — `ptop != stack + 1` (`calcPerform.c:419`)
    /// and its sCalc/aCalc twins — demands be left. `None` when the evaluation
    /// leaked or underflowed.
    pub(crate) fn into_only(mut self) -> Option<T> {
        if self.len == 1 { self.pop() } else { None }
    }

    /// Whether any push was dropped. Checked once, at the evaluator's tail,
    /// before the result is read.
    pub(crate) fn overflowed(&self) -> bool {
        self.overflowed
    }
}

impl<T, const N: usize> Drop for Stack<T, N> {
    fn drop(&mut self) {
        self.truncate(0);
    }
}

/// The live portion only — the slots past [`Stack::len`] are not part of the
/// stack, so `last`, `last_mut` and indexing all see what C's `ptop` bounds.
impl<T, const N: usize> std::ops::Deref for Stack<T, N> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: `buf[..len]` is initialised (type invariant) and
        // `MaybeUninit<T>` has `T`'s layout.
        unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<T>(), self.len) }
    }
}

impl<T, const N: usize> std::ops::DerefMut for Stack<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as `deref`, through the unique borrow.
        unsafe { std::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast::<T>(), self.len) }
    }
}
