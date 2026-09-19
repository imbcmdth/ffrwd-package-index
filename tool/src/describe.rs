//! `rows-from-describe`: what `ffrwd/describe` writes, as rows this
//! tool weaves.
//!
//! The package's recipes hand back one JSON object per vector, with the
//! span in seconds and the vector as an array of numbers:
//!
//! ```text
//! {"start_t":0.0,"end_t":3.92,"pts":190190,"time":3.96,"vector":[...]}
//! ```
//!
//! Turning that into this tool's rows is arithmetic and naming. The
//! arithmetic is seconds into milliseconds. The naming is section 3's
//! SPACE fields, and it is the part worth a subcommand rather than a
//! line of `jq`: a space has to say which model made the vectors and
//! which model turns a search into the same space, and for this package
//! those are two different networks of one model in the clip space and
//! one network twice over in the text spaces. Getting that wrong gives
//! a file whose vectors nobody can search, and nothing about the file
//! would say so.
//!
//! The URIs and the hashes come out of the package's own `ffrwd.json`,
//! whose `models` table already names the repository, the revision, the
//! file and its SHA-256. With no package to read, the URIs are written
//! from the names below and the hashes are left zero, which section 3
//! spells "unknown", and the tool says on stderr that it did that.

use std::collections::BTreeMap;

use ffrwd_index_core::message::{Encoding, Modality, Space, FLAG_UNIT_LENGTH};
use ffrwd_index_rows::json::{number, object, string, Json};
use ffrwd_index_rows::space::space_row;

use crate::Flags;

/// One of the package's vector tracks, and the space it becomes.
struct Track {
    /// The flag that names the file.
    flag: &'static str,
    space_id: u8,
    modality: Modality,
    /// The `models` key of the network that made the vectors.
    model_key: &'static str,
    /// The `models` key of the network that embeds a query into the
    /// same space.
    query_key: &'static str,
}

/// The three tracks `ffrwd/describe` produces vectors for.
///
/// Sound and speech are two spaces and not one, although the model and
/// the dimensionality are the same. A SPACE message carries one
/// modality (section 3), and these are two: an AudioSet label for what
/// was heard, and a transcript of what was said. A reader that wanted
/// to search only what was said could not tell them apart if they
/// shared an id, and the cost of keeping them apart is one more SPACE
/// message on each keyframe.
const TRACKS: [Track; 3] = [
    Track {
        flag: "clip",
        space_id: 1,
        modality: Modality::Picture,
        model_key: "clips",
        query_key: "embed_clip",
    },
    Track {
        flag: "speech",
        space_id: 2,
        modality: Modality::Speech,
        model_key: "embed",
        query_key: "embed",
    },
    Track {
        flag: "sound",
        space_id: 3,
        modality: Modality::SoundText,
        model_key: "embed",
        query_key: "embed",
    },
];

/// A model as the package's manifest describes it.
#[derive(Clone, Debug, Default, PartialEq)]
struct Model {
    uri: String,
    hash: String,
}

pub fn rows_from_describe(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args)?;
    flags.only(&["clip", "speech", "sound", "package", "out", "producer"])?;
    if TRACKS.iter().all(|track| flags.get(track.flag).is_none()) {
        return Err("rows-from-describe wants at least one of --clip, --speech and --sound".into());
    }

    let (models, producer) = match flags.get("package") {
        Some(path) => manifest(path)?,
        None => {
            eprintln!(
                "rows-from-describe: no --package, so the model hashes go out zero, \
                 which section 3 reads as unknown"
            );
            (BTreeMap::new(), "ffrwd/describe".to_string())
        }
    };
    let producer = flags.get("producer").unwrap_or(&producer).to_string();

    let mut out = String::new();
    let mut counts: Vec<(u8, usize)> = Vec::new();
    let mut spaces = Vec::new();
    let mut rows = Vec::new();
    for track in &TRACKS {
        let Some(path) = flags.get(track.flag) else {
            continue;
        };
        let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
        let vectors = read_vectors(&text, path)?;
        let Some(dims) = vectors.first().map(|(_, _, values)| values.len()) else {
            eprintln!(
                "{path}: no vectors, so space {} is not declared",
                track.space_id
            );
            continue;
        };
        let space = declare(track, dims as u32, &models, &producer);
        counts.push((track.space_id, vectors.len()));
        spaces.push(object(vec![("space", space_row(&space))]).write());
        for (start_ms, end_ms, values) in vectors {
            rows.push(
                object(vec![
                    ("space_id", number(track.space_id)),
                    ("start_ms", number(start_ms as f64)),
                    ("end_ms", number(end_ms as f64)),
                    (
                        "vector",
                        Json::Array(
                            values
                                .iter()
                                .copied()
                                .map(ffrwd_index_rows::json::float)
                                .collect(),
                        ),
                    ),
                ])
                .write(),
            );
        }
    }
    // Every space first: the rows a weave reads declare a space before
    // the vectors that use it, and a reader of a stream is happier with
    // the declarations in one place than scattered through the file.
    for line in spaces.iter().chain(rows.iter()) {
        out.push_str(line);
        out.push('\n');
    }

    match flags.get("out") {
        Some(path) => {
            std::fs::write(path, &out).map_err(|err| format!("{path}: {err}"))?;
            eprintln!("{path}: {}", tally(&counts));
        }
        None => {
            print!("{out}");
            eprintln!("rows-from-describe: {}", tally(&counts));
        }
    }
    Ok(())
}

