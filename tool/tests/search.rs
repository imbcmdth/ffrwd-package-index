//! `search`, run as a program over streams and files.
//!
//! The thing worth testing about a ranking is that it is the ranking
//! somebody would get by hand, so every test here computes its own
//! answer from what `read` prints and compares. `read` reconstructs a
//! record from the planes that arrived, which is exactly what a search
//! scores, so a brute force over its output is the truth a search is
//! held to.

use std::path::PathBuf;
use std::process::Command;

const TOOL: &str = env!("CARGO_BIN_EXE_ffrwd-index");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../core/tests/data")
        .join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ffrwd-index-search-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

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

fn text(path: &std::path::Path) -> String {
    path.to_str().expect("a path").to_string()
}

// ---------------------------------------------------------------- //
// Rows in and rows out.
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

fn numbers(text: &str) -> Vec<f64> {
    text.trim_matches(|letter| letter == '[' || letter == ']')
        .split(',')
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().parse().expect("a number"))
        .collect()
}

/// A vector of the test's own making: deterministic, and different
/// enough from record to record that a ranking has something to say.
fn vector(seed: usize, dims: usize) -> Vec<f64> {
    (0..dims)
        .map(|k| ((seed * 7 + k * 3) as f64 * 0.31).sin())
        .collect()
}

