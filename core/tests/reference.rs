//! The carriage pinned against real encoders and a real remuxer.
//!
//! `tests/data/` holds three short clips made once with ffmpeg 9 and
//! committed, so the byte-level tests below run with nothing installed:
//!
//!     ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -t 2 \
//!       -c:v libx264 -preset veryfast -g 30 -bf 2 -pix_fmt yuv420p \
//!       -f h264 core/tests/data/ref.h264
//!
//!     ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -t 2 \
//!       -c:v libx265 -preset fast -pix_fmt yuv420p \
//!       -x265-params keyint=30:min-keyint=30:bframes=2:log-level=error \
//!       -f hevc core/tests/data/ref.h265
//!
//!     ffmpeg -f lavfi -i testsrc2=size=320x176:rate=30 -t 2 \
//!       -c:v libsvtav1 -preset 10 -crf 40 -g 30 -pix_fmt yuv420p \
//!       -f obu core/tests/data/ref.obu
//!
//! The AV1 clip is 320x176 because SVT-AV1 wants a height it can
//! divide by eight and pads one that it cannot, which would leave the
//! committed file at a size nobody asked for.
//!
//! Everything asserted about the fixtures is read out of them, never
//! assumed: the access units, the keyframes and x264's own SEI are
//! found by this file's own hand-written scanner as well as by the
//! crate, and the two must agree. The tests that need ffmpeg skip
//! themselves with a message when it is not on the PATH, and say so
//! rather than passing quietly.
//!
//! Timing: an elementary stream carries no timestamps, so a carrier's
//! presentation time here is its position in decode order at 30 frames
//! a second, the same convention the tool's `--fps` uses. With B-frames
//! that is not the true presentation time; it is a consistent one, and
//! every test that needs real times asks ffprobe for them.

use std::path::{Path, PathBuf};
use std::process::Command;

use ffrwd_index_core::assemble::{Assembler, Limits};
use ffrwd_index_core::avc::{self, Codec};
use ffrwd_index_core::index::FileIndex;
use ffrwd_index_core::message::{Encoding, Message, Modality, Space, Unit, VectorRecord};
use ffrwd_index_core::obu;
use ffrwd_index_core::placement::{plan, Carrier, Mode, Pending, Placement};
use ffrwd_index_core::quant::Planes;
use ffrwd_index_core::UUID;

const FPS: i64 = 30;

/// Which fixture, and how its carriage works.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    H264,
    H265,
    Av1,
}

impl Stream {
    fn every() -> [Stream; 3] {
        [Stream::H264, Stream::H265, Stream::Av1]
    }

