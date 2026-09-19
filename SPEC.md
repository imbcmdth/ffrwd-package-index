# The ffrwd index format, version 1

Embedding vectors carried inside a video stream.

A program that understands a video (a captioner, a detector, an embedder)
produces vectors that describe spans of it. This format puts those vectors in
the video's own elementary stream, as messages a decoder is required to ignore,
so they travel with the picture through remuxing, segmenting and live transport,
and a player that has never heard of them plays the video exactly as before. A
second, optional copy of the same bytes at the container level makes searching a
file a single read.

The format carries anybody's vectors. It says which model made them and which
model turns a query into the same space, and nothing in it is specific to one
model, one producer, or to ffrwd.

Status: draft. Identified by the UUID `041f74a3-8090-5e08-bcfc-764df2dcd466`
(the version 5 UUID of `https://ffrwd.video/index/v1` in the URL namespace).

## 1. Terms

- **Space.** One embedding space: a model, a dimensionality, an encoding. A
  stream may carry several at once.
- **Record.** One vector, in one space, describing one span of time.
- **Carrier.** The access unit (the coded frame) whose metadata holds a message.
- **Unit.** One blob of this format: the bytes that sit in one SEI message or
  one metadata OBU. A unit holds one or more messages.
- **Writer**, **reader.** A program that puts units into a stream, or takes them
  out.

Numbers: `u8` is one byte. Fixed-width numbers wider than a byte are
little-endian. `varint` is an unsigned LEB128 integer of at most 5 bytes (so at
most 2^32 - 1). `svarint` is a signed integer zigzag-mapped to a varint
(0, -1, 1, -2, ... become 0, 1, 2, 3, ...). `str` is a varint byte length
followed by that many bytes of UTF-8, not terminated. `f16` and `f32` are IEEE
754 binary16 and binary32.

## 2. A unit

```
unit     = uuid version message*
uuid     = 16 bytes: 04 1f 74 a3 80 90 5e 08 bc fc 76 4d f2 dc d4 66
version  = u8, 1 for this document
message  = type length value
type     = u8
length   = varint, the number of bytes in value
value    = length bytes
```

A reader that does not find the UUID at the start of a payload leaves the
payload alone: it belongs to someone else. A reader that finds a version it does
not know skips the unit. A reader that finds a message type it does not know
skips `length` bytes and continues, which is how later versions add messages
without breaking earlier readers. A message whose length runs past the end of
the unit ends the unit; what came before it stands.

| type | message |
| --- | --- |
| `0x01` | SPACE: declares an embedding space |
| `0x02` | VECTOR: one record, whole or some of its layers |
| `0x03` | FRAGMENT: a slice of a VECTOR message too large for one carrier |
| `0x80` to `0xff` | private use; never assigned by this format |

Everything else is reserved.

## 3. SPACE

```
space_id    u8        the name VECTOR messages use for this space
dims        varint    components per vector, at least 1
encoding    u8        0 = F32, 1 = F16, 2 = I8 (section 5)
flags       u8        bit 0: the vectors were unit length before they were
                      encoded; other bits are reserved: a writer sets them to
                      zero and a reader ignores them
modality    u8        what was embedded (table below)
source      u8        0 = the stream carrying this message; n = the n-th audio
                      stream of the program as the writer saw it (a hint only:
                      remuxing may reorder streams)
model       str       what produced the vectors, as a URI
model_hash  16 bytes  the first 16 bytes of the SHA-256 of the model's weights,
                      or all zero when unknown
query       str       what embeds a QUERY into this space, as a URI; empty
                      means the same as model
query_hash  16 bytes  as model_hash, for the query model
producer    str       free text naming the writer's source of vectors
```

Bytes after `producer` are reserved for later versions and ignored. A reader
that does not know a space's `encoding` keeps the declaration, so it can still
name the space's models, and ignores that space's VECTOR messages.

