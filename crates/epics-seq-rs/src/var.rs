//! Program variables: what snc-rs declares in `struct seqg_vars`.
//!
//! Each variable is one [`SeqVar`], a lock of its own around a heap value,
//! so a state set and a pv callback touch it without a data race and with
//! C's non-safe visibility: a write is seen by the next read on any
//! thread. Locks are per variable, never nested: snc-rs evaluates every
//! operand of an assignment before it takes the target's lock (`x = x + 1`
//! is a `get` and then a `set`, `x += e` evaluates `e` first), so no
//! [`Place::modify`] closure ever reaches another variable.
//!
//! A multi-PV array (`assign x to {"a", "b"}`) splits its outer dimension
//! into separate cells, `[SeqVar<T>; N]`, so each channel owns one cell. A
//! [`Ptr`] into it, which C sees as one array, is the one thing that locks
//! its cells together, in index order.

use std::any::Any;
use std::ffi::{c_char, c_void};
use std::marker::PhantomData;
use std::sync::Arc;

use epics_base_rs::types::{EpicsValue, PvString};
use parking_lot::Mutex;

use crate::types::{PvSevr, PvStat, SeqString};

/// The value a variable holds before `seqg_init` runs: C's `calloc` (`+r`)
/// or static zero (`-r`).
pub trait Zeroed: Sized {
    fn zeroed() -> Self;

    /// The zero value built where it will stay, so a `double x[65536]` is
    /// never a stack temporary.
    fn zeroed_box() -> Box<Self> {
        Box::new(Self::zeroed())
    }
}

macro_rules! zeroed_scalar {
    ($($t:ty = $z:expr),* $(,)?) => {
        $(impl Zeroed for $t {
            fn zeroed() -> Self {
                $z
            }
        })*
    };
}

zeroed_scalar!(
    i8 = 0,
    u8 = 0,
    i16 = 0,
    u16 = 0,
    i32 = 0,
    u32 = 0,
    i64 = 0,
    u64 = 0,
    f32 = 0.0,
    f64 = 0.0,
    SeqString = SeqString::new(),
    PvStat = PvStat::Ok,
    PvSevr = PvSevr::None,
);

impl<T: Zeroed, const N: usize> Zeroed for [T; N] {
    fn zeroed() -> Self {
        std::array::from_fn(|_| T::zeroed())
    }

    fn zeroed_box() -> Box<Self> {
        let elems: Box<[T]> = (0..N).map(|_| T::zeroed()).collect();
        match elems.try_into() {
            Ok(b) => b,
            Err(_) => unreachable!("the iterator yields exactly N elements"),
        }
    }
}

impl<T: Zeroed> Zeroed for SeqVar<T> {
    fn zeroed() -> Self {
        SeqVar(Arc::new(Mutex::new(T::zeroed_box())))
    }
}

/// The element a channel's cell transfers, which [`crate::SeqProgram::validate`]
/// matches against the table's [`PrimType`](crate::PrimType).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ElemKind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Str,
}

/// The kind of the host C's `char`.
pub const C_CHAR_KIND: ElemKind = if (c_char::MIN as i32) < 0 {
    ElemKind::I8
} else {
    ElemKind::U8
};

/// A type a channel can transfer element by element.
pub trait Prim: Copy + Zeroed + Send + Sync + 'static {
    const KIND: ElemKind;

    /// Element `i` of `value` (a scalar has only element 0), converted as
    /// a C assignment converts it: an integer of the same width keeps its
    /// bits, as C's `memcpy` from the buffer does. `None` past the end, or
    /// between a string and a number, which a request of the channel's
    /// own type never delivers.
    fn from_value(value: &EpicsValue, i: usize) -> Option<Self>;

    /// `elems` as one value of this type, losslessly: a scalar variant for
    /// one element, an array otherwise. Narrowing to the request type is
    /// the pv layer's.
    fn to_value(elems: &[Self]) -> EpicsValue;
}

/// A numeric element of an [`EpicsValue`], before the cast to the
/// variable's type.
enum Num {
    I(i64),
    U(u64),
    F(f64),
}

/// `Char` is `epicsInt8` and `UChar` `epicsUInt8`, as base defines them.
fn num_at(value: &EpicsValue, i: usize) -> Option<Num> {
    use EpicsValue::*;
    let scalar = |n| (i == 0).then_some(n);
    match value {
        Char(v) => scalar(Num::I(*v as i8 as i64)),
        UChar(v) => scalar(Num::U(*v as u64)),
        Short(v) => scalar(Num::I(*v as i64)),
        UShort(v) | Enum(v) | EnumWithChoices { index: v, .. } => scalar(Num::U(*v as u64)),
        Long(v) => scalar(Num::I(*v as i64)),
        ULong(v) => scalar(Num::U(*v as u64)),
        Int64(v) => scalar(Num::I(*v)),
        UInt64(v) => scalar(Num::U(*v)),
        Float(v) => scalar(Num::F(*v as f64)),
        Double(v) => scalar(Num::F(*v)),
        CharArray(a) => a.get(i).map(|v| Num::I(*v as i8 as i64)),
        UCharArray(a) => a.get(i).map(|v| Num::U(*v as u64)),
        ShortArray(a) => a.get(i).map(|v| Num::I(*v as i64)),
        UShortArray(a) | EnumArray(a) => a.get(i).map(|v| Num::U(*v as u64)),
        LongArray(a) => a.get(i).map(|v| Num::I(*v as i64)),
        ULongArray(a) => a.get(i).map(|v| Num::U(*v as u64)),
        Int64Array(a) => a.get(i).map(|v| Num::I(*v)),
        UInt64Array(a) => a.get(i).map(|v| Num::U(*v)),
        FloatArray(a) => a.get(i).map(|v| Num::F(*v as f64)),
        DoubleArray(a) => a.get(i).map(|v| Num::F(*v)),
        String(_) | StringArray(_) => None,
    }
}

macro_rules! prim_num {
    ($($t:ty => $k:ident, $scalar:ident, $array:ident;)*) => {
        $(impl Prim for $t {
            const KIND: ElemKind = ElemKind::$k;

            fn from_value(value: &EpicsValue, i: usize) -> Option<Self> {
                Some(match num_at(value, i)? {
                    Num::I(v) => v as $t,
                    Num::U(v) => v as $t,
                    Num::F(v) => v as $t,
                })
            }

            fn to_value(elems: &[Self]) -> EpicsValue {
                match elems {
                    [v] => EpicsValue::$scalar(*v as _),
                    _ => EpicsValue::$array(elems.iter().map(|&v| v as _).collect()),
                }
            }
        })*
    };
}

prim_num!(
    i8 => I8, Char, CharArray;
    u8 => U8, UChar, UCharArray;
    i16 => I16, Short, ShortArray;
    u16 => U16, UShort, UShortArray;
    i32 => I32, Long, LongArray;
    u32 => U32, ULong, ULongArray;
    i64 => I64, Int64, Int64Array;
    u64 => U64, UInt64, UInt64Array;
    f32 => F32, Float, FloatArray;
    f64 => F64, Double, DoubleArray;
);

impl Prim for SeqString {
    const KIND: ElemKind = ElemKind::Str;

    fn from_value(value: &EpicsValue, i: usize) -> Option<Self> {
        let bytes = match value {
            EpicsValue::String(s) if i == 0 => s.as_bytes(),
            EpicsValue::StringArray(a) => a.get(i)?.as_bytes(),
            _ => return None,
        };
        let mut s = SeqString::new();
        s.set(bytes);
        Some(s)
    }

    fn to_value(elems: &[Self]) -> EpicsValue {
        let pv = |s: &SeqString| PvString::from_bytes(s.as_bytes());
        match elems {
            [s] => EpicsValue::String(pv(s)),
            _ => EpicsValue::StringArray(elems.iter().map(pv).collect()),
        }
    }
}

