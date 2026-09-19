//! Weave vectors into an elementary stream, and read them back.
//!
//! The tool is a thin native shell over `ffrwd-index-core`: it reads
//! files, turns rows of JSON into records, and prints records back as
//! rows of JSON. Every decision about bytes is the library's.
//!
//! Only Annex B elementary streams are handled here. MP4 and Matroska
//! are the next pass; ffmpeg converts either way with `-c copy` and the
//! bitstream filters, which `README.md` spells out.

mod json;

use std::collections::BTreeMap;
use std::path::Path;

use json::{float, number, object, string, Json};

use ffrwd_index_core::assemble::{Assembler, Limits, Record};
use ffrwd_index_core::avc::{self, Codec};
use ffrwd_index_core::index::FileIndex;
use ffrwd_index_core::message::{
    Encoding, Message, Modality, Space, Unit, VectorBody, VectorRecord,
};
use ffrwd_index_core::placement::{plan, Carrier, Mode, Pending, Placement};
use ffrwd_index_core::quant::{f32_to_f16, Planes};

const USAGE: &str = "\
ffrwd-index: embedding vectors in a video's own elementary stream.

    ffrwd-index weave --video IN --vectors ROWS.ndjson --out OUT
                      [--placement keyframe|next|spread:BYTES]
                      [--fps N] [--codec h264|h265] [--live]

    ffrwd-index read  --video IN [--fps N] [--codec h264|h265]
                      [--index OUT.ffix]

    ffrwd-index read  --index IN.ffix

IN and OUT are H.264 or HEVC Annex B elementary streams; the codec is
taken from the file name unless --codec says otherwise. An elementary
stream carries no timestamps, so --fps (30 by default) is what gives
each access unit a presentation time.

The row shapes are in tool/README.md.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = run(&args) {
        eprintln!("ffrwd-index: {message}");
        std::process::exit(2);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("weave") => weave(&args[1..]),
        Some("read") => read(&args[1..]),
        None | Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("{other} is not a command. Try --help")),
    }
}

// ---------------------------------------------------------------- //
// Arguments.
// ---------------------------------------------------------------- //

/// The flags of one command.
struct Flags {
    values: BTreeMap<String, String>,
    switches: Vec<String>,
}

impl Flags {
    /// Reads `--name value` pairs, and the switches that take none.
    fn parse(args: &[String], switches: &[&str]) -> Result<Flags, String> {
        let mut values = BTreeMap::new();
        let mut set = Vec::new();
        let mut at = 0usize;
        while at < args.len() {
            let name = args[at]
                .strip_prefix("--")
                .ok_or_else(|| format!("{} is not a flag", args[at]))?
                .to_string();
            at += 1;
            if switches.contains(&name.as_str()) {
                set.push(name);
                continue;
            }
            let value = args
                .get(at)
                .ok_or_else(|| format!("--{name} wants a value"))?
                .clone();
            at += 1;
            if values.insert(name.clone(), value).is_some() {
                return Err(format!("--{name} was given twice"));
            }
        }
        Ok(Flags {
            values,
            switches: set,
        })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn need(&self, name: &str) -> Result<&str, String> {
        self.get(name).ok_or_else(|| format!("--{name} is needed"))
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|switch| switch == name)
    }

    /// Refuses a flag this command does not know, rather than ignoring
    /// it and doing something else than was asked.
    fn only(&self, known: &[&str]) -> Result<(), String> {
        for name in self.values.keys().chain(self.switches.iter()) {
            if !known.contains(&name.as_str()) {
                return Err(format!("--{name} is not a flag of this command"));
            }
        }
        Ok(())
    }
}

/// The codec of a file, by its name or by what the caller said.
fn codec_of(path: &str, named: Option<&str>) -> Result<Codec, String> {
    if let Some(named) = named {
        return match named {
            "h264" | "avc" => Ok(Codec::H264),
            "h265" | "hevc" => Ok(Codec::H265),
            other => Err(format!("{other} is not h264 or h265")),
        };
    }
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "h264" | "264" | "avc" => Ok(Codec::H264),
        "h265" | "265" | "hevc" => Ok(Codec::H265),
        _ => Err(format!(
            "{path} does not name its codec. Pass --codec h264 or --codec h265"
        )),
    }
}

