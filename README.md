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
with the tool reading the front of the sync samples and the last one, which is
where the records are, and saying what that cost. Ten seconds of 640x360 comes
back out of a sixteen hundredth of the file.

The format is in [SPEC.md](SPEC.md). It is not tied to ffrwd.

[MEASUREMENTS.md](MEASUREMENTS.md) is what the 8-bit encoding costs a search,
measured on real vectors.

## Status

The format is a draft and nothing here is released.

Working today, without ffrwd: the `ffrwd-index` tool weaves rows of vectors
into H.264, HEVC and AV1 elementary streams, reads them back from a stream or
straight out of an MP4 or Matroska file, and writes and reads the file index.
On a 15 MB test file a search reads 0.06% of the bytes to get every vector from
the keyframes, and 0.03% when the file has an index. It also ranks: `search`
scores a query vector against one space of a file by cosine and prints the
spans, and `watch` reads a growing stream from a pipe and prints a match before
the frames after it arrive. See [tool/README.md](tool/README.md).

Working against an unreleased ffrwd: `weave`, the wasm module, runs under a
sidecar built from ffrwd's `packet-filter` branch, which adds the packets-in,
packets-out interface (`ffrwd:av@0.16.0`). No query can place it yet. The
intended use, not runnable today:

```sql
COPY (
  SELECT ffrwd.index.weave(f.video[1], vecs) AS v, f.audio[1] AS a
  FROM input('film.mp4') f
) TO 'film.indexed.mp4'
```

Rows can arrive while packets flow, so the same module serves a live stream: a
vector is woven onto the first frame after it exists, and a watcher reading the
stream hears of it at once. A live stream gets no file index and needs none.

## Layout

- `SPEC.md`: the format.
- `core/`: the codec, plain Rust with no dependencies and no I/O: messages, the
  layered 8-bit encoding, SEI and OBU wrapping, inserting into and reading from
  H.264, HEVC and AV1 streams, placement, and the file index.
- `rows/`: the JSON rows a producer hands over (spaces and vectors), and the
  weaving state machine, shared by the tool and the module.
- `container/`: reading MP4 and Matroska far enough to find the samples that
  carry vectors, and putting the index into a file.
- `tool/`: `ffrwd-index`, a native command line over all of it.
- `weave/`: the ffrwd module, a thin `wasm32-wasip2` layer over `rows/`.
- `ffrwd.json`, `src/index.sql`: the ffrwd package.
- `notes/`: design notes that belong to other repositories.

## License

Apache-2.0.
