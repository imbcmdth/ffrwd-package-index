# Retiring the vector track, and reading vectors out of the stream

The WebVTT vector track is base64 f32 cues in a titled subtitle track,
tagged `vector_dims`, Matroska only. `ffrwd:av@0.16.0` breaks
compatibility anyway, so it does not have to survive. This is what
deleting it costs, what replaces the read side, and the order to land
them so `ffrwd/describe`'s `find.sql` is never pointed at nothing.

Line numbers are ffrwd-cli's, on the `packet-filter` branch at the
commit that added `RETURNS packets`.

## (a) What implements vector tracks, and what deleting it takes

Vector tracks are one feature wearing two coats. The **track** is the
WebVTT/base64/`vector_dims` machinery and it can go whole. The
**`vector` VALUE** is a scalar type with two builtins over it, and it
must stay: it is how a prompt becomes something to compare against.
Nothing below deletes the type.

### The track, which goes

| Site | What it is |
| --- | --- |
| `lower.py:2066` `VECTOR_DIMS_TAG`, `_VECTOR_ITEM_BYTES` | the tag that tells a vector track from a caption track |
| `lower.py:2103` `_vector_payload`, `:2112` `_vector_values` | the base64 f32 codec, both directions |
| `lower.py:2224` `_record_tracks` | the one line that partitions a container's subtitle streams into `cues` and `embeddings` |
| `lower.py:7517` `_track_record_columns`, `:7607` `_track_dims` | demux each track to WebVTT, decode, and read `vector_dims` |
| `lower.py:9937` `_lower_embedding_array`, `:9980` `_embedding_dims`, `:10012` `_embedding_records`, `:10048` `_embedding_record` | `ARRAY[...::embedding]` into a minted WebVTT track |
| `lower.py:4118` the `VECTOR_DIMS_TAG` branch of `_check_metadata_track_container` | the Matroska-only refusal |
| `lower.py:1871` `_Embedding`, `:672` the three `_EMBEDDING_*` hints, `:2253` `_written_vector` | the record and its messages |
| `lower.py:10170` `_rows_output`'s `vector_dims` stamping, `:12389` `_check_vector_dims`, `wasm.py:1571` `rows_vector_dims` | a rows module's vector column fixing the tag |
| `types.py:321` the `embedding` record, `:351` `_ro("embeddings", ...)`, `:442` `EMBEDDINGS_COLUMN`/`EMBEDDING_TYPE`, `:452` `TRACK_RECORD_COLUMNS` | the column and its type |
| `sidecar/ffrwd-wasm/src/subtitles.rs` lines 8-27, 54, 141, 174 | the run-time half: a vector row encoded into a WebVTT cue |

`TRACK_RECORD_COLUMNS` collapses to `{CUES_COLUMN}`, and
`_mint_webvtt_input`'s `metadata` argument loses its only caller with an
argument.

**Docs.** `rows.md`'s whole `## Embedding rows` section (129-152);
`types.md` lines 11, 13, 21-33, 53, 102; `dialect.md` lines 36-37 (the
`rtype` grammar's `embedding`), 846-847, 924; `examples.md` recipe 118
(half of it) and the Matroska paragraph; `corpus.md` recipes 119 and 134
whole, and the vector halves of 120, 131, 132, 135.

**errors.md.** No error code is vector-only; every refusal is
`UNSUPPORTED_SQL`. The only dependency is the worked example under
`## STREAM_NOT_FOUND` (297-317), which searches two vector tracks of
`described.mkv`. It needs rewriting against caption tracks, not
deleting.

**Tests.** `test_lower.py` 10 tests plus 5 helpers (9389-9611, 9673,
12240); `test_parser.py` 7 (2080, 2102, 2593-2646, 3433) -- all of these
are the *value* rules and mostly survive, only the ones naming
`embeddings` change; `test_types.py` 2 directly plus the mirror tables
at 31, 90-95, 104, 125, 137, 183-188, 207, 218, 240, 253 that a dozen
table-equality tests compare against; `exec/test_tracks.py` 3 of 4 plus
4 helpers; `test_wasm.py`'s two rows-module vector-dims tests (6229,
6242). `test_examples.py` compiles every doc block, so deleting a recipe
deletes its test. The fixture `described.mkv` and its generator
(`scripts/gen_fixtures.py` 16-17, 79-81, 421-478) lose their vector
tracks.

`test_embed_text.py` (3 tests) and the `test_wasm.py` value tests
(6514-6725) are the `vector` TYPE, not the track. They stay.

### What `describe.sql` and `find.sql` become

Both live in `ffrwd/describe`, not here.

`describe.sql` today selects three embedding columns beside the streams,
and each alias becomes a titled vector track:

```sql
SELECT d.v, d.a, d.sound, d.speech,
       d.clip                         AS clip_vectors,
       ffrwd.describe.embed(d.sound)  AS sound_vectors,
       ffrwd.describe.embed(d.speech) AS speech_vectors
```

