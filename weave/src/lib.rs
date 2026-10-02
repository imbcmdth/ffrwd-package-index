//! `weave`: an ffrwd node that puts embedding vectors into a video's own
//! encoded stream.
//!
//! Encoded packets arrive on `v` and leave on `v`, one for one, with their
//! timestamps untouched; rows of vectors arrive on an input per declared
//! space, each named for its space. What the module adds is an SEI NAL
//! (H.264, HEVC) or a metadata OBU (AV1) holding the format's messages,
//! before the first coded slice of the access units the placement policy
//! chose. Nothing else in a packet moves, so the pictures a player decodes
//! are the pictures the encoder wrote.
//!
//! The crate is a shim and means to stay one. The decision of which
//! carrier a record rides is `ffrwd_index_rows::weave`, the bytes are
//! `ffrwd_index_core`, and both are tested on the native target where a
//! failure says something. What is here is the node's shape, the params
//! ([`params`]) and the framing (`ffrwd_nal::config::Framing`).
//!
//! Three things the world asks for and this module owes it:
//!
//! - **One packet in, one packet out, in decode order.** Packets are
//!   held while their presentation order settles, because the placement
//!   policy speaks of presentation time and a stream with B-frames does
//!   not arrive in it. They are released in the order they arrived, and
//!   everything held leaves on the final call.
//! - **A latency it keeps.** Under `keyframe` a keyframe is held with its
//!   whole GOP so it can still take a record; the hold is bounded in
//!   packets, bytes and seconds, and the seconds are what `v` declares.
//! - **No index.** Section 8's file index is not written here and
//!   cannot be: a module has no filesystem, and it runs before the
//!   muxer, so the file it would sit in does not exist yet. The rows
//!   this module writes are what a later step builds one from.

mod params;

use ffrwd_index_core::message::{Message, Unit};
use ffrwd_index_core::placement::Placement;
use ffrwd_index_core::{SELECT, UNIT_SOFT_LIMIT};
use ffrwd_index_rows::weave::{Config, Reorder, Weaver, MAX_HELD_PACKETS};
use ffrwd_nal::config::{framing_of, Framing, CODECS};
use ffrwd_node::{
    Bound, Format, Init, Input, Node, Out, Output, Packet, Rational, Result, Shape, StateRow, Tick,
    Wants,
};
use serde_json::value::RawValue;

const ROWS_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["event"],
  "additionalProperties": false,
  "description": "One row per record put into the stream, one per record nothing could carry, one per row that could not be read, and one summary at the end. A later step builds the file index of SPEC section 8 from the woven rows; this module writes no index, having no filesystem and running before the muxer.",
  "properties": {
    "event": {"type": "string", "enum": ["woven", "late", "dropped", "summary"]},
    "space": {"type": "string"},
    "record_id": {"type": "integer"},
    "carrier_pts": {"type": "integer", "description": "The presentation timestamp of the access unit that carried it, in the stream's own time base."},
    "carrier_t": {"type": "number"},
    "start_t": {"type": "number"},
    "end_t": {"type": "number"},
    "start_off_ms": {"type": "integer"},
    "end_off_ms": {"type": "integer"},
    "bytes": {"type": "integer", "description": "The bytes of this record's messages on that carrier, before the NAL or OBU framing around them."},
    "planes": {"type": "array", "items": {"type": "integer"}},
    "reason": {"type": "string"},
    "row": {"type": "string"},
    "records": {"type": "integer"},
    "late": {"type": "integer"},
    "dropped": {"type": "integer"},
    "bytes_added": {"type": "integer", "description": "Every byte the packets grew by, framing included."},
    "spaces": {"type": "integer"},
    "overran": {"type": "integer", "description": "Access units that went out before the placement could be asked for more, because the writer's hold reached its bound."}
  }
}"#;

/// What one space's input reads: a span and a vector, and a `space` of its
/// own where the row names one.
const VECTORS_SCHEMA: &str = r#"{"type":"object","properties":{"start_t":{"type":"number"},"end_t":{"type":"number"},"vector":{"type":"array","items":{"type":"number"}}},"required":["start_t","end_t","vector"]}"#;

