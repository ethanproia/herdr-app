#!/bin/sh
# Test videos for the browser parity tests (tests/fixtures/browser/video.html?src=<name>):
# 5 s, 1280x720, a moving test pattern with a frame counter, VP9/WebM and H.264/MP4 at 30 and
# 60 fps. Written to $HERDR_FORK_DIR/scratch/videos (default ~/work.noindex/herdr-fork), never git.
set -eu
dir=${HERDR_FORK_DIR:-$HOME/work.noindex/herdr-fork}/scratch/videos
mkdir -p "$dir"
for fps in 30 60; do
  src="testsrc2=size=1280x720:rate=$fps:duration=5"
  [ -s "$dir/test-$fps-vp9.webm" ] || ffmpeg -loglevel error -y -f lavfi -i "$src" -c:v libvpx-vp9 -b:v 1M -deadline realtime -cpu-used 8 "$dir/test-$fps-vp9.webm"
  [ -s "$dir/test-$fps-h264.mp4" ] || ffmpeg -loglevel error -y -f lavfi -i "$src" -c:v libx264 -preset veryfast -pix_fmt yuv420p -movflags +faststart "$dir/test-$fps-h264.mp4"
done
ls -l "$dir"
