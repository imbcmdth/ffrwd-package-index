//! Putting the file index of section 8 into an MP4, in place.
//!
//! "Placing it at the end of the file moves nothing else", says section
//! 8, and that is the whole method: the `uuid` box goes on the end, the
//! `moov` and the `mdat` stay where they are, and a player that does
//! not know the box skips it. Writing the index costs one append
//! whatever the file's size.
//!
//! Two files need a word more.
//!
//! **A file that already has one.** When our box is the last thing in
//! the file it is cut off and written again, which is the same append.
//! When it is not, the file has to be copied without it, and this
//! refuses to do that unless it is asked, because a copy is not what
//! "in place" promised.
//!
//! **A fragmented file.** Section 8 has the box go before an `mfra`,
//! and the reason is `mfro`: it is a copy of `mfra`'s own size, placed
//! last so that the last four bytes of the file find it. Appending past
//! the `mfra` leaves those four bytes pointing at an index, and a reader
//! that uses them loses its random access; ffmpeg 9 does not, because it
//! builds the fragment index by walking the file, but a reader that does
//! is not wrong to. So the box goes in front of the `mfra` and the
//! `mfra` is written again after it. That costs the `mfra`'s own length, which is
//! sixteen bytes a fragment, and moves nothing that anything points at:
//! the offsets inside `tfra` name the `moof` boxes, and they are all
//! before the `mfra` already.
//!
//! Matroska is not here. Writing an attachment natively means rewriting
//! the segment: the `SeekHead` at the front names its children by their
//! position, every enclosing length changes, and a segment whose length
//! was written before the attachment existed has to be rewritten too.
//! `tool/README.md` says what `ffrwd-index index` does instead.

use std::io::{Read, Seek, SeekFrom, Write};

use ffrwd_index_core::index::mp4_uuid_box;

use crate::mp4::{self, Spot};
use crate::{Error, Result, Source};

/// Cutting a file short, which neither [`Write`] nor [`Seek`] can do.
pub trait Truncate {
    fn truncate_to(&mut self, len: u64) -> std::io::Result<()>;
}

impl Truncate for std::fs::File {
    fn truncate_to(&mut self, len: u64) -> std::io::Result<()> {
        self.set_len(len)
    }
}

impl Truncate for std::io::Cursor<Vec<u8>> {
    fn truncate_to(&mut self, len: u64) -> std::io::Result<()> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        self.get_mut().truncate(len);
        Ok(())
    }
}

impl<T: Truncate + ?Sized> Truncate for &mut T {
    fn truncate_to(&mut self, len: u64) -> std::io::Result<()> {
        (**self).truncate_to(len)
    }
}

/// What writing the index did to the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    /// Appended, with nothing else touched.
    Appended { at: u64 },
    /// Appended over the box that was already last.
    Replaced { at: u64 },
    /// Put in front of the file's `mfra`, which was written again after
    /// it.
    BeforeMfra { at: u64, moved: u64 },
}

/// How much of an `mfra` this will hold in memory to write it again.
const MAX_MFRA: u64 = 16 << 20;

/// Writes the index into an MP4 that is open for reading and writing.
///
/// The file is read through a [`Source`] first, so the cost of finding
/// where the box goes is counted like any other read.
pub fn install<T: Read + Write + Seek + Truncate>(file: T, index: &[u8]) -> Result<Placed> {
    let mut src = Source::new(file)?;
    let spot = mp4::spot(&mut src)?;
    let boxed = mp4_uuid_box(index);
    match spot {
        Spot::End(at) => {
            let file = src.inner_mut();
            file.seek(SeekFrom::Start(at))?;
            file.write_all(&boxed)?;
            file.flush()?;
            Ok(Placed::Appended { at })
        }
        Spot::Over(at) => {
            let file = src.inner_mut();
            file.truncate_to(at)?;
            file.seek(SeekFrom::Start(at))?;
            file.write_all(&boxed)?;
            file.flush()?;
            Ok(Placed::Replaced { at })
        }
        Spot::BeforeMfra(at) => {
            let moved = src.len() - at;
            if moved > MAX_MFRA {
                return Err(Error::Format("an mfra larger than this writer will move"));
            }
            let mfra = src.span(at, moved)?;
            let file = src.inner_mut();
            file.truncate_to(at)?;
            file.seek(SeekFrom::Start(at))?;
            file.write_all(&boxed)?;
            file.write_all(&mfra)?;
            file.flush()?;
            Ok(Placed::BeforeMfra { at, moved })
        }
        Spot::Rewrite(start, _) => Err(Error::Unsupported(format!(
            "the file already carries an index box at byte {start}, and it is not the last box, \
             so it cannot be replaced without copying the file. Pass --rewrite to copy it"
        ))),
    }
}

