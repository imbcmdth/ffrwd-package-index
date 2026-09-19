//! `search`: rank the records of one space against a query vector.
//!
//! The tool does not embed text. A space says which model turns a query
//! into its own space (section 3's `query`), and getting a vector out of
//! that model is somebody else's job; what arrives here is the vector,
//! in the same two spellings a row's `vector` uses. So a search is three
//! decisions and some arithmetic: which file to read and how little of
//! it, which space of the several a file may carry, and how to rank.
//!
//! The ranking is MEASUREMENTS.md's. Cosine, never a bare dot product:
//! a record's scale is its own, so two reconstructions of few planes are
//! not comparable by a dot product, and the layered encoding means a
//! reader often has few planes. The query stays binary32 throughout,
//! which is also what makes the coarse stage worth having: scoring a
//! full-precision query against the sign bits beat Hamming distance at
//! every candidate count in both measured spaces, so that is the
//! prefilter [`Coarse`] runs, and it is off unless asked for.

use std::collections::BTreeMap;

use ffrwd_index_container::scan::Scan;
use ffrwd_index_container::{mkv, mp4, Kind, Source, Tally};
use ffrwd_index_core::assemble::{Assembler, Limits};
use ffrwd_index_core::index::FileIndex;
use ffrwd_index_core::message::{Message, Space, Unit, VectorBody};
use ffrwd_index_core::quant::unpack_signs;
use ffrwd_index_rows::json::{number, object, Json};
use ffrwd_index_rows::space::read_space;
use ffrwd_index_rows::vector::{from_base64, read_values};

use crate::{planes_row, read_head, Carried, Flags};

/// One record a search can score.
#[derive(Debug)]
pub struct Candidate {
    pub space_id: u8,
    pub record_id: u16,
    pub start_ms: i64,
    pub end_ms: i64,
    pub body: VectorBody,
    /// Which planes arrived, for a layered record.
    pub planes: Option<u8>,
}

/// What a source of records handed back.
pub struct Found {
    /// The spaces, in the order they were first declared.
    pub spaces: Vec<Space>,
    pub candidates: Vec<Candidate>,
    /// What the read cost, for stderr.
    pub note: String,
}

/// How many candidates the sign planes pick before the rescore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coarse(pub usize);

pub fn search(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args)?;
    flags.only(&[
        "video",
        "mp4",
        "mkv",
        "index",
        "rows",
        "space",
        "query",
        "top",
        "threshold",
        "coarse",
        "scan",
        "fps",
        "codec",
    ])?;
    let query_path = flags.need("query")?;
    let found = gather(&flags)?;
    if found.spaces.is_empty() {
        return Err("the file declares no space to search".into());
    }
    let space = choose(&found.spaces, flags.get("space"))?;
    let query = read_query(query_path)?;
    if query.len() as u32 != space.dims {
        return Err(format!(
            "a query of {} components against space {}, which is {} wide",
            query.len(),
            space.space_id,
            space.dims
        ));
    }

    let top = match flags.get("top") {
        Some(text) => Some(
            text.parse::<usize>()
                .map_err(|_| format!("{text} is not a number of results"))?,
        ),
        None => None,
    };
    let threshold = match flags.get("threshold") {
        Some(text) => Some(
            text.parse::<f32>()
                .map_err(|_| format!("{text} is not a cosine threshold"))?,
        ),
        None => None,
    };
    let coarse = match flags.get("coarse") {
        Some(text) => {
            Some(Coarse(text.parse::<usize>().map_err(|_| {
                format!("{text} is not a number of candidates")
            })?))
        }
        None => None,
    };
    // Neither said means the ten best, which is what a person typing
    // the command wants and what a pipeline can override either way.
    let top = match (top, threshold) {
        (None, None) => Some(10),
        (top, _) => top,
    };

    let mine: Vec<&Candidate> = found
        .candidates
        .iter()
        .filter(|candidate| candidate.space_id == space.space_id)
        .collect();
    let hits = rank(&mine, &query, coarse, threshold, top)?;

    let mut out = String::new();
    for (rank, (candidate, score)) in hits.iter().enumerate() {
        out.push_str(&row(rank + 1, candidate, *score).write());
        out.push('\n');
    }
    print!("{out}");

    eprintln!("{}", found.note);
    eprintln!(
        "space {}: {} of {} records scored{}, {} printed",
        space.space_id,
        mine.len(),
        found.candidates.len(),
        match coarse {
            Some(Coarse(n)) => format!(", {n} of them rescored from a plane 0 prefilter"),
            None => String::new(),
        },
        hits.len()
    );
    // Section 3: a reader that wants to search a space needs the model
    // that embeds a query into it, so say which one the file named.
    let query_model = space.query_model();
    if query_model.is_empty() {
        eprintln!(
            "space {}: the file names no model for embedding a query into it",
            space.space_id
        );
    } else {
        eprintln!(
            "space {}: a query for this space is embedded by {query_model}",
            space.space_id
        );
    }
    Ok(())
}

