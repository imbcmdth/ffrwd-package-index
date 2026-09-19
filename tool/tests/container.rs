//! The tool over real containers, end to end.
//!
//! Everything here starts from an elementary stream this tool wove
//! records into, muxes it with `-c copy` so the pictures and the
//! records are the encoder's own bytes, and then asks two questions of
//! the result: does the container path find the same records the
//! elementary stream path does, and does putting the index in the file
//! leave the file playing exactly as it did.
//!
//! Every test skips itself with a message when ffmpeg is not on the
//! PATH, and nothing is committed: the fixtures are made when the tests
//! run, once for the whole binary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const TOOL: &str = env!("CARGO_BIN_EXE_ffrwd-index");

// ---------------------------------------------------------------- //
// Running things.
// ---------------------------------------------------------------- //

fn tool(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(TOOL)
        .args(args)
        .output()
        .expect("the tool runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// The tool, with a refusal turned into a panic naming what it said.
fn ok(args: &[&str]) -> (String, String) {
    let (code, out, told) = tool(args);
    assert_eq!(code, 0, "ffrwd-index {}: {told}", args.join(" "));
    (out, told)
}

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

/// What ffmpeg says about a file at the level a warning shows up at,
/// which is what "no new warnings" is measured with.
fn ffprobe_complaints(path: &Path) -> String {
    let output = Command::new("ffprobe")
        .args(["-hide_banner", "-loglevel", "warning"])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    assert!(
        output.status.success(),
        "ffprobe refused {}",
        path.display()
    );
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

/// Every decoded frame's md5, which is the picture and nothing else.
fn framemd5(path: &Path) -> String {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-f", "framemd5", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(
        output.status.success(),
        "framemd5 of {} failed:\n{}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn text(path: &Path) -> String {
    path.to_str().expect("a path").to_string()
}

// ---------------------------------------------------------------- //
// Rows.
// ---------------------------------------------------------------- //

/// One field of a row, matched by brackets so an array comes back
/// whole.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("\"{name}\":");
    let at = line.find(&needle)? + needle.len();
    let rest = &line[at..];
    let open = rest.chars().next()?;
    if open != '[' && open != '{' {
        let end = rest.find([',', '}'])?;
        return Some(&rest[..end]);
    }
    let close = if open == '[' { ']' } else { '}' };
    let mut depth = 0i32;
    for (index, letter) in rest.char_indices() {
        if letter == open {
            depth += 1;
        } else if letter == close {
            depth -= 1;
            if depth == 0 {
                return Some(&rest[..index + 1]);
            }
        }
    }
    None
}

fn number(line: &str, name: &str) -> i64 {
    field(line, name)
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or_else(|| panic!("no {name} in {line}"))
        .round() as i64
}

/// One record as everything about it that does not depend on which
/// clock the reader was on: who it is, what arrived, and how far its
/// span sits from its carrier. Those last two are the numbers the wire
/// actually carries.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    space_id: String,
    record_id: String,
    planes: String,
    escapes: String,
    vector: String,
    start_off: i64,
    end_off: i64,
}

fn rows_of(printed: &str) -> Vec<Row> {
    printed
        .lines()
        .filter(|line| line.contains("\"vector\":"))
        .map(|line| {
            let carrier = number(line, "carrier_ms");
            Row {
                space_id: field(line, "space_id").expect("a space").into(),
                record_id: field(line, "record_id").expect("an id").into(),
                planes: field(line, "planes").unwrap_or("").into(),
                escapes: field(line, "escapes").unwrap_or("").into(),
                vector: field(line, "vector").expect("a vector").into(),
                start_off: number(line, "start_ms") - carrier,
                end_off: number(line, "end_ms") - carrier,
            }
        })
        .collect()
}

/// The absolute spans a read found, in milliseconds.
fn spans_of(printed: &str) -> Vec<(i64, i64)> {
    printed
        .lines()
        .filter(|line| line.contains("\"vector\":"))
        .map(|line| (number(line, "start_ms"), number(line, "end_ms")))
        .collect()
}

fn spaces_of(printed: &str) -> Vec<&str> {
    printed
        .lines()
        .filter(|line| line.contains("\"space\":"))
        .collect()
}

// ---------------------------------------------------------------- //
// The fixtures.
// ---------------------------------------------------------------- //

/// The rows woven into every fixture: one layered space of sixteen
/// components, six records a third of a second apart.
fn vectors() -> String {
    let mut out = String::from(
        r#"{"space":{"id":1,"dims":16,"encoding":"i8","unit_length":true,"modality":"picture","model":"test:layered","query":"test:layered-text","producer":"the container test"}}"#,
    );
    out.push('\n');
    for index in 0..6 {
        let start = index * 300;
        let values: Vec<String> = (0..16)
            .map(|k| format!("{:.4}", ((index * 7 + k) as f32 * 0.31).sin()))
            .collect();
        out.push_str(&format!(
            r#"{{"space_id":1,"start_ms":{start},"end_ms":{},"vector":[{}]}}"#,
            start + 250,
            values.join(",")
        ));
        out.push('\n');
    }
    out
}

/// Which codec a fixture is, and what ffmpeg calls its pieces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    H264,
    H265,
    Av1,
}

