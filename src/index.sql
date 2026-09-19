-- ffrwd/index: embedding vectors put into a video's own encoded
-- packets, and read back out of them.
--
-- Three modules. `weave` is a packet filter and writes; `records` and
-- `spaces` are packet sinks and read. All three are `ffrwd:av@0.16.0`,
-- which is unreleased, so none of them runs under a released ffrwd.

-- ---------------------------------------------------------------- --
-- Writing.
-- ---------------------------------------------------------------- --

-- weave: the vectors of `vecs` woven into `v`'s own encoded packets,
-- as SEI messages in H.264 and HEVC and a metadata OBU in AV1. The
-- same stream comes back, packet for packet, with the same timestamps:
-- a player that has never heard of the format plays it unchanged.
--
-- NOT RUNNABLE YET. The declaration below is the one the dialect
-- accepts, and a query that writes the call compiles as far as
-- checking it and is then refused, because nothing builds the shape a
-- packet filter sits in (encoder, filter, muxer). The refusal is by
-- name and says so. See ffrwd's own known_gaps.md.
--
-- What it will read, once a destination places one:
--
--   COPY (SELECT weave(f.video[1], embed(f.video[1]).vectors))
--     FROM input('in.mp4') f TO 'out.mp4';
--
--   COPY (SELECT weave(f.video[1], embed(f.video[1]).vectors))
--     FROM input('in.mp4') f TO publish('relay', 'live');
--
-- The rows say which space a vector is in, the span it describes in
-- seconds, and the vector. The module's own params (spaces, placement,
-- budget, escapes, planes) are one JSON object; README.md has the
-- schema. A run's rows say what was woven where, and SPEC.md section
-- 8's file index is built from them by a later step: a module has no
-- filesystem and runs before the muxer, so the index is not this
-- function's to write.
CREATE FUNCTION weave(v video_stream,
                      vecs STRUCT(space text, start_t number, end_t number,
                                  vector vector)[])
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
