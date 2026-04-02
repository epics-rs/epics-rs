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
        _ => {
            return Err(ADError::UnsupportedConversion(format!(
                "convert_rgb_layout: source mode {:?} not RGB",
                src_mode
            )));
        }
    };

    if color != 3 {
        return Err(ADError::InvalidDimensions(
            "RGB conversion requires color dimension = 3".into(),
        ));
    }

    // Build output dimensions
    let out_dims = match dst_mode {
        NDColorMode::RGB1 => vec![
            NDDimension::new(3),
            NDDimension::new(x),
            NDDimension::new(y),
        ],
        NDColorMode::RGB2 => vec![
            NDDimension::new(x),
            NDDimension::new(3),
            NDDimension::new(y),
        ],
        NDColorMode::RGB3 => vec![
            NDDimension::new(x),
            NDDimension::new(y),
            NDDimension::new(3),
        ],
        _ => {
            return Err(ADError::UnsupportedConversion(format!(
                "convert_rgb_layout: target mode {:?} not RGB",
                dst_mode
            )));
        }
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

/// Convert RGB1 to YUV444 using BT.601 coefficients.
/// Input: RGB1 `[3, x, y]`, Output: YUV444 `[3, x, y]`
pub fn rgb1_to_yuv444(src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 3 || src.dims[0].size != 3 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_yuv444 requires 3D input with dims[0]=3".into(),
        ));
    }
    let x = src.dims[1].size;
    let y = src.dims[2].size;
    let n = x * y;

    let out_data = match &src.data {
        NDDataBuffer::U8(v) => {
            let mut out = vec![0u8; n * 3];
            for i in 0..n {
                let r = v[i * 3] as f64;
                let g = v[i * 3 + 1] as f64;
                let b = v[i * 3 + 2] as f64;
                let y_val = 0.299 * r + 0.587 * g + 0.114 * b;
                let cb = -0.169 * r - 0.331 * g + 0.5 * b + 128.0;
                let cr = 0.5 * r - 0.419 * g - 0.081 * b + 128.0;
                out[i * 3] = y_val.round().clamp(0.0, 255.0) as u8;
                out[i * 3 + 1] = cb.round().clamp(0.0, 255.0) as u8;
                out[i * 3 + 2] = cr.round().clamp(0.0, 255.0) as u8;
            }
            NDDataBuffer::U8(out)
        }
        NDDataBuffer::U16(v) => {
            let mut out = vec![0u16; n * 3];
            for i in 0..n {
                let r = v[i * 3] as f64;
                let g = v[i * 3 + 1] as f64;
                let b = v[i * 3 + 2] as f64;
                let y_val = 0.299 * r + 0.587 * g + 0.114 * b;
                let cb = -0.169 * r - 0.331 * g + 0.5 * b + 32768.0;
                let cr = 0.5 * r - 0.419 * g - 0.081 * b + 32768.0;
                out[i * 3] = y_val.round().clamp(0.0, 65535.0) as u16;
                out[i * 3 + 1] = cb.round().clamp(0.0, 65535.0) as u16;
                out[i * 3 + 2] = cr.round().clamp(0.0, 65535.0) as u16;
            }
            NDDataBuffer::U16(out)
        }
        _ => {
            return Err(ADError::UnsupportedConversion(
                "rgb1_to_yuv444 only supports UInt8 and UInt16".into(),
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

/// Convert YUV444 to RGB1 using inverse BT.601.
/// Input: YUV444 `[3, x, y]`, Output: RGB1 `[3, x, y]`
pub fn yuv444_to_rgb1(src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 3 || src.dims[0].size != 3 {
        return Err(ADError::InvalidDimensions(
            "yuv444_to_rgb1 requires 3D input with dims[0]=3".into(),
        ));
    }
    let x = src.dims[1].size;
    let y = src.dims[2].size;
    let n = x * y;

    let out_data = match &src.data {
        NDDataBuffer::U8(v) => {
            let mut out = vec![0u8; n * 3];
            for i in 0..n {
                let y_val = v[i * 3] as f64;
                let cb = v[i * 3 + 1] as f64 - 128.0;
                let cr = v[i * 3 + 2] as f64 - 128.0;
                let r = y_val + 1.402 * cr;
                let g = y_val - 0.344 * cb - 0.714 * cr;
                let b = y_val + 1.772 * cb;
                out[i * 3] = r.round().clamp(0.0, 255.0) as u8;
                out[i * 3 + 1] = g.round().clamp(0.0, 255.0) as u8;
                out[i * 3 + 2] = b.round().clamp(0.0, 255.0) as u8;
            }
            NDDataBuffer::U8(out)
        }
        NDDataBuffer::U16(v) => {
            let mut out = vec![0u16; n * 3];
            for i in 0..n {
                let y_val = v[i * 3] as f64;
                let cb = v[i * 3 + 1] as f64 - 32768.0;
                let cr = v[i * 3 + 2] as f64 - 32768.0;
                let r = y_val + 1.402 * cr;
                let g = y_val - 0.344 * cb - 0.714 * cr;
                let b = y_val + 1.772 * cb;
                out[i * 3] = r.round().clamp(0.0, 65535.0) as u16;
                out[i * 3 + 1] = g.round().clamp(0.0, 65535.0) as u16;
                out[i * 3 + 2] = b.round().clamp(0.0, 65535.0) as u16;
            }
            NDDataBuffer::U16(out)
        }
        _ => {
            return Err(ADError::UnsupportedConversion(
                "yuv444_to_rgb1 only supports UInt8 and UInt16".into(),
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

/// Convert RGB1 to YUV422 packed format (UYVY byte order).
/// Input: RGB1 `[3, x, y]`, Output: packed `[x*2, y]` as UInt8.
/// Width (x) must be even.
pub fn rgb1_to_yuv422(src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 3 || src.dims[0].size != 3 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_yuv422 requires 3D input with dims[0]=3".into(),
        ));
    }
    let x = src.dims[1].size;
    let y = src.dims[2].size;
    if x % 2 != 0 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_yuv422 requires even width".into(),
        ));
    }

    let v = match &src.data {
        NDDataBuffer::U8(v) => v,
        _ => {
            return Err(ADError::UnsupportedConversion(
                "rgb1_to_yuv422 only supports UInt8".into(),
            ));
        }
    };

    let packed_x = x * 2;
    let mut out = vec![0u8; packed_x * y];

    for iy in 0..y {
        for pair in 0..(x / 2) {
            let i0 = (iy * x + pair * 2) * 3;
            let i1 = i0 + 3;
            let r0 = v[i0] as f64;
            let g0 = v[i0 + 1] as f64;
            let b0 = v[i0 + 2] as f64;
            let r1 = v[i1] as f64;
            let g1 = v[i1 + 1] as f64;
            let b1 = v[i1 + 2] as f64;

            let y0 = (0.299 * r0 + 0.587 * g0 + 0.114 * b0)
                .round()
                .clamp(0.0, 255.0) as u8;
            let y1 = (0.299 * r1 + 0.587 * g1 + 0.114 * b1)
                .round()
                .clamp(0.0, 255.0) as u8;
            let cb0 = -0.169 * r0 - 0.331 * g0 + 0.5 * b0 + 128.0;
            let cb1 = -0.169 * r1 - 0.331 * g1 + 0.5 * b1 + 128.0;
            let cr0 = 0.5 * r0 - 0.419 * g0 - 0.081 * b0 + 128.0;
            let cr1 = 0.5 * r1 - 0.419 * g1 - 0.081 * b1 + 128.0;
            let u = ((cb0 + cb1) / 2.0).round().clamp(0.0, 255.0) as u8;
            let v_ch = ((cr0 + cr1) / 2.0).round().clamp(0.0, 255.0) as u8;

            let oi = iy * packed_x + pair * 4;
            out[oi] = u;
            out[oi + 1] = y0;
            out[oi + 2] = v_ch;
            out[oi + 3] = y1;
        }
    }

    let dims = vec![NDDimension::new(packed_x), NDDimension::new(y)];
    let mut arr = NDArray::new(dims, NDDataType::UInt8);
    arr.data = NDDataBuffer::U8(out);
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    arr.codec = src.codec.clone();
    Ok(arr)
}

/// Convert YUV422 packed format (UYVY) to RGB1.
/// Input: packed `[packed_x, y]` as UInt8, Output: RGB1 `[3, packed_x/2, y]`.
/// packed_x must be divisible by 4.
pub fn yuv422_to_rgb1(src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 2 {
        return Err(ADError::InvalidDimensions(
            "yuv422_to_rgb1 requires 2D input".into(),
        ));
    }
    let packed_x = src.dims[0].size;
    let y = src.dims[1].size;
    if packed_x % 4 != 0 {
        return Err(ADError::InvalidDimensions(
            "yuv422_to_rgb1 requires packed_x divisible by 4".into(),
        ));
    }

    let v = match &src.data {
        NDDataBuffer::U8(v) => v,
        _ => {
            return Err(ADError::UnsupportedConversion(
                "yuv422_to_rgb1 only supports UInt8".into(),
            ));
        }
    };

    let width = packed_x / 2;
    let n = width * y;
    let mut out = vec![0u8; n * 3];

    for iy in 0..y {
        for pair in 0..(width / 2) {
            let si = iy * packed_x + pair * 4;
            let u = v[si] as f64 - 128.0;
            let y0 = v[si + 1] as f64;
            let v_ch = v[si + 2] as f64 - 128.0;
            let y1 = v[si + 3] as f64;

            let oi0 = (iy * width + pair * 2) * 3;
            let oi1 = oi0 + 3;

            out[oi0] = (y0 + 1.402 * v_ch).round().clamp(0.0, 255.0) as u8;
            out[oi0 + 1] = (y0 - 0.344 * u - 0.714 * v_ch).round().clamp(0.0, 255.0) as u8;
            out[oi0 + 2] = (y0 + 1.772 * u).round().clamp(0.0, 255.0) as u8;

            out[oi1] = (y1 + 1.402 * v_ch).round().clamp(0.0, 255.0) as u8;
            out[oi1 + 1] = (y1 - 0.344 * u - 0.714 * v_ch).round().clamp(0.0, 255.0) as u8;
            out[oi1 + 2] = (y1 + 1.772 * u).round().clamp(0.0, 255.0) as u8;
        }
    }

    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(width),
        NDDimension::new(y),
    ];
    let mut arr = NDArray::new(dims, NDDataType::UInt8);
    arr.data = NDDataBuffer::U8(out);
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    arr.codec = src.codec.clone();
    Ok(arr)
}

/// Convert RGB1 to YUV411 packed format (UYYVYY byte order).
/// Input: RGB1 `[3, x, y]`, Output: packed `[x*3/2, y]` as UInt8.
/// Width (x) must be divisible by 4.
pub fn rgb1_to_yuv411(src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 3 || src.dims[0].size != 3 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_yuv411 requires 3D input with dims[0]=3".into(),
        ));
    }
    let x = src.dims[1].size;
    let y = src.dims[2].size;
    if x % 4 != 0 {
        return Err(ADError::InvalidDimensions(
            "rgb1_to_yuv411 requires width divisible by 4".into(),
        ));
    }

    let v = match &src.data {
        NDDataBuffer::U8(v) => v,
        _ => {
            return Err(ADError::UnsupportedConversion(
                "rgb1_to_yuv411 only supports UInt8".into(),
            ));
        }
    };

    let packed_x = x * 3 / 2;
    let mut out = vec![0u8; packed_x * y];

    for iy in 0..y {
        for group in 0..(x / 4) {
            let base = (iy * x + group * 4) * 3;
            let mut cbs = [0.0f64; 4];
            let mut crs = [0.0f64; 4];
            let mut ys = [0u8; 4];

            for p in 0..4 {
                let pi = base + p * 3;
                let r = v[pi] as f64;
                let g = v[pi + 1] as f64;
                let b = v[pi + 2] as f64;
                ys[p] = (0.299 * r + 0.587 * g + 0.114 * b)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                cbs[p] = -0.169 * r - 0.331 * g + 0.5 * b + 128.0;
                crs[p] = 0.5 * r - 0.419 * g - 0.081 * b + 128.0;
            }

            let u = ((cbs[0] + cbs[1] + cbs[2] + cbs[3]) / 4.0)
                .round()
                .clamp(0.0, 255.0) as u8;
            let v_ch = ((crs[0] + crs[1] + crs[2] + crs[3]) / 4.0)
                .round()
                .clamp(0.0, 255.0) as u8;

            let oi = iy * packed_x + group * 6;
            out[oi] = u;
            out[oi + 1] = ys[0];
            out[oi + 2] = ys[1];
            out[oi + 3] = v_ch;
            out[oi + 4] = ys[2];
            out[oi + 5] = ys[3];
        }
    }

    let dims = vec![NDDimension::new(packed_x), NDDimension::new(y)];
    let mut arr = NDArray::new(dims, NDDataType::UInt8);
    arr.data = NDDataBuffer::U8(out);
    arr.unique_id = src.unique_id;
    arr.timestamp = src.timestamp;
    arr.attributes = src.attributes.clone();
    arr.codec = src.codec.clone();
    Ok(arr)
}

