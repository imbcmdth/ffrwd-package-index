//! The `weave` module, through the real sidecar and real ffmpeg.
//!
//! Every test here encodes with ffmpeg, pipes the coded packets through
//! `ffrwd-wasm` hosting `weave.wasm`, muxes the result with `-c copy`,
//! and then asks three questions of what came out: do the pictures
//! decode to the same frames, are the packets the packets that went in,
//! and do the records read back where they were put. Nothing is
//! simulated: the interface under test is a host's, and a mock of it
//! would only prove that the mock agrees with itself.
//!
//! The node needs a host, and the host is `ffrwd-wasm` from ffrwd 0.29.
//! `FFRWD_WASM` names that binary and every test here skips without it,
//! loudly. `FFRWD_INDEX_WASM` names a prebuilt `weave.wasm`; without it
//! the module is built once. The rows reach the module the way a query
//! hands them over: a NUT of JSON messages per space, each message at
//! the time its row says it existed, on the input named for the space.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use ffrwd_index_core::carriage;
use ffrwd_index_core::message::{Message, Space, Unit, VectorBody};
use ffrwd_index_core::quant::Planes;
use ffrwd_nal::{h26x, obu, Codec};
use ffrwd_nut::{Muxer, Packet, Stream, TimeBase};

/// Names the sidecar of an ffrwd that hosts nodes.
const SIDECAR_ENV: &str = "FFRWD_WASM";
/// Names a `weave.wasm` already built, instead of building one.
const MODULE_ENV: &str = "FFRWD_INDEX_WASM";

/// Four seconds of testsrc2 at 25 fps, with a keyframe every second.
const FPS: u32 = 25;
const SECONDS: u32 = 4;
const FRAMES: usize = (FPS * SECONDS) as usize;

// ------------------------------------------------------------------ //
// The harness.
// ------------------------------------------------------------------ //

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tool/ has a parent")
        .to_path_buf()
}

/// The sidecar to drive, or nothing, having said why.
fn sidecar() -> Option<PathBuf> {
    let Some(named) = std::env::var_os(SIDECAR_ENV) else {
        eprintln!(
            "SKIPPED: {SIDECAR_ENV} does not name an ffrwd-wasm binary. A node needs a host \
             that speaks ffrwd:av@0.19.0, so point {SIDECAR_ENV} at the ffrwd-wasm of ffrwd \
             0.29 or later."
        );
        return None;
    };
    let path = PathBuf::from(named);
    if !path.is_file() {
        panic!(
            "{SIDECAR_ENV} names {}, which is not a file",
            path.display()
        );
    }
    Some(path)
}

/// `weave.wasm`, built once per test binary.
///
/// The build goes to a target directory of its own: this test runs
/// under a `cargo test` that holds the workspace's build lock, and a
/// second cargo in the same directory would wait for it forever.
fn module() -> PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            if let Some(named) = std::env::var_os(MODULE_ENV) {
                let path = PathBuf::from(named);
                assert!(path.is_file(), "{MODULE_ENV} names no file");
                return path;
            }
            let target = workspace().join("target/wasm-module");
            let output = Command::new(std::env::var("CARGO").unwrap_or("cargo".into()))
                .args([
                    "build",
                    "--release",
                    "--target",
                    "wasm32-wasip2",
                    "-p",
                    "weave",
                ])
                .current_dir(workspace())
                .env("CARGO_TARGET_DIR", &target)
                .output()
                .expect("spawn cargo to build weave.wasm");
            assert!(
                output.status.success(),
                "building weave.wasm failed. Set {MODULE_ENV} to a prebuilt module.\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            target.join("wasm32-wasip2/release/weave.wasm")
        })
        .clone()
}

/// A directory of this test's own, emptied first so a rerun starts
/// clean and left behind so a failure can be looked at.
fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("ffrwd_index_weave_{name}"));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a scratch directory");
    path
}

fn ffmpeg(args: &[&str]) -> String {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        output.status.success(),
        "ffmpeg {args:?} exited with {:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn ffprobe(args: &[&str]) -> String {
    let output = Command::new("ffprobe")
        .args(["-hide_banner", "-v", "error"])
        .args(args)
        .output()
        .expect("spawn ffprobe");
    assert!(
        output.status.success(),
        "ffprobe {args:?} exited with {:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn text(path: &Path) -> String {
    path.to_str().expect("a UTF-8 path").to_string()
}

/// One encode of testsrc2, with B-frames where the codec has them, as a
/// coded NUT: the shape an encoding ffmpeg hands the node.
fn encode(dir: &Path, codec: &str) -> PathBuf {
    let out = dir.join(format!("{codec}.nut"));
    let source = format!("testsrc2=size=320x240:rate={FPS}:duration={SECONDS}");
    let mut args: Vec<String> = vec![
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        source,
        "-pix_fmt".into(),
        "yuv420p".into(),
    ];
    args.extend(
        match codec {
            "h264" => vec![
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "25",
                "-bf",
                "2",
            ],
            "hevc" => vec![
                "-c:v",
                "libx265",
                "-preset",
                "ultrafast",
                "-x265-params",
                "keyint=25:bframes=3:log-level=none",
            ],
            "av1" => vec!["-c:v", "libsvtav1", "-preset", "10", "-g", "25"],
            other => panic!("{other} is not a codec these tests encode"),
        }
        .into_iter()
        .map(String::from),
    );
    args.extend(["-f".into(), "nut".into(), text(&out)]);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    ffmpeg(&borrowed);
    out
}

/// The same packets reframed the way an `avcC` describes them: through
/// MP4 and back out, which is what puts a configuration record in the
/// stream header and a length before every NAL.
fn as_length_prefixed(dir: &Path, nut: &Path) -> PathBuf {
    let mp4 = dir.join("reframe.mp4");
    let out = dir.join("length-prefixed.nut");
    ffmpeg(&["-f", "nut", "-i", &text(nut), "-c", "copy", &text(&mp4)]);
    ffmpeg(&["-i", &text(&mp4), "-c", "copy", "-f", "nut", &text(&out)]);
    out
}

/// Whether a stream's out-of-band header is a configuration record
/// rather than Annex B, which is what the module reads to decide the
/// framing, so a test forcing one should check it got one.
fn length_prefixed(nut: &Path) -> bool {
    let shown = ffprobe(&[
        "-show_streams",
        "-show_data",
        "-of",
        "json",
        "-i",
        &text(nut),
    ]);
    // ffprobe prints extradata as a hex dump; the first byte of the
    // first line is 01 for avcC and hvcC, 00 for a start code.
    shown
        .lines()
        .find(|line| line.contains("00000000: "))
        .is_some_and(|line| {
            line.split("00000000: ")
                .nth(1)
                .is_some_and(|rest| rest.starts_with("01"))
        })
}

struct Run {
    rows: Vec<String>,
    out: PathBuf,
}

impl Run {
    /// Every row of an event, as parsed JSON members.
    fn events(&self, event: &str) -> Vec<BTreeMap<String, String>> {
        self.rows
            .iter()
            .map(|row| members(row))
            .filter(|row| row.get("event").map(String::as_str) == Some(event))
            .collect()
    }

    fn summary(&self) -> BTreeMap<String, String> {
        let summary = self.events("summary");
        assert_eq!(summary.len(), 1, "one summary row, not {}", summary.len());
        assert_eq!(
            members(self.rows.last().expect("a row")).get("event"),
            Some(&"summary".to_string()),
            "the summary is the last row"
        );
        summary.into_iter().next().expect("a summary")
    }
}

/// One JSON object as a flat map of member name to the member's text.
/// The rows here are flat, and a test that compared numbers would have
/// to care whether `0` printed as `0` or `0.0`.
fn members(row: &str) -> BTreeMap<String, String> {
    let value = json(row);
    match value {
        Json::Object(members) => members
            .into_iter()
            .map(|(name, value)| (name, value.text()))
            .collect(),
        _ => panic!("a row that is not an object: {row}"),
    }
}

/// Just enough JSON to read the module's own rows back. The rows crate
/// has a reader, but a test that used the code under test to check the
/// code under test would pass whatever it wrote.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Object(Vec<(String, Json)>),
    Array(Vec<Json>),
    Text(String),
    Number(f64),
    Other(String),
}