fn list(values: &[f64]) -> String {
    values
        .iter()
        .map(|value| format!("{value:.5}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Two spaces with different models, so a search has to pick one, and
/// twelve records in the first of them, so a ranking has an order.
fn rows() -> String {
    let mut out = String::new();
    out.push_str(
        r#"{"space":{"id":1,"dims":16,"encoding":"i8","modality":"picture","model":"hf:example/xclip@rev1/video_tower.onnx","query":"hf:example/xclip@rev1/text_tower.onnx","producer":"the search test"}}"#,
    );
    out.push('\n');
    out.push_str(
        r#"{"space":{"id":2,"dims":16,"encoding":"i8","modality":"speech","model":"hf:example/all-MiniLM-L6-v2@rev2/model.onnx","producer":"the search test"}}"#,
    );
    out.push('\n');
    for index in 0..12 {
        let start = index * 120;
        out.push_str(&format!(
            r#"{{"space_id":1,"start_ms":{start},"end_ms":{},"vector":[{}]}}"#,
            start + 100,
            list(&vector(index, 16))
        ));
        out.push('\n');
    }
    for index in 0..3 {
        let start = index * 500;
        out.push_str(&format!(
            r#"{{"space_id":2,"start_ms":{start},"end_ms":{},"vector":[{}]}}"#,
            start + 400,
            list(&vector(index + 40, 16))
        ));
        out.push('\n');
    }
    out
}

/// A stream with those rows woven in, and a query file beside it.
fn woven(name: &str, placement: &str) -> (PathBuf, PathBuf) {
    let dir = scratch(name);
    let rows_path = dir.join("rows.ndjson");
    let stream = dir.join("woven.h264");
    std::fs::write(&rows_path, rows()).expect("the rows");
    ok(&[
        "weave",
        "--video",
        &text(&fixture("ref.h264")),
        "--vectors",
        &text(&rows_path),
        "--out",
        &text(&stream),
        "--placement",
        placement,
    ]);
    // A query near record 5 but not equal to it, so the best match is
    // an answer and not an identity.
    let query: Vec<f64> = vector(5, 16)
        .iter()
        .enumerate()
        .map(|(k, value)| value + (k as f64 * 0.017).cos() * 0.05)
        .collect();
    let query_path = dir.join("query.json");
    std::fs::write(&query_path, format!("[{}]", list(&query))).expect("the query");
    (stream, query_path)
}

/// Every record `read` printed, as its id and the vector it came back
/// as: the reconstruction a search scores.
fn read_vectors(printed: &str, space_id: &str) -> Vec<(i64, Vec<f64>)> {
    printed
        .lines()
        .filter(|line| line.contains("\"vector\":"))
        .filter(|line| field(line, "space_id") == Some(space_id))
        .map(|line| {
            (
                field(line, "record_id")
                    .and_then(|value| value.parse().ok())
                    .expect("a record id"),
                numbers(field(line, "vector").expect("a vector")),
            )
        })
        .collect()
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let left: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let right: f64 = b.iter().map(|y| y * y).sum::<f64>().sqrt();
    if left * right <= 0.0 {
        return 0.0;
    }
    dot / (left * right)
}

/// The ids a search printed, best first.
fn ranked(printed: &str) -> Vec<i64> {
    printed
        .lines()
        .map(|line| {
            field(line, "record_id")
                .and_then(|value| value.parse().ok())
                .expect("a record id")
        })
        .collect()
}

fn scores(printed: &str) -> Vec<f64> {
    printed
        .lines()
        .map(|line| {
            field(line, "score")
                .and_then(|value| value.parse().ok())
                .expect("a score")
        })
        .collect()
}

// ---------------------------------------------------------------- //
// The tests.
// ---------------------------------------------------------------- //

#[test]
fn the_ranking_is_the_brute_force_over_the_reconstructed_vectors() {
    let (stream, query) = woven("brute", "keyframe");
    let (printed, _) = ok(&["read", "--video", &text(&stream)]);
    let records = read_vectors(&printed, "1");
    assert_eq!(records.len(), 12);
    let wanted = numbers(&std::fs::read_to_string(&query).expect("the query"));

    let mut brute: Vec<(i64, f64)> = records
        .iter()
        .map(|(id, values)| (*id, cosine(values, &wanted)))
        .collect();
    brute.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

    let (found, told) = ok(&[
        "search",
        "--video",
        &text(&stream),
        "--space",
        "1",
        "--query",
        &text(&query),
        "--top",
        "12",
    ]);
    assert_eq!(
        ranked(&found),
        brute.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        "{found}"
    );
    for (index, score) in scores(&found).iter().enumerate() {
        assert!(
            (score - brute[index].1).abs() < 1e-5,
            "record {}: {score} is not {}",
            brute[index].0,
            brute[index].1
        );
    }
    // The stderr says what was read and what must embed a prompt for
    // this space, which is the one thing a searcher cannot work out for
    // itself.
    assert!(told.contains("12 of 15 records scored"), "{told}");
    assert!(told.contains("text_tower.onnx"), "{told}");
}

#[test]
fn a_space_is_chosen_by_a_model_uri_as_well_as_by_its_id() {
    let (stream, query) = woven("by-uri", "keyframe");
    let by_id = ok(&[
        "search",
        "--video",
        &text(&stream),
        "--space",
        "2",
        "--query",
        &text(&query),
        "--top",
        "3",
    ])
    .0;
    let by_uri = ok(&[
        "search",
        "--video",
        &text(&stream),
        "--space",
        "MiniLM",
        "--query",
        &text(&query),
        "--top",
        "3",
    ])
    .0;
    assert_eq!(by_id, by_uri);
    assert_eq!(by_uri.lines().count(), 3);
    assert!(by_uri.lines().all(|line| line.contains("\"space\":2")));

    // The video tower and the text tower are two URIs of one space, and
    // either names it.
    let by_tower = ok(&[
        "search",
        "--video",
        &text(&stream),
        "--space",
        "text_tower",
        "--query",
        &text(&query),
        "--top",
        "1",
    ])
    .0;
    assert!(by_tower.contains("\"space\":1"), "{by_tower}");

    // A name both spaces answer to is a refusal that lists them, not a
    // guess: a search of the wrong space looks like one that found
    // nothing.
    let (code, _, told) = tool(&[
        "search",
        "--video",
        &text(&stream),
        "--space",
        "the search test",
        "--query",
        &text(&query),
    ]);
    assert_eq!(code, 2);
    assert!(told.contains("names 2 of this file's spaces"), "{told}");
    let (code, _, told) = tool(&[
        "search",
        "--video",
        &text(&stream),
        "--query",
        &text(&query),
    ]);
    assert_eq!(code, 2);
    assert!(told.contains("--space has to say which"), "{told}");
}

#[test]
fn a_threshold_and_a_top_cut_the_same_ordered_list() {
    let (stream, query) = woven("cuts", "keyframe");
    let search = |extra: &[&str]| -> String {
        let mut args: Vec<String> = [
            "search",
            "--video",
            &text(&stream),
            "--space",
            "1",
            "--query",
            &text(&query),
        ]
        .iter()
        .map(|value| value.to_string())
        .collect();
        args.extend(extra.iter().map(|value| value.to_string()));
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        ok(&borrowed).0
    };
    let everything = search(&["--top", "12"]);
    let all_scores = scores(&everything);
    assert_eq!(all_scores.len(), 12);

    // Neither flag is the ten best, which is the default.
    assert_eq!(search(&[]).lines().count(), 10);

    // A threshold just under the fourth score keeps at least four. Just
    // under, because a printed score is rounded to six places and the
    // number it was rounded from may sit either side of that.
    let level = all_scores[3] - 1e-5;
    let over = search(&["--threshold", &format!("{level:.6}")]);
    assert!(over.lines().count() >= 4, "{over}");
    assert!(scores(&over).iter().all(|score| *score >= level - 1e-6));
    assert_eq!(
        ranked(&over),
        ranked(&everything)[..over.lines().count()].to_vec()
    );

    // The two together take the shorter of the two prefixes, whichever
    // way round they are.
    let both = search(&["--threshold", &format!("{level:.6}"), "--top", "2"]);
    assert_eq!(both.lines().count(), 2);
    let generous = search(&["--threshold", "-1", "--top", "5"]);
    assert_eq!(generous.lines().count(), 5);
    // A threshold above everything keeps nothing, and that is not an
    // error: a search that found nothing has found nothing.
    let (code, nothing, _) = tool(&[
        "search",
        "--video",
        &text(&stream),
        "--space",
        "1",
        "--query",
        &text(&query),
        "--threshold",
        "0.999999",
    ]);
    assert_eq!(code, 0);
    assert!(nothing.is_empty(), "{nothing}");
}

#[test]
fn a_coarse_prefilter_is_a_subset_of_the_same_order() {
    let (stream, query) = woven("coarse", "keyframe");
    let search = |extra: &[&str]| -> String {
        let mut args: Vec<String> = [
            "search",
            "--video",
            &text(&stream),
            "--space",
            "1",
            "--query",
            &text(&query),
            "--top",
            "12",
        ]
        .iter()
        .map(|value| value.to_string())
        .collect();
        args.extend(extra.iter().map(|value| value.to_string()));
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        ok(&borrowed).0
    };
    let whole = search(&[]);
    // A prefilter wide enough to keep everything is the whole search.
    assert_eq!(search(&["--coarse", "12"]), whole);
    assert_eq!(search(&["--coarse", "99"]), whole);

    // A narrow one keeps a subset, and puts it in the order the full
    // rescore would have: the sign planes choose who is rescored and
    // nothing else.
    let narrow = search(&["--coarse", "5"]);
    assert_eq!(narrow.lines().count(), 5);
    let kept = ranked(&narrow);
    let order: Vec<i64> = ranked(&whole)
        .into_iter()
        .filter(|id| kept.contains(id))
        .collect();
    assert_eq!(kept, order, "{narrow}");
    // And the scores printed are the rescored ones, not the coarse
    // ones, so a threshold means the same thing either way.
    let full = scores(&whole);
    let by_id: Vec<(i64, f64)> = ranked(&whole).into_iter().zip(full).collect();
    for (id, score) in kept.iter().zip(scores(&narrow)) {
        let wanted = by_id
            .iter()
            .find(|(other, _)| other == id)
            .expect("a record")
            .1;
        assert!((score - wanted).abs() < 1e-6, "record {id}: {score}");
    }
    // The best record is one the sign planes found, which is what makes
    // the stage worth having at all.
    assert_eq!(kept[0], ranked(&whole)[0]);
}

#[test]
fn a_file_index_and_a_scan_give_the_same_ranking() {
    if !have_ffmpeg() {
        println!("skipping the container path: ffmpeg is not on the PATH");
        return;
    }
    let dir = scratch("index-vs-scan");
    let (stream, query) = woven("index-vs-scan-stream", "keyframe");
    let file = dir.join("woven.mp4");
    ffmpeg(&[
        "-i",
        &text(&stream),
        "-c",
        "copy",
        "-f",
        "mp4",
        &text(&file),
    ]);

    let search = |extra: &[&str]| -> (String, String) {
        let mut args: Vec<String> = [
            "search",
            "--mp4",
            &text(&file),
            "--space",
            "1",
            "--query",
            &text(&query),
            "--top",
            "12",
        ]
        .iter()
        .map(|value| value.to_string())
        .collect();
        args.extend(extra.iter().map(|value| value.to_string()));
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        ok(&borrowed)
    };

    // With no index in the file, the keyframe scan is what happens, and
    // it is the same answer as a full scan because the records rode
    // keyframes.
    let (by_keyframes, told) = search(&[]);
    assert!(told.contains("scan keyframes"), "{told}");
    let (by_all, told) = search(&["--scan", "all"]);
    assert!(told.contains("scan all"), "{told}");
    assert_eq!(by_keyframes, by_all);

    // With one, it is read instead, in a handful of bytes.
    ok(&["index", &text(&file)]);
    let (by_index, told) = search(&[]);
    assert!(told.contains("the file's own index"), "{told}");
    assert_eq!(by_index, by_keyframes, "{told}");
    // And asking for a scan still reads the stream, which is the
    // authority the index is a copy of.
    let (still_scanned, told) = search(&["--scan", "all"]);
    assert!(told.contains("scan all"), "{told}");
    assert_eq!(still_scanned, by_index);

    // The index on its own, and the same file named as an index, are
    // the same rows again.
    let sidecar = dir.join("out.ffix");
    ok(&[
        "read",
        "--video",
        &text(&stream),
        "--index",
        &text(&sidecar),
    ]);
    let alone = ok(&[
        "search",
        "--index",
        &text(&sidecar),
        "--space",
        "1",
        "--query",
        &text(&query),
        "--top",
        "12",
    ])
    .0;
    assert_eq!(ranked(&alone), ranked(&by_index));
    let in_file = ok(&[
        "search",
        "--index",
        &text(&file),
        "--space",
        "1",
        "--query",
        &text(&query),
        "--top",
        "12",
    ])
    .0;
    assert_eq!(in_file, by_index);
}

#[test]
fn a_spread_record_read_off_a_container_ranks_as_it_does_off_the_stream() {
    if !have_ffmpeg() {
        println!("skipping the container path: ffmpeg is not on the PATH");
        return;
    }
    // `spread` puts a record's planes on several carriers, so this is
    // the case where a scan has to put a record back together before
    // anything can be ranked.
    let dir = scratch("spread");
    let (stream, query) = woven("spread-stream", "spread:48");
    let file = dir.join("woven.mp4");
    ffmpeg(&[
        "-i",
        &text(&stream),
        "-c",
        "copy",
        "-f",
        "mp4",
        &text(&file),
    ]);
    let from_stream = ok(&[
        "search",
        "--video",
        &text(&stream),
        "--space",
        "1",
        "--query",
        &text(&query),
        "--top",
        "12",
    ])
    .0;
    let from_file = ok(&[
        "search",
        "--mp4",
        &text(&file),
        "--scan",
        "all",
        "--space",
        "1",
        "--query",
        &text(&query),
        "--top",
        "12",
    ])
    .0;
    assert_eq!(ranked(&from_file), ranked(&from_stream));
    assert!(
        from_stream.contains("\"planes\":[0,1,2,3,4,5,6,7]"),
        "{from_stream}"
    );
}

#[test]
fn a_query_arrives_as_numbers_as_base64_or_on_standard_input() {
    let (stream, query) = woven("query-shapes", "keyframe");
    let dir = query.parent().expect("a directory").to_path_buf();
    let values = numbers(&std::fs::read_to_string(&query).expect("the query"));
    let by_array = ok(&[
        "search",
        "--video",
        &text(&stream),
        "--space",
        "1",
        "--query",
        &text(&query),
        "--top",
        "3",
    ])
    .0;

    // The same numbers as little-endian binary32, base64: the shape a
    // vector track carries and a row may use.
    const DIGITS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = values
        .iter()
        .flat_map(|value| (*value as f32).to_le_bytes())
        .collect();
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let mut held = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            held |= u32::from(*byte) << (16 - index * 8);
        }
        for index in 0..chunk.len() + 1 {
            encoded.push(DIGITS[(held >> (18 - index * 6) & 0x3f) as usize] as char);
        }
        for _ in chunk.len() + 1..4 {
            encoded.push('=');
        }
    }
    let as_base64 = dir.join("query.b64.json");
    std::fs::write(&as_base64, format!("{{\"vector\":\"{encoded}\"}}")).expect("a query");
    let by_base64 = ok(&[
        "search",
        "--video",
        &text(&stream),
        "--space",
        "1",
        "--query",
        &text(&as_base64),
        "--top",
        "3",
    ])
    .0;
    assert_eq!(by_base64, by_array);

    // And on standard input, which is what a query straight out of
    // another program looks like.
    let child = Command::new(TOOL)
        .args([
            "search",
            "--video",
            &text(&stream),
            "--space",
            "1",
            "--query",
            "-",
            "--top",
            "3",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the tool runs");
    {
        use std::io::Write;
        let mut stdin = child.stdin.as_ref().expect("a pipe");
        stdin
            .write_all(
                std::fs::read_to_string(&query)
                    .expect("the query")
                    .as_bytes(),
            )
            .expect("the query goes in");
    }
    let output = child.wait_with_output().expect("the tool finishes");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), by_array);
}

#[test]
fn search_says_what_it_will_not_do() {
    let (stream, query) = woven("refusals", "keyframe");
    let dir = query.parent().expect("a directory").to_path_buf();
    let short = dir.join("short.json");
    std::fs::write(&short, "[1,2,3]").expect("a query");
    let floats = dir.join("floats.ndjson");
    std::fs::write(
        &floats,
        concat!(
            r#"{"space":{"id":1,"dims":4,"encoding":"f32","model":"test:floats"}}"#,
            "\n",
            r#"{"space_id":1,"start_ms":0,"end_ms":100,"vector":[1,0,0,0]}"#,
            "\n"
        ),
    )
    .expect("the rows");
    let float_stream = dir.join("floats.h264");
    ok(&[
        "weave",
        "--video",
        &text(&fixture("ref.h264")),
        "--vectors",
        &text(&floats),
        "--out",
        &text(&float_stream),
    ]);
    let float_query = dir.join("float-query.json");
    std::fs::write(&float_query, "[1,0,0,0]").expect("a query");

    let stream = text(&stream);
    let query = text(&query);
    let short = text(&short);
    let float_stream = text(&float_stream);
    let float_query = text(&float_query);
    let plain = text(&fixture("ref.h264"));

    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["search", "--video", &stream], "--query is needed"),
        (
            vec!["search", "--query", &query],
            "one of --video, --mp4, --mkv, --index and --rows",
        ),
        (
            vec![
                "search",
                "--video",
                &stream,
                "--mp4",
                "nothing.mp4",
                "--query",
                &query,
            ],
            "one of --video, --mp4, --mkv, --index and --rows",
        ),
        (
            vec![
                "search", "--video", &stream, "--space", "1", "--query", &short,
            ],
            "a query of 3 components",
        ),
        (
            vec![
                "search", "--video", &stream, "--space", "9", "--query", &query,
            ],
            "no space of this file is 9",
        ),
        (
            vec![
                "search", "--video", &stream, "--space", "1", "--query", &query, "--top", "lots",
            ],
            "not a number of results",
        ),
        (
            vec![
                "search", "--video", &stream, "--space", "1", "--query", &query, "--scan", "all",
            ],
            "--scan belongs to --mp4 and --mkv",
        ),
        (
            vec![
                "search",
                "--video",
                &float_stream,
                "--query",
                &float_query,
                "--coarse",
                "1",
            ],
            "sign plane",
        ),
        (
            vec!["search", "--video", &plain, "--query", &query],
            "declares no space",
        ),
    ];
    for (args, wanted) in cases {
        let (code, _, told) = tool(&args);
        assert_eq!(code, 2, "{args:?} did not refuse");
        assert!(told.contains(wanted), "{args:?} said {told}");
    }
}

