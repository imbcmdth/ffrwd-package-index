//! Reading the front of samples, which is all a reader of this format
//! has to read.
//!
//! Section 7 puts a unit before the first coded slice of its access
//! unit, so everything this crate is after sits in the leading NAL
//! units or OBUs of a sample and nothing is after the picture starts.
//! That is why a scan reads a prefix rather than a sample: for a
//! keyframe of a 640x360 encode the picture is tens of kilobytes and
//! the records in front of it are a few hundred bytes.
//!
//! The prefix starts small and grows. It ends where the first coded
//! slice or frame OBU begins, and if it ran out inside a NAL or an OBU
//! that is not one of those, it is read on, twice as far, until it does
//! end there or until the whole sample has been read. Each read carries
//! on from where the last one stopped, so a prefix that had to grow
//! four times still costs its own length in bytes and one seek.
//!
//! The first guess is small on purpose. A sample carrying nothing of
//! ours opens with its coded slice, and five bytes settle that: a
//! length prefix and a NAL header. A keyframe carrying parameter sets
//! and a few hundred bytes of records takes one or two more reads, and
//! those are the samples worth spending reads on.
//!
//! Finding where the leading units end is the only part of this that
//! `ffrwd-nal` does not do, and deliberately: that crate is handed
//! whole packets, and a half-read sample is the container reader's own
//! problem. Everything after the cut is the shared crate's.

use std::io::{Read, Seek};

use ffrwd_bmff::source::Source;
use ffrwd_bmff::track::{Sample, Track};
use ffrwd_index_core::SELECT;
use ffrwd_nal::config::Framing;
use ffrwd_nal::{obu, Codec};

use crate::{Error, Result};

/// Which samples a read visits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scan {
    /// The sync samples. That is where section 7's `keyframe` policy
    /// puts every record, the ones whose span ends after the last
    /// keyframe included: those ride the last keyframe looking forward.
    /// What makes reading a file cheap is that this is a handful of
    /// samples out of thousands.
    #[default]
    Keyframes,
    /// Every sample, which `next` and `spread` need.
    All,
}

impl Scan {
    /// The flag's two spellings.
    pub fn parse(text: &str) -> Option<Scan> {
        match text {
            "keyframes" | "keyframe" => Some(Scan::Keyframes),
            "all" => Some(Scan::All),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scan::Keyframes => "keyframes",
            Scan::All => "all",
        }
    }

    /// The samples of a track this scan visits.
    ///
    /// The keyframe scan is the sync samples and nothing else. Section 7
    /// puts every record of a `keyframe` file on a keyframe, the last
    /// one included: a record whose span ends after the last keyframe
    /// rides that keyframe with an `end_off` that looks forward. There
    /// is nothing after the sync samples for a reader to go and find,
    /// which is what lets the same read work on a transport stream,
    /// where the end of the file is not a thing to seek to.
    pub fn samples(self, track: &Track) -> Vec<Sample> {
        match self {
            Scan::All => track.samples.clone(),
            Scan::Keyframes => track
                .samples
                .iter()
                .copied()
                .filter(|sample| sample.keyframe)
                .collect(),
        }
    }
}

/// How much of a sample is read before anything is known about it.
pub const FIRST_PREFIX: usize = 256;

/// One sample's leading units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Carriage {
    /// Which sample, in decode order.
    pub sample: u32,
    /// When it is shown, in milliseconds of presentation time.
    pub pts_ms: i64,
    pub keyframe: bool,
    /// The units of this format found in front of its picture, still
    /// wrapped as `core` hands them back.
    pub units: Vec<Vec<u8>>,
}

/// Every unit in the samples this scan visits.
pub fn carriages<R: Read + Seek>(
    src: &mut Source<R>,
    track: &Track,
    scan: Scan,
) -> Result<Vec<Carriage>> {
    let framing = crate::framing_of(&track.entry.kind, &track.entry.config)?;
    let mut out = Vec::new();
    for sample in scan.samples(track) {
        out.push(Carriage {
            sample: sample.index,
            pts_ms: track.ms(sample.pts),
            keyframe: sample.keyframe,
            units: prefix_units(src, framing, &sample)?,
        });
    }
    Ok(out)
}

/// The units in front of one sample's picture.
pub fn units_of<R: Read + Seek>(
    src: &mut Source<R>,
    track: &Track,
    sample: &Sample,
) -> Result<Vec<Vec<u8>>> {
    let framing = crate::framing_of(&track.entry.kind, &track.entry.config)?;
    prefix_units(src, framing, sample)
}

fn prefix_units<R: Read + Seek>(
    src: &mut Source<R>,
    framing: Framing,
    sample: &Sample,
) -> Result<Vec<Vec<u8>>> {
    let size = sample.size as usize;
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut prefix: Vec<u8> = Vec::new();
    let mut step = FIRST_PREFIX;
    loop {
        let want = step.min(size - prefix.len());
        let more = src.read_at(sample.offset + prefix.len() as u64, want)?;
        let grew = !more.is_empty();
        prefix.extend_from_slice(&more);
        let complete = prefix.len() >= size || !grew;
        match lead_end(&prefix, complete, framing)? {
            Some(cut) => return Ok(units_in(&prefix[..cut], framing)),
            None if complete => {
                // The whole sample has been read and no coded slice
                // turned up in it. Whatever is there is all there is,
                // so take the units out of it and move on: a sample of
                // parameter sets alone is a real thing.
                return Ok(units_in(&prefix, framing));
            }
            None => step = step.saturating_mul(2),
        }
    }
}