    fn name(self) -> &'static str {
        match self {
            Stream::H264 => "h264",
            Stream::H265 => "h265",
            Stream::Av1 => "av1",
        }
    }

    /// The extension ffmpeg's demuxer for this elementary stream wants.
    fn extension(self) -> &'static str {
        match self {
            Stream::H264 => "h264",
            Stream::H265 => "h265",
            Stream::Av1 => "obu",
        }
    }

    /// The format name for ffmpeg's `-f`.
    fn format(self) -> &'static str {
        match self {
            Stream::H264 => "h264",
            Stream::H265 => "hevc",
            Stream::Av1 => "obu",
        }
    }

    /// The bitstream filter that unpacks this codec out of MP4 and
    /// Matroska, where NALs carry lengths instead of start codes.
    fn to_annexb(self) -> Option<&'static str> {
        match self {
            Stream::H264 => Some("h264_mp4toannexb"),
            Stream::H265 => Some("hevc_mp4toannexb"),
            Stream::Av1 => None,
        }
    }

    fn codec(self) -> Option<Codec> {
        match self {
            Stream::H264 => Some(Codec::H264),
            Stream::H265 => Some(Codec::H265),
            Stream::Av1 => None,
        }
    }

    fn bytes(self) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(format!("ref.{}", self.extension()));
        std::fs::read(&path).expect("the committed fixture")
    }

    /// The carriers of a stream: where each one starts, where a unit
    /// goes in it, and what time it is.
    fn carriers(self, bytes: &[u8]) -> Vec<Spot> {
        match self.codec() {
            Some(codec) => avc::access_units(bytes, codec)
                .into_iter()
                .enumerate()
                .map(|(index, unit)| Spot {
                    start: unit.start,
                    end: unit.end,
                    insert_at: unit.insert_at,
                    keyframe: unit.keyframe,
                    pts_ms: index as i64 * 1000 / FPS,
                })
                .collect(),
            None => obu::temporal_units(bytes)
                .expect("the AV1 fixture reads")
                .into_iter()
                .enumerate()
                .map(|(index, unit)| Spot {
                    start: unit.start,
                    end: unit.end,
                    insert_at: unit.insert_at,
                    // An AV1 writer repeats the sequence header before
                    // each key frame, which is as close to a sync
                    // sample as a reader gets without decoding.
                    keyframe: unit.has_sequence_header,
                    pts_ms: index as i64 * 1000 / FPS,
                })
                .collect(),
        }
    }

    /// The stream with one unit spliced into each carrier that has one.
    fn splice(self, bytes: &[u8], units: &[Option<Vec<u8>>]) -> Vec<u8> {
        let spots = self.carriers(bytes);
        assert_eq!(spots.len(), units.len(), "one slot per carrier");
        let mut out = Vec::with_capacity(bytes.len() + 4096);
        let mut at = 0usize;
        for (spot, unit) in spots.iter().zip(units) {
            let Some(unit) = unit else { continue };
            out.extend_from_slice(&bytes[at..spot.insert_at]);
            match self.codec() {
                Some(codec) => {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(&avc::wrap_unit(unit, codec));
                }
                None => out.extend_from_slice(&obu::write_metadata_obu(unit)),
            }
            at = spot.insert_at;
        }
        out.extend_from_slice(&bytes[at..]);
        out
    }

    /// The units in each carrier of a stream.
    fn units(self, bytes: &[u8]) -> Vec<Vec<Vec<u8>>> {
        self.carriers(bytes)
            .into_iter()
            .map(|spot| {
                let inside = &bytes[spot.start..spot.end];
                match self.codec() {
                    Some(codec) => avc::units_annexb(inside, codec),
                    None => obu::units_obu(inside),
                }
            })
            .collect()
    }
}

/// One carrier: where it is and what time it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Spot {
    start: usize,
    end: usize,
    insert_at: usize,
    keyframe: bool,
    pts_ms: i64,
}

/// A record as it went in, to compare with what comes back.
#[derive(Clone, Debug, PartialEq)]
struct Want {
    space_id: u8,
    record_id: u16,
    start_ms: i64,
    end_ms: i64,
    body: Vec<u8>,
}

/// The two spaces the tests weave: the layered encoding, which arrives
/// in pieces, and a float one, which does not.
fn spaces() -> Vec<Space> {
    let mut layered = Space::new(1, 64, Encoding::I8);
    layered.modality = Modality::Picture;
    layered.model = "test:layered".into();
    layered.query = "test:layered-text".into();
    layered.producer = "ffrwd-index reference test".into();
    let mut floats = Space::new(2, 8, Encoding::F32);
    floats.modality = Modality::Description;
    floats.model = "test:floats".into();
    vec![layered, floats]
}

/// A deterministic vector, so every run weaves the same bytes.
fn vector(seed: u64, dims: usize) -> Vec<f32> {
    (0..dims)
        .map(|index| {
            let turn = (seed as f32 + 1.0) * 0.37 + index as f32 * 0.21;
            turn.sin() * (1.0 + (seed % 3) as f32)
        })
        .collect()
}

/// The records the tests weave, and what they should read back as.
fn records() -> (Vec<Pending>, Vec<Want>) {
    let mut pending = Vec::new();
    let mut want = Vec::new();
    for index in 0..6u16 {
        let start = i64::from(index) * 300;
        let end = start + 250;
        let planes = Planes::quantize(&vector(u64::from(index), 64)).expect("quantized");
        pending.push(Pending::layered(1, index, start, end, &planes));
        want.push(Want {
            space_id: 1,
            record_id: index,
            start_ms: start,
            end_ms: end,
            body: planes.encode(),
        });
    }
    for index in 0..3u16 {
        let start = i64::from(index) * 600;
        let end = start + 400;
        let values = vector(u64::from(index) + 100, 8);
        let body: Vec<u8> = values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        pending.push(Pending::whole(2, index, start, end, body.clone()));
        want.push(Want {
            space_id: 2,
            record_id: index,
            start_ms: start,
            end_ms: end,
            body,
        });
    }
    (pending, want)
}

