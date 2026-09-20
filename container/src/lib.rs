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
//! attachment carrying the file index.
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

#![forbid(unsafe_code)]

pub mod mkv;
pub mod scan;

pub use scan::{carriages, Carriage, Scan};

use std::io::{Read, Seek};

use ffrwd_bmff::patch::Selector;
use ffrwd_bmff::source::Source;
use ffrwd_bmff::track::{Sample, Track};
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

/// A video track of a file, whichever container it came out of.
///
/// This is the small union the scan needs and nothing more: an MP4
/// track is `ffrwd_bmff::track::Track` and a Matroska track is not a
/// `Track` at all, so what the two have in common is stated here rather
/// than one of them being bent into the other's shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Video {
    /// How the samples are framed, which the sample entry decides.
    pub framing: Framing,
    /// Ticks a second, the denominator ffprobe calls the time base.
    pub timescale: u32,
    /// The codec's own out-of-band header (`avcC`, `hvcC` or `av1C`),
    /// exactly as the file carried it. Both containers have one and
    /// neither reads it: parsing it is `ffrwd-nal`'s job.
    pub config: Vec<u8>,
    /// Every sample, in decode order.
    pub samples: Vec<Sample>,
    /// What the file said about its edit list, for a reader that wants
    /// to say why a time is what it is.
    pub start_shift: i64,
}

impl Video {
    /// The video track of an ISO base media file, as a scan sees it.
    pub fn of_track(track: &Track) -> Result<Video> {
        Ok(Video {
            framing: framing_of(&track.entry.kind, &track.entry.config)?,
            timescale: track.timescale,
            config: track.entry.config.clone(),
            samples: track.samples.clone(),
            start_shift: track.start_shift,
        })
    }

    /// A tick count as milliseconds of presentation time.
    pub fn ms(&self, ticks: i64) -> i64 {
        ffrwd_bmff::time::ms(ticks, self.timescale)
    }

    /// The samples a scan of this mode visits.
    ///
    /// The keyframe scan is the sync samples and nothing else. Section 7
    /// puts every record of a `keyframe` file on a keyframe, the last
    /// one included: a record whose span ends after the last keyframe
    /// rides that keyframe with an `end_off` that looks forward. There
    /// is nothing after the sync samples for a reader to go and find,
    /// which is what lets the same read work on a transport stream,
    /// where the end of the file is not a thing to seek to.
    pub fn scanned(&self, scan: Scan) -> Vec<Sample> {
        match scan {
            Scan::All => self.samples.clone(),
            Scan::Keyframes => self
                .samples
                .iter()
                .copied()
                .filter(|sample| sample.keyframe)
                .collect(),
        }
    }
}

/// How a sample entry's samples are framed.
///
/// A container's samples are never Annex B: both NAL sample entries
/// carry a record that declares a length prefix, and AV1 samples are
/// bare OBUs. That is why the entry decides here rather than the
/// extradata sniffing a pipeline's pad needs. A track this format has
/// no carriage for is refused by name rather than scanned for bytes
/// that would mean nothing.
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
}