/// Where the leading units end: at the first coded slice or frame OBU.
///
/// `None` means the prefix ran out inside something that is not one of
/// those, so a longer prefix would say more.
fn lead_end(prefix: &[u8], complete: bool, framing: Framing) -> Result<Option<usize>> {
    match framing {
        Framing::LengthPrefixed { codec, length_size } => {
            if !(1..=4).contains(&length_size) {
                return Err(Error::Format(
                    "a NAL length prefix that is not 1 to 4 bytes",
                ));
            }
            lead_end_nals(prefix, complete, length_size, codec)
        }
        Framing::Av1 => lead_end_obus(prefix, complete),
        // A sample entry never says Annex B, and `crate::framing_of` is
        // where a scan's framing comes from, so this is unreachable
        // rather than a case with bytes behind it.
        Framing::AnnexB(_) => Err(Error::Format(
            "a container track framed as an elementary stream",
        )),
    }
}

fn lead_end_nals(
    prefix: &[u8],
    complete: bool,
    length_size: usize,
    codec: Codec,
) -> Result<Option<usize>> {
    let short = |cut: usize| Ok(if complete { Some(cut) } else { None });
    let mut at = 0usize;
    loop {
        if at >= prefix.len() {
            return short(at);
        }
        let Some(header) = prefix.get(at..at + length_size) else {
            return short(at);
        };
        let length = header
            .iter()
            .fold(0usize, |value, byte| value << 8 | usize::from(*byte));
        let start = at + length_size;
        let end = start
            .checked_add(length)
            .ok_or(Error::Format("a NAL length that overflows the sample"))?;
        let head_len = codec.header_len();
        let Some(head) = prefix.get(start..start + head_len) else {
            return short(at);
        };
        if codec.is_vcl(head) {
            // The picture begins here, and section 7 puts every unit
            // before it.
            return Ok(Some(at));
        }
        if end > prefix.len() {
            return short(at);
        }
        at = end;
    }
}

fn lead_end_obus(prefix: &[u8], complete: bool) -> Result<Option<usize>> {
    let short = |cut: usize| Ok(if complete { Some(cut) } else { None });
    let mut at = 0usize;
    loop {
        if at >= prefix.len() {
            return short(at);
        }
        let header = prefix[at];
        if header & 0x80 != 0 {
            return Err(Error::Codec(ffrwd_index_core::Error::Malformed(
                "an OBU with its forbidden bit set",
            )));
        }
        let kind = header >> 3 & 0x0f;
        if matches!(
            kind,
            obu::OBU_FRAME
                | obu::OBU_FRAME_HEADER
                | obu::OBU_REDUNDANT_FRAME_HEADER
                | obu::OBU_TILE_GROUP
        ) {
            return Ok(Some(at));
        }
        let mut cursor = at + 1 + usize::from(header & 0x04 != 0);
        let end = if header & 0x02 != 0 {
            let Ok((size, next)) = obu::leb128(prefix, cursor) else {
                return short(at);
            };
            cursor = next;
            let size =
                usize::try_from(size).map_err(|_| Error::Format("an OBU wider than memory"))?;
            cursor
                .checked_add(size)
                .ok_or(Error::Format("an OBU length that overflows the sample"))?
        } else {
            // An OBU with no size field runs to the end of the sample,
            // so a prefix cannot say where it ends.
            if !complete {
                return Ok(None);
            }
            prefix.len()
        };
        if end > prefix.len() {
            return short(at);
        }
        at = end;
    }
}

