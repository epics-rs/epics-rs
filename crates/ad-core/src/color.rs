use crate::error::{ADError, ADResult};
use crate::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};

/// Color mode for NDArray interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum NDColorMode {
    Mono = 0,
    Bayer = 1,
    RGB1 = 2,
    RGB2 = 3,
    RGB3 = 4,
    YUV444 = 5,
    YUV422 = 6,
    YUV411 = 7,
}

impl NDColorMode {
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::Mono,
            1 => Self::Bayer,
            2 => Self::RGB1,
            3 => Self::RGB2,
            4 => Self::RGB3,
            5 => Self::YUV444,
            6 => Self::YUV422,
            7 => Self::YUV411,
            _ => Self::Mono,
        }
    }
}

/// Bayer pattern for raw sensor data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NDBayerPattern {
    RGGB,
    GBRG,
    GRBG,
    BGGR,
}

/// Convert a mono 2D array to RGB1 (3-channel interleaved) by replicating the value.
pub fn mono_to_rgb1(src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 2 {
        return Err(ADError::InvalidDimensions(
            "mono_to_rgb1 requires 2D input".into(),
        ));
    }
    let x = src.dims[0].size;
    let y = src.dims[1].size;
    let n = x * y;

    let out_data = match &src.data {
        NDDataBuffer::U8(v) => {
            let mut out = vec![0u8; n * 3];
            for i in 0..n {
                out[i * 3] = v[i];
                out[i * 3 + 1] = v[i];
                out[i * 3 + 2] = v[i];
            }
            NDDataBuffer::U8(out)
        }
        NDDataBuffer::U16(v) => {
            let mut out = vec![0u16; n * 3];
            for i in 0..n {
                out[i * 3] = v[i];
                out[i * 3 + 1] = v[i];
                out[i * 3 + 2] = v[i];
            }
            NDDataBuffer::U16(out)
        }
        _ => {
            return Err(ADError::UnsupportedConversion(
                "mono_to_rgb1 only supports UInt8 and UInt16".into(),
            ));
        }
    };

    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(x),
        NDDimension::new(y),
    ];
    let mut arr = NDArray::new(dims, src.data.data_type());
    arr.data = out_data;
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    arr.codec = src.codec.clone();
    Ok(arr)
}

/// Convert RGB1 (3-channel interleaved) to mono using luminance formula.
/// Y = 0.299*R + 0.587*G + 0.114*B
pub fn rgb1_to_mono(src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 3 || src.dims[0].size != 3 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_mono requires 3D input with dims[0]=3".into(),
        ));
    }
    let x = src.dims[1].size;
    let y = src.dims[2].size;
    let n = x * y;

    let out_data = match &src.data {
        NDDataBuffer::U8(v) => {
            let mut out = vec![0u8; n];
            for i in 0..n {
                let r = v[i * 3] as f64;
                let g = v[i * 3 + 1] as f64;
                let b = v[i * 3 + 2] as f64;
                out[i] = (0.299 * r + 0.587 * g + 0.114 * b).round() as u8;
            }
            NDDataBuffer::U8(out)
        }
        NDDataBuffer::U16(v) => {
            let mut out = vec![0u16; n];
            for i in 0..n {
                let r = v[i * 3] as f64;
                let g = v[i * 3 + 1] as f64;
                let b = v[i * 3 + 2] as f64;
                out[i] = (0.299 * r + 0.587 * g + 0.114 * b).round() as u16;
            }
            NDDataBuffer::U16(out)
        }
        _ => {
            return Err(ADError::UnsupportedConversion(
                "rgb1_to_mono only supports UInt8 and UInt16".into(),
            ));
        }
    };

    let dims = vec![NDDimension::new(x), NDDimension::new(y)];
    let mut arr = NDArray::new(dims, src.data.data_type());
    arr.data = out_data;
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    arr.codec = src.codec.clone();
    Ok(arr)
}

