#!/usr/bin/env bash
# Pull still frames out of a slow-motion recording so each can be read (by eye or by Claude).
# usage: frames.sh recording.mov [out_dir] [fps_to_extract]
# The extraction rate should match the recording's real frame rate (240 for iPhone slo-mo).
set -euo pipefail
in=${1:?recording}; out=${2:-frames}; fps=${3:-240}
mkdir -p "$out"
ffmpeg -loglevel error -i "$in" -vf "fps=$fps,scale=1280:-2" -q:v 3 "$out/f_%05d.jpg"
ls "$out" | wc -l
