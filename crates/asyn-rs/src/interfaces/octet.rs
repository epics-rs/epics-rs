use crate::error::AsynResult;
use crate::user::AsynUser;

/// Byte-stream I/O interface (asynOctet equivalent).
pub trait AsynOctet: Send + Sync {
    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize>;
    /// Returns the number of bytes transferred (C `asynOctet::write`'s
    /// `*nbytesTransfered`).
    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize>;
    fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        Ok(())
    }
}
