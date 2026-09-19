-- ffrwd/index: embedding vectors put into a video's own encoded
-- packets, and read back out of them.
--
-- Three modules. `weave` is a packet filter and writes; `records` and
-- `spaces` are packet sinks and read. All three are `ffrwd:av@0.16.0`,
-- which is unreleased, so none of them runs under a released ffrwd.

-- ---------------------------------------------------------------- --
-- Writing.
-- ---------------------------------------------------------------- --

-- weave: the vectors handed to it woven into `v`'s own encoded
-- packets, as SEI messages in H.264 and HEVC and a metadata OBU in
-- AV1. The same stream comes back, packet for packet, with the same
-- timestamps: a player that has never heard of the format plays it
-- unchanged.
--
--   COPY (
--     SELECT weave(f.video[1],
--                  ffrwd.describe.clips(f.video[1]).shots,
--                  NULL,
--                  NULL,
--                  '[{"name":"clip","dims":512,"modality":"picture"}]')
--     FROM input('in.mp4') f
--   ) TO 'out.mp4';
--
-- THE ROWS. `clip`, `sound` and `speech` are rows arguments, one per
-- producer: an encoder stands between a producer and this filter, so
-- the rows cannot ride the frames and each argument is written at the
-- call and read as an input of its own. A producer's rows are spans
-- and vectors and say nothing about embedding spaces, which is why
-- the argument's own name is what names the space: the host writes
-- `"_arg": "<argument>"` onto every row it delivers, and a row with
-- no `space` field of its own is put in the space that name declares.
-- A run declaring one space takes rows that name neither. An argument
-- written NULL hands the filter no rows at all.
--
-- The three are named for ffrwd/describe's three spaces, which is the
-- producer this package was written against. A DECLARATION IS FIXED
-- ARITY, so a producer with other spaces, or more of them, writes its
-- own CREATE FUNCTION over the same wasm file, naming its arguments
-- after its own spaces. That is a query's to write and not a
-- package's: a package's lib file may only name modules it ships, so
-- no other package can declare a function over this one's
-- weave.wasm.
--
-- THE VALUES. `spaces` is the module's space table, as the JSON text
-- of an array of objects, because a wasm function's value arguments
-- in this dialect are text, number, boolean or vector and an array of
-- objects is none of them. A name in it is what a row's `space`, or
-- its argument, names. `placement`, `budget`, `escapes` and `planes`
-- are the module's own defaults when they are left NULL. README.md
-- has the whole schema.
--
-- A run's rows say what was woven where, and SPEC.md section 8's file
-- index is built from them by a later step: a module has no
-- filesystem and runs before the muxer, so the index is not this
-- function's to write. A live destination gets no index and needs
-- none.
CREATE FUNCTION weave(v video_stream,
                      clip   STRUCT(start_t number, end_t number,
                                    vector vector)[] DEFAULT NULL,
                      sound  STRUCT(start_t number, end_t number,
                                    vector vector)[] DEFAULT NULL,
                      speech STRUCT(start_t number, end_t number,
                                    vector vector)[] DEFAULT NULL,
                      spaces text,
                      placement text DEFAULT NULL,
                      budget number DEFAULT NULL,
                      escapes number DEFAULT NULL,
                      planes number DEFAULT NULL)
RETURNS packets
  AS 'target/wasm32-wasip2/release/weave.wasm', 'weave' LANGUAGE wasm;

-- ---------------------------------------------------------------- --
-- Reading.
-- ---------------------------------------------------------------- --

-- records: one row per record in a woven stream. Which space it is in,
-- the span it describes in seconds of the stream's own presentation
-- clock, and the vector itself, rebuilt from whatever planes arrived
-- and not normalized.
--
-- spaces: one row per embedding space the stream declares. What the
-- vectors are, how many components they have, which model made them
-- and which model turns a search into the same space. `name` is a
-- label this sink derives, not a field of the format.
--
-- Both are packet sinks, so both are COPY destinations and their rows
-- ride the hosting sidecar's own stdout:
--
--   COPY (SELECT f.video[1] FROM input('woven.mp4') f) TO records();
--   COPY (SELECT f.video[1] FROM input('woven.mp4') f) TO spaces();
--
-- That is what runs today. What these exist for is the other thing:
-- reading a woven file at COMPILE time, so that `f.embeddings` is a
-- relation a query can join and filter. Nothing in the dialect spells
-- a packet sink whose rows are a compile-time relation yet; that is
-- being built separately, and these two are what it will run.
--
-- Each says in its meta how much of a stream it has to be handed:
-- `records` asks for `keyframes`, since section 7's `keyframe` policy
-- puts every record of a file on a sync sample, and `spaces` asks for
-- `first`, since section 3 puts every space declaration on every
-- keyframe from the first keyframe of the stream. A host may hand over
-- more than was asked for and never less, and both read whatever they
-- get: a space a writer learned of after the stream began rides its
-- own first carrier, and a host handing over more is what finds it.
CREATE FUNCTION records(v video_stream)
RETURNS sink
  AS 'target/wasm32-wasip2/release/records.wasm', 'records' LANGUAGE wasm;

CREATE FUNCTION spaces(v video_stream)
RETURNS sink
  AS 'target/wasm32-wasip2/release/spaces.wasm', 'spaces' LANGUAGE wasm;
