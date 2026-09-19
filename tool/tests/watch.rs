//! `watch` through a real pipe.
//!
//! The claim `watch` makes is not about its output but about when the
//! output appears, so testing it by running it over a file and looking
//! at what came out would test nothing. Every test here spawns the tool
//! as a process, writes the stream into its standard input in paced
//! chunks, and records how many chunks had been written when each row
//! came back. A row that arrives while later carriers are still unsent
//! is the whole point; a row that arrives at the end is what a file
//! reader already does.
//!
//! What cannot be tested here is the other half of the live case: real
//! time. `ffmpeg -re` feeding `ffrwd-index weave` is not possible,
//! because weave works on files. So the stream is woven ahead of time
//! with `--placement next`, which is the policy a live writer uses, and
//! the pipe supplies the pacing.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const TOOL: &str = env!("CARGO_BIN_EXE_ffrwd-index");

/// How long the writer waits after each chunk, so that a row the tool
/// has already written has been read before the next chunk goes in.
const PACE: std::time::Duration = std::time::Duration::from_millis(30);

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../core/tests/data")
        .join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ffrwd-index-watch-{name}"));
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

fn text(path: &std::path::Path) -> String {
    path.to_str().expect("a path").to_string()
}

fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("\"{name}\":");
    let at = line.find(&needle)? + needle.len();
    let rest = &line[at..];
    let open = rest.chars().next()?;
    if open != '[' {
        let end = rest.find([',', '}'])?;
        return Some(&rest[..end]);
    }
    let end = rest.find(']')?;
    Some(&rest[..end + 1])
}

/// A deterministic pseudo-random vector. Random rather than a curve,
/// so that two of them are nearly orthogonal and a query matches the
/// record it was made from and no other.
fn vector(seed: usize, dims: usize) -> Vec<f64> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ (seed as u64).wrapping_mul(0x5851_f42d_4c95_7f2d);
    (0..dims)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f64 / (1u64 << 31) as f64) - 0.5
        })
        .collect()
}

