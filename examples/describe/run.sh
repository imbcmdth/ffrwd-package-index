#!/bin/sh
# One video through ffrwd/describe, into its own stream, and back out
# again as a search.
#
#   ./run.sh SOURCE [PROMPT]
#
# Everything lands in examples/describe/out/, which is ignored. Nothing
# is downloaded: the models are whatever `ffrwd` already has cached for
# the describe package.
#
# The steps, and what each one is for, are in README.md. In short:
# describe writes vectors, rows-from-describe names their spaces, weave
# puts them in the pictures, index puts a copy at the container level,
# and search ranks them against a prompt the package's own text tower
# embedded.
set -eu

usage() {
  echo "usage: $0 SOURCE [PROMPT]" >&2
  echo >&2
  echo "  FFRWD_DESCRIBE  the ffrwd/describe checkout (default ../ffrwd-package-describe" >&2
  echo "                  beside this repository)" >&2
  echo "  FFRWD_INDEX     the ffrwd-index binary (default the repository's debug build)" >&2
  exit 2
}

[ $# -ge 1 ] || usage
[ -f "$1" ] || { echo "$1 is not there" >&2; exit 2; }
# Absolute, because every ffrwd run below happens in the describe
# checkout's directory and a relative path would mean something else
# there.
SRC=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
PROMPT=${2:-a car driving at night}

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT="$HERE/out"
PKG=${FFRWD_DESCRIBE:-$(cd "$ROOT/.." && pwd)/ffrwd-package-describe}
INDEX=${FFRWD_INDEX:-$ROOT/target/debug/ffrwd-index}

for needed in ffrwd ffmpeg ffprobe; do
  command -v "$needed" >/dev/null 2>&1 || { echo "$needed is not on the PATH" >&2; exit 2; }
done
[ -f "$PKG/ffrwd.json" ] || { echo "$PKG is not an ffrwd/describe checkout; set FFRWD_DESCRIBE" >&2; exit 2; }
[ -x "$INDEX" ] || { echo "$INDEX is not there; run cargo build, or set FFRWD_INDEX" >&2; exit 2; }

mkdir -p "$OUT"
say() { printf '\n== %s ==\n' "$1"; }

# `ffrwd` resolves ffrwd/describe as a project package, which means every
# run of one of its functions happens with that checkout as the working
# directory.
run_sql() {
  sql=$1
  shift
  ( cd "$PKG" && ffrwd run -f "$HERE/$sql" "$@" -y -q )
}

probe() {
  ffprobe -v error -select_streams "$1" -show_entries stream="$2" -of csv=p=0 "$3" | head -1
}

CODEC=$(probe v:0 codec_name "$SRC")
RATE=$(probe v:0 r_frame_rate "$SRC")
FPS=$(printf '%s' "$RATE" | awk -F/ '{ printf "%.6f", ($2 ? $1/$2 : $1) }')
HAS_AUDIO=$(ffprobe -v error -select_streams a -show_entries stream=index -of csv=p=0 "$SRC" | head -1)

case $CODEC in
  h264) BSF=h264_mp4toannexb; FORMAT=h264; STREAM=$OUT/video.h264 ;;
  hevc) BSF=hevc_mp4toannexb; FORMAT=hevc; STREAM=$OUT/video.h265 ;;
  av1)  BSF=;                 FORMAT=obu;  STREAM=$OUT/video.obu  ;;
  *) echo "$SRC is $CODEC, and this format has carriage for h264, hevc and av1" >&2; exit 2 ;;
esac

echo "source:  $SRC"
echo "codec:   $CODEC at $RATE ($FPS fps)"
echo "audio:   ${HAS_AUDIO:-none}"
echo "prompt:  $PROMPT"

# ---------------------------------------------------------------- #
say "1. describe: vectors out of the picture, and of the sound"
# ---------------------------------------------------------------- #
run_sql clip_vectors.sql -v "src=$SRC" -v "dest=$OUT/clip.ndjson"
CONVERT="--clip $OUT/clip.ndjson"
if [ -n "$HAS_AUDIO" ]; then
  run_sql sound_vectors.sql  -v "src=$SRC" -v "dest=$OUT/sound.ndjson"
  run_sql speech_vectors.sql -v "src=$SRC" -v "dest=$OUT/speech.ndjson"
  CONVERT="$CONVERT --sound $OUT/sound.ndjson --speech $OUT/speech.ndjson"
else
  echo "the source has no audio track, so the sound and speech spaces are skipped"
