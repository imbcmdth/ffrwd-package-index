# ffrwd-index

A command line over `ffrwd-index-core`, `ffrwd-index-rows` and
`ffrwd-index-container`: it puts vectors into a video's own elementary
stream and takes them back out, out of a file as well as out of a
stream. The rows below are read by the same code the `weave` module
reads its spaces and vectors with, so a space declared to one means the
same thing to the other.

```
ffrwd-index weave --video IN --vectors ROWS.ndjson --out OUT
                  [--placement keyframe|next|spread:BYTES]
                  [--escapes N] [--fps N] [--codec h264|h265|av1]

ffrwd-index read  --video IN|- [--fps N] [--codec h264|h265|av1]
                  [--index OUT.ffix]

ffrwd-index read  --mp4 IN.mp4 [--scan keyframes|all] [--index OUT.ffix]
ffrwd-index read  --mkv IN.mkv [--scan keyframes|all] [--index OUT.ffix]

ffrwd-index read  --index IN.ffix | IN.mp4 | IN.mkv

ffrwd-index index FILE [--scan all|keyframes] [--rewrite] [--out OUT]

ffrwd-index search --mp4 IN.mp4 | --mkv IN.mkv | --video IN | --index IN
                   | --rows ROWS.ndjson
                   --query Q.json|- [--space ID|URI] [--top K]
                   [--threshold T] [--coarse N] [--scan keyframes|all]
                   [--fps N] [--codec h264|h265|av1]

ffrwd-index watch --video - [--codec h264|h265|av1] [--fps N]
                  --query NAME=FILE [--query NAME=FILE ...]
                  --threshold T [--min-planes N]

ffrwd-index rows-from-describe [--clip C.ndjson] [--speech S.ndjson]
                   [--sound D.ndjson] [--package ffrwd.json]
                   [--producer TEXT] [--out ROWS.ndjson]
```

The format is [SPEC.md](../SPEC.md). Nothing here is specific to one
model or one producer: the rows say which model made the vectors and
which model turns a search into the same space, and the tool carries
whatever it is given.

## What it takes

**Writing** is elementary streams: H.264 and HEVC Annex B, and AV1 as
low-overhead OBUs. ffmpeg converts either way without touching the
picture, so weaving into a file is three commands:

```
ffmpeg -i in.mp4 -c copy -bsf:v h264_mp4toannexb -f h264 in.h264
ffrwd-index weave --video in.h264 --vectors rows.ndjson --out out.h264
ffmpeg -i out.h264 -c copy -f mp4 out.mp4
```

`hevc_mp4toannexb` is the filter for HEVC and AV1 needs none: its
samples are the same OBUs in a file and out of one. `-f obu` is the
elementary stream.

**Reading** is those, and MP4 and Matroska natively. `read --mp4` and
`read --mkv` open the file themselves: they find the video track, work
out where every sample is and when it is shown, and read the front of
the samples they need. ISO base media files work fragmented and not,
`+faststart` or not; Matroska and WebM work with or without `Cues` and
with a segment whose length was never written, which is what a live
writer leaves behind.

**MPEG-TS is out of scope.** A transport stream is a different problem
from a file: it has no sample table to read, the elementary stream is
cut into 188-byte packets with headers through the middle of every NAL,
and the PCR is a clock rather than a timestamp per picture. The records
survive it, as section 10 of the spec says, and ffmpeg takes the stream
back out in one line:

```
ffmpeg -i in.ts -c copy -bsf:v h264_mp4toannexb -f h264 in.h264
ffrwd-index read --video in.h264
```

### Which clock a span is on

An elementary stream carries no timestamps, so `--fps` (30 by default)
is what gives each access unit a presentation time: the access unit at
position `n` in decode order is at `n / fps` seconds. Weave and read
with the same `--fps` and the spans come back where they were put. With
B-frames that time is not the container's presentation time, which is
another reason the spans in the stream are offsets from the frame they
ride on and never absolute times.

