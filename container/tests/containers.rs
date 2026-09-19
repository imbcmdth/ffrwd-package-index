//! The parsers against files real ffmpeg wrote.
//!
//! Nothing is committed: every fixture here is made by ffmpeg when the
//! tests run, once for the whole binary, and every test that needs one
//! skips itself with a message when ffmpeg is not on the PATH. What is
//! asserted about a file is asked of ffprobe rather than assumed, so a
//! change in what ffmpeg writes shows up as a disagreement rather than
//! as a test that still passes over a file nobody has.
//!
//! The fixtures, and what each is here to prove:
//!
//! | file | what it is for |
//! | --- | --- |
//! | `h264.mp4` | the ordinary case: one edit list, B-frames, `ctts` |
//! | `h264-raw.mp4` | a `media_time` that lands between two samples |
//! | `h264-neg.mp4` | `+negative_cts_offsets`: a version 1 `ctts` |
//! | `h264-fast.mp4` | `+faststart`: the `moov` in front of the `mdat` |
//! | `h264-frag.mp4` | `frag_keyframe+empty_moov`: `moof`, `trun`, `mfra` |
//! | `h264-cut.mp4` | `-ss 1 -c copy`: a file that begins mid-stream |
//! | `hevc.mp4`, `av1.mp4` | the other two codecs, and `hvcC`/`av1C` |
//! | `h264.mkv`, `hevc.mkv`, `av1.webm` | the same in Matroska |
//! | `live.mkv` | `-live 1`: a segment with no declared length |
//! | `big.mp4`, `big.mkv` | ten seconds of 640x360 with noise in it, so
//!   that "a keyframe scan reads under a tenth of the file" is a claim
//!   about a file with real pictures in it |

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use ffrwd_index_container::scan::{carriages, Scan};
use ffrwd_index_container::{kind_of, mkv, mp4, write, Kind, Sample, Source, TrackCodec};

// ---------------------------------------------------------------- //
// ffmpeg.
// ---------------------------------------------------------------- //

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn ffmpeg(args: &[&str]) {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        .args(args)
        .output()
        .expect("ffmpeg runs");
    assert!(
        output.status.success(),
        "ffmpeg {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn ffprobe(args: &[&str]) -> String {
    let output = Command::new("ffprobe")
        .args(["-hide_banner", "-v", "error"])
        .args(args)
        .output()
        .expect("ffprobe runs");
    assert!(
        output.status.success(),
        "ffprobe {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Every packet's presentation time, in milliseconds, in decode order.
fn ffprobe_times(path: &Path) -> Vec<f64> {
    ffprobe(&[
        "-select_streams",
        "v:0",
        "-show_entries",
        "packet=pts_time",
        "-of",
        "csv=p=0",
        path.to_str().expect("a path"),
    ])
    .lines()
    .filter_map(|line| line.trim().trim_end_matches(',').parse::<f64>().ok())
    .map(|seconds| seconds * 1000.0)
    .collect()
}

// ---------------------------------------------------------------- //
// The fixtures.
// ---------------------------------------------------------------- //

/// Where the fixtures live, made once for the whole test binary.
fn fixtures() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        if !have_ffmpeg() {
            return None;
        }
        let dir = std::env::temp_dir().join("ffrwd-index-container-fixtures");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        build(&dir);
        Some(dir)
    })
    .as_ref()
}

fn at(name: &str) -> PathBuf {
    fixtures().expect("the fixtures").join(name)
}

/// The path as ffmpeg wants it, which on Windows is still forward
/// slashes as far as anything here cares.
fn text(path: &Path) -> String {
    path.to_str().expect("a path").to_string()
}