/// Frames a second, which is what gives an elementary stream its times.
fn fps_of(flags: &Flags) -> Result<f64, String> {
    let text = flags.get("fps").unwrap_or("30");
    let value: f64 = text
        .parse()
        .map_err(|_| format!("{text} is not a frame rate"))?;
    if !(value.is_finite() && value > 0.0) {
        return Err(format!("{text} is not a frame rate"));
    }
    Ok(value)
}

/// The presentation time of the carrier at `index`.
fn pts_ms(index: usize, fps: f64) -> i64 {
    (index as f64 * 1000.0 / fps).round() as i64
}

fn placement_of(text: &str) -> Result<Placement, String> {
    match text {
        "keyframe" => Ok(Placement::Keyframe),
        "next" => Ok(Placement::Next),
        other => match other.strip_prefix("spread:") {
            Some(budget) => budget
                .parse::<usize>()
                .map(|budget_bytes| Placement::Spread { budget_bytes })
                .map_err(|_| format!("{budget} is not a number of bytes")),
            None => Err(format!("{other} is not keyframe, next or spread:BYTES")),
        },
    }
}

// ---------------------------------------------------------------- //
// weave.
// ---------------------------------------------------------------- //

fn weave(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args, &["live"])?;
    flags.only(&[
        "video",
        "vectors",
        "out",
        "placement",
        "fps",
        "codec",
        "live",
    ])?;
    let video = flags.need("video")?;
    let vectors = flags.need("vectors")?;
    let out = flags.need("out")?;
    let codec = codec_of(video, flags.get("codec"))?;
    let fps = fps_of(&flags)?;
    let policy = placement_of(flags.get("placement").unwrap_or("keyframe"))?;
    let mode = if flags.has("live") {
        Mode::Live
    } else {
        Mode::File
    };

    let stream = std::fs::read(video).map_err(|err| format!("{video}: {err}"))?;
    let rows = std::fs::read_to_string(vectors).map_err(|err| format!("{vectors}: {err}"))?;
    let (spaces, records) = read_rows(&rows)?;

    let units = avc::access_units(&stream, codec);
    if units.is_empty() {
        return Err(format!("{video} holds no access units"));
    }
    let carriers: Vec<Carrier> = units
        .iter()
        .enumerate()
        .map(|(index, unit)| Carrier {
            pts_ms: pts_ms(index, fps),
            keyframe: unit.keyframe,
        })
        .collect();

    let planned = plan(policy, mode, &spaces, &records, &carriers);
    let mut woven = Vec::with_capacity(stream.len() + 4096);
    let mut at = 0usize;
    let mut written = 0usize;
    for (unit, messages) in units.iter().zip(&planned) {
        if messages.is_empty() {
            continue;
        }
        let bytes = Unit::new(messages.clone()).encode();
        woven.extend_from_slice(&stream[at..unit.insert_at]);
        woven.extend_from_slice(&[0, 0, 0, 1]);
        woven.extend_from_slice(&avc::wrap_unit_at(&bytes, codec, unit.temporal_id_plus1));
        at = unit.insert_at;
        written += 1;
    }
    woven.extend_from_slice(&stream[at..]);
    std::fs::write(out, &woven).map_err(|err| format!("{out}: {err}"))?;

    eprintln!(
        "{out}: {} access units, {written} carrying {} records in {} spaces, {} bytes added",
        units.len(),
        records.len(),
        spaces.len(),
        woven.len() - stream.len()
    );
    Ok(())
}

