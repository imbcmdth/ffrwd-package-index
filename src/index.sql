-- ffrwd/index: embedding vectors put into a video's own encoded
-- packets, and read back out of them.
--
-- Three modules. `weave` writes; `records` and `spaces` read. All three
-- are nodes, `ffrwd:av@0.19.1`, which ffrwd hosts from 0.29 on, so that
-- is the floor.

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
--                  clip => ffrwd.describe.clips(f.video[1],
--                                               ffrwd.shots.simple_detector(f.video[1])),
--                  spaces => '[{"name":"clip","dims":512,"modality":"picture"}]')
--     FROM input('in.mp4') f
--   ) TO 'out.mp4';
--
-- THE ROWS. The module has an input of rows per space its `spaces`
-- param declares, named for the space, so the space a row is in is the
-- input the call bound it to: a producer's rows are spans and vectors
-- and say nothing about embedding spaces. A row that names a `space`
-- of its own goes there instead. Each input pairs its rows with the
-- packets by time, waiting for its producer's progress, so a record
-- reaches the module before the packets of its span leave it.
--
-- `clip`, `sound` and `speech` are the inputs ffrwd/describe's three
-- spaces make, each `DEFAULT NULL`: a call binds the ones it has a
-- producer for, by name, and leaves the rest off. An input declared
-- here that the call's `spaces` has no space for is fine left unbound.
-- A producer with other spaces writes its own CREATE FUNCTION over the
-- same wasm file, naming its inputs after its own spaces.
--
-- THE VALUES. `spaces` is the module's space table, as the JSON text
-- of an array of objects, because a wasm function's value arguments
-- in this dialect are text, number, boolean or vector and an array of
-- objects is none of them. A name in it is what a row's `space`, or
-- its input, names. `placement`, `budget`, `escapes` and `planes`
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
-- Both are sinks: packets in, rows alone out. Declared over a stream
-- with an array of records for its RETURNS, as they are here, each is
-- a FROM item: the
-- compiler stream-copies that one stream of the file into the module
-- while the query compiles and binds what the module wrote as a row
-- table, so a search is joins and predicates over rows that exist
-- before ffmpeg is started.
--
--   SELECT v.start_t, v.end_t
--   FROM input('woven.mp4') f, records(f.video[1]) v
--        JOIN spaces(f.video[1]) s ON v.space = s.space
--   WHERE s.modality = 'picture'
--
-- WHICH SPACE IS WHICH, AND NOT BY ID. A record carries a space id and
-- nothing else, and an id is a position in one writer's space table:
-- `weave` hands them out in the order its `spaces` param declares
-- them, FROM ZERO, and the `ffrwd-index` tool's own rows hand out
-- whatever the rows say. A file outlives the run that wrote it, so a
-- consumer joins `spaces` on `v.space = s.space` and selects on
-- `modality`, or on `model`, or on `dims`: those are fields of the
-- format and mean the same thing in every file. A query that hard-codes
-- `v.space = 1` is reading a position it did not write.
--
-- Each says in its shape how much of a stream it has to be handed:
-- `records` asks for `keyframes`, since section 7's `keyframe` policy
-- puts every record of a file on a sync sample, and `spaces` asks for
-- `first`, since section 3 puts every space declaration on every
-- keyframe from the first keyframe of the stream. A host may hand over
-- more than was asked for and never less, and both read whatever they
-- get: a space a writer learned of after the stream began rides its
-- own first carrier, and a host handing over more is what finds it.
--
-- Each column below is one the module writes, checked against its own
-- rows_schema when the call compiles. `planes` is an array, and the
-- one array type the dialect has is `vector`: it is which of the eight
-- bit-planes arrived for an i8 record, not an embedding, and it is
-- absent for the float encodings.
CREATE FUNCTION records(v video_stream)
RETURNS STRUCT(index number, space number, record_id number,
               start_t number, end_t number, planes vector,
               vector vector)[]
  AS 'target/wasm32-wasip2/release/records.wasm', 'records' LANGUAGE wasm;

CREATE FUNCTION spaces(v video_stream)
RETURNS STRUCT(space number, name text, dims number, encoding text,
               unit_length boolean, modality text, source number,
               model text, model_hash text, query text,
               query_hash text, producer text)[]
  AS 'target/wasm32-wasip2/release/spaces.wasm', 'spaces' LANGUAGE wasm;
