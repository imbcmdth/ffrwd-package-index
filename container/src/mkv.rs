//! Matroska and WebM, which are one format with two sets of allowed
//! codecs.
//!
//! What is parsed: the EBML header, the `Segment`, its `Info` for the
//! `TimestampScale`, its `Tracks` for the video track's number, codec
//! and `CodecPrivate`, its `Cues` for the keyframe fast path, its
//! `Attachments` for the file index of section 8, and its `Cluster`s
//! for the blocks themselves.
//!
//! **Timestamps.** Matroska stores presentation time and nothing else:
//! a cluster's `Timestamp` plus a block's signed 16-bit offset from it,
//! in units of `TimestampScale` nanoseconds. There is no composition
//! offset to add and no edit list to apply, so the number here is the
//! number ffprobe prints, to the resolution the file chose, which for
//! everything ffmpeg writes is one millisecond.
//!
//! **Keyframes.** A `SimpleBlock` says so in its flags. A `Block`
//! inside a `BlockGroup` does not, and the rule there is the format's
//! own: a block with no `ReferenceBlock` beside it references nothing,
//! so it is a keyframe. A keyframe scan reads the sync samples and
//! nothing else: section 7 puts every record of a `keyframe` file on a
//! keyframe, the last one included.
//!
//! **Lacing** packs several frames into one block. Video never uses it,
//! every muxer sets `FlagLacing` to zero for a video track, and
//! unpacking it would mean deciding which of several frames a record
//! rode. A laced video block is refused by name.
//!
//! **What is shared.** The reader is `ffrwd_bmff::source::Source`, the
//! same counted reader the MP4 side uses, so a keyframe scan's cost is
//! counted the same way whichever container it read, and what comes
//! back is a `ffrwd_bmff::track::Track` built with `Track::from_parts`,
//! the same type the MP4 reader returns, so one prefix scan serves
//! both. The NAL length an `avcC` or `hvcC` declares is read by
//! `ffrwd_nal`. Nothing about EBML is in either crate, and nothing
//! about Matroska is here twice.
//!
//! **Unknown sizes.** A live writer does not know how long a `Segment`
//! or a `Cluster` will be and writes the all-ones length for it.
//! `ffmpeg -live 1` writes an unknown-size `Segment`; some writers do
//! the same for clusters. Both work here: an unknown-size element ends
//! at the first element that cannot be a child of it, which for a
//! cluster is the next cluster or anything at the segment's own level,
//! and for a segment is the end of the file.

use std::io::{Read, Seek};

use ffrwd_bmff::source::Source;
use ffrwd_bmff::track::{Handler, Sample, SampleEntry, Track};
use ffrwd_index_core::index::MATROSKA_MIME;

use crate::{Error, Result, Scan};

pub const ID_EBML: u32 = 0x1A45_DFA3;
pub const ID_SEGMENT: u32 = 0x1853_8067;
pub const ID_INFO: u32 = 0x1549_A966;
pub const ID_TIMESTAMP_SCALE: u32 = 0x002A_D7B1;
pub const ID_TRACKS: u32 = 0x1654_AE6B;
pub const ID_TRACK_ENTRY: u32 = 0x0000_00AE;
pub const ID_TRACK_NUMBER: u32 = 0x0000_00D7;
pub const ID_TRACK_TYPE: u32 = 0x0000_0083;
pub const ID_CODEC_ID: u32 = 0x0000_0086;
pub const ID_CODEC_PRIVATE: u32 = 0x0000_63A2;
pub const ID_CLUSTER: u32 = 0x1F43_B675;
pub const ID_TIMESTAMP: u32 = 0x0000_00E7;
pub const ID_POSITION: u32 = 0x0000_00A7;
pub const ID_PREV_SIZE: u32 = 0x0000_00AB;
pub const ID_SIMPLE_BLOCK: u32 = 0x0000_00A3;
pub const ID_BLOCK_GROUP: u32 = 0x0000_00A0;
pub const ID_BLOCK: u32 = 0x0000_00A1;
pub const ID_REFERENCE_BLOCK: u32 = 0x0000_00FB;
pub const ID_CUES: u32 = 0x1C53_BB6B;
pub const ID_CUE_POINT: u32 = 0x0000_00BB;
pub const ID_CUE_TIME: u32 = 0x0000_00B3;
pub const ID_CUE_TRACK_POSITIONS: u32 = 0x0000_00B7;
pub const ID_CUE_TRACK: u32 = 0x0000_00F7;
pub const ID_CUE_CLUSTER_POSITION: u32 = 0x0000_00F1;
pub const ID_ATTACHMENTS: u32 = 0x1941_A469;
pub const ID_ATTACHED_FILE: u32 = 0x0000_61A7;
pub const ID_FILE_MIME_TYPE: u32 = 0x0000_4660;
pub const ID_FILE_DATA: u32 = 0x0000_465C;
pub const ID_VOID: u32 = 0x0000_00EC;
pub const ID_CRC32: u32 = 0x0000_00BF;

