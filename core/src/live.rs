//! Cutting a growing stream into carriers as its bytes arrive.
//!
//! [`avc::access_units`] and [`obu::temporal_units`] want the whole
//! stream in memory, which is what a file reader has. A watcher does
//! not: it is handed a pipe that never ends and never seeks, and it has
//! to say what it found on a carrier before the next one is written.
//!
//! So this is the same two boundary rules fed a chunk at a time. Bytes
//! go in, and every carrier the bytes have finished comes out with the
//! units of this format already taken off it, so the bytes themselves
//! are dropped as soon as they are understood. What is held is one
//! unfinished carrier and no more: a carrier that grows past
//! [`Feed::limit`] without ending is given up on, its bytes are dropped,
//! and the feed resynchronises on the next start code or OBU. That is
//! the difference between a reader of a file, which may believe its
//! input, and a reader of a socket, which may not.
//!
//! There is no I/O here. The caller owns the pipe and pushes what it
//! read, which is what keeps this crate free of both.

use crate::avc::{self, Codec};
use crate::obu;
use crate::{Error, Result};

/// Which elementary stream a feed is cutting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    /// H.264 or HEVC, Annex B start codes.
    Nal(Codec),
    /// AV1, low-overhead OBUs.
    Av1,
}

/// One carrier a feed has finished with.
#[derive(Clone, Debug, PartialEq)]
pub struct Carried {
    /// Its place in decode order, from zero.
    pub index: u64,
    /// Whether a reader may start here: a random access point in H.264
    /// and HEVC, a temporal unit carrying a sequence header in AV1.
    pub keyframe: bool,
    /// The units of this format that rode on it.
    pub units: Vec<Vec<u8>>,
}

/// How many bytes one carrier may take before a feed gives up on it.
///
/// A 4K keyframe is a megabyte or two, so this is room for one and then
/// some. What it really bounds is a stream that is not the codec it was
/// said to be, where no boundary ever turns up.
pub const DEFAULT_LIMIT: usize = 8 << 20;

/// Bytes in, carriers out.
#[derive(Clone, Debug)]
pub struct Feed {
    kind: StreamKind,
    /// The carrier being read, from its first byte. Never longer than
    /// `limit` plus the chunk that crossed it.
    held: Vec<u8>,
    index: u64,
    limit: usize,
    dropped: usize,
}

impl Feed {
    /// A feed for one kind of stream, with [`DEFAULT_LIMIT`].
    pub fn new(kind: StreamKind) -> Self {
        Self::with_limit(kind, DEFAULT_LIMIT)
    }

    /// A feed that gives up on a carrier past `limit` bytes.
    pub fn with_limit(kind: StreamKind, limit: usize) -> Self {
        Self {
            kind,
            held: Vec::new(),
            index: 0,
            limit: limit.max(64),
            dropped: 0,
        }
    }

    /// How many carriers have come out so far, which is what gives the
    /// next one its presentation time when a frame rate is all a caller
    /// has.
    pub fn count(&self) -> u64 {
        self.index
    }

    /// How many bytes are being held for the carrier in hand.
    pub fn footprint(&self) -> usize {
        self.held.len()
    }