/// What a source of records has to be one of.
const SOURCES: &str = "search takes one of --video, --mp4, --mkv, --index and --rows";

/// The records of whichever source the flags named.
fn gather(flags: &Flags) -> Result<Found, String> {
    let named = ["video", "mp4", "mkv", "index", "rows"]
        .iter()
        .filter(|name| flags.get(name).is_some())
        .count();
    if named != 1 {
        return Err(SOURCES.into());
    }
    match (
        flags.get("video"),
        flags.get("mp4"),
        flags.get("mkv"),
        flags.get("index"),
        flags.get("rows"),
    ) {
        (Some(video), ..) => from_stream(flags, video),
        (_, Some(file), ..) => from_container(flags, file, Some(Kind::Mp4)),
        (_, _, Some(file), ..) => from_container(flags, file, Some(Kind::Matroska)),
        (_, _, _, Some(file), _) => from_index_file(file),
        (.., Some(file)) => from_rows(file),
        _ => Err(SOURCES.into()),
    }
}

/// The rows a weave would take, scored as the numbers they hold.
///
/// This is the file that never happened: the vectors before anything
/// was quantized, carried or read back. Ranking them is the brute force
/// MEASUREMENTS.md measures the encoding against, and having it behind
/// the same flags is what lets somebody ask what their own file's
/// encoding cost their own search rather than taking a table's word for
/// it. The space declarations are read for their names and their ids;
/// the `encoding` they name is not applied, because applying it is what
/// the comparison is about.
fn from_rows(path: &str) -> Result<Found, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
    let mut spaces: Vec<Space> = Vec::new();
    let mut by_id: BTreeMap<u8, Space> = BTreeMap::new();
    let mut next_id: BTreeMap<u8, u32> = BTreeMap::new();
    let mut candidates = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |what: String| format!("{path} line {}: {what}", number + 1);
        let row = Json::parse(line).map_err(&at)?;
        if let Some(descriptor) = row.get("space") {
            let space = read_space(descriptor).map_err(at)?;
            if by_id.insert(space.space_id, space.clone()).is_none() {
                spaces.push(space);
            }
            continue;
        }
        let space_id = u8::try_from(
            row.get("space_id")
                .and_then(Json::as_i64)
                .ok_or_else(|| at("a row with no space_id".into()))?,
        )
        .map_err(|_| at("a space_id outside 0 to 255".into()))?;
        let space = by_id
            .get(&space_id)
            .ok_or_else(|| at(format!("space {space_id} has not been declared")))?;
        let values = read_values(row.get("vector"), space.dims).map_err(at)?;
        let counter = next_id.entry(space_id).or_insert(0);
        let record_id = match row.get("record_id").and_then(Json::as_i64) {
            Some(id) => {
                u16::try_from(id).map_err(|_| at("a record_id outside 0 to 65535".into()))?
            }
            None => {
                let id = *counter % ffrwd_index_core::RECORD_ID_WRAP;
                *counter += 1;
                id as u16
            }
        };
        candidates.push(Candidate {
            space_id,
            record_id,
            start_ms: row
                .get("start_ms")
                .and_then(Json::as_i64)
                .ok_or_else(|| at("a row with no start_ms".into()))?,
            end_ms: row
                .get("end_ms")
                .and_then(Json::as_i64)
                .ok_or_else(|| at("a row with no end_ms".into()))?,
            body: VectorBody::F32(values),
            planes: None,
        });
    }
    let note = format!(
        "{path}: {} rows read as the binary32 they hold, with no encoding applied",
        candidates.len()
    );
    Ok(Found {
        spaces,
        candidates,
        note,
    })
}

