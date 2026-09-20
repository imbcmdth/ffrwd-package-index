//! Section 8: the optional copy of the same messages, as one blob a
//! search can read in one go.
//!
//! The index is derived, never authoritative: it can always be rebuilt
//! by reading the stream, so building it is a fold over the messages a
//! reader already saw, with each record's several messages merged into
//! the one entry that describes it.

use std::collections::BTreeMap;

use crate::message::{
    expand_record_id, Encoding, Message, Space, VectorBody, VectorRecord, TYPE_SPACE, TYPE_VECTOR,
};
use crate::wire::{put_svarint, put_varint, Reader};
use crate::{Error, Result, VERSION};

/// The four bytes an index opens with.
pub const MAGIC: [u8; 4] = *b"FFIX";

/// The MIME type of the Matroska attachment an index travels as.
pub const MATROSKA_MIME: &str = "application/x-ffrwd-index";

/// The file name of that attachment.
pub const MATROSKA_FILE_NAME: &str = "ffrwd-index.bin";

/// One entry: a message and the presentation time of the carrier it
/// came off.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The carrier's presentation time in milliseconds, on the
    /// container's own clock: the time a player shows that picture at,
    /// once the container's timestamps and, in an MP4, its edit list
    /// have been applied. It is signed because an edit list can put a
    /// carrier before the file's zero, and negative is what every
    /// reader of that file then reports for it.
    pub time_ms: i32,
    /// A SPACE or VECTOR message, exactly as it appears in a unit.
    pub message: Message,
}

/// The index of one file.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct FileIndex {
    pub version: u8,
    pub entries: Vec<Entry>,
}

impl FileIndex {
    /// An index from the messages of a stream, in time order.
    ///
    /// Two things are folded away, as section 8 asks. A SPACE message
    /// appears once, at the time its definition first applied, so a
    /// writer declaring it on every keyframe costs the index nothing; a
    /// writer that redefines an id, which section 3 tells it not to do,
    /// gets a second entry at the time the new definition started. And
    /// the several messages of one record become one entry, at the time
    /// of the first of them, with their planes merged.
    pub fn build(pairs: impl IntoIterator<Item = (i32, Message)>) -> Self {
        let mut entries: Vec<Entry> = Vec::new();
        let mut spaces: BTreeMap<u8, Space> = BTreeMap::new();
        let mut declared: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
        let mut sequence: BTreeMap<u8, u64> = BTreeMap::new();
        let mut records: BTreeMap<(u8, u64), usize> = BTreeMap::new();

        for (time_ms, message) in pairs {
            match &message {
                Message::Space(space) => {
                    let bytes = space.encode();
                    if declared.get(&space.space_id) == Some(&bytes) {
                        continue;
                    }
                    declared.insert(space.space_id, bytes);
                    spaces.insert(space.space_id, space.clone());
                    entries.push(Entry { time_ms, message });
                }
                Message::Vector(record) => {
                    let key = (
                        record.space_id,
                        expand_record_id(sequence.get(&record.space_id).copied(), record.record_id),
                    );
                    sequence.insert(record.space_id, key.1);
                    match records.get(&key) {
                        Some(at) => {
                            let held = &mut entries[*at];
                            if let Message::Vector(first) = &mut held.message {
                                merge_into(spaces.get(&record.space_id), first, record);
                            }
                        }
                        None => {
                            records.insert(key, entries.len());
                            entries.push(Entry { time_ms, message });
                        }
                    }
                }
                // Section 8 holds SPACE and VECTOR only. A slice is put
                // back together before it reaches here, and a message
                // this version does not know has no place in a file its
                // own reader built.
                _ => {}
            }
        }
        // Section 8: entries are in time order, and where two have the
        // same time a SPACE comes before a VECTOR, because a reader
        // needs the declaration before the vectors that use it. The
        // sort is stable, so records sharing a time keep the order
        // their first messages arrived in.
        entries.sort_by_key(|entry| (entry.time_ms, entry.message.kind()));
        Self {
            version: VERSION,
            entries,
        }
    }