impl Codec {
    fn every() -> [Codec; 3] {
        [Codec::H264, Codec::H265, Codec::Av1]
    }

    fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "av1",
        }
    }

    /// The extension the elementary stream takes, which is also how the
    /// tool works out the codec.
    fn extension(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "obu",
        }
    }

    fn format(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "hevc",
            Codec::Av1 => "obu",
        }
    }

    /// What unpacks the codec out of a container back into the
    /// elementary stream the tool reads. AV1 needs none: its samples
    /// are the same OBUs either way.
    fn to_annexb(self) -> Option<&'static str> {
        match self {
            Codec::H264 => Some("h264_mp4toannexb"),
            Codec::H265 => Some("hevc_mp4toannexb"),
            Codec::Av1 => None,
        }
    }
}

fn dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        if !have_ffmpeg() {
            return None;
        }
        let dir = std::env::temp_dir().join("ffrwd-index-tool-containers");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        std::fs::write(dir.join("rows.ndjson"), vectors()).expect("the rows");
        encode(&dir);
        Some(dir)
    })
    .as_ref()
}

fn at(name: &str) -> PathBuf {
    dir().expect("the fixtures").join(name)
}

macro_rules! skip_without_ffmpeg {
    () => {
        if dir().is_none() {
            println!("skipping: ffmpeg is not on the PATH");
            return;
        }
    };
}

/// Three two-second encodes, as elementary streams, plus ten seconds of
/// 640x360 with noise over it so that a bound on what a keyframe scan
/// reads is a claim about a file with real pictures in it.
fn encode(dir: &Path) {
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=30",
        "-t",
        "3",
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
        "-f",
        "h264",
        &text(&dir.join("plain.h264")),
    ]);
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x180:rate=30",
        "-t",
        "3",
        "-c:v",
        "libx265",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-x265-params",
        "keyint=30:min-keyint=30:bframes=2:log-level=error",
        "-f",
        "hevc",
        &text(&dir.join("plain.h265")),
    ]);
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x176:rate=30",
        "-t",
        "3",
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
        "-f",
        "obu",
        &text(&dir.join("plain.obu")),
    ]);
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
        "-f",
        "h264",
        &text(&dir.join("big.h264")),
    ]);
}

/// Weaves the fixture rows into one of the elementary streams.
fn weave(codec: Codec, placement: &str, name: &str) -> PathBuf {
    let out = at(&format!("{name}.{}", codec.extension()));
    ok(&[
        "weave",
        "--video",
        &text(&at(&format!("plain.{}", codec.extension()))),
        "--vectors",
        &text(&at("rows.ndjson")),
        "--out",
        &text(&out),
        "--placement",
        placement,
    ]);
    out
}

/// The woven stream in a container, copied rather than re-encoded.
fn mux(stream: &Path, out: &Path, extra: &[&str]) {
    let mut args = vec!["-i", stream.to_str().expect("a path"), "-c", "copy"];
    args.extend_from_slice(extra);
    args.push(out.to_str().expect("a path"));
    ffmpeg(&args);
}

/// A container's video back out as the elementary stream the tool's
/// `--video` path reads.
fn demux(codec: Codec, from: &Path, out: &Path) {
    let mut args = vec!["-i", from.to_str().expect("a path"), "-c", "copy"];
    if let Some(bsf) = codec.to_annexb() {
        args.extend_from_slice(&["-bsf:v", bsf]);
    }
    args.extend_from_slice(&["-f", codec.format()]);
    args.push(out.to_str().expect("a path"));
    ffmpeg(&args);
}