A container does carry timestamps, and `read --mp4` and `read --mkv`
use them: `carrier_ms` is the presentation time ffprobe prints for that
packet, edit list and all, and the spans are that plus the offsets on
the wire. So the same records read out of a file and out of the
elementary stream inside it agree about every span's length and about
where it sits relative to its picture, and may disagree about the
absolute number, because one of the two readers knows what time it is
and the other was told a frame rate.

A record doled out over several carriers by `spread` is not a special
case here. Section 4 says the record's span is the one its first
message gives and that a reader does not require the later ones to
agree, so the planes merge whatever the two clocks do. That matters
most for AV1, where an encoder codes several frames in one temporal
unit and shows them later with `show_existing_frame`, and sample times
in decode order are not a frame apart: a `spread` record woven against
`--fps` and read back off the container's own clock still comes back
with all eight of its planes.

### What a scan costs

`--scan keyframes` (the default for `read`) looks at the sync samples
and at nothing else. Section 7 puts every record of a `keyframe` file
on a keyframe, the ones whose span ends after the last keyframe
included: those ride the last keyframe with an end that looks forward.
So there is nothing after the sync samples for a reader to go and find,
which is also what lets the same read work on a transport stream, where
the end of the file is not a thing to seek to. `--scan all` looks at
every sample, which `next` and `spread` need. Either way only the
leading NAL units or OBUs of a sample are read, up to the first coded
slice or frame OBU, because that is where a unit goes. Each read says
on stderr what it cost:

```
big.mp4: scan keyframes: 14 of 300 samples, 9317 of 15564590 bytes read
  (0.06% of the file), 18 seeks
big.mp4: scan all: 300 of 300 samples, 82533 of 15564590 bytes read
  (0.53% of the file), 304 seeks
```

Ten seconds of 640x360 with noise over it, fourteen sync samples, six
records. A keyframe scan of it reads a sixteen hundredth of the file, a
full scan a two hundredth, and reading the index instead is 4422 bytes
in three seeks. That ratio is the point of the `keyframe` policy, and
printing it is how a claim about it stays a measurement.

Matroska costs more seeks for the same bytes: an MP4 has a sample table
that says where everything is, and a Matroska file has to be walked
block by block, which is one small read at each. The same content as
`big.mkv` is 8152 bytes in 96 seeks for the keyframe scan, since its
`Cues` say which clusters to go to, and 89802 bytes in 653 seeks for
the full one.

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
| `vector` | `dims` numbers, as a JSON array or as base64 of little-endian binary32 |
| `record_id` | optional; counts up per space from 0, wrapping at 65536 |
| `available_ms` | optional; when the writer had the record, for the `next` and `spread` policies. Defaults to `end_ms` |

The vector is quantized on the way in for an `i8` space, and narrowed
for an `f16` one. An `f32` space carries the numbers as they are. A
component that is not a finite number costs its row, whatever the
encoding: there is nothing to write for it and nothing to read back.

### Escapes

`--escapes N` (2 by default, 16 at most) is how many of a vector's
largest components an `i8` space sends exactly, as an index and a
binary16 value, instead of quantizing them. One scale for the whole
vector is set by its largest component, so a model with one component
that dwarfs the rest spends its range on that component alone; taking
the largest few out of the scale gives the rest the range they occupy.
Measured on a video-text model, one component held 30% of each vector's
energy and two escapes brought the agreement with the original ranking
from 80% to 99%, for eight bytes a record.

`--escapes 0` writes none, which is right for a model whose components
are all of a size. The escapes ride with the record's first message,
the one that carries the sign plane, so a reader that has anything at
all has them.

### What `read` prints

The spaces first, in the same shape the rows use, then one line per
record:

```json
{"space":{"id":1,"dims":8,"encoding":"i8","unit_length":true,...}}
{"space_id":1,"record_id":0,"carrier_ms":300,"start_ms":0,"end_ms":250,"planes":[0,1,2],"escapes":[[5,0.9975586]],"vector":[...]}
```

`carrier_ms` is the frame the record rode in on. `planes` is which of
the eight bit-planes arrived, and appears for `i8` spaces only: the
vector is reconstructed from the planes in hand, so a record with three
of them is a coarse reading of the same vector rather than a wrong one.
`escapes` is the components that travelled exactly, as index and value
pairs, and appears only when there are some; their values are in
`vector` as well, which is where a reader uses them.

A record whose sign plane never arrived, because the stream was cut
before it, is counted on stderr rather than printed: there is nothing
to reconstruct it from yet.

`read --index` prints the same rows with a `time_ms` on each, which is
the index's own record of the carrier's time, on the container's clock
and signed: an MP4's edit list can put a carrier before the time the
file starts at, and ffprobe prints a negative time for it too. It takes
an index on its own, `file.ffix`, or the file carrying one, `file.mp4`
or `file.mkv`, and works out which from the first bytes.

### A stream on standard input

`read --video -` takes the elementary stream from a pipe rather than a
file. The bytes are cut into access units as they arrive and each one's
units are taken off it and its bytes dropped, so a stream that never
ends does not become a buffer that never stops growing. What `read`
still waits for is the end: its output is the spaces first and then the
records, and neither is known until the last carrier has gone by.
`watch` is the command that says something while a stream runs.

A pipe carries no file name, so `--codec` is what says which codec it
is.

## Searching

`search` ranks the records of one space against a query vector and
prints the spans, best first.

```
ffrwd-index search --mp4 film.mp4 --space xclip --query prompt.json --top 5
{"rank":1,"score":0.247323,"start_t":41.167,"end_t":43.419,"space":1,"record_id":8,"planes":[0,1,2,3,4,5,6,7]}
```

**It does not embed text.** A space says which model turns a query into
its own space, and `search` prints that model on stderr; getting a
vector out of it is the caller's business.
[examples/describe](../examples/describe) does it with the same
package's own text tower.

**The query** is a JSON file, or `-` for standard input. Any of these is
read: an array of numbers, a base64 string of little-endian binary32,
an object with a `vector` member, or a document of one such object a
line, of which the first that holds a vector is the query. Those are
the shapes a vector arrives in from a row, from a vector track and from
a query written to a table destination.

**The space** is `--space`, by id or by any part of the `model` URI,
the `query` URI or the `producer` a SPACE message carries. A file with
one space needs no flag. A name that matches two spaces is a refusal
that lists them, because a search of the wrong space looks exactly like
a search that found nothing.

**Where the records come from**, in the order `search` prefers them:

- `--mp4` and `--mkv` read the file's own index when it has one, which
  is the read section 8 put it there for. `--scan keyframes` or
  `--scan all` asks for the stream itself instead, which is the
  authority the index is a copy of; `all` is what `next` and `spread`
  need.
- `--index` takes a bare `file.ffix` or the file carrying one.
- `--video` is the elementary stream, with `--fps` for its clock.
- `--rows` is the NDJSON a weave would take, scored as the binary32 it
  holds with no encoding applied. That is the brute force
  [MEASUREMENTS.md](../MEASUREMENTS.md) measures the encoding against,
  and having it behind the same flags is how somebody asks what their
  own file's encoding cost their own search.

**The ranking is a cosine**, never a bare dot product: a record's scale
is its own, so two reconstructions of few planes are not comparable by
one. `--top K` keeps the best K, `--threshold T` keeps everything at or
above T, and the two together keep the shorter of the two lists.
Neither flag is the ten best.

**`--coarse N`** scores the query against the sign planes alone, keeps
the best N, and rescores those with every plane the records have. The
scores printed are always the rescored ones, so a threshold means the
same thing with the prefilter and without it. The query stays binary32
against the sign bits rather than being quantized to match them, which
is what MEASUREMENTS.md found beat Hamming distance at every candidate
count in both of the spaces it measured. It is off by default.