/// The spaces and records of an NDJSON file.
fn read_rows(text: &str) -> Result<(Vec<Space>, Vec<Pending>), String> {
    let mut spaces: BTreeMap<u8, Space> = BTreeMap::new();
    let mut records = Vec::new();
    let mut next_id: BTreeMap<u8, u32> = BTreeMap::new();

    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let row = Json::parse(line).map_err(|err| format!("line {}: {err}", number + 1))?;
        let at = |err: String| format!("line {}: {err}", number + 1);
        if let Some(descriptor) = row.get("space") {
            let space = read_space(descriptor).map_err(at)?;
            spaces.insert(space.space_id, space);
            continue;
        }
        let space_id = u8::try_from(
            row.get("space_id")
                .and_then(Json::as_i64)
                .ok_or_else(|| at("a row with no space_id".into()))?,
        )
        .map_err(|_| at("a space_id outside 0 to 255".into()))?;
        let space = spaces
            .get(&space_id)
            .ok_or_else(|| at(format!("space {space_id} has not been declared")))?;
        let record = read_vector(&row, space, next_id.entry(space_id).or_insert(0)).map_err(at)?;
        records.push(record);
    }
    if spaces.is_empty() {
        return Err("the rows declare no space".into());
    }
    Ok((spaces.into_values().collect(), records))
}

/// A space descriptor row.
fn read_space(row: &Json) -> Result<Space, String> {
    let id = row
        .get("id")
        .or_else(|| row.get("space_id"))
        .and_then(Json::as_i64)
        .ok_or("a space with no id")?;
    let space_id = u8::try_from(id).map_err(|_| "a space id outside 0 to 255")?;
    let dims = row
        .get("dims")
        .and_then(Json::as_i64)
        .ok_or("a space with no dims")?;
    let dims = u32::try_from(dims).map_err(|_| "dims that are not a count")?;
    let encoding = match row.get("encoding").and_then(Json::as_str).unwrap_or("i8") {
        "f32" => Encoding::F32,
        "f16" => Encoding::F16,
        "i8" => Encoding::I8,
        other => return Err(format!("{other} is not f32, f16 or i8")),
    };
    let mut space = Space::new(space_id, dims, encoding);
    if row.get("unit_length").and_then(Json::as_bool) == Some(true) {
        space.flags |= ffrwd_index_core::message::FLAG_UNIT_LENGTH;
    }
    space.modality = match row.get("modality") {
        None | Some(Json::Null) => Modality::Unspecified,
        Some(Json::Number(_)) => Modality::from_u8(
            u8::try_from(row.get("modality").and_then(Json::as_i64).unwrap_or(0))
                .map_err(|_| "a modality outside 0 to 255")?,
        ),
        Some(value) => modality_of(value.as_str().ok_or("a modality that is not a name")?)?,
    };
    if let Some(source) = row.get("source").and_then(Json::as_i64) {
        space.source = u8::try_from(source).map_err(|_| "a source outside 0 to 255")?;
    }
    space.model = row
        .get("model")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    space.query = row
        .get("query")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    space.producer = row
        .get("producer")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    space.model_hash = hash_of(row.get("model_hash"))?;
    space.query_hash = hash_of(row.get("query_hash"))?;
    if space.dims == 0 || space.dims > ffrwd_index_core::MAX_DIMS {
        return Err(format!("{} dimensions is outside the format", space.dims));
    }
    Ok(space)
}

/// The first sixteen bytes of a SHA-256, as hex, or all zero.
fn hash_of(value: Option<&Json>) -> Result<[u8; 16], String> {
    let Some(text) = value.and_then(Json::as_str) else {
        return Ok([0; 16]);
    };
    if text.is_empty() {
        return Ok([0; 16]);
    }
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if digits.len() < 32 {
        return Err("a hash shorter than sixteen bytes of hex".into());
    }
    let mut out = [0u8; 16];
    for (index, byte) in out.iter_mut().enumerate() {
        let pair = std::str::from_utf8(&digits[index * 2..index * 2 + 2])
            .map_err(|_| "a hash that is not hex")?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| "a hash that is not hex")?;
    }
    Ok(out)
}

fn modality_of(name: &str) -> Result<Modality, String> {
    match name {
        "unspecified" => Ok(Modality::Unspecified),
        "picture" => Ok(Modality::Picture),
        "sound" => Ok(Modality::Sound),
        "speech" => Ok(Modality::Speech),
        "sound-text" => Ok(Modality::SoundText),
        "scene-text" => Ok(Modality::SceneText),
        "description" => Ok(Modality::Description),
        other => Err(format!("{other} is not a modality this format names")),
    }
}

