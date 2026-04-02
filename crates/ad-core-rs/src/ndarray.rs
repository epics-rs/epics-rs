use crate::attributes::NDAttributeList;
use crate::codec::Codec;
use crate::error::{ADError, ADResult};
use crate::timestamp::EpicsTimestamp;

/// NDArray data types matching areaDetector NDDataType_t.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NDDataType {
    Int8 = 0,
    UInt8 = 1,
    Int16 = 2,
    UInt16 = 3,
    Int32 = 4,
    UInt32 = 5,
    Int64 = 6,
    UInt64 = 7,
    Float32 = 8,
    Float64 = 9,
}

impl NDDataType {
    pub fn element_size(&self) -> usize {
        match self {
            Self::Int8 | Self::UInt8 => 1,
            Self::Int16 | Self::UInt16 => 2,
            Self::Int32 | Self::UInt32 | Self::Float32 => 4,
            Self::Int64 | Self::UInt64 | Self::Float64 => 8,
        }
    }

    pub fn from_ordinal(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Int8),
            1 => Some(Self::UInt8),
            2 => Some(Self::Int16),
            3 => Some(Self::UInt16),
            4 => Some(Self::Int32),
            5 => Some(Self::UInt32),
            6 => Some(Self::Int64),
            7 => Some(Self::UInt64),
            8 => Some(Self::Float32),
            9 => Some(Self::Float64),
            _ => None,
        }
    }
}