`model` and `query` are two things because they often are: a video-text model
embeds the picture with one network and the words of a search with another. A
reader that wants to search a space needs the second, so a writer that knows it
must say it. The URI form is the writer's choice; `hf:<repo>@<revision>/<file>`
for a Hugging Face file and an `https:` URL are both reasonable. The hash is
what makes two names for the same weights comparable.

| modality | meaning |
| --- | --- |
| 0 | unspecified |
| 1 | the picture (frames of the span) |
| 2 | the sound (the audio signal of the span) |
| 3 | speech, as text |
| 4 | sounds, as text (labels, captions of the audio) |
| 5 | text seen in the picture |
| 6 | a description of the picture, as text |

A `space_id` means what the most recent SPACE message with that id said, from
that point in the stream on. A writer that changes a space's definition should
use a new id instead.

**Repetition.** A reader may start anywhere: the middle of a file, a segment of
a ladder, a live stream already running. So a writer repeats the SPACE message
of every space it is using on the first carrier it writes to and on every
keyframe after it. A cut or a segment begins at a keyframe, so whatever begins
there can be read. A reader holds VECTOR messages for a space it has not yet
seen declared until the declaration arrives, and may drop them if it does not
arrive within a wait of its own choosing. Since every keyframe carries the
declarations, the longest keyframe interval a reader expects is wait enough.

## 4. VECTOR

```
space_id    u8
record_id   varint    a counter per space, 0 to 65535, wrapping to 0
start_off   svarint   milliseconds from the carrier's presentation time to the
                      start of the span
end_off     svarint   the same, to the end of the span
body        the vector, in the space's encoding (section 5)
```

The span is `[carrier + start_off, carrier + end_off]`. Offsets are relative to
the carrier because absolute times do not survive: remuxing rescales
timestamps, and a cut or a concatenation made without re-encoding shifts them,
while a message keeps its distance from the frame it rides on. In a live stream
a vector exists only after its span has ended, so both offsets are negative.
The format carries only offsets. However a writer is told a span, the span and
the carrier's presentation time have to be on one clock before it subtracts,
and a stream's first frame is often not at zero.

Two VECTOR messages with the same `space_id` and `record_id` are the same
record. For the layered encoding they may carry different layers, and a reader
merges them; each carries its own offsets from its own carrier, and the
record's span is the one its first message gives. A reader does not require the
others to agree with it: clocks are rescaled between a writer and a reader, and
a millisecond of difference is no reason to refuse a plane. A record's id can
be reused once 32768 newer records of that space have been written.

## 5. Encodings

**F32**, **F16.** `dims` values of that width, little-endian, component 0
first.

**I8.** Each component is a sign and a 7-bit magnitude, with one scale for the
vector, sent as up to eight bit-planes, most significant first. A few components
may be sent exactly instead, as escapes:

```
scale       f16       the largest absolute component that is not an escape,
                      rounded up to the next representable binary16 value
planes      u8        bit k set: plane k is present
escapes     u8        how many escapes follow, at most 16
escape      for each: index varint, value f16
plane data  for each present plane, in ascending k: ceil(dims / 8) bytes
```

Quantizing: `m = round(|x| / scale * 127)`, clamped to 0..127, and `sign = 1`
when `x` is negative. An escaped component has magnitude 0 in the planes and
keeps its sign bit, so plane 0 is the same with or without escapes. Plane 0
holds the sign bits. Plane k, for k from 1 to 7, holds bit `7 - k` of each
magnitude, so plane 1 is the magnitude's most significant bit. Within a plane,
component `i` is bit `7 - (i mod 8)` of byte `i div 8`; unused bits of the last
byte are zero.

Escapes exist because one scale per vector is set by the vector's largest
component, and some models have one that dwarfs the rest: measured on a
video-text model, a single component held 30% of each vector's energy, and
sending the two largest components exactly brought agreement with the original
ranking from 80% to 99%, for 8 bytes. A writer sends a record's escapes in the
message that carries plane 0, in ascending index order with no index twice, and
an empty list in the record's other messages. A model without such components
needs none. An index at or above `dims` costs the message (section 9).