/// One woven stream and everything the tests need to check it.
struct Woven {
    stream: Stream,
    original: Vec<u8>,
    bytes: Vec<u8>,
    want: Vec<Want>,
    carriers: Vec<i64>,
}

/// Weaves the fixture with one placement policy.
fn weave(stream: Stream, policy: Placement) -> Woven {
    let original = stream.bytes();
    let spots = stream.carriers(&original);
    let carriers: Vec<Carrier> = spots
        .iter()
        .map(|spot| Carrier {
            pts_ms: spot.pts_ms,
            keyframe: spot.keyframe,
        })
        .collect();
    let (pending, want) = records();
    let planned = plan(policy, Mode::File, &spaces(), &pending, &carriers);
    let units: Vec<Option<Vec<u8>>> = planned
        .iter()
        .map(|messages| {
            if messages.is_empty() {
                None
            } else {
                Some(Unit::new(messages.clone()).encode())
            }
        })
        .collect();
    let bytes = stream.splice(&original, &units);
    Woven {
        stream,
        original,
        bytes,
        want,
        carriers: spots.iter().map(|spot| spot.pts_ms).collect(),
    }
}

impl Woven {
    /// The records read back out of the woven bytes.
    fn read(&self) -> Vec<Want> {
        read_records(self.stream, &self.bytes, &self.carriers)
    }
}

/// Every record in a stream, read with the crate.
fn read_records(stream: Stream, bytes: &[u8], carriers: &[i64]) -> Vec<Want> {
    let mut assembler = Assembler::new(Limits {
        max_records: 65536,
        ..Limits::default()
    });
    for (index, units) in stream.units(bytes).into_iter().enumerate() {
        let pts = carriers
            .get(index)
            .copied()
            .unwrap_or(index as i64 * 1000 / FPS);
        for unit in units {
            let unit = Unit::decode(&unit).expect("a unit of this format");
            assert_eq!(unit.dropped, 0, "a message in carrier {index} was dropped");
            assembler.push_unit(pts, &unit);
        }
    }
    assert_eq!(assembler.dropped(), 0, "the assembler dropped something");
    let mut out: Vec<Want> = assembler
        .records()
        .into_iter()
        .map(|record| Want {
            space_id: record.space.space_id,
            record_id: record.record_id,
            start_ms: record.start_ms,
            end_ms: record.end_ms,
            body: record.body.encode(),
        })
        .collect();
    out.sort_by_key(|want| (want.space_id, want.record_id));
    out
}

// ---------------------------------------------------------------- //
// An independent reader, written without the crate.
// ---------------------------------------------------------------- //

/// The units of an Annex B stream, found by hand.
///
/// Deliberately naive and deliberately not the crate's code: find start
/// codes, take SEI NALs, undo the escapes, walk the messages, keep the
/// payloads of type 5 that open with the UUID.
fn units_by_hand(bytes: &[u8], header_len: usize, sei_type: u8) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut starts = Vec::new();
    for at in 0..bytes.len().saturating_sub(2) {
        if bytes[at] == 0 && bytes[at + 1] == 0 && bytes[at + 2] == 1 {
            starts.push(at + 3);
        }
    }
    for (index, start) in starts.iter().enumerate() {
        let mut end = starts.get(index + 1).copied().unwrap_or(bytes.len() + 3) - 3;
        if end > *start && bytes[end - 1] == 0 {
            end -= 1;
        }
        let nal = &bytes[*start..end.min(bytes.len())];
        let kind = if header_len == 1 {
            nal.first().map(|byte| byte & 0x1f)
        } else {
            nal.first().map(|byte| byte >> 1 & 0x3f)
        };
        if kind != Some(sei_type) || nal.len() <= header_len {
            continue;
        }
        let mut rbsp = Vec::new();
        let mut zeros = 0;
        for byte in &nal[header_len..] {
            if zeros == 2 && *byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if *byte == 0 { zeros + 1 } else { 0 };
            rbsp.push(*byte);
        }
        let mut at = 0usize;
        while at + 2 < rbsp.len() {
            let mut kind = 0usize;
            while rbsp.get(at) == Some(&0xff) {
                kind += 255;
                at += 1;
            }
            kind += rbsp[at] as usize;
            at += 1;
            let mut size = 0usize;
            while rbsp.get(at) == Some(&0xff) {
                size += 255;
                at += 1;
            }
            if at >= rbsp.len() {
                break;
            }
            size += rbsp[at] as usize;
            at += 1;
            if at + size > rbsp.len() {
                break;
            }
            let payload = &rbsp[at..at + size];
            at += size;
            if kind == 5 && payload.starts_with(&UUID) {
                out.push(payload.to_vec());
            }
        }
    }
    out
}

