//! The reader everything in this crate goes through, and the cursor
//! that walks what it hands back.
//!
//! Two jobs. It counts, because how little a keyframe scan reads is the
//! measurable claim section 7's `keyframe` policy makes, and a claim
//! nobody counts is a hope. And it is the one place a number out of a
//! file becomes a buffer, which makes "never allocate from an unchecked
//! length" a property of one function rather than of every parser.

use std::io::{Read, Seek, SeekFrom};

use crate::{Error, Result};

/// The most bytes one read will hand back.
///
/// A `moov` of a long film is a few megabytes and an index of thirty
/// thousand 512-component records is fifteen; sixty-four megabytes is
/// well past both and well short of a length a damaged file could use
/// to exhaust a reader.
pub const MAX_READ: usize = 64 << 20;

/// What a read of a file cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Bytes actually pulled out of the file.
    pub bytes_read: u64,
    /// Times the read head was moved somewhere it was not already.
    pub seeks: u64,
    /// How large the file is, which is what the bytes are a share of.
    pub len: u64,
}

impl Tally {
    /// Bytes read as a percentage of the file, for the line a scan
    /// prints when it is done.
    pub fn share(&self) -> f64 {
        if self.len == 0 {
            return 0.0;
        }
        self.bytes_read as f64 * 100.0 / self.len as f64
    }
}

/// A file, read by absolute position, with the cost kept.
#[derive(Debug)]
pub struct Source<R> {
    inner: R,
    len: u64,
    at: u64,
    bytes_read: u64,
    seeks: u64,
}

impl<R: Read + Seek> Source<R> {
    /// A source over anything that reads and seeks. The one seek this
    /// costs, to find the length, is counted like any other.
    pub fn new(mut inner: R) -> Result<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(0))?;
        Ok(Self {
            inner,
            len,
            at: 0,
            bytes_read: 0,
            seeks: 1,
        })
    }

    /// How many bytes the file holds.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// What the reading has cost so far.
    pub fn tally(&self) -> Tally {
        Tally {
            bytes_read: self.bytes_read,
            seeks: self.seeks,
            len: self.len,
        }
    }

    /// Forgets the cost so far, so a caller can time one phase.
    pub fn reset_tally(&mut self) {
        self.bytes_read = 0;
        self.seeks = 0;
    }

    /// Up to `want` bytes at `at`, short at the end of the file.
    ///
    /// The only allocation in this crate that comes from a length a
    /// file chose, and it is clamped twice first: to the bytes that
    /// actually remain, and to [`MAX_READ`]. A `want` past the ceiling
    /// is a refusal rather than a silent truncation, because a caller
    /// that asked for a gigabyte has read a length it should not trust.
    pub fn read_at(&mut self, at: u64, want: usize) -> Result<Vec<u8>> {
        if want > MAX_READ {
            return Err(Error::Format("a value larger than this reader will hold"));
        }
        if at >= self.len || want == 0 {
            return Ok(Vec::new());
        }
        let left = usize::try_from(self.len - at).unwrap_or(usize::MAX);
        let take = want.min(left);
        if self.at != at {
            self.inner.seek(SeekFrom::Start(at))?;
            self.seeks += 1;
            self.at = at;
        }
        let mut out = vec![0u8; take];
        self.inner.read_exact(&mut out)?;
        self.at += take as u64;
        self.bytes_read += take as u64;
        Ok(out)
    }

    /// Exactly `want` bytes at `at`. Anything less is a truncation.
    pub fn exact_at(&mut self, at: u64, want: usize) -> Result<Vec<u8>> {
        let got = self.read_at(at, want)?;
        if got.len() != want {
            return Err(Error::Format("the file ends inside a value"));
        }
        Ok(got)
    }

    /// The bytes of a span, refused when the span is not inside the
    /// file or is larger than [`MAX_READ`].
    pub fn span(&mut self, at: u64, len: u64) -> Result<Vec<u8>> {
        let len = usize::try_from(len).map_err(|_| Error::Format("a span wider than memory"))?;
        self.exact_at(at, len)
    }

    /// The reader back, for a caller that has to write as well.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// The reader, borrowed. Reads made through it are not counted, so
    /// this is for writing and truncating, not for scanning.
    pub fn inner_mut(&mut self) -> &mut R {
        self.at = u64::MAX;
        &mut self.inner
    }
}