fn from_stream(flags: &Flags, video: &str) -> Result<Found, String> {
    if flags.get("scan").is_some() {
        return Err(
            "--scan belongs to --mp4 and --mkv; a stream has every access unit in it".into(),
        );
    }
    let carried = crate::stream_carriers(flags, video)?;
    let mut found = assemble(&carried);
    found.note = format!("{video}: {} access units read whole", carried.len());
    Ok(found)
}

/// A container, reading as little of it as the flags allow.
///
/// With no `--scan` the file's own index is preferred, because that is
/// what section 8 put it there for: one read instead of a walk. Asking
/// for a scan asks for the stream itself, which is the authority the
/// index is only a copy of.
fn from_container(flags: &Flags, path: &str, kind: Option<Kind>) -> Result<Found, String> {
    let asked = match flags.get("scan") {
        None => None,
        Some(text) => {
            Some(Scan::parse(text).ok_or_else(|| format!("{text} is not keyframes or all"))?)
        }
    };
    if asked.is_none() {
        if let Some((index, tally)) = index_of(path)? {
            let mut found = from_index(path, &index)?;
            found.note = format!(
                "{path}: the file's own index, {} bytes read in {} seeks, \
                 taken at its word: a cut or a join since it was built would \
                 leave it wrong, and `ffrwd-index index` rebuilds it",
                tally.bytes_read, tally.seeks
            );
            return Ok(found);
        }
    }
    let scan = asked.unwrap_or(Scan::Keyframes);
    let file = std::fs::File::open(path).map_err(|err| format!("{path}: {err}"))?;
    let mut src = Source::new(file).map_err(|err| format!("{path}: {err}"))?;
    let (track, carried) =
        crate::walk(&mut src, kind, scan).map_err(|err| format!("{path}: {err}"))?;
    let tally = src.tally();
    let mut found = assemble(&carried);
    found.note = format!(
        "{path}: {}",
        crate::accounting(&track, &carried, scan, tally)
    );
    Ok(found)
}

/// `--index`, which takes a bare index or the file carrying one.
fn from_index_file(path: &str) -> Result<Found, String> {
    let head = read_head(path)?;
    if head.starts_with(&ffrwd_index_core::index::MAGIC) {
        let bytes = std::fs::read(path).map_err(|err| format!("{path}: {err}"))?;
        let index = FileIndex::parse(&bytes).map_err(|err| format!("{path}: {err}"))?;
        let mut found = from_index(path, &index)?;
        found.note = format!("{path}: {} bytes of index read whole", bytes.len());
        return Ok(found);
    }
    let (index, tally) =
        index_of(path)?.ok_or_else(|| format!("{path} carries no index of this format"))?;
    let mut found = from_index(path, &index)?;
    found.note = format!(
        "{path}: the file's own index, {} bytes read in {} seeks",
        tally.bytes_read, tally.seeks
    );
    Ok(found)
}

/// The index a container carries, and what reading it cost.
fn index_of(path: &str) -> Result<Option<(FileIndex, Tally)>, String> {
    let file = std::fs::File::open(path).map_err(|err| format!("{path}: {err}"))?;
    let mut src = Source::new(file).map_err(|err| format!("{path}: {err}"))?;
    let kind = ffrwd_index_container::kind_of(&mut src).map_err(|err| format!("{path}: {err}"))?;
    let found = match kind {
        Kind::Mp4 => mp4::read_index(&mut src),
        Kind::Matroska => mkv::read_index(&mut src),
    }
    .map_err(|err| format!("{path}: {err}"))?;
    let tally = src.tally();
    match found {
        Some(bytes) => {
            let index = FileIndex::parse(&bytes).map_err(|err| format!("{path}: {err}"))?;
            Ok(Some((index, tally)))
        }
        None => Ok(None),
    }
}

/// The candidates of an index: section 8's entries, in time order, with
/// each vector read in the space the entries before it declared.
fn from_index(path: &str, index: &FileIndex) -> Result<Found, String> {
    let mut spaces: Vec<Space> = Vec::new();
    let mut by_id: BTreeMap<u8, Space> = BTreeMap::new();
    let mut candidates = Vec::new();
    for entry in &index.entries {
        match &entry.message {
            Message::Space(space) => {
                if by_id.insert(space.space_id, space.clone()).is_none() {
                    spaces.push(space.clone());
                }
            }
            Message::Vector(record) => {
                // A vector whose space the index never declared cannot
                // be read; it is left out rather than guessed at.
                let Some(space) = by_id.get(&record.space_id) else {
                    continue;
                };
                let Ok(body) = record.decode_body(space) else {
                    continue;
                };
                let planes = match &body {
                    VectorBody::I8(planes) => Some(planes.present()),
                    _ => None,
                };
                let time = i64::from(entry.time_ms);
                candidates.push(Candidate {
                    space_id: record.space_id,
                    record_id: record.record_id,
                    start_ms: time + i64::from(record.start_off),
                    end_ms: time + i64::from(record.end_off),
                    body,
                    planes,
                });
            }
            _ => {}
        }
    }
    let _ = path;
    Ok(Found {
        spaces,
        candidates,
        note: String::new(),
    })
}

