//! Minimal read cursor over a borrowed byte slice. No dependency on any
//! external crate on purpose - this whole crate is meant to be the one
//! dependency-free, fuzzable layer of the stack.

use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotEnoughBytes {
    pub needed: usize,
    pub remaining: usize,
}

impl fmt::Display for NotEnoughBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "needed {} bytes, only {} remaining",
            self.needed, self.remaining
        )
    }
}

impl core::error::Error for NotEnoughBytes {}

pub struct ReadCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ReadCursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn ensure(&self, needed: usize) -> Result<(), NotEnoughBytes> {
        if self.remaining() < needed {
            Err(NotEnoughBytes {
                needed,
                remaining: self.remaining(),
            })
        } else {
            Ok(())
        }
    }

    pub fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    pub fn peek_u16_be(&self) -> Result<u16, NotEnoughBytes> {
        self.ensure(2)?;
        Ok(u16::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
        ]))
    }

    /// Looks at the next `n` bytes without advancing the position.
    pub fn peek_slice(&self, n: usize) -> Result<&'a [u8], NotEnoughBytes> {
        self.ensure(n)?;
        Ok(&self.buf[self.pos..self.pos + n])
    }

    /// Slices the underlying buffer by absolute offsets (as returned by
    /// [`Self::pos`]), independent of the current cursor position.
    pub fn slice_from_to(&self, start: usize, end: usize) -> &'a [u8] {
        &self.buf[start..end]
    }

    pub fn read_u8(&mut self) -> Result<u8, NotEnoughBytes> {
        self.ensure(1)?;
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn read_u16_be(&mut self) -> Result<u16, NotEnoughBytes> {
        self.ensure(2)?;
        let v = u16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_u16_le(&mut self) -> Result<u16, NotEnoughBytes> {
        self.ensure(2)?;
        let v = u16::from_le_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_u32_le(&mut self) -> Result<u32, NotEnoughBytes> {
        self.ensure(4)?;
        let v = u32::from_le_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    pub fn read_u64_le(&mut self) -> Result<u64, NotEnoughBytes> {
        self.ensure(8)?;
        let v = u64::from_le_bytes(self.buf[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Ok(v)
    }

    pub fn read_slice(&mut self, n: usize) -> Result<&'a [u8], NotEnoughBytes> {
        self.ensure(n)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Every byte from the current position to the end of the buffer.
    pub fn read_rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.pos..];
        self.pos = self.buf.len();
        s
    }
}

/// Appends encoded bytes to a growable buffer - encoding is infallible since
/// `Vec` growth never fails in practice, so there's no `WriteCursor` error
/// type to thread through call sites.
pub trait WriteBuf {
    fn write_u8(&mut self, v: u8);
    fn write_u16_be(&mut self, v: u16);
    fn write_u16_le(&mut self, v: u16);
    fn write_u32_le(&mut self, v: u32);
    fn write_u64_le(&mut self, v: u64);
    fn write_slice(&mut self, s: &[u8]);
}

impl WriteBuf for Vec<u8> {
    fn write_u8(&mut self, v: u8) {
        self.push(v);
    }

    fn write_u16_be(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_u16_le(&mut self, v: u16) {
        self.extend_from_slice(&v.to_le_bytes());
    }

    fn write_u32_le(&mut self, v: u32) {
        self.extend_from_slice(&v.to_le_bytes());
    }

    fn write_u64_le(&mut self, v: u64) {
        self.extend_from_slice(&v.to_le_bytes());
    }

    fn write_slice(&mut self, s: &[u8]) {
        self.extend_from_slice(s);
    }
}
