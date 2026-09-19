//! Weave vectors into an elementary stream, and read them back.
//!
//! The tool is a thin native shell over `ffrwd-index-core`: it reads
//! files, turns rows of JSON into records, and prints records back as
//! rows of JSON. Every decision about bytes is the library's.
//!
//! Only Annex B elementary streams are handled here. MP4 and Matroska
//! are the next pass; ffmpeg converts either way with `-c copy` and the
//! bitstream filters, which `README.md` spells out.

use std::collections::BTreeMap;
use std::path::Path;

use ffrwd_index_core::assemble::{Assembler, Limits, Record};
use ffrwd_index_core::avc::{self, Codec};
use ffrwd_index_core::index::FileIndex;
use ffrwd_index_core::message::{Message, Space, Unit, VectorBody, VectorRecord};
use ffrwd_index_core::placement::{plan, Carrier, Pending, Placement};
use ffrwd_index_core::quant::{f16_to_f32, Planes};
use ffrwd_index_rows::json::{float, number, object, Json};
use ffrwd_index_rows::space::{read_space, space_row};
use ffrwd_index_rows::vector::{bodies, plane_numbers, read_values};

const USAGE: &str = "\
ffrwd-index: embedding vectors in a video's own elementary stream.

    ffrwd-index weave --video IN --vectors ROWS.ndjson --out OUT
                      [--placement keyframe|next|spread:BYTES]
                      [--escapes N] [--fps N] [--codec h264|h265]

    ffrwd-index read  --video IN [--fps N] [--codec h264|h265]
                      [--index OUT.ffix]

    ffrwd-index read  --index IN.ffix

IN and OUT are H.264 or HEVC Annex B elementary streams; the codec is
taken from the file name unless --codec says otherwise. An elementary
stream carries no timestamps, so --fps (30 by default) is what gives
each access unit a presentation time. --escapes (2 by default, 16 at
most) is how many of a vector's largest components an i8 space sends
exactly rather than quantized.

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

/// The flags of one command. Every flag this tool has takes a value.
struct Flags {
    values: BTreeMap<String, String>,
}