fn tally(counts: &[(u8, usize)]) -> String {
    format!(
        "{} vectors in {} spaces ({})",
        counts.iter().map(|(_, count)| count).sum::<usize>(),
        counts.len(),
        counts
            .iter()
            .map(|(id, count)| format!("space {id}: {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// One space, named from the manifest where there is one.
fn declare(track: &Track, dims: u32, models: &BTreeMap<String, Model>, producer: &str) -> Space {
    let model = models.get(track.model_key).cloned().unwrap_or_default();
    let query = models.get(track.query_key).cloned().unwrap_or_default();
    let mut space = Space::new(track.space_id, dims, Encoding::I8);
    space.modality = track.modality;
    space.model = model.uri;
    space.query = query.uri;
    space.model_hash = hash(&model.hash);
    space.query_hash = hash(&query.hash);
    space.producer = producer.to_string();
    // The vectors are not unit length: `cos_similarity` is what the
    // package's own recipes rank with, which says the models do not
    // promise it, and saying they are when they are not would have a
    // reader skip a normalisation it needs.
    space.flags &= !FLAG_UNIT_LENGTH;
    space
}

/// The first sixteen bytes of a SHA-256, from the hex the manifest
/// gives, or all zero when it gives none.
fn hash(text: &str) -> [u8; 16] {
    ffrwd_index_rows::space::hash_of(Some(&string(text))).unwrap_or([0; 16])
}

/// The `models` table of a package manifest, as URIs and hashes.
///
/// `hf:<repo>@<revision>/<file>` is the form section 3 suggests for a
/// Hugging Face file, and every field of it is already in the manifest.
fn manifest(path: &str) -> Result<(BTreeMap<String, Model>, String), String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
    let json = Json::parse(text.trim()).map_err(|err| format!("{path}: {err}"))?;
    let mut models = BTreeMap::new();
    if let Some(Json::Object(members)) = json.get("models") {
        for (name, entry) in members {
            let field = |key: &str| entry.get(key).and_then(Json::as_str).unwrap_or_default();
            let (repo, revision, file) = (field("repo"), field("revision"), field("file"));
            if repo.is_empty() || file.is_empty() {
                continue;
            }
            models.insert(
                name.clone(),
                Model {
                    uri: format!("hf:{repo}@{revision}/{file}"),
                    hash: field("sha256").to_string(),
                },
            );
        }
    }
    if models.is_empty() {
        return Err(format!("{path} names no models"));
    }
    let producer = match (
        json.get("name").and_then(Json::as_str),
        json.get("version").and_then(Json::as_str),
    ) {
        (Some(name), Some(version)) => format!("{name} {version}"),
        (Some(name), None) => name.to_string(),
        _ => "ffrwd/describe".to_string(),
    };
    Ok((models, producer))
}

/// The spans and vectors of one of the package's NDJSON outputs.
///
/// A row without a vector is not a failure: the recipes write cue rows
/// beside vector rows, and a caller that pointed this at the wrong file
/// hears about it from the count rather than from a refusal per line.
#[allow(clippy::type_complexity)]
fn read_vectors(text: &str, path: &str) -> Result<Vec<(i64, i64, Vec<f32>)>, String> {
    let mut out: Vec<(i64, i64, Vec<f32>)> = Vec::new();
    let mut dims: Option<usize> = None;
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |what: &str| format!("{path} line {}: {what}", number + 1);
        let row = Json::parse(line).map_err(|err| at(&err))?;
        let Some(values) = crate::search::query_vector(line) else {
            continue;
        };
        match dims {
            None => dims = Some(values.len()),
            Some(want) if want != values.len() => {
                return Err(at(&format!(
                    "a vector of {} components where the rows before it had {want}",
                    values.len()
                )))
            }
            Some(_) => {}
        }
        let start = seconds(&row, "start_t").ok_or_else(|| at("a row with no start_t"))?;
        let end = seconds(&row, "end_t").ok_or_else(|| at("a row with no end_t"))?;
        if end < start {
            return Err(at("a span that ends before it starts"));
        }
        out.push((start, end, values));
    }
    Ok(out)
}

/// One of the row's times, in milliseconds.
fn seconds(row: &Json, name: &str) -> Option<i64> {
    let value = row.get(name)?.as_f64()?;
    if !value.is_finite() {
        return None;
    }
    Some((value * 1000.0).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: &str = concat!(
        r#"{"end_t":3.920583333333333,"pts":190190,"start_t":0.0,"time":3.96,"vector":[0.25,-0.5,0.125,1.0]}"#,
        "\n",
        r#"{"end_t":5.92,"start_t":3.9622916666666668,"vector":[-1.0,0.5,0.0,0.25]}"#,
        "\n",
    );

    #[test]
    fn a_describe_row_becomes_a_span_in_milliseconds() {
        let read = read_vectors(ROWS, "rows.ndjson").expect("rows");
        assert_eq!(read.len(), 2);
        assert_eq!((read[0].0, read[0].1), (0, 3921));
        assert_eq!((read[1].0, read[1].1), (3962, 5920));
        assert_eq!(read[0].2, vec![0.25, -0.5, 0.125, 1.0]);
    }

    #[test]
    fn rows_that_are_not_vectors_are_passed_over_and_ragged_ones_refused() {
        let mixed = concat!(
            r#"{"start_t":0,"end_t":1,"text":"a dog barking"}"#,
            "\n",
            r#"{"start_t":1,"end_t":2,"vector":[1,2]}"#,
            "\n",
        );
        let read = read_vectors(mixed, "rows.ndjson").expect("rows");
        assert_eq!(read.len(), 1, "the cue row is not a vector");

        let ragged = concat!(
            r#"{"start_t":0,"end_t":1,"vector":[1,2]}"#,
            "\n",
            r#"{"start_t":1,"end_t":2,"vector":[1,2,3]}"#,
            "\n",
        );
        let err = read_vectors(ragged, "rows.ndjson").expect_err("a refusal");
        assert!(err.contains("components"), "{err}");

        let backwards = r#"{"start_t":5,"end_t":1,"vector":[1,2]}"#;
        let err = read_vectors(backwards, "rows.ndjson").expect_err("a refusal");
        assert!(err.contains("ends before"), "{err}");

        let nameless = r#"{"end_t":1,"vector":[1,2]}"#;
        let err = read_vectors(nameless, "rows.ndjson").expect_err("a refusal");
        assert!(err.contains("start_t"), "{err}");
    }

    #[test]
    fn a_manifest_names_the_two_models_of_a_space() {
        let text = r#"{"name":"ffrwd/describe","version":"0.1.2","models":{
            "clips":{"repo":"imbcmdth/xclip-onnx","revision":"649f3c9","file":"video_tower.onnx",
                     "sha256":"87a87b51ae52efa549e6e48f3a3b41f4fe0e5b91e154afcda5d9ac8dd1b975b4"},
            "embed_clip":{"repo":"imbcmdth/xclip-onnx","revision":"649f3c9","file":"text_tower.onnx",
                     "sha256":"c14df0fdda1ba530330e278b8fabeb0bfd9a58c8caa10dfdee38b6ea197d0afc"},
            "embed":{"repo":"Xenova/all-MiniLM-L6-v2","revision":"751bff3","file":"onnx/model.onnx",
                     "sha256":"759c3cd2b7fe7e93933ad23c4c9181b7396442a2ed746ec7c1d46192c469c46e"}}}"#;
        let dir = std::env::temp_dir().join("ffrwd-index-manifest-test");
        std::fs::create_dir_all(&dir).expect("a directory");
        let path = dir.join("ffrwd.json");
        std::fs::write(&path, text).expect("a manifest");
        let (models, producer) = manifest(path.to_str().expect("a path")).expect("a manifest");
        assert_eq!(producer, "ffrwd/describe 0.1.2");

        let clip = declare(&TRACKS[0], 512, &models, &producer);
        assert_eq!(clip.space_id, 1);
        assert_eq!(clip.modality, Modality::Picture);
        assert_eq!(
            clip.model,
            "hf:imbcmdth/xclip-onnx@649f3c9/video_tower.onnx"
        );
        assert_eq!(clip.query, "hf:imbcmdth/xclip-onnx@649f3c9/text_tower.onnx");
        assert_ne!(clip.model, clip.query, "one tower embedded both sides");
        assert_eq!(clip.model_hash[0], 0x87);
        assert_eq!(clip.query_hash[0], 0xc1);
        assert!(!clip.unit_length());

        // The two text spaces share a model and differ in modality,
        // which is why they are two.
        let speech = declare(&TRACKS[1], 384, &models, &producer);
        let sound = declare(&TRACKS[2], 384, &models, &producer);
        assert_eq!(speech.model, sound.model);
        assert_eq!(speech.model, speech.query, "one network, reached two ways");
        assert_eq!(speech.modality, Modality::Speech);
        assert_eq!(sound.modality, Modality::SoundText);
        assert_ne!(speech.space_id, sound.space_id);
    }

    #[test]
    fn without_a_manifest_a_space_says_it_does_not_know_the_hashes() {
        let space = declare(&TRACKS[0], 512, &BTreeMap::new(), "ffrwd/describe");
        assert_eq!(space.model_hash, [0; 16]);
        assert_eq!(space.query_hash, [0; 16]);
        assert_eq!(space.dims, 512);
    }
}
