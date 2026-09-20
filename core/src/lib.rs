//! The ffrwd index format, version 1: embedding vectors carried inside
//! a video's own elementary stream.
//!
//! The crate is the codec and nothing else. It never opens a file, runs
//! a process or allocates unboundedly on untrusted input, so the packet
//! filter that will weave inside a pipeline and the command line tool
//! in `tool/` can both compile it in.
//!
//! Where the bytes of a coded stream begin and end is not this crate's
//! question and is not answered here. `ffrwd-nal` cuts H.264, HEVC and
//! AV1 into NAL units, OBUs, access units and temporal units, finds and
//! places `user_data_unregistered` SEI messages and metadata OBUs of
//! any payload, and has no dependencies, no unsafe code and no opinion
//! about what a payload means. What this crate brings is the format:
//! the [`UUID`] a unit opens with, the [`METADATA_TYPE`] the AV1
//! spelling rides in, and the [`SELECT`] built from the two.
//!
//! The modules follow the spec:
//!
//! - [`wire`]: varint, svarint, str, and a bounds-checked reader.
//! - [`message`]: units and messages, sections 2 to 4.
//! - [`quant`]: the layered 8-bit encoding, section 5.
//! - [`fragment`]: slicing a VECTOR value to a budget, section 6.
//! - [`assemble`]: what a reader feeds messages to.
//! - [`carriage`]: section 7 named for this format, over `ffrwd-nal`.
//! - [`placement`]: where a writer puts what, section 7.
//! - [`index`]: the file index, section 8.
//!
//! Nothing here panics on bad input. Every decoder returns [`Error`],
//! and the tests feed each of them truncated and random bytes to prove
//! it.

#![forbid(unsafe_code)]

pub mod assemble;
pub mod carriage;
pub mod fragment;
pub mod index;
pub mod message;
pub mod placement;
pub mod quant;
pub mod wire;

/// The version 5 UUID of `https://ffrwd.video/index/v1`, which opens
/// every unit and tells this format's payloads from everyone else's.
pub const UUID: [u8; 16] = [
    0x04, 0x1f, 0x74, 0xa3, 0x80, 0x90, 0x5e, 0x08, 0xbc, 0xfc, 0x76, 0x4d, 0xf2, 0xdc, 0xd4, 0x66,
];

/// The AV1 `metadata_type` this format's units ride in, from section 7.
///
/// 25 is inside the range 6 to 31 the AV1 specification leaves for
/// unregistered private use, so no registration is needed and the
/// [`UUID`] inside tells this format's OBUs from anyone else's.
pub const METADATA_TYPE: u64 = 25;

/// Which of a stream's payloads are this format's, for the one
/// `ffrwd-nal` call that needs both halves of the answer.
pub const SELECT: ffrwd_nal::Select = ffrwd_nal::Select::new(UUID, METADATA_TYPE);

/// The format version this crate writes and reads.
pub const VERSION: u8 = 1;

/// The unit size a writer stays under unless it knows its transport,
/// from section 7.
pub const UNIT_SOFT_LIMIT: usize = 4096;

/// The largest `dims` a reader accepts, from section 9.
pub const MAX_DIMS: u32 = 65536;

/// Record ids count per space and wrap at this, from section 4.
pub const RECORD_ID_WRAP: u32 = 65536;

/// How many escapes one I8 record may carry, from section 5.
pub const MAX_ESCAPES: u8 = 16;

/// What went wrong reading or building bytes of this format.
///
/// One enum for the crate: a reader that hits any of these drops the
/// message it was in and keeps going, which is what the spec asks for,
/// and having one type makes that easy to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The bytes ran out before the value did.
    Truncated,
    /// A varint longer than five bytes, or one whose value overflows
    /// 32 bits.
    Varint,
    /// A string that is not UTF-8.
    Utf8,
    /// The payload does not open with this format's UUID: it belongs to
    /// someone else and is left alone.
    NotOurs,
    /// A unit version this crate does not know.
    Version(u8),
    /// `dims` is zero or above [`MAX_DIMS`].
    Dims(u32),
    /// A record id at or above [`RECORD_ID_WRAP`].
    RecordId(u32),
    /// A body whose length is not what the space's encoding needs.
    BodyLength { want: usize, got: usize },
    /// Plane data that runs past the message, or a plane set a reader
    /// cannot use.
    Planes,
    /// More escapes than section 5 allows, or an escape whose index is
    /// not a component of the vector.
    Escapes,
    /// Two halves of one record that disagree about dims or scale.
    Mismatch,
    /// A vector component that is not a finite number.
    NotFinite,
    /// A FRAGMENT whose slice falls outside the value it slices.
    FragmentBounds,
    /// A value that will not fit the field it has to go in.
    TooLarge,
    /// A NAL or OBU that is not the shape its codec requires.
    Malformed(&'static str),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Truncated => write!(f, "the bytes end inside a value"),
            Error::Varint => write!(f, "a varint longer than five bytes or wider than 32 bits"),
            Error::Utf8 => write!(f, "a string that is not UTF-8"),
            Error::NotOurs => write!(f, "the payload does not open with this format's UUID"),
            Error::Version(v) => write!(f, "unit version {v} is not version {VERSION}"),
            Error::Dims(d) => write!(f, "{d} dimensions is not 1 to {MAX_DIMS}"),
            Error::RecordId(id) => write!(f, "record id {id} is not below {RECORD_ID_WRAP}"),
            Error::BodyLength { want, got } => {
                write!(f, "a body of {got} bytes where the space needs {want}")
            }
            Error::Planes => write!(f, "a plane set this record cannot be read with"),
            Error::Escapes => write!(
                f,
                "more escapes than {MAX_ESCAPES} or one outside the vector"
            ),
            Error::Mismatch => write!(f, "two parts of one record disagree"),
            Error::NotFinite => write!(f, "a vector component that is not a finite number"),
            Error::FragmentBounds => write!(f, "a slice outside the value it slices"),
            Error::TooLarge => write!(f, "a value too large for the field it goes in"),
            Error::Malformed(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for Error {}

/// What `ffrwd-nal` says went wrong, in this crate's words.
///
/// The shared crate carries the offset the trouble was found at and
/// this one does not: a reader of this format drops the message it was
/// in and carries on, and where in a NAL that message sat tells it
/// nothing it can act on. The offsets are still there for a caller that
/// holds the `ffrwd_nal::Error` itself.
impl From<ffrwd_nal::Error> for Error {
    fn from(err: ffrwd_nal::Error) -> Self {
        match err {
            ffrwd_nal::Error::Truncated { .. } => Error::Truncated,
            ffrwd_nal::Error::TooLarge { .. } => Error::TooLarge,
            ffrwd_nal::Error::Malformed { what, .. } => Error::Malformed(what),
            ffrwd_nal::Error::UnknownCodec => {
                Error::Malformed("a codec this format has no carriage for")
            }
        }
    }
}

/// The crate's result.
pub type Result<T> = core::result::Result<T, Error>;