fn build(dir: &Path) {
    let h264 = text(&dir.join("h264.mp4"));
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=30",
        "-t",
        "2",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-g",
        "30",
        "-bf",
        "2",
        "-pix_fmt",
        "yuv420p",
        &h264,
    ]);
    let raw = text(&dir.join("h264.h264"));
    ffmpeg(&[
        "-i",
        &h264,
        "-c",
        "copy",
        "-bsf:v",
        "h264_mp4toannexb",
        "-f",
        "h264",
        &raw,
    ]);

    // A media time that lands between two samples: the raw stream has
    // no timestamps, so ffmpeg gives it a nominal rate the sample
    // durations themselves do not quite keep to.
    ffmpeg(&["-i", &raw, "-c", "copy", &text(&dir.join("h264-raw.mp4"))]);
    ffmpeg(&[
        "-i",
        &h264,
        "-c",
        "copy",
        "-movflags",
        "+negative_cts_offsets",
        &text(&dir.join("h264-neg.mp4")),
    ]);
    ffmpeg(&[
        "-i",
        &h264,
        "-c",
        "copy",
        "-movflags",
        "+faststart",
        &text(&dir.join("h264-fast.mp4")),
    ]);
    ffmpeg(&[
        "-i",
        &h264,
        "-c",
        "copy",
        "-movflags",
        "frag_keyframe+empty_moov",
        &text(&dir.join("h264-frag.mp4")),
    ]);
    ffmpeg(&[
        "-ss",
        "1",
        "-i",
        &h264,
        "-c",
        "copy",
        &text(&dir.join("h264-cut.mp4")),
    ]);
    ffmpeg(&["-i", &h264, "-c", "copy", &text(&dir.join("h264.mkv"))]);
    ffmpeg(&[
        "-i",
        &h264,
        "-c",
        "copy",
        "-f",
        "matroska",
        "-live",
        "1",
        &text(&dir.join("live.mkv")),
    ]);

    let hevc = text(&dir.join("hevc.mp4"));
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=30",
        "-t",
        "2",
        "-c:v",
        "libx265",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-x265-params",
        "keyint=30:min-keyint=30:bframes=2:log-level=error",
        &hevc,
    ]);
    ffmpeg(&["-i", &hevc, "-c", "copy", &text(&dir.join("hevc.mkv"))]);

    let av1 = text(&dir.join("av1.mp4"));
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x176:rate=30",
        "-t",
        "2",
        "-c:v",
        "libsvtav1",
        "-preset",
        "10",
        "-crf",
        "40",
        "-g",
        "30",
        "-pix_fmt",
        "yuv420p",
        &av1,
    ]);
    ffmpeg(&[
        "-i",
        &av1,
        "-c",
        "copy",
        "-f",
        "webm",
        &text(&dir.join("av1.webm")),
    ]);

    // Ten seconds of 640x360 with noise over it, so the pictures are
    // the size a real file's are and a bound on what a keyframe scan
    // reads means something.
    let big = text(&dir.join("big.mp4"));
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=640x360:rate=30",
        "-t",
        "10",
        "-vf",
        "noise=alls=40:allf=t+u",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-g",
        "30",
        "-bf",
        "2",
        "-pix_fmt",
        "yuv420p",
        &big,
    ]);
    ffmpeg(&["-i", &big, "-c", "copy", &text(&dir.join("big.mkv"))]);
}

/// The whole file in memory, which is what the parsers are handed here:
/// they take `Read + Seek` and a `Cursor` is one.
fn source(name: &str) -> Source<Cursor<Vec<u8>>> {
    let bytes = std::fs::read(at(name)).expect("a fixture");
    Source::new(Cursor::new(bytes)).expect("a source")
}

fn track_of(name: &str, scan: Scan) -> ffrwd_index_container::VideoTrack {
    let mut src = source(name);
    match kind_of(&mut src).expect("a container") {
        Kind::Mp4 => mp4::read(&mut src).expect("an MP4 track"),
        Kind::Matroska => mkv::read(&mut src, scan).expect("a Matroska track"),
    }
}

macro_rules! skip_without_ffmpeg {
    () => {
        if fixtures().is_none() {
            println!("skipping: ffmpeg is not on the PATH");
            return;
        }
    };
}

// ---------------------------------------------------------------- //
// Timing.
// ---------------------------------------------------------------- //