fn list(values: &[f64]) -> String {
    values
        .iter()
        .map(|value| format!("{value:.5}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Six records spread through the two seconds `ref.h264` covers, so a
/// `next` weave puts them on carriers all over the stream.
fn rows(count: usize) -> String {
    let mut out = String::from(
        r#"{"space":{"id":1,"dims":16,"encoding":"i8","modality":"picture","model":"test:layered","query":"test:layered-text","producer":"the watch test"}}"#,
    );
    out.push('\n');
    for index in 0..count {
        let start = index * 300;
        out.push_str(&format!(
            r#"{{"space_id":1,"start_ms":{start},"end_ms":{},"vector":[{}]}}"#,
            start + 200,
            list(&vector(index, 16))
        ));
        out.push('\n');
    }
    out
}

/// The stream every test watches: the reference encode with six records
/// woven in under the live policy, and a query file for each of them.
fn live(name: &str, count: usize) -> (PathBuf, PathBuf) {
    let dir = scratch(name);
    let rows_path = dir.join("rows.ndjson");
    let stream = dir.join("live.h264");
    std::fs::write(&rows_path, rows(count)).expect("the rows");
    ok(&[
        "weave",
        "--video",
        &text(&fixture("ref.h264")),
        "--vectors",
        &text(&rows_path),
        "--out",
        &text(&stream),
        "--placement",
        "next",
    ]);
    for index in 0..count {
        std::fs::write(
            dir.join(format!("q{index}.json")),
            format!("[{}]", list(&vector(index, 16))),
        )
        .expect("a query");
    }
    (dir, stream)
}

/// Where each access unit of an Annex B stream begins.
///
/// The tests cut streams at access unit boundaries, because a cut in
/// the middle of one is a different experiment: this is about what a
/// reader that joined a stream can and cannot read, not about what it
/// does with half a picture. The rule is the one `core` uses, kept
/// short here because the only streams it is asked about are the ones
/// these tests wove.
fn access_units(bytes: &[u8]) -> Vec<usize> {
    let mut nals: Vec<(usize, u8, bool)> = Vec::new();
    let mut at = 0usize;
    while at + 3 < bytes.len() {
        if bytes[at..at + 3] == [0, 0, 1] {
            let code = if at > 0 && bytes[at - 1] == 0 {
                at - 1
            } else {
                at
            };
            let header = bytes[at + 3];
            let first_of_picture = bytes.get(at + 4).is_some_and(|byte| byte & 0x80 != 0);
            nals.push((code, header & 0x1f, first_of_picture));
            at += 3;
        } else {
            at += 1;
        }
    }
    let mut out = Vec::new();
    let mut seen_slice = false;
    for (code, kind, first_of_picture) in nals {
        let slice = (1..=5).contains(&kind);
        let boundary = if slice {
            seen_slice && first_of_picture
        } else {
            seen_slice && matches!(kind, 6..=9 | 13..=18)
        };
        if out.is_empty() || boundary {
            out.push(code);
            seen_slice = false;
        }
        seen_slice |= slice;
    }
    out
}

/// One row, and how many chunks had gone into the pipe when it came
/// back out.
#[derive(Debug)]
struct Timed {
    after_chunks: usize,
    line: String,
}

/// Runs `watch` with `bytes` written to it in `chunks` pieces, and says
/// which rows came back when.
fn paced(args: &[&str], bytes: &[u8], chunks: usize) -> (Vec<Timed>, String) {
    let mut child = Command::new(TOOL)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the tool runs");

    let written = Arc::new(AtomicUsize::new(0));
    let rows: Arc<Mutex<Vec<Timed>>> = Arc::new(Mutex::new(Vec::new()));
    let stdout = child.stdout.take().expect("a pipe");
    let reader = {
        let written = Arc::clone(&written);
        let rows = Arc::clone(&rows);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.expect("a row");
                rows.lock().expect("the rows").push(Timed {
                    after_chunks: written.load(Ordering::SeqCst),
                    line,
                });
            }
        })
    };

    {
        let mut stdin = child.stdin.take().expect("a pipe");
        let size = bytes.len().div_ceil(chunks);
        for (index, chunk) in bytes.chunks(size).enumerate() {
            stdin.write_all(chunk).expect("the chunk goes in");
            stdin.flush().expect("the chunk is sent");
            written.store(index + 1, Ordering::SeqCst);
            std::thread::sleep(PACE);
        }
    }

    reader.join().expect("the reader finishes");
    let output = child.wait_with_output().expect("the tool finishes");
    assert!(
        output.status.success(),
        "watch refused:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let told = String::from_utf8_lossy(&output.stderr).to_string();
    let rows = Arc::try_unwrap(rows)
        .expect("the reader is done")
        .into_inner()
        .expect("the rows");
    (rows, told)
}

// ---------------------------------------------------------------- //
// The tests.
// ---------------------------------------------------------------- //

#[test]
fn a_match_is_printed_before_the_chunks_carrying_later_records() {
    let (dir, stream) = live("incremental", 6);
    let bytes = std::fs::read(&stream).expect("the stream");
    let chunks = 16usize;
    let queries: Vec<String> = (0..6)
        .map(|index| format!("q{index}={}", text(&dir.join(format!("q{index}.json")))))
        .collect();
    let mut args = vec![
        "watch".to_string(),
        "--video".to_string(),
        "-".to_string(),
        "--codec".to_string(),
        "h264".to_string(),
        "--threshold".to_string(),
        "0.97".to_string(),
    ];
    for query in &queries {
        args.push("--query".to_string());
        args.push(query.clone());
    }
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let (rows, told) = paced(&borrowed, &bytes, chunks);

    // Each record matches its own query and nothing else, so there are
    // six rows and they name q0 to q5 in order.
    assert_eq!(rows.len(), 6, "{rows:#?}\n{told}");
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(
            field(&row.line, "query"),
            Some(format!("\"q{index}\"").as_str()),
            "{}",
            row.line
        );
    }

    // The point of the command: each row is out before the chunk that
    // carries the next record has been written. Strictly increasing
    // chunk counts say exactly that, since the rows arrive in the order
    // their carriers do.
    let at: Vec<usize> = rows.iter().map(|row| row.after_chunks).collect();
    for pair in at.windows(2) {
        assert!(
            pair[0] < pair[1],
            "two rows came out of the same chunk: {at:?}\n{told}"
        );
    }
    // And the last of them is out before the stream ends, so nothing
    // here is waiting for the end of a stream that in a live use has
    // none.
    assert!(
        *at.last().expect("a row") < chunks,
        "the last row waited for the whole stream: {at:?}"
    );
    assert!(told.contains("6 matches"), "{told}");
}

