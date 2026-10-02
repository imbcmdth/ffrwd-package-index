//! The two reading sinks, through the real sidecar and real ffmpeg.
//!
//! `records` and `spaces` are how ffrwd will read a woven file: the
//! compiler stream-copies packets into the sidecar hosting the node and
//! uses the rows it writes. Nothing here simulates that. Every test weaves a file
//! with the command line tool, pipes its packets through `ffrwd-wasm`
//! as coded NUT, and asks whether the rows agree with what the same
//! tool reads out of the same file.
//!
//! Each codec is fed twice: every packet, and the keyframes alone. The
//! second is what a compile-time read will actually do, and section 7
//! is what makes it complete.
//!
//! A node needs a host, and the host is `ffrwd-wasm` from ffrwd 0.29.
//! `FFRWD_WASM` names that binary and every test here skips without it,
//! loudly. `FFRWD_INDEX_RECORDS` and `FFRWD_INDEX_SPACES` name prebuilt
//! modules; without them the modules are built once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const SIDECAR_ENV: &str = "FFRWD_WASM";

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

/// One sink's `.wasm`, built once per test binary.
///
/// The build goes to a target directory of its own: this test runs
/// under a `cargo test` that holds the workspace's build lock, and a
/// second cargo in the same directory would wait for it forever.
fn module(name: &str) -> PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    if let Some(named) = std::env::var_os(format!("FFRWD_INDEX_{}", name.to_uppercase())) {
        let path = PathBuf::from(named);
        assert!(
            path.is_file(),
            "FFRWD_INDEX_{} names no file",
            name.to_uppercase()
        );
        return path;
    }
    let target = BUILT.get_or_init(|| {
        let target = workspace().join("target/wasm-module");
        let output = Command::new(std::env::var("CARGO").unwrap_or("cargo".into()))
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "-p",
                "records",
                "-p",
                "spaces",
            ])
            .current_dir(workspace())
            .env("CARGO_TARGET_DIR", &target)
            .output()
            .expect("spawn cargo to build the sinks");
        assert!(
            output.status.success(),
            "building the sinks failed. Set FFRWD_INDEX_RECORDS and FFRWD_INDEX_SPACES to \
             prebuilt modules.\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        target
    });
    target.join(format!("wasm32-wasip2/release/{name}.wasm"))
}

/// A directory of this test binary's own, on the same drive as the
/// workspace where `FFRWD_INDEX_SCRATCH` says so.
fn scratch() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let root = match std::env::var_os("FFRWD_INDEX_SCRATCH") {
            Some(named) => PathBuf::from(named),
            None => std::env::temp_dir(),
        };
        let path = root.join("ffrwd-index-sinks");
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a scratch directory");
        path
    })
}

fn at(name: &str) -> PathBuf {
    scratch().join(name)
}

fn text(path: &Path) -> String {
    path.to_str().expect("a UTF-8 path").to_string()
}

