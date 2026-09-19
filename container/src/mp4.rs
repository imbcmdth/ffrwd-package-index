//! ISO base media files: MP4, MOV, 3GP and the fragmented shape of the
//! same boxes.
//!
//! Only what a reader of this format needs is parsed. From `moov`: the
//! video `trak`, its `mdhd` timescale, its `stsd` (which codec, and the
//! NAL length prefix its `avcC`, `hvcC` or `av1C` declares), and the
//! sample tables `stts`, `ctts`, `stss`, `stsc`, `stsz` or `stz2`, and
//! `stco` or `co64`. From a fragmented file: `mvex/trex` for the
//! defaults and every `moof/traf`'s `tfhd`, `tfdt` and `trun`. `sidx`
//! is ignored: it is a hint about where fragments are, and the
//! fragments themselves say it better.
//!
//! **The edit list.** ffprobe prints a packet's `pts_time` after the
//! `elst` has moved it, so this does the same thing, in one rule:
//!
//! > A sample's composition time is `dts + ctts`, with `dts` running
//! > from zero through the `stts` deltas. The file's zero is the
//! > composition time of the first sample, in decode order, whose
//! > composition time is at or after the `media_time` of the first
//! > edit that is not empty. Presentation time is a sample's
//! > composition time minus that zero, plus the durations of the empty
//! > edits (`media_time` -1) in front of it, rescaled from the movie
//! > timescale to the media one. A file with no `elst` keeps its
//! > composition times as they are.
//!
//! The snap in the middle of that is the part worth saying twice.
//! ffmpeg does not subtract `media_time`; it drops the samples before
//! it and makes the first one it keeps zero. For every file where
//! `media_time` lands exactly on a sample, which is every ordinary
//! encode, the two are the same number. They part company when it does
//! not: raw H.264 remuxed into MP4 gets a `media_time` of two frames at
//! a nominal rate while the samples themselves are a tick shorter, and
//! the difference there is 20 milliseconds, which is most of a frame.
//! A sample before the zero keeps its negative presentation time here
//! rather than being dropped, which is also what ffprobe prints for it.
//!
//! The four shapes this was checked against, all ffmpeg's own output:
//! an H.264 encode with B-frames (one edit, `media_time` 1024 of a
//! 15360 timescale, first frame at zero); the same with
//! `-output_ts_offset 1` (an empty edit of a second in front of it,
//! first frame at one second); the same with `+negative_cts_offsets`
//! (`media_time` 0, a version 1 `ctts` with negative offsets, first
//! frame at zero); and `frag_keyframe+empty_moov`, which has no edit
//! list at all and whose first frame is therefore at 0.067 rather than
//! at zero, exactly as ffprobe says.
//!
//! What is left out: an edit list of several non-empty entries, which
//! ffmpeg implements by cutting and repeating samples. Nothing a muxer
//! writes has one, and guessing at it would be worse than saying so, so
//! the first non-empty entry decides and the rest are ignored.
//!
//! The box nesting walked here is a fixed path, so there is no
//! recursion to bound: `moov` and each `moof` are read once, whole, and
//! their children are walked flat.

use std::io::{Read, Seek};

use ffrwd_index_core::avc::{avcc_length_size, hvcc_length_size};

use crate::source::Bytes;
use crate::{Error, Result, Sample, Source, TrackCodec, VideoTrack};

/// The most samples a track may have before this refuses it. Twenty
/// million is about six days at thirty frames a second.
pub const MAX_SAMPLES: usize = 20_000_000;

/// The most top-level boxes a file may have.
pub const MAX_TOP_LEVEL: usize = 1 << 20;

/// The most child boxes one box's body may hold.
pub const MAX_CHILDREN: usize = 1 << 16;

/// Where a sample entry's child boxes start: past the eight bytes of
/// box header, the eight of `SampleEntry`, and the seventy of
/// `VisualSampleEntry`.
const VISUAL_SAMPLE_ENTRY_LEN: usize = 86;

/// One box, as its header describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxHeader {
    pub kind: [u8; 4],
    /// Where the box begins, at its size field.
    pub start: u64,
    /// Where its body begins.
    pub body: u64,
    /// Where it ends.
    pub end: u64,
}

impl BoxHeader {
    pub fn is(&self, kind: &[u8; 4]) -> bool {
        &self.kind == kind
    }

    pub fn body_len(&self) -> u64 {
        self.end.saturating_sub(self.body)
    }
}

/// The header of the box at `at`, or `None` when `limit` leaves no room
/// for one.
///
/// Three sizes, all of them checked: a 32-bit size, `1` for a 64-bit
/// `largesize` that follows, and `0` for a box that runs to `limit`,
/// which at the top level is the end of the file.
pub fn read_header<R: Read + Seek>(
    src: &mut Source<R>,
    at: u64,
    limit: u64,
) -> Result<Option<BoxHeader>> {
    if limit < at || limit - at < 8 {
        return Ok(None);
    }
    let head = src.exact_at(at, 8)?;
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&head[4..8]);
    let short = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    let (size, body) = match short {
        1 => {
            let wide = src.exact_at(at + 8, 8)?;
            let mut value = [0u8; 8];
            value.copy_from_slice(&wide);
            (u64::from_be_bytes(value), at + 16)
        }
        0 => (limit - at, at + 8),
        other => (u64::from(other), at + 8),
    };
    let end = at
        .checked_add(size)
        .ok_or(Error::Format("a box whose size overflows the file"))?;
    if end > limit || body > end {
        return Err(Error::Format("a box that runs past the file or its parent"));
    }
    Ok(Some(BoxHeader {
        kind,
        start: at,
        body,
        end,
    }))
}

