//! `StreamBuffer.cc` / `StreamBuffer.h`: the byte buffer every part of
//! StreamDevice passes around.
//!
//! The layout is the C one:
//!
//! ```text
//! |----junk-----|####used####|--------00--------|
//! |<--- offs -->|<-- len --->|<- cap-offs-len ->|
//! 0            offs      offs+len              cap
//! ```
//!
//! Everything after the data is 0x00, so the data is always a terminated C
//! string, and removing from the front or clearing only moves `offs`.
//! Index parameters may be negative, counting from the end (-1 is the last
//! byte).

use std::fmt;

use crate::cstr::CPtr;
use crate::error::{AnsiMode, ansi_escape};

const LOCAL_SIZE: usize = 64;

#[derive(Clone)]
pub struct StreamBuffer {
    buffer: Vec<u8>,
    len: usize,
    offs: usize,
}

impl Default for StreamBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for StreamBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "StreamBuffer({:?})",
            String::from_utf8_lossy(self.bytes())
        )
    }
}

impl PartialEq for StreamBuffer {
    fn eq(&self, other: &Self) -> bool {
        self.bytes() == other.bytes()
    }
}

impl StreamBuffer {
    /// `StreamBuffer()`
    pub fn new() -> Self {
        Self::init(None, 0)
    }

    /// `StreamBuffer(const void* s, ssize_t size)`
    pub fn from_bytes(s: &[u8]) -> Self {
        Self::init(Some(s), s.len())
    }

    /// `StreamBuffer(ssize_t size)`: empty, with room for `size` bytes.
    pub fn with_size(size: usize) -> Self {
        Self::init(None, size)
    }

    fn init(s: Option<&[u8]>, minsize: usize) -> Self {
        let mut b = Self {
            buffer: vec![0; LOCAL_SIZE],
            len: 0,
            offs: 0,
        };
        if minsize >= b.buffer.len() {
            b.grow(minsize);
        }
        if let Some(s) = s {
            b.len = minsize;
            b.buffer[..minsize].copy_from_slice(s);
        }
        b
    }

    fn cap(&self) -> usize {
        self.buffer.len()
    }

    fn check(&mut self, size: usize) {
        if self.len + self.offs + size >= self.cap() {
            self.grow(self.len + size);
        }
    }

    /// Make room for `minsize` bytes plus the terminator.
    fn grow(&mut self, minsize: usize) {
        if minsize < self.cap() {
            // Just move the contents to the start and clear the end.
            let (offs, len) = (self.offs, self.len);
            self.buffer.copy_within(offs..offs + len, 0);
            self.buffer[len..len + offs].fill(0);
            self.offs = 0;
            return;
        }
        let mut newcap = LOCAL_SIZE * 2;
        while newcap <= minsize {
            newcap *= 2;
        }
        let mut newbuffer = vec![0; newcap];
        newbuffer[..self.len].copy_from_slice(self.bytes());
        self.buffer = newbuffer;
        self.offs = 0;
    }

    fn index(&self, index: isize) -> usize {
        if index < 0 {
            (index + self.len as isize) as usize
        } else {
            index as usize
        }
    }