// ---------------------------------------------------------------- //
// The reads agree.
// ---------------------------------------------------------------- //

#[test]
fn a_container_read_finds_what_the_elementary_stream_read_finds() {
    skip_without_ffmpeg!();
    for codec in Codec::every() {
        for (placement, scan) in [
            ("keyframe", "keyframes"),
            ("next", "all"),
            ("spread:120", "all"),
        ] {
            let tag = format!("{}-{}", codec.name(), placement.replace(':', "-"));
            let woven = weave(codec, placement, &tag);
            let mp4 = at(&format!("{tag}.mp4"));
            let mkv = at(&format!("{tag}.mkv"));
            mux(&woven, &mp4, &[]);
            mux(&mp4, &mkv, &[]);

            // The elementary stream the container itself holds, so that
            // the comparison is between two readings of the same bytes
            // and not between a file and its ancestor.
            let back = at(&format!("{tag}.back.{}", codec.extension()));
            demux(codec, &mp4, &back);

            let (from_stream, _) = ok(&["read", "--video", &text(&back)]);
            let (from_mp4, told) = ok(&["read", "--mp4", &text(&mp4), "--scan", scan]);
            let (from_mkv, _) = ok(&["read", "--mkv", &text(&mkv), "--scan", scan]);

            assert!(told.contains(&format!("scan {scan}")), "{tag}: {told}");
            assert_eq!(
                spaces_of(&from_mp4),
                spaces_of(&from_stream),
                "{tag}: the spaces differ"
            );
            let wanted = rows_of(&from_stream);
            assert_eq!(wanted.len(), 6, "{tag}: the stream lost records");
            assert_eq!(rows_of(&from_mp4), wanted, "{tag}: the MP4 read differs");
            assert_eq!(
                rows_of(&from_mkv),
                wanted,
                "{tag}: the Matroska read differs"
            );

            // The two containers hold the same pictures the same
            // distance apart, so the spans have to agree to the
            // millisecond Matroska keeps them in. What they need not
            // agree on is where zero is: this MP4 was muxed from an
            // elementary stream, which carries no timestamps, so ffmpeg
            // gave it a nominal frame rate and an edit list that starts
            // it a tenth of a second before zero, and then moved that
            // zero again on the way into Matroska, which has no
            // negative timestamps to move it to. Both readers agree
            // with ffprobe about their own file, which is what
            // `container/tests/containers.rs` pins; what is asserted
            // here is that the same record describes the same moment of
            // the same picture in both.
            let (mp4_spans, mkv_spans) = (spans_of(&from_mp4), spans_of(&from_mkv));
            let (mp4_zero, mkv_zero) = (mp4_spans[0].0, mkv_spans[0].0);
            for (index, (mp4_span, mkv_span)) in mp4_spans.iter().zip(&mkv_spans).enumerate() {
                let mine = (mp4_span.0 - mp4_zero, mp4_span.1 - mp4_zero);
                let theirs = (mkv_span.0 - mkv_zero, mkv_span.1 - mkv_zero);
                assert!(
                    (mine.0 - theirs.0).abs() <= 1 && (mine.1 - theirs.1).abs() <= 1,
                    "{tag}: record {index} spans {mp4_span:?} in MP4 and {mkv_span:?} in Matroska"
                );
            }
        }
    }
}

#[test]
fn a_spread_record_over_a_reordering_stream_still_comes_back_whole() {
    skip_without_ffmpeg!();
    // The hardest case for a record doled out over several carriers.
    // Each of its messages names the span from its own carrier, and an
    // AV1 stream muxed into MP4 does not keep the frame rate the
    // elementary stream was woven against: SVT-AV1 codes several frames
    // in one temporal unit and shows them later with
    // `show_existing_frame`, so sample times in decode order are not a
    // frame apart. Section 4 says the span is the first message's and
    // the others need not agree, so every plane still lands in the
    // record and nothing is dropped.
    let woven = weave(Codec::Av1, "spread:120", "av1-spread-whole");
    let mp4 = at("av1-spread-whole.mp4");
    mux(&woven, &mp4, &[]);
    let (printed, told) = ok(&["read", "--mp4", &text(&mp4), "--scan", "all"]);
    let rows = rows_of(&printed);
    assert_eq!(rows.len(), 6, "a record went missing");
    assert!(
        !told.contains("messages were dropped"),
        "the reader threw something away:\n{told}"
    );
    for row in &rows {
        assert_eq!(
            row.planes, "[0,1,2,3,4,5,6,7]",
            "a record came back coarse: {row:?}"
        );
    }
}

