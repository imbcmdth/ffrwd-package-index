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

The format is a draft and nothing here is released.

Working today, without ffrwd: the `ffrwd-index` tool weaves rows of vectors
into H.264, HEVC and AV1 elementary streams, reads them back from a stream or
straight out of an MP4 or Matroska file, and writes and reads the file index.
On a 15 MB test file a search reads 0.07% of the bytes to get every vector from
the keyframes, and 0.03% when the file has an index. It also ranks: `search`
scores a query vector against one space of a file by cosine and prints the
spans, and `watch` reads a growing stream from a pipe and prints a match before
the frames after it arrive. See [tool/README.md](tool/README.md).

Working today, with released ffrwd: [examples/describe](examples/describe)
takes one video through `ffrwd/describe`, turns its vectors into rows, weaves
them into the file's own pictures, indexes it and searches it with a prompt the
package's own text tower embedded, and prints what the encoding cost against
the same search over the original binary32.

Working against an unreleased ffrwd: three wasm modules, all
`ffrwd:av@0.16.0`, which no released ffrwd hosts.

`weave` writes. It is a packet filter: encoded packets in, the same packets
out, with the vectors woven into them and every timestamp untouched. Rows can
arrive while packets flow, so the same module serves a live stream: a vector is
woven onto the first frame after it exists, and a watcher reading the stream
hears of it at once. A live stream gets no file index and needs none. The
declaration is what the dialect accepts today, and a query that writes the call
is refused by name, because nothing builds the shape a packet filter sits in:

```sql
COPY (
  SELECT ffrwd.index.weave(f.video[1], vecs) AS v, f.audio[1] AS a
  FROM input('film.mp4') f
) TO 'film.indexed.mp4'
```

`records` and `spaces` read. They are packet sinks: `records` answers one row
per record, with the span in seconds of the stream's own clock and the vector
itself, and `spaces` answers one row per embedding space the stream declares.
Both run today as COPY destinations, with their rows on the sidecar's stdout:

```sql
COPY (SELECT f.video[1] FROM input('film.indexed.mp4') f) TO ffrwd.index.records()
```

What they exist for is the other thing: reading a woven file at COMPILE time,
so that `f.embeddings` is a relation a query can join and filter. Nothing in
the dialect spells it yet. Each says in its meta how much of a stream it has to
be handed, which is what will make that read cheap: `records` asks for the
keyframes, which is where section 7 puts every record of a file, and `spaces`
asks for the first packet, which section 3 now puts every space declaration on.
That is a request and never a promise, and both read whatever they are given.

The reading modules and the writing one are the same code: `rows/` holds both
state machines over `core/`, and the wasm crates are shims.

## Layout

- `SPEC.md`: the format.
- `core/`: the codec, plain Rust with no dependencies and no I/O: messages, the
  layered 8-bit encoding, SEI and OBU wrapping, inserting into and reading from
  H.264, HEVC and AV1 streams, placement, and the file index.
- `rows/`: the JSON rows a producer hands over (spaces and vectors), which
  framing a packet is in, and the two state machines over `core/`: weaving
  vectors into packets and reading them back out. Shared by the tool and the
  three modules, and tested natively.
- `container/`: reading MP4 and Matroska far enough to find the samples that
  carry vectors, and putting the index into a file.
- `tool/`: `ffrwd-index`, a native command line over all of it.
- `weave/`: the ffrwd packet filter that writes, a thin `wasm32-wasip2` layer
  over `rows/`.
- `records/`, `spaces/`: the two ffrwd packet sinks that read, the same way.
- `examples/describe/`: one video through `ffrwd/describe` and out again as a
  search, with the real commands and their real output.
- `ffrwd.json`, `src/index.sql`: the ffrwd package.
- `notes/`: design notes that belong to other repositories.

## License

Apache-2.0.