/// The units of this format in bytes already cut at a boundary.
fn units_in(bytes: &[u8], framing: Framing) -> Vec<Vec<u8>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    framing.payloads(bytes, SELECT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_bmff::track::{Handler, SampleEntry};
    use ffrwd_index_core::message::Encoding;
    use ffrwd_index_core::message::{Message, Space, Unit};
    use ffrwd_index_core::METADATA_TYPE;
    use ffrwd_nal::sei;
    use std::io::Cursor;

    /// A one-sample track of a kind this format carries. The four
    /// characters and the record are what the scan's framing comes
    /// from, so they are what a fixture has to get right.
    fn one_track(kind: &[u8; 4], config: Vec<u8>, samples: Vec<Sample>) -> Track {
        Track::from_parts(
            Handler::Video,
            1000,
            SampleEntry {
                kind: *kind,
                config,
                ..SampleEntry::default()
            },
            samples,
        )
    }

    /// An `avcC` declaring a four-byte length prefix, which is what
    /// every muxer writes.
    fn avcc() -> Vec<u8> {
        vec![1, 0x64, 0x00, 0x0d, 0xff, 0xe1, 0x00]
    }

    fn h264_track(samples: Vec<Sample>) -> Track {
        one_track(b"avc1", avcc(), samples)
    }

    fn sample_at(size: u32) -> Sample {
        Sample {
            index: 0,
            offset: 0,
            size,
            keyframe: true,
            ..Sample::default()
        }
    }

    fn unit() -> Vec<u8> {
        let mut space = Space::new(1, 4, Encoding::F32);
        space.model = "test:model".into();
        Unit::new(vec![Message::Space(space)]).encode()
    }

    fn nal(length_size: usize, bytes: &[u8]) -> Vec<u8> {
        let mut out = bytes.len().to_be_bytes()[8 - length_size..].to_vec();
        out.extend_from_slice(bytes);
        out
    }

    /// A sample shaped the way an MP4 keyframe is: parameter sets, the
    /// SEI carrying a unit, then a big coded slice.
    fn h264_sample(picture: usize) -> Vec<u8> {
        let mut out = nal(4, &[0x67, 0x64, 0, 13, 0xac]);
        out.extend_from_slice(&nal(4, &[0x68, 0xeb, 0xe3, 0xcb]));
        out.extend_from_slice(&nal(4, &sei::write_user_data(&unit(), Codec::H264)));
        let mut slice = vec![0x65, 0x88];
        slice.extend(std::iter::repeat_n(0x42u8, picture));
        out.extend_from_slice(&nal(4, &slice));
        out
    }

    #[test]
    fn a_prefix_stops_at_the_picture_and_grows_when_it_has_to() {
        let sample = h264_sample(200_000);
        let size = sample.len() as u32;
        let mut src = Source::new(Cursor::new(sample.clone())).expect("a source");
        let track = h264_track(vec![sample_at(size)]);
        src.reset_tally();
        let found = units_of(&mut src, &track, &track.samples[0]).expect("units");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], unit());
        assert!(
            src.tally().bytes_read < 9000,
            "the picture was read as well: {:?}",
            src.tally()
        );

        // A unit larger than the first prefix costs one more read and
        // still comes back whole.
        let mut big = Space::new(2, 4, Encoding::F32);
        big.model = "x".repeat(20_000);
        let unit = Unit::new(vec![Message::Space(big)]).encode();
        let mut sample = nal(4, &sei::write_user_data(&unit, Codec::H264));
        let slice_at = sample.len();
        sample.extend_from_slice(&nal(4, &[0x65, 0x88, 0x42, 0x42]));
        let size = sample.len() as u32;
        let mut src = Source::new(Cursor::new(sample)).expect("a source");
        let track = h264_track(vec![sample_at(size)]);
        let found = units_of(&mut src, &track, &track.samples[0]).expect("units");
        assert_eq!(found, vec![unit]);
        assert!(
            slice_at > FIRST_PREFIX,
            "the fixture did not need a second read"
        );
    }

    #[test]
    fn an_av1_sample_stops_at_its_first_frame_obu() {
        let mut sample = vec![obu::OBU_SEQUENCE_HEADER << 3 | 0x02, 3, 1, 2, 3];
        sample.extend_from_slice(&obu::write_metadata(METADATA_TYPE, &unit()));
        let cut = sample.len();
        sample.push(obu::OBU_FRAME << 3 | 0x02);
        obu::put_leb128(&mut sample, 1000);
        sample.extend(std::iter::repeat_n(0x11u8, 1000));
        let size = sample.len() as u32;
        let mut src = Source::new(Cursor::new(sample)).expect("a source");
        let track = one_track(b"av01", vec![0x81, 0x05], vec![sample_at(size)]);
        // A prefix that reaches the frame OBU's header says where the
        // lead ends; one that stops short says nothing yet.
        assert_eq!(
            lead_end(
                &src.read_at(0, cut + 4).expect("a prefix"),
                false,
                Framing::Av1
            )
            .expect("a cut"),
            Some(cut)
        );
        assert_eq!(
            lead_end(&src.read_at(0, 8).expect("a prefix"), false, Framing::Av1).expect("a cut"),
            None
        );
        let found = units_of(&mut src, &track, &track.samples[0]).expect("units");
        assert_eq!(found, vec![unit()]);
    }

    #[test]
    fn random_bytes_in_a_sample_never_panic() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        for (kind, config) in [
            (b"avc1", avcc()),
            (b"hvc1", {
                let mut hvcc = vec![1u8; 23];
                hvcc[21] = 0xf3;
                hvcc
            }),
            (b"av01", vec![0x81, 0x05]),
        ] {
            for _ in 0..500 {
                let mut bytes = Vec::new();
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                for _ in 0..(seed >> 40) % 64 {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    bytes.push((seed >> 33) as u8);
                }
                let size = bytes.len() as u32;
                let mut src = Source::new(Cursor::new(bytes)).expect("a source");
                let track = one_track(kind, config.clone(), vec![sample_at(size)]);
                let _ = units_of(&mut src, &track, &track.samples[0]);
            }
        }
    }
}