/// A vector row, in its space's encoding.
fn read_vector(row: &Json, space: &Space, next_id: &mut u32) -> Result<Pending, String> {
    let values: Vec<f32> = row
        .get("vector")
        .and_then(Json::as_array)
        .ok_or("a row with no vector")?
        .iter()
        .map(|value| {
            value
                .as_f64()
                .map(|number| number as f32)
                .ok_or("a vector component that is not a number")
        })
        .collect::<Result<_, _>>()?;
    if values.len() as u32 != space.dims {
        return Err(format!(
            "a vector of {} components in a space of {}",
            values.len(),
            space.dims
        ));
    }
    let start_ms = row
        .get("start_ms")
        .and_then(Json::as_i64)
        .ok_or("a row with no start_ms")?;
    let end_ms = row
        .get("end_ms")
        .and_then(Json::as_i64)
        .ok_or("a row with no end_ms")?;
    if end_ms < start_ms {
        return Err("a span that ends before it starts".into());
    }
    let record_id = match row.get("record_id").and_then(Json::as_i64) {
        Some(id) => u16::try_from(id).map_err(|_| "a record_id outside 0 to 65535")?,
        None => {
            let id = *next_id % ffrwd_index_core::RECORD_ID_WRAP;
            *next_id += 1;
            id as u16
        }
    };
    let available_ms = row
        .get("available_ms")
        .and_then(Json::as_i64)
        .unwrap_or(end_ms);

    let mut pending = match space.encoding {
        Encoding::I8 => {
            let planes = Planes::quantize(&values).map_err(|err| err.to_string())?;
            Pending::layered(space.space_id, record_id, start_ms, end_ms, &planes)
        }
        Encoding::F32 => {
            let body = values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            Pending::whole(space.space_id, record_id, start_ms, end_ms, body)
        }
        Encoding::F16 => {
            let body = values
                .iter()
                .flat_map(|value| f32_to_f16(*value).to_le_bytes())
                .collect();
            Pending::whole(space.space_id, record_id, start_ms, end_ms, body)
        }
        Encoding::Other(other) => return Err(format!("encoding {other} cannot be written")),
    };
    pending.available_ms = available_ms;
    Ok(pending)
}

// ---------------------------------------------------------------- //
// read.
// ---------------------------------------------------------------- //

fn read(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args, &[])?;
    flags.only(&["video", "fps", "codec", "index"])?;
    match (flags.get("video"), flags.get("index")) {
        (None, Some(path)) => dump_index(path),
        (None, None) => Err("read wants --video or --index".into()),
        (Some(video), index) => read_stream(&flags, video, index),
    }
}