#[test]
fn several_queries_each_name_themselves_and_share_the_stream() {
    let (dir, stream) = live("several", 4);
    let bytes = std::fs::read(&stream).expect("the stream");
    // Two of the four queries, and a threshold low enough that each of
    // them matches more than its own record.
    let first = format!("near-zero={}", text(&dir.join("q0.json")));
    let second = format!("near-three={}", text(&dir.join("q3.json")));
    let (rows, told) = paced(
        &[
            "watch",
            "--video",
            "-",
            "--codec",
            "h264",
            "--threshold",
            "0.2",
            "--query",
            &first,
            "--query",
            &second,
        ],
        &bytes,
        12,
    );
    let names: Vec<&str> = rows
        .iter()
        .filter_map(|row| field(&row.line, "query"))
        .collect();
    assert!(names.contains(&"\"near-zero\""), "{names:?}\n{told}");
    assert!(names.contains(&"\"near-three\""), "{names:?}\n{told}");
    // A record that passes both queries is two rows, one per query, and
    // no record is reported twice for the same query.
    let mut seen: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            (
                field(&row.line, "query").expect("a query").to_string(),
                field(&row.line, "record_id").expect("an id").to_string(),
            )
        })
        .collect();
    let before = seen.len();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), before, "a record was reported twice");
    assert!(before > 2, "the threshold matched nothing but the two");
}

#[test]
fn a_watcher_that_joined_late_reads_from_the_next_keyframe() {
    let (dir, stream) = live("mid-stream", 6);
    let bytes = std::fs::read(&stream).expect("the stream");
    // `ref.h264` has its second keyframe at access unit 30, so a join
    // at access unit 10 is a join well before it: the carriers between
    // have no declaration to read their records with, and the keyframe
    // brings one.
    let units = access_units(&bytes);
    assert!(units.len() > 30, "the fixture is shorter than it was");
    let joined = &bytes[units[10]..];
    let query = format!("late={}", text(&dir.join("q5.json")));
    let (rows, told) = paced(
        &[
            "watch",
            "--video",
            "-",
            "--codec",
            "h264",
            "--threshold",
            "0.97",
            "--query",
            &query,
        ],
        joined,
        12,
    );
    assert_eq!(told.matches("1 spaces").count(), 1, "{told}");
    assert_eq!(rows.len(), 1, "{rows:#?}\n{told}");
    assert_eq!(field(&rows[0].line, "query"), Some("\"late\""));
    // The whole stream, for comparison: the same record, and the same
    // score, whichever end a watcher came in at.
    let whole = paced(
        &[
            "watch",
            "--video",
            "-",
            "--codec",
            "h264",
            "--threshold",
            "0.97",
            "--query",
            &query,
        ],
        &bytes,
        12,
    )
    .0;
    assert_eq!(whole.len(), 1);
    assert_eq!(
        field(&whole[0].line, "score"),
        field(&rows[0].line, "score")
    );
}