#[test]
fn a_keyframe_scan_reads_the_last_sample_and_finds_the_record_on_it() {
    skip_without_ffmpeg!();
    // Section 7: a record whose span ends after the last keyframe has
    // no keyframe to ride, so it rides the last access unit, which is
    // not a sync sample. The fast path reads that sample as well, and
    // the extra record is what proves it did.
    let rows = at("past-the-last-keyframe.ndjson");
    let mut text_rows = vectors();
    // The fixtures are three seconds at a keyframe a second, so a span
    // ending at 2.9 has no keyframe at or after it.
    text_rows.push_str(
        r#"{"space_id":1,"start_ms":2650,"end_ms":2900,"vector":[0.5,-0.5,0.25,-0.25,0.125,-0.125,1,-1,0.5,-0.5,0.25,-0.25,0.125,-0.125,1,-1]}"#,
    );
    text_rows.push('\n');
    std::fs::write(&rows, text_rows).expect("the rows");

    let woven = at("past.h264");
    ok(&[
        "weave",
        "--video",
        &text(&at("plain.h264")),
        "--vectors",
        &text(&rows),
        "--out",
        &text(&woven),
        "--placement",
        "keyframe",
    ]);
    let mp4 = at("past.mp4");
    let mkv = at("past.mkv");
    mux(&woven, &mp4, &[]);
    mux(&mp4, &mkv, &[]);

    for (name, flag, path) in [("mp4", "--mp4", &mp4), ("mkv", "--mkv", &mkv)] {
        let (whole, _) = ok(&["read", flag, &text(path), "--scan", "all"]);
        let (fast, told) = ok(&["read", flag, &text(path), "--scan", "keyframes"]);
        assert_eq!(rows_of(&whole).len(), 7, "{name}");
        assert_eq!(
            rows_of(&fast),
            rows_of(&whole),
            "{name}: the fast path missed the record on the last access unit"
        );
        // And the saving is still a saving: the fast path is the sync
        // samples and one more, not the whole track.
        let visited: usize = told
            .split("scan keyframes: ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("{name}: no sample count in {told}"));
        assert_eq!(visited, 4, "{name}: three keyframes and the last sample");
    }
}

#[test]
fn a_span_read_from_a_container_is_the_span_that_was_written() {
    skip_without_ffmpeg!();
    // A stream with no B-frames presents its pictures in the order it
    // decodes them, so the frame rate the elementary stream was woven
    // at and the presentation clock the container carries are the same
    // clock, and the spans that come back are the spans that went in.
    let plain = at("nobf.h264");
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
        "0",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "h264",
        &text(&plain),
    ]);
    let woven = at("nobf.woven.h264");
    ok(&[
        "weave",
        "--video",
        &text(&plain),
        "--vectors",
        &text(&at("rows.ndjson")),
        "--out",
        &text(&woven),
        "--placement",
        "next",
    ]);
    let mp4 = at("nobf.mp4");
    let mkv = at("nobf.mkv");
    mux(&woven, &mp4, &[]);
    mux(&mp4, &mkv, &[]);

    let wanted: Vec<(i64, i64)> = (0..6).map(|i| (i * 300, i * 300 + 250)).collect();
    for (name, flag, path) in [("mp4", "--mp4", &mp4), ("mkv", "--mkv", &mkv)] {
        let (printed, _) = ok(&["read", flag, &text(path), "--scan", "all"]);
        for (index, (got, want)) in spans_of(&printed).iter().zip(&wanted).enumerate() {
            assert!(
                (got.0 - want.0).abs() <= 1 && (got.1 - want.1).abs() <= 1,
                "{name}: record {index} came back as {got:?} and went in as {want:?}"
            );
        }
    }
}

