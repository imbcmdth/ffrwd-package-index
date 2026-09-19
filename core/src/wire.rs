//! The number and string spellings of section 1, and a reader that
//! checks every length against the bytes that remain before it uses it.
//!
//! Section 9 is the reason this is one small module everything else
//! goes through: a reader of this format takes bytes from a video file
//! anybody can write, so there is exactly one place where a length can
//! be trusted, and it is here.

use crate::{Error, Result};

/// A cursor over a message's bytes.
///
/// Every method either returns the value or leaves the cursor where it
/// was and returns an error, so a caller that gives up mid-message
/// cannot half-consume it.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader over `data`, positioned at its first byte.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// How many bytes are left.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Whether the bytes are used up.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// How far in the cursor sits.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// The bytes not yet read, without reading them.
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    /// The next `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        let slice = self.data.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    /// The next `N` bytes as an array, for the fixed-width fields.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// One byte.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    /// Two bytes, little-endian.
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array::<2>()?))
    }

    /// Four bytes, little-endian.
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.array::<4>()?))
    }

    /// An unsigned LEB128 of at most five bytes.
    ///
    /// Five bytes is the format's limit, and the fifth byte carries
    /// four usable bits: anything more names a number wider than the
    /// `varint` of section 1 and is refused rather than wrapped.
    pub fn varint(&mut self) -> Result<u32> {
        let start = self.pos;
        let mut value = 0u64;
        for index in 0..5 {
            let byte = match self.u8() {
                Ok(byte) => byte,
                Err(err) => {
                    self.pos = start;
                    return Err(err);
                }
            };
            value |= u64::from(byte & 0x7f) << (index * 7);
            if byte & 0x80 == 0 {
                if value > u64::from(u32::MAX) {
                    self.pos = start;
                    return Err(Error::Varint);
                }
                return Ok(value as u32);
            }
        }
        self.pos = start;
        Err(Error::Varint)
    }

    /// A varint as a length, in the machine's own width.
    pub fn length(&mut self) -> Result<usize> {
        Ok(self.varint()? as usize)
    }

    /// A zigzag varint.
    pub fn svarint(&mut self) -> Result<i32> {
        Ok(unzigzag(self.varint()?))
    }

    /// A varint length and that many bytes of UTF-8.
    pub fn str(&mut self) -> Result<&'a str> {
        let start = self.pos;
        let len = match self.length() {
            Ok(len) => len,
            Err(err) => {
                self.pos = start;
                return Err(err);
            }
        };
        match self.take(len) {
            Ok(bytes) => core::str::from_utf8(bytes).map_err(|_| {
                self.pos = start;
                Error::Utf8
            }),
            Err(err) => {
                self.pos = start;
                Err(err)
            }
        }
    }
}

/// Appends an unsigned LEB128.
pub fn put_varint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Appends a zigzag varint.
pub fn put_svarint(out: &mut Vec<u8>, value: i32) {
    put_varint(out, zigzag(value));
}

/// Appends a varint length and the string's bytes.
pub fn put_str(out: &mut Vec<u8>, value: &str) {
    put_varint(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

/// How many bytes [`put_varint`] will write.
pub fn varint_len(value: u32) -> usize {
    match value {
        0..=0x7f => 1,
        0x80..=0x3fff => 2,
        0x4000..=0x1f_ffff => 3,
        0x20_0000..=0x0fff_ffff => 4,
        _ => 5,
    }
}

/// 0, -1, 1, -2 become 0, 1, 2, 3.
pub fn zigzag(value: i32) -> u32 {
    ((value << 1) ^ (value >> 31)) as u32
}

/// The inverse of [`zigzag`].
pub fn unzigzag(value: u32) -> i32 {
    ((value >> 1) as i32) ^ -((value & 1) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_at_every_width() {
        for value in [0u32, 1, 127, 128, 16383, 16384, 1 << 21, 1 << 28, u32::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, value);
            assert_eq!(out.len(), varint_len(value), "width of {value}");
            let mut reader = Reader::new(&out);
            assert_eq!(reader.varint().expect("a varint"), value);
            assert!(reader.is_empty());
        }
    }

    #[test]
    fn zigzag_round_trips_across_the_sign() {
        for value in [0i32, -1, 1, -2, 2, i32::MIN, i32::MAX, -1000, 1000] {
            assert_eq!(unzigzag(zigzag(value)), value, "zigzag of {value}");
        }
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
        assert_eq!(zigzag(-2), 3);
    }

    #[test]
    fn a_varint_of_six_bytes_is_refused() {
        let bytes = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert_eq!(Reader::new(&bytes).varint(), Err(Error::Varint));
    }

    #[test]
    fn a_varint_wider_than_thirty_two_bits_is_refused() {
        // Five bytes, but the fifth carries bits past 2^32 - 1.
        let bytes = [0xffu8, 0xff, 0xff, 0xff, 0x1f];
        assert_eq!(Reader::new(&bytes).varint(), Err(Error::Varint));
    }

    #[test]
    fn a_truncated_varint_leaves_the_cursor_alone() {
        let bytes = [0x80u8];
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.varint(), Err(Error::Truncated));
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn a_string_longer_than_its_bytes_is_refused() {
        let mut bytes = Vec::new();
        put_varint(&mut bytes, 9);
        bytes.extend_from_slice(b"short");
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.str(), Err(Error::Truncated));
        assert_eq!(reader.position(), 0, "a refused string reads nothing");
    }

    #[test]
    fn a_string_that_is_not_utf8_is_refused() {
        let bytes = [2u8, 0xff, 0xfe];
        assert_eq!(Reader::new(&bytes).str(), Err(Error::Utf8));
    }

    #[test]
    fn strings_round_trip_including_the_empty_one() {
        for value in ["", "hf:org/model@rev/file.safetensors", "\u{1f600} unicode"] {
            let mut out = Vec::new();
            put_str(&mut out, value);
            let mut reader = Reader::new(&out);
            assert_eq!(reader.str().expect("a string"), value);
        }
    }

    #[test]
    fn taking_past_the_end_is_refused_without_overflow() {
        let bytes = [1u8, 2, 3];
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.take(usize::MAX), Err(Error::Truncated));
        assert_eq!(reader.take(4), Err(Error::Truncated));
        assert_eq!(reader.take(3).expect("all three"), &bytes[..]);
    }
}
