# One video through `ffrwd/describe` and back out as a search

`ffrwd/describe` reads a video with four models and hands back vectors.
This example puts those vectors inside the video's own pictures, builds
the file index, embeds a prompt with the same package's text tower, and
ranks the spans. It ends by running the same query over the binary32
the models produced, so what the 8-bit encoding cost the ranking is a
number on the screen rather than a claim.

```
./run.sh SOURCE [PROMPT]
```

Everything lands in `out/`, which is ignored. Nothing is downloaded:
the models are whatever `ffrwd` already has cached for the package.

- `FFRWD_DESCRIBE` is the package checkout, `../ffrwd-package-describe`
  beside this repository by default. Every `ffrwd` run happens with that
  directory as the working directory, because `ffrwd/describe` resolves
  as a project package.
- `FFRWD_INDEX` is the `ffrwd-index` binary, `target/debug/ffrwd-index`
  by default.

POSIX `sh` only. There is no PowerShell twin: the script is mostly
pipelines and `ffprobe` output turned into shell variables, and a
faithful twin would be a rewrite rather than a translation.

## The steps

**1. describe.** Three one-column queries, one per space
([`clip_vectors.sql`](clip_vectors.sql),
[`sound_vectors.sql`](sound_vectors.sql),
[`speech_vectors.sql`](speech_vectors.sql)), each writing to an
`.ndjson` destination. Each is a slice of the package's own
`recipes/describe.sql`. One object a line comes back:

```json
{"end_t":3.920583333333333,"pts":190190,"start_t":0.0,"time":3.96,"vector":[-0.033245705, ...]}
```

The stock recipe would be `ffrwd run ffrwd/describe:describe -v src=... -v dest=...`,
and it does the same work in one pass. It is not what this example
runs, for two reasons. It selects the video and the audio streams
alongside the vectors, so it needs an `.mkv` destination and writes a
copy of the film to get kilobytes of rows out of it, with the vectors
as WebVTT cues that ffmpeg then has to extract. And it reads
`f.audio[1]`, so on a source with no audio track it stops before it
starts: `STREAM_NOT_FOUND: 'f.audio[1]' does not exist`. Splitting it
into its columns keeps the destination small and lets a silent source
through with the halves that apply to it.

**2. `rows-from-describe`.** Seconds become milliseconds, and each file
becomes a space declared the way section 3 asks. That naming is the
part worth a program: the clip vectors come from X-CLIP's video tower
and a search of them is embedded by the text tower, which is a
different file of the same repository, and the URIs and digests come
out of the package's own `ffrwd.json`.

**3. weave.** The video stream comes out as Annex B, the records go in
before each picture's first slice, and the result is muxed back with
the audio copied. The audio's own bytes are checksummed before and
after.

**4. `index`.** Section 8's copy of the messages in a top-level `uuid`
box on the end of the MP4. Nothing else in the file moves.

**5. Embed the prompt.** [`embed_clip_text.sql`](embed_clip_text.sql)
and [`embed_text.sql`](embed_text.sql) call the same functions
`recipes/find.sql` ranks with, with the cut it feeds replaced by a
destination that keeps the vector.

**6. search.** Once against the file, once against the rows.

## What it printed

Run on a 60-second clip with cuts, on 2026-09-19, with ffrwd 0.17.3,
`ffrwd/describe` 0.1.2 and ffmpeg 9.0.1. The output below is verbatim
except that the absolute path of `out/` is shortened.

```
$ ./run.sh E:/projects/chunkyseal-spike/clips/film_dark.mp4 'a car driving at night'
source:  /e/projects/chunkyseal-spike/clips/film_dark.mp4
codec:   h264 at 24000/1001 (23.976024 fps)
audio:   none
prompt:  a car driving at night

== 1. describe: vectors out of the picture, and of the sound ==
the source has no audio track, so the sound and speech spaces are skipped
13 out/clip.ndjson

== 2. rows-from-describe: the spaces those vectors are in ==
out/rows.ndjson: 13 vectors in 1 spaces (space 1: 13)

== 3. weave: the vectors into the pictures, audio untouched ==
out/woven.h264: 1438 access units, 29 carrying 13 records in 1 spaces, 15203 bytes added

== 4. index: section 8's copy, so a search is one read ==
out/indexed.mp4: scan all: 1438 of 1438 samples, 403159 of 44286234 bytes read (0.91% of the file), 1443 seeks, 14 entries in 7199 bytes of index
out/indexed.mp4: the index box was appended at byte 44286234
```