#[test]
fn a_keyframe_scan_finds_every_record_and_reads_almost_none_of_the_file() {
    skip_without_ffmpeg!();
    let woven = at("big.woven.h264");
    ok(&[
        "weave",
        "--video",
        &text(&at("big.h264")),
        "--vectors",
        &text(&at("rows.ndjson")),
        "--out",
        &text(&woven),
        "--placement",
        "keyframe",
    ]);
    let mp4 = at("big.mp4");
    let mkv = at("big.mkv");
    mux(&woven, &mp4, &[]);
    mux(&mp4, &mkv, &[]);

    for (name, flag, path) in [("mp4", "--mp4", &mp4), ("mkv", "--mkv", &mkv)] {
        let (whole, _) = ok(&["read", flag, &text(path), "--scan", "all"]);
        let (fast, told) = ok(&["read", flag, &text(path), "--scan", "keyframes"]);
        assert_eq!(
            rows_of(&fast),
            rows_of(&whole),
            "{name}: the keyframe scan missed a record the keyframe placement wrote"
        );
        assert_eq!(rows_of(&fast).len(), 6, "{name}");

        let share = told
            .split('(')
            .nth(1)
            .and_then(|rest| rest.split('%').next())
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("{name}: no share in {told}"));
        let size = std::fs::metadata(path).expect("a fixture").len();
        assert!(
            size > 2_000_000,
            "{name}: the fixture is too small to mean anything"
        );
        assert!(
            share < 10.0,
            "{name}: a keyframe scan read {share}% of {size} bytes\n{told}"
        );
    }
}

// ---------------------------------------------------------------- //
// The index in the file.
// ---------------------------------------------------------------- //

#[test]
fn an_mp4_index_is_appended_and_read_back_and_changes_nothing_else() {
    skip_without_ffmpeg!();
    let woven = weave(Codec::H264, "keyframe", "idx");
    for (tag, extra) in [("plain", vec![]), ("fast", vec!["-movflags", "+faststart"])] {
        let mp4 = at(&format!("idx-{tag}.mp4"));
        mux(&woven, &mp4, &extra);

        let before = framemd5(&mp4);
        let complaints = ffprobe_complaints(&mp4);
        let size = std::fs::metadata(&mp4).expect("the file").len();
        let (from_scan, _) = ok(&["read", "--mp4", &text(&mp4), "--scan", "all"]);

        let (_, told) = ok(&["index", &text(&mp4)]);
        assert!(told.contains("index box was appended"), "{tag}: {told}");
        assert!(
            std::fs::metadata(&mp4).expect("the file").len() > size,
            "{tag}: the file did not grow"
        );

        // The pictures are the same pictures, and ffprobe has nothing
        // new to say about the file.
        assert_eq!(framemd5(&mp4), before, "{tag}: the pictures changed");
        assert_eq!(
            ffprobe_complaints(&mp4),
            complaints,
            "{tag}: ffprobe found something new to complain about"
        );

        // The index says what the scan said.
        let (from_index, told) = ok(&["read", "--index", &text(&mp4)]);
        assert!(told.contains("came out of"), "{tag}: {told}");
        assert!(
            told.contains("a cut or a join"),
            "{tag}: the tool did not say the index is taken at its word:\n{told}"
        );
        assert_eq!(
            index_spans(&from_index),
            spans_of(&from_scan),
            "{tag}: the index and the scan disagree"
        );
        assert_eq!(
            index_vectors(&from_index),
            from_scan
                .lines()
                .filter(|line| line.contains("\"vector\":"))
                .map(|line| field(line, "vector").expect("a vector").to_string())
                .collect::<Vec<_>>(),
            "{tag}"
        );

        // Writing it again lands on top rather than adding a second.
        let after_one = std::fs::metadata(&mp4).expect("the file").len();
        let (_, told) = ok(&["index", &text(&mp4)]);
        assert!(told.contains("written over"), "{tag}: {told}");
        assert_eq!(
            std::fs::metadata(&mp4).expect("the file").len(),
            after_one,
            "{tag}: the file grew a second index"
        );
    }
}

/// The spans an `read --index` printed, which carries a `time_ms` the
/// stream rows do not.
fn index_spans(printed: &str) -> Vec<(i64, i64)> {
    printed
        .lines()
        .filter(|line| line.contains("\"vector\":"))
        .map(|line| (number(line, "start_ms"), number(line, "end_ms")))
        .collect()
}

