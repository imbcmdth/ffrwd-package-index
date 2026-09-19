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

use std::io::{Read, Seek};

use ffrwd_index_core::avc;
use ffrwd_index_core::obu;

use crate::{Error, Result, Sample, Source, TrackCodec, VideoTrack};

/// Which samples a read visits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scan {
    /// Sync samples only, which is where section 7's `keyframe` policy
    /// puts every record and what makes reading a file cheap.
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
    track: &VideoTrack,
    scan: Scan,
) -> Result<Vec<Carriage>> {
    let mut out = Vec::new();
    for sample in track.scanned(scan) {
        out.push(Carriage {
            sample: sample.index,
            pts_ms: track.ms(sample.pts),
            keyframe: sample.keyframe,
            units: units_of(src, track, &sample)?,
        });
    }
    Ok(out)
}

/// The units in front of one sample's picture.
pub fn units_of<R: Read + Seek>(
    src: &mut Source<R>,
    track: &VideoTrack,
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
        match lead_end(&prefix, complete, track)? {
            Some(cut) => return units_in(&prefix[..cut], track),
            None if complete => {
                // The whole sample has been read and no coded slice
                // turned up in it. Whatever is there is all there is,
                // so take the units out of it and move on: a sample of
                // parameter sets alone is a real thing.
                return units_in(&prefix, track);
            }
            None => step = step.saturating_mul(2),
        }
    }
}

/// Where the leading units end: at the first coded slice or frame OBU.
///
/// `None` means the prefix ran out inside something that is not one of
/// those, so a longer prefix would say more.
fn lead_end(prefix: &[u8], complete: bool, track: &VideoTrack) -> Result<Option<usize>> {
    match track.codec.nal_codec() {
        Some(codec) => {
            let length_size = track.length_size.unwrap_or(4);
            if !(1..=4).contains(&length_size) {
                return Err(Error::Format(
                    "a NAL length prefix that is not 1 to 4 bytes",
                ));
            }
            lead_end_nals(prefix, complete, length_size, codec)
        }
        None => lead_end_obus(prefix, complete),
    }
}

fn lead_end_nals(
    prefix: &[u8],
    complete: bool,
    length_size: usize,
    codec: avc::Codec,
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
fn units_in(bytes: &[u8], track: &VideoTrack) -> Result<Vec<Vec<u8>>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    match track.codec {
        TrackCodec::Av1 => Ok(obu::units_obu(bytes)),
        _ => {
            let codec = track
                .codec
                .nal_codec()
                .ok_or(Error::Format("a NAL codec with no NAL framing"))?;
            Ok(avc::units_length_prefixed(
                bytes,
                track.length_size.unwrap_or(4),
                codec,
            )?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_index_core::message::Encoding;
    use ffrwd_index_core::message::{Message, Space, Unit};
    use std::io::Cursor;

    fn one_track(codec: TrackCodec, samples: Vec<Sample>) -> VideoTrack {
        VideoTrack {
            codec,
            timescale: 1000,
            length_size: match codec {
                TrackCodec::Av1 => None,
                _ => Some(4),
            },
            codec_private: Vec::new(),
            samples,
            start_shift: 0,
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
        out.extend_from_slice(&nal(4, &avc::wrap_unit(&unit(), avc::Codec::H264)));
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
        let track = one_track(
            TrackCodec::H264,
            vec![Sample {
                index: 0,
                offset: 0,
                size,
                pts: 0,
                keyframe: true,
            }],
        );
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
        let mut sample = nal(4, &avc::wrap_unit(&unit, avc::Codec::H264));
        let slice_at = sample.len();
        sample.extend_from_slice(&nal(4, &[0x65, 0x88, 0x42, 0x42]));
        let size = sample.len() as u32;
        let mut src = Source::new(Cursor::new(sample)).expect("a source");
        let track = one_track(
            TrackCodec::H264,
            vec![Sample {
                index: 0,
                offset: 0,
                size,
                pts: 0,
                keyframe: true,
            }],
        );
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
        sample.extend_from_slice(&obu::write_metadata_obu(&unit()));
        let cut = sample.len();
        sample.push(obu::OBU_FRAME << 3 | 0x02);
        obu::put_leb128(&mut sample, 1000);
        sample.extend(std::iter::repeat_n(0x11u8, 1000));
        let size = sample.len() as u32;
        let mut src = Source::new(Cursor::new(sample)).expect("a source");
        let track = one_track(
            TrackCodec::Av1,
            vec![Sample {
                index: 0,
                offset: 0,
                size,
                pts: 0,
                keyframe: true,
            }],
        );
        // A prefix that reaches the frame OBU's header says where the
        // lead ends; one that stops short says nothing yet.
        assert_eq!(
            lead_end(&src.read_at(0, cut + 4).expect("a prefix"), false, &track).expect("a cut"),
            Some(cut)
        );
        assert_eq!(
            lead_end(&src.read_at(0, 8).expect("a prefix"), false, &track).expect("a cut"),
            None
        );
        let found = units_of(&mut src, &track, &track.samples[0]).expect("units");
        assert_eq!(found, vec![unit()]);
    }

    #[test]
    fn random_bytes_in_a_sample_never_panic() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        for codec in [TrackCodec::H264, TrackCodec::H265, TrackCodec::Av1] {
            for _ in 0..500 {
                let mut bytes = Vec::new();
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                for _ in 0..(seed >> 40) % 64 {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    bytes.push((seed >> 33) as u8);
                }
                let size = bytes.len() as u32;
                let mut src = Source::new(Cursor::new(bytes)).expect("a source");
                let track = one_track(
                    codec,
                    vec![Sample {
                        index: 0,
                        offset: 0,
                        size,
                        pts: 0,
                        keyframe: true,
                    }],
                );
                let _ = units_of(&mut src, &track, &track.samples[0]);
            }
        }
    }
}