/// The units of a low-overhead AV1 stream, found by hand.
fn av1_units_by_hand(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let header = bytes[at];
        let kind = header >> 3 & 0x0f;
        let extension = header & 0x04 != 0;
        let sized = header & 0x02 != 0;
        at += 1 + usize::from(extension);
        let mut size = 0u64;
        if sized {
            let mut shift = 0;
            loop {
                let Some(byte) = bytes.get(at) else {
                    return out;
                };
                at += 1;
                size |= u64::from(byte & 0x7f) << shift;
                shift += 7;
                if byte & 0x80 == 0 {
                    break;
                }
            }
        } else {
            size = (bytes.len() - at) as u64;
        }
        let end = at + size as usize;
        if end > bytes.len() {
            return out;
        }
        let payload = &bytes[at..end];
        at = end;
        // metadata_type 25 is one leb128 byte.
        if kind == 5 && payload.first() == Some(&25) && payload[1..].starts_with(&UUID) {
            let mut body = &payload[1..];
            while body.last() == Some(&0) {
                body = &body[..body.len() - 1];
            }
            if body.last() == Some(&0x80) {
                body = &body[..body.len() - 1];
            }
            out.push(body.to_vec());
        }
    }
    out
}

/// Every unit of a woven stream, by hand.
fn by_hand(stream: Stream, bytes: &[u8]) -> Vec<Vec<u8>> {
    match stream {
        Stream::H264 => units_by_hand(bytes, 1, 6),
        Stream::H265 => units_by_hand(bytes, 2, 39),
        Stream::Av1 => av1_units_by_hand(bytes),
    }
}

// ---------------------------------------------------------------- //
// ffmpeg, when it is here.
// ---------------------------------------------------------------- //

/// Whether ffmpeg and ffprobe are on the PATH.
fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Says a test is skipping, so a run without ffmpeg does not look like
/// a run that proved something.
fn skipping(what: &str) {
    println!("skipping {what}: ffmpeg is not on the PATH");
}

/// A directory of this test's own, emptied first.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ffrwd-index-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Runs ffmpeg, and fails the test with its own words if it refuses.
fn ffmpeg(args: &[&str]) -> String {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg runs");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "ffmpeg {} failed:\n{stderr}",
        args.join(" ")
    );
    stderr
}

/// The per-frame checksums of a file, which is what "the picture is
/// untouched" means in bytes.
fn framemd5(path: &Path) -> String {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-f", "framemd5", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(
        output.status.success(),
        "ffmpeg could not decode {}:\n{}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The presentation time of every packet, in decode order.
fn packet_times(path: &Path) -> Vec<i64> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "packet=pts_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    assert!(
        output.status.success(),
        "ffprobe refused {}",
        path.display()
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().trim_end_matches(',').parse::<f64>().ok())
        .map(|seconds| (seconds * 1000.0).round() as i64)
        .collect()
}

// ---------------------------------------------------------------- //
// The tests.
// ---------------------------------------------------------------- //