fi
wc -l "$OUT"/*.ndjson

# ---------------------------------------------------------------- #
say "2. rows-from-describe: the spaces those vectors are in"
# ---------------------------------------------------------------- #
# shellcheck disable=SC2086
"$INDEX" rows-from-describe $CONVERT --package "$PKG/ffrwd.json" --out "$OUT/rows.ndjson"

# ---------------------------------------------------------------- #
say "3. weave: the vectors into the pictures, audio untouched"
# ---------------------------------------------------------------- #
if [ -n "$BSF" ]; then
  ffmpeg -hide_banner -nostdin -y -loglevel error \
    -i "$SRC" -map 0:v:0 -c copy -bsf:v "$BSF" -f "$FORMAT" "$STREAM"
else
  ffmpeg -hide_banner -nostdin -y -loglevel error \
    -i "$SRC" -map 0:v:0 -c copy -f "$FORMAT" "$STREAM"
fi
WOVEN="$OUT/woven.${STREAM##*.}"
"$INDEX" weave --video "$STREAM" --vectors "$OUT/rows.ndjson" --out "$WOVEN" --fps "$FPS"

# The picture comes from the woven stream and everything else from the
# source, copied: no audio sample is re-encoded or moved.
#
# An elementary stream carries no timestamps, so -r gives it a frame
# rate and +genpts turns that into the presentation times the muxer
# wants. Without them every video packet reaches the muxer with no pts,
# which mp4 warns about at a level -loglevel error hides and which
# costs the audio track outright.
if [ -n "$HAS_AUDIO" ]; then
  ffmpeg -hide_banner -nostdin -y -loglevel error \
    -fflags +genpts -r "$RATE" -i "$WOVEN" -i "$SRC" \
    -map 0:v:0 -map 1:a -c copy "$OUT/indexed.mp4"
else
  ffmpeg -hide_banner -nostdin -y -loglevel error \
    -fflags +genpts -r "$RATE" -i "$WOVEN" -map 0:v:0 -c copy "$OUT/indexed.mp4"
fi

# The audio is the source's own bytes, which is what untouched has to
# mean and what a checksum can say out loud.
if [ -n "$HAS_AUDIO" ]; then
  before=$(ffmpeg -hide_banner -nostdin -loglevel error -i "$SRC" -map 0:a -c copy -f md5 -)
  after=$(ffmpeg -hide_banner -nostdin -loglevel error -i "$OUT/indexed.mp4" -map 0:a -c copy -f md5 -)
  echo "audio: $before in, $after out"
  [ "$before" = "$after" ] || { echo "the mux changed the audio" >&2; exit 1; }
fi

# ---------------------------------------------------------------- #
say "4. index: section 8's copy, so a search is one read"
# ---------------------------------------------------------------- #
"$INDEX" index "$OUT/indexed.mp4"

# ---------------------------------------------------------------- #
say "5. embed the prompt with the package's own text tower"
# ---------------------------------------------------------------- #
# A compile-time vector goes to Matroska and nowhere else in released
# ffrwd, so the prompt's vector comes back as a WebVTT cue. Its text is
# base64 of little-endian binary32, which is one of the two spellings
# `search --query` reads, so the cue becomes a query file as it stands.
embed_prompt() {
  run_sql "$1" -v "src=$SRC" -v "prompt=$PROMPT" -v "dest=$OUT/$2.mkv"
  ffmpeg -hide_banner -nostdin -y -loglevel error \
    -i "$OUT/$2.mkv" -map 0:s:0 -f webvtt "$OUT/$2.vtt"
  awk '/-->/ { getline payload; printf "{\"vector\":\"%s\"}\n", payload; exit }' \
    "$OUT/$2.vtt" > "$OUT/$2.json"
  [ -s "$OUT/$2.json" ] || { echo "the prompt's vector track held no cue" >&2; exit 1; }
}
embed_prompt embed_clip_text.sql query_clip
if [ -n "$HAS_AUDIO" ]; then
  embed_prompt embed_text.sql query_text
fi

# ---------------------------------------------------------------- #
say "6. search the file, and the same query over the original vectors"
# ---------------------------------------------------------------- #
echo "--- out of the woven file, through the file index ---"
"$INDEX" search --mp4 "$OUT/indexed.mp4" --space xclip --query "$OUT/query_clip.json" --top 5
echo "--- the same query over the binary32 the rows hold ---"
"$INDEX" search --rows "$OUT/rows.ndjson" --space xclip --query "$OUT/query_clip.json" --top 5

if [ -n "$HAS_AUDIO" ]; then
  echo "--- the sound space, out of the woven file ---"
  "$INDEX" search --mp4 "$OUT/indexed.mp4" --space 3 --query "$OUT/query_text.json" --top 5
  echo "--- the sound space, over the binary32 the rows hold ---"
  "$INDEX" search --rows "$OUT/rows.ndjson" --space 3 --query "$OUT/query_text.json" --top 5
fi

say "done"
echo "everything is in $OUT"