/// Something read and written as a `T`: a variable, or an element of one.
/// Generated code calls the same-named inherent methods of [`SeqVar`] and
/// [`At`], so it imports no trait; C library shims that write through a
/// pointer (`strcpy`, `sprintf`) take a `&impl Place<_>`.
pub trait Place<T> {
    /// Run `f` on the value under the variable's lock. `f` must not touch
    /// another program variable.
    fn modify<R>(&self, f: impl FnOnce(&mut T) -> R) -> R;

    fn get(&self) -> T
    where
        T: Clone,
    {
        self.modify(|v| v.clone())
    }

    fn set(&self, value: T) {
        self.modify(|v| *v = value)
    }
}

/// One program variable. Its cell is shared, so a pointer to it can hold
/// it.
pub struct SeqVar<T>(Arc<Mutex<Box<T>>>);

impl<T> SeqVar<T> {
    pub fn new(value: T) -> Self {
        SeqVar(Arc::new(Mutex::new(Box::new(value))))
    }

    /// [`Place::modify`].
    pub fn modify<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.0.lock())
    }

    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.modify(|v| v.clone())
    }

    pub fn set(&self, value: T) {
        self.modify(|v| *v = value)
    }
}

impl<A: Indexed> SeqVar<A> {
    /// C's `a[index]`.
    pub fn at(&self, index: usize) -> At<'_, Self, A> {
        At::new(self, index)
    }
}

impl<T: Send + 'static> SeqVar<T> {
    /// C's `&x`.
    pub fn ptr(&self) -> Ptr<T> {
        let home = home(&self.0);
        Ptr::along(Arc::new(Cell(self.0.clone(), home)), 0)
    }
}

impl<T> Place<T> for SeqVar<T> {
    fn modify<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        SeqVar::modify(self, f)
    }
}

impl<T: Send + 'static> Addr<T> for SeqVar<T> {
    fn ptr(&self) -> Ptr<T> {
        SeqVar::ptr(self)
    }
}

/// A new cell, as safe mode's per-state-set copy is a new block: a
/// pointer stored in the variable still points into the original, as C's
/// `memcpy` leaves it.
impl<T: Clone> Clone for SeqVar<T> {
    fn clone(&self) -> Self {
        SeqVar(Arc::new(Mutex::new(self.0.lock().clone())))
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for SeqVar<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.lock().fmt(f)
    }
}

/// What C subscripts and points along: an array, or a `string` as its
/// `char` buffer. Its elements start at its own address, as a C array's
/// do, which is where a [`Ptr`] counts them from.
pub trait Indexed {
    type Elem;

    fn elems(&self) -> &[Self::Elem];

    fn elems_mut(&mut self) -> &mut [Self::Elem];

    /// Run `f` on element `index`; an index past the end panics.
    fn with_elem<R>(&mut self, index: usize, f: impl FnOnce(&mut Self::Elem) -> R) -> R {
        f(&mut self.elems_mut()[index])
    }
}

impl<T, const N: usize> Indexed for [T; N] {
    type Elem = T;

    fn elems(&self) -> &[T] {
        self
    }

    fn elems_mut(&mut self) -> &mut [T] {
        self
    }
}

impl Indexed for SeqString {
    type Elem = c_char;

    fn elems(&self) -> &[c_char] {
        let b = self.as_buffer();
        // SAFETY: `c_char` is `i8` or `u8`, the size and alignment of `u8`,
        // and every bit pattern is valid as either.
        unsafe { std::slice::from_raw_parts(b.as_ptr().cast(), b.len()) }
    }

    fn elems_mut(&mut self) -> &mut [c_char] {
        let b = self.as_buffer_mut();
        // SAFETY: as in `elems`; the slice borrows `self` mutably.
        unsafe { std::slice::from_raw_parts_mut(b.as_mut_ptr().cast(), b.len()) }
    }
}

/// Element `index` of what `parent` holds: C's `a[i]` or `s[i]`, and
/// `a[i][j]` is `a.at(i).at(j)`. An index past the end panics where C
/// would read or write outside the variable.
pub struct At<'p, P: ?Sized, A> {
    parent: &'p P,
    index: usize,
    _array: PhantomData<fn() -> A>,
}

impl<'p, P: ?Sized, A> At<'p, P, A> {
    fn new(parent: &'p P, index: usize) -> Self {
        At {
            parent,
            index,
            _array: PhantomData,
        }
    }
}

impl<A: Indexed, P: Place<A> + ?Sized> At<'_, P, A> {
    /// [`Place::modify`].
    pub fn modify<R>(&self, f: impl FnOnce(&mut A::Elem) -> R) -> R {
        self.parent.modify(|a| a.with_elem(self.index, f))
    }

    pub fn get(&self) -> A::Elem
    where
        A::Elem: Clone,
    {
        self.modify(|v| v.clone())
    }

    pub fn set(&self, value: A::Elem) {
        self.modify(|v| *v = value)
    }
}

impl<A: Indexed<Elem: Indexed>, P: Place<A> + ?Sized> At<'_, P, A> {
    pub fn at(&self, index: usize) -> At<'_, Self, A::Elem> {
        At::new(self, index)
    }
}

impl<A: Indexed<Elem: 'static> + 'static, P: Addr<A> + ?Sized> At<'_, P, A> {
    /// C's `&a[i]`, and `a + i`: a pointer that moves along `a`.
    pub fn ptr(&self) -> Ptr<A::Elem> {
        Ptr::along(Arc::new(Elems(self.parent.ptr())), self.index as isize)
    }
}

impl<A: Indexed, P: Place<A> + ?Sized> Place<A::Elem> for At<'_, P, A> {
    fn modify<R>(&self, f: impl FnOnce(&mut A::Elem) -> R) -> R {
        At::modify(self, f)
    }
}

impl<A: Indexed<Elem: 'static> + 'static, P: Addr<A> + ?Sized> Addr<A::Elem> for At<'_, P, A> {
    fn ptr(&self) -> Ptr<A::Elem> {
        At::ptr(self)
    }
}

/// Member of the struct `parent` holds, C's `s.a` and `p->a`, reached by
/// a reference into it: `&s.a` points into `s` and sees its later writes.
pub struct Field<'p, P: ?Sized, S, F> {
    parent: &'p P,
    get: fn(&mut S) -> &mut F,
}

impl<P: Place<S> + ?Sized, S, F> Field<'_, P, S, F> {
    /// [`Place::modify`], under the lock of the variable holding `parent`.
    pub fn modify<R>(&self, f: impl FnOnce(&mut F) -> R) -> R {
        self.parent.modify(|s| f((self.get)(s)))
    }

    pub fn get(&self) -> F
    where
        F: Clone,
    {
        self.modify(|v| v.clone())
    }

    pub fn set(&self, value: F) {
        self.modify(|v| *v = value)
    }
}

impl<P: Place<S> + ?Sized, S, A: Indexed> Field<'_, P, S, A> {
    /// C's `s.a[index]`.
    pub fn at(&self, index: usize) -> At<'_, Self, A> {
        At::new(self, index)
    }
}

impl<P: Addr<S> + ?Sized, S: 'static, F: 'static> Field<'_, P, S, F> {
    /// C's `&s.a`.
    pub fn ptr(&self) -> Ptr<F> {
        let run = Member {
            parent: self.parent.ptr(),
            get: self.get,
        };
        Ptr::along(Arc::new(run), 0)
    }
}

impl<P: Place<S> + ?Sized, S, F> Place<F> for Field<'_, P, S, F> {
    fn modify<R>(&self, f: impl FnOnce(&mut F) -> R) -> R {
        Field::modify(self, f)
    }
}

impl<P: Addr<S> + ?Sized, S: 'static, F: 'static> Addr<F> for Field<'_, P, S, F> {
    fn ptr(&self) -> Ptr<F> {
        Field::ptr(self)
    }
}