/// Every top-level box of a file, in order.
pub fn top_level<R: Read + Seek>(src: &mut Source<R>) -> Result<Vec<BoxHeader>> {
    let limit = src.len();
    let mut out = Vec::new();
    let mut at = 0u64;
    while let Some(header) = read_header(src, at, limit)? {
        if header.end <= at {
            return Err(Error::Format("a box of no length, which would not end"));
        }
        at = header.end;
        out.push(header);
        if out.len() > MAX_TOP_LEVEL {
            return Err(Error::Format("more top-level boxes than a file has"));
        }
    }
    Ok(out)
}

/// One child box of a body already in hand.
#[derive(Clone, Copy, Debug)]
pub struct Child<'a> {
    pub kind: [u8; 4],
    pub body: &'a [u8],
}

impl Child<'_> {
    pub fn is(&self, kind: &[u8; 4]) -> bool {
        &self.kind == kind
    }
}

/// The child boxes of a body, flat.
pub fn children(body: &[u8]) -> Result<Vec<Child<'_>>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= body.len() {
        let short = u32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&body[at + 4..at + 8]);
        let (size, head) = match short {
            1 => {
                let wide = body
                    .get(at + 8..at + 16)
                    .ok_or(Error::Format("a box that ends inside its own size"))?;
                let mut value = [0u8; 8];
                value.copy_from_slice(wide);
                (
                    usize::try_from(u64::from_be_bytes(value))
                        .map_err(|_| Error::Format("a box wider than memory"))?,
                    16usize,
                )
            }
            0 => (body.len() - at, 8usize),
            other => (other as usize, 8usize),
        };
        if size < head || at + size > body.len() {
            return Err(Error::Format("a child box that runs past its parent"));
        }
        out.push(Child {
            kind,
            body: &body[at + head..at + size],
        });
        at += size;
        if out.len() > MAX_CHILDREN {
            return Err(Error::Format("more child boxes than a box has"));
        }
    }
    Ok(out)
}

/// The first child of a kind.
pub fn child<'a>(list: &[Child<'a>], kind: &[u8; 4]) -> Option<&'a [u8]> {
    list.iter().find(|item| item.is(kind)).map(|item| item.body)
}

// ---------------------------------------------------------------- //
// The track.
// ---------------------------------------------------------------- //

/// The video track of a file, with every sample of it.
///
/// Fragmented and not are the same call: the `moov` gives the timing
/// and the codec, its sample tables give whatever samples it holds, and
/// each `moof` after it adds its own.
pub fn read<R: Read + Seek>(src: &mut Source<R>) -> Result<VideoTrack> {
    let top = top_level(src)?;
    let moov = top
        .iter()
        .find(|header| header.is(b"moov"))
        .ok_or(Error::Format("the file has no moov box"))?;
    let body = src.span(moov.body, moov.body_len())?;
    let mut track = track_of_moov(&body)?;

    let moofs: Vec<BoxHeader> = top.iter().copied().filter(|h| h.is(b"moof")).collect();
    if !moofs.is_empty() {
        let defaults = trex_of_moov(&body, track.track_id)?;
        let mut decode = track.next_dts;
        for moof in &moofs {
            let bytes = src.span(moof.body, moof.body_len())?;
            read_fragment(
                &bytes,
                moof.start,
                &track_ref(&track),
                &defaults,
                &mut decode,
            )?
            .into_iter()
            .for_each(|sample| track.samples.push(sample));
            if track.samples.len() > MAX_SAMPLES {
                return Err(Error::Format("more samples than this reader will hold"));
            }
        }
    }

    // Where the file's zero is: the first sample the edit list keeps,
    // which for a file with no edit list is composition time zero.
    let zero = match track.edit.media_time {
        None => 0,
        Some(media_time) => track
            .samples
            .iter()
            .find(|sample| sample.pts >= media_time)
            .map_or(media_time, |sample| sample.pts),
    };
    let start_shift = track.edit.empty - zero;
    for (index, sample) in track.samples.iter_mut().enumerate() {
        sample.index = index as u32;
        sample.pts += start_shift;
    }
    Ok(VideoTrack {
        codec: track.codec,
        timescale: track.timescale,
        length_size: track.length_size,
        codec_private: track.codec_private,
        samples: track.samples,
        start_shift,
    })
}

/// What a track's `elst` amounts to, before the samples are known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Edit {
    /// The empty edits in front, in media ticks: how much later
    /// everything is shown.
    empty: i64,
    /// The `media_time` of the first edit that shows something, or
    /// `None` when there is no edit list to apply.
    media_time: Option<i64>,
}