#[test]
fn the_fixtures_are_what_the_tests_assume() {
    for stream in Stream::every() {
        let bytes = stream.bytes();
        let spots = stream.carriers(&bytes);
        assert_eq!(spots.len(), 60, "{}: two seconds at 30fps", stream.name());
        let keyframes = spots.iter().filter(|spot| spot.keyframe).count();
        assert_eq!(
            keyframes,
            2,
            "{}: a keyframe every 30 frames",
            stream.name()
        );
        assert!(spots[0].keyframe, "{}: the first frame", stream.name());
        // The carriers tile the stream with nothing left over.
        assert_eq!(spots[0].start, 0);
        assert_eq!(spots.last().expect("a carrier").end, bytes.len());
        for pair in spots.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "{}", stream.name());
            assert!(pair[0].insert_at >= pair[0].start && pair[0].insert_at <= pair[0].end);
        }
        // Nothing of ours is in there yet, by either reader.
        assert!(by_hand(stream, &bytes).is_empty());
        assert!(stream.units(&bytes).iter().all(|units| units.is_empty()));
    }

    // x264 writes its own user_data_unregistered SEI, and it is not
    // ours: a reader must walk straight past it.
    let h264 = Stream::H264.bytes();
    let sei: Vec<avc::SeiMessage> = avc::scan_nals(&h264)
        .iter()
        .filter(|nal| Codec::H264.is_prefix_sei(nal.bytes))
        .flat_map(|nal| avc::parse_sei(nal.bytes, Codec::H264).expect("SEI messages"))
        .collect();
    assert!(
        sei.iter().any(|message| message.payload_type == 5),
        "the fixture has no x264 settings SEI"
    );
    assert!(
        sei.iter().any(|message| {
            message.payload_type == 5 && String::from_utf8_lossy(&message.payload).contains("x264")
        }),
        "the x264 SEI does not name x264"
    );
}

#[test]
fn records_woven_into_a_stream_read_back_byte_for_byte() {
    for stream in Stream::every() {
        for policy in [
            Placement::Keyframe,
            Placement::Next,
            Placement::Spread { budget_bytes: 96 },
        ] {
            let woven = weave(stream, policy);
            let read = woven.read();
            assert_eq!(
                read,
                woven.want,
                "{} with {policy:?} did not read back",
                stream.name()
            );
            // The second, independent reader finds the same units.
            let theirs = by_hand(stream, &woven.bytes);
            let ours: Vec<Vec<u8>> = match stream.codec() {
                Some(codec) => avc::units_annexb(&woven.bytes, codec),
                None => obu::units_obu(&woven.bytes),
            };
            assert_eq!(theirs, ours, "{} with {policy:?}", stream.name());
            assert!(!ours.is_empty());
            // Every unit stays under the soft limit of section 7.
            for unit in &ours {
                assert!(
                    unit.len() <= ffrwd_index_core::UNIT_SOFT_LIMIT,
                    "{}: a unit of {} bytes",
                    stream.name(),
                    unit.len()
                );
            }
        }
    }
}

#[test]
fn weaving_adds_nothing_but_the_units() {
    for stream in Stream::every() {
        let woven = weave(stream, Placement::Keyframe);
        match stream.codec() {
            Some(codec) => {
                let before: Vec<&[u8]> = avc::split_nals(&woven.original);
                let after: Vec<&[u8]> = avc::split_nals(&woven.bytes)
                    .into_iter()
                    .filter(|nal| avc::units_in_nal(nal, codec).is_empty())
                    .collect();
                assert_eq!(after, before, "{}: a NAL changed", stream.name());
            }
            None => {
                let before: Vec<Vec<u8>> = obu::scan_obus(&woven.original)
                    .expect("obus")
                    .iter()
                    .map(|obu| woven.original[obu.start..obu.end].to_vec())
                    .collect();
                let after: Vec<Vec<u8>> = obu::scan_obus(&woven.bytes)
                    .expect("obus")
                    .iter()
                    .filter(|unit| obu::unit_in_obu(unit).is_none())
                    .map(|unit| woven.bytes[unit.start..unit.end].to_vec())
                    .collect();
                assert_eq!(after, before, "{}: an OBU changed", stream.name());
            }
        }
        // And the carriers are still the carriers.
        let spots = stream.carriers(&woven.bytes);
        assert_eq!(spots.len(), 60, "{}", stream.name());
        assert_eq!(
            spots.iter().filter(|spot| spot.keyframe).count(),
            2,
            "{}",
            stream.name()
        );
    }
}

#[test]
fn the_picture_decodes_to_the_same_frames() {
    if !have_ffmpeg() {
        skipping("the framemd5 comparison");
        return;
    }
    for stream in Stream::every() {
        let dir = scratch(&format!("framemd5-{}", stream.name()));
        let woven = weave(stream, Placement::Spread { budget_bytes: 96 });
        let before = dir.join(format!("before.{}", stream.extension()));
        let after = dir.join(format!("after.{}", stream.extension()));
        std::fs::write(&before, &woven.original).expect("the original");
        std::fs::write(&after, &woven.bytes).expect("the woven stream");
        assert_eq!(
            framemd5(&before),
            framemd5(&after),
            "{}: the woven stream decodes to different frames",
            stream.name()
        );
    }
}

