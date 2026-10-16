//! A diagnostic formatted in place into a fixed buffer — C asyn's
//! `pasynUser->errorMessage` and asynRecord's `errs`, both written with
//! `epicsSnprintf` into storage the user owns. Formatting into it allocates
//! nothing, so a report made under a record's hold stays off the heap.

use std::fmt;

/// At most `N` bytes of text: `epicsSnprintf` into a buffer of `N + 1`.
#[derive(Clone, Copy)]
pub struct FixedText<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> FixedText<N> {
    pub const EMPTY: Self = Self {
        buf: [0; N],
        len: 0,
    };

    /// The text cut at `N` bytes. C cuts on a byte; this cuts on a char
    /// boundary so the text stays valid UTF-8.
    pub fn format(args: fmt::Arguments<'_>) -> Self {
        let mut text = Self::EMPTY;
        // `Err` is the cut, which ends the text.
        let _ = fmt::Write::write_fmt(&mut text, args);
        text
    }

    /// The bytes a client wrote, an invalid sequence read as U+FFFD.
    pub fn from_bytes_lossy(bytes: &[u8]) -> Self {
        let mut text = Self::EMPTY;
        for chunk in bytes.utf8_chunks() {
            let cut = fmt::Write::write_str(&mut text, chunk.valid()).is_err()
                || (!chunk.invalid().is_empty()
                    && fmt::Write::write_char(&mut text, char::REPLACEMENT_CHARACTER).is_err());
            if cut {
                break;
            }
        }
        text
    }

    pub fn as_str(&self) -> &str {
        // Only `write_str` fills the buffer, and it stops on a char boundary.
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or_default()
    }
}

/// Bytes shown as text, an invalid sequence as U+FFFD, decoded only when
/// formatted: a trace line whose mask is off costs nothing.
pub struct LossyText<'a>(pub &'a [u8]);

impl fmt::Display for LossyText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            f.write_str(chunk.valid())?;
            if !chunk.invalid().is_empty() {
                fmt::Write::write_char(f, char::REPLACEMENT_CHARACTER)?;
            }
        }
        Ok(())
    }
}

impl<const N: usize> fmt::Write for FixedText<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.floor_char_boundary(N - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        if n < s.len() {
            return Err(fmt::Error);
        }
        Ok(())
    }
}

impl<const N: usize> Default for FixedText<N> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<const N: usize> std::ops::Deref for FixedText<N> {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl<const N: usize> PartialEq for FixedText<N> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<const N: usize> Eq for FixedText<N> {}

impl<const N: usize> PartialEq<str> for FixedText<N> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<const N: usize> PartialEq<&str> for FixedText<N> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl<const N: usize> From<&str> for FixedText<N> {
    fn from(s: &str) -> Self {
        Self::format(format_args!("{s}"))
    }
}

impl<const N: usize> fmt::Display for FixedText<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const N: usize> fmt::Debug for FixedText<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}