/// Typed buffer for NDArray data.
#[derive(Debug, Clone)]
pub enum NDDataBuffer {
    I8(Vec<i8>),
    U8(Vec<u8>),
    I16(Vec<i16>),
    U16(Vec<u16>),
    I32(Vec<i32>),
    U32(Vec<u32>),
    I64(Vec<i64>),
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl NDDataBuffer {
    pub fn zeros(data_type: NDDataType, count: usize) -> Self {
        match data_type {
            NDDataType::Int8 => Self::I8(vec![0; count]),
            NDDataType::UInt8 => Self::U8(vec![0; count]),
            NDDataType::Int16 => Self::I16(vec![0; count]),
            NDDataType::UInt16 => Self::U16(vec![0; count]),
            NDDataType::Int32 => Self::I32(vec![0; count]),
            NDDataType::UInt32 => Self::U32(vec![0; count]),
            NDDataType::Int64 => Self::I64(vec![0; count]),
            NDDataType::UInt64 => Self::U64(vec![0; count]),
            NDDataType::Float32 => Self::F32(vec![0.0; count]),
            NDDataType::Float64 => Self::F64(vec![0.0; count]),
        }
    }

    pub fn data_type(&self) -> NDDataType {
        match self {
            Self::I8(_) => NDDataType::Int8,
            Self::U8(_) => NDDataType::UInt8,
            Self::I16(_) => NDDataType::Int16,
            Self::U16(_) => NDDataType::UInt16,
            Self::I32(_) => NDDataType::Int32,
            Self::U32(_) => NDDataType::UInt32,
            Self::I64(_) => NDDataType::Int64,
            Self::U64(_) => NDDataType::UInt64,
            Self::F32(_) => NDDataType::Float32,
            Self::F64(_) => NDDataType::Float64,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::I8(v) => v.len(),
            Self::U8(v) => v.len(),
            Self::I16(v) => v.len(),
            Self::U16(v) => v.len(),
            Self::I32(v) => v.len(),
            Self::U32(v) => v.len(),
            Self::I64(v) => v.len(),
            Self::U64(v) => v.len(),
            Self::F32(v) => v.len(),
            Self::F64(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn total_bytes(&self) -> usize {
        self.len() * self.data_type().element_size()
    }

    /// Capacity of the underlying Vec in bytes.
    pub fn capacity_bytes(&self) -> usize {
        let cap = match self {
            Self::I8(v) => v.capacity(),
            Self::U8(v) => v.capacity(),
            Self::I16(v) => v.capacity(),
            Self::U16(v) => v.capacity(),
            Self::I32(v) => v.capacity(),
            Self::U32(v) => v.capacity(),
            Self::I64(v) => v.capacity(),
            Self::U64(v) => v.capacity(),
            Self::F32(v) => v.capacity(),
            Self::F64(v) => v.capacity(),
        };
        cap * self.data_type().element_size()
    }

    /// Resize the buffer, zeroing new elements if growing.
    pub fn resize(&mut self, new_len: usize) {
        match self {
            Self::I8(v) => v.resize(new_len, 0),
            Self::U8(v) => v.resize(new_len, 0),
            Self::I16(v) => v.resize(new_len, 0),
            Self::U16(v) => v.resize(new_len, 0),
            Self::I32(v) => v.resize(new_len, 0),
            Self::U32(v) => v.resize(new_len, 0),
            Self::I64(v) => v.resize(new_len, 0),
            Self::U64(v) => v.resize(new_len, 0),
            Self::F32(v) => v.resize(new_len, 0.0),
            Self::F64(v) => v.resize(new_len, 0.0),
        }
    }

    /// View the underlying data as a byte slice.
    pub fn as_u8_slice(&self) -> &[u8] {
        match self {
            Self::I8(v) => unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len()) },
            Self::U8(v) => v.as_slice(),
            Self::I16(v) => unsafe {
                std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2)
            },
            Self::U16(v) => unsafe {
                std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2)
            },
            Self::I32(v) => unsafe {
                std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4)
            },
            Self::U32(v) => unsafe {
                std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4)
            },
            Self::I64(v) => unsafe {
                std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8)
            },
            Self::U64(v) => unsafe {
                std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8)
            },
            Self::F32(v) => unsafe {
                std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4)
            },
            Self::F64(v) => unsafe {
                std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8)
            },
        }
    }

    /// Get element at index as f64.
    pub fn get_as_f64(&self, index: usize) -> Option<f64> {
        match self {
            Self::I8(v) => v.get(index).map(|&x| x as f64),
            Self::U8(v) => v.get(index).map(|&x| x as f64),
            Self::I16(v) => v.get(index).map(|&x| x as f64),
            Self::U16(v) => v.get(index).map(|&x| x as f64),
            Self::I32(v) => v.get(index).map(|&x| x as f64),
            Self::U32(v) => v.get(index).map(|&x| x as f64),
            Self::I64(v) => v.get(index).map(|&x| x as f64),
            Self::U64(v) => v.get(index).map(|&x| x as f64),
            Self::F32(v) => v.get(index).map(|&x| x as f64),
            Self::F64(v) => v.get(index).copied(),
        }
    }

    /// Set element at index from f64 value.
    pub fn set_from_f64(&mut self, index: usize, value: f64) {
        match self {
            Self::I8(v) => {
                if let Some(e) = v.get_mut(index) {
                    *e = value as i8;
                }
            }
            Self::U8(v) => {
                if let Some(e) = v.get_mut(index) {
                    *e = value as u8;
                }
            }
            Self::I16(v) => {
                if let Some(e) = v.get_mut(index) {
                    *e = value as i16;
                }
            }
            Self::U16(v) => {
                if let Some(e) = v.get_mut(index) {
                    *e = value as u16;
                }
            }
            Self::I32(v) => {
                if let Some(e) = v.get_mut(index) {
                    *e = value as i32;
                }
            }
            Self::U32(v) => {
                if let Some(e) = v.get_mut(index) {
                    *e = value as u32;
                }
            }
            Self::I64(v) => {
                if let Some(e) = v.get_mut(index) {
                    *e = value as i64;
                }
            }
            Self::U64(v) => {
                if let Some(e) = v.get_mut(index) {
                    *e = value as u64;
                }
            }
            Self::F32(v) => {
                if let Some(e) = v.get_mut(index) {
                    *e = value as f32;
                }
            }
            Self::F64(v) => {
                if let Some(e) = v.get_mut(index) {
                    *e = value;
                }
            }
        }
    }
}

