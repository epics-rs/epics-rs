use super::error::CalcError;

/// The ARRAY half of C's `stackElement` (`aCalcPerform.c:74-80`):
///
/// ```c
/// typedef struct { double d; double *a; double *array; int firstEl; int numEl; ... } stackElement;
/// ```
///
/// An array stack cell is **a fixed `arraySize` buffer PLUS an active window** —
/// not a bare vector. The distinction is the whole point: every element-wise
/// operator in aCalcPerform writes the buffer (`for (i=0; i<arraySize; i++)`),
/// while every reduction folds only the window. A port that keeps just the
/// vector answers the reduction over the zero fill — `AMIN(AA[1,3])` comes out 0
/// instead of 20.
///
/// # The window
///
/// C derives it in `calcFirstLast` (`:289-295`):
///
/// ```c
/// if (ps->numEl != -1) { *firstEl = ps->firstEl; *lastEl = ps->firstEl + ps->numEl - 1; }
/// else                 { *firstEl = 0;           *lastEl = arraySize-1; }
/// ```
///
/// `firstEl` is **provably always 0**: the only assignments to it in the file are
/// `ps->firstEl = 0` in SUBRANGE and SUBRANGE_IP (`:1537`, `:1543`), and the
/// stack is zero-initialised. The window is therefore always a PREFIX of the
/// buffer, `num_el` alone carries it, and `last_el()` is all the operators need.
/// Modelling `firstEl` as well would be modelling a value that cannot vary.
///
/// `numEl == -1` is C's "no window" SENTINEL, and it is a sentinel rather than a
/// count: SUBRANGE stores `1+j-i`, which is -1 for the inverted range `AA[3,1]` —
/// so that range reads back as the full buffer, while `AA[2,1]` (count 0) is a
/// genuinely empty window whose AVERAGE divides by zero and returns NaN. Both are
/// compiled-C observables, so the sentinel is spelled `None` and the count stays
/// signed.
///
/// Only SUBRANGE, SUBRANGE_IP and CAT (`:1520-1548`, `:1384-1392`) set a window;
/// `to_array` clears it (`:133`) and so does every fresh push (C's `INC`, `:108`).
#[derive(Debug, Clone, PartialEq)]
pub struct ArrayCell {
    /// C's `ps->array`/`ps->a` — exactly `array_size` doubles.
    buf: Vec<f64>,
    /// C's `ps->numEl`, with its `-1` sentinel spelled as `None`.
    num_el: Option<i64>,
}

impl ArrayCell {
    /// Build a cell for an engine whose element count is `array_size`. This is
    /// the constructor that establishes the invariant every operator relies on —
    /// **`buf.len() == array_size`** — which is why it resizes rather than
    /// trusting the caller: C hands every stack cell an `arraySize` buffer from
    /// its freeList and copies at most `arraySize` elements into it
    /// (`FETCH_ARG`, `:169-175`), zero-filling nothing because the buffer is
    /// already that long.
    ///
    /// With the length equal on every array cell, the binary operators cannot see
    /// mismatched operands, which is why there is no length-mismatch error to
    /// raise.
    pub fn new(mut buf: Vec<f64>, array_size: usize) -> Self {
        buf.resize(array_size, 0.0);
        ArrayCell { buf, num_el: None }
    }

    /// C `to_array(ps, setValues=1)` (`:124-143`) — the only way a scalar becomes
    /// an array, and it does not merely repeat the scalar: **a NaN scalar fills
    /// the buffer with 0** (`:135-138`). It also clears the window (`:130`).
    pub fn from_scalar(v: f64, array_size: usize) -> Self {
        let fill = if v.is_nan() { 0.0 } else { v };
        ArrayCell {
            buf: vec![fill; array_size],
            num_el: None,
        }
    }

    /// The whole `arraySize` buffer — what the element-wise operators write
    /// (`:771-834`) and what the record copies out into AVAL (`:1630-1637`).
    pub fn buf(&self) -> &[f64] {
        &self.buf
    }

    pub fn buf_mut(&mut self) -> &mut [f64] {
        &mut self.buf
    }