It becomes a weave: the same rows, written into the video's own packets
rather than into tracks beside it.

```sql
COPY (
  SELECT ffrwd.index.weave(d.v, d.clip) AS v, d.a, d.sound, d.speech
  FROM ffrwd.describe.clips(input(:'src')) d
) TO :'dest'
```

Two things change for the recipe. The dest stops being "an .mkv" --
weaving is in the elementary stream, so MP4 works. And the three spaces
that were three tracks become three `space_id`s in one message stream,
which is what `weave`'s `spaces` parameter is for; the rows carry
`space_id` rather than being sorted by which track they landed in.

`find.sql` today unions two branches over `unnest(f.embeddings)`, each
filtered to its own track before its own `cos_similarity`. It becomes
the same two branches over whatever replaces `f.embeddings`, filtered on
`v.space` instead of `v.track`. The shape of the recipe does not move;
only the column the rows come from and the name of the discriminator.

## (b) How `f.embeddings` should read vectors out of the stream

Decided 2026-09-19 by the owner: ffmpeg does the demuxing, a scanner reads
what it pipes out, and the read is lazy and in tiers. The earlier
recommendation (shell to `ffrwd-index read --mp4` with its own container
parser) is kept at the end of this section for the record.

### What a query needs, and when

1. **Nothing.** A copy, a remux, a ladder, a relay: the vectors are inside the
   packets and travel with them. No read at all.
2. **The shape.** Which spaces a file carries, their dims, encoding, modality,
   and the model and query URIs. Enough to type `f.embeddings`, to refuse a
   384-d row against a 512-d prompt before anything runs, and to choose the
   query model from the file instead of from a track name. This is the SPACE
   messages, and every keyframe carries them, so it is the first keyframe of
   the video stream: a few kilobytes.
3. **The values.** Only when the query unnests `f.embeddings` somewhere that
   shapes the graph. `find.sql` is the case: its WHERE decides how many trims
   exist and with what times, and ffrwd compiles to one fixed command, so the
   vectors have to be in hand at compile time. This is the same position cue
   text is in today.

So the read is lazy: nothing on an ordinary probe, tier 2 when a query names
`f.embeddings` at all, tier 3 when it reads a row's `vector`, `start_t` or
`end_t` in a compile-time position. Each tier is memoized per path beside the
ffprobe result, like `track_cues()`.

### How the bytes are read

ffrwd already runs ffmpeg at compile time for exactly this kind of column:
`probe.track_cues()` extracts a WebVTT track with ffmpeg, memoized, and the
unit tier works from recorded results. The vector read sits beside it.

```
ffmpeg -discard nokey -i SRC -map 0:v:0 -c copy <pipe format> -  |  scanner
```

- `-discard nokey` makes the demuxer drop every packet that is not a keyframe.
  For an indexed container it uses the sample table and never reads the rest.
  Measured on the 44 MB example file: 0.12 s, 9 MB read, no decode.
- It covers everything ffmpeg can open: MP4, Matroska, MPEG-TS, HLS and DASH
  manifests, remote URLs. ffrwd gains no container parser.
- Tier 2 is the same command stopped after the first packet (`-frames:v 1`).
- A stream written with `next` or `spread` placement carries records on frames
  that are not keyframes. The SPACE messages do not say which placement a
  writer used, so the rule is: keyframes first; if a query needs every record
  of a live-written file the caller asks for the full copy (the same command
  without `-discard nokey`, still at disk speed). Worth a `placement` hint in a
  later version of the format if this turns out to matter.
- A `keyframe`-placed record whose span ends after the last keyframe rides the
  last access unit (SPEC section 7). The keyframe copy misses it. On the
  example file it returned 12 of the full scan's 14 rows; which two were
  missing was not checked, and this rule is the likely cause. Cover it with a
  second short copy from near the end (`-sseof -2`), or by reading the file
  index when there is one.
- When the file has an index (section 8) and nothing suggests it is stale,
  tier 3 is one small read and no ffmpeg. The index does not survive a remux,
  so the pipe is the path that always works and the index is the shortcut.

### The scanner is a packet sink in `ffrwd/index`

Decided 2026-09-19: the scanner is a wasm packet sink (`records`, and `spaces`
for the shape), shipped in the `ffrwd/index` package and run by the sidecar at
compile time. The sidecar is the wasm host; it already reads coded NUT with
each packet's pts and hands packets to a sink, so nothing native is added to
it, nothing is published to crates.io, and ffrwd needs no extra binary. `core`
has no dependencies and compiles to wasm as it is.

The pipe format is NUT for the same reason it is everywhere else in ffrwd: it
carries timestamps. A raw elementary stream (`-f h264`) has none, and a reader
of one falls back on a frame-rate clock: wrong for variable frame rate, and off
by the reorder delay on B-frame streams (the 83 ms seen in `examples/describe`).

