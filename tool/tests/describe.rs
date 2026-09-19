//! `rows-from-describe`, and the path the example takes after it.
//!
//! The fixtures are what `ffrwd/describe` writes to an `.ndjson`
//! destination, cut down to four components a vector so the test can
//! say what every number is: one object a line, the span in seconds,
//! the vector as an array. What is checked is the naming, because that
//! is the part a person cannot see is wrong until a search of the file
//! comes back empty: two spaces, two different query models, and the
//! hashes out of the package's own manifest.

use std::path::PathBuf;
use std::process::Command;

const TOOL: &str = env!("CARGO_BIN_EXE_ffrwd-index");

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../core/tests/data")
        .join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ffrwd-index-describe-{name}"));
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

/// A manifest shaped like the describe package's, with its real model
/// names and digests.
const MANIFEST: &str = r#"{
  "name": "ffrwd/describe",
  "version": "0.1.2",
  "models": {
    "clips": {"repo": "imbcmdth/xclip-onnx",
              "revision": "649f3c91b59cd24be316dc505e26eacf5cd00801",
              "file": "video_tower.onnx",
              "sha256": "87a87b51ae52efa549e6e48f3a3b41f4fe0e5b91e154afcda5d9ac8dd1b975b4"},
    "embed_clip": {"repo": "imbcmdth/xclip-onnx",
              "revision": "649f3c91b59cd24be316dc505e26eacf5cd00801",
              "file": "text_tower.onnx",
              "sha256": "c14df0fdda1ba530330e278b8fabeb0bfd9a58c8caa10dfdee38b6ea197d0afc"},
    "embed": {"repo": "Xenova/all-MiniLM-L6-v2",
              "revision": "751bff37182d3f1213fa05d7196b954e230abad9",
              "file": "onnx/model.onnx",
              "sha256": "759c3cd2b7fe7e93933ad23c4c9181b7396442a2ed746ec7c1d46192c469c46e"}
  }
}"#;

/// Four shots, as the clips recipe writes them.
const CLIPS: &str = concat!(
    r#"{"end_t":0.4,"pts":190190,"start_t":0.0,"time":0.42,"vector":[1,0,0,0]}"#,
    "\n",
    r#"{"end_t":0.9,"pts":200000,"start_t":0.44,"time":0.92,"vector":[0,1,0,0]}"#,
    "\n",
    r#"{"end_t":1.4,"pts":210000,"start_t":0.94,"time":1.42,"vector":[0,0,1,0]}"#,
    "\n",
    r#"{"end_t":1.9,"pts":220000,"start_t":1.44,"time":1.92,"vector":[0,0,0,1]}"#,
    "\n",
);

/// Two windows, as the sounds recipe writes them through embed.
const SOUNDS: &str = concat!(
    r#"{"start_t":0.0,"end_t":1.0,"vector":[0.5,0.5,0,0]}"#,
    "\n",
    r#"{"start_t":0.5,"end_t":1.5,"vector":[0,0,0.5,0.5]}"#,
    "\n",
);

/// One transcript span.
const SPEECH: &str = concat!(
    r#"{"start_t":0.2,"end_t":1.2,"vector":[0.25,0,0,0.75]}"#,
    "\n",
);

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

/// The three files and the manifest, written into a fresh directory.
fn described(name: &str) -> PathBuf {
    let dir = scratch(name);
    std::fs::write(dir.join("ffrwd.json"), MANIFEST).expect("a manifest");
    std::fs::write(dir.join("clips.ndjson"), CLIPS).expect("the clip vectors");
    std::fs::write(dir.join("sounds.ndjson"), SOUNDS).expect("the sound vectors");
    std::fs::write(dir.join("speech.ndjson"), SPEECH).expect("the speech vectors");
    dir
}