    /// The index's bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.entries.len() * 48);
        out.extend_from_slice(&MAGIC);
        out.push(self.version);
        put_varint(&mut out, self.entries.len() as u32);
        for entry in &self.entries {
            put_svarint(&mut out, entry.time_ms);
            entry.message.encode_into(&mut out);
        }
        out
    }

    /// An index's bytes.
    ///
    /// Strict, unlike a unit: an index is written in one piece by a
    /// program that had the whole file, so a short one is damaged
    /// rather than merely old, and the stream it came from is still
    /// there to rebuild it from.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        if reader.array::<4>()? != MAGIC {
            return Err(Error::NotOurs);
        }
        let version = reader.u8()?;
        if version != VERSION {
            return Err(Error::Version(version));
        }
        let count = reader.varint()?;
        let mut entries = Vec::new();
        for _ in 0..count {
            let time_ms = reader.svarint()?;
            let kind = reader.u8()?;
            let length = reader.length()?;
            let value = reader.take(length)?;
            if kind != TYPE_SPACE && kind != TYPE_VECTOR {
                return Err(Error::Malformed(
                    "an index entry is not a SPACE or a VECTOR",
                ));
            }
            entries.push(Entry {
                time_ms,
                message: Message::decode(kind, value)?,
            });
        }
        Ok(Self { version, entries })
    }

    /// The spaces the index declares.
    pub fn spaces(&self) -> impl Iterator<Item = (i32, &Space)> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.message {
                Message::Space(space) => Some((entry.time_ms, space)),
                _ => None,
            })
    }

    /// The records the index holds.
    pub fn records(&self) -> impl Iterator<Item = (i32, &VectorRecord)> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.message {
                Message::Vector(record) => Some((entry.time_ms, record)),
                _ => None,
            })
    }
}

