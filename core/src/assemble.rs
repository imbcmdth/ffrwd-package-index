//! What a reader feeds messages to as they come out of a stream.
//!
//! Three things have to happen between a message and a record: planes
//! of one record that rode different frames are merged, slices are
//! reassembled, and vectors whose SPACE has not arrived wait for it.
//! All three are unbounded if written naively, and the bytes come from
//! a file anybody can write, so every one of them has a ceiling here
//! and passes what it drops on to [`Assembler::dropped`] rather than
//! growing.
//!
//! Record ids wrap at 65536 (section 4), so an id alone does not name a
//! record. The assembler expands each id against the last it saw for
//! that space, with the half-range window section 4 gives a writer: an
//! id may be reused once 32768 newer records have gone by.

use std::collections::{BTreeMap, VecDeque};

use crate::fragment::Reassembly;
use crate::message::{expand_record_id, Encoding, Message, Space, Unit, VectorBody, VectorRecord};
use crate::quant::Planes;
use crate::{Error, Result};

/// How much an assembler will hold before it starts dropping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Records held at once, whole or waiting for planes.
    pub max_records: usize,
    /// Records being reassembled from slices at once.
    pub max_reassemblies: usize,
    /// How long a record waits for a SPACE that has not arrived, in
    /// milliseconds of carrier time.
    pub orphan_wait_ms: i64,
}

impl Default for Limits {
    fn default() -> Self {
        // A file reader wants everything; a live reader wants a bound.
        // These are the live numbers, which a file reader raises.
        Self {
            max_records: 4096,
            max_reassemblies: 64,
            orphan_wait_ms: 30_000,
        }
    }
}

/// One record, as far as the messages so far describe it.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// The space as it was declared.
    pub space: Space,
    pub record_id: u16,
    /// The presentation time of the first carrier this record rode on.
    pub carrier_ms: i64,
    /// The span, in milliseconds of presentation time.
    pub start_ms: i64,
    pub end_ms: i64,
    /// The body, merged from every message of this record.
    pub body: VectorBody,
    /// Which planes arrived, for an I8 record; `None` for the float
    /// encodings, which arrive whole or not at all.
    pub planes: Option<u8>,
}

impl Record {
    /// The vector, reconstructed from whatever arrived.
    pub fn values(&self) -> Result<Vec<f32>> {
        self.body.values()
    }
}

/// A record while it is still being put together.
#[derive(Clone, Debug)]
struct Held {
    space_id: u8,
    record_id: u16,
    carrier_ms: i64,
    start_ms: i64,
    end_ms: i64,
    /// The bodies of every VECTOR message of this record, in the order
    /// they arrived. They stay raw until the SPACE says how to read
    /// them, which may be never.
    bodies: Vec<Vec<u8>>,
}

/// One record's slices, and the time of the carrier that opened them.
#[derive(Clone, Debug)]
struct Slices {
    bytes: Reassembly,
    /// The presentation time of the carrier of the slice at offset 0.
    anchor_ms: Option<i64>,
}

/// Messages in, records out.
#[derive(Debug)]
pub struct Assembler {
    limits: Limits,
    spaces: BTreeMap<u8, Space>,
    /// The last expanded sequence seen per space, for the wraparound.
    sequence: BTreeMap<u8, u64>,
    order: VecDeque<(u8, u64)>,
    held: BTreeMap<(u8, u64), Held>,
    slices: BTreeMap<(u8, u64), Slices>,
    slice_order: VecDeque<(u8, u64)>,
    now_ms: i64,
    dropped: usize,
}