/// A single dimension of an NDArray.
#[derive(Debug, Clone)]
pub struct NDDimension {
    pub size: usize,
    pub offset: usize,
    pub binning: usize,
    pub reverse: bool,
}

impl NDDimension {
    pub fn new(size: usize) -> Self {
        Self {
            size,
            offset: 0,
            binning: 1,
            reverse: false,
        }
    }
}

/// Computed info about an NDArray's layout.
#[derive(Debug, Clone)]
pub struct NDArrayInfo {
    pub total_bytes: usize,
    pub bytes_per_element: usize,
    pub num_elements: usize,
    pub x_size: usize,
    pub y_size: usize,
    pub color_size: usize,
}

/// N-dimensional array with typed data buffer.
#[derive(Debug, Clone)]
pub struct NDArray {
    pub unique_id: i32,
    pub timestamp: EpicsTimestamp,
    pub dims: Vec<NDDimension>,
    pub data: NDDataBuffer,
    pub attributes: NDAttributeList,
    pub codec: Option<Codec>,
}

impl NDArray {
    /// Create a new NDArray with zeroed buffer matching dimensions.
    pub fn new(dims: Vec<NDDimension>, data_type: NDDataType) -> Self {
        let num_elements: usize = dims.iter().map(|d| d.size).product();
        Self {
            unique_id: 0,
            timestamp: EpicsTimestamp::default(),
            dims,
            data: NDDataBuffer::zeros(data_type, num_elements),
            attributes: NDAttributeList::new(),
            codec: None,
        }
    }

    /// Compute layout info for this array.
    pub fn info(&self) -> NDArrayInfo {
        let bytes_per_element = self.data.data_type().element_size();
        let num_elements = self.data.len();
        let total_bytes = num_elements * bytes_per_element;

        let ndims = self.dims.len();
        let (x_size, y_size, color_size) = match ndims {
            0 => (0, 0, 0),
            1 => (self.dims[0].size, 1, 1),
            2 => (self.dims[0].size, self.dims[1].size, 1),
            _ => {
                // 3D: interpret as color image
                // NDColorMode convention: dim[0]=color for RGB1
                (self.dims[1].size, self.dims[2].size, self.dims[0].size)
            }
        };

        NDArrayInfo {
            total_bytes,
            bytes_per_element,
            num_elements,
            x_size,
            y_size,
            color_size,
        }
    }

