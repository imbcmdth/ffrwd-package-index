# ffrwd-index

A command line over `ffrwd-index-core`: it puts vectors into a video's
own elementary stream and takes them back out.

```
ffrwd-index weave --video IN --vectors ROWS.ndjson --out OUT
                  [--placement keyframe|next|spread:BYTES]
                  [--fps N] [--codec h264|h265]

ffrwd-index read  --video IN [--fps N] [--codec h264|h265] [--index OUT.ffix]
ffrwd-index read  --index IN.ffix
```

The format is [SPEC.md](../SPEC.md). Nothing here is specific to one
model or one producer: the rows say which model made the vectors and
which model turns a search into the same space, and the tool carries
whatever it is given.

## What it takes

H.264 and HEVC **Annex B elementary streams**, in and out. MP4 and
Matroska are the next pass, not this one; ffmpeg converts either way
without touching the picture:

```
ffmpeg -i in.mp4  -c copy -bsf:v h264_mp4toannexb -f h264 in.h264
ffmpeg -i in.mkv  -c copy -bsf:v hevc_mp4toannexb -f hevc in.h265
ffmpeg -i out.h264 -c copy -f mp4 out.mp4
```

An elementary stream carries no timestamps, so `--fps` (30 by default)
is what gives each access unit a presentation time: the access unit at
position `n` in decode order is at `n / fps` seconds. Weave and read
with the same `--fps` and the spans come back where they were put. With
B-frames that time is not the container's presentation time, which is
another reason the spans in the stream are offsets from the frame they
ride on and never absolute times.

AV1 is in the library but not yet in the tool.

## The rows

One JSON object per line. Blank lines and lines starting with `#` are
skipped. A space has to be declared before the vectors that use it.

### A space

```json
{"space": {
  "id": 1,
  "dims": 512,
  "encoding": "i8",
  "unit_length": true,
  "modality": "picture",
  "source": 0,
  "model": "hf:openai/clip-vit-large-patch14@refs/pr/4/model.safetensors",
  "model_hash": "9f86d081884c7d659a2feaa0c55ad015",
  "query": "hf:openai/clip-vit-large-patch14@refs/pr/4/text.safetensors",
  "query_hash": "",
  "producer": "my captioner 0.3"
}}
```

| field | meaning |
| --- | --- |
| `id` | 0 to 255, the name the vector rows use. `space_id` is accepted too |
| `dims` | components per vector, 1 to 65536 |
| `encoding` | `i8` (the default), `f16` or `f32` |
| `unit_length` | whether the vectors are unit length. Default false |
| `modality` | `unspecified`, `picture`, `sound`, `speech`, `sound-text`, `scene-text`, `description`, or a number for one this version does not name |
| `source` | 0 for the stream itself, `n` for the n-th audio stream |
| `model` | a URI for what made the vectors |
| `model_hash` | the first sixteen bytes of the SHA-256 of the weights, as hex; empty for unknown |
| `query` | a URI for what embeds a query into the same space; empty means the same as `model` |
| `query_hash` | as `model_hash` |
| `producer` | free text naming where the vectors came from |

Everything but `id` and `dims` may be left out.

### A vector

```json
{"space_id": 1, "start_ms": 0, "end_ms": 2000, "vector": [0.12, -0.03, ...]}
{"space_id": 1, "start_ms": 2000, "end_ms": 4000, "vector": [...], "record_id": 7, "available_ms": 2100}
```

| field | meaning |
| --- | --- |
| `space_id` | the space this vector is in |
| `start_ms`, `end_ms` | the span it describes, in milliseconds from the start of the stream |
| `vector` | `dims` numbers |
| `record_id` | optional; counts up per space from 0, wrapping at 65536 |
| `available_ms` | optional; when the writer had the record, for the `next` and `spread` policies. Defaults to `end_ms` |

The vector is quantized on the way in for an `i8` space, and narrowed
for an `f16` one. An `f32` space carries the numbers as they are.

### What `read` prints

The spaces first, in the same shape the rows use, then one line per
record:

```json
{"space":{"id":1,"dims":8,"encoding":"i8","unit_length":true,...}}
{"space_id":1,"record_id":0,"carrier_ms":300,"start_ms":0,"end_ms":250,"planes":[0,1,2],"vector":[...]}
```

`carrier_ms` is the frame the record rode in on. `planes` is which of
the eight bit-planes arrived, and appears for `i8` spaces only: the
vector is reconstructed from the planes in hand, so a record with three
of them is a coarse reading of the same vector rather than a wrong one.

`read --index file.ffix` prints the same rows with a `time_ms` on each,
which is the index's own record of the carrier's time.

## Placement

- `keyframe` (the default): whole records ride on keyframes, each on
  the first keyframe at or after the end of its span. A reader of a file
  then only has to look at sync samples.
- `next`: a record rides the first frame after it exists, which is what
  a live stream wants.
- `spread:BYTES`: as `next`, with a budget per frame, filled with
  planes and then with fragments, most significant first, so the added
  bitrate stays level.

The policy is the only difference between writing a file and writing a
live stream, that and whether an index is built beside it. The space
declarations go on the first frame the tool writes to and on every
keyframe after it whatever the policy is, so a cut made with
`ffmpeg -ss ... -c copy`, a segment of a ladder, or a viewer joining a
live stream can read from its first frame: that frame is a keyframe and
it carries the declarations.

Records that never meet a carrier the policy would choose, a record
whose span ends after the last keyframe for instance, go on the last
access unit rather than being dropped.

## What it writes

The output is the input with SEI NAL units added and nothing else
moved: the picture is untouched, byte for byte, and a player that has
never heard of this format plays the file exactly as before. The
`core` tests prove that against ffmpeg with `-f framemd5`.