/// The candidates of a set of carriers, through the assembler, so that
/// planes doled out over several of them are one record here.
pub fn assemble(carried: &[Carried]) -> Found {
    let mut assembler = Assembler::new(Limits {
        max_records: 1 << 20,
        max_reassemblies: 4096,
        orphan_wait_ms: i64::MAX,
    });
    let mut spaces: Vec<Space> = Vec::new();
    let mut seen: BTreeMap<u8, Space> = BTreeMap::new();
    for carrier in carried {
        for bytes in &carrier.units {
            let Ok(unit) = Unit::decode(bytes) else {
                continue;
            };
            for message in &unit.messages {
                if let Message::Space(space) = message {
                    if seen.insert(space.space_id, space.clone()).is_none() {
                        spaces.push(space.clone());
                    }
                }
            }
            assembler.push_unit(carrier.time_ms, &unit);
        }
    }
    let candidates = assembler
        .records()
        .into_iter()
        .map(|record| Candidate {
            space_id: record.space.space_id,
            record_id: record.record_id,
            start_ms: record.start_ms,
            end_ms: record.end_ms,
            body: record.body,
            planes: record.planes,
        })
        .collect();
    Found {
        spaces,
        candidates,
        note: String::new(),
    }
}

/// Which space a search runs over.
///
/// An id names one exactly. Anything else is matched against the two
/// model URIs and the producer, because a person searching a file they
/// did not write knows the model's name and not the byte a writer
/// happened to pick for it. An ambiguous match is a refusal that lists
/// what it matched: guessing would be a search of the wrong space,
/// which looks like a search that found nothing.
pub fn choose(spaces: &[Space], wanted: Option<&str>) -> Result<Space, String> {
    let Some(wanted) = wanted else {
        return match spaces {
            [only] => Ok(only.clone()),
            _ => Err(format!(
                "the file carries {} spaces, so --space has to say which: {}",
                spaces.len(),
                listed(spaces)
            )),
        };
    };
    if let Ok(id) = wanted.parse::<u8>() {
        if let Some(space) = spaces.iter().find(|space| space.space_id == id) {
            return Ok(space.clone());
        }
    }
    let needle = wanted.to_ascii_lowercase();
    let matched: Vec<&Space> = spaces
        .iter()
        .filter(|space| {
            [&space.model, &space.query, &space.producer]
                .iter()
                .any(|field| field.to_ascii_lowercase().contains(&needle))
        })
        .collect();
    match matched.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(format!(
            "no space of this file is {wanted}: {}",
            listed(spaces)
        )),
        several => Err(format!(
            "{wanted} names {} of this file's spaces: {}",
            several.len(),
            listed(
                &several
                    .iter()
                    .map(|space| (*space).clone())
                    .collect::<Vec<_>>()
            )
        )),
    }
}