/// Member of the union `parent` holds, C's `u.a` and `p->a`. A union's
/// members share its bytes, so a member is a copy the union's accessors
/// convert, read and written back under the parent's lock; it has no
/// address of its own.
pub struct View<'p, P: ?Sized, U, F> {
    parent: &'p P,
    get: fn(&U) -> F,
    set: fn(&mut U, F),
}

impl<P: Place<U> + ?Sized, U, F> View<'_, P, U, F> {
    /// [`Place::modify`].
    pub fn modify<R>(&self, f: impl FnOnce(&mut F) -> R) -> R {
        self.parent.modify(|u| {
            let mut v = (self.get)(u);
            let r = f(&mut v);
            (self.set)(u, v);
            r
        })
    }

    pub fn get(&self) -> F {
        self.parent.modify(|u| (self.get)(u))
    }

    pub fn set(&self, value: F) {
        self.parent.modify(|u| (self.set)(u, value))
    }
}

impl<P: Place<U> + ?Sized, U, F> Place<F> for View<'_, P, U, F> {
    fn modify<R>(&self, f: impl FnOnce(&mut F) -> R) -> R {
        View::modify(self, f)
    }
}

/// C's `.` on the places that can hold a struct or a union: `field` for a
/// struct member, `view` for a union member, each given as a closure
/// generated code writes (`|v| &mut v.a`, or `|v| v.a()` and
/// `|v, x| v.set_a(x)`).
macro_rules! members {
    ($([$($g:tt)*] $place:ty => $s:ty;)*) => {$(
        impl<$($g)*> $place {
            pub fn field<F>(&self, get: fn(&mut $s) -> &mut F) -> Field<'_, Self, $s, F> {
                Field { parent: self, get }
            }

            pub fn view<F>(&self, get: fn(&$s) -> F, set: fn(&mut $s, F)) -> View<'_, Self, $s, F> {
                View {
                    parent: self,
                    get,
                    set,
                }
            }
        }
    )*};
}

members! {
    [T] SeqVar<T> => T;
    [T] Ptr<T> => T;
    [A: Indexed, P: Place<A> + ?Sized] At<'_, P, A> => A::Elem;
    [P: Place<S> + ?Sized, S, M] Field<'_, P, S, M> => M;
}

/// A C union's storage, which a companion's union type wraps: every member
/// is read and written through the same native-endian bytes, as C's are,
/// so a value written through one member reads back through another.
#[derive(Clone)]
pub struct UnionBytes<const N: usize> {
    bytes: [u8; N],
    /// the pointer member last written, whose address the bytes hold
    ptr: Ptr<c_void>,
}

impl<const N: usize> UnionBytes<N> {
    pub fn get<T: UnionMember>(&self) -> T {
        T::load(&self.bytes, &self.ptr)
    }

    pub fn set<T: UnionMember>(&mut self, value: T) {
        value.store(&mut self.bytes, &mut self.ptr)
    }
}

impl<const N: usize> Zeroed for UnionBytes<N> {
    fn zeroed() -> Self {
        UnionBytes {
            bytes: [0; N],
            ptr: Ptr(None),
        }
    }
}

/// A member [`UnionBytes`] holds: a number as its bytes, a pointer as its
/// address.
pub trait UnionMember: Sized {
    fn load(bytes: &[u8], ptr: &Ptr<c_void>) -> Self;
    fn store(self, bytes: &mut [u8], ptr: &mut Ptr<c_void>);
}

macro_rules! union_num {
    ($($t:ty),*) => {$(
        impl UnionMember for $t {
            fn load(bytes: &[u8], _: &Ptr<c_void>) -> Self {
                let n = std::mem::size_of::<$t>();
                <$t>::from_ne_bytes(bytes[..n].try_into().expect("n bytes"))
            }

            fn store(self, bytes: &mut [u8], _: &mut Ptr<c_void>) {
                let n = std::mem::size_of::<$t>();
                bytes[..n].copy_from_slice(&self.to_ne_bytes())
            }
        }
    )*};
}

union_num!(i8, u8, i16, u16, i32, u32, i64, u64, isize, usize, f32, f64);

/// The address in the bytes reads back as the pointer stored there; where
/// another member has overwritten it, C's pointer points nowhere it can
/// name, so the read panics.
impl<T: 'static> UnionMember for Ptr<T> {
    fn load(bytes: &[u8], ptr: &Ptr<c_void>) -> Self {
        match usize::load(bytes, ptr) {
            0 => Ptr(None),
            addr => {
                assert_eq!(
                    addr,
                    ptr.addr(),
                    "union pointer member read over another member"
                );
                ptr.cast()
            }
        }
    }

    fn store(self, bytes: &mut [u8], ptr: &mut Ptr<c_void>) {
        self.addr().store(bytes, ptr);
        *ptr = self.erase();
    }
}

/// A [`Place`] C can take the address of.
pub trait Addr<T>: Place<T> {
    /// C's `&place`.
    fn ptr(&self) -> Ptr<T>;
}

/// What a [`Ptr`] moves along: the run of `T`s it points into.
trait Run<T>: Send + Sync {
    fn read(&self, f: &mut dyn FnMut(&[T]));
    fn write(&self, f: &mut dyn FnMut(&mut [T]));

    /// The typed pointer a `void *` was made from.
    fn erased(&self) -> Option<&(dyn Any + Send + Sync)> {
        None
    }

    /// The address of element `index` where it is stored, never in a copy
    /// `read` hands out, so every path to one element is one address, as
    /// C's `==` and `-` see it.
    fn addr(&self, index: isize) -> usize;
}

/// The address `index` elements of `T` from `at`.
fn nth<T>(at: usize, index: isize) -> usize {
    at.wrapping_add_signed(index * std::mem::size_of::<T>() as isize)
}

/// Where a cell's value is stored: its box is never replaced, only written
/// through, so this holds while the cell lives.
fn home<T>(cell: &Mutex<Box<T>>) -> usize {
    &**cell.lock() as *const T as usize
}

/// A scalar variable's cell, a run of one, and its [`home`].
struct Cell<T>(Arc<Mutex<Box<T>>>, usize);

impl<T: Send> Run<T> for Cell<T> {
    fn read(&self, f: &mut dyn FnMut(&[T])) {
        f(std::slice::from_ref(&**self.0.lock()))
    }

    fn write(&self, f: &mut dyn FnMut(&mut [T])) {
        f(std::slice::from_mut(&mut **self.0.lock()))
    }

    fn addr(&self, index: isize) -> usize {
        nth::<T>(self.1, index)
    }
}

/// A read-only run: a string literal, or a string the run time hands out.
struct Fixed<T>(Arc<[T]>);

impl<T: Send + Sync> Run<T> for Fixed<T> {
    fn read(&self, f: &mut dyn FnMut(&[T])) {
        f(&self.0)
    }

    /// C's write to a string literal is undefined.
    fn write(&self, _: &mut dyn FnMut(&mut [T])) {
        panic!("write through a pointer to a string literal")
    }

    fn addr(&self, index: isize) -> usize {
        nth::<T>(self.0.as_ptr() as usize, index)
    }
}

/// A run `malloc` made, and where it is stored.
struct Heap<T>(Mutex<Box<[T]>>, usize);

impl<T: Send> Run<T> for Heap<T> {
    fn read(&self, f: &mut dyn FnMut(&[T])) {
        f(&self.0.lock())
    }

    fn write(&self, f: &mut dyn FnMut(&mut [T])) {
        f(&mut self.0.lock())
    }

    fn addr(&self, index: isize) -> usize {
        nth::<T>(self.1, index)
    }
}

/// A multi-PV array's cells, read and written as one array through a copy
/// taken under all their locks, and each cell's [`home`].
struct Cells<T>(Box<[Arc<Mutex<Box<T>>>]>, Box<[usize]>);