/// A track while it is being built: the finished shape plus the two
/// things only the fragments need.
struct Building {
    codec: TrackCodec,
    timescale: u32,
    length_size: Option<usize>,
    codec_private: Vec<u8>,
    samples: Vec<Sample>,
    edit: Edit,
    track_id: u32,
    next_dts: i64,
}

/// What a fragment needs to know about the track it belongs to.
struct TrackRef {
    track_id: u32,
}

fn track_ref(track: &Building) -> TrackRef {
    TrackRef {
        track_id: track.track_id,
    }
}

/// The defaults a `trex` sets for every fragment of a track.
#[derive(Clone, Copy, Debug, Default)]
struct Defaults {
    duration: u32,
    size: u32,
    flags: u32,
}

fn track_of_moov(moov: &[u8]) -> Result<Building> {
    let top = children(moov)?;
    let movie_timescale = match child(&top, b"mvhd") {
        Some(body) => mvhd_timescale(body)?,
        None => 1000,
    };
    for entry in top.iter().filter(|item| item.is(b"trak")) {
        let trak = children(entry.body)?;
        let mdia = match child(&trak, b"mdia") {
            Some(body) => children(body)?,
            None => continue,
        };
        if !is_video(&mdia)? {
            continue;
        }
        let mdhd = child(&mdia, b"mdhd").ok_or(Error::Format("a track with no mdhd"))?;
        let timescale = mdhd_timescale(mdhd)?;
        let track_id = child(&trak, b"tkhd")
            .map(tkhd_track_id)
            .transpose()?
            .unwrap_or(1);
        let edit = match child(&trak, b"edts").map(children).transpose()? {
            Some(edts) => match child(&edts, b"elst") {
                Some(elst) => edit_of(elst, movie_timescale, timescale)?,
                None => Edit::default(),
            },
            None => Edit::default(),
        };

        let minf = children(child(&mdia, b"minf").ok_or(Error::Format("a track with no minf"))?)?;
        let stbl = children(child(&minf, b"stbl").ok_or(Error::Format("a track with no stbl"))?)?;
        let stsd = child(&stbl, b"stsd").ok_or(Error::Format("a track with no stsd"))?;
        let (codec, codec_private) = sample_entry(stsd)?;
        let length_size = match codec {
            TrackCodec::H264 => Some(avcc_length_size(&codec_private)),
            TrackCodec::H265 => Some(hvcc_length_size(&codec_private)),
            TrackCodec::Av1 => None,
        };
        let (samples, next_dts) = samples_of_stbl(&stbl)?;
        return Ok(Building {
            codec,
            timescale,
            length_size,
            codec_private,
            samples,
            edit,
            track_id,
            next_dts,
        });
    }
    Err(Error::Unsupported(
        "the file holds no video track this format has carriage for".into(),
    ))
}