impl Json {
    fn text(&self) -> String {
        match self {
            Json::Text(value) => value.clone(),
            Json::Number(value) => format!("{value}"),
            Json::Other(value) => value.clone(),
            Json::Array(values) => values
                .iter()
                .map(Json::text)
                .collect::<Vec<String>>()
                .join(","),
            Json::Object(_) => "{}".into(),
        }
    }

    fn number(&self) -> f64 {
        match self {
            Json::Number(value) => *value,
            other => panic!("{other:?} is not a number"),
        }
    }
}

fn json(text: &str) -> Json {
    let bytes: Vec<char> = text.chars().collect();
    let mut at = 0usize;
    parse(&bytes, &mut at)
}

fn parse(bytes: &[char], at: &mut usize) -> Json {
    while bytes.get(*at).is_some_and(|c| c.is_whitespace()) {
        *at += 1;
    }
    match bytes.get(*at) {
        Some('{') => {
            *at += 1;
            let mut members = Vec::new();
            loop {
                while bytes
                    .get(*at)
                    .is_some_and(|c| c.is_whitespace() || *c == ',')
                {
                    *at += 1;
                }
                if bytes.get(*at) == Some(&'}') {
                    *at += 1;
                    return Json::Object(members);
                }
                let Json::Text(name) = parse(bytes, at) else {
                    panic!("a member name that is not a string");
                };
                while bytes
                    .get(*at)
                    .is_some_and(|c| c.is_whitespace() || *c == ':')
                {
                    *at += 1;
                }
                members.push((name, parse(bytes, at)));
            }
        }
        Some('[') => {
            *at += 1;
            let mut values = Vec::new();
            loop {
                while bytes
                    .get(*at)
                    .is_some_and(|c| c.is_whitespace() || *c == ',')
                {
                    *at += 1;
                }
                if bytes.get(*at) == Some(&']') {
                    *at += 1;
                    return Json::Array(values);
                }
                values.push(parse(bytes, at));
            }
        }
        Some('"') => {
            *at += 1;
            let mut out = String::new();
            while let Some(ch) = bytes.get(*at) {
                *at += 1;
                match ch {
                    '"' => return Json::Text(out),
                    '\\' => {
                        let escape = bytes.get(*at).copied().unwrap_or('?');
                        *at += 1;
                        out.push(match escape {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                    }
                    other => out.push(*other),
                }
            }
            panic!("a string that does not end");
        }
        _ => {
            let start = *at;
            while bytes
                .get(*at)
                .is_some_and(|c| !matches!(c, ',' | '}' | ']') && !c.is_whitespace())
            {
                *at += 1;
            }
            let word: String = bytes[start..*at].iter().collect();
            match word.parse::<f64>() {
                Ok(value) => Json::Number(value),
                Err(_) => Json::Other(word),
            }
        }
    }
}

/// What standard input carries into one run, since only one thing can.
enum Feed<'a> {
    /// Nothing; the packets come off a file and the rows off a path.
    None,
    /// The rows, written once the run has started: a live writer has a
    /// vector when the span it describes is already behind the stream.
    Rows(&'a [u8]),
    /// The packets, written a piece at a time: an encoder feeding a
    /// filter does not hand over a whole file at once, and a rows file
    /// then reaches the module while the packets are still moving.
    Packets(&'a [u8]),
}

/// How params reach the module: always `-params-from`, since a spaces
/// list is JSON with quotes and colons in it, which a filtergraph's
/// options would have to escape.
enum Params<'a> {
    /// Written by the harness, from the test.
    Inline(&'a str),
    /// In a file the test wrote itself.
    File(&'a str),
}

/// The first space a params object names, which is the input a rows
/// file with no space of its own is bound to.
fn first_space(params: &str) -> String {
    let unescaped = params.replace("\\\"", "\"");
    let at = unescaped.find("\"name\"").expect("params naming a space");
    let rest = &unescaped[at + "\"name\"".len()..];
    let open = rest.find('"').expect("a name");
    let close = rest[open + 1..].find('"').expect("the end of a name");
    rest[open + 1..open + 1 + close].to_string()
}

/// One run of the sidecar hosting `weave`, with a rows file bound to the
/// first space the params declare.
fn weave(dir: &Path, input: &Path, params: &str, rows: &Path) -> Run {
    run_weave(
        dir,
        &text(input),
        Params::Inline(params),
        &[(first_space(params), text(rows))],
        Feed::None,
    )
}

/// The same, with the params read out of a file this writes.
fn weave_with_params_file(dir: &Path, input: &Path, params: &str, rows: &Path) -> Run {
    let path = dir.join("params.json");
    std::fs::write(&path, params).expect("write the params");
    run_weave(
        dir,
        &text(input),
        Params::File(&text(&path)),
        &[(first_space(params), text(rows))],
        Feed::None,
    )
}

/// The same, with the packets arriving a piece at a time on standard
/// input rather than off a file all at once.
fn weave_streaming(dir: &Path, input: &Path, params: &str, rows: &Path) -> Run {
    let bytes = std::fs::read(input).expect("read the packets to feed");
    run_weave(
        dir,
        "-",
        Params::Inline(params),
        &[(first_space(params), text(rows))],
        Feed::Packets(&bytes),
    )
}

/// The same, with each rows file on the input of the space it names,
/// which is the shape a query compiles to.
fn weave_arguments(dir: &Path, input: &Path, params: &str, rows: &[(&str, &Path)]) -> Run {
    let named: Vec<(String, String)> = rows
        .iter()
        .map(|(name, path)| (name.to_string(), text(path)))
        .collect();
    run_weave(
        dir,
        &text(input),
        Params::Inline(params),
        &named,
        Feed::None,
    )
}

/// Rows as a NUT of JSON messages, which is how a producer's rows reach
/// a node: each row at the time it existed, `available_t` where it says
/// one and `start_t` otherwise, in microseconds. A line that is not a
/// row goes at the time of the one before it.
fn rows_nut(rows: &str) -> Vec<u8> {
    let micros = TimeBase {
        num: 1,
        den: 1_000_000,
    };
    let mut out = Vec::new();
    {
        let mut muxer = Muxer::new(&mut out, &Stream::json(micros)).expect("a NUT of rows");
        let mut last = 0i64;
        for line in rows.lines().filter(|line| !line.trim().is_empty()) {
            let stamped = ["\"available_t\":", "\"start_t\":"].iter().find_map(|key| {
                let at = line.find(key)? + key.len();
                let number: String = line[at..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E' | '+'))
                    .collect();
                number
                    .parse::<f64>()
                    .ok()
                    .map(|seconds| (seconds * 1e6).round() as i64)
            });
            let pts = stamped.unwrap_or(last).max(last);
            last = pts;
            let packet = Packet {
                pts,
                dts: Some(pts),
                keyframe: true,
            };
            muxer
                .write_coded(&packet, line.as_bytes())
                .expect("a row written");
        }
        muxer.finish().expect("the NUT finished");
    }
    out
}

/// `rows_in` names a port and a rows file each, or `-` for the rows
/// standard input carries.
fn run_weave(
    dir: &Path,
    input: &str,
    params: Params<'_>,
    rows_in: &[(String, String)],
    feed: Feed<'_>,
) -> Run {
    let out = dir.join("woven.nut");
    let params_path = match params {
        Params::Inline(written) => {
            let path = dir.join("inline-params.json");
            std::fs::write(&path, written).expect("write the params");
            text(&path)
        }
        Params::File(path) => path.to_string(),
    };
    let packets_in = if input == "-" { "pipe:0" } else { input };
    let mut args: Vec<String> = vec!["-f".into(), "nut".into(), "-i".into(), packets_in.into()];
    let mut pads = String::from("[v=0:v]");
    for (n, (port, path)) in rows_in.iter().enumerate() {
        let source = if path == "-" {
            "pipe:0".to_string()
        } else {
            let nut = dir.join(format!("rows-{n}.nut"));
            let rows = std::fs::read_to_string(path).expect("read the rows");
            std::fs::write(&nut, rows_nut(&rows)).expect("write the rows as NUT");
            text(&nut)
        };
        args.extend(["-f".into(), "nut".into(), "-i".into(), source]);
        pads.push_str(&format!("[{port}={}:d]", n + 1));
    }
    args.extend([
        "-m".into(),
        format!("weave={}", text(&module())),
        "-params-from".into(),
        format!("weave={params_path}"),
        "-filter_complex".into(),
        format!("{pads}weave[v=out0][@rows=r0]"),
        "-map".into(),
        "[out0]".into(),
        "-f".into(),
        "nut".into(),
        text(&out),
        "-map".into(),
        "[r0]".into(),
        "-f".into(),
        "ndjson".into(),
        "-".into(),
    ]);
    let mut child = Command::new(sidecar().expect("a sidecar"))
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffrwd-wasm");
    let mut stdin = child.stdin.take().expect("child stdin");
    let written: Vec<u8> = match feed {
        Feed::None => Vec::new(),
        Feed::Rows(bytes) => rows_nut(&String::from_utf8_lossy(bytes)),
        Feed::Packets(bytes) => bytes.to_vec(),
    };
    let paced = matches!(feed, Feed::Packets(_));
    let delayed = matches!(feed, Feed::Rows(_));
    let writer = std::thread::spawn(move || {
        if delayed {
            // Long enough that the packets are already moving.
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        if paced {
            // A piece every few milliseconds, so the run takes calls
            // rather than one.
            let pieces = 24usize;
            let step = written.len().div_ceil(pieces).max(1);
            for piece in written.chunks(step) {
                if stdin.write_all(piece).is_err() {
                    break;
                }
                let _ = stdin.flush();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        } else if !written.is_empty() {
            let _ = stdin.write_all(&written);
            let _ = stdin.flush();
        }
        drop(stdin);
    });
    let finished = child.wait_with_output().expect("wait for ffrwd-wasm");
    writer.join().expect("the stdin writer");
    assert!(
        finished.status.success(),
        "ffrwd-wasm exited with {:?}\n{}",
        finished.status.code(),
        String::from_utf8_lossy(&finished.stderr)
    );
    Run {
        rows: String::from_utf8_lossy(&finished.stdout)
            .lines()
            .map(str::to_string)
            .filter(|line| !line.trim().is_empty())
            .collect(),
        out,
    }
}

/// ffmpeg's per-frame hashes, the hash column alone.
///
/// The columns around the hash are a decoder's account of the timing,
/// which the packet comparison below pins exactly; these are the
/// pictures.
fn frame_hashes(path: &Path) -> Vec<String> {
    ffmpeg(&["-i", &text(path), "-f", "framemd5", "-"])
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.rsplit(',').next().map(|hash| hash.trim().to_string()))
        .collect()
}

/// Every packet's pts, dts, duration and flags, as ffprobe prints them.
fn packets(args: &[&str]) -> Vec<String> {
    ffprobe(
        &[
            &[
                "-show_packets",
                "-of",
                "csv=p=0",
                "-show_entries",
                "packet=pts,dts,duration,flags",
            ],
            args,
        ]
        .concat(),
    )
    .lines()
    .map(str::to_string)
    .collect()
}

fn nut_packets(path: &Path) -> Vec<String> {
    packets(&["-f", "nut", "-i", &text(path)])
}

fn file_packets(path: &Path) -> Vec<String> {
    packets(&["-i", &text(path)])
}

/// The woven NUT muxed with `-c copy`, which moves no picture.
fn mux(nut: &Path, to: &Path) {
    ffmpeg(&["-f", "nut", "-i", &text(nut), "-c", "copy", &text(to)]);
}

/// A woven MP4 back as an Annex B elementary stream, which is what the
/// command line tool reads.
fn annexb(dir: &Path, mp4: &Path, codec: &str) -> PathBuf {
    let (bsf, format, extension) = match codec {
        "h264" => ("h264_mp4toannexb", "h264", "h264"),
        "hevc" => ("hevc_mp4toannexb", "hevc", "h265"),
        other => panic!("{other} has no Annex B form the tool reads"),
    };
    let out = dir.join(format!("woven.{extension}"));
    ffmpeg(&[
        "-i",
        &text(mp4),
        "-c",
        "copy",
        "-bsf:v",
        bsf,
        "-f",
        format,
        &text(&out),
    ]);
    out
}

/// What `ffrwd-index read --video` prints, as rows.
fn read_tool(stream: &Path) -> Vec<BTreeMap<String, String>> {
    let output = Command::new(env!("CARGO_BIN_EXE_ffrwd-index"))
        .args(["read", "--video", &text(stream), "--fps", &FPS.to_string()])
        .output()
        .expect("spawn ffrwd-index");
    assert!(
        output.status.success(),
        "ffrwd-index read exited with {:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(members)
        .collect()
}

/// One record of the rows a caller wrote.
struct Record {
    start_t: f64,
    end_t: f64,
    values: Vec<f32>,
}

/// A rows file of `count` vectors, one per second, each a different
/// shape so no two records could be confused.
fn write_rows(path: &Path, space: &str, dims: usize, count: usize) -> Vec<Record> {
    let mut records = Vec::new();
    let mut text = String::new();
    for index in 0..count {
        let values: Vec<f32> = (0..dims)
            .map(|c| ((c + index * 7) as f32 * 0.37).sin() * (1.0 + index as f32 * 0.25))
            .collect();
        let printed: Vec<String> = values.iter().map(|v| format!("{v}")).collect();
        let start_t = index as f64;
        let end_t = start_t + 1.0;
        text.push_str(&format!(
            r#"{{"space":"{space}","start_t":{start_t},"end_t":{end_t},"vector":[{}]}}"#,
            printed.join(",")
        ));
        text.push('\n');
        records.push(Record {
            start_t,
            end_t,
            values,
        });
    }
    std::fs::write(path, text).expect("write the rows");
    records
}

/// A rows file whose rows name NO space, which is what a producer
/// writes: it hands over spans and vectors and knows nothing about
/// embedding spaces. `first_t` is the second the first span starts at.
fn write_unnamed_rows(path: &Path, dims: usize, count: usize, first_t: f64) -> Vec<Record> {
    let mut records = Vec::new();
    let mut text = String::new();
    for index in 0..count {
        let step = index + first_t as usize;
        let values: Vec<f32> = (0..dims)
            .map(|c| ((c + step * 7) as f32 * 0.37).sin() * (1.0 + step as f32 * 0.25))
            .collect();
        let printed: Vec<String> = values.iter().map(|v| format!("{v}")).collect();
        let start_t = first_t + index as f64;
        let end_t = start_t + 1.0;
        text.push_str(&format!(
            r#"{{"start_t":{start_t},"end_t":{end_t},"vector":[{}]}}"#,
            printed.join(",")
        ));
        text.push('\n');
        records.push(Record {
            start_t,
            end_t,
            values,
        });
    }
    std::fs::write(path, text).expect("write the rows");
    records
}

const ESCAPES: usize = 2;

/// One i8 space of `dims`, as the params spell one.
fn space_param(space: &str, dims: usize) -> String {
    format!(
        r#"{{"name":"{space}","dims":{dims},"encoding":"i8","unit_length":false,"modality":"picture","model":"test:model","producer":"the weave tests"}}"#
    )
}

/// The params for one i8 space of `dims`.
fn params(space: &str, dims: usize, placement: &str) -> String {
    format!(
        r#"{{"spaces":[{}],"placement":"{placement}","escapes":{ESCAPES}}}"#,
        space_param(space, dims)
    )
}

/// The params for two spaces, with the list passed as the TEXT a query
/// writes rather than as an array: a wasm function's value arguments
/// are text, number, boolean or vector, so text is the only form a SQL
/// caller has.
fn params_as_text(spaces: &[&str], dims: usize, placement: &str) -> String {
    let list: Vec<String> = spaces
        .iter()
        .map(|space| space_param(space, dims))
        .collect();
    let written = format!("[{}]", list.join(","));
    let quoted = written.replace('"', "\\\"");
    format!(r#"{{"spaces":"{quoted}","placement":"{placement}","escapes":{ESCAPES}}}"#)
}

/// What a reader rebuilds from the record this module would have
/// written: the vector, quantized and read back, component for
/// component. A row whose values match this matches the bytes.
fn expected(values: &[f32]) -> Vec<f32> {
    Planes::quantize(values, ESCAPES)
        .expect("a quantized vector")
        .reconstruct()
        .expect("a reconstruction")
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

// ------------------------------------------------------------------ //
// One codec, one framing, end to end.
// ------------------------------------------------------------------ //

/// Everything test 1 asks of one codec and framing: the pictures, the
/// records, and the packets.
fn whole_stream(name: &str, codec: &str, length_prefixed_input: bool) {
    let Some(_) = sidecar() else { return };
    let dir = scratch(name);
    let plain = encode(&dir, codec);
    let input = if length_prefixed_input {
        let reframed = as_length_prefixed(&dir, &plain);
        assert!(
            length_prefixed(&reframed),
            "the reframed input is not length-prefixed after all"
        );
        reframed
    } else {
        assert!(
            !length_prefixed(&plain),
            "ffmpeg's NUT carries a configuration record, not Annex B"
        );
        plain.clone()
    };

    let dims = 96;
    let rows = dir.join("rows.ndjson");
    let wanted = write_rows(&rows, "clip", dims, 3);
    let run = weave(&dir, &input, &params("clip", dims, "keyframe"), &rows);

    // Every record went in, and nothing was late or dropped.
    let woven = run.events("woven");
    assert_eq!(woven.len(), wanted.len(), "{name}: {:?}", run.rows);
    let summary = run.summary();
    assert_eq!(summary["records"], wanted.len().to_string());
    assert_eq!(summary["late"], "0");
    assert_eq!(summary["dropped"], "0");
    assert!(summary["bytes_added"].parse::<u64>().expect("a count") > 0);

    // (c) One packet in, one packet out, every timestamp untouched, off
    // the wire the filter wrote.
    assert_eq!(
        nut_packets(&run.out),
        nut_packets(&input),
        "{name}: the packets that left are not the packets that arrived"
    );
    assert_eq!(nut_packets(&input).len(), FRAMES);

    // (a) The pictures are the pictures the encoder wrote, and (c)
    // again through a real muxer: pts, dts and duration for every
    // packet, in both containers, against the same encode that never
    // met the filter.
    let mut muxed = Vec::new();
    for container in ["mp4", "mkv"] {
        let unwoven = dir.join(format!("plain.{container}"));
        mux(&plain, &unwoven);
        let path = dir.join(format!("woven.{container}"));
        mux(&run.out, &path);
        let wanted_hashes = frame_hashes(&unwoven);
        assert_eq!(wanted_hashes.len(), FRAMES);
        assert_eq!(
            frame_hashes(&path),
            wanted_hashes,
            "{name}: the woven {container} does not decode to the same frames"
        );
        let wanted_packets = file_packets(&unwoven);
        assert_eq!(wanted_packets.len(), FRAMES);
        assert_eq!(
            file_packets(&path),
            wanted_packets,
            "{name}: the woven {container} does not carry the same packets"
        );
        muxed.push(path);
    }

    // (b) Every record reads back, with the span it was given and the
    // body the encoding would have written.
    let read = match codec {
        "av1" => read_av1(&muxed[0], &dir),
        _ => {
            let stream = annexb(&dir, &muxed[0], codec);
            read_tool(&stream)
        }
    };
    check_records(name, &read, &woven, &wanted, dims);
}

/// The records a `read` printed against the records that were written,
/// with the module's own rows as the bridge: the tool reads an
/// elementary stream, whose clock is decode order and not the
/// container's presentation time, so what is compared is the OFFSETS,
/// which are what the format actually carries.
fn check_records(
    name: &str,
    read: &[BTreeMap<String, String>],
    woven: &[BTreeMap<String, String>],
    wanted: &[Record],
    dims: usize,
) {
    let records: Vec<&BTreeMap<String, String>> = read
        .iter()
        .filter(|row| row.contains_key("record_id"))
        .collect();
    assert_eq!(
        records.len(),
        wanted.len(),
        "{name}: read back {} of {} records",
        records.len(),
        wanted.len()
    );
    // The space declaration came back too, with the fields it went in
    // with.
    let spaces: Vec<&BTreeMap<String, String>> = read
        .iter()
        .filter(|row| !row.contains_key("record_id"))
        .collect();
    assert!(!spaces.is_empty(), "{name}: no space was declared");

    for (index, row) in records.iter().enumerate() {
        let record_id: usize = row["record_id"].parse().expect("a record id");
        let want = &wanted[record_id];
        let reported = woven
            .iter()
            .find(|reported| reported["record_id"] == row["record_id"])
            .unwrap_or_else(|| panic!("{name}: record {record_id} was never reported"));

        // The offsets the stream carries are the offsets the module
        // said it wrote, and those put the span back where it was.
        let carrier: f64 = row["carrier_ms"].parse().expect("a carrier time");
        let start_off: f64 = row["start_ms"].parse::<f64>().expect("a start") - carrier;
        let end_off: f64 = row["end_ms"].parse::<f64>().expect("an end") - carrier;
        assert!(
            close(
                start_off,
                reported["start_off_ms"].parse().expect("an offset")
            ),
            "{name}: record {record_id} start offset {start_off} is not what was reported"
        );
        assert!(
            close(end_off, reported["end_off_ms"].parse().expect("an offset")),
            "{name}: record {record_id} end offset {end_off} is not what was reported"
        );
        assert!(
            close(end_off - start_off, (want.end_t - want.start_t) * 1000.0),
            "{name}: record {record_id} does not span what it was given"
        );
        // And the carrier plus the offsets is the span the caller asked
        // for, on the stream's own presentation clock.
        assert!(
            close(
                reported["carrier_t"].parse::<f64>().expect("a time") + start_off / 1000.0,
                want.start_t
            ),
            "{name}: record {record_id} did not come back at {}",
            want.start_t
        );

        // Byte for byte: the components are the components a reader
        // rebuilds from what the encoding would have written.
        let values: Vec<f64> = row["vector"]
            .split(',')
            .map(|v| v.parse().expect("a component"))
            .collect();
        assert_eq!(
            values.len(),
            dims,
            "{name}: record {record_id} lost a component"
        );
        for (component, (got, want)) in values.iter().zip(expected(&want.values)).enumerate() {
            assert!(
                close(*got, f64::from(want)),
                "{name}: record {record_id} component {component} is {got}, not {want}"
            );
        }
        // Every plane arrived: the policy puts a whole record on one
        // carrier.
        assert_eq!(
            row["planes"], "0,1,2,3,4,5,6,7",
            "{name}: record {record_id} lost a plane"
        );
        let _ = index;
    }
}

/// The same records, read out of an AV1 file's metadata OBUs. The
/// command line tool reads Annex B alone, and AV1 has no Annex B, so
/// this is the library doing what the tool would.
fn read_av1(mp4: &Path, dir: &Path) -> Vec<BTreeMap<String, String>> {
    let raw = dir.join("woven.obu");
    ffmpeg(&["-i", &text(mp4), "-c", "copy", "-f", "obu", &text(&raw)]);
    let bytes = std::fs::read(&raw).expect("read the OBU stream");
    let units = obu::temporal_units(&bytes).expect("the temporal units");
    let mut spaces: BTreeMap<u8, Space> = BTreeMap::new();
    let mut out = Vec::new();
    for (index, unit) in units.iter().enumerate() {
        let carrier_ms = (index as f64 * 1000.0 / f64::from(FPS)).round();
        for payload in carriage::units_obu(&bytes[unit.start..unit.end]) {
            let Ok(decoded) = Unit::decode(&payload) else {
                continue;
            };
            for message in &decoded.messages {
                match message {
                    Message::Space(space) => {
                        if spaces.insert(space.space_id, space.clone()).is_none() {
                            out.push(BTreeMap::from([(
                                "space".to_string(),
                                "declared".to_string(),
                            )]));
                        }
                    }
                    Message::Vector(record) => {
                        let space = spaces.get(&record.space_id).expect("a declared space");
                        let body = record.decode_body(space).expect("a body");
                        let VectorBody::I8(planes) = &body else {
                            panic!("an i8 space came back as something else");
                        };
                        let values = body.values().expect("the components");
                        out.push(BTreeMap::from([
                            ("record_id".to_string(), record.record_id.to_string()),
                            ("carrier_ms".to_string(), format!("{carrier_ms}")),
                            (
                                "start_ms".to_string(),
                                format!("{}", carrier_ms + f64::from(record.start_off)),
                            ),
                            (
                                "end_ms".to_string(),
                                format!("{}", carrier_ms + f64::from(record.end_off)),
                            ),
                            (
                                "planes".to_string(),
                                (0..8)
                                    .filter(|k| planes.present() >> k & 1 == 1)
                                    .map(|k| k.to_string())
                                    .collect::<Vec<String>>()
                                    .join(","),
                            ),
                            (
                                "vector".to_string(),
                                values
                                    .iter()
                                    .map(|v| format!("{v}"))
                                    .collect::<Vec<String>>()
                                    .join(","),
                            ),
                        ]));
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

#[test]
fn h264_in_annex_b_carries_its_records_and_moves_no_picture() {
    whole_stream("h264_annexb", "h264", false);
}

#[test]
fn h264_length_prefixed_carries_its_records_and_moves_no_picture() {
    whole_stream("h264_avcc", "h264", true);
}

#[test]
fn hevc_carries_its_records_and_moves_no_picture() {
    whole_stream("hevc", "hevc", false);
}

#[test]
fn hevc_length_prefixed_carries_its_records_and_moves_no_picture() {
    whole_stream("hevc_hvcc", "hevc", true);
}

#[test]
fn av1_carries_its_records_and_moves_no_picture() {
    whole_stream("av1", "av1", false);
}

// ------------------------------------------------------------------ //
// Live: rows arriving while the packets do.
// ------------------------------------------------------------------ //

/// The live placement, where the arrival times are the rows' own.
///
/// A host on this machine drains a four-second file in well under a
/// second, so which packet is in flight when a row lands on a pipe is
/// not a thing a test can pin, and neither is when a file's rows reach
/// the module, which is a reader thread of the host's. What CAN be
/// pinned is the placement itself, and `available_t` is how: each row
/// says when its writer had it, exactly as it would have arrived on a
/// wire, and every record must then ride a carrier at or after that,
/// looking back at a span that had already ended.
#[test]
fn a_record_that_existed_only_after_its_span_rides_a_later_carrier() {
    let Some(_) = sidecar() else { return };
    let dir = scratch("live_placement");
    let input = encode(&dir, "h264");
    let dims = 32;
    let rows = dir.join("rows.ndjson");
    let mut text = String::new();
    let mut wanted = Vec::new();
    for index in 0..3 {
        let values: Vec<f32> = (0..dims)
            .map(|c| ((c + index) as f32 * 0.29).cos())
            .collect();
        let printed: Vec<String> = values.iter().map(|v| format!("{v}")).collect();
        let start_t = index as f64 * 0.5;
        let end_t = start_t + 0.5;
        // A second and a half behind: the span had long ended before
        // anything described it.
        let available_t = end_t + 1.5;
        text.push_str(&format!(
            r#"{{"space":"clip","start_t":{start_t},"end_t":{end_t},"available_t":{available_t},"vector":[{}]}}"#,
            printed.join(",")
        ));
        text.push('\n');
        wanted.push((start_t, end_t, available_t));
    }
    std::fs::write(&rows, text).expect("write the rows");

    let run = weave(&dir, &input, &params("clip", dims, "next"), &rows);
    let woven = run.events("woven");
    assert_eq!(woven.len(), wanted.len(), "{:?}", run.rows);
    let mut carriers = Vec::new();
    for row in &woven {
        let record_id: usize = row["record_id"].parse().expect("a record id");
        let (start_t, end_t, available_t) = wanted[record_id];
        let carrier: f64 = row["carrier_t"].parse().expect("a time");
        assert!(
            carrier >= available_t - 1e-6,
            "record {record_id} rode a carrier its writer had not reached: {row:?}"
        );
        assert!(
            row["start_off_ms"].parse::<f64>().expect("an offset") < 0.0
                && row["end_off_ms"].parse::<f64>().expect("an offset") < 0.0,
            "a record behind the stream looked forward: {row:?}"
        );
        assert!(close(
            carrier + row["start_off_ms"].parse::<f64>().expect("an offset") / 1000.0,
            start_t
        ));
        assert!(close(
            carrier + row["end_off_ms"].parse::<f64>().expect("an offset") / 1000.0,
            end_t
        ));
        carriers.push((record_id, carrier));
    }
    // A record whose writer had it earlier is never on a later carrier
    // than one whose writer had it later. Which carriers exactly is not
    // a thing to assert here: the rows reader is a thread of the host's
    // and a file's rows may all reach the module on one call, in which
    // case one carrier takes all three, correctly. The unit test in
    // `rows` drives the arrivals itself and pins the spread.
    carriers.sort_by_key(|(record_id, _)| *record_id);
    for pair in carriers.windows(2) {
        assert!(
            pair[1].1 >= pair[0].1 - 1e-6,
            "a record available earlier rode a later carrier: {pair:?}"
        );
    }
    let summary = run.summary();
    assert_eq!(summary["records"], wanted.len().to_string());
    assert_eq!(summary["late"], "0");
    assert_eq!(nut_packets(&run.out), nut_packets(&input));
}

/// The same policy with the rows arriving on a pipe, which is what a
/// live run actually does. Where they land depends on how far the
/// packets have got when each row turns up, and that is a race by
/// design; what is pinned is that every record was woven, that none of
/// them looked forward, and that the rows arriving last - on the final
/// call, after every packet - still found a carrier.
#[test]
fn rows_arriving_after_the_packets_ride_the_next_carrier_looking_back() {
    let Some(_) = sidecar() else { return };
    let dir = scratch("live");
    let input = encode(&dir, "h264");
    let dims = 32;
    let rows = dir.join("rows.ndjson");
    let wanted = write_rows(&rows, "clip", dims, 4);
    let feed = std::fs::read(&rows).expect("the rows to feed");

    // The rows go in on standard input, written only once the run has
    // started: a live writer has a vector when the span it describes is
    // already behind the stream.
    let run = run_weave(
        &dir,
        &text(&input),
        Params::Inline(&params("clip", dims, "next")),
        &[("clip".to_string(), "-".to_string())],
        Feed::Rows(&feed),
    );

    let woven = run.events("woven");
    assert_eq!(woven.len(), wanted.len(), "{:?}", run.rows);
    for row in &woven {
        let start: f64 = row["start_off_ms"].parse().expect("an offset");
        let end: f64 = row["end_off_ms"].parse().expect("an offset");
        assert!(
            start < 0.0 && end <= 0.0,
            "a record that arrived behind the stream looked forward: {row:?}"
        );
        let carrier: f64 = row["carrier_t"].parse().expect("a time");
        let end_t: f64 = row["end_t"].parse().expect("a time");
        assert!(
            carrier >= end_t - 1e-6,
            "a record rode a carrier before its span had ended: {row:?}"
        );
    }
    // The rows are the run's own account of what it did, and they name
    // every record exactly once.
    let mut ids: Vec<String> = woven.iter().map(|row| row["record_id"].clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), wanted.len(), "a record was reported twice");
    let summary = run.summary();
    assert_eq!(summary["records"], wanted.len().to_string());
    assert_eq!(summary["late"], "0");

    // And the picture is still the picture.
    let plain_mp4 = dir.join("plain.mp4");
    let woven_mp4 = dir.join("woven.mp4");
    mux(&input, &plain_mp4);
    mux(&run.out, &woven_mp4);
    assert_eq!(frame_hashes(&woven_mp4), frame_hashes(&plain_mp4));
    assert_eq!(nut_packets(&run.out), nut_packets(&input));
}

// ------------------------------------------------------------------ //
// A budget per access unit.
// ------------------------------------------------------------------ //

/// The `spread` policy, with a budget too small to hold one message.
///
/// A record then leaves as SLICES, which is section 6, and the point of
/// the policy is that the bitrate it adds stays level: no access unit
/// takes more than the budget. Since a FRAGMENT names the record it
/// slices and not which of that record's values, a record cut this way
/// goes as one value, and the proof that it was right is that every
/// plane comes back out of an elementary stream read in DECODE order,
/// which is not the order the slices were written in.
///
/// The packets arrive a piece at a time on standard input, the way an
/// encoder hands them over, so the rows are in the module's hands while
/// there are still carriers to come. Off a file the whole stream can
/// reach the module before the first row does, and then there is no
/// budget left to spread anything over.
#[test]
fn a_budget_spreads_a_record_across_carriers_as_slices() {
    let Some(_) = sidecar() else { return };
    let dir = scratch("spread");
    let input = encode(&dir, "h264");
    let dims = 512;
    let rows = dir.join("rows.ndjson");
    let values: Vec<f32> = (0..dims).map(|c| ((c % 17) as f32) * 0.05 - 0.4).collect();
    let printed: Vec<String> = values.iter().map(|v| format!("{v}")).collect();
    std::fs::write(
        &rows,
        format!(
            r#"{{"space":"clip","start_t":0,"end_t":0.5,"vector":[{}]}}"#,
            printed.join(",")
        ) + "\n",
    )
    .expect("write the rows");

    // Forty bytes of messages per access unit, where one plane's
    // message is past eighty.
    let params = format!(
        r#"{{"spaces":[{{"name":"clip","dims":{dims},"encoding":"i8","model":"test:clip"}}],"placement":"spread","budget":40,"escapes":{ESCAPES}}}"#
    );
    let run = weave_streaming(&dir, &input, &params, &rows);

    let woven = run.events("woven");
    assert!(
        woven.len() > 4,
        "one record over {} carriers is not a spread: {:?}",
        woven.len(),
        run.rows
    );
    // The bitrate stays level: no carrier took more than the budget.
    for row in &woven {
        let bytes: usize = row["bytes"].parse().expect("a byte count");
        assert!(bytes <= 40, "a carrier took {bytes} bytes: {row:?}");
    }
    // The record's first row names the span, which is inside the value
    // being sliced rather than on the slices.
    let first = &woven[0];
    assert!(close(first["start_t"].parse().expect("a time"), 0.0));
    assert!(close(first["end_t"].parse().expect("a time"), 0.5));
    let summary = run.summary();
    assert_eq!(summary["records"], "1");
    assert_eq!(summary["late"], "0");
    assert_eq!(summary["dropped"], "0");
    assert_eq!(nut_packets(&run.out), nut_packets(&input));

    // Every slice came back, in the codec's own order and not the
    // writer's, and the record is whole.
    let mp4 = dir.join("woven.mp4");
    mux(&run.out, &mp4);
    let plain_mp4 = dir.join("plain.mp4");
    mux(&input, &plain_mp4);
    assert_eq!(frame_hashes(&mp4), frame_hashes(&plain_mp4));
    assert_eq!(file_packets(&mp4), file_packets(&plain_mp4));

    let stream = annexb(&dir, &mp4, "h264");
    let slices = fragments(&stream);
    assert!(
        slices > 1,
        "the record went in {slices} slices, so it was never cut"
    );
    let read = read_tool(&stream);
    let records: Vec<&BTreeMap<String, String>> = read
        .iter()
        .filter(|row| row.contains_key("record_id"))
        .collect();
    assert_eq!(records.len(), 1, "the record did not come back: {read:?}");
    assert_eq!(
        records[0]["planes"], "0,1,2,3,4,5,6,7",
        "a plane was lost between the slices"
    );
    let got: Vec<f64> = records[0]["vector"]
        .split(',')
        .map(|v| v.parse().expect("a component"))
        .collect();
    assert_eq!(got.len(), dims);
    for (component, (got, want)) in got.iter().zip(expected(&values)).enumerate() {
        assert!(
            close(*got, f64::from(want)),
            "component {component} is {got}, not {want}"
        );
    }
}

/// How many FRAGMENT messages an Annex B stream carries.
fn fragments(stream: &Path) -> usize {
    let bytes = std::fs::read(stream).expect("read the stream");
    let mut count = 0usize;
    for au in h26x::access_units(&bytes, Codec::H264) {
        for raw in carriage::units_annexb(&bytes[au.start..au.end], Codec::H264) {
            let Ok(unit) = Unit::decode(&raw) else {
                continue;
            };
            count += unit
                .messages
                .iter()
                .filter(|m| matches!(m, Message::Fragment(_)))
                .count();
        }
    }
    count
}

// ------------------------------------------------------------------ //
// More than one space.
// ------------------------------------------------------------------ //

/// Record ids count per space, so two spaces writing their first record
/// are both record 0, and everything downstream has to carry the space
/// beside the id to tell them apart.
///
/// The params go in through `-params-from`, which is where a spaces list
/// belongs: each space carries a name, a dimensionality and up to two
/// model URIs, and a run declaring a handful of them is past what a
/// command line should hold.
#[test]
fn two_spaces_keep_their_own_records_apart() {
    let Some(_) = sidecar() else { return };
    let dir = scratch("two_spaces");
    let input = encode(&dir, "h264");
    let rows = dir.join("rows.ndjson");
    let clip: Vec<f32> = (0..48).map(|c| (c as f32 * 0.31).sin()).collect();
    let text: Vec<f32> = (0..12).map(|c| (c as f32 * 0.77).cos()).collect();
    let printed = |values: &[f32]| {
        values
            .iter()
            .map(|v| format!("{v}"))
            .collect::<Vec<String>>()
            .join(",")
    };
    std::fs::write(
        &rows,
        format!(
            "{{\"space\":\"clip\",\"start_t\":0,\"end_t\":1,\"vector\":[{}]}}\n\
             {{\"space\":\"text\",\"start_t\":2,\"end_t\":3,\"vector\":[{}]}}\n",
            printed(&clip),
            printed(&text)
        ),
    )
    .expect("write the rows");

    let params = r#"{"spaces":[
        {"name":"clip","dims":48,"encoding":"i8","modality":"picture","model":"test:clip"},
        {"name":"text","dims":12,"encoding":"f16","modality":"speech","model":"test:text"}
    ],"escapes":2}"#;
    let run = weave_with_params_file(&dir, &input, params, &rows);
    let woven = run.events("woven");
    assert_eq!(woven.len(), 2, "{:?}", run.rows);
    let by_space: BTreeMap<&str, &BTreeMap<String, String>> = woven
        .iter()
        .map(|row| (row["space"].as_str(), row))
        .collect();
    assert_eq!(by_space.len(), 2, "both rows named the same space");
    // Both are record 0 of their own space, and each reports its own
    // span rather than the other's.
    assert_eq!(by_space["clip"]["record_id"], "0");
    assert_eq!(by_space["text"]["record_id"], "0");
    assert!(close(
        by_space["clip"]["start_t"].parse().expect("a time"),
        0.0
    ));
    assert!(close(
        by_space["clip"]["end_t"].parse().expect("a time"),
        1.0
    ));
    assert!(close(
        by_space["text"]["start_t"].parse().expect("a time"),
        2.0
    ));
    assert!(close(
        by_space["text"]["end_t"].parse().expect("a time"),
        3.0
    ));
    // The layered encoding names its planes; a binary16 space has none.
    assert_eq!(by_space["clip"]["planes"], "0,1,2,3,4,5,6,7");
    assert!(!by_space["text"].contains_key("planes"));
    assert_eq!(run.summary()["spaces"], "2");
    assert_eq!(run.summary()["records"], "2");

    // And both read back out of the stream, in their own spaces.
    let mp4 = dir.join("woven.mp4");
    mux(&run.out, &mp4);
    let read = read_tool(&annexb(&dir, &mp4, "h264"));
    let spaces: Vec<&BTreeMap<String, String>> = read
        .iter()
        .filter(|row| !row.contains_key("record_id"))
        .collect();
    assert_eq!(spaces.len(), 2, "two spaces were declared");
    let records: Vec<&BTreeMap<String, String>> = read
        .iter()
        .filter(|row| row.contains_key("record_id"))
        .collect();
    assert_eq!(records.len(), 2);
    let mut ids: Vec<&str> = records.iter().map(|row| row["space_id"].as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["0", "1"], "both records came back in one space");
    assert_eq!(nut_packets(&run.out), nut_packets(&input));
}

// ------------------------------------------------------------------ //
// Rows nothing can be made of.
// ------------------------------------------------------------------ //

#[test]
fn a_broken_row_is_reported_and_the_stream_goes_on() {
    let Some(_) = sidecar() else { return };
    let dir = scratch("broken");
    let input = encode(&dir, "h264");
    let dims = 16;
    let rows = dir.join("rows.ndjson");
    let good: Vec<String> = (0..dims).map(|c| format!("{}", c as f32 * 0.1)).collect();
    let good = good.join(",");
    std::fs::write(
        &rows,
        format!(
            concat!(
                // A vector of the wrong width.
                "{{\"space\":\"clip\",\"start_t\":0,\"end_t\":1,\"vector\":[1,2,3]}}\n",
                // A space nobody declared.
                "{{\"space\":\"nope\",\"start_t\":0,\"end_t\":1,\"vector\":[{good}]}}\n",
                // Components that are not finite numbers.
                "{{\"space\":\"clip\",\"start_t\":1,\"end_t\":2,\"vector\":[{nan}]}}\n",
                // A row that is not JSON at all.
                "{{ not a row\n",
                // A span no offset from any carrier could reach, which
                // is a row arriving past the last packet by any clock.
                "{{\"space\":\"clip\",\"start_t\":34560000,\"end_t\":34560001,\"vector\":[{good}]}}\n",
                // And one good row, after every one of them.
                "{{\"space\":\"clip\",\"start_t\":2,\"end_t\":3,\"vector\":[{good}]}}\n"
            ),
            good = good,
            nan = vec!["1e400"; dims].join(","),
        ),
    )
    .expect("write the rows");

    let run = weave(&dir, &input, &params("clip", dims, "keyframe"), &rows);
    let dropped = run.events("dropped");
    assert_eq!(
        dropped.len(),
        4,
        "four rows could not be read, and these were reported: {:?}",
        run.rows
    );
    for wanted in ["components", "no space is declared", "finite", "JSON"] {
        assert!(
            dropped.iter().any(|row| row["reason"].contains(wanted)),
            "no row was dropped for {wanted}: {dropped:?}"
        );
    }
    // The far-future span parsed, so it was submitted and then found to
    // be further from every carrier than an offset can say.
    let late = run.events("late");
    assert_eq!(late.len(), 1, "{:?}", run.rows);
    assert_eq!(late[0]["space"], "clip");

    // The good row still went in, and the stream is whole.
    let woven = run.events("woven");
    assert_eq!(woven.len(), 1, "{:?}", run.rows);
    let summary = run.summary();
    assert_eq!(summary["records"], "1");
    assert_eq!(summary["dropped"], "4");
    assert_eq!(summary["late"], "1");
    assert_eq!(nut_packets(&run.out), nut_packets(&input));
}

// ------------------------------------------------------------------ //
// A cut of a woven file.
// ------------------------------------------------------------------ //

#[test]
fn a_cut_of_a_woven_file_reads_from_its_first_frame() {
    let Some(_) = sidecar() else { return };
    let dir = scratch("cut");
    let input = encode(&dir, "h264");
    let dims = 24;
    let rows = dir.join("rows.ndjson");
    let wanted = write_rows(&rows, "clip", dims, 3);
    let run = weave(&dir, &input, &params("clip", dims, "keyframe"), &rows);

    let whole = dir.join("woven.mp4");
    mux(&run.out, &whole);
    let cut = dir.join("cut.mp4");
    ffmpeg(&["-ss", "1", "-i", &text(&whole), "-c", "copy", &text(&cut)]);

    let stream = annexb(&dir, &cut, "h264");
    let read = read_tool(&stream);
    // The cut begins at a keyframe, and every keyframe declares every
    // space, so the first thing the reader finds is the declaration.
    let declared = read
        .iter()
        .filter(|row| !row.contains_key("record_id"))
        .count();
    assert!(declared > 0, "the cut declares no space: {read:?}");
    assert!(
        !read[0].contains_key("record_id"),
        "the cut's first row is not a space declaration"
    );
    let kept: Vec<&BTreeMap<String, String>> = read
        .iter()
        .filter(|row| row.contains_key("record_id"))
        .collect();
    assert!(
        !kept.is_empty() && kept.len() <= wanted.len(),
        "a cut kept {} of {} records",
        kept.len(),
        wanted.len()
    );
    // What survives still describes the span it described, as an offset
    // from a frame that is still there.
    for row in kept {
        let record_id: usize = row["record_id"].parse().expect("a record id");
        let carrier: f64 = row["carrier_ms"].parse().expect("a time");
        let span = row["end_ms"].parse::<f64>().expect("an end")
            - row["start_ms"].parse::<f64>().expect("a start");
        assert!(
            close(
                span,
                (wanted[record_id].end_t - wanted[record_id].start_t) * 1000.0
            ),
            "record {record_id} lost its span in the cut"
        );
        assert!(carrier >= 0.0);
    }
    // And the pictures the cut kept are still the encoder's own.
    assert!(frame_hashes(&cut).len() < FRAMES);
}

// ------------------------------------------------------------------ //
// What the host reads off the module without running it.
// ------------------------------------------------------------------ //

#[test]
fn the_sidecar_describes_the_module_as_a_node() {
    let Some(binary) = sidecar() else { return };
    let output = Command::new(&binary)
        .args(["--describe", &text(&module())])
        .output()
        .expect("spawn ffrwd-wasm");
    assert!(
        output.status.success(),
        "--describe exited with {:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let printed = String::from_utf8_lossy(&output.stdout);
    let described = members(printed.trim());
    assert_eq!(described["world"], "node-module");
    assert_eq!(described["node"], "true");
    assert_eq!(described["name"], "weave");
    assert_eq!(described["version"], env!("CARGO_PKG_VERSION"));
    for capability in ["nn", "http", "udp"] {
        assert_eq!(described[capability], "false", "{capability} was asked for");
    }

    // The schemas it publishes are the schemas it reads by.
    let Json::Object(printed) = json(printed.trim()) else {
        panic!("--describe printed something that is not an object");
    };
    for schema in ["params_schema", "rows_schema"] {
        let Some((_, Json::Object(members))) = printed.iter().find(|(name, _)| name == schema)
        else {
            panic!("no {schema} object");
        };
        assert!(
            members.iter().any(|(name, _)| name == "properties"),
            "{schema}"
        );
    }

    // And its shape: the packets, an input per space, the packets out.
    let output = Command::new(&binary)
        .args([
            "--shape",
            &text(&module()),
            "--params",
            &params_as_text(&["clip", "speech"], 8, "keyframe"),
            "--bound",
            "v,speech",
        ])
        .output()
        .expect("spawn ffrwd-wasm");
    assert!(
        output.status.success(),
        "--shape exited with {:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let shape = String::from_utf8_lossy(&output.stdout);
    for wanted in [
        r#""name":"v""#,
        r#""name":"clip""#,
        r#""name":"speech""#,
        r#""kind":"packets""#,
        r#""kind":"interval""#,
        r#""codecs":["h264","hevc","av1"]"#,
    ] {
        assert!(shape.contains(wanted), "{wanted} is not in {shape}");
    }
}

#[test]
fn params_that_say_nothing_useful_are_refused_before_a_packet_moves() {
    let Some(binary) = sidecar() else { return };
    let dir = scratch("params");
    let input = encode(&dir, "h264");
    for (n, (params, wanted)) in [
        ("{}", "`spaces` is required"),
        (r#"{"spaces":[]}"#, "at least one space"),
        (
            r#"{"spaces":[{"name":"c","dims":8}],"placement":"spread"}"#,
            "budget",
        ),
        (
            r#"{"spaces":[{"name":"c","dims":8}],"planes":9}"#,
            "at most 8",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.join(format!("params-{n}.json"));
        std::fs::write(&path, params).expect("write the params");
        let output = Command::new(&binary)
            .args([
                "-f",
                "nut",
                "-i",
                &text(&input),
                "-m",
                &format!("weave={}", text(&module())),
                "-params-from",
                &format!("weave={}", text(&path)),
                "-filter_complex",
                "[v=0:v]weave[v=out0]",
                "-map",
                "[out0]",
                "-f",
                "nut",
                &text(&dir.join("unwritten.nut")),
            ])
            .output()
            .expect("spawn ffrwd-wasm");
        assert!(!output.status.success(), "{params} was accepted");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(wanted),
            "{params} was refused without saying {wanted}:\n{stderr}"
        );
    }
}

/// Numbers in a row print as numbers, which a caller reading the rows
/// with a JSON parser depends on.
#[test]
fn the_test_harness_reads_the_rows_it_checks() {
    let row = r#"{"event":"woven","space":"clip","record_id":3,"carrier_t":1.08,"planes":[0,1],"bytes":40}"#;
    let read = members(row);
    assert_eq!(read["event"], "woven");
    assert_eq!(read["record_id"], "3");
    assert_eq!(read["planes"], "0,1");
    let Json::Object(parsed) = json(row) else {
        panic!("not an object");
    };
    assert!(close(
        parsed
            .iter()
            .find(|(name, _)| name == "carrier_t")
            .expect("a time")
            .1
            .number(),
        1.08
    ));
}

// ------------------------------------------------------------------ //
// The last keyframe holds what nothing else would.
// ------------------------------------------------------------------ //

/// Section 7 changed on 2026-09-19: a record whose span ends after the
/// last keyframe rides that keyframe, with an `end_off` that looks
/// forward, so the keyframes of a file hold all of its records and a
/// reader of sync samples alone never has to go and find the end of the
/// track. A writer streaming past does not know which keyframe is the
/// last one, so the module holds each keyframe, and every packet behind
/// it, until the next keyframe arrives or the stream ends.
#[test]
fn a_record_past_the_last_keyframe_rides_that_keyframe() {
    let Some(_) = sidecar() else { return };
    for codec in ["h264", "hevc", "av1"] {
        let dir = scratch(&format!("last_keyframe_{codec}"));
        let input = encode(&dir, codec);
        let dims = 16;
        let rows = dir.join("rows.ndjson");
        // The encodes are four seconds at a keyframe a second, so a
        // span ending at 3.8 has no keyframe at or after it.
        let values: Vec<f32> = (0..dims).map(|c| (c as f32 * 0.41).sin()).collect();
        let printed: Vec<String> = values.iter().map(|v| format!("{v}")).collect();
        std::fs::write(
            &rows,
            format!(
                r#"{{"space":"clip","start_t":3.5,"end_t":3.8,"vector":[{}]}}"#,
                printed.join(",")
            ) + "\n",
        )
        .expect("write the rows");

        let run = weave(&dir, &input, &params("clip", dims, "keyframe"), &rows);
        let woven = run.events("woven");
        assert_eq!(woven.len(), 1, "{codec}: {:?}", run.rows);
        let row = &woven[0];
        assert!(
            row["end_off_ms"].parse::<f64>().expect("an offset") > 0.0,
            "{codec}: the record did not look forward: {row:?}"
        );
        let summary = run.summary();
        assert_eq!(summary["late"], "0", "{codec}");
        assert_eq!(
            summary["overran"], "0",
            "{codec}: the hold reached its bound"
        );
        assert_eq!(
            nut_packets(&run.out),
            nut_packets(&input),
            "{codec}: holding a GOP changed the packets"
        );

        // The carrier really is a keyframe, and it is the last one.
        // The times are the NUT's own, which is the clock the module
        // reports in: a muxer shifts a stream whose first picture is
        // not at zero, and this stream's is not.
        let last_keyframe: f64 = ffprobe(&[
            "-show_packets",
            "-of",
            "csv=p=0",
            "-show_entries",
            "packet=pts_time,flags",
            "-f",
            "nut",
            "-i",
            &text(&run.out),
        ])
        .lines()
        .filter(|line| line.contains(",K"))
        .filter_map(|line| line.split(',').next().and_then(|value| value.parse().ok()))
        .fold(f64::MIN, f64::max);
        assert!(
            close(row["carrier_t"].parse().expect("a time"), last_keyframe),
            "{codec}: the record rode {}, and the last keyframe is at {last_keyframe}",
            row["carrier_t"]
        );

        let mp4 = dir.join("woven.mp4");
        mux(&run.out, &mp4);

        // And a copy that keeps the keyframes and throws the rest away
        // still has it, which is the whole point of the rule.
        let keys = dir.join("keys.mp4");
        ffmpeg(&[
            "-discard",
            "nokey",
            "-i",
            &text(&mp4),
            "-c",
            "copy",
            "-f",
            "mp4",
            &text(&keys),
        ]);
        let read = read_container(&keys);
        assert_eq!(
            read.len(),
            1,
            "{codec}: a keyframe copy lost the record: {read:?}"
        );
    }
}

// ------------------------------------------------------------------ //
// Two inputs, and the space each one is named for.
// ------------------------------------------------------------------ //

/// The shape a query compiles to: one rows input per space the call
/// binds, and rows that name no space at all.
///
/// A query binds each producer to the input named for its space, so
/// the input a producer's rows arrive on is what puts them in a space.
/// Nothing here says `space` anywhere: the params declare `clip` and
/// `speech`, the inputs are called `clip` and `speech`, and both spaces
/// have to come back out of the file.
#[test]
fn rows_with_no_space_land_in_the_space_their_input_is_named_for() {
    let Some(_) = sidecar() else { return };
    let dir = scratch("rows_arguments");
    let input = encode(&dir, "h264");
    let dims = 16;
    let clip_rows = dir.join("clip.ndjson");
    let speech_rows = dir.join("speech.ndjson");
    // Four seconds at a keyframe a second: the first two spans belong
    // to one argument and the last two to the other.
    let clip = write_unnamed_rows(&clip_rows, dims, 2, 0.0);
    let speech = write_unnamed_rows(&speech_rows, dims, 2, 2.0);

    let run = weave_arguments(
        &dir,
        &input,
        &params_as_text(&["clip", "speech"], dims, "keyframe"),
        &[("clip", &clip_rows), ("speech", &speech_rows)],
    );
    let woven = run.events("woven");
    assert_eq!(woven.len(), 4, "{:?}", run.rows);
    let named: Vec<&str> = woven.iter().map(|row| row["space"].as_str()).collect();
    assert_eq!(
        named.iter().filter(|space| **space == "clip").count(),
        2,
        "the clip argument's rows: {named:?}"
    );
    assert_eq!(
        named.iter().filter(|space| **space == "speech").count(),
        2,
        "the speech argument's rows: {named:?}"
    );
    let summary = run.summary();
    assert_eq!(summary["dropped"], "0", "{:?}", run.rows);
    assert_eq!(summary["late"], "0", "{:?}", run.rows);
    assert_eq!(summary["spaces"], "2");

    // And out of the muxed file, which is where a reader meets them:
    // two space ids, and each record with the span its own argument
    // wrote.
    let mp4 = dir.join("woven.mp4");
    mux(&run.out, &mp4);
    let read = read_container(&mp4);
    assert_eq!(read.len(), 4, "{read:?}");
    let ids: std::collections::BTreeSet<&str> =
        read.iter().map(|row| row["space_id"].as_str()).collect();
    assert_eq!(
        ids,
        ["0", "1"].into_iter().collect(),
        "both spaces did not come back: {read:?}"
    );
    for (space_id, wanted) in [("0", &clip), ("1", &speech)] {
        let rows: Vec<&BTreeMap<String, String>> = read
            .iter()
            .filter(|row| row["space_id"] == space_id)
            .collect();
        assert_eq!(rows.len(), wanted.len(), "space {space_id}: {rows:?}");
        for row in rows {
            let record_id: usize = row["record_id"].parse().expect("a record id");
            let record = &wanted[record_id];
            let span = row["end_ms"].parse::<f64>().expect("an end")
                - row["start_ms"].parse::<f64>().expect("a start");
            assert!(
                close(span, (record.end_t - record.start_t) * 1000.0),
                "space {space_id} record {record_id} lost its span: {row:?}"
            );
        }
    }
}

/// What `ffrwd-index read --mp4` prints, as rows.
fn read_container(path: &Path) -> Vec<BTreeMap<String, String>> {
    let output = Command::new(env!("CARGO_BIN_EXE_ffrwd-index"))
        .args(["read", "--mp4", &text(path), "--scan", "all"])
        .output()
        .expect("spawn ffrwd-index");
    assert!(
        output.status.success(),
        "ffrwd-index read exited with {:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("\"vector\":"))
        .map(members)
        .collect()
}
