//! Packets in, rows out: what the two reading sinks both do.
//!
//! A packet sink is handed the encoder's own output, one access unit at
//! a time with the timestamp the container will carry. Taking this
//! format back out of it is the mirror of weaving it in: find the units
//! in front of the picture (`ffrwd_nal::config::Framing::payloads`),
//! push their
//! messages at [`Assembler`], and answer rows for what comes back
//! together. The bytes of the picture are never looked at.
//!
//! Both sinks are here rather than in their wasm crates for the reason
//! the weaver is: a failure in a native test says something, and a
//! failure inside a wasm component says a trap.
//!
//! **Times.** Section 4 carries offsets from the carrier and nothing
//! else, so a span is absolute only once the carrier's own presentation
//! time is known. A packet sink is told it exactly: `pts` in the
//! stream's time base, which is the container's clock and not a frame
//! rate guessed at. That is the difference between this and reading an
//! elementary stream.
//!
//! **What a keyframe copy leaves out.** Section 7's `keyframe` policy
//! puts every record on a sync sample, so a reader fed the keyframes
//! alone has the whole file. A stream written `next` or `spread` puts
//! them on any frame, and a keyframe copy of one is missing whatever
//! rode elsewhere; nothing in the format says which policy wrote a
//! stream, so neither sink can tell the caller it is looking at half a
//! file. Fed every packet, both are right either way.

use std::collections::BTreeMap;

use ffrwd_index_core::assemble::{Assembler, Limits, Record};
use ffrwd_index_core::message::{Message, Space, Unit, VectorBody};

use ffrwd_index_core::SELECT;
use ffrwd_nal::config::Framing;

use crate::json::{float, number, object, string, Json};
use crate::space::hex;
use crate::vector::plane_numbers;

/// How many records a sink holds while their planes arrive.
///
/// Under `keyframe` a record is whole on one carrier and is answered
/// the moment its packet is read, so this is only ever reached by a
/// `spread` stream, where the planes of thousands of records may be in
/// flight at once. Past it the assembler drops the oldest, and says so
/// in its own count.
pub const MAX_HELD_RECORDS: usize = 4096;

/// How many records may be in slices at once, which is a `spread`
/// writer's doing and nobody else's.
pub const MAX_HELD_SLICES: usize = 1024;

/// A reader that looks for SPACE declarations and nothing else.
///
/// Section 3 has a writer put every space it is using on every keyframe
/// from the first keyframe of the stream, so one packet is all this
/// ever needs for a space the writer had before it started. It is kept
/// apart from [`Reader`] because assembling records a caller did not
/// ask for is work, and a stream written `next` puts a record on every
/// frame.
pub struct Spaces {
    framing: Framing,
    declared: BTreeMap<u8, Space>,
    packets: u64,
}

impl Spaces {
    pub fn new(framing: Framing) -> Self {
        Self {
            framing,
            declared: BTreeMap::new(),
            packets: 0,
        }
    }

    /// The spaces this packet declared that no packet had declared
    /// before, in the order it declared them.
    pub fn packet(&mut self, data: &[u8]) -> Vec<Space> {
        self.packets += 1;
        let mut fresh = Vec::new();
        for bytes in self.framing.payloads(data, SELECT) {
            let Ok(unit) = Unit::decode(&bytes) else {
                continue;
            };
            for message in &unit.messages {
                let Message::Space(space) = message else {
                    continue;
                };
                if self.declared.get(&space.space_id) != Some(space) {
                    self.declared.insert(space.space_id, space.clone());
                    fresh.push(space.clone());
                }
            }
        }
        fresh
    }

    /// Every space declared so far, in id order.
    pub fn spaces(&self) -> impl Iterator<Item = &Space> {
        self.declared.values()
    }

    pub fn packets(&self) -> u64 {
        self.packets
    }
}

/// One reader over one pad's packets.
pub struct Reader {
    assembler: Assembler,
    framing: Framing,
    /// The stream's time base, as a fraction of a second.
    num: i64,
    den: i64,
    /// Every distinct SPACE declaration answered so far, so that the
    /// same space redeclared on every keyframe is one row.
    declared: BTreeMap<u8, Space>,
    /// How many record rows have gone out, which is what numbers them.
    index: u64,
    packets: u64,
}

