-- The sound half: AST labels every ten-second window of the audio, and
-- the sentence embedder puts each label in MiniLM's space, which is the
-- space a text prompt is embedded into by embed_text.
--
-- variables: src (the video), dest (an .ndjson)
COPY (
  SELECT ffrwd.describe.embed(ffrwd.describe.sounds(f.audio[1]).labels) AS sound_vectors
  FROM input(:'src') f
) TO :'dest'
