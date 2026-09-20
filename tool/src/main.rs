//! Weave vectors into a video stream, and read them back out of one.
//!
//! The tool is a thin native shell over `ffrwd-index-core`,
//! `ffrwd-index-rows` and `ffrwd-index-container`: it reads files,
//! turns rows of JSON into records, and prints records back as rows of
//! JSON. Every decision about bytes is a library's.
//!
//! Two ways in. `--video` is an Annex B elementary stream or a raw AV1
//! OBU stream, which carries no timestamps, so `--fps` is what gives
//! each access unit a presentation time. `--mp4` and `--mkv` are the
//! containers, which carry their own, and which this reads natively:
//! the sample tables say where every sample is and when it is shown,
//! and only the front of each sample is read, because that is where
//! section 7 puts a unit.
//!
//! Three of the commands live in their own files, because each of them
//! is a decision rather than a translation: [`search`] ranks, [`watch`]
//! keeps up with a stream that is still being written, and [`describe`]
//! turns one package's output into rows.

mod describe;
mod search;
mod watch;

use std::collections::BTreeMap;

use std::path::Path;

use ffrwd_bmff::patch::{self, Placed};
use ffrwd_bmff::source::{Source, Tally};
use ffrwd_bmff::track::{self, Pick};
use ffrwd_index_container::scan::{carriages, Scan};
use ffrwd_index_container::{mkv, Kind, Video, INDEX_BOX};
use ffrwd_index_core::assemble::{Assembler, Limits, Record};
use ffrwd_index_core::carriage;
use ffrwd_index_core::index::{FileIndex, MATROSKA_FILE_NAME, MATROSKA_MIME};
use ffrwd_index_core::message::{Message, Space, Unit, VectorBody, VectorRecord};
use ffrwd_index_core::placement::{plan, Carrier, Pending, Placement};
use ffrwd_index_core::quant::{f16_to_f32, Planes};
use ffrwd_index_core::METADATA_TYPE;
use ffrwd_index_rows::json::{float, number, object, Json};
use ffrwd_index_rows::space::{read_space, space_row};
use ffrwd_index_rows::vector::{bodies, plane_numbers, read_values};
use ffrwd_nal::feed::StreamKind;
use ffrwd_nal::{h26x, obu, Codec};

const USAGE: &str = "\
ffrwd-index: embedding vectors in a video's own stream.

    ffrwd-index weave --video IN --vectors ROWS.ndjson --out OUT
                      [--placement keyframe|next|spread:BYTES]
                      [--escapes N] [--fps N] [--codec h264|h265|av1]

    ffrwd-index read  --video IN|- [--fps N] [--codec h264|h265|av1]
                      [--index OUT.ffix]

    ffrwd-index read  --mp4 IN.mp4 [--scan keyframes|all] [--index OUT.ffix]
    ffrwd-index read  --mkv IN.mkv [--scan keyframes|all] [--index OUT.ffix]

    ffrwd-index read  --index IN.ffix | IN.mp4 | IN.mkv

    ffrwd-index index FILE [--scan all|keyframes] [--rewrite] [--out OUT]

    ffrwd-index search --mp4 IN.mp4 | --mkv IN.mkv | --video IN | --index IN
                       | --rows ROWS.ndjson
                       --query Q.json|- [--space ID|URI] [--top K]
                       [--threshold T] [--coarse N] [--scan keyframes|all]

    ffrwd-index watch --video - [--codec h264|h265|av1] [--fps N]
                      --query NAME=FILE [--query NAME=FILE ...]
                      --threshold T [--min-planes N]

    ffrwd-index rows-from-describe [--clip C.ndjson] [--speech S.ndjson]
                       [--sound D.ndjson] [--package ffrwd.json] [--out ROWS]

--video is an H.264 or HEVC Annex B elementary stream or a raw AV1 OBU
stream; the codec is taken from the file name unless --codec says
otherwise, and - is standard input. Such a stream carries no
timestamps, so --fps (30 by default) is what gives each access unit a
presentation time.

--mp4 and --mkv are read natively, with their own timestamps. --scan
keyframes (the default for read) looks at sync samples alone, which is
where the keyframe placement puts every record; --scan all looks at
every sample, which next and spread need. How much of the file that
cost is printed on stderr.

index builds the file index of SPEC.md section 8 and puts it in the
file: a uuid box appended to an MP4, or, for Matroska, an attachment
written by ffmpeg into --out. It scans every sample by default so that
no record is missed.

