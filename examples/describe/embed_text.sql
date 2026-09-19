-- One prompt through the sentence embedder, which is what find.sql's
-- other branch ranks the sound and speech rows with. Same shape as
-- embed_clip_text.sql, and the other of the two spaces.
--
-- variables: src (any media file), prompt (the search text), dest (an .mkv)
COPY (
  SELECT ARRAY[
    STRUCT(0 AS start_t, 1 AS end_t,
           ffrwd.describe.embed_text(:'prompt') AS vector)::embedding
  ] AS query_vector
  FROM input(:'src') f
) TO :'dest'