#[test]
fn every_sample_is_shown_when_ffprobe_says_it_is() {
    skip_without_ffmpeg!();
    for name in [
        "h264.mp4",
        "h264-raw.mp4",
        "h264-neg.mp4",
        "h264-fast.mp4",
        "h264-frag.mp4",
        "h264-cut.mp4",
        "hevc.mp4",
        "av1.mp4",
        "h264.mkv",
        "hevc.mkv",
        "av1.webm",
        "live.mkv",
    ] {
        let track = track_of(name, Scan::All);
        let wanted = ffprobe_times(&at(name));
        assert_eq!(
            track.samples.len(),
            wanted.len(),
            "{name}: a different number of samples than ffprobe found"
        );
        for (sample, want) in track.samples.iter().zip(&wanted) {
            let got = track.ms(sample.pts) as f64;
            assert!(
                (got - want).abs() <= 1.0,
                "{name}: sample {} is at {got} and ffprobe says {want}",
                sample.index
            );
        }
    }
}

#[test]
fn the_files_that_do_not_start_at_zero_say_so() {
    skip_without_ffmpeg!();
    // A fragmented file written with an empty moov has no edit list, so
    // its first picture is shown two frame times in, and ffprobe agrees.
    let frag = track_of("h264-frag.mp4", Scan::All);
    let first = frag.ms(frag.samples[0].pts);
    assert_eq!(first, 67, "the fragmented file starts at 0.067");
    assert_eq!(frag.start_shift, 0, "and it has no edit list to shift it");

    // The same content with a moov has one, and starts at zero.
    let whole = track_of("h264.mp4", Scan::All);
    assert_eq!(whole.ms(whole.samples[0].pts), 0);
    assert!(whole.start_shift < 0, "the edit list moved it back");

    // A raw stream remuxed gets a media time between two samples, and
    // the samples in front of it keep their negative times rather than
    // disappearing.
    let raw = track_of("h264-raw.mp4", Scan::All);
    assert!(
        raw.ms(raw.samples[0].pts) < 0,
        "the first sample is before the file's own zero"
    );
    assert!(
        raw.samples.iter().any(|sample| raw.ms(sample.pts) == 0),
        "and one sample lands exactly on it"
    );
}

// ---------------------------------------------------------------- //
// The sample map.
// ---------------------------------------------------------------- //

#[test]
fn the_codec_and_its_framing_come_out_of_the_file() {
    skip_without_ffmpeg!();
    for (name, codec, length_size) in [
        ("h264.mp4", TrackCodec::H264, Some(4)),
        ("hevc.mp4", TrackCodec::H265, Some(4)),
        ("av1.mp4", TrackCodec::Av1, None),
        ("h264.mkv", TrackCodec::H264, Some(4)),
        ("hevc.mkv", TrackCodec::H265, Some(4)),
        ("av1.webm", TrackCodec::Av1, None),
    ] {
        let track = track_of(name, Scan::Keyframes);
        assert_eq!(track.codec, codec, "{name}");
        assert_eq!(track.length_size, length_size, "{name}");
        assert!(
            !track.codec_private.is_empty(),
            "{name}: no out-of-band header"
        );
    }
}

#[test]
fn the_keyframes_are_the_ones_ffprobe_flags() {
    skip_without_ffmpeg!();
    for name in [
        "h264.mp4",
        "h264-frag.mp4",
        "av1.mp4",
        "h264.mkv",
        "av1.webm",
    ] {
        let flags = ffprobe(&[
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=flags",
            "-of",
            "csv=p=0",
            &text(&at(name)),
        ]);
        let wanted: Vec<bool> = flags
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.starts_with('K'))
            .collect();
        let track = track_of(name, Scan::All);
        let got: Vec<bool> = track.samples.iter().map(|sample| sample.keyframe).collect();
        assert_eq!(got, wanted, "{name}");
    }
}