    pub fn into_buf(self) -> Vec<f64> {
        self.buf
    }

    /// C's `lastEl`. Signed, because an empty (`numEl == 0`) window puts it below
    /// `firstEl` and C's reduction loops then run zero times.
    pub fn last_el(&self) -> i64 {
        match self.num_el {
            Some(n) => n - 1,
            None => self.buf.len() as i64 - 1,
        }
    }

    /// C's `1 + lastEl - firstEl` — the divisor AVERAGE and STD_DEV use (`:929`,
    /// `:936`). Deliberately not `window().len()`: it is signed, and C really does
    /// divide by zero on an empty window (compiled C: `AVG(AA[2,1])` is NaN with
    /// status -1).
    pub fn span(&self) -> i64 {
        1 + self.last_el()
    }

    /// The active elements — what every REDUCTION folds over (AMAX, AMIN, IXMAX,
    /// IXMIN, IXZ, IXNZ, AVERAGE, STD_DEV, FWHM, ARRSUM, SMOOTH, DERIV and the FIT
    /// family all open with `calcFirstLast`). Empty when the window is empty.
    pub fn window(&self) -> &[f64] {
        &self.buf[..self.window_len()]
    }

    pub fn window_mut(&mut self) -> &mut [f64] {
        let n = self.window_len();
        &mut self.buf[..n]
    }

    fn window_len(&self) -> usize {
        self.span().clamp(0, self.buf.len() as i64) as usize
    }

    /// C's DERIV/NDERIV/NSMOOTH tail (`:985-987`, `:614-616`, `:590-591`): after
    /// writing the result into the window, zero everything OUTSIDE it. (`firstEl`
    /// is 0, so only the tail past `lastEl` can be non-empty.) SMOOTH and the
    /// element-wise operators do NOT do this — that asymmetry is C's, and it is
    /// why this is an explicit call and not folded into `window_mut`.
    pub fn clear_outside_window(&mut self) {
        let n = self.window_len();
        self.buf[n..].fill(0.0);
    }

    /// Set C's `numEl`. The single owner of the two encodings that mean "the whole
    /// buffer", so no caller can leave a second one lying around:
    ///
    /// * `-1` is C's sentinel (and `SUBRANGE` really does produce it, for an
    ///   inverted range like `AA[3,1]`);
    /// * `arraySize` describes exactly the same window through `calcFirstLast`, and
    ///   C's array/array CAT assigns it unconditionally (`:1364`).
    ///
    /// Both collapse to `None` here. Keeping them distinct would give one window two
    /// representations, which is the dual meaning this type exists to remove.
    pub fn set_num_el(&mut self, n: i64) {
        self.num_el = if n == -1 || n == self.buf.len() as i64 {
            None
        } else {
            Some(n)
        };
    }

    /// Element-wise map over the WHOLE buffer, preserving the window: C's
    /// element-wise operators write `ps->a[i]` in place and never touch
    /// `ps->numEl`.
    pub fn map(mut self, f: impl Fn(f64) -> f64) -> Self {
        for v in &mut self.buf {
            *v = f(*v);
        }
        self
    }
}