/// A bounds-checked walk over bytes already in hand.
///
/// `core::wire::Reader` does this for the format's own little-endian
/// numbers; containers are big-endian and have their own widths, so
/// this is the same idea with the other byte order.
#[derive(Clone, Copy, Debug)]
pub struct Bytes<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    pub fn left(&self) -> usize {
        self.data.len().saturating_sub(self.at)
    }

    pub fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .ok_or(Error::Format("a length that overflows"))?;
        let out = self
            .data
            .get(self.at..end)
            .ok_or(Error::Format("the bytes end inside a value"))?;
        self.at = end;
        Ok(out)
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let out = &self.data[self.at.min(self.data.len())..];
        self.at = self.data.len();
        out
    }

    pub fn skip(&mut self, count: usize) -> Result<()> {
        self.take(count).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub fn i32(&mut self) -> Result<i32> {
        self.u32().map(|value| value as i32)
    }

    pub fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        let mut out = [0u8; 8];
        out.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(out))
    }

    /// A box's version byte and its three flag bytes, in one.
    pub fn full_box(&mut self) -> Result<(u8, u32)> {
        let version = self.u8()?;
        let bytes = self.take(3)?;
        Ok((
            version,
            u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]),
        ))
    }

    /// An entry count, refused when the entries it claims could not fit
    /// in the bytes that are left.
    ///
    /// This is section 9's rule applied to a container: a table header
    /// saying four billion entries is a lie the file's own length
    /// contradicts, and catching it here is what keeps a `Vec` from
    /// being told to hold four billion of anything.
    pub fn count(&mut self, entry_size: usize) -> Result<usize> {
        let count = self.u32()? as usize;
        let want = count
            .checked_mul(entry_size.max(1))
            .ok_or(Error::Format("a table wider than memory"))?;
        if want > self.left() {
            return Err(Error::Format("a table longer than the box holding it"));
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn source(bytes: &[u8]) -> Source<Cursor<Vec<u8>>> {
        Source::new(Cursor::new(bytes.to_vec())).expect("a source")
    }

    #[test]
    fn a_read_is_clamped_to_the_file_and_counted() {
        let mut src = source(&[1, 2, 3, 4, 5, 6, 7, 8]);
        src.reset_tally();
        assert_eq!(src.read_at(4, 100).expect("bytes"), vec![5, 6, 7, 8]);
        assert_eq!(src.tally().bytes_read, 4);
        assert_eq!(src.tally().seeks, 1, "one move of the head");
        // Reading on from where the head already is costs no seek.
        assert!(src.read_at(8, 4).expect("bytes").is_empty());
        assert_eq!(src.tally().seeks, 1);
        assert!(src.exact_at(4, 5).is_err(), "a short read is a truncation");
    }

    #[test]
    fn a_length_past_the_ceiling_is_refused_rather_than_allocated() {
        let mut src = source(&[0; 16]);
        assert!(src.read_at(0, MAX_READ + 1).is_err());
        assert!(src.span(0, u64::MAX).is_err());
    }

    #[test]
    fn a_cursor_never_reads_past_what_it_was_given() {
        let mut bytes = Bytes::new(&[0, 0, 0, 2, 9, 9]);
        assert_eq!(bytes.count(1).expect("a count"), 2);
        assert_eq!(bytes.take(2).expect("the entries"), &[9, 9]);
        assert!(bytes.u8().is_err());

        let mut wide = Bytes::new(&[0xff, 0xff, 0xff, 0xff]);
        assert!(wide.count(8).is_err(), "a count the box cannot hold");

        let mut short = Bytes::new(&[1, 2]);
        assert!(short.u32().is_err());
        assert!(short.take(usize::MAX).is_err());
    }
}
