# Placing a packet filter in a query

The `packet-filter` interface landed with `ffrwd:av@0.16.0`: encoded
packets in, encoded packets out, rows arriving beside them. The sidecar
hosts one and `ffrwd` reads its describe; nothing in the dialect writes
one into a query. This is what that would take.

Nothing here is implemented. Every path and line number below is in the
ffrwd-cli repository, as of the commit that added the interface there.
This note lives here rather than in that repo because plans do not ship
with the compiler.

## 1. The spelling

### Constraints

- Packets exist only after an encoder, so a filter can only apply to a
  COPY destination's encoded stream. There is no packet anywhere in a
  table query and none in the middle of a filtergraph.
- It takes a stream AND rows. The rows are an annotation array, which
  the dialect already has a type for.
- It has to compose with `RETURNS sink` (weave, then `moq.publish`) and
  with a file destination (weave, then mux to MP4 or MKV with
  `-c copy`).

### Recommended: a stream-returning call, deferred past the encoder

A new `wrtype`, `packets`, beside `sink`:

```sql
CREATE FUNCTION weave(v video_stream, vecs STRUCT(vector vector, t number)[])
  RETURNS packets
  AS 'weave.wasm', 'weave' LANGUAGE wasm;

COPY (SELECT weave(f.video[1], embed(f.video[1]).vectors))
  FROM input('in.mp4') f TO 'out.mp4';

COPY (SELECT weave(f.video[1], embed(f.video[1]).vectors))
  FROM input('in.mp4') f TO publish('relay', 'live');
```

`RETURNS packets` says the call hands back the stream it was given,
still encoded. It reads as a `video_stream` cell everywhere the COPY's
destination consumes one, and the compiler defers it past the encoder
the destination already places.

Grammar delta in `docs/dialect.md`: `wrtype := wstype | sink | packets |
STRUCT(name wstype, name annotation)`. Nothing else moves. The first
parameter is the stream, one annotation parameter carries the rows, and
value parameters configure the module as they do anywhere else.

Why it fits:

- A file destination and a `RETURNS sink` destination both already put
  an encoder in front of the cell. The filter slots into the pipe the
  encoder writes, so both destinations compose with no new rule.
- The rows argument is the annotation column the dialect already spells
  and already type-checks against the module's `rows_schema`.
- A ladder falls out: `COPY (SELECT weave(r.video[1], vecs) FROM
  input('ladder.m3u8') r) TO publish(...)` is one filter instance per
  rendition row, exactly as a packet sink's pads are today.

### Rejected: a sink-side WITH option

```sql
COPY (...) TO 'out.mp4' WITH (packet_filter 'weave.wasm')
```

Smaller in the grammar, worse everywhere else. A WITH value is a scalar,
so the rows argument has nowhere to go; a frame sink takes no WITH at
all, so `RETURNS sink` destinations cannot use it; and it hides a module
call in an option string, which nothing else in the dialect does. It
also cannot say which pad of a ladder it applies to.

### Refusals

| written | message |
| --- | --- |
| `weave(...)` anywhere but a COPY cell | `'weave' returns packets, and packets exist only where a COPY writes them` -- hint: write the call as a cell of the COPY's SELECT |
| `weave(...)` in a bare SELECT (table query) | `a table query runs no ffmpeg, and 'weave' reads what one encoded` |
| `scale(weave(...), ...)` or any ffmpeg filter over it | `ffmpeg filters read frames, and 'weave' hands back the encoded stream` |
| `weave(f.subtitle[1], ...)` | the existing stream-kind refusal, unchanged |
| a WITH `video_codec` the module's describe does not accept | the packet sink's existing message, naming the codecs it does accept |
| the module declares `reads_rows` and the call gives no annotation | `the module 'weave.wasm' reads rows, and 'weave' takes none` -- the same shape `_check_module` already raises for a per-frame rows consumer |

## 2. The planner

### lower

- `cli/ffrwd/lower.py` `_check_module` (~12095): the refusal added with
  the interface -- "no part of a query places one yet" -- becomes
  `_check_packet_filter`, modelled on `_check_packet_sink` (~12184):
  `hosts_packet_filter(described.world)`, the declared return is
  `packets`, the accepted codecs against `WIRE_VIDEO_CODECS` /
  `WIRE_AUDIO_CODECS`, and the arity against the call's stream count.
- `Graph` gains `packet_filters: dict[str, list[dict[str, object]]]`
  beside `packet_sinks`: one entry per node, a dict per pad holding the
  encoder options for that pad (`video_codec`, `pix_fmt`, `crf`, the
  `row` and `rendition` a pad carries). Built exactly where
  `packet_sinks` is built, from the COPY's own WITH options, since the
  encoder in front of a filter is the encoder in front of a sink.

### partition

`cli/ffrwd/processes.py`:

- `_Partitioner._regions` (~1597): `alone = set(self.g.packet_sinks)`
  becomes `set(self.g.packet_sinks) | set(self.g.packet_filters)`. A
  filter reads the encoder's output, so it joins no frame region, for
  the reason already written there.
