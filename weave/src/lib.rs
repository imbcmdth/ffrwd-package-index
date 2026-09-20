//! `weave`: an ffrwd packet filter that puts embedding vectors into a
//! video's own encoded stream.
//!
//! Encoded packets arrive on one video pad and leave on it, one for
//! one, with their timestamps untouched; rows of vectors arrive beside
//! them, whenever the host has some. What the module adds is an SEI NAL
//! (H.264, HEVC) or a metadata OBU (AV1) holding the format's messages,
//! before the first coded slice of the access units the placement
//! policy chose. Nothing else in a packet moves, so the pictures a
//! player decodes are the pictures the encoder wrote.
//!
//! The crate is a shim and means to stay one. The decision of which
//! carrier a record rides is `ffrwd_index_rows::weave`, the bytes are
//! `ffrwd_index_core`, and both are tested on the native target where a
//! failure says something. What is here is the wit boundary, the params
//! ([`params`]) and the framing (`ffrwd_nal::config::Framing`).
//!
//! Two things the interface asks for and this module owes it:
//!
//! - **One packet in, one packet out, in decode order.** Packets are
//!   held while their presentation order settles, because the placement
//!   policy speaks of presentation time and a stream with B-frames does
//!   not arrive in it. They are released in the order they arrived, and
//!   everything held leaves on the final call.
//! - **No index.** Section 8's file index is not written here and
//!   cannot be: a module has no filesystem, and it runs before the
//!   muxer, so the file it would sit in does not exist yet. The rows
//!   this module writes are what a later step builds one from.

mod params;

wit_bindgen::generate!({
    path: "wit",
    world: "packet-filter-module",
});

use std::cell::RefCell;

use crate::ffrwd::av::types::Packet;
use exports::ffrwd::av::packet_filter::{
    Arity, CodedStream, Filtered, Guest, InputStream, Meta, PacketFilterMeta, PadPackets,
};

use ffrwd_index_core::message::{Message, Unit};
use ffrwd_index_core::placement::Placement;
use ffrwd_index_core::{SELECT, UNIT_SOFT_LIMIT};
use ffrwd_index_rows::weave::{Config, Reorder, Weaver, MAX_HELD_PACKETS};

use ffrwd_nal::config::{framing_of, Framing, CODECS};

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

/// And how many bytes, which is the bound that actually matters: a
/// held GOP is held in memory, and a thousand packets of 4K is not the
/// same thing as a thousand packets of a thumbnail. Thirty-two
/// mebibytes is about fifteen seconds at 20 Mbps.
const MAX_GOP_BYTES: usize = 32 << 20;

/// The access unit the writer is holding open under `keyframe`.
#[derive(Clone, Copy, Debug)]
struct OpenCarrier {
    /// Its presentation timestamp, in the stream's own base.
    pts: i64,
    /// Where it arrived in decode order, which is how it is found
    /// again in the hold.
    seq: u64,
}