impl From<Vec<f64>> for ArrayCell {
    /// A windowless cell over exactly this buffer, for callers that already hold
    /// an `array_size`-long one. [`ArrayCell::new`] is what enforces the length.
    fn from(buf: Vec<f64>) -> Self {
        ArrayCell { buf, num_el: None }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrayStackValue {
    Double(f64),
    Array(ArrayCell),
}

impl ArrayStackValue {
    /// An array cell over `buf` as given (no window). Convenience for the many
    /// engine sites that build a fresh `array_size`-long buffer.
    pub fn array(buf: Vec<f64>) -> Self {
        ArrayStackValue::Array(ArrayCell::from(buf))
    }

    pub fn is_double(&self) -> bool {
        matches!(self, ArrayStackValue::Double(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, ArrayStackValue::Array(_))
    }

    /// C `to_double(ps)` (`:121`): `ps->d = ps->a[0]`. Element 0 of the buffer;
    /// `firstEl` is always 0, so that is also element 0 of the window.
    pub fn as_f64(&self) -> Result<f64, CalcError> {
        match self {
            ArrayStackValue::Double(v) => Ok(*v),
            ArrayStackValue::Array(a) => Ok(a.buf().first().copied().unwrap_or(0.0)),
        }
    }

    pub fn as_cell(&self) -> Result<&ArrayCell, CalcError> {
        match self {
            ArrayStackValue::Array(a) => Ok(a),
            ArrayStackValue::Double(_) => Err(CalcError::TypeMismatch),
        }
    }

    /// C `toArray(ps, setValues)` — promote a scalar, pass an array through. The
    /// NaN->0 fill lives in [`ArrayCell::from_scalar`], so no promotion path can
    /// bypass it.
    pub fn into_cell(self, array_size: usize) -> ArrayCell {
        match self {
            ArrayStackValue::Double(v) => ArrayCell::from_scalar(v, array_size),
            ArrayStackValue::Array(a) => a,
        }
    }

    /// The record boundary: C promotes a scalar result with `toArray(ps,1)`
    /// (`:1624`) and copies the whole buffer into `p_aresult` (`:1630-1637`). The
    /// window does not leave the engine.
    pub fn to_array(self, array_size: usize) -> Vec<f64> {
        self.into_cell(array_size).into_buf()
    }

    /// Every element this operand contributes to an all-element reduction — the
    /// whole BUFFER, or the single value of a scalar. This is the shape C's
    /// VARARG predicates fold over (`aCalcPerform.c:1114-1146`: `if (isDouble(ps))
    /// j = f(ps->d); else for (i=0; i<arraySize; i++) j = j OP f(ps->a[i]);`) —
    /// they read `arraySize`, not the window — and it is deliberately NOT
    /// `as_f64`, which collapses an array to its `a[0]`.
    pub fn elements(&self) -> impl Iterator<Item = f64> + '_ {
        let (scalar, arr) = match self {
            ArrayStackValue::Double(v) => (Some(*v), [].as_slice()),
            ArrayStackValue::Array(a) => (None, a.buf()),
        };
        scalar.into_iter().chain(arr.iter().copied())
    }

    pub fn map<F: Fn(f64) -> f64>(self, f: F) -> ArrayStackValue {
        match self {
            ArrayStackValue::Double(v) => ArrayStackValue::Double(f(v)),
            ArrayStackValue::Array(a) => ArrayStackValue::Array(a.map(f)),
        }
    }
}

/// The two-operand shape shared by aCalc's arithmetic, comparison, logical and
/// bitwise operators (`aCalcPerform.c:625-706`, `:1338-1404`).
///
/// C's binary arms promote the LEFT operand only — `toArray(ps,1)` (`:630`,
/// `:1338`) — and then read the right one as the plain double `ps1->d`
/// (`:659-684`). The result IS the left cell, so it keeps the left operand's
/// window. The two mixed shapes are therefore NOT mirror images when the scalar
/// is NaN:
///
///   * `array OP NaN` — the NaN goes into every element (no promotion)
///   * `NaN OP array` — the NaN is promoted, and `to_array` turns it into 0
pub fn zip_map<F: Fn(f64, f64) -> f64>(
    a: ArrayStackValue,
    b: ArrayStackValue,
    f: F,
) -> ArrayStackValue {
    match (a, b) {
        (ArrayStackValue::Double(x), ArrayStackValue::Double(y)) => {
            ArrayStackValue::Double(f(x, y))
        }
        (ArrayStackValue::Array(cell), ArrayStackValue::Double(scalar)) => {
            ArrayStackValue::Array(cell.map(|x| f(x, scalar)))
        }
        (left, ArrayStackValue::Array(right)) => {
            // Both buffers are `array_size` long by ArrayCell's invariant, so the
            // right operand's length is the promotion size for a scalar left.
            let mut cell = left.into_cell(right.buf().len());
            for (x, y) in cell.buf_mut().iter_mut().zip(right.buf()) {
                *x = f(*x, *y);
            }
            ArrayStackValue::Array(cell)
        }
    }
}
