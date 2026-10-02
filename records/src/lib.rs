//! `records`: an ffrwd node that reads the vectors back out of a video
//! stream, a sink: rows alone leave it, on the run's rows.
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

use ffrwd_nal::config::{framing_of, Framing, CODECS};
use ffrwd_node::{Bound, Format, Init, Input, NoParams, Node, Out, Result, Shape, Tick, Wants};
use serde_json::value::RawValue;

use ffrwd_index_rows::read::Reader;

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

struct Records {
    v: u32,
    reader: Reader,
}

impl Node for Records {
    const NAME: &'static str = "records";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    const ROWS_SCHEMA: &'static str = ROWS_SCHEMA;
    type Params = NoParams;

    fn shape(_: &NoParams, _: &Bound) -> Result<Shape> {
        // Section 7's `keyframe` policy puts every record of a file on a
        // sync sample, so the keyframes are all this needs to answer a file
        // whole. A stream written `next` or `spread` puts records elsewhere
        // and a host that hands over more is what finds them; a host may
        // not hand over less than this asks for.
        Ok(Shape::new().input(
            Input::packets("v")
                .clock()
                .codecs(CODECS)
                .wants(Wants::Keyframes),
        ))
    }

    fn init(_: NoParams, init: &Init) -> Result<Records> {
        let (v, framing, num, den) = opened(init, "records")?;
        Ok(Records {
            v,
            reader: Reader::new(framing, num, den),
        })
    }

    fn process(&mut self, tick: &Tick, out: &mut Out) -> Result<()> {
        for packet in tick.packets(self.v) {
            self.reader.packet(packet.pts, &packet.data);
        }
        // A record whose every plane has arrived can gain nothing more, so
        // it goes out now rather than at the end: a `keyframe` file answers
        // each record on the packet that carried it.
        for record in self.reader.finished() {
            let row = self.reader.record_row(&record);
            report(out, row)?;
        }
        // What is left is what a writer never finished: a record sent with
        // a cap on its planes, which section 5 allows, or one whose later
        // planes the stream ended before. Those are answered on the final
        // call, coarse but true.
        if tick.last() {
            for record in self.reader.drain() {
                let row = self.reader.record_row(&record);
                report(out, row)?;
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

ffrwd_node::export!(Records);

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_node::{Kind, Runner};

    #[test]
    fn keyframes_in_and_rows_alone_out() {
        let shape = Runner::<Records>::shape("", &["v".to_owned()]).expect("a shape");
        let v = shape.find_input("v").expect("the packets");
        assert_eq!(v.kind, Kind::Packets);
        assert_eq!(v.accepts.wants, Wants::Keyframes);
        assert!(shape.outputs.is_empty(), "rows leave on the run's rows");
        assert!(Runner::<Records>::shape(r#"{"x":1}"#, &["v".to_owned()]).is_err());
    }
}
