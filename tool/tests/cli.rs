//! The tool, run as a program, over the committed fixtures.
//!
//! The fixtures live with the library that has the byte-level tests
//! (`core/tests/data`, with the ffmpeg commands that made them in
//! `core/tests/reference.rs`). What is tested here is the shell around
//! it: the rows in, the rows out, the flags, and what the tool says
//! when it will not do something.

use std::path::PathBuf;
use std::process::Command;

const TOOL: &str = env!("CARGO_BIN_EXE_ffrwd-index");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../core/tests/data")
        .join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ffrwd-index-tool-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Runs the tool and gives back its exit code, output and complaints.
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

/// The rows the tests weave: one layered space and one float space.
fn rows() -> String {
    let mut out = String::new();
    out.push_str(
        r#"{"space":{"id":1,"dims":8,"encoding":"i8","unit_length":true,"modality":"picture","model":"test:layered","query":"test:layered-text","model_hash":"000102030405060708090a0b0c0d0e0f","producer":"the cli test"}}"#,
    );
    out.push('\n');
    out.push_str(
        r#"{"space":{"id":2,"dims":4,"encoding":"f32","modality":"description","model":"test:floats"}}"#,
    );
    out.push('\n');
    for index in 0..5 {
        let start = index * 300;
        let values: Vec<String> = (0..8)
            .map(|k| format!("{:.4}", ((index * 7 + k) as f32 * 0.31).sin()))
            .collect();
        out.push_str(&format!(
            r#"{{"space_id":1,"start_ms":{start},"end_ms":{},"vector":[{}]}}"#,
            start + 250,
            values.join(",")
        ));
        out.push('\n');
    }
    for index in 0..2 {
        let start = index * 700;
        out.push_str(&format!(
            r#"{{"space_id":2,"start_ms":{start},"end_ms":{},"vector":[0.5,-0.25,0,1]}}"#,
            start + 600
        ));
        out.push('\n');
    }
    out
}

/// One row of output, as name and value pairs of text.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("\"{name}\":");
    let at = line.find(&needle)? + needle.len();
    let rest = &line[at..];
    let end = rest
        .find([',', '}'])
        .filter(|_| !rest.starts_with('[') && !rest.starts_with('{'));
    match end {
        Some(end) => Some(&rest[..end]),
        None => {
            // An array or an object: find its close.
            let open = rest.chars().next()?;
            let close = if open == '[' { ']' } else { '}' };
            let end = rest.find(close)? + 1;
            Some(&rest[..end])
        }
    }
}