impl<T: Clone + Send> Run<T> for Cells<T> {
    fn read(&self, f: &mut dyn FnMut(&[T])) {
        let cells: Vec<_> = self.0.iter().map(|c| c.lock()).collect();
        let copy: Vec<T> = cells.iter().map(|v| T::clone(v)).collect();
        f(&copy)
    }

    fn write(&self, f: &mut dyn FnMut(&mut [T])) {
        let mut cells: Vec<_> = self.0.iter().map(|c| c.lock()).collect();
        let mut copy: Vec<T> = cells.iter().map(|v| T::clone(v)).collect();
        f(&mut copy);
        for (cell, v) in cells.iter_mut().zip(copy) {
            ***cell = v;
        }
    }

    /// A cell's own address, which a pointer from its [`SeqVar`] has too.
    /// The cells are not contiguous, so past either end counts from the
    /// outermost cell, and is none of them.
    fn addr(&self, index: isize) -> usize {
        let homes = &self.1;
        match usize::try_from(index).ok().and_then(|i| homes.get(i)) {
            Some(&at) => at,
            None if index < 0 => nth::<T>(homes.iter().copied().min().unwrap_or(0), index),
            None => {
                let past = index - homes.len() as isize + 1;
                nth::<T>(homes.iter().copied().max().unwrap_or(0), past)
            }
        }
    }
}

/// The struct member a pointer to it points at, a run of one in the
/// struct.
struct Member<S: 'static, F> {
    parent: Ptr<S>,
    get: fn(&mut S) -> &mut F,
}

impl<S, F> Run<F> for Member<S, F> {
    fn read(&self, f: &mut dyn FnMut(&[F])) {
        self.parent
            .modify(|s| f(std::slice::from_ref((self.get)(s))))
    }

    fn write(&self, f: &mut dyn FnMut(&mut [F])) {
        self.parent
            .modify(|s| f(std::slice::from_mut((self.get)(s))))
    }

    /// The parent's own address plus the member's offset in it.
    fn addr(&self, index: isize) -> usize {
        let offset = self
            .parent
            .modify(|s| (self.get)(s) as *mut F as usize - s as *mut S as usize);
        nth::<F>(self.parent.addr() + offset, index)
    }
}

/// What a `void *` points into: the typed [`Ptr`] it was made from, which
/// only a cast back to its type reaches, and that pointer's address. C's
/// `*p` of a `void *` is an error.
struct Erased(Box<dyn Any + Send + Sync>, usize);

impl Run<c_void> for Erased {
    fn read(&self, _: &mut dyn FnMut(&[c_void])) {
        panic!("dereference of a void pointer")
    }

    fn write(&self, _: &mut dyn FnMut(&mut [c_void])) {
        panic!("dereference of a void pointer")
    }

    fn erased(&self) -> Option<&(dyn Any + Send + Sync)> {
        Some(&*self.0)
    }

    fn addr(&self, index: isize) -> usize {
        nth::<c_void>(self.1, index)
    }
}

/// The elements of the array a pointer points at.
struct Elems<A: 'static>(Ptr<A>);

impl<A: Indexed + 'static> Run<A::Elem> for Elems<A> {
    fn read(&self, f: &mut dyn FnMut(&[A::Elem])) {
        self.0.read(|a| f(a.elems()))
    }

    fn write(&self, f: &mut dyn FnMut(&mut [A::Elem])) {
        self.0.modify(|a| f(a.elems_mut()))
    }

    /// Counted from the array's own address, as [`Indexed`] lays it out.
    fn addr(&self, index: isize) -> usize {
        nth::<A::Elem>(self.0.addr(), index)
    }
}

/// C's `T *`: null, or element `index` of a run. It keeps what it points
/// into alive, so it never dangles. `p + n` may leave the run, as C's may;
/// a read or write there panics where C's is undefined, as does one
/// through null.
pub struct Ptr<T: 'static>(Option<Target<T>>);

struct Target<T: 'static> {
    run: Arc<dyn Run<T>>,
    index: isize,
}

impl<T> Ptr<T> {
    fn along(run: Arc<dyn Run<T>>, index: isize) -> Self {
        Ptr(Some(Target { run, index }))
    }

    pub fn is_null(&self) -> bool {
        self.0.is_none()
    }

    /// C's `p + n`.
    pub fn offset(&self, n: isize) -> Self {
        let t = self.0.as_ref().expect("arithmetic on a null pointer");
        Ptr::along(t.run.clone(), t.index + n)
    }

    /// C's `p - q`, in elements. Runs that start at one address are one
    /// object; pointers into different objects panic, where C's difference
    /// is undefined.
    pub fn diff(&self, q: &Self) -> isize {
        match (&self.0, &q.0) {
            (Some(p), Some(q)) if p.run.addr(0) == q.run.addr(0) => p.index - q.index,
            _ => panic!("difference of pointers into different objects"),
        }
    }

    /// The run and the element this points at, which must be inside it.
    fn target(&self) -> (&dyn Run<T>, usize) {
        let t = self.0.as_ref().expect("null pointer dereference");
        let i = usize::try_from(t.index).expect("pointer before its object");
        (&*t.run, i)
    }

    /// Run `f` on the elements from this one to the end of its run, under
    /// the lock of the variable it points into.
    pub(crate) fn with_rest<R>(&self, f: impl FnOnce(&[T]) -> R) -> R {
        let (run, i) = self.target();
        let (mut f, mut r) = (Some(f), None);
        run.read(&mut |s| {
            let rest = s.get(i..).expect("pointer past its object");
            r = f.take().map(|f| f(rest));
        });
        r.expect("a run calls back once")
    }

    /// [`Self::with_rest`], written.
    pub(crate) fn with_rest_mut<R>(&self, f: impl FnOnce(&mut [T]) -> R) -> R {
        let (run, i) = self.target();
        let (mut f, mut r) = (Some(f), None);
        run.write(&mut |s| {
            let rest = s.get_mut(i..).expect("pointer past its object");
            r = f.take().map(|f| f(rest));
        });
        r.expect("a run calls back once")
    }

    fn read<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        self.with_rest(|s| f(s.first().expect("pointer past its object")))
    }

    /// [`Place::modify`]: C's `*p` written.
    pub fn modify<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        self.with_rest_mut(|s| f(s.first_mut().expect("pointer past its object")))
    }

    /// C's `*p` read.
    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.read(T::clone)
    }

    pub fn set(&self, value: T) {
        self.modify(|v| *v = value)
    }

    /// C's `(uintptr_t) p`, as `%p` prints it: the address of the element
    /// this points at, 0 for null.
    pub fn addr(&self) -> usize {
        self.0.as_ref().map_or(0, |t| t.run.addr(t.index))
    }

    /// C's conversion to `void *`: the same address, which [`Ptr::cast`]
    /// turns back into this pointer.
    pub fn erase(&self) -> Ptr<c_void> {
        Ptr(self.0.as_ref().map(|_| Target {
            run: Arc::new(Erased(Box::new(self.clone()), self.addr())),
            index: 0,
        }))
    }
}

impl Ptr<c_void> {
    /// C's conversion of a `void *` to a `T *`. Only the type it was made
    /// from reads what it points to, so another type panics, as does a
    /// `void *` moved by arithmetic, which C does not define.
    pub fn cast<T: 'static>(&self) -> Ptr<T> {
        let Some(t) = &self.0 else {
            return Ptr(None);
        };
        assert_eq!(t.index, 0, "arithmetic on a void pointer");
        t.run
            .erased()
            .and_then(|p| p.downcast_ref::<Ptr<T>>())
            .expect("cast of a void pointer to another type")
            .clone()
    }
}

impl<A: Indexed> Ptr<A> {
    /// C's `(*p)[index]`, an element of the array `p` points at.
    pub fn at(&self, index: usize) -> At<'_, Self, A> {
        At::new(self, index)
    }
}