#[test]
fn a_matroska_keyframe_scan_finds_what_a_full_walk_would() {
    skip_without_ffmpeg!();
    for name in ["h264.mkv", "hevc.mkv", "av1.webm", "live.mkv", "big.mkv"] {
        let all = track_of(name, Scan::All);
        // Section 7's fast path: the sync samples and nothing else,
        // because every record of a `keyframe` file is on one.
        let wanted: Vec<i64> = all
            .samples
            .iter()
            .filter(|sample| sample.keyframe)
            .map(|sample| sample.pts)
            .collect();
        let fast = track_of(name, Scan::Keyframes);
        let found: Vec<i64> = fast
            .scanned(Scan::Keyframes)
            .iter()
            .map(|sample| sample.pts)
            .collect();
        assert_eq!(found, wanted, "{name}: the cues and the walk disagree");
        // And they really are the same blocks, not ones that happen to
        // be shown at the same time.
        let keys: Vec<&Sample> = all.samples.iter().filter(|s| s.keyframe).collect();
        for (taken, want) in fast.scanned(Scan::Keyframes).iter().zip(keys) {
            assert_eq!(taken.offset, want.offset, "{name}");
            assert_eq!(taken.size, want.size, "{name}");
        }
    }
}

// ---------------------------------------------------------------- //
// What a scan costs.
// ---------------------------------------------------------------- //

#[test]
fn a_keyframe_scan_reads_a_small_part_of_a_real_file() {
    skip_without_ffmpeg!();
    for name in ["big.mp4", "big.mkv"] {
        let mut src = source(name);
        let scan = Scan::Keyframes;
        let track = match kind_of(&mut src).expect("a container") {
            Kind::Mp4 => mp4::read(&mut src).expect("a track"),
            Kind::Matroska => mkv::read(&mut src, scan).expect("a track"),
        };
        let found = carriages(&mut src, &track, scan).expect("a scan");
        let tally = src.tally();
        assert!(
            tally.len > 2_000_000,
            "{name}: the fixture is too small to mean anything"
        );
        assert!(!found.is_empty(), "{name}: no sync samples");
        assert!(
            tally.share() < 10.0,
            "{name}: a keyframe scan read {:.2}% of the file ({} of {} bytes)",
            tally.share(),
            tally.bytes_read,
            tally.len
        );

        // And the full scan really is the expensive one, so the ratio
        // above is a saving rather than an accident of the fixture.
        let mut src = source(name);
        let track = match kind_of(&mut src).expect("a container") {
            Kind::Mp4 => mp4::read(&mut src).expect("a track"),
            Kind::Matroska => mkv::read(&mut src, Scan::All).expect("a track"),
        };
        let every = carriages(&mut src, &track, Scan::All).expect("a scan");
        assert!(every.len() > found.len() * 4, "{name}");
        assert!(src.tally().bytes_read > tally.bytes_read * 4, "{name}");
    }
}

// ---------------------------------------------------------------- //
// The index box.
// ---------------------------------------------------------------- //

#[test]
fn an_index_goes_into_every_shape_of_mp4_and_comes_back_out() {
    skip_without_ffmpeg!();
    let index = ffrwd_index_core::index::FileIndex {
        version: 1,
        entries: Vec::new(),
    }
    .encode();
    for name in ["h264.mp4", "h264-fast.mp4", "h264-frag.mp4", "av1.mp4"] {
        let before = std::fs::read(at(name)).expect("a fixture");
        let mut file = Cursor::new(before.clone());
        let placed = write::install(&mut file, &index).expect("a write");
        let after = file.into_inner();
        assert!(after.len() > before.len(), "{name}");

        let mut src = Source::new(Cursor::new(after.clone())).expect("a source");
        assert_eq!(
            mp4::read_index(&mut src)
                .expect("a read")
                .expect("an index"),
            index,
            "{name}"
        );
        // One read near the end of the file is what finding it cost.
        assert!(
            src.tally().bytes_read < 8192,
            "{name}: finding the index read {} bytes",
            src.tally().bytes_read
        );

        // Everything the file already had is still where it was, apart
        // from a fragmented file's mfra, which moves by the box's own
        // length so that it stays last.
        match placed {
            write::Placed::Appended { at } => {
                assert_eq!(&after[..at as usize], &before[..], "{name}")
            }
            write::Placed::BeforeMfra { at, moved } => {
                assert_eq!(&after[..at as usize], &before[..at as usize], "{name}");
                assert_eq!(
                    &after[after.len() - moved as usize..],
                    &before[before.len() - moved as usize..],
                    "{name}: the mfra did not end up last"
                );
            }
            other => panic!("{name}: {other:?}"),
        }

        // And the samples still read exactly as they did.
        let mut before_src = Source::new(Cursor::new(before)).expect("a source");
        let mut after_src = Source::new(Cursor::new(after)).expect("a source");
        assert_eq!(
            mp4::read(&mut before_src).expect("a track"),
            mp4::read(&mut after_src).expect("a track"),
            "{name}"
        );
    }
}

