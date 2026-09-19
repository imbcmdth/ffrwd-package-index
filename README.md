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

Status: the format, its codec and the command line tool are done. The `weave`
module is done and tested against a real sidecar, and is **not yet runnable
from a query**: the interface it is built on, `ffrwd:av@0.16.0`'s
`packet-filter`, is unreleased, and no part of the dialect places a packet
filter in a query yet. See [Using it from a query](#using-it-from-a-query).

## Layout

- `SPEC.md`: the format.
- `core/`: the codec, plain Rust with no I/O and no dependencies: messages,
  the layered 8-bit encoding, SEI and OBU wrapping, the placement policies,
  and the file index.
- `rows/`: the JSON both callers of the codec speak, and what a writer does
  with it: reading a space and a vector, and deciding which access unit a
  record rides. Plain Rust over `core`, tested natively.
- `tool/`: a native command line over `core` and `rows`, for weaving vectors
  into a file and reading them back without ffrwd.
- `weave/`: the ffrwd module, a `wasm32-wasip2` cdylib and a thin one.
- `ffrwd.json`, `src/index.sql`: the package.
- `notes/`: what placing a packet filter in a query would take.

Why `rows/` exists, and why it is not part of `core`: the tool and the module
have to agree on how a row spells a space and a vector, because a row written
for one means the same thing to the other, and a field renamed in one and not
the other is a bug nobody sees until a file comes back empty. But the format
does not care: `core` is the wire, and giving it an opinion about JSON would
make every change to a row name a change to the codec. So the shapes live in
one crate over `core` that both compile in. The same crate holds the weaving
state machine, for the same reason turned around: it is the module's whole
decision, and it belongs somewhere `cargo test` can reach it on the native
target rather than inside a wasm cdylib.

## The module

One video pad, `h264`, `hevc` or `av1`, packets in and packets out, with rows
of vectors arriving beside them. What it adds to a packet is one SEI NAL or
metadata OBU before the first coded slice of the access units the placement
policy chose. Nothing else moves: the pictures, the packet count, the
timestamps and the keyframe flags are the encoder's own, and the stream's
out-of-band header is handed back unchanged.

### Params

One JSON object, through `init` or `set-params`.

```json
{
  "spaces": [
    {
      "name": "clip",
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
    }
  ],
  "placement": "keyframe",
  "escapes": 2
}
```

| param | meaning |
| --- | --- |
| `spaces` | the embedding spaces this run carries, 1 to 256. A row names one by its `name`; the wire's `space_id` is the position in this list |
| `placement` | `keyframe` (the default), `next` or `spread` |
| `budget` | bytes of messages per access unit. Belongs to `spread`, and `spread` needs one |
| `escapes` | how many of a vector's largest components an `i8` space sends exactly rather than quantized. 2 by default, 16 at most |
| `planes` | how many of an `i8` record's eight bit-planes are sent at all, 1 to 8. All eight by default |

A space's fields are section 3's, and they are the fields `tool/`'s own rows
spell, read by the same code:

| field | meaning |
| --- | --- |
| `name` | what the vector rows call this space |
| `dims` | components per vector, 1 to 65536 |
| `encoding` | `i8` (the default), `f16` or `f32` |
| `unit_length` | whether the vectors were unit length before they were encoded. Default false |
| `modality` | `unspecified`, `picture`, `sound`, `speech`, `sound-text`, `scene-text`, `description`, or a number for one this version does not name |
| `source` | 0 for the stream itself, `n` for the n-th audio stream |
| `model` | a URI for what made the vectors |
| `model_hash` | the first sixteen bytes of the SHA-256 of the weights, as hex; empty for unknown |
| `query` | a URI for what embeds a query into the same space; empty means the same as `model` |
| `query_hash` | as `model_hash` |
| `producer` | free text naming where the vectors came from |

Everything but `name` and `dims` may be left out. A param or a space field this
module does not know is refused rather than ignored, so a misspelled
`placement` is heard about rather than silently defaulted.

`set-params` between calls may change `escapes` and `planes`, which are what
future records cost. It may not change `spaces` or `placement`: a record
already submitted was built against those, and a half-written record cannot
change its mind about how many planes it has. A refused change leaves the
previous params in force, as the interface requires.

### Rows in

One JSON object per vector, in ffrwd's own row spelling: times are `start_t`
and `end_t`, in seconds, the way a cue row, an embedding row and every
`ffrwd/describe` row spell them.

```json
{"space": "clip", "start_t": 0.0, "end_t": 2.0, "vector": [0.12, -0.03, ...]}
{"space": "clip", "start_t": 2.0, "end_t": 4.0, "vector": "AACAPwAAAMA=", "available_t": 4.2}
```

| field | meaning |
| --- | --- |
| `space` | which declared space this vector is in, by name. May be left out when exactly one space is declared |
| `start_t`, `end_t` | the span it describes, in seconds on the stream's own presentation clock |
| `vector` | `dims` numbers, as a JSON array or as base64 of little-endian binary32 - the form ffrwd's vector tracks carry, so a writer copying rows out of a file has them already |
| `record_id` | optional; counts up per space from 0, wrapping at 65536 |
| `available_t` | optional; when the writer had the record, in seconds. Defaults to the newest presentation time any packet has reached, which is what makes a live row's span land behind its carrier |

Rows arrive whenever the host has them, which is not when their packets do.
Both extremes work and neither is a special case: a file hands over every row
before the first packet moves, and a live feed hands over a vector only after
the span it describes has ended. The placement policy is what decides where
each one goes, and it reads presentation time, so a record that arrives behind
the stream rides a carrier ahead of its span with negative offsets - which is
exactly what section 4 of the spec says offsets are for.

A row this module cannot read is dropped and reported, one row out per row
dropped. Nothing about a bad row stops the stream, the packets, or the rows
after it.

### Rows out

One row per record put into the stream, one per record nothing could carry,
one per row that could not be read, and one summary at the end.

```json
{"event":"woven","space":"clip","record_id":0,"carrier_pts":55296,"carrier_t":1.08,"start_t":0,"end_t":1,"start_off_ms":-1080,"end_off_ms":-80,"bytes":531,"planes":[0,1,2,3,4,5,6,7]}
{"event":"dropped","reason":"a vector of 4 components in a space of 512","row":"{\"space\":\"clip\",..."}
{"event":"late","space":"clip","record_id":7,"start_t":3,"end_t":4,"reason":"no carrier this placement would choose was left"}
{"event":"summary","records":12,"late":1,"dropped":2,"bytes_added":6372,"spaces":1}
```

| field | on | meaning |
| --- | --- | --- |
| `space`, `record_id` | woven, late | which record |
| `carrier_pts` | woven | the access unit that carried it, in the stream's own time base |
| `carrier_t` | woven | the same, in seconds |
| `start_t`, `end_t` | woven, late | the span, as the carrier and the offsets that went out say it |
| `start_off_ms`, `end_off_ms` | woven | the offsets themselves, which are what the wire carries |
| `bytes` | woven | this record's messages on that carrier, before the NAL or OBU framing around them |
| `planes` | woven, `i8` | which bit-planes rode that carrier |
| `reason`, `row` | dropped, late | why, and enough of the row to find who wrote it |
| `records`, `late`, `dropped`, `bytes_added`, `spaces` | summary | records woven, records nothing carried, rows that could not be read, every byte the packets grew by (framing included), and spaces declared |

A record goes out whole under `keyframe` and `next`, so there is one `woven`
row per record. Under `spread` a record is doled out plane by plane over
several carriers, and there is one `woven` row per carrier it used; the summary
counts the record once.

**This module writes no index.** SPEC.md section 8's file index is a copy of
the messages at the container level, and a module cannot write one: it has no
filesystem, and it runs before the muxer, so the file it would sit in does not
exist yet. The `woven` rows are what a later step builds one from - they name
every record, its carrier and its span - and that step is a separate piece
that reads a finished file and writes a `uuid` box or a Matroska attachment.
[notes/packet-filter-placement.md](notes/packet-filter-placement.md) section 3
is what it would take.

### Decode order, and what is held

Packets arrive in decode order. The placement policies speak of presentation
time, so something has to hold a packet until its place among its neighbours is
settled, and this is how: `dts` never decreases and no packet is presented
before it is decoded, so once a packet with `dts = T` has arrived, nothing
still to come can be presented before `T`. Every held packet whose `pts` is at
or below the newest `dts` is therefore settled, in ascending `pts`, and that is
exact - no guess at a reorder depth, which the interface does not carry anyway.
Packets are released in the order they arrived, which is the order they have to
leave in, and the hold is one reorder depth deep: four packets for a
three-frame reorder.

One packet is always held back until the final call. The final call carries no
packets of its own, so a record that no carrier the policy would choose ever
came along for would have nothing left to ride; this is the one access unit
that is always still there. It costs one call of latency and no more.

### What is bounded, and what happens at the cap

- **Pending records: 4096.** A row arriving when that many records are already
  waiting for a carrier is dropped, with a `dropped` row saying so. This is
  what bounds a file run, where every row arrives before the first packet.
- **Held packets: 256.** Past that, the packet earliest in presentation order
  is settled whether or not its order was, one at a time, until the front of
  the hold can be released. A stream whose timestamps settle normally never
  reaches it; a stream whose `dts` the wire never carries is what it is for.
- **Units: 4096 bytes.** A carrier with more messages than that gets more than
  one unit, each in its own NAL or OBU, which section 7 lets a reader accept.

## Using it from a query

**Not yet runnable.** What the declaration wants to say, from
[notes/packet-filter-placement.md](notes/packet-filter-placement.md):

```sql
CREATE FUNCTION weave(v video_stream, vecs STRUCT(vector vector, t number)[])
  RETURNS packets
  AS 'target/wasm32-wasip2/release/weave.wasm', 'weave' LANGUAGE wasm;

COPY (SELECT weave(f.video[1], embed(f.video[1]).vectors))
  FROM input('in.mp4') f TO 'out.mp4';

COPY (SELECT weave(f.video[1], embed(f.video[1]).vectors))
  FROM input('in.mp4') f TO publish('relay', 'live');
```

`RETURNS packets` would be a new `wrtype` beside `sink`, for a call that hands
back the stream it was given, still encoded, deferred past the encoder the
COPY's destination already places. The grammar delta is one alternative:
`wrtype := wstype | sink | packets | STRUCT(...)`. The note has the planner
changes and the refusals.

None of that exists today, so `RETURNS packets` does not parse, and
[src/index.sql](src/index.sql) declares `video_stream` instead. The compiler
then loads the module, recognises it as a packet filter and refuses it by name,
which is the most useful thing it can do:

```
the module 'target/wasm32-wasip2/release/weave.wasm' is a packet filter,
and no part of a query places one yet
```

What does work today: `ffrwd list ffrwd/index` reads the manifest and lists the
export, and the sidecar's `--describe` reports the module, its schemas, its
codecs and its arity. `ffrwd link` refuses the package, because it declares a
dependency on `ffrwd/wasm` 0.16.0 and no such version is published; that is the
same unreleased world the module is built against, and both land together.

## Building and testing

`core`, `rows` and `tool` are ordinary Rust and need nothing:

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

The module needs the `ffrwd:av` wit. `ffrwd:av@0.16.0` is in no released
`ffrwd/wasm`, so point `FFRWD_WIT_DIR` at the `sidecar/wit` of an ffrwd
checkout that carries the interface; without it the build asks
`ffrwd path ffrwd/wasm` for an installed copy, the way the moq package's does.

```
FFRWD_WIT_DIR=/path/to/ffrwd-cli/sidecar/wit \
  cargo build --release --target wasm32-wasip2 -p weave
FFRWD_WIT_DIR=/path/to/ffrwd-cli/sidecar/wit cargo test -p weave
```

The end-to-end tests in `tool/tests/weave.rs` drive real ffmpeg through the
real sidecar. `FFRWD_WASM` names the sidecar binary built from that same
branch, and every one of them skips with a message saying so when it is
absent:

```
FFRWD_WASM=/path/to/ffrwd-wasm \
FFRWD_WIT_DIR=/path/to/ffrwd-cli/sidecar/wit \
  cargo test -p ffrwd-index --test weave
```

`FFRWD_INDEX_WASM` names a `weave.wasm` already built, which skips the build.

## License

Apache-2.0.
