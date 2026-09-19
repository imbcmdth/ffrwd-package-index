//! Units and messages: sections 2, 3, 4 and the body side of 5.
//!
//! A unit is the UUID, a version and a run of type-length-value
//! messages, and the whole point of that shape is that a reader can
//! skip what it does not know. So decoding here is deliberately
//! two-sided: the framing is strict, and what sits inside a frame is
//! either understood, kept as [`Message::Unknown`], or dropped without
//! taking the rest of the unit with it.

use crate::quant::Planes;
use crate::wire::{put_str, put_svarint, put_varint, varint_len, Reader};
use crate::{Error, Result, MAX_DIMS, RECORD_ID_WRAP, UUID, VERSION};

/// The message type of SPACE.
pub const TYPE_SPACE: u8 = 0x01;
/// The message type of VECTOR.
pub const TYPE_VECTOR: u8 = 0x02;
/// The message type of FRAGMENT.
pub const TYPE_FRAGMENT: u8 = 0x03;
/// Types from here up are private use and never assigned by the format.
pub const TYPE_PRIVATE_FIRST: u8 = 0x80;

/// One blob of this format: what sits in one SEI message or one
/// metadata OBU.
#[derive(Clone, Debug, PartialEq)]
pub struct Unit {
    /// The version byte the unit carried.
    pub version: u8,
    /// The messages that were understood, in the order they appeared.
    pub messages: Vec<Message>,
    /// How many framed messages were dropped as malformed.
    ///
    /// Section 9 says a bad length ends the message it is in. The
    /// framing around it is still good, so the rest of the unit is
    /// read, and this counts what was lost rather than hiding it.
    pub dropped: usize,
}

impl Unit {
    /// A unit of this version carrying `messages`.
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            version: VERSION,
            messages,
            dropped: 0,
        }
    }

    /// The unit's bytes, UUID first.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(17 + self.messages.len() * 32);
        out.extend_from_slice(&UUID);
        out.push(self.version);
        for message in &self.messages {
            message.encode_into(&mut out);
        }
        out
    }

    /// How many bytes [`Unit::encode`] will write.
    pub fn encoded_len(&self) -> usize {
        17 + self
            .messages
            .iter()
            .map(|message| message.encoded_len())
            .sum::<usize>()
    }

    /// The unit in `bytes`.
    ///
    /// [`Error::NotOurs`] means the payload belongs to somebody else
    /// and must be left exactly as it is; [`Error::Version`] means the
    /// unit is ours but from a later version, which a reader skips.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let uuid = reader.array::<16>().map_err(|_| Error::NotOurs)?;
        if uuid != UUID {
            return Err(Error::NotOurs);
        }
        let version = reader.u8()?;
        if version != VERSION {
            return Err(Error::Version(version));
        }
        let mut messages = Vec::new();
        let mut dropped = 0usize;
        while !reader.is_empty() {
            let Ok(kind) = reader.u8() else { break };
            let Ok(length) = reader.length() else { break };
            // A message whose length runs past the end of the unit ends
            // the unit; what came before it stands.
            let Ok(value) = reader.take(length) else {
                break;
            };
            match Message::decode(kind, value) {
                Ok(message) => messages.push(message),
                Err(_) => dropped += 1,
            }
        }
        Ok(Self {
            version,
            messages,
            dropped,
        })
    }

    /// Whether `bytes` opens with this format's UUID.
    pub fn is_ours(bytes: &[u8]) -> bool {
        bytes.len() >= 16 && bytes[..16] == UUID
    }
}

/// One message of a unit.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// An embedding space is declared.
    Space(Space),
    /// One record, whole or some of its planes.
    Vector(VectorRecord),
    /// A slice of a VECTOR message too large for one carrier.
    Fragment(Fragment),
    /// A type this version does not define, kept so a reader can pass
    /// it on unchanged.
    Unknown { kind: u8, value: Vec<u8> },
}