impl Flags {
    /// Reads `--name value` pairs.
    fn parse(args: &[String]) -> Result<Flags, String> {
        let mut values = BTreeMap::new();
        let mut at = 0usize;
        while at < args.len() {
            let name = args[at]
                .strip_prefix("--")
                .ok_or_else(|| format!("{} is not a flag", args[at]))?
                .to_string();
            at += 1;
            let value = args
                .get(at)
                .ok_or_else(|| format!("--{name} wants a value"))?
                .clone();
            at += 1;
            if values.insert(name.clone(), value).is_some() {
                return Err(format!("--{name} was given twice"));
            }
        }
        Ok(Flags { values })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn need(&self, name: &str) -> Result<&str, String> {
        self.get(name).ok_or_else(|| format!("--{name} is needed"))
    }

    /// Refuses a flag this command does not know, rather than ignoring
    /// it and doing something else than was asked.
    fn only(&self, known: &[&str]) -> Result<(), String> {
        for name in self.values.keys() {
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

/// How many of a vector's largest components go exactly.
///
/// Two is the default because that is what the measurement behind
/// section 5 found: one component of a video-text model held 30% of
/// each vector's energy, and sending the two largest exactly brought
/// the agreement with the original ranking from 80% to 99%.
fn escapes_of(flags: &Flags) -> Result<usize, String> {
    let text = flags.get("escapes").unwrap_or("2");
    let value: usize = text
        .parse()
        .map_err(|_| format!("{text} is not a number of escapes"))?;
    if value > usize::from(ffrwd_index_core::MAX_ESCAPES) {
        return Err(format!(
            "{value} escapes is more than the {} this format allows",
            ffrwd_index_core::MAX_ESCAPES
        ));
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
    let flags = Flags::parse(args)?;
    flags.only(&[
        "video",
        "vectors",
        "out",
        "placement",
        "escapes",
        "fps",
        "codec",
    ])?;
    let video = flags.need("video")?;
    let vectors = flags.need("vectors")?;
    let out = flags.need("out")?;
    let codec = codec_of(video, flags.get("codec"))?;
    let fps = fps_of(&flags)?;
    let policy = placement_of(flags.get("placement").unwrap_or("keyframe"))?;
    let escapes = escapes_of(&flags)?;

    let stream = std::fs::read(video).map_err(|err| format!("{video}: {err}"))?;
    let rows = std::fs::read_to_string(vectors).map_err(|err| format!("{vectors}: {err}"))?;
    let (spaces, records) = read_rows(&rows, escapes)?;

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

    let planned = plan(policy, &spaces, &records, &carriers);
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
fn read_rows(text: &str, escapes: usize) -> Result<(Vec<Space>, Vec<Pending>), String> {
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
        let record =
            read_vector(&row, space, escapes, next_id.entry(space_id).or_insert(0)).map_err(at)?;
        records.push(record);
    }
    if spaces.is_empty() {
        return Err("the rows declare no space".into());
    }
    Ok((spaces.into_values().collect(), records))
}

/// A vector row, in its space's encoding.
fn read_vector(
    row: &Json,
    space: &Space,
    escapes: usize,
    next_id: &mut u32,
) -> Result<Pending, String> {
    let values = read_values(row.get("vector"), space.dims)?;
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

    // The tool writes one message per plane whatever the policy: it has
    // the whole file in hand, so a `spread` budget can still dole the
    // planes out, and the other policies put them on one carrier anyway.
    Ok(Pending {
        space_id: space.space_id,
        record_id,
        start_ms,
        end_ms,
        available_ms,
        bodies: bodies(space, &values, escapes, None, true)?,
    })
}

// ---------------------------------------------------------------- //
// read.
// ---------------------------------------------------------------- //

fn read(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(args)?;
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
    let mut unreadable = 0usize;
    for record in &records {
        // A record whose plane 0 never arrived, because the stream was
        // cut before it, is held but cannot be turned into a vector.
        // It is counted rather than printed, and it is not a reason to
        // give up on the rest of the file.
        match record_row(record) {
            Ok(row) => {
                out.push_str(&row.write());
                out.push('\n');
            }
            Err(_) => unreadable += 1,
        }
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
            if record.values().is_err() {
                continue;
            }
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
    if unreadable > 0 {
        eprintln!("{video}: {unreadable} records never got their plane 0");
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
                            if !planes.escapes().is_empty() {
                                members.push(("escapes", escapes_row(planes)));
                            }
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
    if let VectorBody::I8(planes) = &record.body {
        if !planes.escapes().is_empty() {
            members.push(("escapes", escapes_row(planes)));
        }
    }
    members.push(("vector", vector_row(&values)));
    Ok(object(members))
}

/// The escaped components, as index and value pairs. The vector
/// already holds their values; this says which of them came exactly.
fn escapes_row(planes: &Planes) -> Json {
    Json::Array(
        planes
            .escapes()
            .iter()
            .map(|(index, value)| Json::Array(vec![number(*index), float(f16_to_f32(*value))]))
            .collect(),
    )
}

/// Which planes arrived, as their numbers.
fn planes_row(present: u8) -> Json {
    Json::Array(plane_numbers(present).into_iter().map(number).collect())
}

fn vector_row(values: &[f32]) -> Json {
    Json::Array(values.iter().copied().map(float).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (spaces, records) = read_rows(text, 0).expect("rows");
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
            let err = read_rows(text, 0).expect_err("a refusal");
            assert!(err.contains(wanted), "{text} gave {err}");
        }
    }

    #[test]
    fn the_flags_of_a_command_are_checked() {
        let args: Vec<String> = ["--video", "a.h264", "--placement", "next"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let flags = Flags::parse(&args).expect("flags");
        assert_eq!(flags.get("video"), Some("a.h264"));
        assert_eq!(flags.get("placement"), Some("next"));
        assert!(flags.only(&["video", "placement"]).is_ok());
        assert!(flags.only(&["video"]).is_err());
        assert!(flags.need("out").is_err());

        let dangling: Vec<String> = vec!["--video".into()];
        assert!(Flags::parse(&dangling).is_err());
        let bare: Vec<String> = vec!["video".into()];
        assert!(Flags::parse(&bare).is_err());
        let twice: Vec<String> = ["--fps", "30", "--fps", "60"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        assert!(Flags::parse(&twice).is_err());
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
        let flags = Flags::parse(&["--fps".to_string(), "0".to_string()]).expect("flags");
        assert!(fps_of(&flags).is_err());
    }
}