Thirteen shots, 15203 bytes of records on a 44 MB file: 0.034% of it.
Twenty-nine carriers for thirteen records, because the space
declaration goes on every keyframe as well.

The space the converter wrote, which is what makes the file searchable
by somebody who did not write it:

```json
{"space":{"id":1,"dims":512,"encoding":"i8","unit_length":false,"modality":"picture","source":0,
  "model":"hf:imbcmdth/xclip-onnx@649f3c91b59cd24be316dc505e26eacf5cd00801/video_tower.onnx",
  "model_hash":"87a87b51ae52efa549e6e48f3a3b41f4",
  "query":"hf:imbcmdth/xclip-onnx@649f3c91b59cd24be316dc505e26eacf5cd00801/text_tower.onnx",
  "query_hash":"c14df0fdda1ba530330e278b8fabeb0b","producer":"ffrwd/describe 0.1.2"}}
```

(one line in the file; wrapped here.)

### The search, and the same search without the encoding

```
== 6. search the file, and the same query over the original vectors ==
--- out of the woven file, through the file index ---
{"rank":1,"score":0.247323,"start_t":41.167,"end_t":43.419,"space":1,"record_id":8,"planes":[0,1,2,3,4,5,6,7]}
{"rank":2,"score":0.243554,"start_t":31.449,"end_t":38.122,"space":1,"record_id":6,"planes":[0,1,2,3,4,5,6,7]}
{"rank":3,"score":0.240273,"start_t":46.338,"end_t":53.596,"space":1,"record_id":10,"planes":[0,1,2,3,4,5,6,7]}
{"rank":4,"score":0.239000,"start_t":23.315,"end_t":25.276,"space":1,"record_id":4,"planes":[0,1,2,3,4,5,6,7]}
{"rank":5,"score":0.236784,"start_t":-0.083,"end_t":3.838,"space":1,"record_id":0,"planes":[0,1,2,3,4,5,6,7]}
out/indexed.mp4: the file's own index, 11363 bytes read in 6 seeks, taken at its word: a cut or a join since it was built would leave it wrong, and `ffrwd-index index` rebuilds it
space 1: 13 of 13 records scored, 5 printed
space 1: a query for this space is embedded by hf:imbcmdth/xclip-onnx@649f3c91b59cd24be316dc505e26eacf5cd00801/text_tower.onnx
--- the same query over the binary32 the rows hold ---
{"rank":1,"score":0.247439,"start_t":41.250,"end_t":43.502,"space":1,"record_id":8}
{"rank":2,"score":0.243536,"start_t":31.532,"end_t":38.205,"space":1,"record_id":6}
{"rank":3,"score":0.240230,"start_t":46.421,"end_t":53.679,"space":1,"record_id":10}
{"rank":4,"score":0.238564,"start_t":23.398,"end_t":25.359,"space":1,"record_id":4}
{"rank":5,"score":0.236714,"start_t":0.000,"end_t":3.921,"space":1,"record_id":0}
out/rows.ndjson: 13 rows read as the binary32 they hold, with no encoding applied
space 1: 13 of 13 records scored, 5 printed
```

Same five records, same order. The scores differ in the fourth decimal:
0.247323 against 0.247439, 0.243554 against 0.243536. That is eight
bit-planes and two escapes over a 512-d X-CLIP vector, which
[MEASUREMENTS.md](../../MEASUREMENTS.md) measures at recall@1 0.991
across a hundred prompts and 29,150 vectors.

The spans differ by 83 milliseconds, which is two frames at 23.976 fps.
The rows are on the source's clock; the file's spans are offsets from
the picture they ride on, read back against the container's own
presentation times, and an encode with B-frames does not show its
pictures in the order it codes them. `tool/README.md` has the longer
version under "Which clock a span is on". Record 0 lands at -0.083
because its span starts two frames before the first picture the
container shows.

The file index costs 11363 bytes in 6 seeks. A keyframe scan of the
same file for the same answer is 42963 bytes in 37 seeks, and a full
scan, which this file does not need, is 403159 in 1443.

### The coarse stage, doing exactly what it was measured to do

