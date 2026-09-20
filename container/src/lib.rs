//! The containers the format's section 8 names, read natively.
//!
//! Most of what used to be here is somewhere else now. The boxes of an
//! ISO base media file, where its samples are, when they are shown, and
//! putting a top-level box into one in place are `ffrwd-bmff`'s; the
//! NAL units and OBUs inside a sample are `ffrwd-nal`'s. Both are
//! dependency-free crates shared with the other ffrwd packages that
//! used to carry their own copy of the same code, and both are fuzzed
//! and truncated against real files in their own repositories.
//!
//! What is left is the two things neither of them can answer.
//!
//! **Matroska**, which no other package here reads, and which is not an
//! ISO base media file: its own elements, its own timestamps, its own
//! attachment carrying the file index. It hands back the same
//! `ffrwd_bmff::track::Track` the MP4 reader does, built with
//! `Track::from_parts`, so one scan serves both containers.
//!
//! **The prefix scan**, which is the one place the box layer and the
//! byte layer meet. Section 7 puts whole records on keyframes for
//! files, and a record rides before the first coded slice of its access
//! unit. So a reader after records does not need the pictures: it needs
//! the first few hundred bytes of each sync sample. [`Scan::Keyframes`]
//! does exactly that and `ffrwd_bmff::source::Source` counts what it
//! cost, because the ratio is the point of the placement policy.
//!
//! Both read through [`Read`] and [`Seek`], never a path, so every
//! parser in here runs against a `Cursor<Vec<u8>>` and the fuzz tests
//! can hand it any bytes at all, and both go through `Source`, which is
//! the only place a number out of a file becomes a buffer.
//!
//! - [`mkv`]: Matroska and WebM.
//! - [`scan`]: the sample prefixes, and the units in them.
//!
//! Two small things sit beside them, each because it is about having
//! two containers rather than about either one: [`kind_of`], which says
//! which of them a file is, and [`Error`], which is where Matroska's
//! own faults, `ffrwd-bmff`'s and the format's meet. The one thing this
//! crate says about a box is [`INDEX_BOX`].

#![forbid(unsafe_code)]

pub mod mkv;
pub mod scan;

pub use scan::{carriages, Carriage, Scan};

use std::io::{Read, Seek};

use ffrwd_bmff::patch::Selector;
use ffrwd_bmff::source::Source;

/// Section 8's box: the MP4 top-level `uuid` box a file index travels
/// in, named by this format's UUID.
///
/// It is what `ffrwd_bmff::patch` finds, reads, installs and rewrites
/// by, so the format's one claim about MP4 is one constant rather than
/// a box builder.
pub const INDEX_BOX: Selector = Selector::Uuid(ffrwd_index_core::UUID);

/// What went wrong reading a container.
#[derive(Debug)]
pub enum Error {
    /// A read of the file failed, or its boxes are not the shape the
    /// standard requires. Every truncation of an MP4 lands here.
    Read(ffrwd_bmff::Error),
    /// The bytes are not the shape Matroska's own rules require.
    Format(&'static str),
    /// The file is one of these containers and holds something this
    /// crate will not read: a codec this format has no carriage for, or
    /// a decoder configuration record too damaged to say how its
    /// samples are framed. Said rather than guessed at.
    Unsupported(String),
    /// The codec refused what came out of the container.
    Codec(ffrwd_index_core::Error),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Read(err) => write!(f, "{err}"),
            Error::Format(what) => write!(f, "{what}"),
            Error::Unsupported(what) => write!(f, "{what}"),
            Error::Codec(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Read(ffrwd_bmff::Error::Io(err))
    }
}

impl From<ffrwd_bmff::Error> for Error {
    fn from(err: ffrwd_bmff::Error) -> Self {
        Error::Read(err)
    }
}

impl From<ffrwd_index_core::Error> for Error {
    fn from(err: ffrwd_index_core::Error) -> Self {
        Error::Codec(err)
    }
}

/// The crate's result.
pub type Result<T> = core::result::Result<T, Error>;

/// Which container a file is, by its first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Mp4,
    Matroska,
}

/// What a file looks like from its first bytes.
///
/// An EBML file opens with the EBML header's id; an ISO base media file
/// opens with a box, and the only one worth recognising by name is
/// `ftyp`, so anything else with a plausible box header at zero is read
/// as MP4 and told off later if it is not.
///
/// This is here and not in `ffrwd-bmff` because choosing between two
/// containers belongs to the crate that reads both, which is this one.
pub fn kind_of<R: Read + Seek>(src: &mut Source<R>) -> Result<Kind> {
    let head = src.read_at(0, 12)?;
    if head.len() >= 4 && head[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        return Ok(Kind::Matroska);
    }
    if head.len() >= 8 {
        let kind = &head[4..8];
        if kind
            .iter()
            .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
        {
            return Ok(Kind::Mp4);
        }
    }
    Err(Error::Format(
        "the file opens as neither an EBML document nor an ISO box",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_index_core::index::FileIndex;
    use ffrwd_index_core::message::{Encoding, Message, Space};

    /// Section 8's claim about MP4, which is the one thing this format
    /// says about a box: the index travels in a top-level `uuid` box
    /// whose extended type is the format's own UUID.
    #[test]
    fn the_mp4_box_is_a_uuid_box_of_this_format() {
        let mut space = Space::new(1, 16, Encoding::I8);
        space.model = "test:model".into();
        let index = FileIndex::build(vec![(0, Message::Space(space))]);
        let bytes = index.encode();
        let boxed = INDEX_BOX.boxed(&bytes);
        assert_eq!(&boxed[4..8], b"uuid");
        assert_eq!(&boxed[8..24], &ffrwd_index_core::UUID);
        assert_eq!(
            u32::from_be_bytes(boxed[..4].try_into().expect("four bytes")) as usize,
            boxed.len()
        );
        assert_eq!(FileIndex::parse(&boxed[24..]).expect("an index"), index);
    }
}