fn is_video(mdia: &[Child<'_>]) -> Result<bool> {
    let Some(hdlr) = child(mdia, b"hdlr") else {
        return Ok(false);
    };
    let mut bytes = Bytes::new(hdlr);
    bytes.full_box()?;
    bytes.skip(4)?;
    Ok(bytes.take(4)? == b"vide")
}

fn mvhd_timescale(body: &[u8]) -> Result<u32> {
    let mut bytes = Bytes::new(body);
    let (version, _) = bytes.full_box()?;
    bytes.skip(if version == 1 { 16 } else { 8 })?;
    Ok(bytes.u32()?.max(1))
}

fn mdhd_timescale(body: &[u8]) -> Result<u32> {
    let mut bytes = Bytes::new(body);
    let (version, _) = bytes.full_box()?;
    bytes.skip(if version == 1 { 16 } else { 8 })?;
    Ok(bytes.u32()?.max(1))
}

fn tkhd_track_id(body: &[u8]) -> Result<u32> {
    let mut bytes = Bytes::new(body);
    let (version, _) = bytes.full_box()?;
    bytes.skip(if version == 1 { 16 } else { 8 })?;
    bytes.u32()
}

/// The edit list, as far as the rule at the top of this file uses it.
fn edit_of(elst: &[u8], movie_timescale: u32, media_timescale: u32) -> Result<Edit> {
    let mut bytes = Bytes::new(elst);
    let (version, _) = bytes.full_box()?;
    let entry_size = if version == 1 { 20 } else { 12 };
    let count = bytes.count(entry_size)?;
    let mut empty = 0i64;
    for _ in 0..count {
        let (duration, media_time) = if version == 1 {
            (bytes.u64()? as i64, bytes.u64()? as i64)
        } else {
            (i64::from(bytes.u32()?), i64::from(bytes.i32()?))
        };
        bytes.skip(4)?;
        if media_time < 0 {
            // An empty edit: the file shows nothing for this long, so
            // everything after it is that much later.
            empty += rescale(duration, movie_timescale, media_timescale);
            continue;
        }
        // The first edit that shows something decides where zero is.
        return Ok(Edit {
            empty,
            media_time: Some(media_time),
        });
    }
    Ok(Edit {
        empty,
        media_time: None,
    })
}

fn rescale(value: i64, from: u32, to: u32) -> i64 {
    let from = i128::from(from.max(1));
    let to = i128::from(to.max(1));
    ((i128::from(value) * to) / from).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// The codec and its out-of-band header, from the first sample entry.
fn sample_entry(stsd: &[u8]) -> Result<(TrackCodec, Vec<u8>)> {
    let mut bytes = Bytes::new(stsd);
    bytes.full_box()?;
    let count = bytes.count(8)?;
    if count == 0 {
        return Err(Error::Format("an stsd with no sample entry"));
    }
    let entries = children(bytes.rest())?;
    for entry in &entries {
        let codec = match &entry.kind {
            b"avc1" | b"avc3" => TrackCodec::H264,
            b"hvc1" | b"hev1" => TrackCodec::H265,
            b"av01" => TrackCodec::Av1,
            _ => continue,
        };
        // The child boxes of a visual sample entry start past its fixed
        // fields. Its own header is already off, so seventy-eight of
        // the eighty-six are left.
        let inner = entry
            .body
            .get(VISUAL_SAMPLE_ENTRY_LEN - 8..)
            .ok_or(Error::Format("a sample entry shorter than its own fields"))?;
        let config = children(inner)?;
        let wanted: &[u8; 4] = match codec {
            TrackCodec::H264 => b"avcC",
            TrackCodec::H265 => b"hvcC",
            TrackCodec::Av1 => b"av1C",
        };
        let private = child(&config, wanted).unwrap_or_default().to_vec();
        return Ok((codec, private));
    }
    Err(Error::Unsupported(format!(
        "the video track is {}, which this format has no carriage for",
        entries
            .first()
            .map(|entry| String::from_utf8_lossy(&entry.kind).to_string())
            .unwrap_or_else(|| "nothing".into())
    )))
}

// ---------------------------------------------------------------- //
// The sample tables.
// ---------------------------------------------------------------- //

/// Every sample a `stbl` describes, and the decode time after the last
/// of them, which is where a fragment carries on from.
fn samples_of_stbl(stbl: &[Child<'_>]) -> Result<(Vec<Sample>, i64)> {
    let sizes = match child(stbl, b"stsz") {
        Some(body) => stsz(body)?,
        None => match child(stbl, b"stz2") {
            Some(body) => stz2(body)?,
            None => Sizes::Uniform { size: 0, count: 0 },
        },
    };
    let total = sizes.count();
    if total > MAX_SAMPLES {
        return Err(Error::Format("more samples than this reader will hold"));
    }
    if total == 0 {
        return Ok((Vec::new(), 0));
    }
    let chunks = match child(stbl, b"stco") {
        Some(body) => offsets32(body)?,
        None => match child(stbl, b"co64") {
            Some(body) => offsets64(body)?,
            None => return Err(Error::Format("a track with samples but no chunk offsets")),
        },
    };
    let runs = match child(stbl, b"stsc") {
        Some(body) => stsc(body)?,
        None => return Err(Error::Format("a track with samples but no stsc")),
    };
    let times = match child(stbl, b"stts") {
        Some(body) => pairs(body)?,
        None => Vec::new(),
    };
    let composition = match child(stbl, b"ctts") {
        Some(body) => ctts(body)?,
        None => Vec::new(),
    };
    let sync = match child(stbl, b"stss") {
        Some(body) => Some(sync_numbers(body)?),
        None => None,
    };

    let mut samples: Vec<Sample> = Vec::new();
    samples
        .try_reserve(total)
        .map_err(|_| Error::Format("a sample table larger than memory"))?;
    for (index, run) in runs.iter().enumerate() {
        let first = run.0.max(1) as usize;
        let after = match runs.get(index + 1) {
            Some(next) => (next.0.max(1) as usize).max(first),
            None => chunks.len() + 1,
        };
        for chunk in first..after {
            let Some(base) = chunks.get(chunk - 1) else {
                break;
            };
            let mut at = *base;
            for _ in 0..run.1 {
                if samples.len() >= total {
                    break;
                }
                let size = sizes.get(samples.len());
                samples.push(Sample {
                    index: samples.len() as u32,
                    offset: at,
                    size,
                    pts: 0,
                    keyframe: false,
                });
                at = at
                    .checked_add(u64::from(size))
                    .ok_or(Error::Format("a chunk that runs past the file"))?;
            }
        }
        if samples.len() >= total {
            break;
        }
    }
    if samples.len() != total {
        return Err(Error::Format(
            "the chunk table describes fewer samples than the size table",
        ));
    }

    // Decode times, then the composition offsets on top of them.
    let mut dts = 0i64;
    let mut at = 0usize;
    for (count, delta) in &times {
        for _ in 0..*count {
            if at >= samples.len() {
                break;
            }
            samples[at].pts = dts;
            dts += i64::from(*delta);
            at += 1;
        }
    }
    // A stts that ran out leaves the rest of the samples at the last
    // decode time reached, which is what a damaged table deserves and
    // what ffmpeg does with one.
    for sample in samples.iter_mut().skip(at) {
        sample.pts = dts;
    }
    let mut at = 0usize;
    for (count, offset) in &composition {
        for _ in 0..*count {
            if at >= samples.len() {
                break;
            }
            samples[at].pts += i64::from(*offset);
            at += 1;
        }
    }

    match sync {
        Some(numbers) => {
            for number in numbers {
                if let Some(sample) = samples.get_mut(number.saturating_sub(1) as usize) {
                    sample.keyframe = true;
                }
            }
        }
        // No stss at all means every sample is a sync sample, which is
        // what an all-intra track looks like.
        None => samples.iter_mut().for_each(|sample| sample.keyframe = true),
    }
    Ok((samples, dts))
}

/// A sample size table, either one number for all of them or a list.
#[derive(Clone, Debug)]
enum Sizes {
    Uniform { size: u32, count: usize },
    Table(Vec<u32>),
}

impl Sizes {
    fn count(&self) -> usize {
        match self {
            Sizes::Uniform { count, .. } => *count,
            Sizes::Table(list) => list.len(),
        }
    }

    fn get(&self, index: usize) -> u32 {
        match self {
            Sizes::Uniform { size, .. } => *size,
            Sizes::Table(list) => list.get(index).copied().unwrap_or(0),
        }
    }
}

fn stsz(body: &[u8]) -> Result<Sizes> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let uniform = bytes.u32()?;
    if uniform != 0 {
        let count = bytes.u32()? as usize;
        if count > MAX_SAMPLES {
            return Err(Error::Format("more samples than this reader will hold"));
        }
        return Ok(Sizes::Uniform {
            size: uniform,
            count,
        });
    }
    let count = bytes.count(4)?;
    let mut list = Vec::new();
    list.try_reserve(count)
        .map_err(|_| Error::Format("a size table larger than memory"))?;
    for _ in 0..count {
        list.push(bytes.u32()?);
    }
    Ok(Sizes::Table(list))
}

fn stz2(body: &[u8]) -> Result<Sizes> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    bytes.skip(3)?;
    let field = bytes.u8()?;
    if !matches!(field, 4 | 8 | 16) {
        return Err(Error::Format("an stz2 field width that is not 4, 8 or 16"));
    }
    let count = bytes.u32()? as usize;
    let needed = match field {
        4 => count.div_ceil(2),
        8 => count,
        _ => count
            .checked_mul(2)
            .ok_or(Error::Format("a table wider than memory"))?,
    };
    let packed = bytes.take(needed)?;
    let mut list = Vec::new();
    list.try_reserve(count)
        .map_err(|_| Error::Format("a size table larger than memory"))?;
    for index in 0..count {
        list.push(match field {
            4 => {
                let byte = packed[index / 2];
                u32::from(if index % 2 == 0 {
                    byte >> 4
                } else {
                    byte & 0x0f
                })
            }
            8 => u32::from(packed[index]),
            _ => u32::from(u16::from_be_bytes([
                packed[index * 2],
                packed[index * 2 + 1],
            ])),
        });
    }
    Ok(Sizes::Table(list))
}

fn offsets32(body: &[u8]) -> Result<Vec<u64>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(4)?;
    let mut out = Vec::new();
    out.try_reserve(count)
        .map_err(|_| Error::Format("a chunk table larger than memory"))?;
    for _ in 0..count {
        out.push(u64::from(bytes.u32()?));
    }
    Ok(out)
}

