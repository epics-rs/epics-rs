//! C arithmetic on an operand whose type snc-rs does not know: a foreign
//! value or a foreign struct's member. C's compiler converts the operands
//! by their C types (C11 6.3.1.8's usual arithmetic conversions); here
//! rustc picks the same common type from their Rust types, so a `double`
//! member times a `long` is a `double` product, not a truncated `long`.
//! Integer types are LP64's, as [`crate::types`] maps them.

use std::ops::{Add, BitAnd, BitOr, BitXor, Mul, Sub};

/// An arithmetic type, by its integer promotion (C11 6.3.1.1).
pub trait Arith: Copy {
    type Promoted: Copy;
    fn promote(self) -> Self::Promoted;
}

macro_rules! arith {
    ($($t:ty => $p:ty),* $(,)?) => {$(
        impl Arith for $t {
            type Promoted = $p;
            fn promote(self) -> $p {
                self as $p
            }
        }
    )*};
}

arith!(
    i8 => i32, u8 => i32, i16 => i32, u16 => i32, i32 => i32, u32 => u32,
    i64 => i64, u64 => u64, isize => i64, usize => u64, f32 => f32, f64 => f64,
);

/// A common type's operators as C's: unsigned arithmetic is modulo 2^n.
pub trait Ops: Copy + PartialOrd {
    fn c_add(self, b: Self) -> Self;
    fn c_sub(self, b: Self) -> Self;
    fn c_mul(self, b: Self) -> Self;
    fn c_div(self, b: Self) -> Self;
    fn c_rem(self, b: Self) -> Self;
}

macro_rules! ops {
    ($add:ident, $sub:ident, $mul:ident: $($t:ty),*) => {$(
        impl Ops for $t {
            fn c_add(self, b: Self) -> Self {
                self.$add(b)
            }
            fn c_sub(self, b: Self) -> Self {
                self.$sub(b)
            }
            fn c_mul(self, b: Self) -> Self {
                self.$mul(b)
            }
            fn c_div(self, b: Self) -> Self {
                self / b
            }
            fn c_rem(self, b: Self) -> Self {
                self % b
            }
        }
    )*};
}

ops!(add, sub, mul: i32, i64, f32, f64);
ops!(wrapping_add, wrapping_sub, wrapping_mul: u32, u64);

/// The common type of two promoted types, `Self` on the left.
pub trait Common<B> {
    type T: Ops;
    fn left(self) -> Self::T;
    fn right(b: B) -> Self::T;
}

macro_rules! common {
    ($($a:ty: [$($b:ty => $t:ty),*]),* $(,)?) => {$($(
        impl Common<$b> for $a {
            type T = $t;
            fn left(self) -> $t {
                self as $t
            }
            fn right(b: $b) -> $t {
                b as $t
            }
        }
    )*)*};
}

common! {
    i32: [i32 => i32, u32 => u32, i64 => i64, u64 => u64, f32 => f32, f64 => f64],
    u32: [i32 => u32, u32 => u32, i64 => i64, u64 => u64, f32 => f32, f64 => f64],
    i64: [i32 => i64, u32 => i64, i64 => i64, u64 => u64, f32 => f32, f64 => f64],
    u64: [i32 => u64, u32 => u64, i64 => u64, u64 => u64, f32 => f32, f64 => f64],
    f32: [i32 => f32, u32 => f32, i64 => f32, u64 => f32, f32 => f32, f64 => f64],
    f64: [i32 => f64, u32 => f64, i64 => f64, u64 => f64, f32 => f64, f64 => f64],
}

/// `Self` and `B` converted to their common type.
pub trait Usual<B> {
    type T: Ops;
    fn left(self) -> Self::T;
    fn right(b: B) -> Self::T;
}

impl<A: Arith, B: Arith> Usual<B> for A
where
    A::Promoted: Common<B::Promoted>,
{
    type T = <A::Promoted as Common<B::Promoted>>::T;
    fn left(self) -> Self::T {
        <A::Promoted as Common<B::Promoted>>::left(self.promote())
    }
    fn right(b: B) -> Self::T {
        <A::Promoted as Common<B::Promoted>>::right(b.promote())
    }
}

