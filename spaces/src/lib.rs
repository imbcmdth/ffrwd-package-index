//! `spaces`: an ffrwd packet sink that says which embedding spaces a
//! video stream carries.
//!
//! One row per distinct SPACE declaration, which is section 3's message
//! field for field: what the vectors are, how many components they
//! have, which model made them and which model turns a search into the
//! same space. That is what a caller needs before it can ask anything
//! of the vectors themselves, and it is cheap: section 3 has a writer
//! repeat every space on the first carrier it writes to and on every
//! keyframe after it, so the keyframes of a stream answer this whole
//! sink.
//!
//! It asks for `keyframes` and not `first`, which is a sharper
//! distinction than it looks. "The first carrier the writer writes to"
//! is not the first packet: a `keyframe` writer with nothing to say
//! until its first record's span has ended declares its spaces on
//! whichever keyframe that turns out to be, and in a file of
//! two-second GOPs that is routinely the second or the third. A sink
//! asking for `first` would be handed a packet that declares nothing
//! and would answer an empty relation. The keyframes are exactly the
//! packets section 3 promises the declarations are on.
//!
//! It is a request and not a promise either way: a host may hand over
//! the whole stream, and this one reads whatever it gets, answering
//! each space on the packet that first declared it.
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

use ffrwd_index_rows::read::{space_row, Spaces};
use ffrwd_index_rows::stream::{framing_of, Framing, CODECS};

const PARAMS_SCHEMA: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;

/// What section 3 declares, one for one, and a `name` that is not in
/// the format at all: see `ffrwd_index_rows::read::name_of`.
const ROWS_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["space", "name", "dims", "encoding", "unit_length", "modality", "source", "model", "model_hash", "query", "query_hash", "producer"],
  "additionalProperties": false,
  "description": "One row per distinct SPACE declaration in the stream, which is SPEC section 3 field for field. A writer repeats every space on every keyframe from the first one it writes to, which is why this sink asks for the keyframes and not for the first packet.",
  "properties": {
    "space": {"type": "integer", "minimum": 0, "maximum": 255, "description": "The space id the stream's VECTOR messages name."},
    "name": {"type": "string", "description": "A label, not a field of the format: the producer where the writer gave one, else the model URI, else 'space <id>'."},
    "dims": {"type": "integer", "minimum": 1, "maximum": 65536},
    "encoding": {"enum": ["i8", "f16", "f32"]},
    "unit_length": {"type": "boolean", "description": "Whether the vectors were unit length before they were encoded."},
    "modality": {"type": "string", "description": "What was embedded: unspecified, picture, sound, speech, sound-text, scene-text, description, or the number of one this version does not name."},
    "source": {"type": "integer", "minimum": 0, "maximum": 255},
    "model": {"type": "string", "description": "A URI for what made the vectors."},
    "model_hash": {"type": "string", "description": "The first sixteen bytes of the SHA-256 of the weights, as hex; empty where the writer did not say."},
    "query": {"type": "string", "description": "A URI for what embeds a query into the same space; empty means the same as model."},
    "query_hash": {"type": "string"},
    "producer": {"type": "string"}
  }
}"#;

struct State {
    spaces: Spaces,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct SpacesSink;

impl Guest for SpacesSink {
    fn describe() -> PacketSinkMeta {
        PacketSinkMeta {
            meta: Meta {
                name: "spaces".to_string(),
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
            // Section 3 repeats every space on the first carrier a
            // writer writes to and on every keyframe AFTER it, which is
            // not the same as the first packet: a writer with nothing
            // to say on frame one declares nothing there. The keyframes
            // are what the declarations are promised to be on.
            wants: Wants::Keyframes,
        }
    }

    fn init(streams: Vec<InputStream>, params: String) -> Result<(), String> {
        let framing = open(&streams, &params, "spaces")?;
        STATE.with(|state| {
            *state.borrow_mut() = Some(State {
                spaces: Spaces::new(framing),
            });
        });
        Ok(())
    }

    fn set_params(params: String) -> Result<(), String> {
        no_params(&params, "spaces")
    }

    fn process(pads: Vec<PadPackets>, last: bool) -> Processed {
        let _ = last;
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
                    for space in state.spaces.packet(&packet.data) {
                        rows.push(space_row(&space));
                    }
                }
            }
            // Nothing is ever held: a space is answered on the packet
            // that declared it, so the final call has no leftovers and
            // `trailing` stays empty, which is the only thing a host
            // lets a call that is not the last one do.
            Processed {
                rows,
                trailing: vec![],
            }
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
    framing_of(&coded.codec, &coded.extradata)
}

fn no_params(params: &str, name: &str) -> Result<(), String> {
    match params.trim() {
        "" | "{}" => Ok(()),
        other => Err(format!("{name} takes no params, and got: {other}")),
    }
}

export!(SpacesSink);