- `_Partitioner.run` (~2066): the branch that interposes an encoding
  ffmpeg when `consumer in self.g.packet_sinks` applies verbatim --
  widen the test to the filter table. That is what puts
  `encoder ffmpeg -> coded NUT -> sidecar filter` in place.
- `_Partitioner._format` (~2330 and ~2350): `pads =
  self.g.packet_sinks.get(target)` gains the filter table, so the edge
  INTO a filter carries the encoder's output rather than rawvideo/pcm.
  A second branch is new: when the PRODUCER of an edge is a packet
  filter, the edge carries the same coded format its input did, and the
  consumer copies it. `_add_edge(..., copy=True)` (~2250) is the
  existing path for a stream that travels as it arrived.
- `SidecarProcess` (~677) gains `packet_filter: bool`. `network`
  (~774) must include it beside `packet_sink` and `packet_source`: a
  filter's pads are whole encoded streams, one `-i` apiece, not pads cut
  out of one stdout.
- `_region_pad_meta` (~2156): find the filter node the same way it finds
  the sink node, so `-pad` renders for a filter's pads too.
- `cli/ffrwd/wasm.py` `_stream_output` (~1319): a filter writes one
  `-f nut` per pad, in pad order, where a sink writes `-f null -`. The
  `-rows-in` flag goes in the argv builder beside `_ROWS_FROM_FLAG`
  and friends.

The shape either way:

```
ffmpeg (encode) -> coded NUT -> ffrwd-wasm (filter) -> coded NUT -> ffmpeg -c copy -> out.mp4
ffmpeg (encode) -> coded NUT -> ffrwd-wasm (filter) -> coded NUT -> ffrwd-wasm (packet sink)
```

### the rows edge

Two cases, and they want different edges.

**Rows on disk, written by an earlier stage.** The common case today:
an earlier COPY wrote `.ndjson`, this COPY reads it. That is a
`FileEdge` (~449) from the writing process to the filter's sidecar, and
`ProcessPlan.stages` (~868) already orders two processes joined by one.
The filter's argv names the path on `-rows-in`. Nothing new is needed
beyond emitting the edge.

**Rows arriving while packets flow (live).** A pipe, concurrent with the
packets. `RowsEdge` (~466) exists but its `target` is an ffmpeg process
and its `alias` is a compiler-minted `-i`; for a filter the target is a
SIDECAR and the alias belongs on `-rows-in`. Either add a flag to
`RowsEdge` or let the sidecar argv builder place a rows edge whose
target is a sidecar on `-rows-in`. The second is smaller.

**What the annotation stream cannot do here.** Rows ride beside frames
in NUT between two sidecars (`-annotations out` / `-annotations in`).
Between the encoder and the filter there is an ffmpeg, and ffmpeg drops
a stream it does not understand -- so rows produced upstream of the
encoder cannot reach the filter on the stream. They need their own pipe.
That is why the sidecar reads them through `-rows-in` and not
`-annotations in`, and why `run_packet_filter` refuses `-annotations`
outright.

### startup

`cli/ffrwd/startup.py`:

- An edge into a sidecar is CARRIED -- the module docstring says so, and
  the reason holds for `-rows-in`: `run_packet_filter` starts a reader
  thread per pad and then the rows reader before the module's drive loop
  runs. So a rows pipe into a filter orders nothing, which is what lets
  rows arrive mid-run.
- The filter's own `-f nut` outputs are ordinary milestones. The
  downstream ffmpeg opens its inputs one at a time, so a multi-pad
  filter must write its output headers in the order `arrange` picked.
  `run_packet_filter` opens every output's header before any packet
  moves, which is what `run_packet_source` does and for the same reason;
  honouring `arrange`'s order means opening them in that order, so the
  plan has to hand the order to the sidecar (it is the `-f nut` argv
  order already).
- `check` needs no new rule. The filter is one more process in
  `relation`.

## 3. The file index, later

SPEC section 8. Out of scope for the filter itself and out of scope for
live, which produces no index.

- The index is derived from the stream, so it can be rebuilt from a
  finished file. It is a POST-MUX step: read the MP4, gather the SPACE
  and VECTOR messages (or take them from the filter's own rows output,
  which already names every message it wove and where), build
  `FFIX version count entry*`, append a top-level `uuid` box whose
  extended type is the format's UUID.
- ffmpeg cannot write an arbitrary top-level box, so this is not an
  ffmpeg step. It is a new sidecar subcommand over a path, or a rows
  module handed the path -- a process that reads a file and writes a
  file, which the plan has no shape for today.
- Matroska is different: an attachment with MIME type
  `application/x-ffrwd-index`, which ffmpeg CAN write at mux time
  (`-attach`). That needs the index BEFORE the mux, so it only works
  when the rows are all known in advance -- the file case, never live.
- Ordering: a `FileEdge` from the muxing ffmpeg to the index process.
  `stages` already orders on one.
