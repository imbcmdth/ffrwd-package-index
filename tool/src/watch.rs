//! `watch`: an elementary stream on standard input, and a row the
//! moment a record matches.
//!
//! This is the live case the `next` placement exists for. The stream
//! grows, never seeks and has no end, so nothing here may wait for one:
//! bytes are cut into carriers as they arrive (`ffrwd_nal::feed::Feed`), each
//! carrier's messages go into the assembler, and only the records that
//! carrier touched are scored. A record that passes the threshold is
//! printed and flushed at once, and is not printed again when a later
//! plane of it arrives: a watcher says a thing has happened, once.
//!
//! Three things a live reader must do that a file reader need not.
//!
//! **Join mid-stream.** Section 3 has every keyframe repeat every space
//! declaration, so a watcher that attached in the middle of a stream is
//! at most one keyframe interval from being able to read anything. The
//! records that arrive in that interval are held, and scored when their
//! declaration turns up.
//!
//! **Stay bounded.** What is held is the assembler's live ceilings: a
//! few thousand records, a few dozen reassemblies, and an orphan wait
//! after which a record whose space never arrived is dropped. The feed
//! holds one carrier. Neither grows with the length of the stream.
//!
//! **Score early.** A record is scored as soon as it has plane 0, and
//! again on every later plane, because the layered encoding means the
//! first message of a record is already a coarse reading of it and the
//! rest are refinements. `--min-planes` is for a watcher that would
//! rather wait than act on a coarse score.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use ffrwd_index_core::assemble::{Assembler, Limits};
use ffrwd_index_core::carriage;
use ffrwd_index_core::message::{Message, Space, Unit, VectorBody};
use ffrwd_index_rows::json::{number, object, string, Json};
use ffrwd_nal::feed::StreamKind;

use crate::search::{cosine, read_query};
use crate::{planes_row, Flags, Stream};

/// How many bytes are asked of the stream at a time.
///
/// Small on purpose: this is the granularity at which a watcher hears
/// about a carrier, and a pipe hands back what it has rather than
/// waiting to fill the buffer.
const CHUNK: usize = 64 * 1024;

/// One query, and the vector it is.
#[derive(Debug)]
struct Query {
    name: String,
    values: Vec<f32>,
}

pub fn watch(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse_repeating(args, &["query"])?;
    flags.only(&["video", "codec", "fps", "query", "threshold", "min-planes"])?;
    let video = flags.need("video")?;
    let fps = crate::fps_of(&flags)?;
    let wanted = flags.need("threshold")?;
    let threshold: f32 = wanted
        .parse()
        .map_err(|_| format!("{wanted} is not a cosine threshold"))?;
    let min_planes: u8 = match flags.get("min-planes") {
        None => 1,
        Some(text) => text
            .parse::<u8>()
            .map_err(|_| format!("{text} is not a number of planes"))?
            .clamp(1, 8),
    };
    let kind = match crate::codec_of(video, flags.get("codec"))? {
        Stream::Nal(codec) => StreamKind::Nal(codec),
        Stream::Av1 => StreamKind::Av1,
    };
    let queries = read_queries(&flags)?;
    if queries.is_empty() {
        return Err("watch wants at least one --query NAME=FILE".into());
    }

    let reader: Box<dyn Read> = if video == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(video).map_err(|err| format!("{video}: {err}"))?)
    };
    run(reader, kind, fps, &queries, threshold, min_planes)
}

/// `--query NAME=FILE`, as many times as the caller gave it.
fn read_queries(flags: &Flags) -> Result<Vec<Query>, String> {
    let mut queries = Vec::new();
    for pair in flags.every("query") {
        let (name, path) = pair
            .split_once('=')
            .ok_or_else(|| format!("{pair} is not NAME=FILE"))?;
        if name.is_empty() {
            return Err(format!("{pair} has no name in front of its file"));
        }
        queries.push(Query {
            name: name.to_string(),
            values: read_query(path)?,
        });
    }
    Ok(queries)
}

/// The whole of a watch: bytes in, rows out.
fn run(
    mut reader: Box<dyn Read>,
    kind: StreamKind,
    fps: f64,
    queries: &[Query],
    threshold: f32,
    min_planes: u8,
) -> Result<(), String> {
    let mut feed = carriage::feed(kind);
    // The live ceilings, not a file reader's: this holds while a stream
    // runs and nothing here may grow with it.
    let mut assembler = Assembler::new(Limits::default());
    let mut declared: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
    let mut said: BTreeSet<(usize, u8, u16)> = BTreeSet::new();
    let mut matches = 0usize;
    let mut carriers = 0usize;

    let mut buffer = vec![0u8; CHUNK];
    loop {
        let got = reader
            .read(&mut buffer)
            .map_err(|err| format!("the stream: {err}"))?;
        let carried = if got == 0 {
            feed.finish().map_err(|err| err.to_string())?
        } else {
            feed.push(&buffer[..got]).map_err(|err| err.to_string())?
        };
        for carrier in carried {
            carriers += 1;
            let time_ms = crate::pts_ms(carrier.index as usize, fps);
            matches += carry(
                &mut assembler,
                &mut declared,
                &mut said,
                &carrier.payloads,
                time_ms,
                queries,
                threshold,
                min_planes,
            )?;
        }
        if got == 0 {
            break;
        }
    }

    eprintln!(
        "{carriers} carriers, {} spaces, {matches} matches at or above {threshold}, \
         {} records still held, {} messages dropped",
        declared.len(),
        assembler.len(),
        assembler.dropped()
    );
    if feed.dropped() > 0 {
        eprintln!(
            "{} carriers were given up on for growing past the feed's limit",
            feed.dropped()
        );
    }
    Ok(())
}