/// What one open instance holds.
struct State {
    weaver: Weaver,
    reorder: Reorder<Packet>,
    framing: Framing,
    /// The stream's time base, as a fraction of a second.
    num: i64,
    den: i64,
    /// The keyframe still open, under `keyframe` placement alone.
    open: Option<OpenCarrier>,
    /// The params in force, kept so `set-params` can say what changed.
    config: Config,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct Weave;

impl Guest for Weave {
    fn describe() -> PacketFilterMeta {
        PacketFilterMeta {
            meta: Meta {
                name: "weave".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                params_schema: params::PARAMS_SCHEMA.to_string(),
                rows_schema: ROWS_SCHEMA.to_string(),
                // No decoded payload reaches a packet filter, so the
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
            // The vectors are the point: a host with no rows to give
            // has nothing for this module to do.
            reads_rows: true,
        }
    }

    fn init(streams: Vec<InputStream>, params: String) -> Result<Vec<CodedStream>, String> {
        if streams.len() != 1 {
            return Err(format!(
                "weave writes one video stream, and it was opened for {}",
                streams.len()
            ));
        }
        let config = params::read(&params)?;
        let coded = streams[0].coded.clone();
        let framing = framing_of(&coded.codec, &coded.extradata)
            .map_err(|_| format!("weave writes {} and not {}", CODECS.join(", "), coded.codec))?;
        if coded.time_base.den <= 0 || coded.time_base.num <= 0 {
            return Err(format!(
                "a time base of {}/{} is not a fraction of a second",
                coded.time_base.num, coded.time_base.den
            ));
        }
        STATE.with(|state| {
            *state.borrow_mut() = Some(State {
                weaver: Weaver::new(config.clone()),
                // The stream's own reorder depth is the bound on how far
                // decode order and presentation order differ, and it is
                // what settles the first packets, whose dts the wire
                // does not carry.
                reorder: Reorder::new(MAX_HELD_PACKETS, streams[0].decode_delay),
                framing,
                num: i64::from(coded.time_base.num),
                den: i64::from(coded.time_base.den),
                open: None,
                config,
            });
        });
        // Nothing out of band changes. The SPS and PPS the stream
        // opened with still describe every picture in it, and an SEI is
        // not something a decoder configuration mentions.
        Ok(vec![coded])
    }

    /// Between calls a caller may change what future records cost, and
    /// nothing else. The spaces and the placement are what the records
    /// already in flight were built against: changing a space's dims
    /// mid-run would leave a half-written record describing a vector
    /// the reader would rebuild wrongly, and changing the policy would
    /// mean throwing away the carriers already chosen. Either is
    /// refused, which leaves the previous params in force.
    fn set_params(params: String) -> Result<(), String> {
        let wanted = params::read(&params)?;
        STATE.with(|state| {
            let mut state = state.borrow_mut();
            let state = state.as_mut().ok_or("set-params before init")?;
            if wanted.spaces != state.config.spaces {
                return Err("weave cannot change its spaces mid-stream".to_string());
            }
            if wanted.placement != state.config.placement {
                return Err("weave cannot change its placement mid-stream".to_string());
            }
            state.weaver.set_escapes(wanted.escapes, wanted.plane_cap);
            state.config = wanted;
            Ok(())
        })
    }

    fn process(pads: Vec<PadPackets>, rows: Vec<String>, last: bool) -> Filtered {
        STATE.with(|cell| {
            let mut borrowed = cell.borrow_mut();
            let Some(state) = borrowed.as_mut() else {
                return Filtered {
                    pads: vec![PadPackets { packets: vec![] }; pads.len()],
                    rows: vec![],
                    trailing: vec![],
                };
            };
            // A real `&mut State`, so the weaver and the hold below can
            // be borrowed at the same time: they are separate fields.
            let state: &mut State = state;
            let mut written = Vec::new();

            // The rows first: one that arrived while the packets of
            // this call were flowing existed before them.
            for row in &rows {
                if let Some(reported) = state.weaver.row(row) {
                    written.push(reported);
                }
            }

            for pad in pads {
                for packet in pad.packets {
                    let pts = packet.pts;
                    let dts = packet.dts;
                    state.weaver.seen(to_ms(pts, state.num, state.den));
                    state.reorder.push(packet, pts, dts);
                }
            }
            if last {
                state.reorder.close();
            }

            // Carriers in presentation order, which is not the order the
            // packets arrived in.
            let mut added = Vec::new();
            while let Some(settled) = state.reorder.settle() {
                let pts = settled.pts;
                let seq = settled.seq;
                let pts_ms = to_ms(pts, state.num, state.den);
                let carried = state.weaver.carrier(pts, pts_ms, settled.item.keyframe);
                let open = carried.open;
                added.push(apply(
                    state.framing,
                    pts,
                    settled.item,
                    carried,
                    &mut written,
                ));
                if open {
                    // A keyframe may still be asked to take a record
                    // nothing else will carry, so it stays in hand, and
                    // so does every packet behind it: they cannot
                    // overtake it on the way out. The keyframe before
                    // this one is closed by the same stroke.
                    state.reorder.hold_from(seq);
                    state.open = Some(OpenCarrier { pts, seq });
                }
            }
            // Bounded: a GOP is held, not a stream. Past the bound the
            // keyframe goes out with what it has and takes no more, and
            // a record that would have ridden it falls to the next
            // keyframe or, at the end of the stream, is reported late.
            if state.reorder.barrier().is_some() {
                let (packets, bytes) = state.reorder.behind_barrier(|packet| packet.data.len());
                if packets > MAX_GOP_PACKETS || bytes > MAX_GOP_BYTES {
                    state.reorder.lift();
                    state.open = None;
                    state.weaver.note_overrun();
                }
            }
            // The final call carries the last packets. Under `keyframe`
            // what no carrier the policy would choose came along for
            // rides the LAST KEYFRAME, which is the access unit held
            // open for exactly this; under the live policies it rides
            // the last access unit of all.
            //
            // Where neither is in hand - a stream with no keyframe in
            // it, or one whose last GOP overran the bound - there is no
            // access unit a keyframe reader would visit, and the
            // records are reported late rather than written somewhere
            // nobody will look.
            if last {
                state.reorder.lift();
                let target: Option<(i64, u64)> = match state.config.placement {
                    Placement::Keyframe => state.open.take().map(|open| (open.pts, open.seq)),
                    _ => state.reorder.last_held().map(|(pts, seq, _)| (pts, seq)),
                };
                if let Some((pts, seq)) = target {
                    let keyframe = state.reorder.at(seq).is_some_and(|packet| packet.keyframe);
                    let pts_ms = to_ms(pts, state.num, state.den);
                    let carried = state.weaver.flush(pts, pts_ms, keyframe);
                    if let Some(packet) = state.reorder.at(seq) {
                        added.push(apply(state.framing, pts, packet, carried, &mut written));
                    }
                }
            }
            for grew in added {
                state.weaver.note_bytes(grew);
            }

            let released = state.reorder.release();
            let trailing = if last {
                state.weaver.trailing()
            } else {
                Vec::new()
            };
            Filtered {
                pads: vec![PadPackets { packets: released }],
                rows: written,
                trailing,
            }
        })
    }
}

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

export!(Weave);