    /// How many carriers were given up on for growing past the limit.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// The carriers `chunk` finished.
    ///
    /// The last carrier of a stream is not one of them: nothing after it
    /// says where it ends, so it comes out of [`Feed::finish`] when the
    /// caller knows the stream is over.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Carried>> {
        self.held.extend_from_slice(chunk);
        let found = self.cut();
        if found.is_err() {
            // The bytes are not the codec they were said to be. What is
            // held cannot parse now and will not parse when more
            // arrives, so it goes: the caller hears the error, and the
            // next chunk starts again rather than piling up behind a
            // byte that will never be read.
            self.held.clear();
            self.dropped += 1;
        }
        if self.held.len() > self.limit {
            // No boundary in a carrier's worth of bytes. Keep the last
            // three, which is as much as a start code split across the
            // cut can be, and let the next one resynchronise.
            let keep = self.held.len() - 3;
            self.held.drain(..keep);
            self.dropped += 1;
        }
        found
    }

    /// Every carrier the held bytes have finished, taken off them.
    fn cut(&mut self) -> Result<Vec<Carried>> {
        let mut out = Vec::new();
        loop {
            let (complete, pending_at) = self.boundaries()?;
            for (start, end, keyframe) in complete {
                out.push(self.carried(start, end, keyframe));
            }
            if pending_at == 0 {
                return Ok(out);
            }
            self.held.drain(..pending_at);
        }
    }

    /// The carrier still in hand, now that the stream has ended.
    pub fn finish(&mut self) -> Result<Vec<Carried>> {
        if self.held.is_empty() {
            return Ok(Vec::new());
        }
        let keyframe = match self.kind {
            StreamKind::Nal(codec) => avc::access_units(&self.held, codec)
                .first()
                .is_some_and(|unit| unit.keyframe),
            StreamKind::Av1 => obu::temporal_units(&self.held)
                .ok()
                .and_then(|units| units.first().map(|unit| unit.has_sequence_header))
                .unwrap_or(false),
        };
        let end = self.held.len();
        let last = self.carried(0, end, keyframe);
        self.held.clear();
        Ok(vec![last])
    }

    /// One finished carrier: its units taken off it and its bytes left
    /// behind.
    fn carried(&mut self, start: usize, end: usize, keyframe: bool) -> Carried {
        let bytes = &self.held[start..end];
        let units = match self.kind {
            StreamKind::Nal(codec) => avc::units_annexb(bytes, codec),
            StreamKind::Av1 => obu::units_obu(bytes),
        };
        let index = self.index;
        self.index += 1;
        Carried {
            index,
            keyframe,
            units,
        }
    }

    /// The carriers the held bytes have finished, and where the one
    /// still open begins.
    ///
    /// Each carrier is `(start, end, keyframe)` inside `held`.
    #[allow(clippy::type_complexity)]
    fn boundaries(&self) -> Result<(Vec<(usize, usize, bool)>, usize)> {
        match self.kind {
            StreamKind::Nal(codec) => {
                let units = avc::access_units(&self.held, codec);
                // The last access unit is still open: nothing has said
                // where it ends yet.
                let Some((open, complete)) = units.split_last() else {
                    return Ok((Vec::new(), 0));
                };
                Ok((
                    complete
                        .iter()
                        .map(|unit| (unit.start, unit.end, unit.keyframe))
                        .collect(),
                    open.start,
                ))
            }
            StreamKind::Av1 => av1_boundaries(&self.held),
        }
    }
}