/// Copies a file without the index box it already has, then appends the
/// new one. What `--rewrite` does.
pub fn rewrite<R: Read + Seek, W: Write>(
    src: &mut Source<R>,
    out: &mut W,
    index: &[u8],
) -> Result<()> {
    let (start, end) = match mp4::spot(src)? {
        Spot::Rewrite(start, end) => (start, end),
        Spot::Over(start) => (start, src.len()),
        _ => (src.len(), src.len()),
    };
    copy_span(src, out, 0, start)?;
    copy_span(src, out, end, src.len())?;
    out.write_all(&mp4_uuid_box(index))?;
    out.flush()?;
    Ok(())
}

/// How much of a file is copied at a time.
const CHUNK: usize = 1 << 20;

fn copy_span<R: Read + Seek, W: Write>(
    src: &mut Source<R>,
    out: &mut W,
    from: u64,
    to: u64,
) -> Result<()> {
    let mut at = from;
    while at < to {
        let want = usize::try_from(to - at).unwrap_or(CHUNK).min(CHUNK);
        let bytes = src.read_at(at, want)?;
        if bytes.is_empty() {
            break;
        }
        out.write_all(&bytes)?;
        at += bytes.len() as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn plain() -> Vec<u8> {
        let mut file = bx(b"ftyp", b"isom");
        file.extend_from_slice(&bx(b"mdat", &[5; 64]));
        file.extend_from_slice(&bx(b"moov", &[0; 32]));
        file
    }

    fn index(byte: u8) -> Vec<u8> {
        vec![b'F', b'F', b'I', b'X', 1, byte]
    }

    #[test]
    fn an_index_is_appended_and_then_written_over() {
        let before = plain();
        let mut file = Cursor::new(before.clone());
        let placed = install(&mut file, &index(0)).expect("a write");
        assert_eq!(
            placed,
            Placed::Appended {
                at: before.len() as u64
            }
        );
        assert_eq!(
            &file.get_ref()[..before.len()],
            &before[..],
            "nothing moved"
        );

        // A second write lands on top of the first, so the file does not
        // grow an index a read.
        let grown = file.get_ref().len();
        let placed = install(&mut file, &index(1)).expect("a write");
        assert_eq!(
            placed,
            Placed::Replaced {
                at: before.len() as u64
            }
        );
        assert_eq!(file.get_ref().len(), grown);
        let mut src = Source::new(Cursor::new(file.into_inner())).expect("a source");
        assert_eq!(
            mp4::read_index(&mut src)
                .expect("a read")
                .expect("an index"),
            index(1)
        );
    }

    #[test]
    fn a_fragmented_file_keeps_its_mfra_last() {
        let mut before = bx(b"ftyp", b"isom");
        before.extend_from_slice(&bx(b"moof", &[0; 16]));
        before.extend_from_slice(&bx(b"mdat", &[5; 64]));
        let at = before.len() as u64;
        let mfra = bx(b"mfra", &[1, 2, 3, 4]);
        before.extend_from_slice(&mfra);

        let mut file = Cursor::new(before.clone());
        let placed = install(&mut file, &index(2)).expect("a write");
        assert_eq!(
            placed,
            Placed::BeforeMfra {
                at,
                moved: mfra.len() as u64
            }
        );
        let after = file.into_inner();
        assert_eq!(
            &after[after.len() - mfra.len()..],
            &mfra[..],
            "mfra is last"
        );
        let mut src = Source::new(Cursor::new(after)).expect("a source");
        assert_eq!(
            mp4::read_index(&mut src)
                .expect("a read")
                .expect("an index"),
            index(2)
        );
    }

    #[test]
    fn a_box_in_the_middle_is_refused_until_a_rewrite_is_asked_for() {
        let mut before = bx(b"ftyp", b"isom");
        before.extend_from_slice(&ffrwd_index_core::index::mp4_uuid_box(&index(3)));
        before.extend_from_slice(&bx(b"mdat", &[5; 64]));
        before.extend_from_slice(&bx(b"moov", &[0; 32]));

        let mut file = Cursor::new(before.clone());
        let err = install(&mut file, &index(4)).expect_err("a refusal");
        assert!(format!("{err}").contains("--rewrite"), "{err}");
        assert_eq!(file.into_inner(), before, "the file was left alone");

        let mut src = Source::new(Cursor::new(before)).expect("a source");
        let mut out: Vec<u8> = Vec::new();
        rewrite(&mut src, &mut out, &index(4)).expect("a rewrite");
        let mut src = Source::new(Cursor::new(out)).expect("a source");
        assert_eq!(
            mp4::read_index(&mut src)
                .expect("a read")
                .expect("an index"),
            index(4)
        );
        // And there is only one of them left.
        let top = mp4::top_level(&mut src).expect("the boxes");
        assert_eq!(top.iter().filter(|header| header.is(b"uuid")).count(), 1);
    }
}