```
$ ffrwd-index search --mp4 out/indexed.mp4 --space xclip --query out/query_clip.json --top 3 --coarse 4
{"rank":1,"score":0.243554,"start_t":31.449,"end_t":38.122,"space":1,"record_id":6,"planes":[0,1,2,3,4,5,6,7]}
{"rank":2,"score":0.236784,"start_t":-0.083,"end_t":3.838,"space":1,"record_id":0,"planes":[0,1,2,3,4,5,6,7]}
{"rank":3,"score":0.216503,"start_t":15.850,"end_t":23.274,"space":1,"record_id":3,"planes":[0,1,2,3,4,5,6,7]}
space 1: 13 of 13 records scored, 4 of them rescored from a plane 0 prefilter, 3 printed
```

The best record is gone. That is the one case MEASUREMENTS.md warns
about: a text prompt against X-CLIP video vectors is where the sign
plane is a poor first stage, because the component that dominates every
one of those vectors has the same sign in all of them and its bit
carries nothing. Rescoring cannot bring back what was never gathered.
`--coarse` is off by default for this reason.

## The sound and speech spaces

None of the local clips carries an audio track, so the run above has
one space in it. The other two were exercised on a copy of the same
clip with a generated noise track, which is enough to make AST and
Whisper produce something for the sentence embedder to embed:

```
ffmpeg -i film_dark.mp4 -f lavfi -i "anoisesrc=color=pink:sample_rate=48000:amplitude=0.3" \
  -shortest -c:v copy -c:a aac -ac 2 toned.mp4
./run.sh toned.mp4 'a car driving at night'
```

```
audio:   1
   13 out/clip.ndjson
    1 out/sound.ndjson
    1 out/speech.ndjson

out/rows.ndjson: 15 vectors in 3 spaces (space 1: 13, space 2: 1, space 3: 1)
out/woven.h264: 1438 access units, 29 carrying 15 records in 3 spaces, 29172 bytes added
audio: MD5=3c23bcbc2b61151212283234701d8e70 in, MD5=3c23bcbc2b61151212283234701d8e70 out
out/indexed.mp4: 18 entries in 8478 bytes of index

--- the sound space, out of the woven file ---
{"rank":1,"score":0.111256,"start_t":-0.083,"end_t":9.917,"space":3,"record_id":0,"planes":[0,1,2,3,4,5,6,7]}
space 3: 1 of 15 records scored, 1 printed
space 3: a query for this space is embedded by hf:Xenova/all-MiniLM-L6-v2@751bff37182d3f1213fa05d7196b954e230abad9/onnx/model.onnx
--- the sound space, over the binary32 the rows hold ---
{"rank":1,"score":0.111757,"start_t":0.000,"end_t":10.000,"space":3,"record_id":0}
```

The audio's checksum is the same before and after, which is what
"untouched" has to mean. Space 2 is speech and space 3 is sound: the
same sentence embedder and the same 384 components, kept apart because
a SPACE message carries one modality and a reader that wanted to search
only what was said could not tell them apart if they shared an id.

## What released ffrwd could not do

**The prompt's vector cannot go to a table destination.** Step 5 writes
an `.mkv` and pulls the vector back out with ffmpeg, because
`ffrwd` 0.17.3 refuses a compile-time vector anywhere else:

```
UNSUPPORTED_SQL: '.../q.json' is json, and a vector track is Matroska's:
no other container keeps its vector_dims tag, so the rows could not be
read back (hint: write the file as .mkv, or drop the vector column)
```

and for an `.ndjson` destination:

```
UNSUPPORTED_SQL: '.../query_clip.ndjson' is a rows file, and this query
writes 1 columns to it (hint: a rows file holds one module's annotation
column and nothing else)
```

A vector column that a module produced does reach an `.ndjson`, which
is what steps 1 to 3 rely on; a vector the compiler computed does not.
So the script writes the prompt to Matroska, runs
`ffmpeg -map 0:s:0 -f webvtt`, and turns the cue's base64 into
`{"vector":"..."}`. `search --query` reads base64 of little-endian
binary32 as one of its two spellings, so the cue needs no decoding.

The rows form is not a way around it either:

```
UDF_ARG_TYPE: ffrwd.describe.embed_clip() reads a module's rows, and its
argument is a compile-time cue array
```

**Weaving inside a query is not runnable.** The point of all of this is
that `ffrwd.index.weave` should be a function in the same query that
describes the file, with no elementary stream round trip and no second
pass. That needs the packets-in, packets-out interface, which lives on
ffrwd's `packet-filter` branch and is not in 0.17.3. The root
[README](../../README.md) has the query it would be. Until then the
weave is this script's three ffmpeg-and-tool steps.

**A silent source cannot run the stock recipe.** `describe.sql` reads
`f.audio[1]`, and none of the local clips has one.