fn read_stream(flags: &Flags, video: &str, index: Option<&str>) -> Result<(), String> {
    let codec = codec_of(video, flags.get("codec"))?;
    let fps = fps_of(flags)?;
    let stream = std::fs::read(video).map_err(|err| format!("{video}: {err}"))?;

    let mut assembler = Assembler::new(Limits {
        // A file is read whole, so the ceilings are the ones a file
        // reader wants rather than a live reader's.
        max_records: 1 << 20,
        max_reassemblies: 4096,
        orphan_wait_ms: i64::MAX,
    });
    let mut spaces: Vec<(u32, Space)> = Vec::new();
    for (index, unit) in avc::access_units(&stream, codec).iter().enumerate() {
        let time = pts_ms(index, fps);
        for bytes in avc::units_annexb(&stream[unit.start..unit.end], codec) {
            let unit = match Unit::decode(&bytes) {
                Ok(unit) => unit,
                // A unit of a version this build does not know is not
                // an error: it is somebody ahead of us.
                Err(_) => continue,
            };
            for message in &unit.messages {
                if let Message::Space(space) = message {
                    spaces.push((time as u32, space.clone()));
                }
            }
            assembler.push_unit(time, &unit);
        }
    }

    let mut out = String::new();
    let mut seen: BTreeMap<u8, Space> = BTreeMap::new();
    for (_, space) in &spaces {
        if seen.insert(space.space_id, space.clone()).is_none() {
            out.push_str(&object(vec![("space", space_row(space))]).write());
            out.push('\n');
        }
    }
    let records = assembler.records();
    for record in &records {
        out.push_str(&record_row(record)?.write());
        out.push('\n');
    }
    print!("{out}");

    if let Some(path) = index {
        // Section 8 holds SPACE and VECTOR messages only, so what goes
        // in is the records as the assembler put them back together,
        // each against the carrier its first message rode.
        let mut pairs: Vec<(u32, Message)> = spaces
            .into_iter()
            .map(|(time, space)| (time, Message::Space(space)))
            .collect();
        for record in &records {
            pairs.push((
                record.carrier_ms.max(0) as u32,
                Message::Vector(VectorRecord {
                    space_id: record.space.space_id,
                    record_id: record.record_id,
                    start_off: offset(record.start_ms - record.carrier_ms)?,
                    end_off: offset(record.end_ms - record.carrier_ms)?,
                    body: record.body.encode(),
                }),
            ));
        }
        pairs.sort_by_key(|(time, message)| (*time, message.kind()));
        let built = FileIndex::build(pairs);
        std::fs::write(path, built.encode()).map_err(|err| format!("{path}: {err}"))?;
        eprintln!("{path}: {} entries", built.entries.len());
    }
    if assembler.dropped() > 0 {
        eprintln!(
            "{video}: {} messages were dropped as unreadable",
            assembler.dropped()
        );
    }
    Ok(())
}

fn dump_index(path: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|err| format!("{path}: {err}"))?;
    let index = FileIndex::parse(&bytes).map_err(|err| format!("{path}: {err}"))?;
    let mut spaces: BTreeMap<u8, Space> = BTreeMap::new();
    let mut out = String::new();
    for entry in &index.entries {
        match &entry.message {
            Message::Space(space) => {
                spaces.insert(space.space_id, space.clone());
                out.push_str(
                    &object(vec![
                        ("time_ms", number(entry.time_ms)),
                        ("space", space_row(space)),
                    ])
                    .write(),
                );
            }
            Message::Vector(record) => {
                let mut members = vec![
                    ("time_ms", number(entry.time_ms)),
                    ("space_id", number(record.space_id)),
                    ("record_id", number(record.record_id)),
                    (
                        "start_ms",
                        number(i64::from(entry.time_ms) as f64 + f64::from(record.start_off)),
                    ),
                    (
                        "end_ms",
                        number(i64::from(entry.time_ms) as f64 + f64::from(record.end_off)),
                    ),
                ];
                // The vector needs its space, which the index declares
                // before it uses it. Without one, say how many bytes
                // are there rather than guessing at them.
                match spaces.get(&record.space_id) {
                    Some(space) => {
                        let body = record
                            .decode_body(space)
                            .map_err(|err| format!("{path}: {err}"))?;
                        if let VectorBody::I8(planes) = &body {
                            members.push(("planes", planes_row(planes.present())));
                        }
                        let values = body.values().map_err(|err| format!("{path}: {err}"))?;
                        members.push(("vector", vector_row(&values)));
                    }
                    None => members.push(("body_bytes", number(record.body.len() as u32))),
                }
                out.push_str(&object(members).write());
            }
            _ => continue,
        }
        out.push('\n');
    }
    print!("{out}");
    Ok(())
}

fn offset(value: i64) -> Result<i32, String> {
    i32::try_from(value).map_err(|_| "a span too far from its carrier to write".to_string())
}

fn space_row(space: &Space) -> Json {
    object(vec![
        ("id", number(space.space_id)),
        ("dims", number(space.dims)),
        ("encoding", string(space.encoding.name())),
        ("unit_length", Json::Bool(space.unit_length())),
        ("modality", string(space.modality.name())),
        ("source", number(space.source)),
        ("model", string(space.model.clone())),
        ("model_hash", string(hex(&space.model_hash))),
        ("query", string(space.query.clone())),
        ("query_hash", string(hex(&space.query_hash))),
        ("producer", string(space.producer.clone())),
    ])
}

