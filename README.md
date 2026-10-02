# ffrwd/index

Embedding vectors woven into a video stream, and read back out of it.

`ffrwd/index` takes a video stream and rows of vectors, from any model, and
writes the vectors into the stream itself: SEI messages in H.264 and HEVC, a
metadata OBU in AV1. A player that has never heard of them plays the video
unchanged. A reader that has finds, for every span somebody described, the
vector, the model that made it, and the model that turns a search into the same
space. For a file it can also write one small index inside the container so a
search is a single read; for a live stream it writes only the messages, as the
vectors arrive.

Reading a file is native, without ffmpeg: MP4 and Matroska, fragmented or not,
with the tool reading the front of the sync samples, which is where every
record of a file is and where every keyframe says what spaces it carries, and
saying what that cost. Ten seconds of 640x360 comes back out of a fifteen
hundredth of the file, and its first packet alone says what is in it.

The format is in [SPEC.md](SPEC.md). It is not tied to ffrwd.

[MEASUREMENTS.md](MEASUREMENTS.md) is what the 8-bit encoding costs a search,
measured on real vectors.

## Status

The format is a draft. The package is installable:

```
ffrwd install ffrwd/index
```

Requires ffrwd 0.29, whose `ffrwd/wasm` is 0.19.1. The three modules are
nodes of `ffrwd:av@0.19.1`. They ship built, and an install compiles nothing.

The `ffrwd-index` tool is the same format without ffrwd: it weaves rows of
vectors into H.264, HEVC and AV1 elementary streams, reads them back from a
stream or straight out of an MP4 or Matroska file, and writes and reads the
file index. On a 15 MB test file a search reads 0.07% of the bytes to get every
vector from the keyframes, and 0.03% when the file has an index. It also ranks:
`search` scores a query vector against one space of a file by cosine and prints
the spans, and `watch` reads a growing stream from a pipe and prints a match
before the frames after it arrive. See [tool/README.md](tool/README.md).

`ffrwd/describe` is the producer these modules were written against, and its
own `describe` and `find` recipes run the whole path.
[examples/describe](examples/describe) is what the encoding costs a ranking,
measured against those recipes' own vectors.

`weave` writes. It is a node: encoded packets in on `v`, the same packets out,
with the vectors woven into them and every timestamp untouched, and rows of
vectors in on an input per space. Each input pairs its rows with the packets by
time, waiting for its producer, so a record is in hand before the packets of
its span leave. The same module serves a live stream: a vector is woven onto
the first frame after it exists, and a watcher reading the stream hears of it
at once. A live stream gets no file index and needs none. A COPY whose
destination places an encoder is where the call goes, and the compiler puts
the node between that encoder and the muxer:

```sql
COPY (
  SELECT ffrwd.index.weave(f.video[1],
                           clip => ffrwd.describe.clips(f.video[1],
                                                        ffrwd.shots.simple_detector(f.video[1])),
                           spaces => '[{"name":"clip","dims":512,"modality":"picture"}]') AS v,
         f.audio[1] AS a
  FROM input('film.mp4') f
) TO 'film.indexed.mp4'
```

The arguments and the params are their own section below.

`records` and `spaces` read. They are sinks read in FROM, nodes with packets in
and rows alone out: the compiler stream-copies one stream of the file into the
module while the query compiles and binds what the module wrote as a row table. `records` answers one row per
record, with the span in seconds of the stream's own clock and the vector
itself; `spaces` answers one row per embedding space the stream declares. A
search is joins and predicates over those rows, decided before ffmpeg is
started:

```sql
COPY (
  SELECT concat(VARIADIC array_agg(ffmpeg.trim(f.video[1],
                                               start => v.start_t,
                                               end => v.end_t)))
  FROM input('film.indexed.mp4') f, ffrwd.index.records(f.video[1]) v
       JOIN ffrwd.index.spaces(f.video[1]) s ON v.space = s.space
  WHERE s.modality = 'picture'
    AND cos_similarity(v.vector, ffrwd.describe.embed_clip_text('a dog')) > 0.25
) TO 'found.mp4'
```