/// Convert between RGB layout orders (RGB1 ↔ RGB2 ↔ RGB3).
/// RGB1: [color, x, y] — pixel-interleaved (RGBRGBRGB...)
/// RGB2: [x, color, y] — row-interleaved
/// RGB3: [x, y, color] — planar
pub fn convert_rgb_layout(
    src: &NDArray,
    src_mode: NDColorMode,
    dst_mode: NDColorMode,
) -> ADResult<NDArray> {
    if src.dims.len() != 3 {
        return Err(ADError::InvalidDimensions(
            "RGB conversion requires 3D input".into(),
        ));
    }

    // Determine x, y, color from source layout
    let (color, x, y) = match src_mode {
        NDColorMode::RGB1 => (src.dims[0].size, src.dims[1].size, src.dims[2].size),
        NDColorMode::RGB2 => (src.dims[1].size, src.dims[0].size, src.dims[2].size),
        NDColorMode::RGB3 => (src.dims[2].size, src.dims[0].size, src.dims[1].size),
        _ => return Err(ADError::UnsupportedConversion(
            format!("convert_rgb_layout: source mode {:?} not RGB", src_mode),
        )),
    };

    if color != 3 {
        return Err(ADError::InvalidDimensions(
            "RGB conversion requires color dimension = 3".into(),
        ));
    }

    // Build output dimensions
    let out_dims = match dst_mode {
        NDColorMode::RGB1 => vec![NDDimension::new(3), NDDimension::new(x), NDDimension::new(y)],
        NDColorMode::RGB2 => vec![NDDimension::new(x), NDDimension::new(3), NDDimension::new(y)],
        NDColorMode::RGB3 => vec![NDDimension::new(x), NDDimension::new(y), NDDimension::new(3)],
        _ => return Err(ADError::UnsupportedConversion(
            format!("convert_rgb_layout: target mode {:?} not RGB", dst_mode),
        )),
    };

    // Convert via generic index mapping
    let n = x * y;

    macro_rules! convert_layout {
        ($vec:expr, $T:ty) => {{
            let mut out = vec![<$T>::default(); n * 3];
            for iy in 0..y {
                for ix in 0..x {
                    for c in 0..3usize {
                        let src_idx = match src_mode {
                            NDColorMode::RGB1 => c + ix * 3 + iy * x * 3,
                            NDColorMode::RGB2 => ix + c * x + iy * x * 3,
                            NDColorMode::RGB3 => ix + iy * x + c * x * y,
                            _ => unreachable!(),
                        };
                        let dst_idx = match dst_mode {
                            NDColorMode::RGB1 => c + ix * 3 + iy * x * 3,
                            NDColorMode::RGB2 => ix + c * x + iy * x * 3,
                            NDColorMode::RGB3 => ix + iy * x + c * x * y,
                            _ => unreachable!(),
                        };
                        out[dst_idx] = $vec[src_idx];
                    }
                }
            }
            out
        }};
    }

    let out_data = match &src.data {
        NDDataBuffer::U8(v) => NDDataBuffer::U8(convert_layout!(v, u8)),
        NDDataBuffer::U16(v) => NDDataBuffer::U16(convert_layout!(v, u16)),
        NDDataBuffer::I8(v) => NDDataBuffer::I8(convert_layout!(v, i8)),
        NDDataBuffer::I16(v) => NDDataBuffer::I16(convert_layout!(v, i16)),
        NDDataBuffer::I32(v) => NDDataBuffer::I32(convert_layout!(v, i32)),
        NDDataBuffer::U32(v) => NDDataBuffer::U32(convert_layout!(v, u32)),
        NDDataBuffer::I64(v) => NDDataBuffer::I64(convert_layout!(v, i64)),
        NDDataBuffer::U64(v) => NDDataBuffer::U64(convert_layout!(v, u64)),
        NDDataBuffer::F32(v) => NDDataBuffer::F32(convert_layout!(v, f32)),
        NDDataBuffer::F64(v) => NDDataBuffer::F64(convert_layout!(v, f64)),
    };

    let mut arr = NDArray::new(out_dims, src.data.data_type());
    arr.data = out_data;
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    arr.codec = src.codec.clone();
    Ok(arr)
}