fn usual<A: Usual<B>, B>(a: A, b: B) -> (A::T, A::T) {
    (<A as Usual<B>>::left(a), A::right(b))
}

macro_rules! binary {
    ($($(#[$doc:meta])* $f:ident => $op:ident),* $(,)?) => {$(
        $(#[$doc])*
        pub fn $f<A: Usual<B>, B>(a: A, b: B) -> A::T {
            let (a, b) = usual(a, b);
            a.$op(b)
        }
    )*};
}

binary!(
    /// C's `a + b`.
    add => c_add,
    /// C's `a - b`.
    sub => c_sub,
    /// C's `a * b`.
    mul => c_mul,
    /// C's `a / b`.
    div => c_div,
    /// C's `a % b`.
    rem => c_rem,
);

macro_rules! compare {
    ($($(#[$doc:meta])* $f:ident => $op:ident),* $(,)?) => {$(
        $(#[$doc])*
        pub fn $f<A: Usual<B>, B>(a: A, b: B) -> bool {
            let (a, b) = usual(a, b);
            a.$op(&b)
        }
    )*};
}

compare!(
    /// C's `a == b`.
    eq => eq,
    /// C's `a != b`.
    ne => ne,
    /// C's `a < b`.
    lt => lt,
    /// C's `a <= b`.
    le => le,
    /// C's `a > b`.
    gt => gt,
    /// C's `a >= b`.
    ge => ge,
);

macro_rules! bitwise {
    ($($(#[$doc:meta])* $f:ident => $tr:ident $op:tt),* $(,)?) => {$(
        $(#[$doc])*
        pub fn $f<A: Usual<B>, B>(a: A, b: B) -> A::T
        where
            A::T: $tr<Output = A::T>,
        {
            let (a, b) = usual(a, b);
            a $op b
        }
    )*};
}

bitwise!(
    /// C's `a & b`.
    and => BitAnd &,
    /// C's `a | b`.
    or => BitOr |,
    /// C's `a ^ b`.
    xor => BitXor ^,
);

/// C's `c ? a : b`, which evaluates one of `a` and `b` and converts it to
/// their common type.
pub fn select<A: Usual<B>, B>(c: bool, a: impl FnOnce() -> A, b: impl FnOnce() -> B) -> A::T {
    if c {
        <A as Usual<B>>::left(a())
    } else {
        A::right(b())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_double_with_an_integer_is_a_double() {
        assert_eq!(mul(0.5f64, 3), 1.5);
        assert_eq!(mul(0.5f64, 3i64), 1.5);
        assert_eq!(div(7, 2.0f64), 3.5);
        assert!(eq(1.0f64, 1));
        assert!(lt(0.5f64, 1u32));
    }

    #[test]
    fn integers_convert_by_rank_and_sign() {
        // int with unsigned int is unsigned: -1 is UINT_MAX
        assert!(!lt(-1i32, 1u32));
        assert_eq!(add(u32::MAX, 1i32), 0u32);
        // long holds every unsigned int on LP64
        assert!(lt(-1i64, 1u32));
        assert_eq!(sub(0u64, 1i32), u64::MAX);
        // char and short promote to int
        assert_eq!(mul(100i8, 100u8), 10000i32);
        assert_eq!(div(-7i16, 2u16), -3i32);
        assert_eq!(rem(-7, 2), -1);
    }

    #[test]
    fn a_float_with_an_integer_is_a_float() {
        assert_eq!(add(0.5f32, 1), 1.5f32);
        assert_eq!(add(0.5f32, 1.0f64), 1.5f64);
    }

    #[test]
    fn bitwise_operands_convert_as_arithmetic_ones() {
        assert_eq!(and(0xffi64, 0x0fu32), 0x0fi64);
        assert_eq!(or(-1i8, 0u16), -1i32);
        assert_eq!(xor(1u32, 3i32), 2u32);
    }

    #[test]
    fn select_evaluates_one_operand_in_the_common_type() {
        let (mut a, mut b) = (0, 0);
        let v = select(
            true,
            || {
                a += 1;
                2i32
            },
            || {
                b += 1;
                0.5f64
            },
        );
        assert_eq!((v, a, b), (2.0, 1, 0));
        assert_eq!(select(false, || 2i32, || 0.5f64), 0.5);
    }
}