**Join `spaces`; do not name an id.** A record carries a space id and nothing
else, and an id is a position in one writer's table: `weave` hands them out in
the order its `spaces` param declares them, from zero, and the `ffrwd-index`
tool hands out whatever its rows say. A file outlives the run that wrote it, so
the thing to select on is `modality`, or `model`, or `dims`, which are fields of
the format and mean the same in every file. `records(...) v JOIN spaces(...) s
ON v.space = s.space` is how a query gets at them, and it is what the
`ffrwd/describe` recipes do.

Each module says in its shape how much of a stream it has to be handed, which is
what makes the read cheap: `records` asks for the keyframes, which is where
section 7 puts every record of a file, and `spaces` asks for the first packet,
which section 3 now puts every space declaration on. That is a request and never
a promise, and both read whatever they are given. The read is memoized per file,
stream, module and params, so a query naming columns of both reads each once.

The columns are what each module writes, and `src/index.sql` names every one of
them. `records` answers `index`, `space`, `record_id`, `start_t` and `end_t` as
numbers (the two times in seconds), and `planes` and `vector` as vectors.
`spaces` answers `space`, `dims` and `source` as numbers, `unit_length` as a
boolean, and `name`,
`encoding`, `modality`, `model`, `model_hash`, `query`, `query_hash` and
`producer` as text.

`planes` is which of the eight bit-planes of an `i8` record arrived; it reads
NULL for the float encodings, which arrive whole or not at all. It is an array,
and `vector` is the one array type the dialect has, so `vector` is what it is
declared. It is not an embedding and nothing should score it.

