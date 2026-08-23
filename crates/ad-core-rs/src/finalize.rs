//! Scope guard for strong state transitions.
//!
//! A function that sets a strong state marker (`is_open`, `Some(handle)`,
//! `capture_active`) or opens an external resource owes the matching cleanup on
//! *every* exit path. Written as a trailing assignment that cleanup is skipped
//! by the first `?` above it, and every fallible call added later re-opens the
//! hole. [`Finalize`] moves it onto a guard's `Drop`, where a normal return, an
//! early `return`, a `?` and an unwinding panic all reach it.

/// Borrows `target` for the length of a state transition and runs `finish`
/// against it when the guard leaves scope.
///
/// The transition body runs through [`Finalize::run`], which hands it a plain
/// `&mut T`, so field-level borrows inside the body behave exactly as they
/// would without the guard. [`Finalize::disarm`] cancels the finalizer for the
/// case where the body performed the cleanup itself in order to report its
/// error.
pub struct Finalize<'a, T: ?Sized, F: FnOnce(&mut T)> {
    target: &'a mut T,
    finish: Option<F>,
}

impl<'a, T: ?Sized, F: FnOnce(&mut T)> Finalize<'a, T, F> {
    /// Arm the finalizer over `target`.
    pub fn new(target: &'a mut T, finish: F) -> Self {
        Self {
            target,
            finish: Some(finish),
        }
    }

    /// Run one step of the transition against the guarded target. A `?` applied
    /// to the returned value leaves the enclosing function through the guard's
    /// `Drop`, so the finalizer still runs.
    pub fn run<R>(&mut self, body: impl FnOnce(&mut T) -> R) -> R {
        body(self.target)
    }

    /// Cancel the finalizer because the caller already performed the cleanup —
    /// the fallible form, whose error it wants to propagate — and it must not
    /// run a second time.
    pub fn disarm(mut self) {
        self.finish = None;
    }
}

impl<T: ?Sized, F: FnOnce(&mut T)> Drop for Finalize<'_, T, F> {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish(self.target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Finalize;

    fn early_return(flag: &mut bool) -> Result<(), &'static str> {
        let mut guard = Finalize::new(flag, |f: &mut bool| *f = false);
        guard.run(|_| Err("boom"))
    }

    #[test]
    fn finalizer_runs_on_an_early_error_exit() {
        let mut flag = true;
        assert!(early_return(&mut flag).is_err());
        assert!(!flag, "the `?` exit must still reach the finalizer");
    }

    #[test]
    fn disarm_suppresses_the_finalizer() {
        let mut flag = true;
        {
            let guard = Finalize::new(&mut flag, |f: &mut bool| *f = false);
            guard.disarm();
        }
        assert!(flag);
    }
}
