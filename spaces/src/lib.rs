//! `spaces`: an ffrwd node that says which embedding spaces a video
//! stream carries, a sink: rows alone leave it, on the run's rows.
//!
//! One row per distinct SPACE declaration, which is section 3's message
//! field for field: what the vectors are, how many components they
//! have, which model made them and which model turns a search into the
//! same space. That is what a caller needs before it can ask anything
//! of the vectors themselves, and it is as cheap as a read gets.
//!
//! It asks for `first`, the least a sink can ask for, and section 3 is
//! what makes that answerable: a writer puts every space it is using
//! on every keyframe FROM THE FIRST KEYFRAME OF THE STREAM, whether or
//! not a record rides there, so the first packet of a file says what
//! the file carries. `tool/tests/sinks.rs` holds a sink to it: fed one
//! packet of a woven file, in three codecs and two containers, this
//! answers every space.
//!
//! A space the writer learned of after the stream began is the one
//! case the first packet cannot have: section 3 declares it on the
//! first carrier written for it and on every keyframe after that. A
//! host that hands over more than `first` finds those too, and `wants`
//! is a request rather than a promise for exactly that reason. This
//! sink reads whatever it is given and answers each space on the
//! packet that first declared it.
//!
//! The crate is a shim. The reading is `ffrwd_index_rows::read`, the
//! bytes are `ffrwd_index_core`, and both are tested on the native
//! target where a failure says something.

use ffrwd_nal::config::{framing_of, Framing, CODECS};
use ffrwd_node::{Bound, Format, Init, Input, NoParams, Node, Out, Result, Shape, Tick, Wants};
use serde_json::value::RawValue;

use ffrwd_index_rows::read::{space_row, Spaces as Declared};

/// What section 3 declares, one for one, and a `name` that is not in
/// the format at all: see `ffrwd_index_rows::read::name_of`.
const ROWS_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["space", "name", "dims", "encoding", "unit_length", "modality", "source", "model", "model_hash", "query", "query_hash", "producer"],
  "additionalProperties": false,
  "description": "One row per distinct SPACE declaration in the stream, which is SPEC section 3 field for field. A writer puts every space it is using on every keyframe from the first keyframe of the stream, so the first packet of a file says what the file carries.",
  "properties": {
    "space": {"type": "integer", "minimum": 0, "maximum": 255, "description": "The space id the stream's VECTOR messages name."},
    "name": {"type": "string", "description": "A label, not a field of the format: the producer where the writer gave one, else the model URI, else 'space <id>'."},
    "dims": {"type": "integer", "minimum": 1, "maximum": 65536},
    "encoding": {"type": "string", "enum": ["i8", "f16", "f32"]},
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

struct Spaces {
    v: u32,
    spaces: Declared,
}

impl Node for Spaces {
    const NAME: &'static str = "spaces";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    const ROWS_SCHEMA: &'static str = ROWS_SCHEMA;
    type Params = NoParams;

    fn shape(_: &NoParams, _: &Bound) -> Result<Shape> {
        // Section 3 puts every space on every keyframe from the first
        // keyframe of the stream, so the first packet of a file answers
        // this whole sink. It is a request rather than a promise: a host
        // may hand over more, and one that does is what finds a space the
        // writer learned of live.
        Ok(Shape::new().input(
            Input::packets("v")
                .clock()
                .codecs(CODECS)
                .wants(Wants::First),
        ))
    }

    fn init(_: NoParams, init: &Init) -> Result<Spaces> {
        let (v, framing, _, _) = opened(init, "spaces")?;
        Ok(Spaces {
            v,
            spaces: Declared::new(framing),
        })
    }

    fn process(&mut self, tick: &Tick, out: &mut Out) -> Result<()> {
        // Nothing is ever held: a space is answered on the packet that
        // declared it, so the final call has no leftovers.
        for packet in tick.packets(self.v) {
            for space in self.spaces.packet(&packet.data) {
                report(out, space_row(&space))?;
            }
        }
        Ok(())
    }
}

/// What a sink checks before it reads a byte: a coded stream on `v`, in a
/// codec it knows, and its time base.
fn opened(init: &Init, name: &str) -> Result<(u32, Framing, i64, i64)> {
    let v = init.stream("v")?;
    let Some(Format::Packets(coded)) = &v.format else {
        return Err(format!("{name} reads coded packets, and `v` is not a coded stream").into());
    };
    // `ffrwd-nal` refuses a codec it has no framing for without formatting
    // a string; which module is asking is this module's to say.
    let framing = framing_of(&coded.codec, &coded.extradata)
        .map_err(|_| format!("{name} reads {} and not {}", CODECS.join(", "), coded.codec))?;
    if coded.time_base.den <= 0 || coded.time_base.num <= 0 {
        return Err(format!(
            "a time base of {}/{} is not a fraction of a second",
            coded.time_base.num, coded.time_base.den
        )
        .into());
    }
    Ok((
        v.id,
        framing,
        i64::from(coded.time_base.num),
        i64::from(coded.time_base.den),
    ))
}

/// One row the sink wrote, handed on as it was written.
fn report(out: &mut Out, row: String) -> Result<()> {
    let raw = RawValue::from_string(row).map_err(|err| format!("a row: {err}"))?;
    Ok(out.report(&raw)?)
}

ffrwd_node::export!(Spaces);

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_node::{Kind, Runner};

    #[test]
    fn the_first_packet_in_and_rows_alone_out() {
        let shape = Runner::<Spaces>::shape("", &["v".to_owned()]).expect("a shape");
        let v = shape.find_input("v").expect("the packets");
        assert_eq!(v.kind, Kind::Packets);
        assert_eq!(v.accepts.wants, Wants::First);
        assert!(shape.outputs.is_empty(), "rows leave on the run's rows");
        assert!(Runner::<Spaces>::shape(r#"{"x":1}"#, &["v".to_owned()]).is_err());
    }
}