// ---------------------------------------------------------------- //
// Bad bytes.
// ---------------------------------------------------------------- //

/// Every entry point, over whatever bytes it is given.
fn every_parser(bytes: Vec<u8>) {
    let mut src = Source::new(Cursor::new(bytes)).expect("a source");
    let _ = kind_of(&mut src);
    let _ = mp4::top_level(&mut src);
    let _ = mp4::find_index_box(&mut src);
    let _ = mp4::read_index(&mut src);
    let _ = mp4::spot(&mut src);
    let _ = mkv::segment(&mut src);
    let _ = mkv::read_index(&mut src);
    for scan in [Scan::Keyframes, Scan::All] {
        if let Ok(track) = mp4::read(&mut src) {
            let _ = carriages(&mut src, &track, scan);
        }
        if let Ok(track) = mkv::read(&mut src, scan) {
            let _ = carriages(&mut src, &track, scan);
        }
    }
}

#[test]
fn every_truncation_of_a_real_file_is_refused_rather_than_survived() {
    skip_without_ffmpeg!();
    for name in ["h264.mp4", "h264-frag.mp4", "h264.mkv"] {
        let bytes = std::fs::read(at(name)).expect("a fixture");
        // Every truncation, thinned out past the first kilobyte: the
        // interesting cuts are inside the headers and the tables, and
        // a cut in the middle of a picture is the same cut a thousand
        // times over.
        let cuts: Vec<usize> = (0..bytes.len().min(2048))
            .chain((2048..bytes.len()).step_by(97))
            .chain(bytes.len().saturating_sub(4096)..bytes.len())
            .collect();
        for cut in cuts {
            every_parser(bytes[..cut].to_vec());
        }
    }
}

#[test]
fn random_damage_to_a_real_file_is_refused_rather_than_survived() {
    skip_without_ffmpeg!();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        seed >> 11
    };
    for name in ["h264.mp4", "h264-frag.mp4", "h264.mkv", "av1.webm"] {
        let bytes = std::fs::read(at(name)).expect("a fixture");
        for _ in 0..200 {
            let mut damaged = bytes.clone();
            // Flips land in the first few kilobytes as often as
            // anywhere, because that is where the structure is.
            for _ in 0..1 + next() % 8 {
                let at = if next() % 2 == 0 {
                    (next() as usize) % damaged.len().min(4096)
                } else {
                    (next() as usize) % damaged.len()
                };
                damaged[at] ^= 1 << (next() % 8);
            }
            every_parser(damaged);
        }
    }
}

#[test]
fn a_file_of_nothing_much_is_refused_rather_than_survived() {
    for bytes in [
        Vec::new(),
        vec![0u8; 8],
        vec![0xffu8; 64],
        b"ftyp".to_vec(),
        b"\x1a\x45\xdf\xa3".to_vec(),
        // A box claiming the whole of a 64-bit address space.
        [1u8, 0, 0, 0]
            .iter()
            .chain(b"moov")
            .chain(&[0xffu8; 8])
            .copied()
            .collect(),
    ] {
        every_parser(bytes);
    }
}