fn index_vectors(printed: &str) -> Vec<String> {
    printed
        .lines()
        .filter(|line| line.contains("\"vector\":"))
        .map(|line| field(line, "vector").expect("a vector").to_string())
        .collect()
}

#[test]
fn an_index_keeps_a_carrier_that_sits_before_the_files_zero() {
    skip_without_ffmpeg!();
    // ffmpeg gives an MP4 muxed from an elementary stream an edit list
    // whose media time lands after the first few samples, so ffprobe
    // prints a negative `pts_time` for them. Section 8's time is signed
    // so an index can say the same; clamping it to zero would put the
    // entry on a picture it did not come off.
    let declaration = vectors()
        .lines()
        .next()
        .expect("the space declaration")
        .to_string();
    // One record available before the first picture, so `next` puts it
    // on the very first access unit, which is the one before zero.
    let early = r#"{"space_id":1,"start_ms":0,"end_ms":0,"available_ms":0,"vector":[1,-1,0.5,-0.5,0.25,-0.25,0.125,-0.125,1,-1,0.5,-0.5,0.25,-0.25,0.125,-0.125]}"#;
    let rows = at("early.ndjson");
    std::fs::write(&rows, format!("{declaration}\n{early}\n")).expect("the rows");

    let woven = at("early.h264");
    ok(&[
        "weave",
        "--video",
        &text(&at("plain.h264")),
        "--vectors",
        &text(&rows),
        "--out",
        &text(&woven),
        "--placement",
        "next",
    ]);
    let mp4 = at("early.mp4");
    mux(&woven, &mp4, &[]);

    // The first packet really is before zero, by ffprobe's own reading.
    let first = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
            "-read_intervals",
            "%+#1",
        ])
        .arg(&mp4)
        .output()
        .expect("ffprobe runs");
    let first: f64 = String::from_utf8_lossy(&first.stdout)
        .lines()
        .next()
        .and_then(|line| line.trim().trim_end_matches(',').parse().ok())
        .expect("a first packet time");
    assert!(first < 0.0, "the fixture does not start before zero");

    let (from_scan, _) = ok(&["read", "--mp4", &text(&mp4), "--scan", "all"]);
    assert!(
        from_scan.contains("\"carrier_ms\":-"),
        "the record did not ride a carrier before zero:\n{from_scan}"
    );
    ok(&["index", &text(&mp4)]);
    let (from_index, _) = ok(&["read", "--index", &text(&mp4)]);
    assert!(
        from_index
            .lines()
            .any(|line| line.contains("\"time_ms\":-")),
        "the index clamped a negative carrier:\n{from_index}"
    );
    assert_eq!(
        index_spans(&from_index),
        spans_of(&from_scan),
        "the index and the scan disagree either side of zero"
    );
}

#[test]
fn a_remux_drops_the_index_box_and_keeps_the_records_in_the_stream() {
    skip_without_ffmpeg!();
    let woven = weave(Codec::H264, "keyframe", "remux");
    let mp4 = at("remux.mp4");
    mux(&woven, &mp4, &[]);
    ok(&["index", &text(&mp4)]);
    let (before, _) = ok(&["read", "--mp4", &text(&mp4), "--scan", "all"]);

    let again = at("remux-again.mp4");
    mux(&mp4, &again, &[]);
    // The box is gone, which is expected: it is a top-level box no
    // muxer knows, and the index is rebuildable from the stream.
    let (code, _, told) = tool(&["read", "--index", &text(&again)]);
    assert_eq!(code, 2, "the box survived a remux");
    assert!(told.contains("carries no index"), "{told}");
    // The records themselves rode inside the pictures' own access
    // units, and they are still there.
    let (after, _) = ok(&["read", "--mp4", &text(&again), "--scan", "all"]);
    assert_eq!(rows_of(&after), rows_of(&before));

    // And building it again is one command.
    ok(&["index", &text(&again)]);
    let (rebuilt, _) = ok(&["read", "--index", &text(&again)]);
    assert_eq!(index_vectors(&rebuilt).len(), 6);
}