/// The spaces of a file, as a line a refusal can end with.
fn listed(spaces: &[Space]) -> String {
    spaces
        .iter()
        .map(|space| {
            format!(
                "{} ({}, {}d, {}{})",
                space.space_id,
                space.encoding.name(),
                space.dims,
                space.modality.name(),
                match space.model.as_str() {
                    "" => String::new(),
                    model => format!(", {model}"),
                }
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The records worth printing, best first.
///
/// The coarse stage is a prefilter and nothing else: it picks `n`
/// candidates by their sign planes and the scores that come out are the
/// rescored ones, so a threshold means the same thing with it and
/// without it. What it changes is which records were rescored at all.
pub fn rank<'a>(
    candidates: &[&'a Candidate],
    query: &[f32],
    coarse: Option<Coarse>,
    threshold: Option<f32>,
    top: Option<usize>,
) -> Result<Vec<(&'a Candidate, f32)>, String> {
    let mut pool: Vec<&Candidate> = candidates.to_vec();
    if let Some(Coarse(want)) = coarse {
        let mut rough: Vec<(&Candidate, f32)> = Vec::with_capacity(pool.len());
        for candidate in &pool {
            rough.push((*candidate, sign_score(candidate, query)?));
        }
        order(&mut rough);
        rough.truncate(want);
        pool = rough.into_iter().map(|(candidate, _)| candidate).collect();
    }
    let mut scored: Vec<(&Candidate, f32)> = Vec::with_capacity(pool.len());
    for candidate in pool {
        let values = candidate.body.values().map_err(|err| err.to_string())?;
        scored.push((candidate, cosine(&values, query)));
    }
    order(&mut scored);
    if let Some(threshold) = threshold {
        scored.retain(|(_, score)| *score >= threshold);
    }
    if let Some(top) = top {
        scored.truncate(top);
    }
    Ok(scored)
}

/// Best first, and where two scores are equal the earlier span first,
/// so the same file and the same query give the same rows every time.
fn order(scored: &mut [(&Candidate, f32)]) {
    scored.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then(a.0.start_ms.cmp(&b.0.start_ms))
            .then(a.0.record_id.cmp(&b.0.record_id))
    });
}

/// The cosine of a full-precision query against a record's sign plane.
///
/// MEASUREMENTS.md: the sign plane is a good first stage, and a searcher
/// holding the query at full precision should score it against the sign
/// bits directly rather than quantizing the query and counting bits,
/// which beat Hamming distance at every candidate count in both
/// measured spaces.
fn sign_score(candidate: &Candidate, query: &[f32]) -> Result<f32, String> {
    let VectorBody::I8(planes) = &candidate.body else {
        return Err(
            "--coarse reads the sign plane, which only the i8 encoding has; this space has none"
                .into(),
        );
    };
    let signs = planes
        .signs()
        .ok_or("--coarse reads the sign plane, and a record here never got one")?;
    let values: Vec<f32> = unpack_signs(signs, planes.dims())
        .into_iter()
        .map(f32::from)
        .collect();
    Ok(cosine(&values, query))
}

/// The cosine of two vectors, zero when either points nowhere.
pub fn cosine(values: &[f32], query: &[f32]) -> f32 {
    let mut dot = 0f32;
    let mut ours = 0f32;
    let mut theirs = 0f32;
    for (value, component) in values.iter().zip(query) {
        dot += value * component;
        ours += value * value;
        theirs += component * component;
    }
    let length = ours.sqrt() * theirs.sqrt();
    if length <= 0.0 {
        return 0.0;
    }
    dot / length
}

/// One result row.
fn row(rank: usize, candidate: &Candidate, score: f32) -> Json {
    let mut members = vec![
        ("rank", number(rank as f64)),
        ("score", Json::Written(format!("{score:.6}"))),
        ("start_t", seconds(candidate.start_ms)),
        ("end_t", seconds(candidate.end_ms)),
        ("space", number(candidate.space_id)),
        ("record_id", number(candidate.record_id)),
    ];
    if let Some(present) = candidate.planes {
        members.push(("planes", planes_row(present)));
    }
    object(members)
}

/// Milliseconds as the seconds a span is usually spoken of in, which is
/// also what `ffrwd` calls `start_t` and `end_t`.
fn seconds(ms: i64) -> Json {
    Json::Written(format!("{:.3}", ms as f64 / 1000.0))
}

/// The query vector, from a file or from standard input.
pub fn read_query(path: &str) -> Result<Vec<f32>, String> {
    let text = if path == "-" {
        use std::io::Read;
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|err| format!("the query on standard input: {err}"))?;
        text
    } else {
        std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?
    };
    query_vector(&text).ok_or_else(|| {
        format!("{path} holds no vector: an array of numbers, base64 of little-endian binary32, or an object with a vector member")
    })
}

/// The first vector in a JSON document or in a document of one JSON
/// value a line, which is what a query written by `ffrwd` to an
/// `.ndjson` destination is.
pub fn query_vector(text: &str) -> Option<Vec<f32>> {
    if let Ok(value) = Json::parse(text) {
        if let Some(values) = vector_in(&value) {
            return Some(values);
        }
    }
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(values) = Json::parse(line).ok().as_ref().and_then(vector_in) {
            return Some(values);
        }
    }
    None
}