/// One carrier's units, and the rows they turn into.
#[allow(clippy::too_many_arguments)]
fn carry(
    assembler: &mut Assembler,
    declared: &mut BTreeMap<u8, Vec<u8>>,
    said: &mut BTreeSet<(usize, u8, u16)>,
    units: &[Vec<u8>],
    time_ms: i64,
    queries: &[Query],
    threshold: f32,
    min_planes: u8,
) -> Result<usize, String> {
    let mut touched: Vec<(u8, u16)> = Vec::new();
    let mut arrived: Vec<Space> = Vec::new();
    for bytes in units {
        let Ok(unit) = Unit::decode(bytes) else {
            continue;
        };
        for message in &unit.messages {
            match message {
                // A space is declared on every keyframe, so only a
                // declaration that is new or has changed is news: it is
                // the one that lets records already held be read.
                Message::Space(space) => {
                    let encoded = space.encode();
                    if declared.insert(space.space_id, encoded.clone()) != Some(encoded) {
                        arrived.push(space.clone());
                    }
                }
                Message::Vector(record) => touched.push((record.space_id, record.record_id)),
                Message::Fragment(fragment) => {
                    touched.push((fragment.space_id, fragment.record_id))
                }
                Message::Unknown { .. } => {}
            }
        }
        assembler.push_unit(time_ms, &unit);
    }

    let mut matches = 0usize;
    // The records that were waiting for a declaration that just came.
    for space in &arrived {
        for record in assembler.records_of(space.space_id) {
            matches += report(&record, said, queries, threshold, min_planes, time_ms)?;
        }
    }
    for (space_id, record_id) in touched {
        if arrived.iter().any(|space| space.space_id == space_id) {
            continue; // already looked at, and saying so twice is a bug
        }
        let Some(record) = assembler.record(space_id, record_id) else {
            continue;
        };
        matches += report(&record, said, queries, threshold, min_planes, time_ms)?;
    }
    Ok(matches)
}

/// Every query this record passes and has not already passed.
fn report(
    record: &ffrwd_index_core::assemble::Record,
    said: &mut BTreeSet<(usize, u8, u16)>,
    queries: &[Query],
    threshold: f32,
    min_planes: u8,
    carrier_ms: i64,
) -> Result<usize, String> {
    if let VectorBody::I8(planes) = &record.body {
        // The longest run starting at plane 0 is what a reader may use,
        // so that is what `--min-planes` counts.
        match planes.prefix() {
            Some(prefix) if prefix + 1 >= min_planes => {}
            _ => return Ok(0),
        }
    }
    let Ok(values) = record.values() else {
        return Ok(0);
    };
    let mut matches = 0usize;
    for (index, query) in queries.iter().enumerate() {
        if query.values.len() != values.len() {
            continue; // a query of another space's width
        }
        let key = (index, record.space.space_id, record.record_id);
        if said.contains(&key) {
            continue;
        }
        let score = cosine(&values, &query.values);
        if score < threshold {
            continue;
        }
        said.insert(key);
        print_row(&query.name, record, score, carrier_ms)?;
        matches += 1;
    }
    Ok(matches)
}

/// One match, flushed: a watcher's output is read as it is written.
fn print_row(
    name: &str,
    record: &ffrwd_index_core::assemble::Record,
    score: f32,
    carrier_ms: i64,
) -> Result<(), String> {
    let mut members = vec![
        ("query", string(name)),
        ("score", Json::Written(format!("{score:.6}"))),
        ("start_t", seconds(record.start_ms)),
        ("end_t", seconds(record.end_ms)),
        ("carrier_t", seconds(carrier_ms)),
        ("space", number(record.space.space_id)),
        ("record_id", number(record.record_id)),
    ];
    if let Some(present) = record.planes {
        members.push(("planes", planes_row(present)));
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", object(members).write()).map_err(|err| format!("stdout: {err}"))?;
    out.flush().map_err(|err| format!("stdout: {err}"))
}

fn seconds(ms: i64) -> Json {
    Json::Written(format!("{:.3}", ms as f64 / 1000.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_pair_is_a_name_and_a_file() {
        let args: Vec<String> = ["--query", "dog=nowhere.json"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let flags = Flags::parse_repeating(&args, &["query"]).expect("flags");
        assert_eq!(flags.every("query"), vec!["dog=nowhere.json"]);
        let err = read_queries(&flags).expect_err("a refusal");
        assert!(err.contains("nowhere.json"), "{err}");

        let args: Vec<String> = ["--query", "nofile"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let flags = Flags::parse_repeating(&args, &["query"]).expect("flags");
        let err = read_queries(&flags).expect_err("a refusal");
        assert!(err.contains("NAME=FILE"), "{err}");
    }

    #[test]
    fn a_flag_that_may_repeat_repeats_and_the_others_still_may_not() {
        let args: Vec<String> = [
            "--query",
            "a=1.json",
            "--query",
            "b=2.json",
            "--threshold",
            "0.3",
        ]
        .iter()
        .map(|value| value.to_string())
        .collect();
        let flags = Flags::parse_repeating(&args, &["query"]).expect("flags");
        assert_eq!(flags.every("query"), vec!["a=1.json", "b=2.json"]);
        assert_eq!(flags.get("threshold"), Some("0.3"));
        // The last of a repeated flag is what `get` gives, which is what
        // `only` and the refusals want.
        assert_eq!(flags.get("query"), Some("b=2.json"));

        let twice: Vec<String> = ["--fps", "30", "--fps", "60"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        assert!(Flags::parse_repeating(&twice, &["query"]).is_err());
    }
}