A vector whose components are all zero has scale zero and magnitudes zero. A
vector whose largest component is past binary16's range takes binary16's
largest finite value as its scale, and the clamp does the rest. A record whose
every component is an escape has scale zero: nothing is left to set one.

Reading: with planes 0 to K present, a component's magnitude is the bits known
so far, with the unknown low bits replaced by a one followed by zeros (`1 << (6
- K)` when K is less than 7, nothing when K is 7: the middle of the range they
could hold, rounded up), and its value is `sign * magnitude / 127 * scale`. An
escaped component's value is the one the escape gives, whatever planes are
present. With plane 0 alone every component that is not an escape has the same
magnitude and differs only in sign: a binary embedding up to a scale,
comparable by Hamming distance with no arithmetic, which is what a coarse
search over many records wants. The scale differs from record to record, so a
reader comparing reconstructions of few planes uses cosine or Hamming distance
and not a bare dot product, and a reader that needs unit vectors normalizes
what it reconstructs: bit 0 of the space's `flags` describes the originals.
Each further plane halves the uncertainty, and all eight are the full 8-bit
vector. A reader uses the longest run of planes it has starting at plane 0 and
ignores any plane above the first one missing; a record without plane 0 cannot
be read. A writer must send plane 0 before or with any other plane of a record,
and should send planes in order.

A writer with room sends all planes in one message. A writer with a byte budget
per carrier sends plane 0 first and the rest in later messages for the same
record. A writer may also stop early and never send a record's lower planes,
where it knows the space does not need them; a reader cannot tell that from a
loss and does not need to.

## 6. FRAGMENT

For a writer whose per-carrier budget is smaller than one message.

```
space_id    u8
record_id   varint
total       varint    the length in bytes of the VECTOR value being sliced
offset      varint    where this slice starts within it
bytes       the rest of the message
```

The slices of one VECTOR value, reassembled in order, are that value, and its
`start_off` and `end_off` are relative to the carrier of the slice whose offset
is 0. A FRAGMENT repeats the record's `space_id` and `record_id`, which the
sliced value also carries, so a reader can file slices that arrive out of
order. A FRAGMENT names a record and not one message of it, so a record that is
sent in slices is sent as one VECTOR value: a writer does not slice a record it
also sends as several VECTOR messages, whose values a reader could not tell
apart. A reader that misses any slice drops the record. A writer should prefer
whole planes in separate VECTOR messages (section 5) to fragments: a lost
VECTOR message costs precision, a lost fragment costs the record.

## 7. Carriage in the video stream

**H.264.** An SEI NAL unit (`nal_unit_type` 6) holding one `sei_message` with
`payloadType` 5, `user_data_unregistered`. The payload is the unit: its first 16
bytes are this format's UUID, which is where `uuid_iso_iec_11578` goes.
`payloadSize` is coded with `0xff` bytes as the standard describes. The NAL unit
ends with `rbsp_trailing_bits`, and emulation prevention bytes are inserted over
the whole RBSP as for any NAL unit. The SEI NAL unit goes before the first VCL
NAL unit of its access unit, after any access unit delimiter and parameter sets.

**HEVC.** The same message in a prefix SEI NAL unit (`nal_unit_type` 39), with
the two-byte NAL header, `payloadType` 5. `nuh_layer_id` is zero and
`nuh_temporal_id_plus1` is the access unit's own.

**AV1.** A metadata OBU (`OBU_METADATA`) with `metadata_type` 25, from the range
the specification leaves for unregistered private use, whose payload is the
unit. The UUID at the start tells it from any other user of that type. The OBU
ends with the trailing bits the specification gives every OBU of its kind (one
`0x80` byte, counted in `obu_size`); a reader accepts one without. The OBU
belongs to the temporal unit of its carrier and precedes its frame header.