Each search says on stderr what it read, in bytes and seeks, how many
records it scored, and what embeds a query into the space it searched.

## Watching a stream

`watch` reads an elementary stream from a pipe and prints a row the
moment a record passes `--threshold`, before the frames after it have
arrived.

```
ffmpeg ... -f h264 - | ffrwd-index watch --video - --codec h264 \
  --query dog=dog.json --query siren=siren.json --threshold 0.25
{"query":"dog","score":0.312,"start_t":12.4,"end_t":14.9,"carrier_t":14.9,"space":1,"record_id":37,"planes":[0,1]}
```

`--query NAME=FILE` may be given as often as there are things to watch
for, and every match row names the query it matched. A query is scored
against every space whose width it has, so one command watches a file's
picture and sound spaces at once when its vectors are the same width in
both, and is ignored by a space it does not fit.

A record is scored as soon as it has plane 0, and again on each later
plane, because the layered encoding makes the first message of a record
a coarse reading of it and the rest refinements. The first score that
passes is printed and the record is not reported again for that query.
`--min-planes N` is for a watcher that would rather wait: it scores
nothing until a record holds a run of N planes from plane 0.

**Joining mid-stream** works because section 3 has every keyframe repeat
every space declaration. Records that arrive before one are held and
scored when it comes. What is held is bounded: a few thousand records,
a few dozen reassemblies, and a wait after which a record whose space
never arrived is dropped. The stream reader holds one access unit and
gives up on one that grows past a limit rather than buffering it.

Each row is flushed as it is written. At the end, stderr says how many
carriers went by, how many matched, how many records are still held and
how many messages were dropped.

## Rows from `ffrwd/describe`

`rows-from-describe` turns what that package writes to an `.ndjson`
destination into the rows `weave` takes.

```
ffrwd-index rows-from-describe --clip clip.ndjson --sound sound.ndjson \
  --speech speech.ndjson --package ../ffrwd-package-describe/ffrwd.json \
  --out rows.ndjson
```

The arithmetic is seconds into milliseconds. The naming is section 3's,
and it is the part worth a subcommand: the clip space's vectors come
from X-CLIP's video tower and a search of it is embedded by the text
tower, which is a different file of the same repository, and the sound
and speech spaces are one sentence embedder reached twice. `--package`
reads the URIs and the SHA-256 digests out of the package's own
manifest; without it the URIs are empty and the hashes go out zero,
which section 3 reads as unknown, and the tool says so on stderr.

Sound and speech become two spaces rather than one, although the model
and the dimensionality are the same, because a SPACE message carries
one modality and these are two: a label for what was heard and a
transcript of what was said. A reader that wanted to search only what
was said could not tell them apart if they shared an id.

The whole path, run on a real file, is in
[examples/describe](../examples/describe).

## The file index

Section 8's copy of the messages at the container level, so that
searching a file is one read instead of a scan. It is derived and never
authoritative: a file without one loses nothing but speed, and
`ffrwd-index index` builds it again whenever it is gone.

Its times are absolute where the stream's are offsets, so a cut or a
join made without re-encoding leaves the stream's records right and the
index wrong. Section 8 has whatever cuts a file drop the index or
rebuild it, and ffmpeg does the first of those for nothing: any remux
drops a top-level box it does not know. What this tool cannot do is
tell a stale index from a good one. Nothing in an index says which
pictures it was built from, so a check would mean reading the sample
table to compare time ranges, which costs more than the index read it
would guard and still misses a cut that kept the file's length. So
`read --index` takes the index at its word and says on stderr that it
is doing so.

```
ffrwd-index index out.mp4
ffrwd-index index out.mkv --out indexed.mkv
ffrwd-index read  --index out.mp4
```

