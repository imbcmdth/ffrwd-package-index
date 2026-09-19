-- The picture half of ffrwd/describe:describe, to a table destination.
--
-- `clips` runs X-CLIP's video tower over each shot a cut detector found
-- and hands back one 512-d vector a shot. Selecting that column alone,
-- with no stream beside it, makes the destination a table rather than a
-- media file, so an .ndjson destination holds one JSON object a vector
-- and nothing else has to be unpacked to read them.
--
-- variables: src (the video), dest (an .ndjson)
COPY (
  SELECT ffrwd.describe.clips(ffrwd.shots.simple_detector(f.video[1])).shots AS clip_vectors
  FROM input(:'src') f
) TO :'dest'