search ranks one space's records against a query vector by cosine and
prints the spans, best first. It reads a file's own index where there
is one and --scan does not ask otherwise. It does not embed text: the
vector comes from the model the space names, which search prints.
--rows ranks the numbers in a weave's own rows instead, unencoded,
which is what the same search would have found had nothing been
quantized.

watch reads a growing stream and prints a row the moment a record
passes --threshold, before the frames after it have arrived.

--escapes (2 by default, 16 at most) is how many of a vector's largest
components an i8 space sends exactly rather than quantized.

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
        Some("index") => index(&args[1..]),
        Some("search") => search::search(&args[1..]),
        Some("watch") => watch::watch(&args[1..]),
        Some("rows-from-describe") => describe::rows_from_describe(&args[1..]),
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
    /// Every value a repeatable flag was given, in order. A flag that
    /// may not repeat has at most one here and is refused a second.
    repeats: BTreeMap<String, Vec<String>>,
}

impl Flags {
    /// Reads `--name value` pairs, none of them twice.
    fn parse(args: &[String]) -> Result<Flags, String> {
        Flags::parse_repeating(args, &[])
    }

    /// The same, with the flags in `repeatable` allowed more than once.
    ///
    /// `watch` takes several queries at a time, and a flag given twice
    /// is a mistake everywhere else, so the exception is named by the
    /// command that has it rather than made the rule.
    fn parse_repeating(args: &[String], repeatable: &[&str]) -> Result<Flags, String> {
        let mut values = BTreeMap::new();
        let mut repeats: BTreeMap<String, Vec<String>> = BTreeMap::new();
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
            let seen = values.insert(name.clone(), value.clone()).is_some();
            if seen && !repeatable.contains(&name.as_str()) {
                return Err(format!("--{name} was given twice"));
            }
            repeats.entry(name).or_default().push(value);
        }
        Ok(Flags { values, repeats })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Every value a flag was given, in the order they were given.
    fn every(&self, name: &str) -> Vec<&str> {
        self.repeats
            .get(name)
            .map(|values| values.iter().map(String::as_str).collect())
            .unwrap_or_default()
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

/// Which elementary stream a `--video` file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Nal(Codec),
    Av1,
}

/// The codec of a file, by its name or by what the caller said.
fn codec_of(path: &str, named: Option<&str>) -> Result<Stream, String> {
    if let Some(named) = named {
        return match named {
            "h264" | "avc" => Ok(Stream::Nal(Codec::H264)),
            "h265" | "hevc" => Ok(Stream::Nal(Codec::H265)),
            "av1" | "obu" => Ok(Stream::Av1),
            other => Err(format!("{other} is not h264, h265 or av1")),
        };
    }
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "h264" | "264" | "avc" => Ok(Stream::Nal(Codec::H264)),
        "h265" | "265" | "hevc" => Ok(Stream::Nal(Codec::H265)),
        "obu" | "av1" => Ok(Stream::Av1),
        _ => Err(format!(
            "{path} does not name its codec. Pass --codec h264, --codec h265 or --codec av1"
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

/// Which samples a container read visits.
fn scan_of(flags: &Flags, fallback: Scan) -> Result<Scan, String> {
    match flags.get("scan") {
        None => Ok(fallback),
        Some(text) => Scan::parse(text).ok_or_else(|| format!("{text} is not keyframes or all")),
    }
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
// Elementary streams.
// ---------------------------------------------------------------- //

/// One access unit or temporal unit: where a unit of this format goes
/// in it, what it holds already, and whether it is a random access
/// point.
struct Spot {
    start: usize,
    end: usize,
    insert_at: usize,
    keyframe: bool,
    temporal_id_plus1: u8,
}

/// The carriers of an elementary stream.
fn spots(stream: &[u8], kind: Stream) -> Result<Vec<Spot>, String> {
    Ok(match kind {
        Stream::Nal(codec) => h26x::access_units(stream, codec)
            .into_iter()
            .map(|unit| Spot {
                start: unit.start,
                end: unit.end,
                insert_at: unit.insert_at,
                keyframe: unit.keyframe,
                temporal_id_plus1: unit.temporal_id_plus1,
            })
            .collect(),
        Stream::Av1 => obu::temporal_units(stream)
            .map_err(|err| err.to_string())?
            .into_iter()
            .map(|unit| Spot {
                start: unit.start,
                end: unit.end,
                insert_at: unit.insert_at,
                // An AV1 writer repeats the sequence header before
                // every key frame, which is as close to a sync sample
                // as a reader gets without decoding a frame header.
                keyframe: unit.has_sequence_header,
                temporal_id_plus1: 1,
            })
            .collect(),
    })
}

/// A unit framed for its codec, ready to splice in at a spot.
fn framed(kind: Stream, unit: &[u8], temporal_id_plus1: u8) -> Vec<u8> {
    match kind {
        Stream::Nal(codec) => {
            let mut out = vec![0, 0, 0, 1];
            out.extend_from_slice(&carriage::wrap_unit_at(unit, codec, temporal_id_plus1));
            out
        }
        Stream::Av1 => obu::write_metadata(METADATA_TYPE, unit),
    }
}

/// Every unit of this format in one carrier's bytes.
fn units_in(bytes: &[u8], kind: Stream) -> Vec<Vec<u8>> {
    match kind {
        Stream::Nal(codec) => carriage::units_annexb(bytes, codec),
        Stream::Av1 => carriage::units_obu(bytes),
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
    let kind = codec_of(video, flags.get("codec"))?;
    let fps = fps_of(&flags)?;
    let policy = placement_of(flags.get("placement").unwrap_or("keyframe"))?;
    let escapes = escapes_of(&flags)?;

    let stream = std::fs::read(video).map_err(|err| format!("{video}: {err}"))?;
    let rows = std::fs::read_to_string(vectors).map_err(|err| format!("{vectors}: {err}"))?;
    let (spaces, records) = read_rows(&rows, escapes)?;

    let units = spots(&stream, kind)?;
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
    let mut declaring = 0usize;
    for (unit, messages) in units.iter().zip(&planned) {
        if messages.is_empty() {
            continue;
        }
        if messages.iter().all(|m| matches!(m, Message::Space(_))) {
            declaring += 1;
        }
        let bytes = Unit::new(messages.clone()).encode();
        woven.extend_from_slice(&stream[at..unit.insert_at]);
        woven.extend_from_slice(&framed(kind, &bytes, unit.temporal_id_plus1));
        at = unit.insert_at;
        written += 1;
    }
    woven.extend_from_slice(&stream[at..]);
    std::fs::write(out, &woven).map_err(|err| format!("{out}: {err}"))?;

    // Section 3 puts the spaces on every keyframe whether a record
    // rides there or not, so the access units written to are not all
    // access units carrying a record, and saying so is the difference
    // between a count somebody can check and one they cannot.
    eprintln!(
        "{out}: {units} access units, {carrying} carrying {records} records, {declaring} declaring {spaces} spaces alone, {added} bytes added",
        units = units.len(),
        carrying = written - declaring,
        records = records.len(),
        spaces = spaces.len(),
        added = woven.len() - stream.len()
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
    flags.only(&["video", "mp4", "mkv", "fps", "codec", "scan", "index"])?;
    let named = ["video", "mp4", "mkv"]
        .iter()
        .filter(|name| flags.get(name).is_some())
        .count();
    if named > 1 {
        return Err("read takes one of --video, --mp4 and --mkv".into());
    }
    let sidecar = flags.get("index");
    match (flags.get("video"), flags.get("mp4"), flags.get("mkv")) {
        (Some(video), _, _) => read_stream(&flags, video, sidecar),
        (_, Some(file), _) => read_container(&flags, file, Some(Kind::Mp4), sidecar),
        (_, _, Some(file)) => read_container(&flags, file, Some(Kind::Matroska), sidecar),
        (None, None, None) => match sidecar {
            Some(path) => dump_index(path),
            None => Err("read wants --video, --mp4, --mkv or --index".into()),
        },
    }
}

/// One carrier of whatever kind: when it was shown, and the units of
/// this format that rode on it.
struct Carried {
    time_ms: i64,
    units: Vec<Vec<u8>>,
}

fn read_stream(flags: &Flags, video: &str, index: Option<&str>) -> Result<(), String> {
    let carried = stream_carriers(flags, video)?;
    report(&carried, video, index)
}

/// The carriers of an elementary stream, from a file or from standard
/// input.
///
/// A pipe is cut into carriers as its bytes arrive, which is what
/// `watch` is built on, but `read` still prints nothing until the
/// stream ends: its output is the spaces first and then the records,
/// and neither is known until the last carrier has gone by. `watch` is
/// the command that says something while a stream runs.
fn stream_carriers(flags: &Flags, video: &str) -> Result<Vec<Carried>, String> {
    let kind = codec_of(video, flags.get("codec"))?;
    let fps = fps_of(flags)?;
    if video == "-" {
        return piped_carriers(kind, fps);
    }
    let stream = std::fs::read(video).map_err(|err| format!("{video}: {err}"))?;
    Ok(spots(&stream, kind)?
        .iter()
        .enumerate()
        .map(|(index, spot)| Carried {
            time_ms: pts_ms(index, fps),
            units: units_in(&stream[spot.start..spot.end], kind),
        })
        .collect())
}

/// The carriers of a stream on standard input, one chunk at a time, so
/// the bytes of a picture are dropped as soon as the units in front of
/// it have been taken off.
fn piped_carriers(kind: Stream, fps: f64) -> Result<Vec<Carried>, String> {
    use std::io::Read;
    let mut feed = carriage::feed(match kind {
        Stream::Nal(codec) => StreamKind::Nal(codec),
        Stream::Av1 => StreamKind::Av1,
    });
    let mut stdin = std::io::stdin();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut out = Vec::new();
    loop {
        let got = stdin
            .read(&mut buffer)
            .map_err(|err| format!("standard input: {err}"))?;
        let carried = if got == 0 {
            feed.finish().map_err(|err| err.to_string())?
        } else {
            feed.push(&buffer[..got]).map_err(|err| err.to_string())?
        };
        for carrier in carried {
            out.push(Carried {
                time_ms: pts_ms(carrier.index as usize, fps),
                units: carrier.payloads,
            });
        }
        if got == 0 {
            break;
        }
    }
    Ok(out)
}

fn read_container(
    flags: &Flags,
    path: &str,
    kind: Option<Kind>,
    index: Option<&str>,
) -> Result<(), String> {
    let scan = scan_of(flags, Scan::Keyframes)?;
    let file = std::fs::File::open(path).map_err(|err| format!("{path}: {err}"))?;
    let mut src = Source::new(file).map_err(|err| format!("{path}: {err}"))?;
    let (track, carried) = walk(&mut src, kind, scan).map_err(|err| format!("{path}: {err}"))?;
    let tally = src.tally();
    report(&carried, path, index)?;
    eprintln!("{path}: {}", accounting(&track, &carried, scan, tally));
    Ok(())
}

/// The track and its carriers, whichever container the file is.
fn walk<R: std::io::Read + std::io::Seek>(
    src: &mut Source<R>,
    kind: Option<Kind>,
    scan: Scan,
) -> Result<(Video, Vec<Carried>), ffrwd_index_container::Error> {
    // What the file says it is decides, even when a flag named a
    // container: a `--mp4` pointed at an elementary stream should hear
    // that rather than a complaint about a box.
    let found = ffrwd_index_container::kind_of(src)?;
    if kind.is_some_and(|wanted| wanted != found) {
        return Err(ffrwd_index_container::Error::Unsupported(format!(
            "the file is {}, not {}",
            name_of(found),
            name_of(kind.unwrap_or(found))
        )));
    }
    let kind = found;
    let track = match kind {
        Kind::Mp4 => Video::of_track(&track::read(src, Pick::Video)?)?,
        Kind::Matroska => mkv::read(src, scan)?,
    };
    let carried = carriages(src, &track, scan)?
        .into_iter()
        .map(|carriage| Carried {
            time_ms: carriage.pts_ms,
            units: carriage.units,
        })
        .collect();
    Ok((track, carried))
}

fn name_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Mp4 => "an ISO base media file",
        Kind::Matroska => "a Matroska file",
    }
}

/// The line a container read prints on stderr.
///
/// The ratio is the point: section 7's `keyframe` policy exists so that
/// reading a file's records is not reading the file.
fn accounting(track: &Video, carried: &[Carried], scan: Scan, tally: Tally) -> String {
    format!(
        "scan {}: {} of {} samples, {} of {} bytes read ({:.2}% of the file), {} seeks",
        scan.name(),
        carried.len(),
        track.samples.len(),
        tally.bytes_read,
        tally.len,
        tally.share(),
        tally.seeks,
    )
}

/// The spaces and records of a set of carriers, printed, and the index
/// written beside them if one was asked for.
fn report(carried: &[Carried], name: &str, index: Option<&str>) -> Result<(), String> {
    let mut assembler = Assembler::new(Limits {
        // A file is read whole, so the ceilings are the ones a file
        // reader wants rather than a live reader's.
        max_records: 1 << 20,
        max_reassemblies: 4096,
        orphan_wait_ms: i64::MAX,
    });
    let mut spaces: Vec<(i64, Space)> = Vec::new();
    for carrier in carried {
        for bytes in &carrier.units {
            let unit = match Unit::decode(bytes) {
                Ok(unit) => unit,
                // A unit of a version this build does not know is not
                // an error: it is somebody ahead of us.
                Err(_) => continue,
            };
            for message in &unit.messages {
                if let Message::Space(space) = message {
                    spaces.push((carrier.time_ms, space.clone()));
                }
            }
            assembler.push_unit(carrier.time_ms, &unit);
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
        let built = build_index(&spaces, &records)?;
        std::fs::write(path, built.encode()).map_err(|err| format!("{path}: {err}"))?;
        eprintln!("{path}: {} entries", built.entries.len());
    }
    if assembler.dropped() > 0 {
        eprintln!(
            "{name}: {} messages were dropped as unreadable",
            assembler.dropped()
        );
    }
    if unreadable > 0 {
        eprintln!("{name}: {unreadable} records never got their plane 0");
    }
    Ok(())
}

/// Section 8's index, from what a read found.
///
/// It holds SPACE and VECTOR messages only, so what goes in is the
/// records as the assembler put them back together, each against the
/// carrier its first message rode.
fn build_index(spaces: &[(i64, Space)], records: &[Record]) -> Result<FileIndex, String> {
    let mut pairs: Vec<(i32, Message)> = Vec::new();
    for (time, space) in spaces {
        pairs.push((index_time(*time)?, Message::Space(space.clone())));
    }
    for record in records {
        if record.values().is_err() {
            continue;
        }
        pairs.push((
            index_time(record.carrier_ms)?,
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
    Ok(FileIndex::build(pairs))
}

/// `read --index`, which takes a bare index or a file carrying one.
///
/// An index of its own opens with `FFIX`; anything else is a container,
/// and the first bytes say which.
fn dump_index(path: &str) -> Result<(), String> {
    let head = read_head(path)?;
    let bytes = if head.starts_with(&ffrwd_index_core::index::MAGIC) {
        std::fs::read(path).map_err(|err| format!("{path}: {err}"))?
    } else {
        let file = std::fs::File::open(path).map_err(|err| format!("{path}: {err}"))?;
        let mut src = Source::new(file).map_err(|err| format!("{path}: {err}"))?;
        let kind =
            ffrwd_index_container::kind_of(&mut src).map_err(|err| format!("{path}: {err}"))?;
        let found = match kind {
            Kind::Mp4 => patch::read(&mut src, INDEX_BOX).map_err(Into::into),
            Kind::Matroska => mkv::read_index(&mut src),
        }
        .map_err(|err| format!("{path}: {err}"))?;
        let tally = src.tally();
        // Section 8: the index holds absolute times where the stream
        // holds offsets, so a cut or a join leaves the stream right and
        // the index wrong. Nothing cheap tells the two apart, because
        // nothing in an index says which pictures it was built from;
        // checking would mean reading the sample table to compare time
        // ranges, which costs more than the index read it would guard
        // and still misses a cut that kept the file's length. So the
        // index is taken at its word, and the word is said out loud.
        eprintln!(
            "{path}: the index came out of {} bytes read in {} seeks, \
             and is taken at its word: a cut or a join since it was built \
             would leave it wrong, and `ffrwd-index index` rebuilds it",
            tally.bytes_read, tally.seeks
        );
        found.ok_or_else(|| format!("{path} carries no index of this format"))?
    };
    print_index(
        path,
        &FileIndex::parse(&bytes).map_err(|err| format!("{path}: {err}"))?,
    )
}

fn read_head(path: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|err| format!("{path}: {err}"))?;
    let mut head = vec![0u8; 8];
    let got = file
        .read(&mut head)
        .map_err(|err| format!("{path}: {err}"))?;
    head.truncate(got);
    Ok(head)
}

fn print_index(path: &str, index: &FileIndex) -> Result<(), String> {
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

// ---------------------------------------------------------------- //
// index.
// ---------------------------------------------------------------- //

/// `index FILE`: scan the file, build section 8's index, and put it in.
///
/// The default scan is every sample, not the sync samples alone: an
/// index built from half a file would be an index that quietly lies,
/// and the one command whose job is to be complete should be.
fn index(args: &[String]) -> Result<(), String> {
    let (file, rest) = match args.first() {
        Some(first) if !first.starts_with("--") => (first.as_str(), &args[1..]),
        _ => return Err("index wants a file to put an index into".into()),
    };
    // `--rewrite` is the one flag in this tool that takes no value, so
    // it is taken out before the pairs are read rather than making
    // every other flag's parsing answer for it.
    let rewrite = rest.iter().any(|arg| arg == "--rewrite");
    let rest: Vec<String> = rest
        .iter()
        .filter(|arg| *arg != "--rewrite")
        .cloned()
        .collect();
    let flags = Flags::parse(&rest)?;
    flags.only(&["scan", "out"])?;
    let scan = scan_of(&flags, Scan::All)?;

    let handle = std::fs::File::open(file).map_err(|err| format!("{file}: {err}"))?;
    let mut src = Source::new(handle).map_err(|err| format!("{file}: {err}"))?;
    let kind = ffrwd_index_container::kind_of(&mut src).map_err(|err| format!("{file}: {err}"))?;
    let (track, carried) =
        walk(&mut src, Some(kind), scan).map_err(|err| format!("{file}: {err}"))?;
    let tally = src.tally();
    drop(src);

    let (spaces, records) = collect(&carried);
    let built = build_index(&spaces, &records)?;
    let bytes = built.encode();
    eprintln!(
        "{file}: {}, {} entries in {} bytes of index",
        accounting(&track, &carried, scan, tally),
        built.entries.len(),
        bytes.len()
    );
    if built.entries.is_empty() {
        return Err(format!("{file} carries no records to index"));
    }

    match kind {
        Kind::Mp4 => {
            if flags.get("out").is_some() {
                return Err("--out belongs to Matroska; an MP4 takes its index in place".into());
            }
            install_mp4(file, &bytes, rewrite)
        }
        Kind::Matroska => {
            let out = flags.need("out").map_err(|_| {
                "a Matroska index is written by ffmpeg into a new file, so --out is needed"
                    .to_string()
            })?;
            if rewrite {
                return Err(
                    "--rewrite belongs to MP4; a Matroska index is always a new file".into(),
                );
            }
            attach_mkv(file, out, &bytes)
        }
    }
}

fn install_mp4(file: &str, bytes: &[u8], rewrite: bool) -> Result<(), String> {
    if rewrite {
        // A copy without the box that is there, then the new one on the
        // end of the copy, then the copy in the original's place.
        let handle = std::fs::File::open(file).map_err(|err| format!("{file}: {err}"))?;
        let mut src = Source::new(handle).map_err(|err| format!("{file}: {err}"))?;
        let temporary = format!("{file}.ffrwd-index-rewrite");
        let mut out =
            std::fs::File::create(&temporary).map_err(|err| format!("{temporary}: {err}"))?;
        let done = patch::rewrite(&mut src, &mut out, INDEX_BOX, bytes);
        drop(out);
        drop(src);
        if let Err(err) = done {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!("{file}: {err}"));
        }
        std::fs::rename(&temporary, file).map_err(|err| format!("{file}: {err}"))?;
        eprintln!("{file}: the file was copied without its old index box and a new one appended");
        return Ok(());
    }
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(file)
        .map_err(|err| format!("{file}: {err}"))?;
    let placed = patch::install(handle, INDEX_BOX, bytes).map_err(|err| refusal(file, err))?;
    eprintln!(
        "{file}: {}",
        match placed {
            Placed::Appended { at } => format!("the index box was appended at byte {at}"),
            Placed::Replaced { at } =>
                format!("the index box already there was written over, at byte {at}"),
            Placed::BeforeMfra { at, moved } => format!(
                "the index box went in at byte {at}, in front of the {moved}-byte mfra, \
                 which was written again after it so that it stays last"
            ),
        }
    );
    Ok(())
}

/// What `patch::install` would not do, and what to do about it.
///
/// The shared crate refuses a box that is not last and says why, with
/// the byte it found the old one at. It does not know this tool's flags
/// and says nothing about them, so the sentence naming `--rewrite` is
/// added here, where the flag lives. The offset goes in front of the
/// crate's own sentence, which ends on the copy it will not make.
fn refusal(file: &str, err: ffrwd_bmff::Error) -> String {
    match &err {
        ffrwd_bmff::Error::Unsupported(fault) => {
            let at = match fault.at {
                Some(at) => format!("at byte {at}, "),
                None => String::new(),
            };
            format!(
                "{file}: {at}{}. Pass --rewrite to copy the file without the box it \
                 already carries",
                fault.what
            )
        }
        _ => format!("{file}: {err}"),
    }
}

/// Matroska's attachment, through ffmpeg.
///
/// Writing one natively means rewriting the segment: the `SeekHead` at
/// the front names its children by position, every enclosing length
/// changes, and a length written before the attachment existed has to
/// be written again. That is a muxer, and ffmpeg is already one.
fn attach_mkv(file: &str, out: &str, bytes: &[u8]) -> Result<(), String> {
    let temporary = std::env::temp_dir().join(format!("ffrwd-index-{}.bin", std::process::id()));
    std::fs::write(&temporary, bytes).map_err(|err| format!("{}: {err}", temporary.display()))?;
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-i"])
        .arg(file)
        .args(["-map", "0", "-c", "copy", "-attach"])
        .arg(&temporary)
        .args([
            "-metadata:s:t",
            &format!("mimetype={MATROSKA_MIME}"),
            "-metadata:s:t",
            &format!("filename={MATROSKA_FILE_NAME}"),
        ])
        .arg(out)
        .status();
    let _ = std::fs::remove_file(&temporary);
    match status {
        Ok(status) if status.success() => {
            eprintln!("{out}: the index was attached by ffmpeg as {MATROSKA_FILE_NAME}");
            Ok(())
        }
        Ok(status) => Err(format!("ffmpeg refused the attachment: {status}")),
        Err(err) => Err(format!(
            "a Matroska index needs ffmpeg on the PATH to write the attachment: {err}"
        )),
    }
}

/// The spaces and records a set of carriers holds, for the index.
fn collect(carried: &[Carried]) -> (Vec<(i64, Space)>, Vec<Record>) {
    let mut assembler = Assembler::new(Limits {
        max_records: 1 << 20,
        max_reassemblies: 4096,
        orphan_wait_ms: i64::MAX,
    });
    let mut spaces: Vec<(i64, Space)> = Vec::new();
    for carrier in carried {
        for bytes in &carrier.units {
            let Ok(unit) = Unit::decode(bytes) else {
                continue;
            };
            for message in &unit.messages {
                if let Message::Space(space) = message {
                    spaces.push((carrier.time_ms, space.clone()));
                }
            }
            assembler.push_unit(carrier.time_ms, &unit);
        }
    }
    let records = assembler.records();
    (spaces, records)
}

fn offset(value: i64) -> Result<i32, String> {
    i32::try_from(value).map_err(|_| "a span too far from its carrier to write".to_string())
}

/// A carrier's time as section 8's signed milliseconds.
///
/// Signed because an MP4's edit list can put a carrier before the time
/// the file starts at, and ffprobe reports a negative time for it; a
/// clamp to zero would move the entry to a picture it did not come off.
/// The field is a zigzag varint of at most five bytes, so it holds
/// about twenty-four days either side of zero.
fn index_time(value: i64) -> Result<i32, String> {
    i32::try_from(value).map_err(|_| "a carrier too far from zero to index".to_string())
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
        assert_eq!(
            codec_of("a.h264", None).expect("a codec"),
            Stream::Nal(Codec::H264)
        );
        assert_eq!(
            codec_of("a.265", None).expect("a codec"),
            Stream::Nal(Codec::H265)
        );
        assert_eq!(codec_of("a.obu", None).expect("a codec"), Stream::Av1);
        assert_eq!(
            codec_of("a.bin", Some("hevc")).expect("a codec"),
            Stream::Nal(Codec::H265)
        );
        assert_eq!(
            codec_of("a.bin", Some("av1")).expect("a codec"),
            Stream::Av1
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
    fn the_scan_flag_reads_its_two_shapes() {
        let flags = Flags::parse(&["--scan".to_string(), "all".to_string()]).expect("flags");
        assert_eq!(scan_of(&flags, Scan::Keyframes).expect("a scan"), Scan::All);
        let none = Flags::parse(&[]).expect("flags");
        assert_eq!(scan_of(&none, Scan::All).expect("a scan"), Scan::All);
        let wrong = Flags::parse(&["--scan".to_string(), "some".to_string()]).expect("flags");
        assert!(scan_of(&wrong, Scan::All).is_err());
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
