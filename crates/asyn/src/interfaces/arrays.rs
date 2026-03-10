use crate::error::AsynResult;
use crate::user::AsynUser;

/// Generic array I/O interface (asynXxxArray equivalents).
pub trait AsynFloat64Array: Send + Sync {
    fn read_float64_array(&mut self, user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize>;
    fn write_float64_array(&mut self, user: &AsynUser, data: &[f64]) -> AsynResult<()>;
}

pub trait AsynInt32Array: Send + Sync {
    fn read_int32_array(&mut self, user: &AsynUser, buf: &mut [i32]) -> AsynResult<usize>;
    fn write_int32_array(&mut self, user: &AsynUser, data: &[i32]) -> AsynResult<()>;
}

pub trait AsynInt8Array: Send + Sync {
    fn read_int8_array(&mut self, user: &AsynUser, buf: &mut [i8]) -> AsynResult<usize>;
    fn write_int8_array(&mut self, user: &AsynUser, data: &[i8]) -> AsynResult<()>;
}

pub trait AsynInt16Array: Send + Sync {
    fn read_int16_array(&mut self, user: &AsynUser, buf: &mut [i16]) -> AsynResult<usize>;
    fn write_int16_array(&mut self, user: &AsynUser, data: &[i16]) -> AsynResult<()>;
}

pub trait AsynInt64Array: Send + Sync {
    fn read_int64_array(&mut self, user: &AsynUser, buf: &mut [i64]) -> AsynResult<usize>;
    fn write_int64_array(&mut self, user: &AsynUser, data: &[i64]) -> AsynResult<()>;
}

pub trait AsynFloat32Array: Send + Sync {
    fn read_float32_array(&mut self, user: &AsynUser, buf: &mut [f32]) -> AsynResult<usize>;
    fn write_float32_array(&mut self, user: &AsynUser, data: &[f32]) -> AsynResult<()>;
}