fn ffmpeg(args: &[&str]) {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        .args(args)
        .output()
        .expect("spawn ffmpeg");
    assert!(
        output.status.success(),
        "ffmpeg {args:?} exited with {:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn tool(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_ffrwd-index"))
        .args(args)
        .output()
        .expect("spawn ffrwd-index");
    assert!(
        output.status.success(),
        "ffrwd-index {args:?} exited with {:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// One run of the sidecar hosting a sink over one NUT file.
struct Run {
    rows: Vec<String>,
    stderr: String,
    ok: bool,
}

fn run_sink(name: &str, nut: &Path) -> Run {
    let output = Command::new(sidecar().expect("a sidecar"))
        .args([
            "-f",
            "nut",
            "-i",
            &text(nut),
            "-m",
            &format!("{name}={}", text(&module(name))),
            "-filter_complex",
            &format!("[v=0:v]{name}[@rows=r0]"),
            "-map",
            "[r0]",
            "-f",
            "ndjson",
            "-",
        ])
        .output()
        .expect("spawn ffrwd-wasm");
    Run {
        rows: String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        ok: output.status.success(),
    }
}

fn sink_rows(name: &str, nut: &Path) -> Vec<BTreeMap<String, String>> {
    let run = run_sink(name, nut);
    assert!(run.ok, "{name} over {}: {}", nut.display(), run.stderr);
    run.rows.iter().map(|row| members(row)).collect()
}

/// One flat JSON object as a map of member name to the member's text.
fn members(row: &str) -> BTreeMap<String, String> {
    let bytes: Vec<char> = row.chars().collect();
    let mut at = 0usize;
    let mut out = BTreeMap::new();
    assert_eq!(bytes.first(), Some(&'{'), "not an object: {row}");
    at += 1;
    loop {
        while bytes
            .get(at)
            .is_some_and(|c| c.is_whitespace() || *c == ',')
        {
            at += 1;
        }
        if bytes.get(at) == Some(&'}') || at >= bytes.len() {
            return out;
        }
        let name = string_at(&bytes, &mut at);
        while bytes
            .get(at)
            .is_some_and(|c| c.is_whitespace() || *c == ':')
        {
            at += 1;
        }
        let value = value_at(&bytes, &mut at);
        out.insert(name, value);
    }
}

fn string_at(bytes: &[char], at: &mut usize) -> String {
    assert_eq!(bytes.get(*at), Some(&'"'), "a name that is not a string");
    *at += 1;
    let mut out = String::new();
    while let Some(ch) = bytes.get(*at) {
        *at += 1;
        match ch {
            '"' => return out,
            '\\' => {
                let escape = bytes.get(*at).copied().unwrap_or('?');
                *at += 1;
                out.push(escape);
            }
            other => out.push(*other),
        }
    }
    panic!("a string that does not end");
}

/// One member's value as the text it was written as, with an array kept
/// whole so two vectors can be compared character for character.
fn value_at(bytes: &[char], at: &mut usize) -> String {
    match bytes.get(*at) {
        Some('"') => string_at(bytes, at),
        // An array or an object is one value, kept whole so that two
        // vectors compare character for character and a describe's own
        // schemas do not run the parser off the end.
        Some('[') | Some('{') => {
            let start = *at;
            let mut depth = 0usize;
            let mut in_string = false;
            while let Some(ch) = bytes.get(*at) {
                *at += 1;
                if in_string {
                    match ch {
                        '\\' => *at += 1,
                        '"' => in_string = false,
                        _ => {}
                    }
                    continue;
                }
                match ch {
                    '"' => in_string = true,
                    '[' | '{' => depth += 1,
                    ']' | '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return bytes[start..*at].iter().collect();
                        }
                    }
                    _ => {}
                }
            }
            panic!("a value that does not end");
        }
        _ => {
            let start = *at;
            while bytes
                .get(*at)
                .is_some_and(|c| !matches!(c, ',' | '}') && !c.is_whitespace())
            {
                *at += 1;
            }
            bytes[start..*at].iter().collect()
        }
    }
}

// ------------------------------------------------------------------ //
// The fixtures.
// ------------------------------------------------------------------ //

/// The rows woven into every fixture: one layered space of sixteen
/// components, four records, the last of them ending after the last
/// keyframe so that section 7's rule is under test here too.
fn vectors() -> String {
    let mut out = String::from(
        r#"{"space":{"id":1,"dims":16,"encoding":"i8","unit_length":true,"modality":"picture","model":"hf:test/tower@main/model.safetensors","query":"hf:test/tower@main/text.safetensors","producer":"the sink tests"}}"#,
    );
    out.push('\n');
    for index in 0..4 {
        let start = index * 700;
        let values: Vec<String> = (0..16)
            .map(|k| format!("{:.4}", ((index * 5 + k) as f32 * 0.29).sin()))
            .collect();
        out.push_str(&format!(
            r#"{{"space_id":1,"start_ms":{start},"end_ms":{},"vector":[{}]}}"#,
            start + 600,
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

    fn extension(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "obu",
        }
    }

    /// What ffmpeg calls the elementary stream, which is not always
    /// what the file is called: `-f hevc` writes a `.h265`.
    fn format(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "hevc",
            Codec::Av1 => "obu",
        }
    }

    fn encoder(self) -> Vec<&'static str> {
        match self {
            Codec::H264 => vec![
                "-c:v", "libx264", "-preset", "veryfast", "-g", "30", "-bf", "2",
            ],
            Codec::H265 => vec![
                "-c:v",
                "libx265",
                "-preset",
                "ultrafast",
                "-x265-params",
                "keyint=30:min-keyint=30:bframes=2:log-level=error",
            ],
            Codec::Av1 => vec![
                "-c:v",
                "libsvtav1",
                "-preset",
                "10",
                "-crf",
                "40",
                "-g",
                "30",
            ],
        }
    }
}