#[test]
fn three_tracks_become_three_spaces_that_name_their_models() {
    let dir = described("spaces");
    let rows = dir.join("rows.ndjson");
    let (_, told) = ok(&[
        "rows-from-describe",
        "--clip",
        &text(&dir.join("clips.ndjson")),
        "--sound",
        &text(&dir.join("sounds.ndjson")),
        "--speech",
        &text(&dir.join("speech.ndjson")),
        "--package",
        &text(&dir.join("ffrwd.json")),
        "--out",
        &text(&rows),
    ]);
    assert!(told.contains("7 vectors in 3 spaces"), "{told}");

    let written = std::fs::read_to_string(&rows).expect("the rows");
    let spaces: Vec<&str> = written
        .lines()
        .filter(|line| line.starts_with(r#"{"space":"#))
        .collect();
    assert_eq!(spaces.len(), 3, "{written}");

    // The clip space: the video tower made the vectors, the text tower
    // embeds a search, and they are not the same file.
    assert!(spaces[0].contains(r#""id":1"#), "{}", spaces[0]);
    assert!(spaces[0].contains(r#""modality":"picture""#));
    assert!(spaces[0].contains("video_tower.onnx"));
    assert!(spaces[0].contains("text_tower.onnx"));
    assert!(
        spaces[0].contains(r#""model_hash":"87a87b51ae52efa549e6e48f3a3b41f4""#),
        "{}",
        spaces[0]
    );
    assert!(
        spaces[0].contains(r#""query_hash":"c14df0fdda1ba530330e278b8fabeb0b""#),
        "{}",
        spaces[0]
    );
    assert!(spaces[0].contains("ffrwd/describe 0.1.2"));

    // The two text spaces: one model reached two ways, and two
    // modalities, which is why they are two spaces and not one.
    assert!(spaces[1].contains(r#""id":2"#));
    assert!(spaces[1].contains(r#""modality":"speech""#));
    assert!(spaces[2].contains(r#""id":3"#));
    assert!(spaces[2].contains(r#""modality":"sound-text""#));
    for space in &spaces[1..] {
        assert!(space.contains("all-MiniLM-L6-v2"), "{space}");
        assert!(
            space.contains(r#""model_hash":"759c3cd2b7fe7e93933ad23c4c9181b7""#),
            "{space}"
        );
        // An empty query URI means the same as the model, which is what
        // section 3 says and what one network reached two ways is.
        assert!(space.contains("\"query\":\"hf:Xenova"), "{space}");
    }

    // Seconds became milliseconds, rounded.
    let vectors: Vec<&str> = written
        .lines()
        .filter(|line| line.contains("\"vector\":"))
        .collect();
    assert_eq!(vectors.len(), 7);
    assert_eq!(field(vectors[0], "start_ms"), Some("0"));
    assert_eq!(field(vectors[0], "end_ms"), Some("400"));
    assert_eq!(field(vectors[1], "start_ms"), Some("440"));
    assert_eq!(field(vectors[4], "space_id"), Some("2"), "{}", vectors[4]);
    assert_eq!(field(vectors[5], "space_id"), Some("3"), "{}", vectors[5]);
}

#[test]
fn the_rows_weave_and_a_search_of_the_result_finds_the_shot() {
    let dir = described("round-trip");
    let rows = dir.join("rows.ndjson");
    ok(&[
        "rows-from-describe",
        "--clip",
        &text(&dir.join("clips.ndjson")),
        "--sound",
        &text(&dir.join("sounds.ndjson")),
        "--package",
        &text(&dir.join("ffrwd.json")),
        "--out",
        &text(&rows),
    ]);
    let woven = dir.join("woven.h264");
    ok(&[
        "weave",
        "--video",
        &text(&fixture("ref.h264")),
        "--vectors",
        &text(&rows),
        "--out",
        &text(&woven),
    ]);
    let query = dir.join("query.json");
    std::fs::write(&query, "[0,0,0.9,0.1]").expect("a query");

    // The clip space by its model's name, which is how a person who did
    // not write the file would ask for it.
    let (found, told) = ok(&[
        "search",
        "--video",
        &text(&woven),
        "--space",
        "xclip",
        "--query",
        &text(&query),
        "--top",
        "1",
    ]);
    assert!(found.contains(r#""space":1"#), "{found}");
    assert!(
        found.contains(r#""start_t":0.940"#),
        "the third shot is not first: {found}"
    );
    assert!(told.contains("text_tower.onnx"), "{told}");

    // And the sound space is its own search, with its own query model.
    let (found, told) = ok(&[
        "search",
        "--video",
        &text(&woven),
        "--space",
        "MiniLM",
        "--query",
        &text(&query),
        "--top",
        "1",
    ]);
    assert!(found.contains(r#""space":3"#), "{found}");
    assert!(told.contains("all-MiniLM-L6-v2"), "{told}");
}

#[test]
fn without_a_manifest_the_hashes_go_out_unknown() {
    let dir = described("no-manifest");
    let (written, told) = ok(&[
        "rows-from-describe",
        "--clip",
        &text(&dir.join("clips.ndjson")),
    ]);
    assert!(told.contains("hashes go out zero"), "{told}");
    let space = written.lines().next().expect("a space row");
    assert!(space.contains(r#""model_hash":"""#), "{space}");
    assert!(space.contains(r#""query_hash":"""#), "{space}");
    assert!(space.contains(r#""dims":4"#), "{space}");
}

#[test]
fn rows_from_describe_says_what_it_will_not_do() {
    let dir = described("refusals");
    let clips = text(&dir.join("clips.ndjson"));
    let empty = dir.join("empty.json");
    std::fs::write(&empty, "{}").expect("a file");
    let empty = text(&empty);

    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["rows-from-describe"],
            "at least one of --clip, --speech and --sound",
        ),
        (
            vec!["rows-from-describe", "--clip", "nowhere.ndjson"],
            "nowhere.ndjson",
        ),
        (
            vec!["rows-from-describe", "--clip", &clips, "--package", &empty],
            "names no models",
        ),
        (
            vec!["rows-from-describe", "--clip", &clips, "--scan", "all"],
            "not a flag of this command",
        ),
    ];
    for (args, wanted) in cases {
        let (code, _, told) = tool(&args);
        assert_eq!(code, 2, "{args:?} did not refuse");
        assert!(told.contains(wanted), "{args:?} said {told}");
    }
}
