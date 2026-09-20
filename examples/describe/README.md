# What the encoding cost one real ranking

`ffrwd/describe` 0.2.0 runs this whole path in two commands, `describe` and
`find`:

```
ffrwd run ffrwd/describe -v src=film.mp4 -v dest=film.described.mkv
ffrwd run ffrwd/describe:find -v src=film.described.mkv \
  -v 'prompt=a car driving at night' -v threshold=0.25 -v dest=clips.mp4
```

There used to be a `run.sh` here that did the same thing in six steps, because
the ffrwd of the time could not put a compile-time vector anywhere but a
Matroska track and could not run a packet filter at all. Both limits are gone
and the script went with them.

What is worth keeping is the number it produced, which is the part the recipes
do not print.

## The measured comparison

Run on a 60-second clip with cuts on 2026-09-19: thirteen shots described by
X-CLIP's video tower, woven into the file's own pictures as 8-bit records with
two escapes, and ranked against a prompt the same package's text tower
embedded. The same query was then run over the binary32 the model produced,
with no encoding applied.

```
--- out of the woven file, through the file index ---
{"rank":1,"score":0.247323,"start_t":41.167,"end_t":43.419,"record_id":8}
{"rank":2,"score":0.243554,"start_t":31.449,"end_t":38.122,"record_id":6}
{"rank":3,"score":0.240273,"start_t":46.338,"end_t":53.596,"record_id":10}
{"rank":4,"score":0.239000,"start_t":23.315,"end_t":25.276,"record_id":4}
{"rank":5,"score":0.236784,"start_t":-0.083,"end_t":3.838,"record_id":0}
--- the same query over the binary32 the rows hold ---
{"rank":1,"score":0.247439,"start_t":41.250,"end_t":43.502,"record_id":8}
{"rank":2,"score":0.243536,"start_t":31.532,"end_t":38.205,"record_id":6}
{"rank":3,"score":0.240230,"start_t":46.421,"end_t":53.679,"record_id":10}
{"rank":4,"score":0.238564,"start_t":23.398,"end_t":25.359,"record_id":4}
{"rank":5,"score":0.236714,"start_t":0.000,"end_t":3.921,"record_id":0}
```

Same five records, same order, scores apart in the fourth decimal.
[MEASUREMENTS.md](../../MEASUREMENTS.md) puts that at recall@1 0.991 across a
hundred prompts and 29,150 vectors.

Thirteen records cost 15685 bytes on a 44 MB file, which is 0.035% of it.
Thirty keyframes carried them: twelve with a record, eighteen with nothing but
the space declaration that section 3 puts on every keyframe whether a record
rides there or not. Reading them back through the file index is 11364 bytes in
6 seeks; a keyframe scan for the same answer is 43731 bytes in 36 seeks, and a
full scan, which this file does not need, is 404183 in 1443.

The spans differ by 83 milliseconds, two frames at 23.976 fps. The rows are on
the source's clock; a file's spans are offsets from the picture they ride on,
read back against the container's own presentation times, and an encode with
B-frames does not show its pictures in the order it codes them.
[tool/README.md](../../tool/README.md) has the longer version under "Which
clock a span is on".

## Where a coarse first stage fails

```
$ ffrwd-index search --mp4 indexed.mp4 --space xclip --query q.json --top 3 --coarse 4
{"rank":1,"score":0.243554,"start_t":31.449,"end_t":38.122,"record_id":6}
{"rank":2,"score":0.236784,"start_t":-0.083,"end_t":3.838,"record_id":0}
{"rank":3,"score":0.216503,"start_t":15.850,"end_t":23.274,"record_id":3}
```

The best record is gone. MEASUREMENTS.md warns about exactly this case: against
X-CLIP video vectors, a text prompt is where the sign plane is a poor
prefilter, because the component that dominates every one of those vectors has
the same sign in all of them and its bit carries nothing. Rescoring cannot
bring back what was never gathered, which is why `--coarse` is off by default.