`index` scans every sample by default, not the sync samples alone: an
index built from half a file would be an index that quietly lies, and
the one command whose job is to be complete should be. `--scan
keyframes` is allowed for a file written under the `keyframe` policy,
where it is the same answer for a fiftieth of the reads.

### MP4

The index goes in a top-level `uuid` box with this format's extended
type, appended to the end of the file. Nothing else moves: the `moov`
and the `mdat` stay where they are, the pictures are the same bytes,
and writing it costs one append whatever the file's size. Reading it
back is one read near the end of the file, and only if that misses are
the top-level boxes walked.

Checked against real ffmpeg, on a plain file, a `+faststart` file and a
fragmented one: `framemd5` is unchanged to the byte, `ffprobe` at
warning level says exactly what it said before, and a seek into the
fragmented file still works.

- **A file that already has one.** When our box is last it is cut off
  and written again, so a file does not grow an index a read. When it
  is not last, the file has to be copied without it, and that is
  refused until `--rewrite` asks for it, because a copy is not what
  "in place" promised.
- **A fragmented file.** Its last box is usually `mfra`, whose `mfro`
  child is a copy of `mfra`'s own size, put last so that the last four
  bytes of the file find it. Appending past the `mfra` leaves those
  four bytes pointing at an index instead. ffmpeg 9 does not mind, as
  it builds the fragment index by walking the file, but a reader that
  uses `mfro` is not wrong to, so the box goes in **front of** the
  `mfra` and the `mfra` is written again after it. That costs the
  `mfra`'s own length, sixteen bytes a fragment, and moves nothing
  anything points at: the offsets inside `tfra` name the `moof` boxes,
  and those are all before the `mfra` already.
- **A remux drops it.** `ffmpeg -i indexed.mp4 -c copy out.mp4` writes
  the boxes it knows and ours is not one of them, so the box is gone
  from the copy. That is expected and it costs nothing: the records
  themselves rode inside the pictures' own access units and are still
  there, and `ffrwd-index index out.mp4` builds the box again from
  them. That is what section 8 means by derived.

### Matroska

Reading an attachment is native: `read --index file.mkv` finds the
first `AttachedFile` whose MIME type is `application/x-ffrwd-index` and
prints from it. The MIME type is what identifies it; the file name is
the writer's business, and this one writes `ffrwd-index.bin`.

**Writing one needs ffmpeg**, which is why `index file.mkv` wants an
`--out`. A Matroska attachment is not an append. It lives inside the
`Segment`, so the segment's own length changes; the `SeekHead` at the
front of the file names its children by position, and every position
after the attachment moves; and a file whose `Cues` sit at the end has
those positions in it too. Writing one means rewriting the segment,
which is a muxer, and ffmpeg is already a muxer this workspace tests
against. So the tool builds the index natively, writes it to a
temporary file and runs:

```
ffmpeg -i in.mkv -map 0 -c copy -attach idx \
  -metadata:s:t mimetype=application/x-ffrwd-index \
  -metadata:s:t filename=ffrwd-index.bin out.mkv
```

`-metadata:s:t` is the spelling that reaches the attachment stream, and
it is what puts `FileMimeType` and `FileName` where section 8 says they
go. The tests read the result back natively to prove it. ffmpeg not
being on the PATH is a refusal saying so, and nothing else in the tool
needs it.

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
access unit rather than being dropped. That access unit is usually not
a sync sample, which is why `--scan keyframes` reads it as well: one
extra sample, and the fast path stops having a blind spot at the end of
every file.

## What it writes

`weave` writes the input with SEI NAL units or metadata OBUs added and
nothing else moved: the picture is untouched, byte for byte, and a
player that has never heard of this format plays the file exactly as
before. The `core` tests prove that against ffmpeg with `-f framemd5`.

`index` writes one box on the end of an MP4 and touches nothing else,
which the container tests prove the same way.