/// Convert NDArray data type with clamping.
pub fn convert_data_type(src: &NDArray, target_type: NDDataType) -> ADResult<NDArray> {
    if src.data.data_type() == target_type {
        return Ok(src.clone());
    }

    let n = src.data.len();
    let out_data = match target_type {
        NDDataType::UInt8 => {
            let mut out = vec![0u8; n];
            for i in 0..n {
                let v = src.data.get_as_f64(i).unwrap_or(0.0);
                out[i] = v.clamp(0.0, 255.0) as u8;
            }
            NDDataBuffer::U8(out)
        }
        NDDataType::UInt16 => {
            let mut out = vec![0u16; n];
            for i in 0..n {
                let v = src.data.get_as_f64(i).unwrap_or(0.0);
                out[i] = v.clamp(0.0, 65535.0) as u16;
            }
            NDDataBuffer::U16(out)
        }
        NDDataType::Int8 => {
            let mut out = vec![0i8; n];
            for i in 0..n {
                let v = src.data.get_as_f64(i).unwrap_or(0.0);
                out[i] = v.clamp(-128.0, 127.0) as i8;
            }
            NDDataBuffer::I8(out)
        }
        NDDataType::Int16 => {
            let mut out = vec![0i16; n];
            for i in 0..n {
                let v = src.data.get_as_f64(i).unwrap_or(0.0);
                out[i] = v.clamp(-32768.0, 32767.0) as i16;
            }
            NDDataBuffer::I16(out)
        }
        NDDataType::Int32 => {
            let mut out = vec![0i32; n];
            for i in 0..n {
                let v = src.data.get_as_f64(i).unwrap_or(0.0);
                out[i] = v as i32;
            }
            NDDataBuffer::I32(out)
        }
        NDDataType::UInt32 => {
            let mut out = vec![0u32; n];
            for i in 0..n {
                let v = src.data.get_as_f64(i).unwrap_or(0.0);
                out[i] = v.max(0.0) as u32;
            }
            NDDataBuffer::U32(out)
        }
        NDDataType::Int64 => {
            let mut out = vec![0i64; n];
            for i in 0..n {
                let v = src.data.get_as_f64(i).unwrap_or(0.0);
                out[i] = v as i64;
            }
            NDDataBuffer::I64(out)
        }
        NDDataType::UInt64 => {
            let mut out = vec![0u64; n];
            for i in 0..n {
                let v = src.data.get_as_f64(i).unwrap_or(0.0);
                out[i] = v.max(0.0) as u64;
            }
            NDDataBuffer::U64(out)
        }
        NDDataType::Float32 => {
            let mut out = vec![0f32; n];
            for i in 0..n {
                out[i] = src.data.get_as_f64(i).unwrap_or(0.0) as f32;
            }
            NDDataBuffer::F32(out)
        }
        NDDataType::Float64 => {
            let mut out = vec![0f64; n];
            for i in 0..n {
                out[i] = src.data.get_as_f64(i).unwrap_or(0.0);
            }
            NDDataBuffer::F64(out)
        }
    };

    let mut arr = NDArray::new(src.dims.clone(), target_type);
    arr.data = out_data;
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    arr.codec = src.codec.clone();
    Ok(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mono_to_rgb1() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 10; v[1] = 20; v[2] = 30; v[3] = 40;
        }
        let rgb = mono_to_rgb1(&arr).unwrap();
        assert_eq!(rgb.dims.len(), 3);
        assert_eq!(rgb.dims[0].size, 3);
        assert_eq!(rgb.dims[1].size, 2);
        assert_eq!(rgb.dims[2].size, 2);
        if let NDDataBuffer::U8(ref v) = rgb.data {
            // First pixel: R=10, G=10, B=10
            assert_eq!(v[0], 10);
            assert_eq!(v[1], 10);
            assert_eq!(v[2], 10);
            // Second pixel
            assert_eq!(v[3], 20);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_rgb1_to_mono() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(2), NDDimension::new(1)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // Pixel 0: R=255, G=0, B=0
            v[0] = 255; v[1] = 0; v[2] = 0;
            // Pixel 1: R=0, G=255, B=0
            v[3] = 0; v[4] = 255; v[5] = 0;
        }
        let mono = rgb1_to_mono(&arr).unwrap();
        assert_eq!(mono.dims.len(), 2);
        assert_eq!(mono.dims[0].size, 2);
        if let NDDataBuffer::U8(ref v) = mono.data {
            assert_eq!(v[0], 76);   // 0.299 * 255 ≈ 76
            assert_eq!(v[1], 150);  // 0.587 * 255 ≈ 150
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_rgb1_to_rgb2_to_rgb3() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(2), NDDimension::new(1)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // pixel 0: R=10, G=20, B=30; pixel 1: R=40, G=50, B=60
            v[0] = 10; v[1] = 20; v[2] = 30;
            v[3] = 40; v[4] = 50; v[5] = 60;
        }

        // RGB1 → RGB2
        let rgb2 = convert_rgb_layout(&arr, NDColorMode::RGB1, NDColorMode::RGB2).unwrap();
        assert_eq!(rgb2.dims[0].size, 2); // x
        assert_eq!(rgb2.dims[1].size, 3); // color
        assert_eq!(rgb2.dims[2].size, 1); // y

        // RGB2 → RGB3
        let rgb3 = convert_rgb_layout(&rgb2, NDColorMode::RGB2, NDColorMode::RGB3).unwrap();
        assert_eq!(rgb3.dims[0].size, 2); // x
        assert_eq!(rgb3.dims[1].size, 1); // y
        assert_eq!(rgb3.dims[2].size, 3); // color

        // RGB3 → RGB1 (roundtrip)
        let rgb1_back = convert_rgb_layout(&rgb3, NDColorMode::RGB3, NDColorMode::RGB1).unwrap();
        if let (NDDataBuffer::U8(ref orig), NDDataBuffer::U8(ref back)) = (&arr.data, &rgb1_back.data) {
            assert_eq!(orig, back);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_data_type_u8_to_u16() {
        let mut arr = NDArray::new(vec![NDDimension::new(3)], NDDataType::UInt8);
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 10; v[1] = 128; v[2] = 255;
        }
        let result = convert_data_type(&arr, NDDataType::UInt16).unwrap();
        assert_eq!(result.data.data_type(), NDDataType::UInt16);
        if let NDDataBuffer::U16(ref v) = result.data {
            assert_eq!(v[0], 10);
            assert_eq!(v[1], 128);
            assert_eq!(v[2], 255);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_data_type_f32_to_u8_clamp() {
        let mut arr = NDArray::new(vec![NDDimension::new(3)], NDDataType::Float32);
        if let NDDataBuffer::F32(ref mut v) = arr.data {
            v[0] = -10.0; v[1] = 128.7; v[2] = 300.0;
        }
        let result = convert_data_type(&arr, NDDataType::UInt8).unwrap();
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v[0], 0);    // clamped
            assert_eq!(v[1], 128);  // truncated
            assert_eq!(v[2], 255);  // clamped
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_data_type_same_type_noop() {
        let arr = NDArray::new(vec![NDDimension::new(5)], NDDataType::UInt8);
        let result = convert_data_type(&arr, NDDataType::UInt8).unwrap();
        assert_eq!(result.data.len(), 5);
        assert_eq!(result.data.data_type(), NDDataType::UInt8);
    }

    #[test]
    fn test_color_mode_from_i32() {
        assert_eq!(NDColorMode::from_i32(0), NDColorMode::Mono);
        assert_eq!(NDColorMode::from_i32(2), NDColorMode::RGB1);
        assert_eq!(NDColorMode::from_i32(99), NDColorMode::Mono);
    }

    #[test]
    fn test_mono_to_rgb1_u16() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(1)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            v[0] = 1000; v[1] = 2000;
        }
        let rgb = mono_to_rgb1(&arr).unwrap();
        if let NDDataBuffer::U16(ref v) = rgb.data {
            assert_eq!(v[0], 1000);
            assert_eq!(v[1], 1000);
            assert_eq!(v[2], 1000);
            assert_eq!(v[3], 2000);
        } else {
            panic!("wrong type");
        }
    }
}