/// Three seconds of testsrc2 in each codec, woven, and put in an MP4.
/// Built once for the whole test binary.
fn fixtures() -> &'static BTreeMap<String, PathBuf> {
    static BUILT: OnceLock<BTreeMap<String, PathBuf>> = OnceLock::new();
    BUILT.get_or_init(|| {
        let rows = at("rows.ndjson");
        std::fs::write(&rows, vectors()).expect("the rows");
        let mut out = BTreeMap::new();
        for codec in Codec::every() {
            let plain = at(&format!("plain.{}", codec.extension()));
            let mut args = vec![
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x176:rate=30",
                "-t",
                "3",
                "-pix_fmt",
                "yuv420p",
            ];
            args.extend(codec.encoder());
            args.extend(["-f", codec.format(), plain.to_str().expect("a path")]);
            ffmpeg(&args);

            let woven = at(&format!("woven.{}", codec.extension()));
            tool(&[
                "weave",
                "--video",
                &text(&plain),
                "--vectors",
                &text(&rows),
                "--out",
                &text(&woven),
                "--placement",
                "keyframe",
            ]);
            let mp4 = at(&format!("woven-{}.mp4", codec.extension()));
            ffmpeg(&["-i", &text(&woven), "-c", "copy", &text(&mp4)]);
            out.insert(codec.extension().to_string(), mp4);

            // The same encode with nothing woven into it.
            let bare = at(&format!("bare-{}.mp4", codec.extension()));
            ffmpeg(&["-i", &text(&plain), "-c", "copy", &text(&bare)]);
            out.insert(format!("bare-{}", codec.extension()), bare);
        }
        out
    })
}

/// A file's packets as coded NUT, which is what a sink is fed.
fn as_nut(from: &Path, name: &str, keyframes_only: bool) -> PathBuf {
    let out = at(name);
    let mut args: Vec<&str> = Vec::new();
    if keyframes_only {
        // The MP4 demuxer's own flag: it reads the sample table and
        // never touches the rest of the file, which is what makes a
        // compile-time read cheap.
        args.extend(["-discard", "nokey"]);
    }
    let from = text(from);
    let out_text = text(&out);
    args.extend(["-i", &from, "-c", "copy", "-f", "nut", &out_text]);
    ffmpeg(&args);
    out
}

/// What `ffrwd-index read --mp4` says about the same file, which is the
/// answer the sinks are checked against.
fn read_tool(mp4: &Path) -> Vec<BTreeMap<String, String>> {
    tool(&["read", "--mp4", &text(mp4), "--scan", "all"])
        .lines()
        .filter(|line| line.contains("\"vector\":"))
        .map(members)
        .collect()
}

fn close(a: f64, b: f64, within: f64) -> bool {
    (a - b).abs() <= within
}

// ------------------------------------------------------------------ //
// records.
// ------------------------------------------------------------------ //