#[test]
fn ffmpeg_and_ffprobe_say_nothing_new_about_the_woven_stream() {
    if !have_ffmpeg() {
        skipping("the decoder's own opinion");
        return;
    }
    for stream in Stream::every() {
        let dir = scratch(&format!("quiet-{}", stream.name()));
        let woven = weave(stream, Placement::Keyframe);
        let before = dir.join(format!("before.{}", stream.extension()));
        let after = dir.join(format!("after.{}", stream.extension()));
        std::fs::write(&before, &woven.original).expect("the original");
        std::fs::write(&after, &woven.bytes).expect("the woven stream");

        let complaints = |path: &Path| -> Vec<String> {
            let output = Command::new("ffmpeg")
                .args(["-hide_banner", "-nostdin", "-v", "warning", "-i"])
                .arg(path)
                .args(["-f", "null", "-"])
                .output()
                .expect("ffmpeg runs");
            assert!(output.status.success(), "ffmpeg refused {}", path.display());
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .map(|line| line.replace(&path.display().to_string(), "<file>"))
                .filter(|line| !line.trim().is_empty())
                .collect()
        };
        assert_eq!(
            complaints(&after),
            complaints(&before),
            "{}: the decoder has something new to say",
            stream.name()
        );

        let probe = Command::new("ffprobe")
            .args(["-v", "warning", "-show_entries", "stream=codec_name"])
            .args(["-of", "csv=p=0"])
            .arg(&after)
            .output()
            .expect("ffprobe runs");
        assert!(probe.status.success(), "{}: ffprobe refused", stream.name());
        assert!(
            String::from_utf8_lossy(&probe.stderr).trim().is_empty(),
            "{}: ffprobe warned: {}",
            stream.name(),
            String::from_utf8_lossy(&probe.stderr)
        );
    }
}

#[test]
fn x264s_own_sei_is_still_there_and_untouched() {
    let woven = weave(Stream::H264, Placement::Keyframe);
    let theirs = |bytes: &[u8]| -> Vec<Vec<u8>> {
        avc::scan_nals(bytes)
            .iter()
            .filter(|nal| Codec::H264.is_prefix_sei(nal.bytes))
            .flat_map(|nal| avc::parse_sei(nal.bytes, Codec::H264).expect("SEI messages"))
            .filter(|message| !message.payload.starts_with(&UUID))
            .map(|message| message.payload)
            .collect()
    };
    let before = theirs(&woven.original);
    assert!(!before.is_empty(), "the fixture has no foreign SEI");
    assert_eq!(theirs(&woven.bytes), before, "a foreign SEI changed");
}

#[test]
fn remuxing_through_the_containers_keeps_the_records() {
    if !have_ffmpeg() {
        skipping("the remux round trips");
        return;
    }
    for stream in Stream::every() {
        let dir = scratch(&format!("remux-{}", stream.name()));
        let woven = weave(stream, Placement::Keyframe);
        let source = dir.join(format!("woven.{}", stream.extension()));
        std::fs::write(&source, &woven.bytes).expect("the woven stream");
        let want = woven.read();
        assert!(!want.is_empty());

        // An elementary stream carries no timestamps, and the Matroska
        // and MPEG-TS muxers will not take packets without them, so the
        // MP4 made first is what the other two are made from. That is
        // what a real pipeline does as well.
        let mp4 = dir.join("remux.mp4");
        ffmpeg(&[
            "-loglevel",
            "error",
            "-i",
            source.to_str().expect("a path"),
            "-c",
            "copy",
            "-f",
            "mp4",
            mp4.to_str().expect("a path"),
        ]);

        let mut containers = vec![("mp4", mp4.clone())];
        for (name, format) in [("mkv", "matroska"), ("ts", "mpegts")] {
            if stream == Stream::Av1 && name == "ts" {
                continue; // MPEG-TS has no place to put AV1 here.
            }
            let path = dir.join(format!("remux.{name}"));
            ffmpeg(&[
                "-loglevel",
                "error",
                "-i",
                mp4.to_str().expect("a path"),
                "-c",
                "copy",
                "-f",
                format,
                path.to_str().expect("a path"),
            ]);
            containers.push((name, path));
        }

        for (name, path) in containers {
            let back = dir.join(format!("back-{name}.{}", stream.extension()));
            let mut args = vec![
                "-loglevel",
                "error",
                "-i",
                path.to_str().expect("a path"),
                "-c",
                "copy",
            ];
            // MPEG-TS already carries start codes; MP4 and Matroska do
            // not, and the bitstream filter is what puts them back.
            if let (Some(filter), false) = (stream.to_annexb(), name == "ts") {
                args.extend(["-bsf:v", filter]);
            }
            args.extend(["-f", stream.format(), back.to_str().expect("a path")]);
            ffmpeg(&args);

            let bytes = std::fs::read(&back).expect("the stream back out");
            let carriers: Vec<i64> = (0..stream.carriers(&bytes).len())
                .map(|index| index as i64 * 1000 / FPS)
                .collect();
            assert_eq!(
                read_records(stream, &bytes, &carriers),
                want,
                "{} lost records through {name}",
                stream.name()
            );
        }
    }
}