/// How many packets one held GOP may be before the keyframe goes out
/// with what it has.
///
/// The reorder cap is a handful of frames and is far too small for
/// this: a GOP is seconds of video, and under `keyframe` the whole of
/// it is held so that its keyframe can still take a record nothing
/// else will carry. A thousand packets is forty seconds at 25 fps and
/// ten at a hundred, which is longer than any keyframe interval worth
/// writing.
const MAX_GOP_PACKETS: usize = 1024;

/// And how many bytes, which is the bound that matters for memory: a
/// held GOP is held in memory, and a thousand packets of 4K is not the
/// same thing as a thousand packets of a thumbnail. Thirty-two
/// mebibytes is about fifteen seconds at 20 Mbps.
const MAX_GOP_BYTES: usize = 32 << 20;

/// And how many seconds of presentation time, which is the bound the
/// node promises downstream: `v` declares it as its latency.
const MAX_GOP_SECONDS: f64 = 10.0;

/// How many frames a packet held while its presentation order settles may
/// trail its tick by: the deepest reorder H.264 and HEVC allow.
const REORDER_FRAMES: u64 = 16;

/// The same in seconds where the call leaves the stream's rate unknown.
const REORDER_SECONDS: f64 = 1.0;

/// The access unit the writer is holding open under `keyframe`.
#[derive(Clone, Copy, Debug)]
struct OpenCarrier {
    /// Its presentation timestamp, in the stream's own base.
    pts: i64,
    /// Where it arrived in decode order, which is how it is found
    /// again in the hold.
    seq: u64,
}

struct Weave {
    v: u32,
    weaver: Weaver,
    reorder: Reorder<Packet>,
    framing: Framing,
    /// The stream's time base, as a fraction of a second.
    num: i64,
    den: i64,
    /// The keyframe still open, under `keyframe` placement alone.
    open: Option<OpenCarrier>,
    /// The newest presentation time a packet has carried, in the stream's
    /// own base.
    newest: Option<i64>,
    /// The params in force, kept so `set-params` can say what changed.
    config: Config,
    /// Rows the weaver wrote about rows folded in before this tick.
    reported: Vec<String>,
}

fn read(params: &serde_json::Value) -> Result<Config, String> {
    params::read(&params.to_string())
}

/// How late a packet may leave `v` under `placement`, at the stream's
/// frame rate where the call says what that is.
fn latency(placement: Placement, rate: Option<Rational>) -> f64 {
    let reorder = rate.map_or(REORDER_SECONDS, |rate| rate.duration(REORDER_FRAMES));
    match placement {
        Placement::Keyframe => MAX_GOP_SECONDS + reorder,
        _ => reorder,
    }
}

impl Node for Weave {
    const NAME: &'static str = "weave";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");
    const PARAMS_SCHEMA: &'static str = params::PARAMS_SCHEMA;
    const ROWS_SCHEMA: &'static str = ROWS_SCHEMA;
    type Params = serde_json::Value;

    fn shape(params: &serde_json::Value, bound: &Bound) -> Result<Shape> {
        let config = read(params)?;
        let mut shape =
            Shape::new().input(Input::packets("v").clock().codecs(CODECS).wants(Wants::All));
        for (name, _) in &config.spaces {
            shape = shape.input(
                Input::rows(name)
                    .optional()
                    .interval()
                    .state()
                    .schema_json(VECTORS_SCHEMA),
            );
        }
        // No format of its own: the clock input's, so the packets leave in
        // the codec, time base, geometry and extradata they came in.
        let latency = latency(config.placement, bound.rate_of("v"));
        Ok(shape.output(Output::packets("v").latency(latency)))
    }