impl Message {
    /// The message's type byte.
    pub fn kind(&self) -> u8 {
        match self {
            Message::Space(_) => TYPE_SPACE,
            Message::Vector(_) => TYPE_VECTOR,
            Message::Fragment(_) => TYPE_FRAGMENT,
            Message::Unknown { kind, .. } => *kind,
        }
    }

    /// The message's value bytes, without the type and length.
    pub fn value(&self) -> Vec<u8> {
        match self {
            Message::Space(space) => space.encode(),
            Message::Vector(vector) => vector.encode(),
            Message::Fragment(fragment) => fragment.encode(),
            Message::Unknown { value, .. } => value.clone(),
        }
    }

    /// Appends type, length and value.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let value = self.value();
        out.push(self.kind());
        put_varint(out, value.len() as u32);
        out.extend_from_slice(&value);
    }

    /// The message as its own bytes, type first.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// How many bytes [`Message::encode_into`] will write.
    pub fn encoded_len(&self) -> usize {
        let value = self.value().len();
        1 + varint_len(value as u32) + value
    }

    /// One message's value, by type.
    pub fn decode(kind: u8, value: &[u8]) -> Result<Self> {
        match kind {
            TYPE_SPACE => Ok(Message::Space(Space::decode(value)?)),
            TYPE_VECTOR => Ok(Message::Vector(VectorRecord::decode(value)?)),
            TYPE_FRAGMENT => Ok(Message::Fragment(Fragment::decode(value)?)),
            _ => Ok(Message::Unknown {
                kind,
                value: value.to_vec(),
            }),
        }
    }
}

/// How a space's vectors are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    F32,
    F16,
    I8,
    /// A value section 3 does not define. Kept rather than refused so a
    /// reader can still report the space, and name its model, without
    /// pretending it can read the vectors.
    Other(u8),
}

impl Encoding {
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Encoding::F32,
            1 => Encoding::F16,
            2 => Encoding::I8,
            other => Encoding::Other(other),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Encoding::F32 => 0,
            Encoding::F16 => 1,
            Encoding::I8 => 2,
            Encoding::Other(other) => other,
        }
    }

    /// The name this crate's tool uses in its rows.
    pub fn name(self) -> &'static str {
        match self {
            Encoding::F32 => "f32",
            Encoding::F16 => "f16",
            Encoding::I8 => "i8",
            Encoding::Other(_) => "unknown",
        }
    }
}

/// What was embedded, from section 3's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modality {
    Unspecified,
    Picture,
    Sound,
    Speech,
    SoundText,
    SceneText,
    Description,
    Other(u8),
}

impl Modality {
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Modality::Unspecified,
            1 => Modality::Picture,
            2 => Modality::Sound,
            3 => Modality::Speech,
            4 => Modality::SoundText,
            5 => Modality::SceneText,
            6 => Modality::Description,
            other => Modality::Other(other),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Modality::Unspecified => 0,
            Modality::Picture => 1,
            Modality::Sound => 2,
            Modality::Speech => 3,
            Modality::SoundText => 4,
            Modality::SceneText => 5,
            Modality::Description => 6,
            Modality::Other(other) => other,
        }
    }

    /// The name this crate's tool uses in its rows.
    pub fn name(self) -> &'static str {
        match self {
            Modality::Unspecified => "unspecified",
            Modality::Picture => "picture",
            Modality::Sound => "sound",
            Modality::Speech => "speech",
            Modality::SoundText => "sound-text",
            Modality::SceneText => "scene-text",
            Modality::Description => "description",
            Modality::Other(_) => "other",
        }
    }
}

/// The flag bit that says a space's vectors are unit length.
pub const FLAG_UNIT_LENGTH: u8 = 0x01;

/// One embedding space: what the vectors are and what made them.
#[derive(Clone, Debug, PartialEq)]
pub struct Space {
    pub space_id: u8,
    pub dims: u32,
    pub encoding: Encoding,
    /// The raw flags byte. Bits other than [`FLAG_UNIT_LENGTH`] are
    /// reserved; they are carried through rather than refused, so a
    /// version 1 reader still reads a version 1 space a later writer
    /// added a flag to.
    pub flags: u8,
    pub modality: Modality,
    pub source: u8,
    pub model: String,
    pub model_hash: [u8; 16],
    pub query: String,
    pub query_hash: [u8; 16],
    pub producer: String,
}

