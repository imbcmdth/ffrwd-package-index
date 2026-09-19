-- The speech half: voice activity detection, then Whisper, then the
-- same sentence embedder over the transcript. Same model and same
-- dimensionality as the sound vectors, and a different modality, which
-- is why the rows go into a space of their own.
--
-- variables: src (the video), dest (an .ndjson)
COPY (
  SELECT ffrwd.describe.embed(ffrwd.whisper.transcribe(ffrwd.vad.speech(f.audio[1])).words) AS speech_vectors
  FROM input(:'src') f
) TO :'dest'