/// The most elements one walk will visit before it gives up on the
/// file, which bounds every loop in this module.
pub const MAX_ELEMENTS: usize = 8_000_000;

/// How many bytes are read at an element's start: enough for the
/// longest id and length together, and for a block's own header after
/// them.
const HEAD: usize = 32;

/// The most bytes an element this module reads whole may have.
const MAX_BODY: u64 = 64 << 20;

/// One element, as its header describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Element {
    pub id: u32,
    /// Where the element begins, at its id.
    pub start: u64,
    /// Where its body begins.
    pub body: u64,
    /// Where it ends, or `None` when the length was the all-ones
    /// unknown.
    pub end: Option<u64>,
}

impl Element {
    /// Where it ends, taking the parent's limit for an unknown length.
    pub fn end_or(&self, limit: u64) -> u64 {
        self.end.unwrap_or(limit).min(limit)
    }
}

/// An element id and how many bytes it took.
pub fn read_id(bytes: &[u8]) -> Result<(u32, usize)> {
    let first = *bytes
        .first()
        .ok_or(Error::Format("the bytes end inside an EBML id"))?;
    if first == 0 {
        return Err(Error::Format("an EBML id with no class marker"));
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return Err(Error::Format("an EBML id wider than four bytes"));
    }
    let raw = bytes
        .get(..len)
        .ok_or(Error::Format("the bytes end inside an EBML id"))?;
    let mut value = 0u32;
    for byte in raw {
        value = value << 8 | u32::from(*byte);
    }
    Ok((value, len))
}

/// An element length and how many bytes it took. `None` is the
/// all-ones value, which means the length is not known yet.
pub fn read_size(bytes: &[u8]) -> Result<(Option<u64>, usize)> {
    let first = *bytes
        .first()
        .ok_or(Error::Format("the bytes end inside an EBML length"))?;
    if first == 0 {
        return Err(Error::Format("an EBML length with no class marker"));
    }
    let len = first.leading_zeros() as usize + 1;
    let raw = bytes
        .get(..len)
        .ok_or(Error::Format("the bytes end inside an EBML length"))?;
    let mut value = u64::from(raw[0] & (0x7f >> (len - 1)));
    for byte in &raw[1..] {
        value = value << 8 | u64::from(*byte);
    }
    let unknown = value == (1u64 << (7 * len)) - 1;
    Ok((if unknown { None } else { Some(value) }, len))
}

/// An unsigned integer element's value, which Matroska stores in as few
/// bytes as it needs.
pub fn uint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .take(8)
        .fold(0u64, |value, byte| value << 8 | u64::from(*byte))
}

/// The element at `at`, with the window read to find it.
///
/// The window is longer than the header on purpose: a block's own
/// header follows straight after, and reading both at once is what
/// keeps a cluster walk to one read per block.
pub fn read_element<R: Read + Seek>(
    src: &mut Source<R>,
    at: u64,
    limit: u64,
) -> Result<Option<(Element, Vec<u8>)>> {
    if at >= limit {
        return Ok(None);
    }
    let want = usize::try_from(limit - at).unwrap_or(HEAD).min(HEAD);
    let window = src.read_at(at, want)?;
    if window.is_empty() {
        return Ok(None);
    }
    let (id, id_len) = read_id(&window)?;
    let (size, size_len) = read_size(&window[id_len..])?;
    let body = at + (id_len + size_len) as u64;
    let end = match size {
        Some(size) => {
            let end = body
                .checked_add(size)
                .ok_or(Error::Format("an element whose length overflows the file"))?;
            if end > limit {
                return Err(Error::Format("an element that runs past its parent"));
            }
            Some(end)
        }
        None => None,
    };
    Ok(Some((
        Element {
            id,
            start: at,
            body,
            end,
        },
        window,
    )))
}

