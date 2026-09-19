-- One prompt through X-CLIP's text tower, which is the network that
-- puts a search in the same space as the video tower's vectors. It is
-- the call recipes/find.sql makes in its clip branch, with the cut it
-- feeds replaced by a destination that keeps the vector itself.
--
-- The vector is wrapped in an `embedding` the way a vector track's rows
-- are, and the destination is Matroska rather than the .ndjson the
-- describe queries write to. Released ffrwd 0.17.3 will not put a
-- compile-time vector in a table destination: "a vector track is
-- Matroska's: no other container keeps its vector_dims tag, so the rows
-- could not be read back". So the prompt's vector comes back as a
-- WebVTT cue and run.sh turns that base64 into a query file.
--
-- The input is only there because a SELECT needs a FROM. Nothing of it
-- reaches the destination, and the span is a placeholder nothing reads.
--
-- variables: src (any media file), prompt (the search text), dest (an .mkv)
COPY (
  SELECT ARRAY[
    STRUCT(0 AS start_t, 1 AS end_t,
           ffrwd.describe.embed_clip_text(:'prompt') AS vector)::embedding
  ] AS query_vector
  FROM input(:'src') f
) TO :'dest'