impl Assembler {
    /// An assembler with the given ceilings.
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            spaces: BTreeMap::new(),
            sequence: BTreeMap::new(),
            order: VecDeque::new(),
            held: BTreeMap::new(),
            slices: BTreeMap::new(),
            slice_order: VecDeque::new(),
            now_ms: i64::MIN,
            dropped: 0,
        }
    }

    /// Every message of a unit, from a carrier at `carrier_ms`.
    ///
    /// A message the assembler cannot use is counted and skipped: one
    /// bad record in a file does not stop the read.
    pub fn push_unit(&mut self, carrier_ms: i64, unit: &Unit) {
        for message in &unit.messages {
            if self.push(carrier_ms, message).is_err() {
                self.dropped += 1;
            }
        }
    }

    /// One message, from a carrier at `carrier_ms`.
    pub fn push(&mut self, carrier_ms: i64, message: &Message) -> Result<()> {
        self.now_ms = self.now_ms.max(carrier_ms);
        match message {
            Message::Space(space) => {
                self.spaces.insert(space.space_id, space.clone());
                Ok(())
            }
            Message::Vector(record) => self.push_vector(carrier_ms, record),
            Message::Fragment(fragment) => {
                let key = (
                    fragment.space_id,
                    self.expand(fragment.space_id, fragment.record_id),
                );
                if !self.slices.contains_key(&key) {
                    self.evict_slices();
                    self.slices.insert(
                        key,
                        Slices {
                            bytes: Reassembly::new(fragment.total)?,
                            anchor_ms: None,
                        },
                    );
                    self.slice_order.push_back(key);
                }
                let slices = self.slices.get_mut(&key).ok_or(Error::FragmentBounds)?;
                slices.bytes.push(fragment)?;
                // Section 6: the offsets belong to the carrier of the
                // slice at offset 0, not to whichever slice arrived
                // last, so that carrier's time is kept here.
                if fragment.offset == 0 {
                    slices.anchor_ms = Some(carrier_ms);
                }
                if slices.bytes.complete() {
                    let held = self.slices.remove(&key).ok_or(Error::FragmentBounds)?;
                    self.slice_order.retain(|other| *other != key);
                    let anchor = held.anchor_ms.unwrap_or(carrier_ms);
                    let bytes = held.bytes.take().ok_or(Error::FragmentBounds)?;
                    let record = VectorRecord::decode(&bytes)?;
                    return self.push_vector(anchor, &record);
                }
                Ok(())
            }
            Message::Unknown { .. } => Ok(()),
        }
    }

    /// The spaces declared so far.
    pub fn spaces(&self) -> impl Iterator<Item = &Space> {
        self.spaces.values()
    }

    /// One space, if it has been declared.
    pub fn space(&self, space_id: u8) -> Option<&Space> {
        self.spaces.get(&space_id)
    }

    /// The records in hand, in the order their first message arrived.
    ///
    /// A record whose space never arrived, or whose bodies do not read
    /// in that space's encoding, is left out.
    pub fn records(&self) -> Vec<Record> {
        self.order
            .iter()
            .filter_map(|key| self.held.get(key))
            .filter_map(|held| self.resolve(held).ok())
            .collect()
    }

    /// The records of one space, in the order their first message
    /// arrived, which is what a reader that has just seen that space
    /// declared wants: the vectors that were waiting for it.
    pub fn records_of(&self, space_id: u8) -> Vec<Record> {
        self.order
            .iter()
            .filter(|(space, _)| *space == space_id)
            .filter_map(|key| self.held.get(key))
            .filter_map(|held| self.resolve(held).ok())
            .collect()
    }

    /// One record, as far as the messages so far describe it.
    ///
    /// A live reader pushes a message and wants that record and no
    /// other: rebuilding every record it holds, on every frame, to find
    /// the one that just changed is the difference between a watcher
    /// that keeps up with a stream and one that does not. The id is
    /// expanded against the last seen for its space, exactly as
    /// [`Assembler::push`] expanded it on the way in.
    pub fn record(&self, space_id: u8, record_id: u16) -> Option<Record> {
        let sequence = expand_record_id(self.sequence.get(&space_id).copied(), record_id);
        let held = self.held.get(&(space_id, sequence))?;
        self.resolve(held).ok()
    }

    /// How many records are being held, resolvable or not.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Whether anything is being held.
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// How many messages and records were dropped: malformed, orphaned
    /// past the wait, or pushed out by a ceiling.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// The bytes being held for records that are not finished.
    pub fn footprint(&self) -> usize {
        let records: usize = self
            .held
            .values()
            .map(|held| held.bodies.iter().map(Vec::len).sum::<usize>())
            .sum();
        let slices: usize = self
            .slices
            .values()
            .map(|slices| slices.bytes.footprint())
            .sum();
        records + slices
    }

    fn push_vector(&mut self, carrier_ms: i64, record: &VectorRecord) -> Result<()> {
        let sequence = self.expand(record.space_id, record.record_id);
        let key = (record.space_id, sequence);
        match self.held.get_mut(&key) {
            Some(held) => {
                // Section 4: a record's span is the one its first
                // message gives, and a reader does not require the
                // others to agree with it. They cannot always. Each
                // names the span from its own carrier, and between a
                // writer and a reader those carriers' times are
                // rescaled by remuxing and rounded to the millisecond
                // at both ends; a stream woven against a frame rate and
                // read back off a container's own clock can differ by
                // more. None of that is a reason to throw away a plane,
                // so a later message adds its body and nothing else.
                held.bodies.push(record.body.clone());
            }
            None => {
                self.evict_records();
                self.held.insert(
                    key,
                    Held {
                        space_id: record.space_id,
                        record_id: record.record_id,
                        carrier_ms,
                        start_ms: carrier_ms + i64::from(record.start_off),
                        end_ms: carrier_ms + i64::from(record.end_off),
                        bodies: vec![record.body.clone()],
                    },
                );
                self.order.push_back(key);
            }
        }
        Ok(())
    }

    /// One held record, read in its space's encoding.
    fn resolve(&self, held: &Held) -> Result<Record> {
        let space = self.spaces.get(&held.space_id).ok_or(Error::Mismatch)?;
        let mut bodies = held.bodies.iter();
        let first = bodies.next().ok_or(Error::Mismatch)?;
        let mut body = VectorBody::decode(space, first)?;
        for next in bodies {
            let next = VectorBody::decode(space, next)?;
            // Only the layered encoding arrives in pieces. A float
            // record that arrives twice is the same vector twice.
            if let (VectorBody::I8(planes), VectorBody::I8(more)) = (&mut body, next) {
                planes.merge(&more)?;
            }
        }
        let planes = match (&body, space.encoding) {
            (VectorBody::I8(planes), Encoding::I8) => Some(planes.present()),
            _ => None,
        };
        Ok(Record {
            space: space.clone(),
            record_id: held.record_id,
            carrier_ms: held.carrier_ms,
            start_ms: held.start_ms,
            end_ms: held.end_ms,
            body,
            planes,
        })
    }

    /// A 16-bit record id as a sequence that does not wrap, against the
    /// last id seen for that space, and remembers it.
    fn expand(&mut self, space_id: u8, record_id: u16) -> u64 {
        let sequence = expand_record_id(self.sequence.get(&space_id).copied(), record_id);
        let entry = self.sequence.entry(space_id).or_insert(sequence);
        if sequence > *entry {
            *entry = sequence;
        }
        sequence
    }

    /// Makes room for one more record: the oldest orphan past its wait
    /// first, then simply the oldest.
    fn evict_records(&mut self) {
        while let Some(key) = self.order.front().copied() {
            let Some(held) = self.held.get(&key) else {
                self.order.pop_front();
                continue;
            };
            let orphaned = !self.spaces.contains_key(&held.space_id)
                && self.now_ms.saturating_sub(held.carrier_ms) > self.limits.orphan_wait_ms;
            if !orphaned {
                break;
            }
            self.order.pop_front();
            self.held.remove(&key);
            self.dropped += 1;
        }
        while self.held.len() >= self.limits.max_records {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            if self.held.remove(&key).is_some() {
                self.dropped += 1;
            }
        }
    }

    /// Makes room for one more reassembly.
    fn evict_slices(&mut self) {
        while self.slices.len() >= self.limits.max_reassemblies {
            let Some(key) = self.slice_order.pop_front() else {
                break;
            };
            if self.slices.remove(&key).is_some() {
                self.dropped += 1;
            }
        }
    }
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