fn offsets64(body: &[u8]) -> Result<Vec<u64>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(8)?;
    let mut out = Vec::new();
    out.try_reserve(count)
        .map_err(|_| Error::Format("a chunk table larger than memory"))?;
    for _ in 0..count {
        out.push(bytes.u64()?);
    }
    Ok(out)
}

fn stsc(body: &[u8]) -> Result<Vec<(u32, u32, u32)>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(12)?;
    let mut out = Vec::new();
    out.try_reserve(count)
        .map_err(|_| Error::Format("a chunk map larger than memory"))?;
    for _ in 0..count {
        out.push((bytes.u32()?, bytes.u32()?, bytes.u32()?));
    }
    Ok(out)
}

fn pairs(body: &[u8]) -> Result<Vec<(u32, u32)>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(8)?;
    let mut out = Vec::new();
    out.try_reserve(count)
        .map_err(|_| Error::Format("a time table larger than memory"))?;
    for _ in 0..count {
        out.push((bytes.u32()?, bytes.u32()?));
    }
    Ok(out)
}

/// The composition offsets. Version 1 signs them; version 0 does not,
/// but ffmpeg has written large unsigned ones meaning negative, so both
/// are read as signed, which is what every other reader does.
fn ctts(body: &[u8]) -> Result<Vec<(u32, i32)>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(8)?;
    let mut out = Vec::new();
    out.try_reserve(count)
        .map_err(|_| Error::Format("a composition table larger than memory"))?;
    for _ in 0..count {
        out.push((bytes.u32()?, bytes.i32()?));
    }
    Ok(out)
}