#[test]
fn a_cut_at_a_keyframe_keeps_the_spans_of_what_survives() {
    if !have_ffmpeg() {
        skipping("the cut");
        return;
    }
    let stream = Stream::H264;
    let dir = scratch("cut");
    // The `next` policy spreads records over ordinary frames, so a cut
    // at the second keyframe leaves some behind. The `keyframe` policy
    // would put every record of this two second clip on or after that
    // same keyframe and the cut would drop none of them.
    let woven = weave(stream, Placement::Next);
    let source = dir.join("woven.h264");
    std::fs::write(&source, &woven.bytes).expect("the woven stream");
    let whole = dir.join("whole.mp4");
    ffmpeg(&[
        "-loglevel",
        "error",
        "-i",
        source.to_str().expect("a path"),
        "-c",
        "copy",
        "-f",
        "mp4",
        whole.to_str().expect("a path"),
    ]);
    // The second keyframe is one second in.
    let cut = dir.join("cut.mp4");
    ffmpeg(&[
        "-loglevel",
        "error",
        "-ss",
        "1",
        "-i",
        whole.to_str().expect("a path"),
        "-c",
        "copy",
        "-avoid_negative_ts",
        "make_zero",
        "-f",
        "mp4",
        cut.to_str().expect("a path"),
    ]);

    // Spans against the times the container really gives its frames,
    // which is what a player would compute.
    let spans = |path: &Path| -> Vec<(u16, i64, i64, i32, i32)> {
        let annexb = path.with_extension("h264");
        ffmpeg(&[
            "-loglevel",
            "error",
            "-i",
            path.to_str().expect("a path"),
            "-c",
            "copy",
            "-bsf:v",
            "h264_mp4toannexb",
            "-f",
            "h264",
            annexb.to_str().expect("a path"),
        ]);
        let bytes = std::fs::read(&annexb).expect("the stream back out");
        let times = packet_times(path);
        let mut out = Vec::new();
        for (index, units) in stream.units(&bytes).into_iter().enumerate() {
            let pts = times.get(index).copied().expect("a time for every carrier");
            for unit in units {
                for message in Unit::decode(&unit).expect("a unit").messages {
                    if let Message::Vector(VectorRecord {
                        record_id,
                        start_off,
                        end_off,
                        space_id,
                        ..
                    }) = message
                    {
                        if space_id != 1 {
                            continue;
                        }
                        out.push((
                            record_id,
                            pts + i64::from(start_off),
                            pts + i64::from(end_off),
                            start_off,
                            end_off,
                        ));
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    };

    let before = spans(&whole);
    let after = spans(&cut);
    assert!(!after.is_empty(), "the cut kept no records at all");
    assert!(
        after.len() < before.len(),
        "the cut kept every record, so it cut nothing"
    );

    // Every record that survived kept its offsets exactly, and its
    // absolute span moved by the one amount the cut moved everything.
    let mut shifts = Vec::new();
    for (id, start, end, start_off, end_off) in &after {
        let (_, was_start, was_end, was_start_off, was_end_off) = before
            .iter()
            .find(|(other, ..)| other == id)
            .unwrap_or_else(|| panic!("record {id} was not in the whole file"));
        assert_eq!(
            (start_off, end_off),
            (was_start_off, was_end_off),
            "record {id} had its offsets rewritten"
        );
        shifts.push((start - was_start, end - was_end));
    }
    let first = shifts[0];
    assert!(
        shifts.iter().all(|shift| *shift == first),
        "the surviving records moved by different amounts: {shifts:?}"
    );
    assert_eq!(first.0, first.1, "a span changed length");
    assert!(
        first.0 <= -900,
        "the cut was supposed to drop about a second, not {} ms",
        -first.0
    );
}

#[test]
fn trace_headers_sees_a_user_data_unregistered_sei_of_ours() {
    if !have_ffmpeg() {
        skipping("the trace_headers reading");
        return;
    }
    // A small stream of its own, so the payload stays under the 255
    // bytes that would make ffmpeg print the size in several bytes.
    let dir = scratch("trace");
    let original = Stream::H264.bytes();
    let mut space = Space::new(9, 16, Encoding::I8);
    space.model = "t:m".into();
    let planes = Planes::quantize(&vector(3, 16)).expect("quantized");
    let unit = Unit::new(vec![
        Message::Space(space),
        Message::Vector(VectorRecord {
            space_id: 9,
            record_id: 1,
            start_off: -500,
            end_off: 0,
            body: planes.encode(),
        }),
    ])
    .encode();
    assert!(unit.len() < 255, "the unit is {} bytes", unit.len());

    let spots = Stream::H264.carriers(&original);
    let mut units = vec![None; spots.len()];
    units[0] = Some(unit.clone());
    let woven = Stream::H264.splice(&original, &units);
    let path = dir.join("traced.h264");
    std::fs::write(&path, &woven).expect("the woven stream");

    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "trace", "-i"])
        .arg(&path)
        .args(["-c", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(output.status.success(), "ffmpeg refused the woven stream");
    let trace = String::from_utf8_lossy(&output.stderr);

    // ffmpeg's own reading of the SEI: the payload type, the size, and
    // the sixteen bytes of the UUID, in order.
    let values = |needle: &str| -> Vec<u8> {
        trace
            .lines()
            .filter(|line| line.contains(needle))
            .filter_map(|line| line.rsplit('=').next())
            .filter_map(|value| value.trim().parse::<u32>().ok())
            .map(|value| value as u8)
            .collect()
    };
    let types = values("last_payload_type_byte");
    assert!(
        types.contains(&5),
        "no user_data_unregistered SEI in the trace"
    );
    let uuids = values("uuid_iso_iec_11578[");
    assert!(
        uuids.windows(16).any(|window| window == UUID),
        "the trace does not show this format's UUID"
    );
    // The SEI whose UUID is ours is the one whose size is our unit's.
    let sizes = values("last_payload_size_byte");
    assert!(
        sizes.contains(&(unit.len() as u8)),
        "no SEI of {} bytes in the trace: sizes were {sizes:?}",
        unit.len()
    );
}

#[test]
fn the_file_index_says_what_the_stream_says() {
    for stream in Stream::every() {
        let woven = weave(stream, Placement::Spread { budget_bytes: 96 });
        // What a reader would build while walking the file.
        let mut pairs: Vec<(u32, Message)> = Vec::new();
        for (index, units) in stream.units(&woven.bytes).into_iter().enumerate() {
            let time = (index as i64 * 1000 / FPS) as u32;
            for unit in units {
                for message in Unit::decode(&unit).expect("a unit").messages {
                    // Slices are put back together before they reach an
                    // index; this policy makes none of them.
                    assert!(!matches!(message, Message::Fragment(_)));
                    pairs.push((time, message));
                }
            }
        }
        let index = FileIndex::build(pairs);
        let bytes = index.encode();
        assert_eq!(
            FileIndex::parse(&bytes).expect("the index reads back"),
            index
        );
        assert_eq!(index.spaces().count(), 2, "{}", stream.name());
        assert_eq!(index.records().count(), 9, "{}", stream.name());

        // The index's records are the stream's records, span for span.
        let mut from_index: Vec<Want> = index
            .records()
            .map(|(time, record)| Want {
                space_id: record.space_id,
                record_id: record.record_id,
                start_ms: i64::from(time) + i64::from(record.start_off),
                end_ms: i64::from(time) + i64::from(record.end_off),
                body: record.body.clone(),
            })
            .collect();
        from_index.sort_by_key(|want| (want.space_id, want.record_id));
        assert_eq!(from_index, woven.want, "{}", stream.name());
    }
}
