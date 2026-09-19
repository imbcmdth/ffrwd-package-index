//! The containers the format's section 8 names, read natively.
//!
//! `core` is the codec and knows nothing about files. This crate is the
//! other half of reading one: it finds the video track of an MP4 or a
//! Matroska file, works out where every sample is and when it is shown,
//! reads the front of each sample, and hands the units it finds there
//! to `core`. It also finds the file index: the MP4 `uuid` box of
//! section 8, and the Matroska attachment.
//!
//! Three things shape the whole crate.
//!
//! **It reads as little as it can.** Section 7 puts whole records on
//! keyframes for files, and a record rides before the first coded slice
//! of its access unit. So a reader after records does not need the
//! pictures: it needs the first few hundred bytes of each sync sample.
//! [`Scan::Keyframes`] does exactly that and [`Source`] counts what it
//! cost, because the ratio is the point of the placement policy.
//!
//! **It reads through [`Read`] and [`Seek`]**, never a path, so every
//! parser in here runs against a `Cursor<Vec<u8>>` and the fuzz tests
//! can hand it any bytes at all.
//!
//! **It is parsing strangers' files.** Every length is checked against
//! what the file actually holds before a byte of it is allocated, all
//! of it through [`Source::read_at`], which is the only place in the
//! crate that turns a number into a buffer. Recursion into child boxes
//! and elements is bounded. Truncations and random bytes come back as
//! [`Error`], never as a panic and never as a gigabyte of zeroes.
//!
//! - [`mp4`]: ISO base media files, fragmented and not.
//! - [`mkv`]: Matroska and WebM.
//! - [`scan`]: the sample prefixes, and the units in them.
//! - [`source`]: the counted reader everything goes through.
//! - [`write`]: putting the index into an MP4 in place.

#![forbid(unsafe_code)]

pub mod mkv;
pub mod mp4;
pub mod scan;
pub mod source;
pub mod write;

pub use scan::{carriages, Carriage, Scan};
pub use source::{Source, Tally};

use std::io::{Read, Seek};

/// What went wrong reading a container.
#[derive(Debug)]
pub enum Error {
    /// The file would not read.
    Io(std::io::Error),
    /// The bytes are not the shape the container's own rules require.
    /// Every truncation lands here.
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
            Error::Io(err) => write!(f, "{err}"),
            Error::Format(what) => write!(f, "{what}"),
            Error::Unsupported(what) => write!(f, "{what}"),
            Error::Codec(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<ffrwd_index_core::Error> for Error {
    fn from(err: ffrwd_index_core::Error) -> Self {
        Error::Codec(err)
    }
}

/// The crate's result.
pub type Result<T> = core::result::Result<T, Error>;

/// Which codec a video track holds.
///
/// The three the format has carriage for, and no others: a track this
/// crate cannot find records in is refused by name rather than scanned
/// for bytes that mean nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackCodec {
    H264,
    H265,
    Av1,
}

impl TrackCodec {
    /// The `avc`-module codec, for the two that have one. AV1 is OBUs
    /// and has no NAL framing at all.
    pub fn nal_codec(self) -> Option<ffrwd_index_core::avc::Codec> {
        match self {
            TrackCodec::H264 => Some(ffrwd_index_core::avc::Codec::H264),
            TrackCodec::H265 => Some(ffrwd_index_core::avc::Codec::H265),
            TrackCodec::Av1 => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TrackCodec::H264 => "h264",
            TrackCodec::H265 => "hevc",
            TrackCodec::Av1 => "av1",
        }
    }
}

/// One coded sample: where it is, when it is shown, and whether a
/// reader may start at it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Its place in decode order, from zero.
    pub index: u32,
    /// Where its bytes begin in the file.
    pub offset: u64,
    /// How many bytes they are.
    pub size: u32,
    /// Presentation time in the track's own ticks, with the edit list
    /// already applied: the number ffprobe divides by the time base to
    /// print `pts_time`.
    pub pts: i64,
    /// Whether the sample is a random access point.
    pub keyframe: bool,
}

/// The video track of a file, and every sample of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoTrack {
    pub codec: TrackCodec,
    /// Ticks a second, the denominator ffprobe calls the time base.
    pub timescale: u32,
    /// The NAL length prefix, for the two codecs that have one; `None`
    /// for AV1, whose samples are bare OBUs.
    pub length_size: Option<usize>,
    /// The codec's own out-of-band header (`avcC`, `hvcC` or `av1C`).
    pub codec_private: Vec<u8>,
    /// Every sample, in decode order.
    pub samples: Vec<Sample>,
    /// What the file said about its edit list, for a reader that wants
    /// to say why a time is what it is.
    pub start_shift: i64,
}

impl VideoTrack {
    /// A tick count as milliseconds of presentation time.
    pub fn ms(&self, ticks: i64) -> i64 {
        let scale = i128::from(self.timescale.max(1));
        let value = i128::from(ticks) * 1000;
        // Round to nearest, away from zero, so a 1/15360 tick lands
        // where ffprobe's decimal says it does.
        let rounded = if value >= 0 {
            (value + scale / 2) / scale
        } else {
            (value - scale / 2) / scale
        };
        rounded.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }

    /// The samples a scan of this mode visits.
    ///
    /// The keyframe scan is the sync samples **and the last sample of
    /// the track**. Section 7: a record whose span ends after the last
    /// keyframe has no keyframe to ride, so it rides the last access
    /// unit, and a reader taking the fast path reads that sample as
    /// well. One sample is what it costs.
    pub fn scanned(&self, scan: Scan) -> Vec<Sample> {
        match scan {
            Scan::All => self.samples.clone(),
            Scan::Keyframes => {
                let mut out: Vec<Sample> = self
                    .samples
                    .iter()
                    .copied()
                    .filter(|sample| sample.keyframe)
                    .collect();
                if let Some(last) = self.samples.last() {
                    // The samples are in decode order, so the last one
                    // goes on the end and the order holds.
                    if out.last().map(|sample| sample.index) != Some(last.index) {
                        out.push(*last);
                    }
                }
                out
            }
        }
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