/// Convert YUV411 packed format (UYYVYY) to RGB1.
/// Input: packed `[packed_x, y]` as UInt8, Output: RGB1 `[3, packed_x*2/3, y]`.
/// packed_x must be divisible by 6.
pub fn yuv411_to_rgb1(src: &NDArray) -> ADResult<NDArray> {
    if src.dims.len() != 2 {
        return Err(ADError::InvalidDimensions(
            "yuv411_to_rgb1 requires 2D input".into(),
        ));
    }
    let packed_x = src.dims[0].size;
    let y = src.dims[1].size;
    if packed_x % 6 != 0 {
        return Err(ADError::InvalidDimensions(
            "yuv411_to_rgb1 requires packed_x divisible by 6".into(),
        ));
    }

    let v = match &src.data {
        NDDataBuffer::U8(v) => v,
        _ => {
            return Err(ADError::UnsupportedConversion(
                "yuv411_to_rgb1 only supports UInt8".into(),
            ));
        }
    };

    let width = packed_x * 2 / 3;
    let n = width * y;
    let mut out = vec![0u8; n * 3];

    for iy in 0..y {
        for group in 0..(width / 4) {
            let si = iy * packed_x + group * 6;
            let u = v[si] as f64 - 128.0;
            let y0 = v[si + 1] as f64;
            let y1 = v[si + 2] as f64;
            let v_ch = v[si + 3] as f64 - 128.0;
            let y2 = v[si + 4] as f64;
            let y3 = v[si + 5] as f64;

            for (p, y_val) in [(0, y0), (1, y1), (2, y2), (3, y3)] {
                let oi = (iy * width + group * 4 + p) * 3;
                out[oi] = (y_val + 1.402 * v_ch).round().clamp(0.0, 255.0) as u8;
                out[oi + 1] = (y_val - 0.344 * u - 0.714 * v_ch).round().clamp(0.0, 255.0) as u8;
                out[oi + 2] = (y_val + 1.772 * u).round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    let dims = vec![
        NDDimension::new(3),
        NDDimension::new(width),
        NDDimension::new(y),
    ];
    let mut arr = NDArray::new(dims, NDDataType::UInt8);
    arr.data = NDDataBuffer::U8(out);
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
            v[0] = 10;
            v[1] = 20;
            v[2] = 30;
            v[3] = 40;
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
            vec![
                NDDimension::new(3),
                NDDimension::new(2),
                NDDimension::new(1),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // Pixel 0: R=255, G=0, B=0
            v[0] = 255;
            v[1] = 0;
            v[2] = 0;
            // Pixel 1: R=0, G=255, B=0
            v[3] = 0;
            v[4] = 255;
            v[5] = 0;
        }
        let mono = rgb1_to_mono(&arr).unwrap();
        assert_eq!(mono.dims.len(), 2);
        assert_eq!(mono.dims[0].size, 2);
        if let NDDataBuffer::U8(ref v) = mono.data {
            assert_eq!(v[0], 76); // 0.299 * 255 ≈ 76
            assert_eq!(v[1], 150); // 0.587 * 255 ≈ 150
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_rgb1_to_rgb2_to_rgb3() {
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(2),
                NDDimension::new(1),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            // pixel 0: R=10, G=20, B=30; pixel 1: R=40, G=50, B=60
            v[0] = 10;
            v[1] = 20;
            v[2] = 30;
            v[3] = 40;
            v[4] = 50;
            v[5] = 60;
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
        if let (NDDataBuffer::U8(orig), NDDataBuffer::U8(back)) = (&arr.data, &rgb1_back.data) {
            assert_eq!(orig, back);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_data_type_u8_to_u16() {
        let mut arr = NDArray::new(vec![NDDimension::new(3)], NDDataType::UInt8);
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 10;
            v[1] = 128;
            v[2] = 255;
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
            v[0] = -10.0;
            v[1] = 128.7;
            v[2] = 300.0;
        }
        let result = convert_data_type(&arr, NDDataType::UInt8).unwrap();
        if let NDDataBuffer::U8(ref v) = result.data {
            assert_eq!(v[0], 0); // clamped
            assert_eq!(v[1], 128); // truncated
            assert_eq!(v[2], 255); // clamped
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
    fn test_rgb1_to_yuv444_roundtrip() {
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            v[0] = 100;
            v[1] = 150;
            v[2] = 200;
            v[3] = 50;
            v[4] = 100;
            v[5] = 50;
            v[6] = 255;
            v[7] = 0;
            v[8] = 0;
            v[9] = 0;
            v[10] = 255;
            v[11] = 0;
        }
        let yuv = rgb1_to_yuv444(&arr).unwrap();
        assert_eq!(yuv.dims.len(), 3);
        assert_eq!(yuv.dims[0].size, 3);

        let back = yuv444_to_rgb1(&yuv).unwrap();
        if let (NDDataBuffer::U8(orig), NDDataBuffer::U8(result)) = (&arr.data, &back.data) {
            for i in 0..orig.len() {
                assert!(
                    (orig[i] as i16 - result[i] as i16).unsigned_abs() <= 2,
                    "pixel diff at {}: orig={}, result={}",
                    i,
                    orig[i],
                    result[i],
                );
            }
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_rgb1_to_yuv422_roundtrip() {
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            let colors: [u8; 24] = [
                100, 150, 200, 50, 100, 50, 255, 0, 0, 0, 255, 0, 128, 128, 128, 200, 100, 50, 0,
                0, 255, 255, 255, 0,
            ];
            v[..24].copy_from_slice(&colors);
        }
        let yuv = rgb1_to_yuv422(&arr).unwrap();
        assert_eq!(yuv.dims.len(), 2);
        assert_eq!(yuv.dims[0].size, 8);

        let back = yuv422_to_rgb1(&yuv).unwrap();
        assert_eq!(back.dims[0].size, 3);
        assert_eq!(back.dims[1].size, 4);
        assert_eq!(back.dims[2].size, 2);
    }

    #[test]
    fn test_rgb1_to_yuv411_roundtrip() {
        let mut arr = NDArray::new(
            vec![
                NDDimension::new(3),
                NDDimension::new(4),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            let colors: [u8; 24] = [
                100, 150, 200, 50, 100, 50, 255, 0, 0, 0, 255, 0, 128, 128, 128, 200, 100, 50, 0,
                0, 255, 255, 255, 0,
            ];
            v[..24].copy_from_slice(&colors);
        }
        let yuv = rgb1_to_yuv411(&arr).unwrap();
        assert_eq!(yuv.dims.len(), 2);
        assert_eq!(yuv.dims[0].size, 6);

        let back = yuv411_to_rgb1(&yuv).unwrap();
        assert_eq!(back.dims[0].size, 3);
        assert_eq!(back.dims[1].size, 4);
        assert_eq!(back.dims[2].size, 2);
    }

    #[test]
    fn test_mono_to_rgb1_u16() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(1)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = arr.data {
            v[0] = 1000;
            v[1] = 2000;
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