/// Whether an id may be a child of a `Cluster`, which is how an
/// unknown-size cluster finds its end.
fn is_cluster_child(id: u32) -> bool {
    matches!(
        id,
        ID_TIMESTAMP
            | ID_POSITION
            | ID_PREV_SIZE
            | ID_SIMPLE_BLOCK
            | ID_BLOCK_GROUP
            | ID_VOID
            | ID_CRC32
            | 0x0000_00AF // EncryptedBlock, from an older revision
    )
}

/// The segment of a file: where its children start and where they end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub body: u64,
    pub end: u64,
    /// Whether the segment declared its own length.
    pub sized: bool,
}

/// The file's segment, past the EBML header.
pub fn segment<R: Read + Seek>(src: &mut Source<R>) -> Result<Segment> {
    let len = src.len();
    let mut at = 0u64;
    for _ in 0..16 {
        let Some((element, _)) = read_element(src, at, len)? else {
            break;
        };
        if element.id == ID_SEGMENT {
            return Ok(Segment {
                body: element.body,
                end: element.end_or(len),
                sized: element.end.is_some(),
            });
        }
        if element.id != ID_EBML && element.id != ID_VOID && element.id != ID_CRC32 {
            return Err(Error::Format("the file has no Matroska segment"));
        }
        at = element.end_or(len);
        if at <= element.start {
            break;
        }
    }
    Err(Error::Format("the file has no Matroska segment"))
}

/// Where the parts of a segment are, found without reading any of them.
#[derive(Clone, Debug, Default)]
struct Outline {
    info: Option<(u64, u64)>,
    tracks: Option<(u64, u64)>,
    cues: Option<(u64, u64)>,
    attachments: Option<(u64, u64)>,
    clusters: Vec<Element>,
}

/// One pass over the segment's children, reading only their headers.
///
/// A cluster that declared its length is jumped over; one that did not
/// has to be walked to find where it ends, and the walk stops at the
/// first id that cannot be a cluster's child.
fn outline<R: Read + Seek>(src: &mut Source<R>, seg: Segment) -> Result<Outline> {
    let mut out = Outline::default();
    let mut at = seg.body;
    let mut visited = 0usize;
    while at < seg.end {
        let Some((element, _)) = read_element(src, at, seg.end)? else {
            break;
        };
        visited += 1;
        if visited > MAX_ELEMENTS {
            return Err(Error::Format("more elements than a segment has"));
        }
        let span = (element.body, element.end_or(seg.end));
        match element.id {
            ID_INFO => out.info = Some(span),
            ID_TRACKS => out.tracks = Some(span),
            ID_CUES => out.cues = Some(span),
            ID_ATTACHMENTS => out.attachments = Some(span),
            ID_CLUSTER => out.clusters.push(element),
            _ => {}
        }
        let next = match element.end {
            Some(end) => end,
            None if element.id == ID_CLUSTER => cluster_end(src, element, seg.end)?,
            // An unknown-size element that is not a cluster ends the
            // walk: there is nothing sensible to skip to.
            None => break,
        };
        if next <= at {
            break;
        }
        at = next;
    }
    Ok(out)
}

/// Where an unknown-size cluster ends: at the first element that is not
/// one of its own children.
fn cluster_end<R: Read + Seek>(src: &mut Source<R>, cluster: Element, limit: u64) -> Result<u64> {
    let mut at = cluster.body;
    let mut visited = 0usize;
    while at < limit {
        let Some((element, _)) = read_element(src, at, limit)? else {
            return Ok(limit);
        };
        if !is_cluster_child(element.id) {
            return Ok(at);
        }
        visited += 1;
        if visited > MAX_ELEMENTS {
            return Err(Error::Format("more elements than a cluster has"));
        }
        let Some(end) = element.end else {
            return Ok(limit);
        };
        if end <= at {
            return Ok(limit);
        }
        at = end;
    }
    Ok(limit)
}

/// The bytes of an element, refused when it is larger than this module
/// will hold whole.
fn body_of<R: Read + Seek>(src: &mut Source<R>, span: (u64, u64)) -> Result<Vec<u8>> {
    let len = span.1.saturating_sub(span.0);
    if len > MAX_BODY {
        return Err(Error::Format(
            "an element larger than this reader will hold",
        ));
    }
    Ok(src.span(span.0, len)?)
}

/// One child of a body already in hand: what it is, where in that body
/// it starts, and its bytes.
type Child<'a> = (u32, usize, &'a [u8]);