#[test]
fn rows_woven_in_come_back_out() {
    for placement in ["keyframe", "next", "spread:80"] {
        let dir = scratch(&format!("round-{}", placement.replace(':', "-")));
        let rows_path = dir.join("rows.ndjson");
        let woven = dir.join("woven.h264");
        std::fs::write(&rows_path, rows()).expect("the rows");

        let (code, _, told) = tool(&[
            "weave",
            "--video",
            fixture("ref.h264").to_str().expect("a path"),
            "--vectors",
            rows_path.to_str().expect("a path"),
            "--out",
            woven.to_str().expect("a path"),
            "--placement",
            placement,
        ]);
        assert_eq!(code, 0, "{placement}: {told}");
        assert!(told.contains("7 records in 2 spaces"), "{told}");

        let (code, printed, told) = tool(&["read", "--video", woven.to_str().expect("a path")]);
        assert_eq!(code, 0, "{placement}: {told}");
        let lines: Vec<&str> = printed.lines().collect();
        assert_eq!(lines.len(), 9, "{placement}: two spaces and seven records");

        // The spaces come back with every field they went in with.
        assert!(lines[0].contains(r#""id":1"#));
        assert!(lines[0].contains(r#""encoding":"i8""#));
        assert!(lines[0].contains(r#""unit_length":true"#));
        assert!(lines[0].contains(r#""modality":"picture""#));
        assert!(lines[0].contains(r#""model":"test:layered""#));
        assert!(lines[0].contains(r#""query":"test:layered-text""#));
        assert!(lines[0].contains(r#""model_hash":"000102030405060708090a0b0c0d0e0f""#));
        assert!(lines[0].contains(r#""producer":"the cli test""#));
        assert!(lines[1].contains(r#""encoding":"f32""#));

        // Every layered record arrives with all eight planes, whatever
        // the policy did with them on the way.
        for line in &lines[2..] {
            if field(line, "space_id") == Some("1") {
                assert_eq!(
                    field(line, "planes"),
                    Some("[0,1,2,3,4,5,6,7]"),
                    "{placement}: {line}"
                );
            }
        }
        // The spans are the spans the rows asked for.
        let spans: Vec<(String, String)> = lines[2..]
            .iter()
            .filter(|line| field(line, "space_id") == Some("1"))
            .map(|line| {
                (
                    field(line, "start_ms").expect("a start").to_string(),
                    field(line, "end_ms").expect("an end").to_string(),
                )
            })
            .collect();
        assert_eq!(
            spans,
            vec![
                ("0".into(), "250".into()),
                ("300".into(), "550".into()),
                ("600".into(), "850".into()),
                ("900".into(), "1150".into()),
                ("1200".into(), "1450".into()),
            ],
            "{placement}"
        );
        // A float vector is not quantized, so it comes back exactly.
        let floats: Vec<&str> = lines[2..]
            .iter()
            .filter(|line| field(line, "space_id") == Some("2"))
            .map(|line| field(line, "vector").expect("a vector"))
            .collect();
        assert_eq!(
            floats,
            vec!["[0.5,-0.25,0,1]", "[0.5,-0.25,0,1]"],
            "{placement}"
        );
    }
}

#[test]
fn the_woven_stream_is_the_original_with_more_in_it() {
    let dir = scratch("added");
    let rows_path = dir.join("rows.ndjson");
    let woven = dir.join("woven.h264");
    std::fs::write(&rows_path, rows()).expect("the rows");
    let (code, _, told) = tool(&[
        "weave",
        "--video",
        fixture("ref.h264").to_str().expect("a path"),
        "--vectors",
        rows_path.to_str().expect("a path"),
        "--out",
        woven.to_str().expect("a path"),
    ]);
    assert_eq!(code, 0, "{told}");
    let before = std::fs::read(fixture("ref.h264")).expect("the fixture");
    let after = std::fs::read(&woven).expect("the woven stream");
    assert!(after.len() > before.len());
    // Reading the original finds nothing, and says so by printing
    // nothing at all.
    let (code, printed, _) = tool(&[
        "read",
        "--video",
        fixture("ref.h264").to_str().expect("a path"),
    ]);
    assert_eq!(code, 0);
    assert!(printed.is_empty(), "the fixture already had records in it");
}

#[test]
fn hevc_is_woven_and_read_the_same_way() {
    let dir = scratch("hevc");
    let rows_path = dir.join("rows.ndjson");
    let woven = dir.join("woven.h265");
    std::fs::write(&rows_path, rows()).expect("the rows");
    let (code, _, told) = tool(&[
        "weave",
        "--video",
        fixture("ref.h265").to_str().expect("a path"),
        "--vectors",
        rows_path.to_str().expect("a path"),
        "--out",
        woven.to_str().expect("a path"),
        "--placement",
        "next",
    ]);
    assert_eq!(code, 0, "{told}");
    let (code, printed, told) = tool(&["read", "--video", woven.to_str().expect("a path")]);
    assert_eq!(code, 0, "{told}");
    assert_eq!(printed.lines().count(), 9);
}

#[test]
fn the_index_holds_what_the_stream_holds() {
    let dir = scratch("index");
    let rows_path = dir.join("rows.ndjson");
    let woven = dir.join("woven.h264");
    let index = dir.join("out.ffix");
    std::fs::write(&rows_path, rows()).expect("the rows");
    tool(&[
        "weave",
        "--video",
        fixture("ref.h264").to_str().expect("a path"),
        "--vectors",
        rows_path.to_str().expect("a path"),
        "--out",
        woven.to_str().expect("a path"),
        "--placement",
        "spread:64",
    ]);
    let (code, from_stream, told) = tool(&[
        "read",
        "--video",
        woven.to_str().expect("a path"),
        "--index",
        index.to_str().expect("a path"),
    ]);
    assert_eq!(code, 0, "{told}");
    assert!(told.contains("9 entries"), "{told}");

    let written = std::fs::read(&index).expect("the index");
    assert_eq!(&written[..4], b"FFIX");

    let (code, from_index, told) = tool(&["read", "--index", index.to_str().expect("a path")]);
    assert_eq!(code, 0, "{told}");
    assert_eq!(from_index.lines().count(), 9);

    // The same records, span for span and component for component.
    let spans = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| line.contains("\"vector\":"))
            .map(|line| {
                format!(
                    "{} {} {} {}",
                    field(line, "space_id").expect("a space"),
                    field(line, "start_ms").expect("a start"),
                    field(line, "end_ms").expect("an end"),
                    field(line, "vector").expect("a vector"),
                )
            })
            .collect()
    };
    assert_eq!(spans(&from_index), spans(&from_stream));
}

#[test]
fn the_tool_says_what_it_will_not_do() {
    let dir = scratch("refusals");
    let rows_path = dir.join("rows.ndjson");
    std::fs::write(
        &rows_path,
        "{\"space_id\":1,\"start_ms\":0,\"end_ms\":1,\"vector\":[1]}\n",
    )
    .expect("the rows");
    let video = fixture("ref.h264");
    let video = video.to_str().expect("a path");
    let out = dir.join("out.h264");
    let out = out.to_str().expect("a path");
    let rows_path = rows_path.to_str().expect("a path");

    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["dance"], "is not a command"),
        (vec!["weave", "--video", video], "--vectors is needed"),
        (
            vec![
                "weave",
                "--video",
                video,
                "--vectors",
                rows_path,
                "--out",
                out,
            ],
            "has not been declared",
        ),
        (
            vec![
                "weave",
                "--video",
                video,
                "--vectors",
                rows_path,
                "--out",
                out,
                "--placement",
                "everywhere",
            ],
            "is not keyframe",
        ),
        (
            vec![
                "weave",
                "--video",
                "nothing.h264",
                "--vectors",
                rows_path,
                "--out",
                out,
            ],
            "nothing.h264",
        ),
        (vec!["read"], "wants --video or --index"),
        (vec!["read", "--video", "a.mov"], "does not name its codec"),
        (vec!["read", "--index", "nothing.ffix"], "nothing.ffix"),
        (
            vec!["read", "--video", video, "--colour", "blue"],
            "not a flag",
        ),
    ];
    for (args, wanted) in cases {
        let (code, _, told) = tool(&args);
        assert_eq!(code, 2, "{args:?} did not refuse");
        assert!(told.contains(wanted), "{args:?} said {told}");
    }

    let (code, printed, _) = tool(&["--help"]);
    assert_eq!(code, 0);
    assert!(printed.contains("ffrwd-index weave"));
}

#[test]
fn a_cut_of_a_woven_file_reads_from_its_first_frame() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
    {
        println!("skipping the cut: ffmpeg is not on the PATH");
        return;
    }
    let dir = scratch("cut");
    let rows_path = dir.join("rows.ndjson");
    let woven = dir.join("woven.h264");
    std::fs::write(&rows_path, rows()).expect("the rows");
    // The `next` policy puts records on ordinary frames, so a cut at
    // the keyframe one second in leaves some of them behind and keeps
    // the rest. Nothing else is asked for: the spaces go on every
    // keyframe whatever the policy is.
    let (code, _, told) = tool(&[
        "weave",
        "--video",
        fixture("ref.h264").to_str().expect("a path"),
        "--vectors",
        rows_path.to_str().expect("a path"),
        "--out",
        woven.to_str().expect("a path"),
        "--placement",
        "next",
    ]);
    assert_eq!(code, 0, "{told}");

    let ffmpeg = |args: &[&str]| {
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
    };
    let whole = dir.join("whole.mp4");
    let cut = dir.join("cut.mp4");
    let back = dir.join("cut.h264");
    ffmpeg(&[
        "-i",
        woven.to_str().expect("a path"),
        "-c",
        "copy",
        "-f",
        "mp4",
        whole.to_str().expect("a path"),
    ]);
    ffmpeg(&[
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
    ffmpeg(&[
        "-i",
        cut.to_str().expect("a path"),
        "-c",
        "copy",
        "-bsf:v",
        "h264_mp4toannexb",
        "-f",
        "h264",
        back.to_str().expect("a path"),
    ]);

    let (code, printed, told) = tool(&["read", "--video", back.to_str().expect("a path")]);
    assert_eq!(code, 0, "{told}");
    assert!(!told.contains("dropped"), "{told}");
    let spaces = printed
        .lines()
        .filter(|line| line.contains("\"space\":"))
        .count();
    let records = printed
        .lines()
        .filter(|line| line.contains("\"vector\":"))
        .count();
    assert_eq!(spaces, 2, "the cut lost a space declaration:\n{printed}");
    assert!(records > 0, "the cut kept no records");
    assert!(
        records < 7,
        "the cut kept all seven records, so it cut nothing"
    );
    // The records the cut kept are the ones whose carriers it kept, and
    // their spans have moved with the file's own zero.
    for line in printed.lines().filter(|line| line.contains("\"vector\":")) {
        let start: i64 = field(line, "start_ms")
            .and_then(|value| value.parse().ok())
            .expect("a start");
        assert!(start < 500, "a span was not moved by the cut: {line}");
    }
}

#[test]
fn a_frame_rate_moves_every_span_with_it() {
    // The stream carries no timestamps, so reading at a rate other than
    // the one that was woven moves the spans by that ratio. The tool
    // has to be consistent, not clairvoyant.
    let dir = scratch("fps");
    let rows_path = dir.join("rows.ndjson");
    let woven = dir.join("woven.h264");
    std::fs::write(&rows_path, rows()).expect("the rows");
    tool(&[
        "weave",
        "--video",
        fixture("ref.h264").to_str().expect("a path"),
        "--vectors",
        rows_path.to_str().expect("a path"),
        "--out",
        woven.to_str().expect("a path"),
        "--fps",
        "60",
    ]);
    let (code, at_sixty, _) = tool(&[
        "read",
        "--video",
        woven.to_str().expect("a path"),
        "--fps",
        "60",
    ]);
    assert_eq!(code, 0);
    let (code, at_thirty, _) = tool(&["read", "--video", woven.to_str().expect("a path")]);
    assert_eq!(code, 0);

    let first = |text: &str| -> i64 {
        text.lines()
            .find(|line| line.contains("\"vector\":"))
            .and_then(|line| field(line, "carrier_ms"))
            .and_then(|value| value.parse().ok())
            .expect("a carrier time")
    };
    assert_eq!(first(&at_sixty) * 2, first(&at_thirty));
}