impl Space {
    /// A space with the fields a writer must decide and the rest empty.
    pub fn new(space_id: u8, dims: u32, encoding: Encoding) -> Self {
        Self {
            space_id,
            dims,
            encoding,
            flags: 0,
            modality: Modality::Unspecified,
            source: 0,
            model: String::new(),
            model_hash: [0; 16],
            query: String::new(),
            query_hash: [0; 16],
            producer: String::new(),
        }
    }

    /// Whether the space says its vectors are unit length.
    pub fn unit_length(&self) -> bool {
        self.flags & FLAG_UNIT_LENGTH != 0
    }

    /// What embeds a query into this space: `query`, or `model` when
    /// `query` is empty, as section 3 says.
    pub fn query_model(&self) -> &str {
        if self.query.is_empty() {
            &self.model
        } else {
            &self.query
        }
    }

    /// How long a body of this space is, for the fixed encodings.
    pub fn body_len(&self) -> Option<usize> {
        match self.encoding {
            Encoding::F32 => Some(self.dims as usize * 4),
            Encoding::F16 => Some(self.dims as usize * 2),
            _ => None,
        }
    }

    /// The message value.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.model.len() + self.producer.len());
        out.push(self.space_id);
        put_varint(&mut out, self.dims);
        out.push(self.encoding.as_u8());
        out.push(self.flags);
        out.push(self.modality.as_u8());
        out.push(self.source);
        put_str(&mut out, &self.model);
        out.extend_from_slice(&self.model_hash);
        put_str(&mut out, &self.query);
        out.extend_from_slice(&self.query_hash);
        put_str(&mut out, &self.producer);
        out
    }

    /// A message value. Bytes after `producer` are a later version's
    /// and are ignored.
    pub fn decode(value: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(value);
        let space_id = reader.u8()?;
        let dims = reader.varint()?;
        if dims == 0 || dims > MAX_DIMS {
            return Err(Error::Dims(dims));
        }
        let encoding = Encoding::from_u8(reader.u8()?);
        let flags = reader.u8()?;
        let modality = Modality::from_u8(reader.u8()?);
        let source = reader.u8()?;
        let model = reader.str()?.to_string();
        let model_hash = reader.array::<16>()?;
        let query = reader.str()?.to_string();
        let query_hash = reader.array::<16>()?;
        let producer = reader.str()?.to_string();
        Ok(Self {
            space_id,
            dims,
            encoding,
            flags,
            modality,
            source,
            model,
            model_hash,
            query,
            query_hash,
            producer,
        })
    }
}

/// One record: a vector, or some of its planes, and the span it
/// describes as offsets from the frame it rides on.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorRecord {
    pub space_id: u8,
    pub record_id: u16,
    /// Milliseconds from the carrier's presentation time to the start
    /// of the span.
    pub start_off: i32,
    /// Milliseconds from the carrier's presentation time to the end.
    pub end_off: i32,
    /// The body, in the space's encoding. Raw because the space may
    /// not have arrived yet: [`VectorRecord::decode_body`] reads it
    /// once it has.
    pub body: Vec<u8>,
}

impl VectorRecord {
    /// The message value.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.body.len());
        out.push(self.space_id);
        put_varint(&mut out, u32::from(self.record_id));
        put_svarint(&mut out, self.start_off);
        put_svarint(&mut out, self.end_off);
        out.extend_from_slice(&self.body);
        out
    }

    /// A message value.
    pub fn decode(value: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(value);
        let space_id = reader.u8()?;
        let record_id = record_id(reader.varint()?)?;
        let start_off = reader.svarint()?;
        let end_off = reader.svarint()?;
        Ok(Self {
            space_id,
            record_id,
            start_off,
            end_off,
            body: reader.rest().to_vec(),
        })
    }

    /// The body, read in the space's encoding.
    pub fn decode_body(&self, space: &Space) -> Result<VectorBody> {
        VectorBody::decode(space, &self.body)
    }
}