/// The vector inside a JSON value, in any of the shapes a query arrives
/// in: the array itself, the base64 a vector track carries, the object
/// a row is, or the first element of a list of rows.
fn vector_in(value: &Json) -> Option<Vec<f32>> {
    match value {
        Json::String(text) => from_base64(text).ok().filter(|values| !values.is_empty()),
        Json::Array(items) if items.is_empty() => None,
        Json::Array(items) => {
            if items.iter().all(|item| item.as_f64().is_some()) {
                return Some(
                    items
                        .iter()
                        .filter_map(|item| item.as_f64())
                        .map(|value| value as f32)
                        .collect(),
                );
            }
            items.iter().find_map(vector_in)
        }
        Json::Object(_) => value
            .get("vector")
            .and_then(vector_in)
            .or_else(|| value.get("query_vectors").and_then(vector_in)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_index_core::message::{Encoding, Modality};
    use ffrwd_index_core::quant::Planes;

    fn space(id: u8, model: &str, query: &str) -> Space {
        let mut space = Space::new(id, 8, Encoding::I8);
        space.modality = Modality::Picture;
        space.model = model.into();
        space.query = query.into();
        space.producer = "a test".into();
        space
    }

    fn candidate(id: u16, values: &[f32], mask: u8) -> Candidate {
        let planes = Planes::quantize(values, 0).expect("quantized").subset(mask);
        Candidate {
            space_id: 1,
            record_id: id,
            start_ms: i64::from(id) * 1000,
            end_ms: i64::from(id) * 1000 + 500,
            planes: Some(planes.present()),
            body: VectorBody::I8(planes),
        }
    }

    fn vectors() -> Vec<Vec<f32>> {
        (0..12)
            .map(|index| {
                (0..8)
                    .map(|k| ((index * 5 + k) as f32 * 0.37).sin())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_ranking_is_a_cosine_over_the_reconstructed_vectors() {
        let all = vectors();
        let pool: Vec<Candidate> = all
            .iter()
            .enumerate()
            .map(|(index, values)| candidate(index as u16, values, 0xff))
            .collect();
        let refs: Vec<&Candidate> = pool.iter().collect();
        let query = &all[3];
        let hits = rank(&refs, query, None, None, None).expect("a ranking");

        // The same thing by hand: every record's reconstruction, scored
        // and sorted, which is the brute force the encoding is measured
        // against.
        let mut brute: Vec<(u16, f32)> = pool
            .iter()
            .map(|candidate| {
                let values = candidate.body.values().expect("a reconstruction");
                (candidate.record_id, cosine(&values, query))
            })
            .collect();
        brute.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let got: Vec<(u16, f32)> = hits
            .iter()
            .map(|(candidate, score)| (candidate.record_id, *score))
            .collect();
        assert_eq!(got, brute);
        assert_eq!(got[0].0, 3, "a vector does not match itself best");
        assert!(got[0].1 > 0.99);
    }

    #[test]
    fn a_threshold_and_a_top_cut_the_same_list() {
        let all = vectors();
        let pool: Vec<Candidate> = all
            .iter()
            .enumerate()
            .map(|(index, values)| candidate(index as u16, values, 0xff))
            .collect();
        let refs: Vec<&Candidate> = pool.iter().collect();
        let query = &all[0];
        let whole = rank(&refs, query, None, None, None).expect("a ranking");
        let cut = rank(&refs, query, None, None, Some(3)).expect("a ranking");
        assert_eq!(cut.len(), 3);
        for (index, (candidate, score)) in cut.iter().enumerate() {
            assert_eq!(candidate.record_id, whole[index].0.record_id);
            assert_eq!(*score, whole[index].1);
        }
        // A threshold keeps a prefix of the same order, and the two
        // together keep the shorter of the two prefixes.
        let level = whole[2].1;
        let over = rank(&refs, query, None, Some(level), None).expect("a ranking");
        assert!(over.len() >= 3, "the threshold dropped its own record");
        assert!(over.iter().all(|(_, score)| *score >= level));
        let both = rank(&refs, query, None, Some(level), Some(2)).expect("a ranking");
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn a_coarse_stage_wide_enough_is_the_whole_search() {
        let all = vectors();
        let pool: Vec<Candidate> = all
            .iter()
            .enumerate()
            .map(|(index, values)| candidate(index as u16, values, 0xff))
            .collect();
        let refs: Vec<&Candidate> = pool.iter().collect();
        let query = &all[5];
        let whole = rank(&refs, query, None, None, None).expect("a ranking");
        let wide = rank(&refs, query, Some(Coarse(pool.len())), None, None).expect("a ranking");
        let ids = |hits: &[(&Candidate, f32)]| -> Vec<u16> {
            hits.iter().map(|(c, _)| c.record_id).collect()
        };
        assert_eq!(ids(&wide), ids(&whole), "a prefilter that filters nothing");

        // And a narrow one is a subset, in the same order the rescore
        // would have put those records in.
        let narrow = rank(&refs, query, Some(Coarse(4)), None, None).expect("a ranking");
        assert_eq!(narrow.len(), 4);
        let kept: Vec<u16> = ids(&narrow);
        let order: Vec<u16> = ids(&whole)
            .into_iter()
            .filter(|id| kept.contains(id))
            .collect();
        assert_eq!(kept, order, "the rescore did not put them back in order");
        for (candidate, score) in &narrow {
            let values = candidate.body.values().expect("a reconstruction");
            assert!(
                (score - cosine(&values, query)).abs() < 1e-6,
                "a coarse score reached the output"
            );
        }
    }

    #[test]
    fn a_coarse_stage_over_a_float_space_is_refused() {
        let candidate = Candidate {
            space_id: 2,
            record_id: 0,
            start_ms: 0,
            end_ms: 1,
            body: VectorBody::F32(vec![1.0, 0.0, 0.0, 0.0]),
            planes: None,
        };
        let refs = vec![&candidate];
        let err =
            rank(&refs, &[1.0, 0.0, 0.0, 0.0], Some(Coarse(1)), None, None).expect_err("a refusal");
        assert!(err.contains("sign plane"), "{err}");
    }

    #[test]
    fn a_space_is_chosen_by_id_or_by_what_it_names() {
        let spaces = vec![
            space(
                1,
                "hf:microsoft/xclip@rev/video_tower.onnx",
                "hf:microsoft/xclip@rev/text_tower.onnx",
            ),
            space(7, "hf:Xenova/all-MiniLM-L6-v2@rev/model.onnx", ""),
        ];
        assert_eq!(choose(&spaces, Some("7")).expect("a space").space_id, 7);
        assert_eq!(
            choose(&spaces, Some("MiniLM")).expect("a space").space_id,
            7
        );
        // The query URI counts as a name of the space too.
        assert_eq!(
            choose(&spaces, Some("text_tower"))
                .expect("a space")
                .space_id,
            1
        );
        // The producer is the same on both, so it names neither.
        let err = choose(&spaces, Some("a test")).expect_err("a refusal");
        assert!(err.contains("names 2"), "{err}");
        let err = choose(&spaces, Some("whisper")).expect_err("a refusal");
        assert!(err.contains("no space"), "{err}");
        let err = choose(&spaces, None).expect_err("a refusal");
        assert!(err.contains("--space"), "{err}");
        assert_eq!(
            choose(&spaces[..1], None).expect("the only space").space_id,
            1
        );
    }

    #[test]
    fn a_query_reads_in_every_shape_one_arrives_in() {
        let want = vec![0.5f32, -0.25, 0.0, 1.0];
        let base64 = "AAAAPwAAgL4AAAAAAACAPw==";
        for text in [
            "[0.5,-0.25,0,1]",
            r#"{"vector":[0.5,-0.25,0,1]}"#,
            r#"{"start_t":0,"end_t":1,"vector":[0.5,-0.25,0,1]}"#,
            r#"[{"vector":[0.5,-0.25,0,1]}]"#,
            r#"{"query_vectors":[{"vector":[0.5,-0.25,0,1]}]}"#,
        ] {
            assert_eq!(query_vector(text), Some(want.clone()), "{text}");
        }
        assert_eq!(query_vector(&format!("\"{base64}\"")), Some(want.clone()));
        // Several rows a line at a time: the first one that holds a
        // vector is the query.
        let ndjson = concat!(
            "# a comment\n",
            r#"{"note":"nothing here"}"#,
            "\n",
            r#"{"vector":[0.5,-0.25,0,1]}"#,
            "\n",
        );
        assert_eq!(query_vector(ndjson), Some(want));
        assert_eq!(query_vector("{}"), None);
        assert_eq!(query_vector("not json"), None);
        assert_eq!(query_vector("[]"), None);
    }
}