#[test]
fn records_whose_space_never_arrives_are_held_and_bounded() {
    let (dir, stream) = live("orphans", 6);
    let bytes = std::fs::read(&stream).expect("the stream");
    // Access units 7 to 29: past the first carrier the writer wrote to,
    // which is where the `next` policy puts the declarations, and short
    // of the keyframe at access unit 30, which repeats them. So no
    // space is ever declared and every record in there is an orphan.
    // Section 3 has a reader hold them; section 9 has it bound what it
    // holds.
    let units = access_units(&bytes);
    assert!(units.len() > 30, "the fixture is shorter than it was");
    let middle = &bytes[units[7]..units[30]];
    let query = format!("nothing={}", text(&dir.join("q2.json")));
    let (rows, told) = paced(
        &[
            "watch",
            "--video",
            "-",
            "--codec",
            "h264",
            "--threshold",
            "-1",
            "--query",
            &query,
        ],
        middle,
        8,
    );
    assert!(rows.is_empty(), "{rows:#?}\n{told}");
    assert!(told.contains("0 spaces"), "{told}");
    let held: usize = told
        .split("records still held")
        .next()
        .and_then(|before| before.split_whitespace().last())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no count of held records in {told}"));
    assert!(held > 0, "the orphans were dropped rather than held");
    assert!(held <= 4096, "{held} records held is not a ceiling");
}

#[test]
fn min_planes_waits_and_the_default_does_not() {
    // `spread` doles a record's planes out over several carriers, so a
    // watcher scoring on plane 0 hears of a record earlier, and with a
    // coarser number, than one that waits for all eight.
    let dir = scratch("min-planes");
    let rows_path = dir.join("rows.ndjson");
    let stream = dir.join("live.h264");
    std::fs::write(&rows_path, rows(6)).expect("the rows");
    ok(&[
        "weave",
        "--video",
        &text(&fixture("ref.h264")),
        "--vectors",
        &text(&rows_path),
        "--out",
        &text(&stream),
        "--placement",
        "spread:40",
    ]);
    std::fs::write(dir.join("q0.json"), format!("[{}]", list(&vector(0, 16)))).expect("a query");
    let bytes = std::fs::read(&stream).expect("the stream");
    let query = format!("first={}", text(&dir.join("q0.json")));

    let at = |min_planes: &str| -> Vec<usize> {
        paced(
            &[
                "watch",
                "--video",
                "-",
                "--codec",
                "h264",
                "--threshold",
                "0.5",
                "--min-planes",
                min_planes,
                "--query",
                &query,
            ],
            &bytes,
            16,
        )
        .0
        .iter()
        .map(|row| row.after_chunks)
        .collect()
    };
    let early = at("1");
    let late = at("8");
    assert!(!early.is_empty(), "nothing matched on the sign plane alone");
    assert!(!late.is_empty(), "nothing matched with every plane");
    assert!(
        early[0] <= late[0],
        "waiting for eight planes reported sooner: {early:?} {late:?}"
    );
}

#[test]
fn watch_says_what_it_will_not_do() {
    let (dir, stream) = live("refusals", 2);
    let stream = text(&stream);
    let query = format!("a={}", text(&dir.join("q0.json")));
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["watch", "--video", &stream, "--query", &query],
            "--threshold is needed",
        ),
        (
            vec!["watch", "--video", &stream, "--threshold", "0.5"],
            "at least one --query",
        ),
        (
            vec![
                "watch",
                "--video",
                &stream,
                "--threshold",
                "warm",
                "--query",
                &query,
            ],
            "not a cosine threshold",
        ),
        (
            vec![
                "watch",
                "--video",
                "-",
                "--threshold",
                "0.5",
                "--query",
                &query,
            ],
            "does not name its codec",
        ),
        (
            vec![
                "watch",
                "--video",
                &stream,
                "--threshold",
                "0.5",
                "--query",
                "no-equals-sign",
            ],
            "NAME=FILE",
        ),
        (
            vec![
                "watch",
                "--video",
                &stream,
                "--threshold",
                "0.5",
                "--query",
                &query,
                "--scan",
                "all",
            ],
            "not a flag of this command",
        ),
    ];
    for (args, wanted) in cases {
        let (code, _, told) = tool(&args);
        assert_eq!(code, 2, "{args:?} did not refuse");
        assert!(told.contains(wanted), "{args:?} said {told}");
    }
}