impl Reader {
    /// A reader for one pad, from what `init` was told about it.
    pub fn new(framing: Framing, num: i64, den: i64) -> Self {
        Self {
            assembler: Assembler::new(Limits {
                max_records: MAX_HELD_RECORDS,
                max_reassemblies: MAX_HELD_SLICES,
                // A reader of packets is a reader of a whole stream,
                // and a VECTOR whose SPACE has not arrived is waiting
                // for the next keyframe rather than for nothing.
                orphan_wait_ms: i64::MAX,
            }),
            framing,
            num,
            den,
            declared: BTreeMap::new(),
            index: 0,
            packets: 0,
        }
    }

    /// One packet, with the presentation timestamp the wire gave it.
    ///
    /// Answers the spaces this packet declared that no packet had
    /// declared before, in the order it declared them.
    pub fn packet(&mut self, pts: i64, data: &[u8]) -> Vec<Space> {
        self.packets += 1;
        let carrier_ms = to_ms(pts, self.num, self.den);
        let mut fresh = Vec::new();
        for bytes in self.framing.payloads(data, SELECT) {
            // A unit of a version this build does not know, or one that
            // belongs to somebody else, is not an error: it is somebody
            // else's business.
            let Ok(unit) = Unit::decode(&bytes) else {
                continue;
            };
            for message in &unit.messages {
                if let Message::Space(space) = message {
                    // Section 3: a space id means what the most recent
                    // declaration said. A writer changing a definition
                    // should use a new id, so a changed one is a second
                    // space under one name and is answered as one.
                    if self.declared.get(&space.space_id) != Some(space) {
                        self.declared.insert(space.space_id, space.clone());
                        fresh.push(space.clone());
                    }
                }
            }
            self.assembler.push_unit(carrier_ms, &unit);
        }
        fresh
    }

    /// Every record nothing more can be added to, in the order their
    /// first message arrived.
    pub fn finished(&mut self) -> Vec<Record> {
        self.assembler.take_finished()
    }

    /// Everything left, however coarse: a record sent with fewer than
    /// eight planes is finished only when the stream is.
    pub fn drain(&mut self) -> Vec<Record> {
        self.assembler.drain()
    }

    /// Every space declared so far, in id order.
    pub fn spaces(&self) -> impl Iterator<Item = &Space> {
        self.declared.values()
    }

    /// How many packets have been read.
    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// How many messages the assembler could not use.
    pub fn dropped(&self) -> usize {
        self.assembler.dropped()
    }

    /// The bytes held for records that are not finished.
    pub fn footprint(&self) -> usize {
        self.assembler.footprint()
    }

    /// One record as the row `records` answers, numbered from one in
    /// the order the rows go out.
    pub fn record_row(&mut self, record: &Record) -> String {
        self.index += 1;
        record_row(self.index, record)
    }
}

/// One record as a row.
///
/// `start_t` and `end_t` are seconds of presentation time, absolute:
/// the carrier's own timestamp plus the offsets section 4 carries. The
/// vector is at whatever precision arrived and is not normalized, since
/// a caller that wants unit length can do it and one that wants the
/// numbers the writer had cannot undo it. `unit_length` on the space
/// says what the originals were.
pub fn record_row(index: u64, record: &Record) -> String {
    let values = record.values().unwrap_or_default();
    let mut members = vec![
        ("index", number(index as f64)),
        ("space", number(record.space.space_id)),
        ("record_id", number(record.record_id)),
        ("start_t", seconds(record.start_ms)),
        ("end_t", seconds(record.end_ms)),
    ];
    if let VectorBody::I8(planes) = &record.body {
        members.push((
            "planes",
            Json::Array(
                plane_numbers(planes.present())
                    .into_iter()
                    .map(number)
                    .collect(),
            ),
        ));
    }
    members.push((
        "vector",
        Json::Array(values.into_iter().map(float).collect()),
    ));
    object(members).write()
}