fn record_row(record: &Record) -> Result<Json, String> {
    let values = record.values().map_err(|err| err.to_string())?;
    let mut members = vec![
        ("space_id", number(record.space.space_id)),
        ("record_id", number(record.record_id)),
        ("carrier_ms", number(record.carrier_ms as f64)),
        ("start_ms", number(record.start_ms as f64)),
        ("end_ms", number(record.end_ms as f64)),
    ];
    if let Some(present) = record.planes {
        members.push(("planes", planes_row(present)));
    }
    members.push(("vector", vector_row(&values)));
    Ok(object(members))
}

/// Which planes arrived, as their numbers.
fn planes_row(present: u8) -> Json {
    Json::Array(
        (0..8)
            .filter(|plane| present >> plane & 1 == 1)
            .map(number)
            .collect(),
    )
}

fn vector_row(values: &[f32]) -> Json {
    Json::Array(values.iter().copied().map(float).collect())
}

fn hex(bytes: &[u8; 16]) -> String {
    if bytes.iter().all(|byte| *byte == 0) {
        return String::new();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_row_reads_every_field() {
        let row = Json::parse(
            r#"{"space":{"id":3,"dims":4,"encoding":"f32","unit_length":true,
                "modality":"speech","source":2,"model":"hf:a/b@c/d.safetensors",
                "model_hash":"000102030405060708090a0b0c0d0e0f","query":"hf:q",
                "query_hash":"","producer":"a test"}}"#,
        )
        .expect("a row");
        let space = read_space(row.get("space").expect("a space")).expect("a descriptor");
        assert_eq!(space.space_id, 3);
        assert_eq!(space.dims, 4);
        assert_eq!(space.encoding, Encoding::F32);
        assert!(space.unit_length());
        assert_eq!(space.modality, Modality::Speech);
        assert_eq!(space.source, 2);
        assert_eq!(space.model, "hf:a/b@c/d.safetensors");
        assert_eq!(space.model_hash[..4], [0, 1, 2, 3]);
        assert_eq!(space.query_model(), "hf:q");
        assert_eq!(space.query_hash, [0; 16]);
        assert_eq!(space.producer, "a test");
        // And back out again as the same row.
        let written = space_row(&space);
        assert_eq!(
            read_space(&written).expect("a descriptor again"),
            space,
            "{}",
            written.write()
        );
    }

    #[test]
    fn a_modality_may_be_a_name_or_a_number() {
        let by_name = Json::parse(r#"{"id":1,"dims":2,"modality":"scene-text"}"#).expect("a row");
        assert_eq!(
            read_space(&by_name).expect("a space").modality,
            Modality::SceneText
        );
        let by_number = Json::parse(r#"{"id":1,"dims":2,"modality":9}"#).expect("a row");
        assert_eq!(
            read_space(&by_number).expect("a space").modality,
            Modality::Other(9)
        );
    }

    #[test]
    fn rows_become_spaces_and_records() {
        let text = concat!(
            "# a comment line, and a blank one\n",
            "\n",
            r#"{"space":{"id":1,"dims":4,"encoding":"i8","model":"m"}}"#,
            "\n",
            r#"{"space_id":1,"start_ms":0,"end_ms":500,"vector":[1,-1,0.5,0]}"#,
            "\n",
            r#"{"space_id":1,"start_ms":500,"end_ms":1000,"vector":[0,1,-1,0.25]}"#,
            "\n",
            r#"{"space_id":1,"record_id":40,"start_ms":0,"end_ms":10,"vector":[0,0,0,0],"available_ms":900}"#,
            "\n",
        );
        let (spaces, records) = read_rows(text).expect("rows");
        assert_eq!(spaces.len(), 1);
        assert_eq!(records.len(), 3);
        // Ids count up per space where a row does not give one.
        assert_eq!(records[0].record_id, 0);
        assert_eq!(records[1].record_id, 1);
        assert_eq!(records[2].record_id, 40);
        assert_eq!(records[2].available_ms, 900);
        // The layered encoding goes out as eight bodies, one a plane.
        assert_eq!(records[0].bodies.len(), 8);
    }

    #[test]
    fn broken_rows_say_which_line_and_why() {
        let cases = [
            (
                r#"{"space_id":1,"start_ms":0,"end_ms":1,"vector":[1]}"#,
                "declared",
            ),
            (
                concat!(
                    r#"{"space":{"id":1,"dims":2}}"#,
                    "\n",
                    r#"{"space_id":1,"start_ms":0,"end_ms":1,"vector":[1]}"#
                ),
                "components",
            ),
            (
                concat!(
                    r#"{"space":{"id":1,"dims":2}}"#,
                    "\n",
                    r#"{"space_id":1,"start_ms":10,"end_ms":1,"vector":[1,2]}"#
                ),
                "ends before",
            ),
            (r#"{"space":{"id":1}}"#, "dims"),
            (r#"{"space":{"id":1,"dims":0}}"#, "outside the format"),
            (r#"{"space":{"id":1,"dims":2,"encoding":"f8"}}"#, "not f32"),
            ("{not json", "line 1"),
        ];
        for (text, wanted) in cases {
            let err = read_rows(text).expect_err("a refusal");
            assert!(err.contains(wanted), "{text} gave {err}");
        }
    }

    #[test]
    fn a_hash_reads_from_hex_and_writes_back() {
        assert_eq!(hash_of(None).expect("zeroes"), [0; 16]);
        assert_eq!(hash_of(Some(&string(""))).expect("zeroes"), [0; 16]);
        let text = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
        let bytes = hash_of(Some(&string(text))).expect("a hash");
        assert_eq!(bytes[0], 0x0f);
        assert_eq!(hex(&bytes), text);
        assert_eq!(hex(&[0; 16]), "");
        assert!(hash_of(Some(&string("00"))).is_err());
        assert!(hash_of(Some(&string("zz1e2d3c4b5a69788796a5b4c3d2e1f0"))).is_err());
    }

    #[test]
    fn the_flags_of_a_command_are_checked() {
        let args: Vec<String> = ["--video", "a.h264", "--live"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let flags = Flags::parse(&args, &["live"]).expect("flags");
        assert_eq!(flags.get("video"), Some("a.h264"));
        assert!(flags.has("live"));
        assert!(flags.only(&["video", "live"]).is_ok());
        assert!(flags.only(&["video"]).is_err());
        assert!(flags.need("out").is_err());

        let dangling: Vec<String> = vec!["--video".into()];
        assert!(Flags::parse(&dangling, &[]).is_err());
        let bare: Vec<String> = vec!["video".into()];
        assert!(Flags::parse(&bare, &[]).is_err());
        let twice: Vec<String> = ["--fps", "30", "--fps", "60"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        assert!(Flags::parse(&twice, &[]).is_err());
    }

    #[test]
    fn the_codec_comes_from_the_name_or_the_flag() {
        assert_eq!(codec_of("a.h264", None).expect("a codec"), Codec::H264);
        assert_eq!(codec_of("a.265", None).expect("a codec"), Codec::H265);
        assert_eq!(
            codec_of("a.bin", Some("hevc")).expect("a codec"),
            Codec::H265
        );
        assert!(codec_of("a.bin", None).is_err());
        assert!(codec_of("a.h264", Some("vp9")).is_err());
    }

    #[test]
    fn the_placement_flag_reads_its_three_shapes() {
        assert_eq!(
            placement_of("keyframe").expect("a policy"),
            Placement::Keyframe
        );
        assert_eq!(placement_of("next").expect("a policy"), Placement::Next);
        assert_eq!(
            placement_of("spread:512").expect("a policy"),
            Placement::Spread { budget_bytes: 512 }
        );
        assert!(placement_of("spread").is_err());
        assert!(placement_of("spread:lots").is_err());
    }

    #[test]
    fn times_come_from_the_frame_rate() {
        assert_eq!(pts_ms(0, 30.0), 0);
        assert_eq!(pts_ms(30, 30.0), 1000);
        assert_eq!(pts_ms(30, 29.97), 1001);
        let flags = Flags::parse(&["--fps".to_string(), "0".to_string()], &[]).expect("flags");
        assert!(fps_of(&flags).is_err());
    }
}