/// A body, once the space that names its encoding is in hand.
#[derive(Clone, Debug, PartialEq)]
pub enum VectorBody {
    F32(Vec<f32>),
    /// Binary16 components, widened. Narrowing them back is exact, so
    /// this round trips.
    F16(Vec<f32>),
    /// The planes of section 5, whichever of them this message carried.
    I8(Planes),
}

impl VectorBody {
    /// The body bytes.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            VectorBody::F32(values) => values.iter().flat_map(|v| v.to_le_bytes()).collect(),
            VectorBody::F16(values) => values
                .iter()
                .flat_map(|v| crate::quant::f32_to_f16(*v).to_le_bytes())
                .collect(),
            VectorBody::I8(planes) => planes.encode(),
        }
    }

    /// A body, in `space`'s encoding.
    pub fn decode(space: &Space, bytes: &[u8]) -> Result<Self> {
        let dims = space.dims as usize;
        crate::quant::check_dims(dims)?;
        match space.encoding {
            Encoding::F32 => {
                let want = dims * 4;
                if bytes.len() != want {
                    return Err(Error::BodyLength {
                        want,
                        got: bytes.len(),
                    });
                }
                Ok(VectorBody::F32(
                    bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| f32::from_le_bytes(*c))
                        .collect(),
                ))
            }
            Encoding::F16 => {
                let want = dims * 2;
                if bytes.len() != want {
                    return Err(Error::BodyLength {
                        want,
                        got: bytes.len(),
                    });
                }
                Ok(VectorBody::F16(
                    bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| crate::quant::f16_to_f32(u16::from_le_bytes(*c)))
                        .collect(),
                ))
            }
            Encoding::I8 => Ok(VectorBody::I8(Planes::decode(dims, bytes)?)),
            Encoding::Other(_) => Err(Error::Malformed(
                "the space names an encoding this version does not define",
            )),
        }
    }

    /// The vector, whatever the encoding: the I8 planes reconstructed,
    /// the float encodings as they are.
    pub fn values(&self) -> Result<Vec<f32>> {
        match self {
            VectorBody::F32(values) | VectorBody::F16(values) => Ok(values.clone()),
            VectorBody::I8(planes) => planes.reconstruct(),
        }
    }
}

/// A slice of a VECTOR message's value, for a writer whose per-carrier
/// budget is smaller than one message.
#[derive(Clone, Debug, PartialEq)]
pub struct Fragment {
    pub space_id: u8,
    pub record_id: u16,
    /// The length of the whole VECTOR value being sliced.
    pub total: u32,
    /// Where this slice starts within it.
    pub offset: u32,
    pub bytes: Vec<u8>,
}

impl Fragment {
    /// The message value.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(10 + self.bytes.len());
        out.push(self.space_id);
        put_varint(&mut out, u32::from(self.record_id));
        put_varint(&mut out, self.total);
        put_varint(&mut out, self.offset);
        out.extend_from_slice(&self.bytes);
        out
    }

    /// A message value, with section 9's bound on the slice checked.
    pub fn decode(value: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(value);
        let space_id = reader.u8()?;
        let record_id = record_id(reader.varint()?)?;
        let total = reader.varint()?;
        let offset = reader.varint()?;
        let bytes = reader.rest().to_vec();
        let end = u64::from(offset) + bytes.len() as u64;
        if total == 0 || end > u64::from(total) {
            return Err(Error::FragmentBounds);
        }
        Ok(Self {
            space_id,
            record_id,
            total,
            offset,
            bytes,
        })
    }
}

/// A 16-bit record id as a sequence that does not wrap, against the
/// last one seen for that space.
///
/// Section 4 gives a writer a window of 32768 newer records before an
/// id may be used again, so the reading of an id is the candidate
/// nearest the last one seen. Both the assembler and the file index key
/// records this way, so an index built from a stream agrees with
/// reading the stream.
pub fn expand_record_id(last: Option<u64>, record_id: u16) -> u64 {
    let wrap = u64::from(RECORD_ID_WRAP);
    match last {
        None => u64::from(record_id),
        Some(last) => {
            let base = last / wrap * wrap + u64::from(record_id);
            [base.checked_sub(wrap), Some(base), Some(base + wrap)]
                .into_iter()
                .flatten()
                .min_by_key(|candidate| candidate.abs_diff(last))
                .unwrap_or(base)
        }
    }
}