/// Folds a later message of one record into the entry already held.
///
/// The entry keeps the first message's time and offsets, which name the
/// same span the later one does; only the body grows, and only for the
/// layered encoding, which is the one that arrives in pieces.
fn merge_into(space: Option<&Space>, held: &mut VectorRecord, later: &VectorRecord) {
    let Some(space) = space else { return };
    if space.encoding != Encoding::I8 {
        return;
    }
    let (Ok(VectorBody::I8(mut planes)), Ok(VectorBody::I8(more))) = (
        VectorBody::decode(space, &held.body),
        VectorBody::decode(space, &later.body),
    ) else {
        return;
    };
    if planes.merge(&more).is_ok() {
        held.body = planes.encode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quant::Planes;

    fn space(id: u8) -> Space {
        let mut space = Space::new(id, 16, Encoding::I8);
        space.model = "test:model".into();
        space
    }

    fn values() -> Vec<f32> {
        (0..16).map(|i| (i as f32 * 0.4).cos()).collect()
    }

    fn planes() -> Planes {
        Planes::quantize(&values(), 0).expect("quantized")
    }

    fn vector(record_id: u16, mask: u8) -> Message {
        Message::Vector(VectorRecord {
            space_id: 1,
            record_id,
            start_off: -500,
            end_off: 0,
            body: planes().subset(mask).encode(),
        })
    }

    #[test]
    fn an_index_round_trips() {
        let index = FileIndex::build(vec![
            (0, Message::Space(space(1))),
            (1000, vector(0, 0xff)),
            (2000, vector(1, 0xff)),
        ]);
        let bytes = index.encode();
        assert_eq!(&bytes[..4], b"FFIX");
        assert_eq!(bytes[4], VERSION);
        assert_eq!(FileIndex::parse(&bytes).expect("an index"), index);
        assert_eq!(index.entries.len(), 3);
        assert_eq!(index.spaces().count(), 1);
        assert_eq!(index.records().count(), 2);
    }

    #[test]
    fn a_carrier_before_the_files_zero_keeps_its_negative_time() {
        // An MP4's edit list can put a sample before the time the file
        // starts at, and ffprobe prints a negative `pts_time` for it.
        // Section 8's time is signed so that an index can say the same.
        let index = FileIndex::build(vec![
            (-100, Message::Space(space(1))),
            (-100, vector(0, 0xff)),
            (0, vector(1, 0xff)),
            (2_000_000, vector(2, 0xff)),
        ]);
        let bytes = index.encode();
        let read = FileIndex::parse(&bytes).expect("an index");
        assert_eq!(read, index);
        let times: Vec<i32> = read.entries.iter().map(|entry| entry.time_ms).collect();
        assert_eq!(times, vec![-100, -100, 0, 2_000_000]);
        // The span a record names is still its carrier plus its own
        // offsets, and that arithmetic works either side of zero.
        let (time, record) = read.records().next().expect("a record");
        assert_eq!(i64::from(time) + i64::from(record.start_off), -600);
    }

    #[test]
    fn a_space_comes_before_a_vector_of_the_same_time() {
        // A reader needs the declaration before the vectors that use
        // it, so the order is the index's own property rather than a
        // thing every caller has to remember.
        let index = FileIndex::build(vec![
            (1000, vector(0, 0xff)),
            (1000, Message::Space(space(1))),
            (500, vector(1, 0xff)),
        ]);
        let kinds: Vec<(i32, u8)> = index
            .entries
            .iter()
            .map(|entry| (entry.time_ms, entry.message.kind()))
            .collect();
        assert_eq!(
            kinds,
            vec![(500, TYPE_VECTOR), (1000, TYPE_SPACE), (1000, TYPE_VECTOR)]
        );
    }

    #[test]
    fn a_repeated_space_appears_once_at_the_time_it_first_applied() {
        let index = FileIndex::build(vec![
            (0, Message::Space(space(1))),
            (10_000, Message::Space(space(1))),
            (20_000, Message::Space(space(1))),
        ]);
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].time_ms, 0);
    }

    #[test]
    fn a_redefined_space_appears_again_at_its_own_time() {
        let mut second = space(1);
        second.producer = "somebody else".into();
        let index = FileIndex::build(vec![
            (0, Message::Space(space(1))),
            (5_000, Message::Space(second.clone())),
            (10_000, Message::Space(second)),
        ]);
        assert_eq!(index.entries.len(), 2);
        assert_eq!(index.entries[1].time_ms, 5_000);
    }

    #[test]
    fn the_messages_of_one_record_become_one_entry() {
        let index = FileIndex::build(vec![
            (0, Message::Space(space(1))),
            (1000, vector(7, 0b0000_0001)),
            (2000, vector(7, 0b0000_0110)),
            (3000, vector(7, 0b1111_1000)),
        ]);
        assert_eq!(index.entries.len(), 2, "the space and one record");
        let (time, record) = index.records().next().expect("a record");
        assert_eq!(time, 1000, "the time of the first message");
        assert_eq!(record.start_off, -500, "the first message's offsets");
        let body = record
            .decode_body(&space(1))
            .expect("a body in the space's encoding");
        let VectorBody::I8(merged) = body else {
            panic!("the layered encoding came back as something else");
        };
        assert_eq!(merged.present(), 0xff, "every plane made it into the entry");
        assert_eq!(merged, planes());
    }

    #[test]
    fn the_escapes_of_a_record_survive_being_merged() {
        // Plane 0 and the escapes on one carrier, the rest on another:
        // the one entry the index keeps has both.
        let escaped = Planes::quantize(&values(), 2).expect("quantized");
        assert_eq!(escaped.escapes().len(), 2);
        let message = |mask: u8| {
            Message::Vector(VectorRecord {
                space_id: 1,
                record_id: 11,
                start_off: -500,
                end_off: 0,
                body: escaped.subset(mask).encode(),
            })
        };
        let index = FileIndex::build(vec![
            (0, Message::Space(space(1))),
            (1000, message(0b0000_0001)),
            (2000, message(0b1111_1110)),
        ]);
        assert_eq!(index.records().count(), 1);
        let (time, record) = index.records().next().expect("a record");
        assert_eq!(time, 1000);
        let body = record.decode_body(&space(1)).expect("a body");
        let VectorBody::I8(merged) = body else {
            panic!("the layered encoding came back as something else");
        };
        assert_eq!(merged, escaped);
        assert_eq!(merged.escapes(), escaped.escapes());
        // And the whole thing still writes and reads as bytes.
        let bytes = index.encode();
        assert_eq!(FileIndex::parse(&bytes).expect("an index"), index);
    }

    #[test]
    fn records_whose_ids_wrap_stay_apart() {
        let mut pairs = vec![(0i32, Message::Space(space(1)))];
        for (index, id) in [65534u16, 65535, 0, 1].iter().enumerate() {
            pairs.push((index as i32 * 100, vector(*id, 0xff)));
        }
        let index = FileIndex::build(pairs);
        assert_eq!(index.records().count(), 4);
    }

    #[test]
    fn slices_and_unknown_messages_do_not_go_in() {
        let index = FileIndex::build(vec![
            (0, Message::Space(space(1))),
            (
                0,
                Message::Fragment(crate::message::Fragment {
                    space_id: 1,
                    record_id: 1,
                    total: 8,
                    offset: 0,
                    bytes: vec![1, 2, 3],
                }),
            ),
            (
                0,
                Message::Unknown {
                    kind: 0x90,
                    value: vec![1],
                },
            ),
        ]);
        assert_eq!(index.entries.len(), 1);
    }

    #[test]
    fn a_damaged_index_is_refused() {
        let index = FileIndex::build(vec![(0, Message::Space(space(1))), (10, vector(0, 0xff))]);
        let bytes = index.encode();
        for cut in 0..bytes.len() {
            assert!(
                FileIndex::parse(&bytes[..cut]).is_err(),
                "a cut at {cut} was accepted"
            );
        }
        assert_eq!(FileIndex::parse(b"FFIY\x01\x00"), Err(Error::NotOurs));
        assert_eq!(FileIndex::parse(b"FFIX\x02\x00"), Err(Error::Version(2)));
        // A count that claims more entries than there are bytes.
        assert!(FileIndex::parse(b"FFIX\x01\x40").is_err());
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut seed = 0xdead_beef_1234_5678u64;
        for _ in 0..2000 {
            let mut bytes = b"FFIX".to_vec();
            bytes.push(VERSION);
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            for _ in 0..(seed >> 40) % 40 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push((seed >> 33) as u8);
            }
            let _ = FileIndex::parse(&bytes);
        }
    }
}