#[test]
fn a_fragmented_file_takes_its_index_in_front_of_the_mfra() {
    skip_without_ffmpeg!();
    let woven = weave(Codec::H264, "keyframe", "frag");
    let frag = at("frag.mp4");
    mux(&woven, &frag, &["-movflags", "frag_keyframe+empty_moov"]);
    let before = framemd5(&frag);
    let complaints = ffprobe_complaints(&frag);
    let bytes = std::fs::read(&frag).expect("the file");
    let has_mfra = bytes.len() > 16 && bytes[bytes.len() - 12..bytes.len() - 8] == *b"mfro";

    let (_, told) = ok(&["index", &text(&frag)]);
    if has_mfra {
        assert!(told.contains("mfra"), "{told}");
        let after = std::fs::read(&frag).expect("the file");
        assert_eq!(
            &after[after.len() - 12..after.len() - 8],
            b"mfro",
            "the mfra is no longer last, so its size is no longer the file's last four bytes"
        );
    }
    assert_eq!(framemd5(&frag), before, "the pictures changed");
    assert_eq!(ffprobe_complaints(&frag), complaints);
    let (printed, _) = ok(&["read", "--index", &text(&frag)]);
    assert_eq!(index_vectors(&printed).len(), 6);

    // Seeking inside it still works, which is what the mfra is for.
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-ss",
            "1",
            "-i",
        ])
        .arg(&frag)
        .args(["-c", "copy", "-f", "null", "-"])
        .output()
        .expect("ffmpeg runs");
    assert!(
        output.status.success(),
        "a seek into the fragmented file failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_matroska_index_is_attached_by_ffmpeg_and_read_back_natively() {
    skip_without_ffmpeg!();
    let woven = weave(Codec::H264, "keyframe", "att");
    let mp4 = at("att.mp4");
    let mkv = at("att.mkv");
    let out = at("att-indexed.mkv");
    mux(&woven, &mp4, &[]);
    mux(&mp4, &mkv, &[]);

    let (from_scan, _) = ok(&["read", "--mkv", &text(&mkv), "--scan", "all"]);
    let (_, told) = ok(&["index", &text(&mkv), "--out", &text(&out)]);
    assert!(told.contains("attached by ffmpeg"), "{told}");
    assert_eq!(framemd5(&out), framemd5(&mkv), "the pictures changed");

    let (from_index, _) = ok(&["read", "--index", &text(&out)]);
    assert_eq!(index_spans(&from_index), spans_of(&from_scan));
    assert_eq!(index_vectors(&from_index).len(), 6);

    // The attachment is the one section 8 names, by both its names.
    let bytes = std::fs::read(&out).expect("the file");
    let holds = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
    assert!(
        holds(b"application/x-ffrwd-index"),
        "the MIME type is not there"
    );
    assert!(holds(b"ffrwd-index.bin"), "the file name is not there");

    // And the records in the stream are untouched beside it.
    let (after, _) = ok(&["read", "--mkv", &text(&out), "--scan", "all"]);
    assert_eq!(rows_of(&after), rows_of(&from_scan));
}

#[test]
fn a_cut_of_an_indexed_file_still_reads_from_its_first_frame() {
    skip_without_ffmpeg!();
    let woven = weave(Codec::H264, "next", "cut");
    let whole = at("cut-whole.mp4");
    mux(&woven, &whole, &[]);
    ok(&["index", &text(&whole)]);

    let cut = at("cut.mp4");
    ffmpeg(&[
        "-ss",
        "1",
        "-i",
        &text(&whole),
        "-c",
        "copy",
        "-avoid_negative_ts",
        "make_zero",
        &text(&cut),
    ]);
    let (printed, told) = ok(&["read", "--mp4", &text(&cut), "--scan", "all"]);
    assert!(!told.contains("dropped"), "{told}");
    assert_eq!(
        spaces_of(&printed).len(),
        1,
        "the cut lost the space declaration:\n{printed}"
    );
    let kept = rows_of(&printed);
    assert!(!kept.is_empty(), "the cut kept no records");
    assert!(kept.len() < 6, "the cut kept everything, so it cut nothing");
    // The cut did not carry the index box with it, and the records it
    // kept build a new one.
    let (code, _, _) = tool(&["read", "--index", &text(&cut)]);
    assert_eq!(code, 2, "a cut carried the old index along");
    ok(&["index", &text(&cut)]);
    let (rebuilt, _) = ok(&["read", "--index", &text(&cut)]);
    assert_eq!(index_vectors(&rebuilt).len(), kept.len());
}