/// The children of a body already in hand.
fn children(body: &[u8]) -> Result<Vec<Child<'_>>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < body.len() {
        let (id, id_len) = read_id(&body[at..])?;
        let (size, size_len) = read_size(
            body.get(at + id_len..)
                .ok_or(Error::Format("the bytes end inside an EBML length"))?,
        )?;
        let start = at + id_len + size_len;
        let size = match size {
            Some(size) => {
                usize::try_from(size).map_err(|_| Error::Format("an element wider than memory"))?
            }
            // Inside a body already in hand, an unknown length can only
            // mean the rest of it.
            None => body.len().saturating_sub(start),
        };
        let end = start
            .checked_add(size)
            .ok_or(Error::Format("an element whose length overflows"))?;
        let bytes = body
            .get(start..end)
            .ok_or(Error::Format("an element that runs past its parent"))?;
        out.push((id, start, bytes));
        at = end;
        if out.len() > MAX_ELEMENTS {
            return Err(Error::Format("more elements than a body holds"));
        }
    }
    Ok(out)
}

/// The bytes of the first child of a kind.
fn field<'a>(list: &[Child<'a>], wanted: u32) -> Option<&'a [u8]> {
    list.iter()
        .find(|(id, _, _)| *id == wanted)
        .map(|(_, _, bytes)| *bytes)
}

/// What the `Tracks` element says about the video track.
///
/// `entry` is the four characters the same codec's MP4 sample entry
/// would carry, so that one function decides the framing for both
/// containers.
#[derive(Clone, Debug)]
struct TrackInfo {
    number: u64,
    entry: [u8; 4],
    private: Vec<u8>,
}