    /// The data bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.buffer[self.offs..self.offs + self.len]
    }

    /// The data bytes, mutable.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        let (offs, len) = (self.offs, self.len);
        &mut self.buffer[offs..offs + len]
    }

    /// `operator()(index)`: a `char*` to `index`, reading on into the zero
    /// tail.
    pub fn ptr(&self, index: isize) -> CPtr<'_> {
        CPtr::at_offset(self.bytes(), self.index(index))
    }

    /// `operator()()` read as a C string: the data up to the first 0x00.
    pub fn cstr(&self) -> &[u8] {
        self.ptr(0).cstr()
    }

    /// `operator[](index)`; past the data it is the zero tail.
    pub fn get(&self, index: isize) -> u8 {
        let i = self.index(index);
        self.buffer.get(self.offs + i).copied().unwrap_or(0)
    }

    /// `operator[](index) = c`
    pub fn set_byte(&mut self, index: isize, c: u8) {
        let i = self.offs + self.index(index);
        self.buffer[i] = c;
    }

    /// `operator bool`: not empty?
    pub fn is_set(&self) -> bool {
        self.len > 0
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn length(&self) -> usize {
        self.len
    }

    /// Current maximum data length (one byte is spared for the end).
    pub fn capacity(&self) -> usize {
        self.cap() - 1
    }

    /// Set the length to 0 without blanking memory.
    pub fn clear(&mut self) -> &mut Self {
        self.offs += self.len;
        self.len = 0;
        self
    }

    /// Reserve `size` bytes at the end and return them for writing.
    pub fn reserve(&mut self, size: usize) -> &mut [u8] {
        self.check(size);
        let start = self.offs + self.len;
        self.len += size;
        &mut self.buffer[start..start + size]
    }

    /// `append(char c)`
    pub fn append_byte(&mut self, c: u8) -> &mut Self {
        self.check(1);
        let i = self.offs + self.len;
        self.buffer[i] = c;
        self.len += 1;
        self
    }

    /// `append(char c, ssize_t count)`: a negative count deletes from the end.
    pub fn append_repeat(&mut self, c: u8, count: isize) -> &mut Self {
        if count < 0 {
            self.truncate(count);
        } else {
            let count = count as usize;
            self.check(count);
            let i = self.offs + self.len;
            self.buffer[i..i + count].fill(c);
            self.len += count;
        }
        self
    }

    /// `append(const void* s, ssize_t size)`
    pub fn append(&mut self, s: &[u8]) -> &mut Self {
        if !s.is_empty() {
            self.check(s.len());
            let i = self.offs + self.len;
            self.buffer[i..i + s.len()].copy_from_slice(s);
            self.len += s.len();
        }
        self
    }

    /// `append(NULL, -size)`: delete `size` bytes from the end.
    pub fn append_negative(&mut self, size: usize) -> &mut Self {
        let size = size.min(self.len);
        let end = self.offs + self.len;
        self.buffer[end - size..end].fill(0);
        self.len -= size;
        self
    }

    /// `append(const char* s)`: the bytes up to the first 0x00.
    pub fn append_cstr(&mut self, s: CPtr<'_>) -> &mut Self {
        let s = s.cstr().to_vec();
        self.append(&s)
    }

    /// `append(const StreamBuffer& s)`
    pub fn append_buffer(&mut self, s: &StreamBuffer) -> &mut Self {
        self.append(s.bytes())
    }

    /// `set(const void* s, size_t size)`
    pub fn set(&mut self, s: &[u8]) -> &mut Self {
        self.clear();
        self.append(s)
    }

    /// Delete `length` bytes at `pos` and insert `ins` there.
    pub fn replace(&mut self, remstart: isize, remlen: isize, ins: &[u8]) -> &mut Self {
        let len = self.len as isize;
        let mut remstart = remstart;
        let mut remlen = remlen;
        if remstart < 0 {
            // remove from end
            remstart += len;
        }
        if remlen < 0 {
            // remove left of remstart
            remstart += remlen;
            remlen = -remlen;
        }
        if remstart < 0 {
            // truncate remove before bufferstart
            remlen += remstart;
            remstart = 0;
        }
        if remstart > len {
            // remove begins after bufferend
            remstart = len;
        }
        // A range wholly before the buffer start leaves remlen negative;
        // C compares it as size_t (StreamBuffer.cc:242), so it removes
        // everything after remstart; compileNumber's truncate after an
        // empty variable depends on it.
        if remlen < 0 || remlen >= len - remstart {
            // truncate remove after bufferend
            remlen = len - remstart;
        }
        let (remstart, remlen) = (remstart as usize, remlen as usize);
        let inslen = ins.len();
        if inslen == 0 && remstart == 0 {
            // optimize remove of bufferstart
            self.offs += remlen;
            self.len -= remlen;
            return self;
        }
        let len = self.len;
        let offs = self.offs;
        let remend = remstart + remlen;
        let newlen = len + inslen - remlen;
        if self.cap() <= newlen {
            // buffer too short, copy to new buffer
            let mut newcap = LOCAL_SIZE * 2;
            while newcap <= newlen {
                newcap *= 2;
            }
            let mut newbuffer = vec![0; newcap];
            newbuffer[..remstart].copy_from_slice(&self.buffer[offs..offs + remstart]);
            newbuffer[remstart..remstart + inslen].copy_from_slice(ins);
            newbuffer[remstart + inslen..newlen]
                .copy_from_slice(&self.buffer[offs + remend..offs + len]);
            self.buffer = newbuffer;
            self.offs = 0;
        } else if offs + newlen < self.cap() {
            // modified content still fits with current offs
            if newlen != len {
                self.buffer
                    .copy_within(offs + remend..offs + len, offs + remstart + inslen);
            }
            self.buffer[offs + remstart..offs + remstart + inslen].copy_from_slice(ins);
            if newlen < len {
                self.buffer[offs + newlen..offs + len].fill(0);
            }
        } else {
            // move content to start of buffer
            self.buffer.copy_within(offs..offs + remstart, 0);
            self.buffer
                .copy_within(offs + remend..offs + len, remstart + inslen);
            self.buffer[remstart..remstart + inslen].copy_from_slice(ins);
            if newlen < offs + len {
                self.buffer[newlen..offs + len].fill(0);
            }
            self.offs = 0;
        }
        self.len = newlen;
        self
    }

    /// `remove(ssize_t pos, ssize_t length)`
    pub fn remove(&mut self, pos: isize, length: isize) -> &mut Self {
        self.replace(pos, length, &[])
    }

    /// `remove(size_t length)`: drop bytes from the start, fast.
    pub fn remove_front(&mut self, length: usize) -> &mut Self {
        let length = length.min(self.len);
        self.offs += length;
        self.len -= length;
        self
    }

    /// Delete the end of the buffer from `pos` on.
    pub fn truncate(&mut self, pos: isize) -> &mut Self {
        let len = self.len as isize;
        self.replace(pos, len, &[])
    }

    pub fn insert(&mut self, pos: isize, s: &[u8]) -> &mut Self {
        self.replace(pos, 0, s)
    }

    /// `print(fmt, ...)`: append formatted text.
    pub fn print(&mut self, args: fmt::Arguments<'_>) -> &mut Self {
        let s = fmt::format(args);
        self.append(s.as_bytes())
    }

    /// `find(char c, ssize_t start)`: index of `c` at or after `start`, or -1.
    pub fn find_byte(&self, c: u8, start: isize) -> isize {
        // C writes `start -= len` here, which turns every negative start into
        // 0 (StreamBuffer.h, find(char)); a negative index counts from the
        // end everywhere else in this class.
        let mut start = start;
        if start < 0 {
            start += self.len as isize;
            if start < 0 {
                start = 0;
            }
        }
        let start = start as usize;
        if start > self.len {
            return -1;
        }
        self.bytes()[start..]
            .iter()
            .position(|&b| b == c)
            .map_or(-1, |i| (start + i) as isize)
    }

    /// `find(const void* s, size_t size, ssize_t start)`
    pub fn find(&self, s: &[u8], start: isize) -> isize {
        let len = self.len as isize;
        let mut start = start;
        if start < 0 {
            start += len;
            if start < 0 {
                start = 0;
            }
        }
        if start + s.len() as isize > len {
            return -1; // find nothing after end
        }
        if s.is_empty() {
            return start; // find empty string at start
        }
        let start = start as usize;
        self.bytes()[start..]
            .windows(s.len())
            .position(|w| w == s)
            .map_or(-1, |i| (start + i) as isize)
    }

    /// `startswith(const void* s, size_t size)`: the first bytes equal `s`.
    pub fn startswith_bytes(&self, s: &[u8]) -> bool {
        self.len >= s.len() && &self.bytes()[..s.len()] == s
    }

    /// `startswith(const char* s)`: `strcmp(data, s) == 0`; an empty buffer
    /// matches only an empty or absent string.
    pub fn startswith(&self, s: Option<&[u8]>) -> bool {
        if self.len > 0 {
            match s {
                Some(s) => self.cstr() == s,
                // C dereferences NULL here; no caller reaches it.
                None => false,
            }
        } else {
            s.is_none_or(|s| s.is_empty())
        }
    }

    /// Copy where every byte below 0x20 or from 0x7f on reads `<xx>`.
    pub fn expand_range(&self, start: isize, length: isize) -> StreamBuffer {
        let len = self.len as isize;
        let mut start = start;
        let mut length = length;
        if start < 0 {
            start += len;
        }
        if length < 0 {
            start += length;
            length = -length;
        }
        if start < 0 {
            length += start;
            start = 0;
        }
        // A negative end is a size_t in C (StreamBuffer.cc:366): a range
        // wholly before the buffer start expands the whole buffer.
        let end = if start + length < 0 {
            len
        } else {
            (start + length).min(len)
        };
        let mut result = StreamBuffer::new();
        let mut i = start;
        while i < end {
            let c = self.buffer[self.offs + i as usize];
            // C's `char` is signed: 0x80..0xff are below 0x20 too.
            if !(0x20..0x7f).contains(&c) {
                result.print(format_args!(
                    "{}<{:02x}>{}",
                    ansi_escape(AnsiMode::ReverseVideo),
                    c,
                    ansi_escape(AnsiMode::NotReverseVideo)
                ));
            } else {
                result.append_byte(c);
            }
            i += 1;
        }
        result
    }

    /// `expand(start)`
    pub fn expand_from(&self, start: isize) -> StreamBuffer {
        self.expand_range(start, self.len as isize)
    }

    /// `expand()`
    pub fn expand(&self) -> StreamBuffer {
        self.expand_from(0)
    }

    /// Like `expand`, but also shows the memory before and after the data.
    pub fn dump(&self) -> StreamBuffer {
        let mut result = StreamBuffer::new();
        result.print(format_args!("{},{},{}:", self.offs, self.len, self.cap()));
        if self.offs > 0 {
            result.append(ansi_escape(AnsiMode::BgWhite).as_bytes());
        }
        for i in 0..self.cap() {
            let c = self.buffer[i];
            if self.offs > 0 && i == self.offs {
                result.append(ansi_escape(AnsiMode::Reset).as_bytes());
            }
            if !(0x20..0x7f).contains(&c) {
                result.print(format_args!(
                    "{}<{:02x}>{}",
                    ansi_escape(AnsiMode::ReverseVideo),
                    c,
                    ansi_escape(AnsiMode::NotReverseVideo)
                ));
            } else {
                result.append_byte(c);
            }
            if self.offs + self.len >= 1 && i == self.offs + self.len - 1 {
                result.append(ansi_escape(AnsiMode::BgWhite).as_bytes());
            }
        }
        result.append(ansi_escape(AnsiMode::Reset).as_bytes());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &[u8]) -> StreamBuffer {
        StreamBuffer::from_bytes(s)
    }

    /// The zero tail is what lets C read a token's terminator and line.
    fn assert_zero_tail(x: &StreamBuffer) {
        assert!(
            x.buffer[x.offs + x.len..].iter().all(|&c| c == 0),
            "{:?}",
            x.buffer
        );
        assert!(x.offs + x.len < x.cap());
    }

    #[test]
    fn negative_indices_count_from_the_end() {
        let x = b(b"abcdef");
        assert_eq!(x.get(-1), b'f');
        assert_eq!(x.get(6), 0);
        let mut y = x.clone();
        y.truncate(-2);
        assert_eq!(y.bytes(), b"abcd");
        assert_zero_tail(&y);
        let mut y = x.clone();
        y.remove(-3, 1);
        assert_eq!(y.bytes(), b"abcef");
        let mut y = x.clone();
        y.replace(2, -1, b"XY");
        assert_eq!(y.bytes(), b"aXYcdef");
    }

    #[test]
    fn every_edit_keeps_the_zero_tail() {
        let mut x = StreamBuffer::new();
        for i in 0..300u32 {
            x.append(format!("{i},").as_bytes());
            assert_zero_tail(&x);
            if i % 7 == 0 {
                x.remove_front(3);
                assert_zero_tail(&x);
            }
            if i % 11 == 0 {
                x.replace(1, 4, b"");
                assert_zero_tail(&x);
            }
            if i % 13 == 0 {
                x.replace(-5, 2, b"0123456789");
                assert_zero_tail(&x);
            }
        }
        x.clear();
        assert_zero_tail(&x);
        x.append(b"q");
        assert_eq!(x.cstr(), b"q");
    }

    #[test]
    /// The results are C's StreamBuffer.cc at 562dd5a, run on these calls.
    fn a_range_wholly_before_the_start_takes_the_whole_buffer_as_c() {
        let mut x = b(b"abc");
        // (-5, 1) reaches from 2 before the start to 1 before it.
        x.replace(-5, 1, b"X");
        assert_eq!(x.bytes(), b"X");
        x.replace(-9, -2, b"");
        assert_eq!(x.bytes(), b"");
        let mut x = b(b"abcdef");
        x.replace(-10, 2, b"XY");
        assert_eq!(x.bytes(), b"XY");
        // A range that overlaps the start is cut at the start.
        let mut x = b(b"Xabc");
        x.replace(-6, 3, b"");
        assert_eq!(x.bytes(), b"abc");
        assert_eq!(x.expand_range(-5, 1).bytes(), b"abc");
        assert_eq!(x.expand_range(-9, -2).bytes(), b"abc");
        assert_eq!(x.expand_range(-4, 2).bytes(), b"a");
        // compileNumber's truncate after an empty variable.
        for (s, want) in [(&b"1"[..], &b""[..]), (b"12", b""), (b"123", b"23")] {
            let mut x = b(s);
            x.truncate(-5);
            assert_eq!(x.bytes(), want);
        }
    }

    #[test]
    fn grow_doubles_from_twice_the_local_size() {
        let mut x = StreamBuffer::new();
        assert_eq!(x.capacity(), 63);
        x.append(&[b'a'; 64]);
        assert_eq!(x.capacity(), 127);
        x.append(&[b'a'; 64]);
        assert_eq!(x.capacity(), 255);
        assert_eq!(StreamBuffer::with_size(500).capacity(), 511);
    }

    #[test]
    fn find_and_startswith() {
        let x = b(b"abcabc");
        assert_eq!(x.find(b"ca", 0), 2);
        assert_eq!(x.find(b"bc", 2), 4);
        assert_eq!(x.find(b"bc", -2), 4);
        assert_eq!(x.find(b"", 3), 3);
        assert_eq!(x.find(b"x", 0), -1);
        assert_eq!(x.find_byte(b'c', -1), 5);
        assert_eq!(x.find_byte(b'a', 1), 3);
        assert!(x.startswith_bytes(b"abc"));
        assert!(x.startswith(Some(b"abcabc")));
        assert!(!x.startswith(Some(b"abc")));
        let mut tok = b(b"name");
        tok.append_byte(0).append(&5i32.to_ne_bytes());
        assert!(tok.startswith(Some(b"name")));
        assert!(StreamBuffer::new().startswith(None));
        assert!(StreamBuffer::new().startswith(Some(b"")));
        assert!(!StreamBuffer::new().startswith(Some(b"a")));
    }

    #[test]
    fn expand_marks_signed_char_non_printables() {
        let _switches = crate::error::TEST_SWITCHES.lock();
        crate::error::set_stream_debug_colored(false);
        let x = b(b"a\r\n\x7f\xc3z");
        assert_eq!(x.expand().bytes(), b"a<0d><0a><7f><c3>z");
        assert_eq!(x.expand_range(-2, 2).bytes(), b"<c3>z");
    }
}