This makes the read a package function and not a core column: a core
`f.embeddings` would depend on a package being installed. `find.sql` reads
`FROM input(:'src') f, ffrwd.index.records(f.video[1]) v`. What core gains is
generic: a packet sink called in FROM over an input's stream is evaluated while
compiling (the same two-process pipe ffrwd builds at run time for any sink),
with a hint in the sink's meta, `wants: all | keyframes | first`, that the
compiler turns into `-discard nokey` or `-frames:v 1`. The earlier objection to
this route, that it needs ffmpeg at compile time, does not hold:
`probe.track_cues()` already runs ffmpeg at compile time.

The full plan is `vectors-in-stream-replace-vector-tracks.md` in the owner's
plans directory.

### What the seam has to get right

The tool reports a span in milliseconds and a `space_id`; `f.embeddings`'
record should be `(index, space, start_t, end_t, vector)` with `start_t` and
`end_t` in seconds like every other row column, plus the space's `model`,
`query` and `dims` for tier 2, so the conversion happens at the seam and
nothing above it learns the format's units. A file with no records is an empty
relation, not a refusal: an un-woven file is a normal thing to point a query
at.

### Considered and set aside

- **Shelling to `ffrwd-index read --mp4` / `--mkv`**, the tool's own container
  parser. Reads less (0.06% of a file against about 20% for the keyframe copy)
  but only MP4 and Matroska, local files only, and it puts a second demuxer's
  opinion about timestamps into the compiler. It stays in the tool, where the
  small read is the point.
- **A packet-sink module in the `ffrwd/index` package as a probe-time step.**
  Needs a compile-time call path that does not exist and ships the format as a
  wasm blob to do what a native scanner does.
- **A dummy track or container tags carrying the shape.** Brings back the track
  that tools drop and that can disagree with the stream, to save a few
  kilobytes of read.
- **Piping the file into ffprobe through the sidecar.** ffprobe reads a pipe
  for streamable input and fails on an MP4 whose `moov` is at the end; and a
  sidecar that has parsed the vectors in order to rewrite the input can emit
  the rows itself.

## (c) Getting a query-time vector to a destination

`embed_clip_text('prompt')` is already an ordinary compile-time value: a
`RETURNS vector` wasm value function, evaluated once per call, and
`cos_similarity` and `vector_length` already take it. Nothing about the
read path changes that. Two things stop it reaching a file.

**A vector cell is printed capped, everywhere.** `table.py:77`
`VECTOR_CELL_CAP = 4` and `_cell_text` (140-144) render
`[0.12, -0.03, 0.5, 0.77, ... (384)]`, and `render_csv` (171-186) goes
through the same function -- so a 384-d embedding exported to CSV today
silently writes four numbers and a count. The cap is right for the
psql-style table a human reads and wrong for every machine-readable
destination. Split it: cap in `render_table`, write the full JSON array
in `render_csv` and anything else a program reads.

**There is no table destination a vector belongs in.** `.ndjson` is a
ROWS file, not a table: `lower.py:3778` `_rows_file` accepts exactly one
column and only if it is a module's annotation projection or a rows
function's result, so `COPY (SELECT embed_text('cat')) TO 'x.ndjson'` is
refused. `.json` is not handled at all -- it falls through to the media
path and is handed to ffmpeg as a container. The smallest fix that makes
a prompt vector writable is to let a table sink write JSON: one more
`format` value beside `csv` in `sink.py:430`, one more renderer beside
`render_csv`, and vectors write in full there by the rule above.

That is enough for the two things a search needs: a prompt vector that
can be written down and read back, and a row's vector that can be
compared against it.

## (d) The order to land them

The rule is that `find.sql` always has something to read.

0. **A packet sink can be read at compile time**, and `records` and `spaces`
   exist in `ffrwd/index` (section (b)).
1. **The read path first.** `f.embeddings` (or `f.vectors`, if the
   column is renamed with the record) reads woven records through
   ffmpeg's keyframe copy and the scanner of section (b), beside the existing track reader rather than instead of it. A
   file that has been woven becomes searchable; a file with tracks still
   reads as it did. Nothing breaks.
2. **Full-fidelity vectors at a machine-readable destination.** The
   `render_csv` cap, and the JSON table format if it is wanted. A prompt
   vector can now be written down.
3. **`ffrwd/index`'s placement.** `RETURNS packets` and a destination
   that puts a filter between the encoder and the muxer, so `describe.sql`
   can weave instead of minting tracks. Until this lands, step 1 has
   nothing to read in a file ffrwd itself produced -- only in one the
   `ffrwd-index` tool wove -- which is why it comes before the deletion
   and after the read path.
4. **Rewrite `describe.sql` and `find.sql`** against the woven stream.
   Both packages move together; `ffrwd/describe` gains a dependency on
   `ffrwd/index`.
5. **Delete the track.** The code in the table above, then the docs,
   then the fixture's vector tracks, then the tests. Last, because until
   step 4 has shipped, a released `find.sql` still reads tracks.

Steps 1 and 2 are additive and can land in any order. Step 5 is the only
breaking one, and by the time it runs nothing points at what it removes.