    fn init(params: serde_json::Value, init: &Init) -> Result<Weave> {
        let config = read(&params)?;
        let v = init.stream("v")?;
        let Some(Format::Packets(coded)) = &v.format else {
            return Err("weave writes coded packets, and `v` is not a coded stream".into());
        };
        let framing = framing_of(&coded.codec, &coded.extradata)
            .map_err(|_| format!("weave writes {} and not {}", CODECS.join(", "), coded.codec))?;
        if coded.time_base.den <= 0 || coded.time_base.num <= 0 {
            return Err(format!(
                "a time base of {}/{} is not a fraction of a second",
                coded.time_base.num, coded.time_base.den
            )
            .into());
        }
        Ok(Weave {
            v: v.id,
            weaver: Weaver::new(config.clone()),
            // The stream's own reorder depth is the bound on how far
            // decode order and presentation order differ, and it is what
            // settles the first packets, whose dts the wire does not
            // carry.
            reorder: Reorder::new(MAX_HELD_PACKETS, v.decode_delay),
            framing,
            num: i64::from(coded.time_base.num),
            den: i64::from(coded.time_base.den),
            open: None,
            newest: None,
            config,
            reported: Vec::new(),
        })
    }

    /// Between calls a caller may change what future records cost, and
    /// nothing else. The spaces and the placement are what the records
    /// already in flight were built against: changing a space's dims
    /// mid-run would leave a half-written record describing a vector
    /// the reader would rebuild wrongly, and changing the policy would
    /// mean throwing away the carriers already chosen. Either is
    /// refused, which leaves the previous params in force.
    fn set_params(&mut self, params: serde_json::Value) -> Result<()> {
        let wanted = read(&params)?;
        if wanted.spaces != self.config.spaces {
            return Err("weave cannot change its spaces mid-stream".into());
        }
        if wanted.placement != self.config.placement {
            return Err("weave cannot change its placement mid-stream".into());
        }
        self.weaver.set_escapes(wanted.escapes, wanted.plane_cap);
        self.config = wanted;
        Ok(())
    }

    fn fold(&mut self, row: StateRow) -> Result<()> {
        if let Some(reported) = self.weaver.row_on(row.port, row.json) {
            self.reported.push(reported);
        }
        Ok(())
    }

    fn process(&mut self, tick: &Tick, out: &mut Out) -> Result<()> {
        // The rows first: they were folded in before the packets of this
        // tick, which is when they existed.
        let mut written = std::mem::take(&mut self.reported);

        for packet in tick.packets(self.v) {
            let pts = packet.pts;
            let dts = packet.dts;
            self.weaver.seen(to_ms(pts, self.num, self.den));
            self.newest = Some(self.newest.map_or(pts, |newest| newest.max(pts)));
            self.reorder.push(packet, pts, dts);
        }
        let last = tick.last();
        if last {
            self.reorder.close();
        }

        // Carriers in presentation order, which is not the order the
        // packets arrived in.
        let mut added = Vec::new();
        while let Some(settled) = self.reorder.settle() {
            let pts = settled.pts;
            let seq = settled.seq;
            let pts_ms = to_ms(pts, self.num, self.den);
            let carried = self.weaver.carrier(pts, pts_ms, settled.item.keyframe);
            let open = carried.open;
            added.push(apply(
                self.framing,
                pts,
                settled.item,
                carried,
                &mut written,
            ));
            if open {
                // A keyframe may still be asked to take a record nothing
                // else will carry, so it stays in hand, and so does every
                // packet behind it: they cannot overtake it on the way out.
                // The keyframe before this one is closed by the same stroke.
                self.reorder.hold_from(seq);
                self.open = Some(OpenCarrier { pts, seq });
            }
        }
        // Bounded: a GOP is held, not a stream. Past the bound the
        // keyframe goes out with what it has and takes no more, and a
        // record that would have ridden it falls to the next keyframe or,
        // at the end of the stream, is reported late.
        if self.reorder.barrier().is_some() {
            let (packets, bytes) = self.reorder.behind_barrier(|packet| packet.data.len());
            let held = match (self.open, self.newest) {
                (Some(open), Some(newest)) => {
                    (newest - open.pts) as f64 * self.num as f64 / self.den as f64
                }
                _ => 0.0,
            };
            if packets > MAX_GOP_PACKETS || bytes > MAX_GOP_BYTES || held > MAX_GOP_SECONDS {
                self.reorder.lift();
                self.open = None;
                self.weaver.note_overrun();
            }
        }
        // The final call carries the last packets. Under `keyframe` what
        // no carrier the policy would choose came along for rides the LAST
        // KEYFRAME, which is the access unit held open for exactly this;
        // under the live policies it rides the last access unit of all.
        //
        // Where neither is in hand (a stream with no keyframe in it, or
        // one whose last GOP overran the bound) there is no access unit a
        // keyframe reader would visit, and the records are reported late
        // rather than written somewhere nobody will look.
        if last {
            self.reorder.lift();
            let target: Option<(i64, u64)> = match self.config.placement {
                Placement::Keyframe => self.open.take().map(|open| (open.pts, open.seq)),
                _ => self.reorder.last_held().map(|(pts, seq, _)| (pts, seq)),
            };
            if let Some((pts, seq)) = target {
                let keyframe = self.reorder.at(seq).is_some_and(|packet| packet.keyframe);
                let pts_ms = to_ms(pts, self.num, self.den);
                let carried = self.weaver.flush(pts, pts_ms, keyframe);
                if let Some(packet) = self.reorder.at(seq) {
                    added.push(apply(self.framing, pts, packet, carried, &mut written));
                }
            }
        }
        for grew in added {
            self.weaver.note_bytes(grew);
        }

        for packet in self.reorder.release() {
            out.packet("v", packet)?;
        }
        if last {
            written.extend(self.weaver.trailing());
        }
        for row in written {
            let raw = RawValue::from_string(row).map_err(|err| format!("a row: {err}"))?;
            out.report(&raw)?;
        }
        Ok(())
    }
}