fn sync_numbers(body: &[u8]) -> Result<Vec<u32>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(4)?;
    let mut out = Vec::new();
    out.try_reserve(count)
        .map_err(|_| Error::Format("a sync table larger than memory"))?;
    for _ in 0..count {
        out.push(bytes.u32()?);
    }
    Ok(out)
}

// ---------------------------------------------------------------- //
// Fragments.
// ---------------------------------------------------------------- //

fn trex_of_moov(moov: &[u8], track_id: u32) -> Result<Defaults> {
    let top = children(moov)?;
    let Some(mvex) = child(&top, b"mvex") else {
        return Ok(Defaults::default());
    };
    for entry in children(mvex)?.iter().filter(|item| item.is(b"trex")) {
        let mut bytes = Bytes::new(entry.body);
        bytes.full_box()?;
        if bytes.u32()? != track_id {
            continue;
        }
        bytes.skip(4)?;
        return Ok(Defaults {
            duration: bytes.u32()?,
            size: bytes.u32()?,
            flags: bytes.u32()?,
        });
    }
    Ok(Defaults::default())
}

/// The samples of one `moof`, and the decode time it leaves behind.
fn read_fragment(
    moof: &[u8],
    moof_start: u64,
    track: &TrackRef,
    defaults: &Defaults,
    decode: &mut i64,
) -> Result<Vec<Sample>> {
    let mut out = Vec::new();
    for traf in children(moof)?.iter().filter(|item| item.is(b"traf")) {
        let inner = children(traf.body)?;
        let tfhd = child(&inner, b"tfhd").ok_or(Error::Format("a traf with no tfhd"))?;
        let mut bytes = Bytes::new(tfhd);
        let (_, flags) = bytes.full_box()?;
        if bytes.u32()? != track.track_id {
            continue;
        }
        let base = if flags & 0x01 != 0 {
            bytes.u64()?
        } else {
            // Either the flag says so or the default applies, and both
            // put the first traf's data at the head of the moof.
            moof_start
        };
        if flags & 0x02 != 0 {
            bytes.skip(4)?;
        }
        let default_duration = if flags & 0x08 != 0 {
            bytes.u32()?
        } else {
            defaults.duration
        };
        let default_size = if flags & 0x10 != 0 {
            bytes.u32()?
        } else {
            defaults.size
        };
        let default_flags = if flags & 0x20 != 0 {
            bytes.u32()?
        } else {
            defaults.flags
        };

        if let Some(tfdt) = child(&inner, b"tfdt") {
            let mut bytes = Bytes::new(tfdt);
            let (version, _) = bytes.full_box()?;
            *decode = if version == 1 {
                bytes.u64()? as i64
            } else {
                i64::from(bytes.u32()?)
            };
        }

        for trun in inner.iter().filter(|item| item.is(b"trun")) {
            let mut bytes = Bytes::new(trun.body);
            let (version, flags) = bytes.full_box()?;
            let mut width = 0usize;
            for bit in [0x100u32, 0x200, 0x400, 0x800] {
                if flags & bit != 0 {
                    width += 4;
                }
            }
            let count = bytes.u32()? as usize;
            let data_offset = if flags & 0x01 != 0 {
                i64::from(bytes.i32()?)
            } else {
                0
            };
            let first_flags = if flags & 0x04 != 0 {
                Some(bytes.u32()?)
            } else {
                None
            };
            if count
                .checked_mul(width.max(1))
                .is_none_or(|want| want > bytes.left())
            {
                return Err(Error::Format("a trun longer than the box holding it"));
            }
            let mut at = base
                .checked_add_signed(data_offset)
                .ok_or(Error::Format("a trun whose data offset leaves the file"))?;
            for index in 0..count {
                let duration = if flags & 0x100 != 0 {
                    bytes.u32()?
                } else {
                    default_duration
                };
                let size = if flags & 0x200 != 0 {
                    bytes.u32()?
                } else {
                    default_size
                };
                let sample_flags = if flags & 0x400 != 0 {
                    bytes.u32()?
                } else if index == 0 {
                    first_flags.unwrap_or(default_flags)
                } else {
                    default_flags
                };
                let composition = if flags & 0x800 != 0 {
                    if version == 0 {
                        i64::from(bytes.u32()?)
                    } else {
                        i64::from(bytes.i32()?)
                    }
                } else {
                    0
                };
                out.push(Sample {
                    index: 0,
                    offset: at,
                    size,
                    pts: *decode + composition,
                    // `sample_is_non_sync_sample` is the bit at 16.
                    keyframe: sample_flags & 0x0001_0000 == 0,
                });
                at = at
                    .checked_add(u64::from(size))
                    .ok_or(Error::Format("a sample that runs past the file"))?;
                *decode += i64::from(duration);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- //
// The index box, section 8.
// ---------------------------------------------------------------- //

/// A `uuid` box of this format already in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexBox {
    /// Where the box begins.
    pub start: u64,
    /// Where the index inside it begins, past the header and the UUID.
    pub body: u64,
    /// Where the box ends.
    pub end: u64,
    /// Whether it is the last top-level box of the file.
    pub last: bool,
}

/// This format's `uuid` box, if the file has one.
///
/// The tail first, because section 8 says to put it at the end and that
/// is where this crate puts it: one read of the last few hundred bytes
/// answers the question for every file this tool wrote. Only when that
/// misses are the top-level boxes walked.
pub fn find_index_box<R: Read + Seek>(src: &mut Source<R>) -> Result<Option<IndexBox>> {
    if let Some(found) = index_box_at_tail(src)? {
        return Ok(Some(found));
    }
    let top = top_level(src)?;
    for (at, header) in top.iter().enumerate() {
        if !header.is(b"uuid") || header.body_len() < 16 {
            continue;
        }
        let uuid = src.exact_at(header.body, 16)?;
        if uuid == ffrwd_index_core::UUID {
            return Ok(Some(IndexBox {
                start: header.start,
                body: header.body + 16,
                end: header.end,
                last: at + 1 == top.len(),
            }));
        }
    }
    Ok(None)
}

/// How far back from the end of a file the tail guess looks for the
/// header of a box that ends there.
const TAIL_WINDOW: usize = 4096;

/// The tail guess: a `uuid` box of ours whose end is the end of the
/// file.
///
/// The box's size field is at its own start, which is not known, so
/// this reads one window at the end and takes the earliest header in
/// it whose size lands exactly on the end of the file and whose
/// extended type is ours. Wrong guesses cost nothing: the size has to
/// hit the end of the file to the byte and the sixteen bytes after the
/// header have to be this format's UUID.
fn index_box_at_tail<R: Read + Seek>(src: &mut Source<R>) -> Result<Option<IndexBox>> {
    let len = src.len();
    if len < 24 {
        return Ok(None);
    }
    let window = src.read_at(len.saturating_sub(TAIL_WINDOW as u64), TAIL_WINDOW)?;
    let base = len - window.len() as u64;
    for at in 0..window.len().saturating_sub(24) {
        if &window[at + 4..at + 8] != b"uuid" {
            continue;
        }
        let short =
            u32::from_be_bytes([window[at], window[at + 1], window[at + 2], window[at + 3]]);
        let (size, head) = if short == 1 {
            let Some(wide) = window.get(at + 8..at + 16) else {
                continue;
            };
            let mut value = [0u8; 8];
            value.copy_from_slice(wide);
            (u64::from_be_bytes(value), 16usize)
        } else {
            (u64::from(short), 8usize)
        };
        if size < head as u64 + 16 || base + at as u64 + size != len {
            continue;
        }
        if window.get(at + head..at + head + 16) != Some(&ffrwd_index_core::UUID[..]) {
            continue;
        }
        let start = base + at as u64;
        return Ok(Some(IndexBox {
            start,
            body: start + head as u64 + 16,
            end: len,
            last: true,
        }));
    }
    Ok(None)
}

/// The index bytes out of a file, if it carries one.
pub fn read_index<R: Read + Seek>(src: &mut Source<R>) -> Result<Option<Vec<u8>>> {
    let Some(found) = find_index_box(src)? else {
        return Ok(None);
    };
    Ok(Some(src.span(found.body, found.end - found.body)?))
}

/// Where a new `uuid` box goes, and what it costs to put it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    /// The end of the file. Nothing else moves, which is the whole
    /// reason section 8 puts it there.
    End(u64),
    /// Just before the file's `mfra`, which has to stay last: its
    /// `mfro` is a copy of its own size, placed so that the last four
    /// bytes of the file find it. Appending past it would quietly
    /// break that, and moving the `mfra` costs its own length and
    /// nothing else, since the offsets it holds point at the `moof`
    /// boxes before it.
    BeforeMfra(u64),
    /// Over the box already there, which is last so it can be cut off.
    Over(u64),
    /// A box of ours is in the file but not at the end, so the file has
    /// to be copied without it. The span is the box to leave out.
    Rewrite(u64, u64),
}

/// Where this file's index box belongs.
pub fn spot<R: Read + Seek>(src: &mut Source<R>) -> Result<Spot> {
    let top = top_level(src)?;
    if let Some(found) = find_index_box(src)? {
        return Ok(if found.last {
            Spot::Over(found.start)
        } else {
            Spot::Rewrite(found.start, found.end)
        });
    }
    match top.last() {
        Some(last) if last.is(b"mfra") => Ok(Spot::BeforeMfra(last.start)),
        _ => Ok(Spot::End(src.len())),
    }
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

    fn source(bytes: Vec<u8>) -> Source<Cursor<Vec<u8>>> {
        Source::new(Cursor::new(bytes)).expect("a source")
    }

    #[test]
    fn a_box_of_every_size_shape_reads() {
        let mut file = bx(b"ftyp", b"isom");
        let mut wide = 1u32.to_be_bytes().to_vec();
        wide.extend_from_slice(b"free");
        wide.extend_from_slice(&20u64.to_be_bytes());
        wide.extend_from_slice(&[0; 4]);
        file.extend_from_slice(&wide);
        // A size of zero runs to the end of the file.
        file.extend_from_slice(&[0, 0, 0, 0]);
        file.extend_from_slice(b"mdat");
        file.extend_from_slice(&[9; 16]);
        let mut src = source(file);
        let top = top_level(&mut src).expect("the boxes");
        assert_eq!(top.len(), 3);
        assert_eq!(top[1].body_len(), 4);
        assert_eq!(top[2].end, src.len());
    }

    #[test]
    fn a_box_that_runs_past_the_file_is_refused() {
        let mut file = bx(b"ftyp", b"isom");
        file.extend_from_slice(&1_000_000u32.to_be_bytes());
        file.extend_from_slice(b"moov");
        let mut src = source(file);
        assert!(top_level(&mut src).is_err());
    }

    #[test]
    fn the_edit_list_rule_is_the_one_ffprobe_follows() {
        // One edit whose media time is two frames of a 15360 timescale:
        // ffmpeg's own B-frame output, which starts at zero.
        let one = edit_of(&elst_v0(&[(30720, 1024)]), 1000, 15360).expect("an edit");
        assert_eq!(
            one,
            Edit {
                empty: 0,
                media_time: Some(1024)
            }
        );
        // An empty edit of a second in front of it: `-output_ts_offset`.
        let two = edit_of(&elst_v0(&[(1000, -1), (30720, 1024)]), 1000, 15360).expect("an edit");
        assert_eq!(
            two,
            Edit {
                empty: 15360,
                media_time: Some(1024)
            }
        );
        // No edit list at all leaves composition times alone, which is
        // what a fragmented file with an empty moov has.
        assert_eq!(
            edit_of(&elst_v0(&[]), 1000, 15360).expect("an edit"),
            Edit::default()
        );
    }

    #[test]
    fn the_zero_snaps_to_the_first_sample_the_edit_keeps() {
        // Composition times a tick short of the nominal frame duration,
        // which is what raw H.264 remuxed into MP4 has, and a media
        // time that therefore lands between two samples.
        let times = [0i64, 40000, 79999, 119999, 159999];
        let media_time = 96000i64;
        let zero = times
            .iter()
            .find(|pts| **pts >= media_time)
            .copied()
            .unwrap_or(media_time);
        assert_eq!(zero, 119999, "ffmpeg keeps the first sample at or past it");
        assert_eq!(times[0] - zero, -119999, "and ffprobe prints -0.099999");
    }

    fn elst_v0(entries: &[(u32, i32)]) -> Vec<u8> {
        let mut out = vec![0u8, 0, 0, 0];
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (duration, media_time) in entries {
            out.extend_from_slice(&duration.to_be_bytes());
            out.extend_from_slice(&media_time.to_be_bytes());
            out.extend_from_slice(&[0, 1, 0, 0]);
        }
        out
    }

    #[test]
    fn an_index_box_is_found_at_the_tail_and_in_the_middle() {
        let index = b"FFIX\x01\x00".to_vec();
        let mut boxed = ((8 + 16 + index.len()) as u32).to_be_bytes().to_vec();
        boxed.extend_from_slice(b"uuid");
        boxed.extend_from_slice(&ffrwd_index_core::UUID);
        boxed.extend_from_slice(&index);

        let mut last = bx(b"ftyp", b"isom");
        last.extend_from_slice(&bx(b"mdat", &[7; 32]));
        last.extend_from_slice(&boxed);
        let mut src = source(last);
        let found = find_index_box(&mut src).expect("a walk").expect("a box");
        assert!(found.last);
        assert_eq!(
            read_index(&mut src).expect("a read").expect("an index"),
            index
        );
        assert_eq!(spot(&mut src).expect("a spot"), Spot::Over(found.start));

        let mut middle = bx(b"ftyp", b"isom");
        let at = middle.len() as u64;
        middle.extend_from_slice(&boxed);
        middle.extend_from_slice(&bx(b"mdat", &[7; 32]));
        let end = at + boxed.len() as u64;
        let mut src = source(middle);
        let found = find_index_box(&mut src).expect("a walk").expect("a box");
        assert!(!found.last);
        assert_eq!(spot(&mut src).expect("a spot"), Spot::Rewrite(at, end));
    }

    #[test]
    fn a_fragmented_file_keeps_its_mfra_last() {
        let mut file = bx(b"ftyp", b"isom");
        file.extend_from_slice(&bx(b"moof", &[0; 8]));
        file.extend_from_slice(&bx(b"mdat", &[1; 8]));
        let at = file.len() as u64;
        file.extend_from_slice(&bx(b"mfra", &[0; 8]));
        let mut src = source(file);
        assert_eq!(spot(&mut src).expect("a spot"), Spot::BeforeMfra(at));
    }

    #[test]
    fn every_truncation_of_a_box_tree_is_an_error_and_not_a_panic() {
        let mut file = bx(b"ftyp", b"isom");
        file.extend_from_slice(&bx(b"moov", &bx(b"mvhd", &[0; 100])));
        file.extend_from_slice(&bx(b"mdat", &[3; 64]));
        for cut in 0..file.len() {
            let mut src = source(file[..cut].to_vec());
            let _ = top_level(&mut src);
            let _ = read(&mut src);
            let _ = find_index_box(&mut src);
            let _ = spot(&mut src);
        }
    }
}