/// Every record the tool reads out of a file, the sink reads out of the
/// same file's packets: the same ids, the same spans to a millisecond,
/// and vectors character for character.
///
/// Twice per codec, because a compile-time read will be fed the second:
/// every packet, and the keyframes alone.
#[test]
fn records_reads_what_the_tool_reads() {
    let Some(_) = sidecar() else { return };
    for codec in Codec::every() {
        let name = codec.extension();
        let mp4 = fixtures()[name].clone();
        let wanted = read_tool(&mp4);
        assert_eq!(wanted.len(), 4, "{name}: the fixture is not what it was");

        for (how, keyframes_only) in [("every packet", false), ("keyframes alone", true)] {
            let nut = as_nut(
                &mp4,
                &format!("{name}-{}.nut", keyframes_only as u8),
                keyframes_only,
            );
            let rows = sink_rows("records", &nut);
            assert_eq!(
                rows.len(),
                wanted.len(),
                "{name}, {how}: {} rows for {} records",
                rows.len(),
                wanted.len()
            );
            // Every span is the same span, and every absolute time is
            // the same distance from the tool's, which is the shift the
            // container's edit list puts between the two clocks.
            let shifts: Vec<f64> = rows
                .iter()
                .zip(&wanted)
                .map(|(row, want)| {
                    want["start_ms"].parse::<f64>().expect("a time") / 1000.0
                        - row["start_t"].parse::<f64>().expect("a time")
                })
                .collect();
            for (index, (row, want)) in rows.iter().zip(&wanted).enumerate() {
                assert_eq!(row["index"], (index + 1).to_string(), "{name}, {how}");
                assert_eq!(row["space"], want["space_id"], "{name}, {how}");
                assert_eq!(row["record_id"], want["record_id"], "{name}, {how}");
                assert_eq!(row["planes"], want["planes"], "{name}, {how}");
                // The vector is the tool's own reconstruction, printed
                // by the same code, so it agrees to the character.
                assert_eq!(
                    row["vector"], want["vector"],
                    "{name}, {how}: record {index} came back a different vector"
                );
                // The two clocks are one edit list apart. An MP4 with
                // B-frames in it starts its presentation before its
                // first sample, and `-c copy` into NUT leaves that
                // behind: the sink is right about the packets it was
                // handed and the tool is right about the file it read.
                // What has to agree is every span and the one shift.
                for (field, from_ms) in [("start_t", "start_ms"), ("end_t", "end_ms")] {
                    let got: f64 = row[field].parse().expect("a time");
                    let ms: f64 = want[from_ms].parse().expect("a time");
                    let shift = ms / 1000.0 - got;
                    assert!(
                        shift.abs() < 0.2,
                        "{name}, {how}: record {index} {field} is {got}, and the tool says {}",
                        ms / 1000.0
                    );
                    assert!(
                        close(shift, shifts[0], 0.001),
                        "{name}, {how}: record {index} {field} is {shift} from the tool, and the first row was {}",
                        shifts[0]
                    );
                }
            }
        }
    }
}

/// A file with nothing of this format in it is not an error: it is a
/// file with no vectors, and a query asking for its records gets none.
#[test]
fn records_over_an_unwoven_file_answers_nothing() {
    let Some(_) = sidecar() else { return };
    for codec in Codec::every() {
        let name = codec.extension();
        let bare = fixtures()[&format!("bare-{name}")].clone();
        let nut = as_nut(&bare, &format!("bare-{name}.nut"), false);
        for sink in ["records", "spaces"] {
            let run = run_sink(sink, &nut);
            assert!(run.ok, "{sink} over an unwoven {name}: {}", run.stderr);
            assert!(
                run.rows.is_empty(),
                "{sink} over an unwoven {name} answered {:?}",
                run.rows
            );
        }
    }
}

// ------------------------------------------------------------------ //
// spaces.
// ------------------------------------------------------------------ //