ffrwd_node::export!(Weave);

/// Puts one carrier's messages into its packet, and says how much the
/// packet grew. A packet this module cannot read is handed on exactly
/// as it arrived: the container still gets its picture, and a row says
/// the vectors did not travel.
fn apply(
    framing: Framing,
    pts: i64,
    packet: &mut Packet,
    carried: ffrwd_index_rows::weave::Carried,
    written: &mut Vec<String>,
) -> usize {
    written.extend(carried.rows);
    if carried.messages.is_empty() {
        return 0;
    }
    let was = packet.data.len();
    match weave_into(framing, &packet.data, &carried.messages) {
        Ok(woven) => {
            packet.data = woven;
            packet.data.len() - was
        }
        Err(reason) => {
            written.push(refused_row(pts, &reason));
            0
        }
    }
}

/// One access unit with every message of its carrier in it.
///
/// Section 7 asks a writer to put everything it has for a carrier in
/// one unit and lets a reader accept several, and section 7 also asks a
/// unit to stay under 4096 bytes. A keyframe that a dozen records all
/// chose is past that on its own, so the messages are cut into as few
/// units as fit and each goes in its own NAL or OBU, in order.
fn weave_into(framing: Framing, packet: &[u8], messages: &[Message]) -> Result<Vec<u8>, String> {
    let mut out = packet.to_vec();
    for batch in batches(messages) {
        let unit = Unit::new(batch).encode();
        out = framing
            .insert(&out, &unit, SELECT)
            .map_err(|err| err.to_string())?;
    }
    Ok(out)
}

/// The messages of one carrier, cut into units no larger than the soft
/// limit. A single message past the limit on its own still goes: a
/// record is worth more than a ceiling the spec calls a preference.
fn batches(messages: &[Message]) -> Vec<Vec<Message>> {
    /// The UUID and the version byte every unit opens with.
    const HEADER: usize = 17;
    let mut out: Vec<Vec<Message>> = Vec::new();
    let mut room = 0usize;
    for message in messages {
        let size = message.encoded_len();
        match out.last_mut() {
            Some(batch) if size <= room => {
                room -= size;
                batch.push(message.clone());
            }
            _ => {
                room = UNIT_SOFT_LIMIT.saturating_sub(HEADER + size);
                out.push(vec![message.clone()]);
            }
        }
    }
    out
}