fn track_info(tracks: &[u8]) -> Result<TrackInfo> {
    for (id, _, body) in children(tracks)? {
        if id != ID_TRACK_ENTRY {
            continue;
        }
        let fields = children(body)?;
        if field(&fields, ID_TRACK_TYPE).map(uint).unwrap_or(0) != 1 {
            continue;
        }
        let number = field(&fields, ID_TRACK_NUMBER)
            .map(uint)
            .ok_or(Error::Format("a video track with no number"))?;
        let name = field(&fields, ID_CODEC_ID)
            .map(|bytes| {
                String::from_utf8_lossy(bytes)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .unwrap_or_default();
        let entry = match name.as_str() {
            "V_MPEG4/ISO/AVC" => *b"avc1",
            "V_MPEGH/ISO/HEVC" => *b"hvc1",
            "V_AV1" => *b"av01",
            other => {
                return Err(Error::Unsupported(format!(
                    "the video track is {other}, which this format has no carriage for"
                )))
            }
        };
        // `FlagLacing` is a permission, not a fact: a track that allows
        // lacing may still have none. A block that turns out to be
        // laced is what gets refused, in `block_header`.
        return Ok(TrackInfo {
            number,
            entry,
            private: field(&fields, ID_CODEC_PRIVATE)
                .unwrap_or_default()
                .to_vec(),
        });
    }
    Err(Error::Unsupported(
        "the file holds no video track this format has carriage for".into(),
    ))
}

/// One block, as a cluster walk found it.
#[derive(Clone, Copy, Debug)]
struct RawBlock {
    offset: u64,
    size: u32,
    /// The block's own offset from its cluster's timestamp.
    relative: i64,
    keyframe: bool,
}

/// A block's header: its track number, how many bytes the header took,
/// its offset from the cluster's timestamp, and its flags.
///
/// The layout is the format's own: a track number written like a
/// length, a signed 16-bit offset, and one byte of flags.
fn block_header(bytes: &[u8]) -> Result<(u64, usize, i64, u8)> {
    let (track, track_len) = read_size(bytes)?;
    let track = track.ok_or(Error::Format("a block with no track number"))?;
    let rest = bytes
        .get(track_len..track_len + 3)
        .ok_or(Error::Format("the bytes end inside a block header"))?;
    let flags = rest[2];
    if flags & 0x06 != 0 {
        return Err(Error::Unsupported(
            "a laced video block, which this reader will not take apart".into(),
        ));
    }
    Ok((
        track,
        track_len + 3,
        i64::from(i16::from_be_bytes([rest[0], rest[1]])),
        flags,
    ))
}

/// Every block of one cluster that belongs to `track`.
///
/// `stop_after` is the keyframe fast path: once a block at or past that
/// time has been taken, the rest of the cluster is left unread, because
/// the cue that asked for it has been answered.
///
fn walk_cluster<R: Read + Seek>(
    src: &mut Source<R>,
    cluster: Element,
    limit: u64,
    track: u64,
    keyframes_only: bool,
    stop_after: Option<i64>,
) -> Result<Vec<Sample>> {
    let end = cluster.end_or(limit);
    let mut at = cluster.body;
    let mut timestamp = 0i64;
    let mut out: Vec<Sample> = Vec::new();
    let mut visited = 0usize;
    while at < end {
        let Some((element, window)) = read_element(src, at, end)? else {
            break;
        };
        if !is_cluster_child(element.id) {
            break;
        }
        visited += 1;
        if visited > MAX_ELEMENTS {
            return Err(Error::Format("more elements than a cluster has"));
        }
        let Some(next) = element.end else {
            break;
        };
        match element.id {
            ID_TIMESTAMP => {
                timestamp = uint(&src.span(element.body, next - element.body)?) as i64;
            }
            ID_SIMPLE_BLOCK => {
                let head = usize::try_from(element.body - element.start).unwrap_or(HEAD);
                let bytes = window
                    .get(head..)
                    .ok_or(Error::Format("the file ends inside a block header"))?;
                let (number, header_len, relative, flags) = block_header(bytes)?;
                let offset = element.body + header_len as u64;
                if offset > next {
                    return Err(Error::Format("a block header longer than the block"));
                }
                if number == track {
                    let block = RawBlock {
                        offset,
                        size: u32::try_from(next - offset).unwrap_or(u32::MAX),
                        relative,
                        keyframe: flags & 0x80 != 0,
                    };
                    if !keyframes_only || block.keyframe {
                        out.push(sample_of(block, timestamp));
                    }
                    if stop_after.is_some_and(|stop| timestamp + relative >= stop)
                        && !out.is_empty()
                    {
                        return Ok(out);
                    }
                }
            }
            ID_BLOCK_GROUP => {
                let body = body_of(src, (element.body, next))?;
                let fields = children(&body)?;
                // A block inside a group carries no keyframe flag. The
                // format's rule is that a block referencing nothing is
                // one, so the absence of a ReferenceBlock decides it.
                let keyframe = !fields.iter().any(|(id, _, _)| *id == ID_REFERENCE_BLOCK);
                for (id, start, bytes) in &fields {
                    if *id != ID_BLOCK {
                        continue;
                    }
                    let (number, header_len, relative, _) = block_header(bytes)?;
                    if number != track || header_len > bytes.len() {
                        continue;
                    }
                    let block = RawBlock {
                        offset: element.body + (*start + header_len) as u64,
                        size: u32::try_from(bytes.len() - header_len).unwrap_or(u32::MAX),
                        relative,
                        keyframe,
                    };
                    if !keyframes_only || keyframe {
                        out.push(sample_of(block, timestamp));
                    }
                }
            }
            _ => {}
        }
        if next <= at {
            break;
        }
        at = next;
    }
    Ok(out)
}

/// One block as a sample.
///
/// Matroska stores presentation time and nothing else, so `dts` is
/// `pts`: there is no composition offset to take back off, and a
/// reader that wants decode order has the order the blocks are in.
/// `duration` is left at zero, which is what "the file did not say"
/// means in the shared `Sample`.
fn sample_of(block: RawBlock, cluster_timestamp: i64) -> Sample {
    let pts = cluster_timestamp + block.relative;
    Sample {
        index: 0,
        offset: block.offset,
        size: block.size,
        dts: pts,
        pts,
        duration: 0,
        keyframe: block.keyframe,
    }
}

/// The cue points of a segment, as cluster positions and the latest
/// time cued in each.
fn cue_clusters(cues: &[u8], segment_body: u64, track: u64) -> Result<Vec<(u64, i64)>> {
    let mut out: Vec<(u64, i64)> = Vec::new();
    for (id, _, body) in children(cues)? {
        if id != ID_CUE_POINT {
            continue;
        }
        let fields = children(body)?;
        let time = field(&fields, ID_CUE_TIME)
            .map(|bytes| uint(bytes) as i64)
            .unwrap_or(0);
        for (id, _, positions) in &fields {
            if *id != ID_CUE_TRACK_POSITIONS {
                continue;
            }
            let inner = children(positions)?;
            let number = field(&inner, ID_CUE_TRACK).map(uint).unwrap_or(track);
            if number != track {
                continue;
            }
            let Some(bytes) = field(&inner, ID_CUE_CLUSTER_POSITION) else {
                continue;
            };
            let at = segment_body.saturating_add(uint(bytes));
            match out.iter_mut().find(|(start, _)| *start == at) {
                Some(found) => found.1 = found.1.max(time),
                None => out.push((at, time)),
            }
        }
    }
    out.sort_by_key(|(at, _)| *at);
    Ok(out)
}

/// The video track of a Matroska file, and the samples a scan of this
/// mode finds.
///
/// `Scan::Keyframes` uses `Cues` when the file has them, which turns a
/// full walk of every block into one seek per cued cluster. A file with
/// no cues, which is what a live writer produces, falls back to the
/// full walk and keeps the keyframes.
pub fn read<R: Read + Seek>(src: &mut Source<R>, scan: Scan) -> Result<Track> {
    let seg = segment(src)?;
    let map = outline(src, seg)?;
    let tracks = map
        .tracks
        .ok_or(Error::Format("the segment has no Tracks element"))?;
    let tracks = body_of(src, tracks)?;
    let info = track_info(&tracks)?;

    let scale_ns = match map.info {
        Some(span) => {
            let body = body_of(src, span)?;
            field(&children(&body)?, ID_TIMESTAMP_SCALE)
                .map(uint)
                .unwrap_or(1_000_000)
        }
        None => 1_000_000,
    };
    let scale_ns = scale_ns.clamp(1, 1_000_000_000);
    // Ticks a second, exactly where the scale divides a second evenly,
    // which it does for every file a muxer writes. Where it does not,
    // the ticks become nanoseconds and nothing is lost.
    let (timescale, multiplier) = if 1_000_000_000 % scale_ns == 0 {
        ((1_000_000_000 / scale_ns) as u32, 1i64)
    } else {
        (1_000_000_000u32, scale_ns as i64)
    };

    let keyframes_only = scan == Scan::Keyframes;
    let cues = match (keyframes_only, map.cues) {
        (true, Some(span)) => {
            let body = body_of(src, span)?;
            let found = cue_clusters(&body, seg.body, info.number)?;
            (!found.is_empty()).then_some(found)
        }
        _ => None,
    };

    let mut samples: Vec<Sample> = Vec::new();
    match cues {
        Some(cues) => {
            for (at, time) in cues {
                let Some((element, _)) = read_element(src, at, seg.end)? else {
                    continue;
                };
                if element.id != ID_CLUSTER {
                    return Err(Error::Format("a cue that does not point at a cluster"));
                }
                samples.extend(walk_cluster(
                    src,
                    element,
                    seg.end,
                    info.number,
                    true,
                    Some(time),
                )?);
            }
        }
        None => {
            for cluster in &map.clusters {
                samples.extend(walk_cluster(
                    src,
                    *cluster,
                    seg.end,
                    info.number,
                    keyframes_only,
                    None,
                )?);
                if samples.len() > ffrwd_bmff::MAX_SAMPLES {
                    return Err(Error::Format("more samples than this reader will hold"));
                }
            }
        }
    }
    // The blocks stay in the order the file has them, which is decode
    // order, the same order an MP4's sample tables are in and the order
    // ffprobe lists packets in, which is what `from_parts` asks for. It
    // renumbers `index` itself, so this loop only scales the times.
    for sample in samples.iter_mut() {
        sample.pts *= multiplier;
        sample.dts = sample.pts;
    }

    // What `from_parts` is not given is what Matroska does not have: no
    // edit list, so no start shift and no movie timescale, and no
    // `trex`, because there are no movie fragments. The offsets and
    // sizes above are absolute in `src`, and the times are in the ticks
    // `timescale` counts, which is what it asks the caller to promise.
    // The track number is Matroska's own, which is the nearest thing
    // the format has to a `track_ID`.
    Ok(Track::from_parts(
        Handler::Video,
        timescale,
        SampleEntry {
            kind: info.entry,
            config: info.private,
            ..SampleEntry::default()
        },
        samples,
    )
    .with_track_id(u32::try_from(info.number).unwrap_or(1)))
}

/// The file index out of a Matroska attachment, if there is one.
///
/// Section 8: the MIME type is what a reader looks for, and the first
/// attachment carrying it is the one taken. The file name is the
/// writer's business and not the reader's, so an attachment renamed on
/// its way through a muxer costs nothing here.
pub fn read_index<R: Read + Seek>(src: &mut Source<R>) -> Result<Option<Vec<u8>>> {
    let seg = segment(src)?;
    let map = outline(src, seg)?;
    let Some(span) = map.attachments else {
        return Ok(None);
    };
    let body = body_of(src, span)?;
    for (id, _, file) in children(&body)? {
        if id != ID_ATTACHED_FILE {
            continue;
        }
        let fields = children(file)?;
        let mime = field(&fields, ID_FILE_MIME_TYPE)
            .map(|bytes| {
                String::from_utf8_lossy(bytes)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .unwrap_or_default();
        if mime != MATROSKA_MIME {
            continue;
        }
        if let Some(data) = field(&fields, ID_FILE_DATA) {
            return Ok(Some(data.to_vec()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn vint(value: u64) -> Vec<u8> {
        for len in 1..=8usize {
            let max = (1u64 << (7 * len)) - 1;
            if value < max {
                let mut out = value.to_be_bytes()[8 - len..].to_vec();
                out[0] |= 0x80 >> (len - 1);
                return out;
            }
        }
        vec![0xff]
    }

    fn id_bytes(id: u32) -> Vec<u8> {
        let full = id.to_be_bytes();
        let start = full.iter().position(|byte| *byte != 0).unwrap_or(3);
        full[start..].to_vec()
    }

    fn element(id: u32, body: &[u8]) -> Vec<u8> {
        let mut out = id_bytes(id);
        out.extend_from_slice(&vint(body.len() as u64));
        out.extend_from_slice(body);
        out
    }

    fn unknown(id: u32, body: &[u8]) -> Vec<u8> {
        let mut out = id_bytes(id);
        out.push(0xff);
        out.extend_from_slice(body);
        out
    }

    fn simple_block(track: u64, relative: i16, keyframe: bool, frame: &[u8]) -> Vec<u8> {
        let mut body = vint(track);
        body.extend_from_slice(&relative.to_be_bytes());
        body.push(if keyframe { 0x80 } else { 0x00 });
        body.extend_from_slice(frame);
        element(ID_SIMPLE_BLOCK, &body)
    }

    fn tracks(codec: &str, private: &[u8]) -> Vec<u8> {
        let mut entry = element(ID_TRACK_NUMBER, &[1]);
        entry.extend_from_slice(&element(ID_TRACK_TYPE, &[1]));
        entry.extend_from_slice(&element(ID_CODEC_ID, codec.as_bytes()));
        entry.extend_from_slice(&element(ID_CODEC_PRIVATE, private));
        element(ID_TRACKS, &element(ID_TRACK_ENTRY, &entry))
    }

    /// A file of two clusters, the second with an unknown length, which
    /// is what a live writer produces.
    fn file(segment_unknown: bool, cluster_unknown: bool) -> Vec<u8> {
        file_with_tail(segment_unknown, cluster_unknown, false)
    }

    /// `trailing` adds a block after the last keyframe, so that the
    /// track's last sample is not a sync sample, which is the shape a
    /// keyframe scan must not go looking past.
    fn file_with_tail(segment_unknown: bool, cluster_unknown: bool, trailing: bool) -> Vec<u8> {
        let mut first = element(ID_TIMESTAMP, &[0]);
        first.extend_from_slice(&simple_block(1, 0, true, &[1, 2, 3, 4]));
        first.extend_from_slice(&simple_block(1, 33, false, &[5, 6]));
        let mut second = element(ID_TIMESTAMP, &[100]);
        second.extend_from_slice(&simple_block(1, 0, true, &[7, 8, 9]));
        if trailing {
            second.extend_from_slice(&simple_block(1, 33, false, &[10, 11, 12, 13, 14]));
        }

        let mut body = element(ID_INFO, &element(ID_TIMESTAMP_SCALE, &[0x0f, 0x42, 0x40]));
        body.extend_from_slice(&tracks("V_MPEG4/ISO/AVC", &[1, 0x64, 0, 13, 0xff]));
        body.extend_from_slice(&element(ID_CLUSTER, &first));
        body.extend_from_slice(&if cluster_unknown {
            unknown(ID_CLUSTER, &second)
        } else {
            element(ID_CLUSTER, &second)
        });

        let mut out = element(ID_EBML, &[0x42, 0x86, 0x81, 0x01]);
        out.extend_from_slice(&if segment_unknown {
            unknown(ID_SEGMENT, &body)
        } else {
            element(ID_SEGMENT, &body)
        });
        out
    }

    fn source(bytes: Vec<u8>) -> Source<Cursor<Vec<u8>>> {
        Source::new(Cursor::new(bytes)).expect("a source")
    }

    #[test]
    fn a_varint_reads_its_id_and_its_length() {
        assert_eq!(
            read_id(&[0x1a, 0x45, 0xdf, 0xa3]).expect("an id"),
            (ID_EBML, 4)
        );
        assert_eq!(read_id(&[0xa3]).expect("an id"), (ID_SIMPLE_BLOCK, 1));
        assert!(read_id(&[0x00, 0x01]).is_err(), "an id of no class");
        assert!(read_id(&[0x07, 0, 0, 0, 0]).is_err(), "wider than four");
        assert_eq!(read_size(&[0x81]).expect("a length"), (Some(1), 1));
        assert_eq!(read_size(&[0xff]).expect("a length"), (None, 1));
        assert_eq!(
            read_size(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).expect("a length"),
            (None, 8)
        );
    }

    #[test]
    fn both_unknown_sizes_still_give_every_block() {
        for (segment_unknown, cluster_unknown) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut src = source(file(segment_unknown, cluster_unknown));
            let track = read(&mut src, Scan::All).expect("a track");
            assert_eq!(&track.entry.kind, b"avc1");
            assert_eq!(
                crate::framing_of(&track.entry.kind, &track.entry.config).expect("a framing"),
                ffrwd_nal::config::Framing::LengthPrefixed {
                    codec: ffrwd_nal::Codec::H264,
                    length_size: 4
                }
            );
            assert_eq!(track.timescale, 1000, "a millisecond a tick");
            let times: Vec<i64> = track.samples.iter().map(|sample| sample.pts).collect();
            assert_eq!(
                times,
                vec![0, 33, 100],
                "segment unknown {segment_unknown}, cluster unknown {cluster_unknown}"
            );
            let keys: Vec<bool> = track.samples.iter().map(|s| s.keyframe).collect();
            assert_eq!(keys, vec![true, false, true]);
            assert_eq!(track.samples[0].size, 4);

            let mut src = source(file(segment_unknown, cluster_unknown));
            let only_keys = read(&mut src, Scan::Keyframes).expect("a track");
            assert_eq!(only_keys.samples.len(), 2);
        }
    }

    #[test]
    fn a_keyframe_scan_reads_the_sync_samples_and_nothing_else() {
        // Section 7 puts every record of a `keyframe` file on a
        // keyframe, the last one included, so the fast path has no
        // reason to go and find the end of the track. On a transport
        // stream there is no end to go and find.
        for (segment_unknown, cluster_unknown) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let bytes = file_with_tail(segment_unknown, cluster_unknown, true);
            let mut src = source(bytes.clone());
            let all = read(&mut src, Scan::All).expect("a track");
            assert_eq!(all.samples.len(), 4);
            assert!(!all.samples[3].keyframe, "the last block is not a keyframe");

            let mut src = source(bytes);
            let fast = read(&mut src, Scan::Keyframes).expect("a track");
            let visited: Vec<i64> = Scan::Keyframes
                .samples(&fast)
                .iter()
                .map(|sample| sample.pts)
                .collect();
            assert_eq!(
                visited,
                vec![0, 100],
                "the sync samples alone, segment unknown {segment_unknown},                  cluster unknown {cluster_unknown}"
            );
        }
    }

    #[test]
    fn a_laced_block_is_refused_by_name() {
        let mut body = vint(1);
        body.extend_from_slice(&0i16.to_be_bytes());
        body.push(0x80 | 0x02); // Xiph lacing
        body.extend_from_slice(&[1, 2, 3]);
        let err = block_header(&body).expect_err("a refusal");
        assert!(format!("{err}").contains("laced"), "{err}");
    }

    #[test]
    fn every_truncation_of_a_file_is_an_error_and_not_a_panic() {
        let bytes = file(false, false);
        for cut in 0..bytes.len() {
            let mut src = source(bytes[..cut].to_vec());
            let _ = read(&mut src, Scan::All);
            let _ = read(&mut src, Scan::Keyframes);
            let _ = read_index(&mut src);
        }
    }
}