    /// Validate that buffer length matches dimension product.
    pub fn validate(&self) -> ADResult<()> {
        let expected: usize = if self.dims.is_empty() {
            0
        } else {
            self.dims.iter().map(|d| d.size).product()
        };
        if self.data.len() != expected {
            return Err(ADError::BufferSizeMismatch {
                expected,
                actual: self.data.len(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_element_size_all_types() {
        assert_eq!(NDDataType::Int8.element_size(), 1);
        assert_eq!(NDDataType::UInt8.element_size(), 1);
        assert_eq!(NDDataType::Int16.element_size(), 2);
        assert_eq!(NDDataType::UInt16.element_size(), 2);
        assert_eq!(NDDataType::Int32.element_size(), 4);
        assert_eq!(NDDataType::UInt32.element_size(), 4);
        assert_eq!(NDDataType::Int64.element_size(), 8);
        assert_eq!(NDDataType::UInt64.element_size(), 8);
        assert_eq!(NDDataType::Float32.element_size(), 4);
        assert_eq!(NDDataType::Float64.element_size(), 8);
    }

    #[test]
    fn test_from_ordinal_roundtrip() {
        for i in 0..10u8 {
            let dt = NDDataType::from_ordinal(i).unwrap();
            assert_eq!(dt as u8, i);
        }
        assert!(NDDataType::from_ordinal(10).is_none());
    }

    #[test]
    fn test_buffer_zeros_type_and_len() {
        let buf = NDDataBuffer::zeros(NDDataType::UInt16, 100);
        assert_eq!(buf.data_type(), NDDataType::UInt16);
        assert_eq!(buf.len(), 100);
        assert_eq!(buf.total_bytes(), 200);
    }

    #[test]
    fn test_buffer_zeros_all_types() {
        for i in 0..10u8 {
            let dt = NDDataType::from_ordinal(i).unwrap();
            let buf = NDDataBuffer::zeros(dt, 10);
            assert_eq!(buf.data_type(), dt);
            assert_eq!(buf.len(), 10);
            assert_eq!(buf.total_bytes(), 10 * dt.element_size());
        }
    }

    #[test]
    fn test_buffer_as_u8_slice() {
        let buf = NDDataBuffer::U8(vec![1, 2, 3]);
        assert_eq!(buf.as_u8_slice(), &[1, 2, 3]);
    }

    #[test]
    fn test_ndarray_new_allocates() {
        let dims = vec![NDDimension::new(256), NDDimension::new(256)];
        let arr = NDArray::new(dims, NDDataType::UInt8);
        assert_eq!(arr.data.len(), 256 * 256);
        assert_eq!(arr.data.data_type(), NDDataType::UInt8);
    }

    #[test]
    fn test_ndarray_validate_ok() {
        let dims = vec![NDDimension::new(10), NDDimension::new(20)];
        let arr = NDArray::new(dims, NDDataType::Float64);
        arr.validate().unwrap();
    }

    #[test]
    fn test_ndarray_validate_mismatch() {
        let mut arr = NDArray::new(
            vec![NDDimension::new(10), NDDimension::new(20)],
            NDDataType::UInt8,
        );
        arr.data = NDDataBuffer::U8(vec![0; 100]);
        assert!(arr.validate().is_err());
    }

    #[test]
    fn test_ndarray_info_2d_mono() {
        let dims = vec![NDDimension::new(640), NDDimension::new(480)];
        let arr = NDArray::new(dims, NDDataType::UInt16);
        let info = arr.info();
        assert_eq!(info.x_size, 640);
        assert_eq!(info.y_size, 480);
        assert_eq!(info.color_size, 1);
        assert_eq!(info.num_elements, 640 * 480);
        assert_eq!(info.bytes_per_element, 2);
        assert_eq!(info.total_bytes, 640 * 480 * 2);
    }

    #[test]
    fn test_ndarray_info_3d_rgb() {
        let dims = vec![
            NDDimension::new(3),
            NDDimension::new(640),
            NDDimension::new(480),
        ];
        let arr = NDArray::new(dims, NDDataType::UInt8);
        let info = arr.info();
        assert_eq!(info.color_size, 3);
        assert_eq!(info.x_size, 640);
        assert_eq!(info.y_size, 480);
        assert_eq!(info.num_elements, 3 * 640 * 480);
    }

    #[test]
    fn test_ndarray_info_1d() {
        let dims = vec![NDDimension::new(1024)];
        let arr = NDArray::new(dims, NDDataType::Float64);
        let info = arr.info();
        assert_eq!(info.x_size, 1024);
        assert_eq!(info.y_size, 1);
        assert_eq!(info.color_size, 1);
    }

    #[test]
    fn test_buffer_is_empty() {
        let buf = NDDataBuffer::zeros(NDDataType::UInt8, 0);
        assert!(buf.is_empty());
        let buf2 = NDDataBuffer::zeros(NDDataType::UInt8, 1);
        assert!(!buf2.is_empty());
    }

    #[test]
    fn test_codec_field_preserved() {
        let mut arr = NDArray::new(vec![NDDimension::new(10)], NDDataType::UInt8);
        arr.codec = Some(Codec {
            name: crate::codec::CodecName::JPEG,
            compressed_size: 42,
        });
        let cloned = arr.clone();
        assert_eq!(cloned.codec.as_ref().unwrap().compressed_size, 42);
    }
}
