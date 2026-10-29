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

/// Dispatch on an [`NDDataBuffer`](crate::ndarray::NDDataBuffer)'s element
/// type, binding its elements as a slice to `$v`. The body `$body` is
/// monomorphized for each type.
#[macro_export]
macro_rules! with_buffer {
    ($buffer:expr, |$v:ident| $body:expr) => {
        match ($buffer).view() {
            $crate::ndarray::NDDataView::I8($v) => $body,
            $crate::ndarray::NDDataView::U8($v) => $body,
            $crate::ndarray::NDDataView::I16($v) => $body,
            $crate::ndarray::NDDataView::U16($v) => $body,
            $crate::ndarray::NDDataView::I32($v) => $body,
            $crate::ndarray::NDDataView::U32($v) => $body,
            $crate::ndarray::NDDataView::I64($v) => $body,
            $crate::ndarray::NDDataView::U64($v) => $body,
            $crate::ndarray::NDDataView::F32($v) => $body,
            $crate::ndarray::NDDataView::F64($v) => $body,
        }
    };
}

/// Same as with_buffer! but binds a mutable slice.
#[macro_export]
macro_rules! with_buffer_mut {
    ($buffer:expr, |$v:ident| $body:expr) => {
        match ($buffer).view_mut() {
            $crate::ndarray::NDDataViewMut::I8($v) => $body,
            $crate::ndarray::NDDataViewMut::U8($v) => $body,
            $crate::ndarray::NDDataViewMut::I16($v) => $body,
            $crate::ndarray::NDDataViewMut::U16($v) => $body,
            $crate::ndarray::NDDataViewMut::I32($v) => $body,
            $crate::ndarray::NDDataViewMut::U32($v) => $body,
            $crate::ndarray::NDDataViewMut::I64($v) => $body,
            $crate::ndarray::NDDataViewMut::U64($v) => $body,
            $crate::ndarray::NDDataViewMut::F32($v) => $body,
            $crate::ndarray::NDDataViewMut::F64($v) => $body,
        }
    };
}

/// [`with_buffer_mut!`] that also names the element type: `$t` is a type
/// alias for the arm's element type inside `$body`, for a body that has to
/// cast into the buffer (`x as $t`).
#[macro_export]
macro_rules! with_buffer_mut_typed {
    ($buffer:expr, |$v:ident : $t:ident| $body:expr) => {
        match ($buffer).view_mut() {
            $crate::ndarray::NDDataViewMut::I8($v) => {
                type $t = i8;
                $body
            }
            $crate::ndarray::NDDataViewMut::U8($v) => {
                type $t = u8;
                $body
            }
            $crate::ndarray::NDDataViewMut::I16($v) => {
                type $t = i16;
                $body
            }
            $crate::ndarray::NDDataViewMut::U16($v) => {
                type $t = u16;
                $body
            }
            $crate::ndarray::NDDataViewMut::I32($v) => {
                type $t = i32;
                $body
            }
            $crate::ndarray::NDDataViewMut::U32($v) => {
                type $t = u32;
                $body
            }
            $crate::ndarray::NDDataViewMut::I64($v) => {
                type $t = i64;
                $body
            }
            $crate::ndarray::NDDataViewMut::U64($v) => {
                type $t = u64;
                $body
            }
            $crate::ndarray::NDDataViewMut::F32($v) => {
                type $t = f32;
                $body
            }
            $crate::ndarray::NDDataViewMut::F64($v) => {
                type $t = f64;
                $body
            }
        }
    };
}

pub use with_buffer;
pub use with_buffer_mut;
pub use with_buffer_mut_typed;

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
        let buf = NDDataBuffer::from_vec::<u8>(vec![1, 2, 3]);
        let sum: f64 = with_buffer!(&buf, |v| { v.iter().map(|x| PixelCast::to_f64(*x)).sum() });
        assert_eq!(sum, 6.0);
    }
}