/// The complete temporal units of a prefix of an AV1 stream, and where
/// the open one begins.
///
/// An OBU that carries no size field runs to the end of the stream, so a
/// feed cannot tell where it ends and holds it: a writer of a growing
/// stream has to write sizes, and the low-overhead format every muxer
/// writes does.
#[allow(clippy::type_complexity)]
fn av1_boundaries(bytes: &[u8]) -> Result<(Vec<(usize, usize, bool)>, usize)> {
    let mut units: Vec<(usize, usize, bool)> = Vec::new();
    let mut at = 0usize;
    while let Some(header) = bytes.get(at).copied() {
        if header & 0x80 != 0 {
            return Err(Error::Malformed("an OBU with its forbidden bit set"));
        }
        let kind = header >> 3 & 0x0f;
        let mut cursor = at + 1 + usize::from(header & 0x04 != 0);
        if cursor > bytes.len() {
            break;
        }
        let end = if header & 0x02 != 0 {
            let Ok((size, next)) = obu::leb128(bytes, cursor) else {
                break;
            };
            cursor = next;
            let Ok(size) = usize::try_from(size) else {
                return Err(Error::TooLarge);
            };
            match cursor.checked_add(size) {
                Some(end) if end <= bytes.len() => end,
                Some(_) => break,
                None => return Err(Error::TooLarge),
            }
        } else {
            break;
        };
        if kind == obu::OBU_TEMPORAL_DELIMITER || units.is_empty() {
            if let Some(last) = units.last_mut() {
                last.1 = at;
            }
            units.push((at, bytes.len(), false));
        }
        if kind == obu::OBU_SEQUENCE_HEADER {
            if let Some(last) = units.last_mut() {
                last.2 = true;
            }
        }
        at = end;
    }
    // The last temporal unit is still open, and so is any OBU that has
    // not parsed yet.
    let Some(open) = units.pop() else {
        return Ok((Vec::new(), 0));
    };
    Ok((units, open.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Encoding, Message, Space, Unit};

    fn unit(id: u8) -> Vec<u8> {
        let mut space = Space::new(id, 4, Encoding::F32);
        space.model = "test:model".into();
        Unit::new(vec![Message::Space(space)]).encode()
    }

    /// Three H.264 access units, the first an IDR, each carrying one
    /// unit of this format.
    fn h264_stream() -> Vec<u8> {
        let mut out = Vec::new();
        for (index, slice) in [0x65u8, 0x41, 0x41].iter().enumerate() {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&[0x67, 0x64, 0x00, 0x1f]);
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&avc::wrap_unit(&unit(index as u8 + 1), Codec::H264));
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&[*slice, 0x88, 0x84, index as u8]);
        }
        out
    }

    fn av1_stream() -> Vec<u8> {
        let mut out = Vec::new();
        for index in 0..3u8 {
            out.push(obu::OBU_TEMPORAL_DELIMITER << 3 | 0x02);
            obu::put_leb128(&mut out, 0);
            if index == 0 {
                out.push(obu::OBU_SEQUENCE_HEADER << 3 | 0x02);
                obu::put_leb128(&mut out, 2);
                out.extend_from_slice(&[0x0a, 0x0b]);
            }
            out.extend_from_slice(&obu::write_metadata_obu(&unit(index + 1)));
            out.push(obu::OBU_FRAME << 3 | 0x02);
            obu::put_leb128(&mut out, 8);
            out.extend_from_slice(&[index; 8]);
        }
        out
    }

    /// Every carrier a feed makes of `stream`, pushed `step` bytes at a
    /// time, with the index each one came out after.
    fn fed(kind: StreamKind, stream: &[u8], step: usize) -> Vec<(usize, Carried)> {
        let mut feed = Feed::new(kind);
        let mut out = Vec::new();
        let mut at = 0usize;
        while at < stream.len() {
            let end = (at + step).min(stream.len());
            for carried in feed.push(&stream[at..end]).expect("a push") {
                out.push((end, carried));
            }
            at = end;
        }
        for carried in feed.finish().expect("the last carrier") {
            out.push((stream.len(), carried));
        }
        out
    }

    #[test]
    fn a_stream_cut_at_any_chunk_size_gives_the_same_carriers() {
        for (kind, stream) in [
            (StreamKind::Nal(Codec::H264), h264_stream()),
            (StreamKind::Av1, av1_stream()),
        ] {
            let whole = fed(kind, &stream, stream.len());
            let carriers: Vec<Carried> = whole.iter().map(|(_, c)| c.clone()).collect();
            assert_eq!(carriers.len(), 3, "{kind:?}");
            assert!(carriers[0].keyframe, "{kind:?}: the first is a key frame");
            assert!(!carriers[1].keyframe, "{kind:?}");
            for (index, carried) in carriers.iter().enumerate() {
                assert_eq!(carried.index, index as u64);
                assert_eq!(carried.units, vec![unit(index as u8 + 1)], "{kind:?}");
            }
            for step in [1usize, 2, 3, 5, 7, 16, 64] {
                let again: Vec<Carried> = fed(kind, &stream, step)
                    .into_iter()
                    .map(|(_, c)| c)
                    .collect();
                assert_eq!(again, carriers, "{kind:?} at {step} bytes a chunk");
            }
        }
    }

    #[test]
    fn a_carrier_comes_out_before_the_next_ones_bytes_are_in() {
        // This is the whole point of the feed: a watcher hears about a
        // carrier as soon as the carrier after it has begun, not when
        // the stream ends. Each carrier here is out before the bytes of
        // the one two along have been pushed.
        for (kind, stream) in [
            (StreamKind::Nal(Codec::H264), h264_stream()),
            (StreamKind::Av1, av1_stream()),
        ] {
            let starts: Vec<usize> = match kind {
                StreamKind::Nal(codec) => avc::access_units(&stream, codec)
                    .iter()
                    .map(|unit| unit.start)
                    .collect(),
                StreamKind::Av1 => obu::temporal_units(&stream)
                    .expect("temporal units")
                    .iter()
                    .map(|unit| unit.start)
                    .collect(),
            };
            for (fed_by, carried) in fed(kind, &stream, 4) {
                let next = carried.index as usize + 1;
                if let Some(after) = starts.get(next + 1) {
                    assert!(
                        fed_by <= *after,
                        "{kind:?}: carrier {} waited for byte {fed_by}, past {after}",
                        carried.index
                    );
                }
            }
        }
    }

    #[test]
    fn what_is_held_is_one_carrier_and_no_more() {
        let stream = h264_stream();
        let mut feed = Feed::new(StreamKind::Nal(Codec::H264));
        let mut most = 0usize;
        for chunk in stream.chunks(3) {
            feed.push(chunk).expect("a push");
            most = most.max(feed.footprint());
        }
        assert!(most < stream.len(), "the feed held the whole stream");
        assert_eq!(feed.dropped(), 0);
    }

    #[test]
    fn a_carrier_past_the_limit_is_given_up_on_and_the_feed_carries_on() {
        let mut feed = Feed::with_limit(StreamKind::Nal(Codec::H264), 256);
        // A slice larger than the limit, and no boundary in it.
        let mut flood = vec![0, 0, 0, 1, 0x65];
        flood.extend(std::iter::repeat_n(0x42u8, 4000));
        assert!(feed.push(&flood).expect("a push").is_empty());
        assert!(feed.footprint() <= 256, "{}", feed.footprint());
        assert!(feed.dropped() > 0, "the flood was held whole");
        // And the carriers after it still read.
        let carriers = {
            let stream = h264_stream();
            let mut out = feed.push(&stream).expect("a push");
            out.extend(feed.finish().expect("the last one"));
            out
        };
        let found: Vec<Vec<Vec<u8>>> = carriers.into_iter().map(|c| c.units).collect();
        assert!(
            found.contains(&vec![unit(3)]),
            "the feed never resynchronised: {found:?}"
        );
    }

    #[test]
    fn an_av1_obu_with_its_forbidden_bit_set_is_refused() {
        let mut feed = Feed::new(StreamKind::Av1);
        assert_eq!(
            feed.push(&[0x80]),
            Err(Error::Malformed("an OBU with its forbidden bit set"))
        );
    }

    #[test]
    fn random_bytes_never_panic_and_never_grow() {
        let mut seed = 0x51ed_2c93_7f10_ab44u64;
        for kind in [
            StreamKind::Nal(Codec::H264),
            StreamKind::Nal(Codec::H265),
            StreamKind::Av1,
        ] {
            let mut feed = Feed::with_limit(kind, 1024);
            for _ in 0..3000 {
                let mut bytes = Vec::new();
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                for _ in 0..(seed >> 40) % 48 {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    bytes.push((seed >> 33) as u8);
                }
                let _ = feed.push(&bytes);
                assert!(feed.footprint() <= 1024, "{}", feed.footprint());
            }
            let _ = feed.finish();
        }
    }
}