impl<T: Zeroed + Send + 'static> Ptr<T> {
    /// A pointer to the first of `n` new zeroed elements.
    pub(crate) fn alloc(n: usize) -> Self {
        let run: Box<[T]> = (0..n).map(|_| T::zeroed()).collect();
        let at = run.as_ptr() as usize;
        Ptr::along(Arc::new(Heap(Mutex::new(run), at)), 0)
    }
}

impl Ptr<c_char> {
    /// A read-only C string of `bytes` and a terminating NUL: a string
    /// literal, or a string the run time hands out.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let run: Arc<[c_char]> = bytes.iter().map(|&b| b as c_char).chain([0]).collect();
        Ptr::along(Arc::new(Fixed(run)), 0)
    }
}

/// C's decay of a string literal (`&b"..."[..]` when it is not UTF-8).
impl From<&str> for Ptr<c_char> {
    fn from(s: &str) -> Self {
        Ptr::from_bytes(s.as_bytes())
    }
}

impl From<&[u8]> for Ptr<c_char> {
    fn from(s: &[u8]) -> Self {
        Ptr::from_bytes(s)
    }
}

/// C's decay of an array variable to a pointer to its first element.
impl<A: Indexed<Elem: 'static> + Send + 'static> From<&SeqVar<A>> for Ptr<A::Elem> {
    fn from(a: &SeqVar<A>) -> Self {
        a.at(0).ptr()
    }
}

/// C's decay of a multi-PV array, whose cells C lays out as one array: the
/// pointer spans them all, so `&v[i]` is `v + i`.
impl<T: Clone + Send + 'static, const N: usize> From<&[SeqVar<T>; N]> for Ptr<T> {
    fn from(a: &[SeqVar<T>; N]) -> Self {
        let cells = a.iter().map(|v| v.0.clone()).collect();
        let homes = a.iter().map(|v| home(&v.0)).collect();
        Ptr::along(Arc::new(Cells(cells, homes)), 0)
    }
}

/// C's decay of an array element that is itself an array: a row of a
/// two-dimensional array, or a `string` of an array of them.
impl<A, P> From<&At<'_, P, A>> for Ptr<<A::Elem as Indexed>::Elem>
where
    A: Indexed<Elem: Indexed<Elem: 'static> + 'static> + 'static,
    P: Addr<A> + ?Sized,
{
    fn from(row: &At<'_, P, A>) -> Self {
        row.at(0).ptr()
    }
}

/// C's decay of a struct member that is an array.
impl<P, S, A> From<&Field<'_, P, S, A>> for Ptr<A::Elem>
where
    P: Addr<S> + ?Sized,
    S: 'static,
    A: Indexed<Elem: 'static> + 'static,
{
    fn from(a: &Field<'_, P, S, A>) -> Self {
        a.at(0).ptr()
    }
}

impl<T> Clone for Ptr<T> {
    fn clone(&self) -> Self {
        Ptr(self.0.as_ref().map(|t| Target {
            run: t.run.clone(),
            index: t.index,
        }))
    }
}

impl<T> Default for Ptr<T> {
    fn default() -> Self {
        Ptr(None)
    }
}

impl<T> Zeroed for Ptr<T> {
    fn zeroed() -> Self {
        Ptr(None)
    }
}

/// C's `==`: the same element, or both null.
impl<T> PartialEq for Ptr<T> {
    fn eq(&self, other: &Self) -> bool {
        self.addr() == other.addr()
    }
}

impl<T> std::fmt::Debug for Ptr<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.addr() {
            0 => f.write_str("null"),
            a => write!(f, "{a:#x}"),
        }
    }
}

impl<T> Place<T> for Ptr<T> {
    fn modify<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        Ptr::modify(self, f)
    }
}

/// `&*p` is `p`.
impl<T> Addr<T> for Ptr<T> {
    fn ptr(&self) -> Ptr<T> {
        self.clone()
    }
}

/// The variable a channel is assigned to, as the pv layer sees it: C's
/// `offset` into the variable block, plus the copy C does with `memcpy`.
///
/// Every transfer converts element by element through [`EpicsValue`]; none
/// copies bytes, so the request type and the variable type may differ in
/// width.
pub trait ChanCell: Sync {
    fn kind(&self) -> ElemKind;

    /// Elements the variable holds (the declared array length, 1 for a
    /// scalar).
    fn count(&self) -> u32;

    /// The first `count` elements, as `pvPut` sends them.
    fn load(&self, count: u32) -> EpicsValue;

    /// Write the first `count` elements of `value`, as a get or monitor
    /// delivers them; elements past what `value` holds are left alone.
    fn store(&self, value: &EpicsValue, count: u32);

    /// Copy the first `count` elements from the same channel's cell in
    /// another instance of the variables (safe mode's buffer to state set
    /// copy, `dbCount` elements as `seq_task.c:139` copies), leaving the
    /// rest alone.
    fn copy_from(&self, src: &dyn ChanCell, count: u32);

    fn as_any(&self) -> &dyn Any;
}

impl<T: Prim> ChanCell for SeqVar<T> {
    fn kind(&self) -> ElemKind {
        T::KIND
    }

    fn count(&self) -> u32 {
        1
    }

    fn load(&self, count: u32) -> EpicsValue {
        self.modify(|v| T::to_value(&std::slice::from_ref(v)[..count as usize]))
    }

    fn store(&self, value: &EpicsValue, count: u32) {
        if count > 0
            && let Some(v) = T::from_value(value, 0)
        {
            self.set(v);
        }
    }