/// One packet of a woven file says what the file carries.
///
/// This is `spaces`'s whole claim and section 3's: a writer puts every
/// space it is using on every keyframe from the FIRST keyframe of the
/// stream, whether or not a record rides there, so a reader that wants
/// only the shape of a file names one packet in advance and reads it.
/// Three codecs and two containers, because a demuxer is what decides
/// which packet is first and they do not all agree.
#[test]
fn spaces_answers_from_the_first_packet_alone() {
    let Some(_) = sidecar() else { return };
    for codec in Codec::every() {
        let name = codec.extension();
        let mp4 = fixtures()[name].clone();
        let mkv = at(&format!("first-{name}.mkv"));
        ffmpeg(&["-i", &text(&mp4), "-c", "copy", &text(&mkv)]);

        let whole = sink_rows("spaces", &as_nut(&mp4, &format!("{name}-whole.nut"), false));
        assert_eq!(whole.len(), 1, "{name}: the fixture is not what it was");

        for (container, from) in [("mp4", &mp4), ("mkv", &mkv)] {
            let first = at(&format!("{name}-first-{container}.nut"));
            ffmpeg(&[
                "-i",
                &text(from),
                "-c",
                "copy",
                "-frames:v",
                "1",
                "-f",
                "nut",
                &text(&first),
            ]);
            // One packet, and it is the whole answer.
            let rows = sink_rows("spaces", &first);
            assert_eq!(
                rows, whole,
                "{name}, {container}: the first packet did not say what the file carries"
            );

            let row = &rows[0];
            assert_eq!(row["space"], "1", "{name}, {container}");
            assert_eq!(row["dims"], "16", "{name}, {container}");
            assert_eq!(row["encoding"], "i8", "{name}, {container}");
            assert_eq!(row["unit_length"], "true", "{name}, {container}");
            assert_eq!(row["modality"], "picture", "{name}, {container}");
            assert_eq!(row["source"], "0", "{name}, {container}");
            assert_eq!(
                row["model"], "hf:test/tower@main/model.safetensors",
                "{name}, {container}"
            );
            assert_eq!(
                row["query"], "hf:test/tower@main/text.safetensors",
                "{name}, {container}"
            );
            assert_eq!(row["producer"], "the sink tests", "{name}, {container}");
            // The format carries no name, so this is a label made from
            // what it does carry: the producer, where there is one.
            assert_eq!(row["name"], "the sink tests", "{name}, {container}");
        }

        // And the space is answered once however many keyframes repeat
        // it, which is every one of them.
        let keys = as_nut(&mp4, &format!("{name}-keys.nut"), true);
        assert_eq!(sink_rows("spaces", &keys), whole, "{name}");
    }
}

// ------------------------------------------------------------------ //
// What a sink refuses.
// ------------------------------------------------------------------ //

/// A stream neither sink can read is refused before the module is
/// opened: one with no video in it has nothing to bind to `v`, and a
/// video codec the sinks do not read is refused by the codecs each
/// one's shape accepts.
#[test]
fn a_stream_neither_sink_can_read_is_refused_by_name() {
    let Some(_) = sidecar() else { return };
    let aac = at("aac.nut");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=1",
        "-c:a",
        "aac",
        "-f",
        "nut",
        &text(&aac),
    ]);
    for sink in ["records", "spaces"] {
        let run = run_sink(sink, &aac);
        assert!(!run.ok, "{sink} accepted a stream with no video in it");
        assert!(
            run.stderr.contains("0:v"),
            "{sink}: the refusal does not name the missing stream:\n{}",
            run.stderr
        );
    }

    let vp9 = at("vp9.nut");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x96:rate=10",
        "-t",
        "1",
        "-c:v",
        "libvpx-vp9",
        "-cpu-used",
        "8",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "nut",
        &text(&vp9),
    ]);
    let run = run_sink("records", &vp9);
    assert!(!run.ok, "records accepted vp9");
    assert!(
        run.stderr.contains("records"),
        "the refusal does not say which module:\n{}",
        run.stderr
    );
}