#[test]
fn the_rows_are_the_search_the_encoding_is_measured_against() {
    // `--rows` ranks the numbers a weave was handed, so the two runs
    // here are the same query against the same vectors with and without
    // the layered encoding in between. What they agree about is the
    // answer; what they differ by is what the encoding cost.
    let dir = scratch("rows");
    let rows_path = dir.join("rows.ndjson");
    let stream = dir.join("woven.h264");
    std::fs::write(&rows_path, rows()).expect("the rows");
    ok(&[
        "weave",
        "--video",
        &text(&fixture("ref.h264")),
        "--vectors",
        &text(&rows_path),
        "--out",
        &text(&stream),
    ]);
    let query = dir.join("query.json");
    std::fs::write(&query, format!("[{}]", list(&vector(5, 16)))).expect("the query");

    let (unencoded, told) = ok(&[
        "search",
        "--rows",
        &text(&rows_path),
        "--space",
        "1",
        "--query",
        &text(&query),
        "--top",
        "4",
    ]);
    assert!(told.contains("no encoding applied"), "{told}");
    let (from_file, _) = ok(&[
        "search",
        "--video",
        &text(&stream),
        "--space",
        "1",
        "--query",
        &text(&query),
        "--top",
        "4",
    ]);
    assert_eq!(ranked(&unencoded), ranked(&from_file), "{unencoded}");
    // The unencoded search scores a vector against itself exactly; the
    // one out of the stream has been through eight bit-planes and two
    // escapes and comes back very slightly short.
    let exact = scores(&unencoded)[0];
    let woven_score = scores(&from_file)[0];
    assert!((exact - 1.0).abs() < 1e-6, "{exact}");
    assert!(woven_score < exact, "{woven_score} is not below {exact}");
    assert!(woven_score > 0.99, "{woven_score}");
    // And the rows carry no planes, because nothing was cut into any.
    assert!(!unencoded.contains("\"planes\""), "{unencoded}");
}
