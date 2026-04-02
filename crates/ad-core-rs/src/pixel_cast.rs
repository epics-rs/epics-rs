/// Trait for casting f64 values to/from typed pixel values.
/// Integer types clamp and round; float types pass through directly.
pub trait PixelCast: Copy + Default + 'static {
    fn from_f64(v: f64) -> Self;
    fn to_f64(self) -> f64;
}

macro_rules! impl_pixel_cast_int {
    ($t:ty) => {
        impl PixelCast for $t {
            #[inline]
            fn from_f64(v: f64) -> Self {
                if v.is_nan() {
                    return 0;
                }
                v.round().clamp(Self::MIN as f64, Self::MAX as f64) as Self
            }
            #[inline]
            fn to_f64(self) -> f64 {
                self as f64
            }
        }
    };
}

impl_pixel_cast_int!(i8);
impl_pixel_cast_int!(u8);
impl_pixel_cast_int!(i16);
impl_pixel_cast_int!(u16);
impl_pixel_cast_int!(i32);
impl_pixel_cast_int!(u32);
impl_pixel_cast_int!(i64);
impl_pixel_cast_int!(u64);

impl PixelCast for f32 {
    #[inline]
    fn from_f64(v: f64) -> Self {
        v as f32
    }
    #[inline]
    fn to_f64(self) -> f64 {
        self as f64
    }
}

impl PixelCast for f64 {
    #[inline]
    fn from_f64(v: f64) -> Self {
        v
    }
    #[inline]
    fn to_f64(self) -> f64 {
        self
    }
}

/// Dispatch on NDDataBuffer variant, binding the inner Vec to `$v`.
/// The body `$body` is monomorphized for each type.
#[macro_export]
macro_rules! with_buffer {
    ($buffer:expr, |$v:ident| $body:expr) => {
        match $buffer {
            $crate::ndarray::NDDataBuffer::I8($v) => $body,
            $crate::ndarray::NDDataBuffer::U8($v) => $body,
            $crate::ndarray::NDDataBuffer::I16($v) => $body,
            $crate::ndarray::NDDataBuffer::U16($v) => $body,
            $crate::ndarray::NDDataBuffer::I32($v) => $body,
            $crate::ndarray::NDDataBuffer::U32($v) => $body,
            $crate::ndarray::NDDataBuffer::I64($v) => $body,
            $crate::ndarray::NDDataBuffer::U64($v) => $body,
            $crate::ndarray::NDDataBuffer::F32($v) => $body,
            $crate::ndarray::NDDataBuffer::F64($v) => $body,
        }
    };
}

/// Same as with_buffer! but gives mutable access.
#[macro_export]
macro_rules! with_buffer_mut {
    ($buffer:expr, |$v:ident| $body:expr) => {
        match $buffer {
            $crate::ndarray::NDDataBuffer::I8($v) => $body,
            $crate::ndarray::NDDataBuffer::U8($v) => $body,
            $crate::ndarray::NDDataBuffer::I16($v) => $body,
            $crate::ndarray::NDDataBuffer::U16($v) => $body,
            $crate::ndarray::NDDataBuffer::I32($v) => $body,
            $crate::ndarray::NDDataBuffer::U32($v) => $body,
            $crate::ndarray::NDDataBuffer::I64($v) => $body,
            $crate::ndarray::NDDataBuffer::U64($v) => $body,
            $crate::ndarray::NDDataBuffer::F32($v) => $body,
            $crate::ndarray::NDDataBuffer::F64($v) => $body,
        }
    };
}

pub use with_buffer;
pub use with_buffer_mut;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndarray::NDDataBuffer;

    #[test]
    fn test_u8_from_f64_normal() {
        assert_eq!(u8::from_f64(128.7), 129);
        assert_eq!(u8::from_f64(0.0), 0);
        assert_eq!(u8::from_f64(255.0), 255);
    }

    #[test]
    fn test_u8_from_f64_overflow() {
        assert_eq!(u8::from_f64(300.0), 255);
        assert_eq!(u8::from_f64(-10.0), 0);
    }

    #[test]
    fn test_u8_from_f64_nan() {
        assert_eq!(u8::from_f64(f64::NAN), 0);
    }

    #[test]
    fn test_i16_from_f64_clamp() {
        assert_eq!(i16::from_f64(40000.0), i16::MAX);
        assert_eq!(i16::from_f64(-40000.0), i16::MIN);
    }

    #[test]
    fn test_u16_from_f64_negative() {
        assert_eq!(u16::from_f64(-5.0), 0);
    }

    #[test]
    fn test_f32_roundtrip() {
        let v = 3.14f64;
        let cast = f32::from_f64(v);
        assert!((cast - 3.14f32).abs() < 1e-5);
        assert!((cast.to_f64() - v).abs() < 1e-5);
    }

    #[test]
    fn test_f64_identity() {
        let v = 1.23456789012345;
        assert_eq!(f64::from_f64(v), v);
        assert_eq!(v.to_f64(), v);
    }

    #[test]
    fn test_i64_large_values() {
        assert_eq!(i64::from_f64(1e18), 1_000_000_000_000_000_000i64);
    }

    #[test]
    fn test_u32_nan() {
        assert_eq!(u32::from_f64(f64::NAN), 0);
    }

    #[test]
    fn test_with_buffer_macro() {
        let buf = NDDataBuffer::U8(vec![1, 2, 3]);
        let sum: f64 = with_buffer!(&buf, |v| { v.iter().map(|x| PixelCast::to_f64(*x)).sum() });
        assert_eq!(sum, 6.0);
    }
}