/// What the host reads off each module without running it: a node, and
/// a shape of one packets input, asking for as much of the stream as
/// the module needs, and no outputs but its rows.
#[test]
fn the_sidecar_describes_both_sinks() {
    let Some(binary) = sidecar() else { return };
    for (name, wants) in [("records", "keyframes"), ("spaces", "first")] {
        let output = Command::new(&binary)
            .args(["--describe", &text(&module(name))])
            .output()
            .expect("spawn ffrwd-wasm");
        assert!(
            output.status.success(),
            "--describe {name} exited with {:?}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        let printed = String::from_utf8_lossy(&output.stdout);
        let described = members(printed.trim());
        assert_eq!(described["world"], "node-module", "{name}");
        assert_eq!(described["node"], "true", "{name}");
        assert_eq!(described["name"], name);
        for capability in ["nn", "http", "udp"] {
            assert_eq!(described[capability], "false", "{name}: {capability}");
        }

        let output = Command::new(&binary)
            .args(["--shape", &text(&module(name)), "--bound", "v"])
            .output()
            .expect("spawn ffrwd-wasm");
        assert!(
            output.status.success(),
            "--shape {name} exited with {:?}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        let shape = String::from_utf8_lossy(&output.stdout);
        for wanted in [
            r#""kind":"packets""#.to_string(),
            r#""codecs":["h264","hevc","av1"]"#.to_string(),
            format!(r#""wants":"{wants}""#),
            r#""outputs":[]"#.to_string(),
        ] {
            assert!(
                shape.contains(&wanted),
                "{name}: {wanted} is not in {shape}"
            );
        }
    }
}

/// The harness reads the rows it checks.
#[test]
fn the_harness_reads_a_row() {
    let read = members(
        r#"{"index":1,"space":1,"start_t":0.5,"planes":[0,1],"vector":[0.5,-0.25],"name":"a, b"}"#,
    );
    assert_eq!(read["index"], "1");
    assert_eq!(read["start_t"], "0.5");
    assert_eq!(read["planes"], "[0,1]");
    assert_eq!(read["vector"], "[0.5,-0.25]");
    assert_eq!(read["name"], "a, b");
}

/// The other way a keyframe copy is made, which is what the compiler
/// reaches for where the demuxer has no `-discard nokey` of its own:
/// the `noise=drop=not(key)` bitstream filter, over Matroska here.
///
/// Neither copy is exact. `-discard nokey` lets the occasional non-key
/// packet through and a filter drops what it is told to, so a sink has
/// to be right when it is handed more than it asked for. Both of these
/// answer the same records as the whole file does.
#[test]
fn records_reads_a_keyframe_copy_made_with_the_bitstream_filter() {
    let Some(_) = sidecar() else { return };
    for codec in Codec::every() {
        let name = codec.extension();
        let mp4 = fixtures()[name].clone();
        let wanted = sink_rows("records", &as_nut(&mp4, &format!("{name}-full.nut"), false));

        let mkv = at(&format!("woven-{name}.mkv"));
        ffmpeg(&["-i", &text(&mp4), "-c", "copy", &text(&mkv)]);
        let keys = at(&format!("{name}-drop.nut"));
        ffmpeg(&[
            "-i",
            &text(&mkv),
            "-c",
            "copy",
            "-bsf:v",
            "noise=drop=not(key)",
            "-f",
            "nut",
            &text(&keys),
        ]);
        let found = sink_rows("records", &keys);
        assert_eq!(
            found.len(),
            wanted.len(),
            "{name}: the filtered copy gave {} of {} records",
            found.len(),
            wanted.len()
        );
        // Matroska will not write a negative timestamp, so this copy
        // sits at a different place on the clock from the MP4's. Every
        // record is the same record and every span the same length,
        // one constant shift apart.
        let shift: f64 = found[0]["start_t"].parse::<f64>().expect("a time")
            - wanted[0]["start_t"].parse::<f64>().expect("a time");
        for (row, want) in found.iter().zip(&wanted) {
            assert_eq!(row["record_id"], want["record_id"], "{name}");
            assert_eq!(row["planes"], want["planes"], "{name}");
            assert_eq!(row["vector"], want["vector"], "{name}");
            for field in ["start_t", "end_t"] {
                let got: f64 = row[field].parse().expect("a time");
                let expected: f64 = want[field].parse().expect("a time");
                assert!(
                    close(got - expected, shift, 0.002),
                    "{name}: {field} is {got}, the whole file says {expected},                      and the first row was {shift} apart"
                );
            }
        }
    }
}
