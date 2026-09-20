//! `records`: an ffrwd packet sink that reads the vectors back out of a
//! video stream.
//!
//! One row per complete record: which space it is in, the span it
//! describes in seconds of the stream's own presentation clock, and the
//! vector itself. That is the whole of what a search needs, and it is
//! the mirror of `weave/`: the same messages, the same layered
//! encoding, taken apart instead of put together.
//!
//! **The clock.** Section 4 carries offsets from the carrier and
//! nothing else, because absolute times do not survive a remux. A
//! packet sink is told the carrier's `pts` in the stream's own time
//! base, so the spans here are absolute and exact. A reader of a bare
//! elementary stream has no such thing and falls back on a frame rate,
//! which is wrong for variable frame rate and off by the reorder delay
//! where there are B-frames.
//!
//! **Keyframes or everything.** Section 7's `keyframe` policy puts
//! every record of a file on a sync sample, the ones whose span ends
//! after the last keyframe included, so a copy that kept the keyframes
//! alone has all of them. A stream written `next` or `spread` puts them
//! on any frame, and nothing in the format says which policy wrote a
//! stream: fed every packet this sink is right either way, and fed
//! keyframes alone it answers what rode on them. It asks for
//! `keyframes`, which is a request and not a promise: a host may hand
//! over more, and one that hands over everything is what finds the
//! records of a live-written stream.
//!
//! The crate is a shim. The reading is `ffrwd_index_rows::read`, the
//! bytes are `ffrwd_index_core`, and both are tested on the native
//! target where a failure says something.

wit_bindgen::generate!({
    path: "wit",
    world: "packet-sink-module",
});

use std::cell::RefCell;

use exports::ffrwd::av::packet_sink::{
    Arity, Guest, InputStream, Meta, PacketSinkMeta, PadPackets, Processed, Wants,
};

use ffrwd_index_rows::read::Reader;
use ffrwd_nal::config::{framing_of, Framing, CODECS};

const PARAMS_SCHEMA: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;

/// One record. `vector`'s length is not fixed here and cannot be: how
/// many components a space has is in the stream's own SPACE message,
/// which this module has not read when `describe` is called. A caller
/// that needs the dimension asks `spaces`.
const ROWS_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["index", "space", "record_id", "start_t", "end_t", "vector"],
  "additionalProperties": false,
  "description": "One row per complete record in the stream. Spans are seconds of the stream's own presentation clock, from the carrier packet's pts and the input's time base, per SPEC section 4.",
  "properties": {
    "index": {"type": "integer", "minimum": 1, "description": "Where this row falls among the rows, from one, in the order the records were read."},
    "space": {"type": "integer", "minimum": 0, "maximum": 255, "description": "The space id, as the `spaces` sink reports it."},
    "record_id": {"type": "integer", "minimum": 0, "maximum": 65535, "description": "The writer's own counter for this space, which wraps."},
    "start_t": {"type": "number"},
    "end_t": {"type": "number"},
    "planes": {"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": 7}, "description": "Which of the eight bit-planes arrived, for an i8 space; absent for the float encodings, which arrive whole or not at all. A record read from fewer than eight is a coarser reading of the same vector, not a wrong one."},
    "vector": {"type": "array", "items": {"type": "number"}, "description": "The vector as the planes that arrived rebuild it, not normalized: unit_length on the space says what the originals were."}
  }
}"#;

struct State {
    reader: Reader,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct RecordsSink;

impl Guest for RecordsSink {
    fn describe() -> PacketSinkMeta {
        PacketSinkMeta {
            meta: Meta {
                name: "records".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                params_schema: PARAMS_SCHEMA.to_string(),
                rows_schema: ROWS_SCHEMA.to_string(),
                // No decoded payload reaches a packet sink, so the
                // frame formats stay empty and the codecs below are
                // what this module accepts instead.
                pixel_formats: vec![],
                sample_formats: vec![],
                sample_rates: vec![],
                channel_counts: vec![],
                rows_language: vec![],
            },
            video_codecs: CODECS.iter().map(|name| name.to_string()).collect(),
            audio_codecs: vec![],
            video: Arity::One,
            audio: Arity::Zero,
            // Section 7's `keyframe` policy puts every record of a
            // file on a sync sample, so the keyframes are all this
            // needs to answer a file whole. A stream written `next` or
            // `spread` puts records elsewhere and a host that hands
            // over more is what finds them; a host may not hand over
            // less than this asks for.
            wants: Wants::Keyframes,
        }
    }

    fn init(streams: Vec<InputStream>, params: String) -> Result<(), String> {
        let framing = open(&streams, &params, "records")?;
        let coded = &streams[0].coded;
        if coded.time_base.den <= 0 || coded.time_base.num <= 0 {
            return Err(format!(
                "a time base of {}/{} is not a fraction of a second",
                coded.time_base.num, coded.time_base.den
            ));
        }
        let (num, den) = (
            i64::from(coded.time_base.num),
            i64::from(coded.time_base.den),
        );
        STATE.with(|state| {
            *state.borrow_mut() = Some(State {
                reader: Reader::new(framing, num, den),
            });
        });
        Ok(())
    }

    fn set_params(params: String) -> Result<(), String> {
        no_params(&params, "records")
    }

    fn process(pads: Vec<PadPackets>, last: bool) -> Processed {
        STATE.with(|cell| {
            let mut borrowed = cell.borrow_mut();
            let Some(state) = borrowed.as_mut() else {
                return Processed {
                    rows: vec![],
                    trailing: vec![],
                };
            };
            let mut rows = Vec::new();
            for pad in &pads {
                for packet in &pad.packets {
                    state.reader.packet(packet.pts, &packet.data);
                }
            }
            // A record whose every plane has arrived can gain nothing
            // more, so it goes out now rather than at the end: a
            // `keyframe` file answers each record on the packet that
            // carried it.
            for record in state.reader.finished() {
                rows.push(state.reader.record_row(&record));
            }
            // What is left is what a writer never finished: a record
            // sent with a cap on its planes, which section 5 allows,
            // or one whose later planes the stream ended before. Those
            // are answered on the final call, coarse but true, and
            // `trailing` is where a host lets them go.
            let trailing = if last {
                let left = state.reader.drain();
                left.iter()
                    .map(|record| state.reader.record_row(record))
                    .collect()
            } else {
                Vec::new()
            };
            Processed { rows, trailing }
        })
    }
}

/// What both sinks check before they read a byte: one video pad, a
/// codec they know, and no parameters.
fn open(streams: &[InputStream], params: &str, name: &str) -> Result<Framing, String> {
    no_params(params, name)?;
    if streams.len() != 1 {
        return Err(format!(
            "{name} reads one video stream, and it was opened for {}",
            streams.len()
        ));
    }
    let coded = &streams[0].coded;
    // `ffrwd-nal` refuses a codec it has no framing for without
    // formatting a string; which module is asking is this module's to
    // say.
    framing_of(&coded.codec, &coded.extradata)
        .map_err(|_| format!("{name} reads {} and not {}", CODECS.join(", "), coded.codec))
}

fn no_params(params: &str, name: &str) -> Result<(), String> {
    match params.trim() {
        "" | "{}" => Ok(()),
        other => Err(format!("{name} takes no params, and got: {other}")),
    }
}

export!(RecordsSink);
