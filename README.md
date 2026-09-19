# ffrwd/index

Embedding vectors woven into a video stream, and read back out of it.

`ffrwd/index` takes a video stream and rows of vectors, from any model, and
writes the vectors into the stream itself: SEI messages in H.264 and HEVC, a
metadata OBU in AV1. A player that has never heard of them plays the video
unchanged. A reader that has finds, for every span somebody described, the
vector, the model that made it, and the model that turns a search into the same
space. For a file it can also write one small index beside the stream so a
search is a single read; for a live stream it writes only the messages, as the
vectors arrive.

The format is in [SPEC.md](SPEC.md). It is not tied to ffrwd.

Status: the format and its codec are being built first. The ffrwd package, a
packet filter that does the weaving inside a pipeline, follows once the `ffrwd:av`
world has a packets-in, packets-out interface.

## Layout

- `SPEC.md`: the format.
- `core/`: the codec, plain Rust with no I/O: messages, the layered 8-bit
  encoding, SEI and OBU wrapping, and inserting into and reading from H.264,
  HEVC and AV1 streams.
- `tool/`: a native command-line tool over `core`, for weaving vectors into a
  file and reading them back without ffrwd.

## License

Apache-2.0.