The reading modules and the writing one are the same code: `rows/` holds both
state machines over `core/`, and the wasm crates are shims over
[`ffrwd-node`](https://github.com/imbcmdth/ffrwd-node).

## What a query hands `weave`

The module has an input of rows per space its `spaces` param declares, named
for the space. `clip`, `sound` and `speech` are the three `src/index.sql`
declares, for `ffrwd/describe`'s three spaces, each `DEFAULT NULL`: a call binds
the ones it has a producer for, by name, and leaves the rest off. A declared
input the call's `spaces` has no space for is fine left unbound.

The input a row arrives on is how it gets a space. A producer hands over spans
and vectors and knows nothing of embedding spaces, so the precedence is: the
row's own `space` field; else the input it arrived on; else the single declared
space, where a run declares exactly one; else the row is dropped and a row says
so. Rows the `ffrwd-index` tool reads name their own space and are untouched by
any of this.

A producer with other spaces, or more of them, writes its own `CREATE FUNCTION`
over the same wasm file, naming its inputs after its own spaces. That has to be
a query's declaration rather than another package's: a package's lib file may
only name modules the package itself ships.

Every value argument in the dialect is text, number, boolean or vector, so
every one of the module's params is a scalar:

| param | type | what it is |
| --- | --- | --- |
| `spaces` | text | The space table, as an array of objects in JSON. Required. |
| `placement` | text | `keyframe` (the default), `next` or `spread`. |
| `budget` | number | Bytes of messages an access unit, for `spread` alone. |
| `escapes` | number | 0 to 16, 2 by default. |
| `planes` | number | 1 to 8 of an `i8` record's bit-planes, all eight by default. |

`spaces` is the one that wanted to be an array of objects. A query cannot
write one, so the module declares it as text and a producer passes the array's
own JSON as a literal:

```json
[{"name": "clip", "dims": 512, "encoding": "i8", "unit_length": false,
  "modality": "picture", "source": 0,
  "model": "hf:org/repo@rev/video_tower.onnx", "model_hash": "0f1e...",
  "query": "hf:org/repo@rev/text_tower.onnx", "query_hash": "c14d...",
  "producer": "ffrwd/describe 0.1.2"}]
```

`name` and `dims` are required and the rest have defaults; the fields are
section 3's, the same ones the tool's own `{"space": {...}}` rows spell. The
array itself is still read wherever it appears, so `ffrwd-wasm -params-from`
and the native tool go on passing the array.

**The wire ids are this array's own positions, from zero.** The first space
declared is id 0, the second id 1, and a run that declares a different table
gives the same model a different id. The `ffrwd-index` tool's rows hand out
whatever the rows themselves say, which is a third numbering again. None of
that is a property of the file, so nothing downstream should read it: a
consumer joins `spaces` and selects on `modality`, `model` or `dims`, which
section 3 makes mean the same thing in every file. The one place an id belongs
is the join itself, `records(...) v JOIN spaces(...) s ON v.space = s.space`.

## Building the modules

```
cargo build --release --target wasm32-wasip2 -p weave -p records -p spaces
cargo test
```

The modules are nodes written with
[`ffrwd-node`](https://github.com/imbcmdth/ffrwd-node), which carries the
world they are built against, so nothing is installed first. The files land in
`target/wasm32-wasip2/release/`, the path `src/index.sql` names. The tests that
drive the modules through a real host skip without `FFRWD_WASM` naming the
`ffrwd-wasm` of ffrwd 0.29.

To run a consuming query against the checkout, `ffrwd link` here and `ffrwd
link ffrwd/index` in that project. The first is recorded in
`~/.cache/ffrwd/ffrwd.links` for the whole machine and the second in the
project's own `ffrwd.links`; neither is version control's business, and every
command afterwards says what it is reading:

```
warning: package 'ffrwd/index' is linked to <path>, so this command depends on
files no lockfile pins
```

`ffrwd unlink ffrwd/index` puts the pinned version back.

## Layout

This repository holds the format and nothing below it. The byte level it rides
on is two crates of its own, shared with the other ffrwd packages that used to
carry a copy of the same code each:

- [`ffrwd-nal`](https://github.com/imbcmdth/ffrwd-nal): where the NAL units and
  OBUs of H.264, HEVC and AV1 begin and end, which of them open a picture, and
  how to put a payload into one or read one out without moving anything else.
- [`ffrwd-bmff`](https://github.com/imbcmdth/ffrwd-bmff): ISO base media files,
  read and written. Where every sample of a track is, when it is shown, and a
  top-level box found, appended or replaced in a file in place.

Both are dependency-free, forbid unsafe code and build for `wasm32-wasip2`,
which is what the empty dependency lists here were protecting. What is left
below is this format: its UUID, its messages, where a writer puts them, and the
two containers' own business.

- `SPEC.md`: the format.
- `core/`: the codec, plain Rust and no I/O: messages, the layered 8-bit
  encoding, placement, the file index, and the thin `carriage` layer that names
  `ffrwd-nal`'s SEI and OBU calls with this format's UUID.
- `rows/`: the JSON rows a producer hands over (spaces and vectors) and the two
  state machines over `core/`: weaving vectors into packets and reading them
  back out. Shared by the tool and the three modules, and tested natively.
- `container/`: Matroska, which nothing else here reads, and the prefix scan
  that finds the samples carrying vectors without reading the pictures. MP4 is
  `ffrwd-bmff`, and so is putting the index box into a file.
- `tool/`: `ffrwd-index`, a native command line over all of it.
- `weave/`: the ffrwd node that writes, a thin `wasm32-wasip2` layer over
  `rows/`.
- `records/`, `spaces/`: the two ffrwd nodes that read, the same way.
- `examples/describe/`: what the 8-bit encoding cost one real ranking, against
  the same search over the vectors `ffrwd/describe` produced.
- `ffrwd.json`, `src/index.sql`: the ffrwd package.

## License

Apache-2.0.
