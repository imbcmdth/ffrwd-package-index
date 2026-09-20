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
//! Three small things sit beside them, each because it is about having
//! two containers rather than about either one: [`kind_of`], which says
//! which of them a file is; [`framing_of`], which turns a sample entry
//! into the framing a scan reads by; and [`Error`], which is where
//! Matroska's own faults, `ffrwd-bmff`'s and the format's meet. The one
//! thing this crate says about a box is [`INDEX_BOX`].

#![forbid(unsafe_code)]

pub mod mkv;
pub mod scan;

pub use scan::{carriages, Carriage, Scan};

use std::io::{Read, Seek};

use ffrwd_bmff::patch::Selector;
use ffrwd_bmff::source::Source;
use ffrwd_nal::config::{avcc_length_size, hvcc_length_size, Framing};
use ffrwd_nal::Codec;

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
    /// crate will not read. Said rather than guessed at.
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

impl From<ffrwd_nal::Error> for Error {
    fn from(err: ffrwd_nal::Error) -> Self {
        Error::Codec(err.into())
    }
}

/// The crate's result.
pub type Result<T> = core::result::Result<T, Error>;

/// How a track's samples are framed, from its sample entry.
///
/// Not `ffrwd_nal::config::framing_of`, which answers a different
/// question and is right for the caller it was written for. That one
/// takes ffmpeg's codec name and decides between Annex B and a length
/// prefix by looking at the extradata, which is what a pipeline's pad
/// has to do. A container's sample entry already says both things and
/// says them better:
///
/// - The four characters a sample entry carries are not codec names.
///   `framing_of` knows `avc1`, `hvc1` and `hev1` because they happen
///   to spell codecs too, and has no case for `avc3` or `av01`, which
///   `ffprobe` will hand out of real files all day.
/// - A sample in either container is never Annex B. `framing_of` reads
///   a short or damaged record as Annex B, which for a track is not a
///   guess worth making: the entry said length-prefixed, and a damaged
///   `avcC` means the width is unknown, not that the framing changed.
///
/// A track this format has no carriage for is refused by name rather
/// than scanned for bytes that would mean nothing.
pub fn framing_of(kind: &[u8; 4], config: &[u8]) -> Result<Framing> {
    match kind {
        b"avc1" | b"avc3" => Ok(Framing::LengthPrefixed {
            codec: Codec::H264,
            length_size: avcc_length_size(config),
        }),
        b"hvc1" | b"hev1" => Ok(Framing::LengthPrefixed {
            codec: Codec::H265,
            length_size: hvcc_length_size(config),
        }),
        b"av01" => Ok(Framing::Av1),
        other => Err(Error::Unsupported(format!(
            "the video track is {}, which this format has no carriage for",
            String::from_utf8_lossy(other)
        ))),
    }
}

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

    /// The sample entry names the framing, and a track this format
    /// cannot carry says so by name.
    #[test]
    fn a_sample_entry_says_how_its_samples_are_framed() {
        let mut avcc = vec![1u8, 0x64, 0x00, 0x0d, 0xff, 0xe1, 0x00];
        assert_eq!(
            framing_of(b"avc1", &avcc).expect("a framing"),
            Framing::LengthPrefixed {
                codec: Codec::H264,
                length_size: 4
            }
        );
        avcc[4] = 0xfd;
        assert_eq!(
            framing_of(b"avc3", &avcc).expect("a framing"),
            Framing::LengthPrefixed {
                codec: Codec::H264,
                length_size: 2
            }
        );
        let mut hvcc = vec![1u8; 23];
        hvcc[21] = 0xf3;
        assert_eq!(
            framing_of(b"hev1", &hvcc).expect("a framing"),
            Framing::LengthPrefixed {
                codec: Codec::H265,
                length_size: 4
            }
        );
        assert_eq!(
            framing_of(b"av01", &[0x81, 0x05]).expect("a framing"),
            Framing::Av1
        );
        let err = framing_of(b"vp09", &[]).expect_err("a refusal");
        assert!(format!("{err}").contains("vp09"), "{err}");
    }

    /// Why this is not `ffrwd_nal::config::framing_of` with the four
    /// characters passed through. That function answers a pipeline
    /// pad's question, from ffmpeg's codec name and the extradata, and
    /// on a sample entry it gets two things wrong that matter here.
    #[test]
    fn the_shared_framing_answers_a_pads_question_and_not_a_tracks() {
        use ffrwd_nal::config::framing_of as pad_framing_of;

        // Two of the five entry types a track really carries are not
        // codec names and have no case there.
        for kind in [b"avc3", b"av01"] {
            let name = std::str::from_utf8(kind).expect("four ascii characters");
            assert_eq!(
                pad_framing_of(name, &[]),
                Err(ffrwd_nal::Error::UnknownCodec)
            );
            assert!(framing_of(kind, &[0x81, 0x05]).is_ok(), "{name}");
        }

        // And a record too short to read is Annex B to a pad, which is
        // the right guess for a stream with no extradata and the wrong
        // one for a sample entry that said length-prefixed.
        assert_eq!(
            pad_framing_of("avc1", &[1, 0x64]).expect("a framing"),
            Framing::AnnexB(Codec::H264)
        );
        assert_eq!(
            framing_of(b"avc1", &[1, 0x64]).expect("a framing"),
            Framing::LengthPrefixed {
                codec: Codec::H264,
                length_size: 4
            }
        );
    }
}