    fn copy_from(&self, src: &dyn ChanCell, count: u32) {
        if count > 0 {
            let v = same_cell(self, src).get();
            self.set(v);
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl<T: Prim, const N: usize> ChanCell for SeqVar<[T; N]> {
    fn kind(&self) -> ElemKind {
        T::KIND
    }

    fn count(&self) -> u32 {
        N as u32
    }

    fn load(&self, count: u32) -> EpicsValue {
        self.modify(|a| T::to_value(&a[..count as usize]))
    }

    fn store(&self, value: &EpicsValue, count: u32) {
        self.modify(|a| {
            for (i, elem) in a.iter_mut().take(count as usize).enumerate() {
                let Some(v) = T::from_value(value, i) else {
                    break;
                };
                *elem = v;
            }
        })
    }

    fn copy_from(&self, src: &dyn ChanCell, count: u32) {
        let n = (count as usize).min(N);
        let src = same_cell(self, src).0.lock();
        self.modify(|a| a[..n].copy_from_slice(&src[..n]));
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `src` as the cell type it must be: the same channel's cell in another
/// instance of the variables.
fn same_cell<'a, C: ChanCell + 'static>(_: &C, src: &'a dyn ChanCell) -> &'a C {
    src.as_any()
        .downcast_ref()
        .expect("copy_from takes the same channel's cell")
}

/// `struct seqg_vars`: the variable block snc-rs generates.
///
/// `Clone` is safe mode's per-state-set copy of the shared buffer.
pub trait ProgramVars: Clone + Send + Sync + 'static {
    /// Every variable zero, before `seqg_init` assigns the initializers.
    fn zeroed() -> Self;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_is_a_cell_of_its_own() {
        let x = SeqVar::new(1i32);
        let y = x.clone();
        y.set(2);
        assert_eq!((x.get(), y.get()), (1, 2));
    }

    #[test]
    fn a_pointer_to_a_variable_reads_and_writes_it() {
        let x = SeqVar::new(1i32);
        let p = SeqVar::new(Ptr::default());
        p.set(x.ptr());
        p.get().set(5);
        assert_eq!(x.get(), 5);
        x.set(7);
        assert_eq!(p.get().get(), 7);
        p.get().modify(|v| *v += 1);
        assert_eq!(x.get(), 8);
    }

    #[test]
    fn pointers_are_equal_at_one_variable_or_both_null() {
        let (x, y) = (SeqVar::new(0.5f64), SeqVar::new(0.5f64));
        assert_eq!(x.ptr(), x.ptr());
        assert_ne!(x.ptr(), y.ptr());
        assert_ne!(x.ptr(), Ptr::default());
        assert_eq!(<Ptr<f64>>::zeroed(), Ptr::default());
        assert!(<Ptr<f64>>::default().is_null() && !x.ptr().is_null());
    }

    /// Safe mode's copy keeps the pointer value, as `memcpy` does.
    #[test]
    fn a_copied_pointer_still_points_into_the_original() {
        let x = SeqVar::new(1i32);
        let p = SeqVar::new(x.ptr());
        let copy = p.clone();
        copy.get().set(2);
        assert_eq!(x.get(), 2);
    }

    #[test]
    fn a_pointer_moves_along_its_array() {
        let a = SeqVar::new([1i32, 2, 3, 4]);
        let p = a.at(1).ptr();
        assert_eq!(p.get(), 2);
        p.offset(2).set(9);
        assert_eq!(a.get(), [1, 2, 3, 9]);
        assert_eq!(p.offset(-1), a.at(0).ptr());
        assert_eq!(a.at(4).ptr().diff(&p), 3);
    }

    #[test]
    fn a_pointer_into_a_row_moves_along_the_row() {
        let a = <SeqVar<[[f64; 3]; 2]>>::zeroed();
        let p = a.at(1).at(0).ptr();
        p.offset(2).set(7.0);
        assert_eq!(a.get(), [[0.0; 3], [0.0, 0.0, 7.0]]);
        // one past row 0 is row 1's first element, as in C
        assert_eq!(a.at(0).at(3).ptr(), p);
    }

    /// array.st:40 `(*a4ps[i])[j]`.
    #[test]
    fn an_element_of_the_array_a_pointer_points_at_is_its_place() {
        let a = <SeqVar<[[i16; 3]; 2]>>::zeroed();
        let p = a.at(1).ptr();
        p.at(2).set(5);
        assert_eq!(a.at(1).at(2).get(), 5);
        assert_eq!(p.at(2).ptr(), a.at(1).at(2).ptr());
        let s = <SeqVar<[[SeqString; 2]; 2]>>::zeroed();
        let q = s.at(1).ptr();
        assert_eq!(Ptr::from(&q.at(1)), Ptr::from(&s.at(1).at(1)));
    }

    #[test]
    fn a_char_pointer_moves_along_a_string() {
        let s = <SeqVar<SeqString>>::zeroed();
        s.modify(|v| v.set(b"idle"));
        let p = s.at(1).ptr();
        p.set(b'c' as c_char);
        assert_eq!(p.offset(1).get(), b'l' as c_char);
        assert_eq!(s.get().as_bytes(), b"icle");
    }

    #[test]
    fn a_pointer_into_a_multi_pv_array_moves_along_its_cells() {
        // reviewer2's mpv.st: C lays `int v[4]; assign v to {...}` out as
        // one array, so &v[0] + 1 is &v[1]
        let v: [SeqVar<i32>; 4] = std::array::from_fn(|i| SeqVar::new(10 * i as i32));
        let p = Ptr::from(&v);
        assert!(p.offset(1) == Ptr::from(&v).offset(1));
        assert_eq!(p.offset(1).get(), 10);
        assert_eq!(Ptr::from(&v).offset(3).diff(&p), 3);
        p.offset(2).set(7);
        assert_eq!(v[2].get(), 7);
        let w: [SeqVar<i32>; 4] = std::array::from_fn(|_| SeqVar::new(0));
        assert!(p != Ptr::from(&w));
    }

    /// array.st:40: `a4a5s[i][j] == (*a4ps[i])[j]`, where `a4ps[i]` is
    /// `&a4a5s[i]`, is one address however each side reaches it.
    #[test]
    fn a_pointer_through_a_multi_pv_array_is_its_cells_own() {
        let v = <[SeqVar<[SeqString; 5]>; 4]>::zeroed();
        let rows = <SeqVar<[Ptr<[SeqString; 5]>; 4]>>::zeroed();
        for i in 0..4 {
            rows.at(i).set(Ptr::from(&v).offset(i as isize));
        }
        for i in 0..4 {
            assert!(Ptr::from(&v).offset(i as isize) == v[i].ptr());
            for j in 0..5 {
                let own = Ptr::from(&v[i].at(j));
                let through = Ptr::from(&rows.at(i).get().at(j));
                assert!(own == through, "a4a5s[{i}][{j}]");
                assert_eq!(own.addr(), through.addr());
                assert_eq!(through.offset(3).diff(&own), 3);
            }
        }
        // one past the last cell is none of them
        let end = Ptr::from(&v).offset(4);
        assert!((0..4).all(|i| end != Ptr::from(&v).offset(i)));
    }

    #[test]
    fn a_void_pointer_casts_back_to_the_pointer_it_was_made_from() {
        // void.st: void *p = &x; *(int *)p == x
        let x = SeqVar::new(42i32);
        let v = x.ptr().erase();
        assert!(v == x.ptr().erase());
        assert_eq!(v.addr(), x.ptr().addr());
        assert_eq!(v.cast::<i32>().get(), 42);
        v.cast::<i32>().set(7);
        assert_eq!(x.get(), 7);
        assert!(Ptr::<i32>::default().erase().is_null());
        assert!(Ptr::<std::ffi::c_void>::default().cast::<i32>().is_null());
    }

    #[test]
    #[should_panic(expected = "cast of a void pointer to another type")]
    fn a_void_pointer_cast_to_another_type_panics() {
        SeqVar::new(42i32).ptr().erase().cast::<f64>();
    }

    #[test]
    #[should_panic(expected = "dereference of a void pointer")]
    fn a_void_pointer_does_not_dereference() {
        SeqVar::new(42i32).ptr().erase().modify(|_| ());
    }

    #[derive(Clone, Debug, PartialEq)]
    struct Limits {
        low: f64,
        high: f64,
    }

    /// C's member order, as snc-rs declares an SNL struct
    #[derive(Clone, Debug, PartialEq)]
    #[repr(C)]
    struct Rec {
        mark: i32,
        value: [f64; 3],
        alarm: Limits,
    }

    fn rec() -> SeqVar<Rec> {
        SeqVar::new(Rec {
            mark: 1,
            value: [0.0, 1.1, 2.2],
            alarm: Limits {
                low: -10.0,
                high: 10.0,
            },
        })
    }

    #[test]
    fn a_struct_member_is_a_place_in_its_struct() {
        let s = rec();
        s.field(|v| &mut v.mark).set(5);
        s.field(|v| &mut v.alarm)
            .field(|v| &mut v.high)
            .modify(|v| *v *= 2.0);
        s.field(|v| &mut v.value).at(2).set(4.4);
        let r = s.get();
        assert_eq!((r.mark, r.alarm.high, r.value[2]), (5, 20.0, 4.4));
    }

    #[test]
    fn a_pointer_to_a_struct_member_points_into_the_struct() {
        let s = rec();
        let p = s.field(|v| &mut v.alarm).field(|v| &mut v.low).ptr();
        s.field(|v| &mut v.alarm).set(Limits {
            low: -1.0,
            high: 1.0,
        });
        assert_eq!(p.get(), -1.0);
        p.set(-2.0);
        assert_eq!(s.get().alarm.low, -2.0);
        // `&s.mark` is `&s` when the member is first, as C lays it out
        assert_eq!(s.field(|v| &mut v.mark).ptr().addr(), s.ptr().addr());
    }

    #[test]
    fn a_member_through_a_pointer_is_the_member_of_what_it_points_to() {
        let s = rec();
        let p = s.ptr();
        // `p->mark`, which is `(*p).mark`
        p.field(|v| &mut v.mark).set(7);
        assert_eq!(s.get().mark, 7);
    }

    #[test]
    fn an_array_member_decays_to_a_pointer_along_the_member() {
        let s = rec();
        let p = Ptr::from(&s.field(|v| &mut v.value));
        assert_eq!(p.offset(1).get(), 1.1);
        p.offset(2).set(9.9);
        assert_eq!(s.get().value[2], 9.9);
        assert_eq!(p.offset(1), s.field(|v| &mut v.value).at(1).ptr());
    }

    #[test]
    fn a_struct_assignment_copies_the_struct() {
        let (a, b) = (rec(), rec());
        b.set(a.get());
        a.field(|v| &mut v.mark).set(3);
        a.field(|v| &mut v.value).at(0).set(8.0);
        let b = b.get();
        assert_eq!((b.mark, b.value[0]), (1, 0.0));
    }

    /// A companion's `union { int i; float f; double d; Rec *p; }`.
    #[derive(Clone)]
    struct Union(UnionBytes<8>);

    impl Union {
        fn i(&self) -> i32 {
            self.0.get()
        }
        fn set_i(&mut self, v: i32) {
            self.0.set(v)
        }
        fn f(&self) -> f32 {
            self.0.get()
        }
        fn d(&self) -> f64 {
            self.0.get()
        }
        fn set_d(&mut self, v: f64) {
            self.0.set(v)
        }
        fn p(&self) -> Ptr<Rec> {
            self.0.get()
        }
        fn set_p(&mut self, v: Ptr<Rec>) {
            self.0.set(v)
        }
    }

    #[test]
    fn a_union_member_reads_back_what_another_wrote() {
        let u = SeqVar::new(Union(UnionBytes::zeroed()));
        let i = u.view(|v| v.i(), |v, x| v.set_i(x));
        i.set(0x3f80_0000);
        assert_eq!(u.view(|v| v.f(), |_, _: f32| ()).get(), 1.0);
        u.view(|v| v.d(), |v, x| v.set_d(x)).set(-0.0);
        // the sign bit is the top byte's, past `i` on a little-endian host
        let low = i32::from_ne_bytes((-0.0f64).to_ne_bytes()[..4].try_into().unwrap());
        assert_eq!(i.get(), low);
        i.modify(|v| *v += 1);
        assert_eq!(i.get(), low + 1);
    }

    #[test]
    fn a_union_pointer_member_reads_back_its_pointer() {
        let s = rec();
        let u = SeqVar::new(Union(UnionBytes::zeroed()));
        let p = u.view(|v| v.p(), |v, x| v.set_p(x));
        assert!(p.get().is_null());
        p.set(s.ptr());
        assert_eq!(p.get(), s.ptr());
        // `u.p->mark`
        p.get().field(|v| &mut v.mark).set(4);
        assert_eq!(s.get().mark, 4);
        // the address's low bytes, as C reads them through `i`
        let low = i32::from_ne_bytes(s.ptr().addr().to_ne_bytes()[..4].try_into().unwrap());
        assert_eq!(u.view(|v| v.i(), |v, x| v.set_i(x)).get(), low);
    }

    #[test]
    #[should_panic(expected = "union pointer member read over another member")]
    fn a_union_pointer_member_overwritten_by_another_member_panics() {
        let s = rec();
        let u = SeqVar::new(Union(UnionBytes::zeroed()));
        u.view(|v| v.p(), |v, x| v.set_p(x)).set(s.ptr());
        u.view(|v| v.i(), |v, x| v.set_i(x)).set(13);
        u.view(|v| v.p(), |v, x| v.set_p(x)).get();
    }

    #[test]
    #[should_panic(expected = "pointer past its object")]
    fn a_pointer_past_its_array_panics() {
        let a = SeqVar::new([1i32, 2]);
        a.at(0).ptr().offset(2).get();
    }

    #[test]
    #[should_panic(expected = "different objects")]
    fn a_difference_across_variables_panics() {
        let (a, b) = (SeqVar::new([1i32, 2]), SeqVar::new([1i32, 2]));
        a.at(0).ptr().diff(&b.at(0).ptr());
    }

    #[test]
    fn an_array_decays_to_a_pointer_to_its_first_element() {
        let a = SeqVar::new([1i32, 2, 3]);
        assert_eq!(Ptr::from(&a), a.at(0).ptr());
        let m = SeqVar::new([[1i32, 2], [3, 4]]);
        let row = Ptr::from(&m.at(1));
        assert_eq!(row, m.at(1).at(0).ptr());
        assert_eq!(row.offset(1).get(), 4);
        let s = SeqVar::new(SeqString::new());
        Ptr::from(&s).set(b'x' as c_char);
        assert_eq!(s.get().as_bytes(), b"x");
        assert_eq!(Ptr::<c_char>::from("ab").offset(1).get(), b'b' as c_char);
    }

    #[test]
    fn a_c_string_reads_up_to_its_terminator() {
        let p = Ptr::from_bytes(b"ab");
        assert_eq!(p.offset(1).get(), b'b' as c_char);
        assert_eq!(p.offset(2).get(), 0);
        assert_ne!(p, Ptr::from_bytes(b"ab"));
    }

    #[test]
    #[should_panic(expected = "write through a pointer to a string literal")]
    fn a_write_through_a_c_string_panics() {
        Ptr::from_bytes(b"ab").set(0);
    }

    #[test]
    #[should_panic(expected = "null pointer dereference")]
    fn a_null_pointer_dereference_panics() {
        <Ptr<i32>>::default().get();
    }

    #[test]
    fn a_large_array_is_built_on_the_heap() {
        let v = <SeqVar<[f64; 65536]>>::zeroed();
        assert_eq!(v.at(65535).get(), 0.0);
    }

    #[test]
    fn a_two_dimensional_element_reads_back_what_was_set() {
        let v = <SeqVar<[[f64; 3]; 3]>>::zeroed();
        v.at(1).at(2).set(4.5);
        assert_eq!(v.get()[1], [0.0, 0.0, 4.5]);
        v.at(1).at(2).modify(|x| *x += 1.0);
        assert_eq!(v.at(1).at(2).get(), 5.5);
    }

    #[test]
    fn a_string_character_reads_and_writes_as_a_char() {
        let s = <SeqVar<SeqString>>::zeroed();
        s.modify(|v| v.set(b"idle"));
        assert_eq!(s.at(0).get(), b'i' as c_char);
        s.at(1).set(b'c' as c_char);
        assert_eq!(s.get().as_bytes(), b"icle");
        let a = <SeqVar<[SeqString; 2]>>::zeroed();
        a.at(1).modify(|v| v.set(b"ab"));
        assert_eq!(a.at(1).at(1).get(), b'b' as c_char);
    }

    #[test]
    #[should_panic]
    fn a_string_subscript_past_the_buffer_panics() {
        let s = <SeqVar<SeqString>>::zeroed();
        s.at(crate::types::MAX_STRING_SIZE).get();
    }

    #[test]
    #[should_panic]
    fn a_subscript_past_the_end_panics() {
        let v = <SeqVar<[i32; 4]>>::zeroed();
        v.at(4).get();
    }

    /// Narrowing to the put type is the pv layer's, so an unsigned value
    /// goes out unsigned.
    #[test]
    fn load_sends_the_variable_s_own_type() {
        assert_eq!(SeqVar::new(-3i8).load(1), EpicsValue::Char(0xfd));
        assert_eq!(SeqVar::new(200u8).load(1), EpicsValue::UChar(200));
        assert_eq!(SeqVar::new(-7i16).load(1), EpicsValue::Short(-7));
        assert_eq!(SeqVar::new(0xffffu16).load(1), EpicsValue::UShort(0xffff));
        assert_eq!(SeqVar::new(-9i32).load(1), EpicsValue::Long(-9));
        assert_eq!(SeqVar::new(u32::MAX).load(1), EpicsValue::ULong(u32::MAX));
        assert_eq!(SeqVar::new(1.5f32).load(1), EpicsValue::Float(1.5));
        assert_eq!(SeqVar::new(2.5f64).load(1), EpicsValue::Double(2.5));
        let mut s = SeqString::new();
        s.set(b"idle");
        assert_eq!(
            SeqVar::new(s).load(1),
            EpicsValue::String(PvString::from_bytes(*b"idle"))
        );
    }

    #[test]
    fn load_sends_the_first_count_elements() {
        let a = SeqVar::new([1i16, 2, 3, 4]);
        assert_eq!(a.load(3), EpicsValue::ShortArray(vec![1, 2, 3].into()));
        assert_eq!(a.load(1), EpicsValue::Short(1));
        assert_eq!(a.load(0), EpicsValue::ShortArray(Vec::new().into()));
        // `c_char` is `u8` on aarch64 Linux.
        let c = <SeqVar<[c_char; 3]>>::zeroed();
        let zeros = vec![0, 0, 0].into();
        let want = match C_CHAR_KIND {
            ElemKind::I8 => EpicsValue::CharArray(zeros),
            _ => EpicsValue::UCharArray(zeros),
        };
        assert_eq!(c.load(3), want);
    }

    #[test]
    fn a_cell_reports_its_element_kind_and_count() {
        assert_eq!(<SeqVar<i16>>::zeroed().kind(), ElemKind::I16);
        assert_eq!(<SeqVar<[SeqString; 32]>>::zeroed().count(), 32);
        assert_eq!(<SeqVar<[c_char; 64]>>::zeroed().kind(), C_CHAR_KIND);
    }

    #[test]
    fn a_same_width_integer_keeps_its_bits() {
        assert_eq!(u32::from_value(&EpicsValue::Long(-1), 0), Some(u32::MAX));
        assert_eq!(i16::from_value(&EpicsValue::UShort(0xffff), 0), Some(-1));
        assert_eq!(i8::from_value(&EpicsValue::UChar(0xff), 0), Some(-1));
    }

    /// `Char` is signed and `UChar` unsigned when widened.
    #[test]
    fn a_char_widens_by_its_signedness() {
        assert_eq!(i32::from_value(&EpicsValue::Char(0xff), 0), Some(-1));
        assert_eq!(i32::from_value(&EpicsValue::UChar(0xff), 0), Some(255));
        let a = EpicsValue::CharArray(vec![1, 0x80].into());
        assert_eq!(i32::from_value(&a, 1), Some(-128));
    }

    #[test]
    fn a_float_truncates_into_an_integer() {
        assert_eq!(i32::from_value(&EpicsValue::Double(2.7), 0), Some(2));
        assert_eq!(f64::from_value(&EpicsValue::Enum(3), 0), Some(3.0));
    }

    #[test]
    fn an_element_past_the_end_is_none() {
        assert_eq!(f64::from_value(&EpicsValue::Double(1.0), 1), None);
        let a = EpicsValue::LongArray(vec![1, 2].into());
        assert_eq!(i32::from_value(&a, 1), Some(2));
        assert_eq!(i32::from_value(&a, 2), None);
    }

    #[test]
    fn a_string_and_a_number_do_not_convert() {
        let s = EpicsValue::String(PvString::from_bytes(&b"1"[..]));
        assert_eq!(i32::from_value(&s, 0), None);
        assert_eq!(SeqString::from_value(&EpicsValue::Long(1), 0), None);
    }

    #[test]
    fn a_string_element_is_cut_to_the_buffer() {
        let a = EpicsValue::StringArray(
            vec![
                PvString::from_bytes(&b"a"[..]),
                PvString::from_bytes(vec![b'x'; 64]),
            ]
            .into(),
        );
        let s = SeqString::from_value(&a, 1).unwrap();
        assert_eq!(s.as_bytes().len(), crate::types::MAX_STRING_SIZE - 1);
        assert_eq!(SeqString::from_value(&a, 0).unwrap().as_bytes(), b"a");
    }

    #[test]
    fn one_element_is_a_scalar_and_more_an_array() {
        assert_eq!(i8::to_value(&[-1]), EpicsValue::Char(0xff));
        assert_eq!(u8::to_value(&[0xff]), EpicsValue::UChar(0xff));
        assert_eq!(
            i32::to_value(&[1, -2]),
            EpicsValue::LongArray(vec![1, -2].into())
        );
        let mut s = SeqString::new();
        s.set(b"idle");
        assert_eq!(
            SeqString::to_value(&[s]),
            EpicsValue::String(PvString::from_bytes(&b"idle"[..]))
        );
        assert_eq!(
            SeqString::to_value(&[s, s]),
            EpicsValue::StringArray(vec![PvString::from_bytes(&b"idle"[..]); 2].into())
        );
    }

    #[test]
    fn a_value_round_trips_through_its_own_type() {
        let v = [1.5f32, -2.0];
        let back: Vec<f32> = (0..2)
            .map(|i| f32::from_value(&f32::to_value(&v), i).unwrap())
            .collect();
        assert_eq!(back, v);
        assert_eq!(
            u64::from_value(&u64::to_value(&[u64::MAX]), 0),
            Some(u64::MAX)
        );
    }

    /// `ss_write_buffer` moves `dbCount` elements of what arrived.
    #[test]
    fn store_writes_the_first_count_elements_it_is_given() {
        let a = SeqVar::new([9i32; 4]);
        a.store(&EpicsValue::LongArray(vec![1, 2, 3].into()), 2);
        assert_eq!(a.get(), [1, 2, 9, 9]);
        a.store(&EpicsValue::LongArray(vec![5, 6, 7].into()), 4);
        assert_eq!(a.get(), [5, 6, 7, 9]);
    }

    /// `pv_type_map` gets an unsigned variable as the signed type of its
    /// width, and `ss_write_buffer` copies the bits.
    #[test]
    fn an_unsigned_variable_stores_the_bits_of_its_get_type() {
        let c = SeqVar::new(0u8);
        c.store(&EpicsValue::Char(0xff), 1);
        assert_eq!(c.get(), 0xff);
        let s = SeqVar::new([0u16; 2]);
        s.store(&EpicsValue::ShortArray(vec![-1, i16::MIN].into()), 2);
        assert_eq!(s.get(), [0xffff, 0x8000]);
        let l = SeqVar::new(0u32);
        l.store(&EpicsValue::Long(i32::MIN), 1);
        assert_eq!(l.get(), 0x8000_0000);
    }

    /// Before connect `dbCount` is 0, and C's copy moves nothing.
    #[test]
    fn a_scalar_stores_only_with_a_count() {
        let x = SeqVar::new(7i16);
        x.store(&EpicsValue::Short(1), 0);
        assert_eq!(x.get(), 7);
        x.store(&EpicsValue::Double(2.9), 1);
        assert_eq!(x.get(), 2);
    }

    #[test]
    fn a_value_of_the_other_kind_stores_nothing() {
        let s = SeqVar::new(SeqString::new());
        s.store(&EpicsValue::Long(1), 1);
        assert_eq!(s.get().as_bytes(), b"");
        let a = SeqVar::new([3.0f64; 2]);
        a.store(&EpicsValue::String(PvString::from_bytes(&b"1"[..])), 2);
        assert_eq!(a.get(), [3.0; 2]);
    }

    /// A `+s` state set's elements past `dbCount` are its own.
    #[test]
    fn copy_from_copies_count_elements_and_keeps_the_rest() {
        let buf = SeqVar::new([1u16, 2, 3, 4]);
        let copy = SeqVar::new([9u16; 4]);
        copy.copy_from(&buf, 2);
        assert_eq!(copy.get(), [1, 2, 9, 9]);
        let x = SeqVar::new(5.5f32);
        let y = SeqVar::new(0.0f32);
        y.copy_from(&x, 0);
        assert_eq!(y.get(), 0.0);
        y.copy_from(&x, 1);
        assert_eq!(y.get(), 5.5);
    }
}