/// Planes merged out of several bodies of one record, for a caller that
/// has the bodies already.
pub fn merge_bodies(space: &Space, bodies: &[Vec<u8>]) -> Result<Planes> {
    let mut merged: Option<Planes> = None;
    for body in bodies {
        let planes = Planes::decode(space.dims as usize, body)?;
        match merged.as_mut() {
            Some(held) => held.merge(&planes)?,
            None => merged = Some(planes),
        }
    }
    merged.ok_or(Error::Planes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Fragment, Modality};

    fn i8_space() -> Space {
        let mut space = Space::new(1, 16, Encoding::I8);
        space.modality = Modality::Picture;
        space.model = "test:model".into();
        space
    }

    fn vector(dims: usize) -> Vec<f32> {
        (0..dims).map(|i| (i as f32 * 0.3).sin()).collect()
    }

    fn record(
        space_id: u8,
        record_id: u16,
        planes: &Planes,
        mask: u8,
        start: i32,
        end: i32,
    ) -> VectorRecord {
        VectorRecord {
            space_id,
            record_id,
            start_off: start,
            end_off: end,
            body: planes.subset(mask).encode(),
        }
    }

    #[test]
    fn planes_from_several_carriers_become_one_record() {
        let space = i8_space();
        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let mut assembler = Assembler::default();
        assembler
            .push(1000, &Message::Space(space.clone()))
            .expect("a space");
        // Plane 0 on one carrier, planes 1 and 2 on a later one, with
        // each message's offsets taken from its own carrier.
        assembler
            .push(
                1000,
                &Message::Vector(record(1, 7, &planes, 0b0000_0001, -900, -100)),
            )
            .expect("plane 0");
        assembler
            .push(
                2000,
                &Message::Vector(record(1, 7, &planes, 0b0000_0110, -1900, -1100)),
            )
            .expect("planes 1 and 2");
        let records = assembler.records();
        assert_eq!(records.len(), 1, "one record, not three");
        assert_eq!(records[0].planes, Some(0b0000_0111));
        assert_eq!((records[0].start_ms, records[0].end_ms), (100, 900));
        assert_eq!(records[0].carrier_ms, 1000);
        let read = records[0].values().expect("a reconstruction");
        assert_eq!(read.len(), 16);
    }

    #[test]
    fn the_escapes_arrive_with_plane_zero_and_stay() {
        // Section 5 puts a record's escapes in the message that carries
        // plane 0, whichever carrier that turns out to be.
        let planes = Planes::quantize(&vector(16), 2).expect("quantized");
        assert_eq!(planes.escapes().len(), 2);
        let mut assembler = Assembler::default();
        assembler
            .push(1000, &Message::Space(i8_space()))
            .expect("a space");
        // The magnitude planes first, with no escapes in them.
        assembler
            .push(
                1000,
                &Message::Vector(record(1, 4, &planes, 0b1111_1110, -900, -100)),
            )
            .expect("the magnitude planes");
        let held = assembler.records();
        assert_eq!(held.len(), 1, "the record is held, planes and all");
        assert!(
            held[0].values().is_err(),
            "but without plane 0 there is nothing to read yet"
        );
        // Then plane 0, which brings them.
        assembler
            .push(
                2000,
                &Message::Vector(record(1, 4, &planes, 0b0000_0001, -1900, -1100)),
            )
            .expect("plane 0 and the escapes");

        let records = assembler.records();
        assert_eq!(records.len(), 1);
        let VectorBody::I8(merged) = &records[0].body else {
            panic!("the layered encoding came back as something else");
        };
        assert_eq!(merged, &planes, "the record is the one that was sent");
        assert_eq!(merged.escapes(), planes.escapes());
        let read = records[0].values().expect("a reconstruction");
        for (index, value) in planes.escapes() {
            assert_eq!(
                read[*index as usize],
                crate::quant::f16_to_f32(*value),
                "an escaped component is the escape's own value"
            );
        }
    }

    #[test]
    fn a_vector_waits_for_its_space_and_then_reads() {
        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let mut assembler = Assembler::default();
        assembler
            .push(0, &Message::Vector(record(1, 1, &planes, 0xff, 0, 500)))
            .expect("a vector");
        assert!(assembler.records().is_empty(), "no space, no record");
        assert_eq!(assembler.len(), 1, "but it is held");
        assembler
            .push(0, &Message::Space(i8_space()))
            .expect("a space");
        assert_eq!(assembler.records().len(), 1, "the space arrived");
    }

    #[test]
    fn a_space_that_never_arrives_is_dropped_after_the_wait() {
        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let limits = Limits {
            orphan_wait_ms: 1000,
            ..Limits::default()
        };
        let mut assembler = Assembler::new(limits);
        assembler
            .push(0, &Message::Vector(record(1, 1, &planes, 0xff, 0, 100)))
            .expect("a vector");
        // A later carrier, past the wait, and one more record to make
        // the assembler look at what it is holding.
        assembler
            .push(5000, &Message::Vector(record(1, 2, &planes, 0xff, 0, 100)))
            .expect("a vector");
        assert_eq!(assembler.len(), 1, "the first record aged out");
        assert_eq!(assembler.dropped(), 1);
    }

    #[test]
    fn the_ceiling_on_records_holds() {
        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let limits = Limits {
            max_records: 8,
            ..Limits::default()
        };
        let mut assembler = Assembler::new(limits);
        assembler
            .push(0, &Message::Space(i8_space()))
            .expect("a space");
        for id in 0..100u16 {
            assembler
                .push(
                    i64::from(id),
                    &Message::Vector(record(1, id, &planes, 0xff, 0, 10)),
                )
                .expect("a vector");
        }
        assert!(assembler.len() <= 8, "held {}", assembler.len());
        assert_eq!(assembler.dropped(), 92);
        assert!(assembler.footprint() < 8 * 1024);
    }

    #[test]
    fn record_ids_that_wrap_are_different_records() {
        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let mut assembler = Assembler::new(Limits {
            max_records: 1024,
            ..Limits::default()
        });
        assembler
            .push(0, &Message::Space(i8_space()))
            .expect("a space");
        // 65534, 65535, 0, 1: the last two are new records, not the
        // ones from the start of the stream.
        for (index, id) in [65534u16, 65535, 0, 1].iter().enumerate() {
            assembler
                .push(
                    index as i64 * 100,
                    &Message::Vector(record(1, *id, &planes, 0xff, 0, 10)),
                )
                .expect("a vector");
        }
        assert_eq!(assembler.len(), 4, "four records across the wrap");
        let records = assembler.records();
        assert_eq!(
            records.iter().map(|r| r.record_id).collect::<Vec<_>>(),
            vec![65534, 65535, 0, 1]
        );
    }

    #[test]
    fn the_same_id_again_within_the_window_is_the_same_record() {
        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let mut assembler = Assembler::default();
        assembler
            .push(0, &Message::Space(i8_space()))
            .expect("a space");
        assembler
            .push(
                0,
                &Message::Vector(record(1, 900, &planes, 0b0000_0001, 0, 10)),
            )
            .expect("plane 0");
        assembler
            .push(
                0,
                &Message::Vector(record(1, 900, &planes, 0b0000_0010, 0, 10)),
            )
            .expect("plane 1");
        assert_eq!(assembler.len(), 1);
        assert_eq!(assembler.records()[0].planes, Some(0b0000_0011));
    }

    #[test]
    fn slices_become_a_record() {
        let space = i8_space();
        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let whole = record(1, 5, &planes, 0xff, -2000, -1000);
        let slices = crate::fragment::fragment_record(&whole, 20).expect("slices");
        assert!(slices.len() > 1);
        let mut assembler = Assembler::default();
        assembler.push(0, &Message::Space(space)).expect("a space");
        for (index, slice) in slices.iter().enumerate() {
            assembler
                .push(3000 + index as i64, &Message::Fragment(slice.clone()))
                .expect("a slice");
        }
        let records = assembler.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].planes, Some(0xff));
        // Section 6: the span is relative to the carrier of the slice
        // at offset 0, which is carrier 3000, not the one the last
        // slice rode in on.
        assert_eq!((records[0].start_ms, records[0].end_ms), (1000, 2000));
        assert_eq!(records[0].carrier_ms, 3000);
    }

    #[test]
    fn a_missing_slice_leaves_no_record_and_is_bounded() {
        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let whole = record(1, 5, &planes, 0xff, 0, 10);
        let slices = crate::fragment::fragment_record(&whole, 20).expect("slices");
        let mut assembler = Assembler::new(Limits {
            max_reassemblies: 2,
            ..Limits::default()
        });
        assembler
            .push(0, &Message::Space(i8_space()))
            .expect("a space");
        for id in 0..10u16 {
            let mut first = slices[0].clone();
            first.record_id = id;
            assembler
                .push(0, &Message::Fragment(first))
                .expect("a slice");
        }
        assert!(assembler.records().is_empty());
        assert!(assembler.footprint() < 4096, "{}", assembler.footprint());
        assert!(assembler.dropped() >= 8);
    }

    #[test]
    fn a_slice_of_an_impossible_size_is_refused() {
        let mut assembler = Assembler::default();
        let fragment = Fragment {
            space_id: 1,
            record_id: 1,
            total: crate::fragment::MAX_VALUE_BYTES as u32 + 1,
            offset: 0,
            bytes: vec![1, 2, 3],
        };
        assert!(assembler.push(0, &Message::Fragment(fragment)).is_err());
        assert_eq!(assembler.footprint(), 0);
    }

    #[test]
    fn messages_of_one_record_that_disagree_about_the_span_still_merge() {
        // Section 4: the span is the first message's, and the later
        // ones are not held to it. A writer that computed its offsets
        // against a frame rate and a reader on a container's own clock
        // will disagree by more than rounding, and the planes are still
        // the same record's planes.
        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let mut assembler = Assembler::default();
        assembler
            .push(0, &Message::Space(i8_space()))
            .expect("a space");
        assembler
            .push(0, &Message::Vector(record(1, 3, &planes, 0b0001, 0, 100)))
            .expect("plane 0");
        assembler
            .push(40, &Message::Vector(record(1, 3, &planes, 0b1110, 0, 200)))
            .expect("the planes after it, from a carrier that says otherwise");
        assert_eq!(assembler.dropped(), 0);
        let held = assembler.records();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].planes, Some(0b1111), "every plane made it in");
        assert_eq!(held[0].start_ms, 0, "the first message's span");
        assert_eq!(held[0].end_ms, 100);
        assert_eq!(held[0].carrier_ms, 0, "and the first message's carrier");
    }

    #[test]
    fn a_space_this_version_cannot_read_is_still_reported() {
        // Section 3: a reader that does not know an encoding keeps the
        // declaration, so it can still name the space's models, and
        // ignores that space's vectors without calling them an error.
        let mut space = i8_space();
        space.encoding = Encoding::Other(9);
        assert_eq!(
            Space::decode(&space.encode()).expect("the declaration survives"),
            space
        );

        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let mut assembler = Assembler::default();
        assembler.push(0, &Message::Space(space)).expect("a space");
        assembler
            .push(0, &Message::Vector(record(1, 1, &planes, 0xff, 0, 10)))
            .expect("a vector");
        let held = assembler.space(1).expect("the space is still there");
        assert_eq!(held.encoding, Encoding::Other(9));
        assert_eq!(held.model, "test:model");
        assert!(assembler.records().is_empty(), "its vectors are ignored");
        assert_eq!(assembler.dropped(), 0, "and are not an error");
    }

    #[test]
    fn a_unit_of_mixed_messages_counts_what_it_cannot_use() {
        let planes = Planes::quantize(&vector(16), 0).expect("quantized");
        let mut assembler = Assembler::default();
        let unit = Unit::new(vec![
            Message::Space(i8_space()),
            Message::Vector(record(1, 1, &planes, 0xff, 0, 10)),
            // A slice that falls outside the value it slices, which is
            // section 9's rule and not section 4's: the framing itself
            // is wrong, and the message goes.
            Message::Fragment(crate::message::Fragment {
                space_id: 1,
                record_id: 2,
                total: 4,
                offset: 3,
                bytes: vec![1, 2, 3],
            }),
            Message::Unknown {
                kind: 0x90,
                value: vec![1, 2],
            },
        ]);
        assembler.push_unit(0, &unit);
        assert_eq!(assembler.records().len(), 1);
        assert_eq!(assembler.dropped(), 1, "the slice outside its value");
    }
}