/// A record id, refused above the wrap section 4 gives it.
fn record_id(value: u32) -> Result<u16> {
    if value >= RECORD_ID_WRAP {
        return Err(Error::RecordId(value));
    }
    Ok(value as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_space() -> Space {
        Space {
            space_id: 3,
            dims: 8,
            encoding: Encoding::I8,
            flags: FLAG_UNIT_LENGTH,
            modality: Modality::Description,
            source: 0,
            model: "hf:openai/clip-vit-base-patch32@abc123/model.safetensors".into(),
            model_hash: [7; 16],
            query: "hf:openai/clip-vit-base-patch32@abc123/text.safetensors".into(),
            query_hash: [9; 16],
            producer: "ffrwd-index tests".into(),
        }
    }

    fn a_vector() -> VectorRecord {
        VectorRecord {
            space_id: 3,
            record_id: 65535,
            start_off: -2500,
            end_off: -500,
            body: vec![0x00, 0x3c, 0x01, 0xaa],
        }
    }

    #[test]
    fn a_unit_round_trips() {
        let unit = Unit::new(vec![
            Message::Space(a_space()),
            Message::Vector(a_vector()),
            Message::Fragment(Fragment {
                space_id: 3,
                record_id: 2,
                total: 40,
                offset: 8,
                bytes: vec![1, 2, 3, 4],
            }),
            Message::Unknown {
                kind: 0x90,
                value: vec![0xde, 0xad],
            },
        ]);
        let bytes = unit.encode();
        assert_eq!(bytes.len(), unit.encoded_len());
        assert_eq!(&bytes[..16], &UUID);
        assert_eq!(bytes[16], VERSION);
        let read = Unit::decode(&bytes).expect("a unit");
        assert_eq!(read, unit);
        assert_eq!(read.dropped, 0);
    }

    #[test]
    fn a_payload_that_is_not_ours_is_left_alone() {
        let mut bytes = vec![0u8; 20];
        bytes[..16].copy_from_slice(&[0xab; 16]);
        assert_eq!(Unit::decode(&bytes), Err(Error::NotOurs));
        assert!(!Unit::is_ours(&bytes));
        assert_eq!(Unit::decode(&[]), Err(Error::NotOurs));
    }

    #[test]
    fn a_later_version_is_skipped_whole() {
        let mut bytes = UUID.to_vec();
        bytes.push(2);
        bytes.extend_from_slice(&[TYPE_SPACE, 1, 0]);
        assert_eq!(Unit::decode(&bytes), Err(Error::Version(2)));
    }

    #[test]
    fn a_message_running_past_the_unit_ends_it_and_the_rest_stands() {
        let unit = Unit::new(vec![Message::Space(a_space())]);
        let mut bytes = unit.encode();
        // A second message that claims more bytes than are left.
        bytes.extend_from_slice(&[TYPE_VECTOR, 40, 1, 2, 3]);
        let read = Unit::decode(&bytes).expect("a unit");
        assert_eq!(read.messages, unit.messages);
        assert_eq!(read.dropped, 0, "an overrun is not a dropped message");
    }

    #[test]
    fn a_malformed_message_is_dropped_and_the_next_one_is_read() {
        let mut bytes = UUID.to_vec();
        bytes.push(VERSION);
        // A SPACE whose dims are zero, then a good unknown message.
        bytes.extend_from_slice(&[TYPE_SPACE, 2, 1, 0]);
        bytes.extend_from_slice(&[0x81, 2, 0xaa, 0xbb]);
        let read = Unit::decode(&bytes).expect("a unit");
        assert_eq!(read.dropped, 1);
        assert_eq!(
            read.messages,
            vec![Message::Unknown {
                kind: 0x81,
                value: vec![0xaa, 0xbb]
            }]
        );
    }

    #[test]
    fn an_unknown_type_survives_a_round_trip() {
        let message = Message::Unknown {
            kind: 0x7f,
            value: vec![1, 2, 3],
        };
        let bytes = message.encode();
        assert_eq!(
            Message::decode(bytes[0], &bytes[2..]).expect("a message"),
            message
        );
    }

    #[test]
    fn dims_outside_the_range_end_the_message() {
        let mut space = a_space();
        space.dims = 0;
        assert_eq!(Space::decode(&space.encode()), Err(Error::Dims(0)));
        space.dims = MAX_DIMS + 1;
        assert_eq!(
            Space::decode(&space.encode()),
            Err(Error::Dims(MAX_DIMS + 1))
        );
        space.dims = MAX_DIMS;
        assert!(Space::decode(&space.encode()).is_ok());
    }

    #[test]
    fn a_record_id_at_the_wrap_is_refused() {
        let mut value = vec![3u8];
        put_varint(&mut value, RECORD_ID_WRAP);
        put_svarint(&mut value, 0);
        put_svarint(&mut value, 0);
        assert_eq!(
            VectorRecord::decode(&value),
            Err(Error::RecordId(RECORD_ID_WRAP))
        );
    }

    #[test]
    fn a_fragment_outside_its_value_is_refused() {
        let fragment = Fragment {
            space_id: 1,
            record_id: 1,
            total: 6,
            offset: 4,
            bytes: vec![1, 2, 3],
        };
        assert_eq!(
            Fragment::decode(&fragment.encode()),
            Err(Error::FragmentBounds)
        );
        let empty = Fragment {
            total: 0,
            offset: 0,
            bytes: vec![],
            ..fragment.clone()
        };
        assert_eq!(
            Fragment::decode(&empty.encode()),
            Err(Error::FragmentBounds)
        );
    }

    #[test]
    fn a_body_reads_in_the_spaces_own_encoding() {
        let mut space = Space::new(1, 3, Encoding::F32);
        let body = VectorBody::F32(vec![1.5, -2.0, 0.25]);
        let bytes = body.encode();
        assert_eq!(VectorBody::decode(&space, &bytes).expect("a body"), body);
        assert_eq!(space.body_len(), Some(12));
        assert_eq!(
            VectorBody::decode(&space, &bytes[..11]),
            Err(Error::BodyLength { want: 12, got: 11 })
        );

        space.encoding = Encoding::F16;
        let body = VectorBody::F16(vec![1.5, -2.0, 0.25]);
        let bytes = body.encode();
        assert_eq!(bytes.len(), 6);
        assert_eq!(VectorBody::decode(&space, &bytes).expect("a body"), body);

        space.encoding = Encoding::Other(9);
        assert!(VectorBody::decode(&space, &bytes).is_err());
    }

    #[test]
    fn every_truncation_of_a_unit_is_an_error_or_a_shorter_unit() {
        let unit = Unit::new(vec![Message::Space(a_space()), Message::Vector(a_vector())]);
        let bytes = unit.encode();
        for cut in 0..bytes.len() {
            if let Ok(read) = Unit::decode(&bytes[..cut]) {
                assert!(
                    read.messages.len() <= unit.messages.len(),
                    "a cut at {cut} grew the unit"
                );
            }
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        for _ in 0..2000 {
            let mut bytes = Vec::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let length = (seed >> 33) as usize % 64;
            for _ in 0..length {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push((seed >> 40) as u8);
            }
            let _ = Unit::decode(&bytes);
            // The same bytes behind our own UUID and version, so the
            // message decoders see them rather than the UUID check.
            let mut ours = UUID.to_vec();
            ours.push(VERSION);
            ours.extend_from_slice(&bytes);
            let _ = Unit::decode(&ours);
            for kind in [TYPE_SPACE, TYPE_VECTOR, TYPE_FRAGMENT, 0x00, 0xff] {
                let _ = Message::decode(kind, &bytes);
            }
        }
    }
}