A decoder that does not know the UUID is required by each of these standards to
ignore the message. Every stream an x264 encoder writes already carries an SEI
of this kind, with the encoder's settings in it.

**One unit per carrier.** A writer puts everything it has for a carrier in one
unit. When that would pass the size below, it writes several units on the
carrier instead, splitting between messages. A reader accepts any number.

**Placement.** Where a record goes is the writer's policy, and a reader handles
all of them the same way:

- `keyframe`: whole records ride on keyframes, each on the first keyframe at or
  after the end of its span. A reader of a file then needs only the start of
  each sync sample. A record whose span ends after the last keyframe rides on
  the last access unit, so such a reader reads the last sample as well. This is
  the policy for files.
- `next`: a record rides on the first carrier after it exists. This is the
  policy for live streams, where a watcher should hear of a match at once and
  the next keyframe may be seconds away.
- `spread`: as `next`, with a byte budget per carrier, filled with planes and
  fragments, most significant first, so the stream's bitrate stays level.

A unit should stay under 4096 bytes unless the writer knows its transport and
its players accept more.

## 8. The file index

The optional copy for search. It is derived from the stream and can always be
rebuilt from it; a file without one, or a stream that never becomes a file,
loses nothing but speed.

```
index    = "FFIX" version count entry*
version  = u8, 1
count    = varint
entry    = time message
time     = svarint, the presentation time in milliseconds of the carrier the
           message came from, on the container's own clock
message  = a SPACE or VECTOR message exactly as it appears in a unit
```

The container's own clock is the time a reader gets for a sample once the
container's timestamps and, in an MP4, its edit list have been applied: the time
a player shows. A file whose first picture is not at zero has an index whose
first entry is not at zero, and a carrier the edit list puts before zero has a
negative time.

Entries are in time order, and where two have the same time a SPACE comes
before a VECTOR. Each distinct SPACE definition appears once, at the time it
first applied. VECTOR messages of one record are merged into one entry, which
takes the time and the offsets of the record's first message; the later
messages add only planes.

The index holds absolute times where the stream holds offsets, so a cut or a
join made without re-encoding leaves the stream's records right and the index
wrong. Whatever cuts or joins a file drops the index or rebuilds it from the
stream.

- **MP4 and its relatives:** a top-level `uuid` box whose extended type is this
  format's UUID and whose content is the index. Players ignore boxes they do
  not know. Placing it at the end of the file moves nothing else. In a
  fragmented file that ends with an `mfra` box it goes before the `mfra`, whose
  last four bytes have to stay the file's last.
- **Matroska:** an attachment with the MIME type `application/x-ffrwd-index`,
  which is what a reader looks for, taking the first if there are several. A
  writer names it `ffrwd-index.bin`.

A live writer produces no index.

## 9. What a reader must not trust

Every length is checked against the bytes that remain before it is used. `dims`
above 65536, a `record_id` at or above 65536, more than 16 escapes or an escape
index at or above `dims`, a string longer than its message, a plane set whose
data runs past the message, and a FRAGMENT whose `offset` plus length exceeds
`total` each cost the message they are in: it is dropped, and the messages
after it in the unit, whose framing is unaffected, are still read. That is not
the case of section 2, a length that runs past the end of the unit, where the
framing itself is lost. A reader bounds what it holds for records that never
complete and spaces that are never declared. Nothing in a unit is executable,
and a URI in a SPACE message is a name, not an instruction to fetch.

## 10. What survives

Remuxing between MP4, Matroska, MPEG-TS and fragmented MP4; HLS and DASH
packaging; RTMP, SRT, WebRTC and Media over QUIC transport; and cuts and joins
made without re-encoding, which keep the records of the frames they keep,
readable from the first keyframe because every keyframe declares its spaces. A
re-encode discards the messages along with every other SEI, as it does
captions. A service that makes its own renditions copies the units onto each
rendition, since they are tied to times and not to pixels.