/// Milliseconds of presentation time from one timestamp in the stream's
/// own time base.
fn to_ms(pts: i64, num: i64, den: i64) -> i64 {
    let ticks = i128::from(pts) * i128::from(num) * 1000 / i128::from(den);
    ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn refused_row(pts: i64, reason: &str) -> String {
    use ffrwd_index_rows::json::{object, string};
    object(vec![
        ("event", string("dropped")),
        (
            "reason",
            string(format!(
                "the packet at {pts} could not be written: {reason}"
            )),
        ),
        ("row", string("")),
    ])
    .write()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_node::{Kind, Pairing, RowsUse, Runner};

    const TWO: &str =
        r#"{"spaces":"[{\"name\":\"clip\",\"dims\":4},{\"name\":\"speech\",\"dims\":4}]"}"#;

    #[test]
    fn a_space_is_an_input_named_for_it() {
        let bound = ["v".to_owned(), "speech".to_owned()];
        let shape = Runner::<Weave>::shape(TWO, &bound).expect("a shape");
        assert_eq!(shape.clock_input(), Some("v"));
        let v = shape.find_input("v").expect("the packets");
        assert_eq!(v.kind, Kind::Packets);
        assert_eq!(v.accepts.codecs, CODECS);
        let names: Vec<&str> = shape
            .inputs
            .iter()
            .map(|input| input.name.as_str())
            .collect();
        assert_eq!(names, ["v", "clip", "speech"]);
        for name in ["clip", "speech"] {
            let input = shape.find_input(name).expect("a space");
            assert_eq!(input.kind, Kind::Data);
            assert!(
                !input.required,
                "a space a call has no producer for is left unbound"
            );
            assert!(matches!(input.pairing, Pairing::Interval(_)));
            assert_eq!(input.rows, RowsUse::State);
        }
    }

    #[test]
    fn the_packets_leave_like_they_came_and_as_late_as_a_gop_is_held() {
        let shape = Runner::<Weave>::shape(TWO, &["v".to_owned()]).expect("a shape");
        assert_eq!(shape.outputs.len(), 1);
        let v = &shape.outputs[0];
        assert_eq!((v.name.as_str(), v.kind), ("v", Kind::Packets));
        assert!(
            v.format.is_none() && v.like.is_none(),
            "the clock input's format"
        );
        assert_eq!(v.latency, MAX_GOP_SECONDS + REORDER_SECONDS);
        assert!(!shape.pure);

        let next = r#"{"spaces":"[{\"name\":\"clip\",\"dims\":4}]","placement":"next"}"#;
        let shape = Runner::<Weave>::shape(next, &["v".to_owned()]).expect("a shape");
        assert_eq!(shape.outputs[0].latency, REORDER_SECONDS);
    }

    #[test]
    fn at_a_bound_rate_the_reorder_is_sixteen_frames_of_it() {
        let at = |params: &str, rate: Rational| {
            let bound = Bound::new(&["v"]).rate("v", rate);
            Runner::<Weave>::shape(params, &bound).expect("a shape").outputs[0].latency
        };
        let next = r#"{"spaces":"[{\"name\":\"clip\",\"dims\":4}]","placement":"next"}"#;
        assert_eq!(at(next, Rational::new(25, 1)), 0.64);
        assert_eq!(at(next, Rational::new(60000, 1001)), 16.0 * 1001.0 / 60000.0);
        assert_eq!(at(TWO, Rational::new(25, 1)), MAX_GOP_SECONDS + 0.64);
    }

    #[test]
    fn a_shape_with_no_spaces_is_refused() {
        let error =
            Runner::<Weave>::shape(r#"{"spaces":"[]"}"#, &["v".to_owned()]).expect_err("no spaces");
        assert!(error.contains("at least one space"), "{error}");
    }

    #[test]
    fn a_space_named_like_the_packets_is_refused() {
        let params = r#"{"spaces":"[{\"name\":\"v\",\"dims\":4}]"}"#;
        assert!(Runner::<Weave>::shape(params, &["v".to_owned()]).is_err());
    }
}