/// One space as a row.
///
/// `name` is not in the format. Section 3 gives a space an id, a shape
/// and two model URIs, and nothing a person would call a name, so this
/// is a label derived from what is there: the `producer` where the
/// writer gave one, else the `model` URI, else the id. Two spaces whose
/// writer named the same producer get the same label, which is the
/// writer's doing and not this reader's to fix.
pub fn space_row(space: &Space) -> String {
    object(vec![
        ("space", number(space.space_id)),
        ("name", string(name_of(space))),
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
    .write()
}

/// What to call a space that carries no name.
pub fn name_of(space: &Space) -> String {
    if !space.producer.is_empty() {
        return space.producer.clone();
    }
    if !space.model.is_empty() {
        return space.model.clone();
    }
    format!("space {}", space.space_id)
}

/// Milliseconds of presentation time from a timestamp in a stream's own
/// time base.
pub fn to_ms(pts: i64, num: i64, den: i64) -> i64 {
    if den == 0 {
        return pts;
    }
    let ticks = i128::from(pts) * i128::from(num) * 1000 / i128::from(den);
    ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn seconds(ms: i64) -> Json {
    number(ms as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_index_core::message::Encoding;
    use ffrwd_index_core::placement::{plan, Carrier, Pending, Placement};
    use ffrwd_index_core::quant::Planes;
    use ffrwd_nal::Codec;

    fn space(id: u8, dims: u32) -> Space {
        let mut space = Space::new(id, dims, Encoding::I8);
        space.model = "hf:test/model@main/weights.safetensors".into();
        space.producer = "the read tests".into();
        space
    }

    fn values(index: usize, dims: usize) -> Vec<f32> {
        (0..dims)
            .map(|c| ((c + index * 5) as f32 * 0.37).sin())
            .collect()
    }

    /// One Annex B access unit with a unit of this format in it, shaped
    /// the way an encoder's packet is: parameter sets, the SEI, then a
    /// coded slice.
    fn packet(messages: Vec<Message>, keyframe: bool) -> Vec<u8> {
        let mut out: Vec<u8> = vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x0a, 0x96];
        if !messages.is_empty() {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&ffrwd_nal::sei::write_user_data(
                &Unit::new(messages).encode(),
                Codec::H264,
            ));
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.push(if keyframe { 0x65 } else { 0x41 });
        out.extend_from_slice(&[0x88, 0x84, 0x00, 0x21]);
        out
    }

    /// The messages one writer would put on each of `count` carriers,
    /// at 40 ms apart with a keyframe every `gop`.
    fn written(
        policy: Placement,
        spaces: &[Space],
        records: &[Pending],
        count: usize,
        gop: usize,
    ) -> Vec<(i64, bool, Vec<Message>)> {
        let carriers: Vec<Carrier> = (0..count)
            .map(|index| Carrier {
                pts_ms: index as i64 * 40,
                keyframe: index % gop == 0,
            })
            .collect();
        plan(policy, spaces, records, &carriers)
            .into_iter()
            .zip(&carriers)
            .map(|(messages, carrier)| (carrier.pts_ms, carrier.keyframe, messages))
            .collect()
    }

    /// A reader over a millisecond time base, which is what the
    /// carriers above are counted in.
    fn reader() -> Reader {
        Reader::new(Framing::AnnexB(Codec::H264), 1, 1000)
    }

    #[test]
    fn every_record_comes_back_with_the_span_it_was_written_with() {
        let dims = 16;
        let records: Vec<Pending> = (0..4)
            .map(|index| {
                Pending::layered(
                    1,
                    index as u16,
                    index as i64 * 1000,
                    index as i64 * 1000 + 500,
                    &Planes::quantize(&values(index, dims), 2).expect("quantized"),
                )
            })
            .collect();
        let stream = written(
            Placement::Keyframe,
            &[space(1, dims as u32)],
            &records,
            150,
            25,
        );

        let mut reader = reader();
        let mut rows = Vec::new();
        let mut spaces = Vec::new();
        for (pts_ms, keyframe, messages) in &stream {
            let _ = keyframe;
            spaces.extend(reader.packet(*pts_ms, &packet(messages.clone(), *keyframe)));
            for record in reader.finished() {
                rows.push(reader.record_row(&record));
            }
        }
        for record in reader.drain() {
            rows.push(reader.record_row(&record));
        }

        assert_eq!(spaces.len(), 1, "one space, declared on every keyframe");
        assert_eq!(rows.len(), records.len());
        assert_eq!(reader.dropped(), 0);
        for (index, row) in rows.iter().enumerate() {
            let row = Json::parse(row).expect("a row");
            assert_eq!(
                row.get("index").and_then(Json::as_i64),
                Some(index as i64 + 1)
            );
            assert_eq!(row.get("space").and_then(Json::as_i64), Some(1));
            assert_eq!(
                row.get("start_t").and_then(Json::as_f64),
                Some(index as f64)
            );
            assert_eq!(
                row.get("end_t").and_then(Json::as_f64),
                Some(index as f64 + 0.5)
            );
            let got: Vec<f32> = row
                .get("vector")
                .and_then(Json::as_array)
                .expect("a vector")
                .iter()
                .map(|value| value.as_f64().expect("a number") as f32)
                .collect();
            let want = Planes::quantize(&values(index, dims), 2)
                .expect("quantized")
                .reconstruct()
                .expect("a reconstruction");
            assert_eq!(got, want, "record {index} is not the vector that went in");
            assert_eq!(
                row.get("planes")
                    .and_then(Json::as_array)
                    .map(<[Json]>::len),
                Some(8)
            );
        }
    }

    #[test]
    fn a_record_is_answered_as_soon_as_nothing_more_can_reach_it() {
        let dims = 8;
        let records: Vec<Pending> = (0..3)
            .map(|index| {
                Pending::layered(
                    1,
                    index as u16,
                    index as i64 * 500,
                    index as i64 * 500 + 400,
                    &Planes::quantize(&values(index, dims), 0).expect("quantized"),
                )
            })
            .collect();
        let stream = written(
            Placement::Keyframe,
            &[space(1, dims as u32)],
            &records,
            120,
            25,
        );

        let mut reader = reader();
        let mut answered_at: Vec<i64> = Vec::new();
        for (pts_ms, keyframe, messages) in &stream {
            reader.packet(*pts_ms, &packet(messages.clone(), *keyframe));
            for _ in reader.finished() {
                answered_at.push(*pts_ms);
            }
        }
        assert_eq!(answered_at.len(), 3, "a record waited for the drain");
        assert!(
            reader.drain().is_empty(),
            "a record was answered twice over"
        );
        // Each one came back on the carrier it rode, not at the end.
        assert!(
            answered_at.iter().any(|at| *at < 4000),
            "nothing was answered while the stream ran: {answered_at:?}"
        );
    }

    #[test]
    fn a_record_in_slices_comes_back_whole() {
        // A budget below one message cuts a record into slices, which
        // arrive over many packets and merge into one row.
        let dims = 64;
        let record = Pending::whole(
            1,
            0,
            0,
            500,
            Planes::quantize(&values(0, dims), 2)
                .expect("quantized")
                .encode(),
        );
        let stream = written(
            Placement::Spread { budget_bytes: 32 },
            &[space(1, dims as u32)],
            &[record],
            160,
            25,
        );
        let slices = stream
            .iter()
            .flat_map(|(_, _, messages)| messages)
            .filter(|m| matches!(m, Message::Fragment(_)))
            .count();
        assert!(slices > 1, "the fixture was never cut");

        let mut reader = reader();
        let mut rows = Vec::new();
        for (pts_ms, keyframe, messages) in &stream {
            reader.packet(*pts_ms, &packet(messages.clone(), *keyframe));
            for record in reader.finished() {
                rows.push(reader.record_row(&record));
            }
        }
        for record in reader.drain() {
            rows.push(reader.record_row(&record));
        }
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(reader.dropped(), 0);
        let row = Json::parse(&rows[0]).expect("a row");
        assert_eq!(
            row.get("planes")
                .and_then(Json::as_array)
                .map(<[Json]>::len),
            Some(8)
        );
    }

    #[test]
    fn a_space_is_answered_once_however_often_it_is_declared() {
        let stream = written(
            Placement::Keyframe,
            &[space(1, 8), space(2, 4)],
            &[Pending::layered(
                1,
                0,
                0,
                100,
                &Planes::quantize(&values(0, 8), 0).expect("quantized"),
            )],
            120,
            10,
        );
        let mut reader = reader();
        let mut fresh = Vec::new();
        for (pts_ms, keyframe, messages) in &stream {
            fresh.extend(reader.packet(*pts_ms, &packet(messages.clone(), *keyframe)));
        }
        assert_eq!(fresh.len(), 2, "the declarations repeat on every keyframe");
        assert_eq!(reader.spaces().count(), 2);
        let row = Json::parse(&space_row(&fresh[0])).expect("a row");
        assert_eq!(row.get("space").and_then(Json::as_i64), Some(1));
        assert_eq!(row.get("dims").and_then(Json::as_i64), Some(8));
        assert_eq!(row.get("encoding").and_then(Json::as_str), Some("i8"));
        assert_eq!(
            row.get("name").and_then(Json::as_str),
            Some("the read tests"),
            "the producer is the label where there is one"
        );
    }

    #[test]
    fn a_space_with_nothing_to_call_it_is_named_for_its_id() {
        let bare = Space::new(7, 4, Encoding::F32);
        assert_eq!(name_of(&bare), "space 7");
        let mut modelled = bare.clone();
        modelled.model = "hf:a/b@c/d.safetensors".into();
        assert_eq!(name_of(&modelled), "hf:a/b@c/d.safetensors");
        let mut produced = modelled.clone();
        produced.producer = "a captioner".into();
        assert_eq!(name_of(&produced), "a captioner");
    }

    #[test]
    fn a_stream_with_nothing_of_ours_in_it_answers_nothing() {
        let mut reader = reader();
        for index in 0..30i64 {
            let fresh = reader.packet(index * 40, &packet(Vec::new(), index % 10 == 0));
            assert!(fresh.is_empty());
            assert!(reader.finished().is_empty());
        }
        assert!(reader.drain().is_empty());
        assert_eq!(reader.dropped(), 0, "an empty stream is not a broken one");
        assert_eq!(reader.packets(), 30);
        assert_eq!(reader.footprint(), 0);
    }

    #[test]
    fn bytes_that_are_not_the_framing_they_were_said_to_be_answer_nothing() {
        let mut seed = 0x2f6b_9a14_c3d5_e708u64;
        let mut reader = Reader::new(
            Framing::LengthPrefixed {
                codec: Codec::H264,
                length_size: 4,
            },
            1,
            1000,
        );
        for index in 0..400i64 {
            let mut bytes = Vec::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            for _ in 0..(seed >> 40) % 96 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push((seed >> 33) as u8);
            }
            let _ = reader.packet(index, &bytes);
            let _ = reader.finished();
        }
        let _ = reader.drain();
    }

    #[test]
    fn a_span_is_the_containers_clock_and_not_a_frame_rate() {
        // The same carriers in a time base of 1/90000, which is what a
        // transport stream counts in: the rows are the same seconds.
        let dims = 8;
        let record = Pending::layered(
            1,
            0,
            1000,
            2000,
            &Planes::quantize(&values(0, dims), 0).expect("quantized"),
        );
        let stream = written(
            Placement::Keyframe,
            &[space(1, dims as u32)],
            &[record],
            100,
            25,
        );
        let mut reader = Reader::new(Framing::AnnexB(Codec::H264), 1, 90_000);
        let mut rows = Vec::new();
        for (pts_ms, keyframe, messages) in &stream {
            // The same instants, counted in 90 kHz ticks.
            let pts = pts_ms * 90;
            reader.packet(pts, &packet(messages.clone(), *keyframe));
            for record in reader.finished() {
                rows.push(reader.record_row(&record));
            }
        }
        assert_eq!(rows.len(), 1);
        let row = Json::parse(&rows[0]).expect("a row");
        assert_eq!(row.get("start_t").and_then(Json::as_f64), Some(1.0));
        assert_eq!(row.get("end_t").and_then(Json::as_f64), Some(2.0));
    }
}