// ---------------------------------------------------------------- //
// Refusals.
// ---------------------------------------------------------------- //

#[test]
fn the_tool_says_what_it_will_not_do_with_a_container() {
    skip_without_ffmpeg!();
    let woven = weave(Codec::H264, "keyframe", "refuse");
    let mp4 = at("refuse.mp4");
    let mkv = at("refuse.mkv");
    mux(&woven, &mp4, &[]);
    mux(&mp4, &mkv, &[]);
    let plain = text(&at("plain.h264"));

    let cases: Vec<(Vec<String>, &str)> = vec![
        (
            vec![
                "read".into(),
                "--mp4".into(),
                text(&mp4),
                "--scan".into(),
                "some".into(),
            ],
            "not keyframes or all",
        ),
        (
            vec![
                "read".into(),
                "--mp4".into(),
                text(&mp4),
                "--mkv".into(),
                text(&mkv),
            ],
            "one of --video",
        ),
        (
            vec!["read".into(), "--mp4".into(), plain.clone()],
            "neither an EBML document nor an ISO box",
        ),
        (
            vec!["read".into(), "--mkv".into(), text(&mp4)],
            "is an ISO base media file, not a Matroska file",
        ),
        (vec!["index".into()], "wants a file"),
        (vec!["index".into(), text(&mkv)], "--out is needed"),
        (
            vec![
                "index".into(),
                text(&mp4),
                "--out".into(),
                text(&at("no.mp4")),
            ],
            "--out belongs to Matroska",
        ),
        (
            vec![
                "index".into(),
                text(&mkv),
                "--out".into(),
                text(&at("no.mkv")),
                "--rewrite".into(),
            ],
            "--rewrite belongs to MP4",
        ),
        (
            vec!["index".into(), plain.clone()],
            "neither an EBML document nor an ISO box",
        ),
        (
            vec!["read".into(), "--index".into(), text(&at("plain.obu"))],
            "neither an EBML document nor an ISO box",
        ),
    ];
    for (args, wanted) in cases {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, _, told) = tool(&borrowed);
        assert_eq!(code, 2, "{args:?} did not refuse");
        assert!(told.contains(wanted), "{args:?} said {told}");
    }
}

#[test]
fn an_index_box_that_is_not_last_is_refused_until_a_rewrite_is_asked_for() {
    skip_without_ffmpeg!();
    let woven = weave(Codec::H264, "keyframe", "rewrite");
    let mp4 = at("rewrite.mp4");
    mux(&woven, &mp4, &[]);
    ok(&["index", &text(&mp4)]);

    // A remux with the box still in the file puts it where a muxer
    // would, which is nowhere: ffmpeg drops it. So the awkward case is
    // made by hand, by appending a `free` box after ours.
    let mut bytes = std::fs::read(&mp4).expect("the file");
    bytes.extend_from_slice(&8u32.to_be_bytes());
    bytes.extend_from_slice(b"free");
    let buried = at("rewrite-buried.mp4");
    std::fs::write(&buried, &bytes).expect("the file");

    let (code, _, told) = tool(&["index", &text(&buried)]);
    assert_eq!(code, 2);
    assert!(told.contains("--rewrite"), "{told}");
    assert_eq!(
        std::fs::read(&buried).expect("the file"),
        bytes,
        "the refusal still changed the file"
    );

    let (_, told) = ok(&["index", &text(&buried), "--rewrite"]);
    assert!(told.contains("copied without its old index"), "{told}");
    let (printed, _) = ok(&["read", "--index", &text(&buried)]);
    assert_eq!(index_vectors(&printed).len(), 6);
    // And there is one of them, not two. Counting the UUID alone would
    // count the SEI messages in the stream as well, which carry it too,
    // so what is counted is a `uuid` box header with it after.
    let after = std::fs::read(&buried).expect("the file");
    let mut header = b"uuid".to_vec();
    header.extend_from_slice(&ffrwd_index_core::UUID);
    assert_eq!(
        after
            .windows(header.len())
            .filter(|window| *window == header)
            .count(),
        1,
        "the rewrite left the old box in"
    );
}
