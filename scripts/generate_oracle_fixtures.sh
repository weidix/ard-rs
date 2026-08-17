#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
fixture_dir="$repo_dir/crates/ard-core/examples/fixtures"
duration_seconds=5
display_fps=60
slice_count=4
coded_fps=$((display_fps * slice_count))
h264_tick_rate=$((coded_fps * 2))
display_frames=$((duration_seconds * display_fps))
coded_frames=$((display_frames * slice_count))
frame_generator=$(mktemp "${TMPDIR:-/tmp}/ard-fixture-frames.XXXXXX")
trap 'rm -f "$frame_generator"' EXIT HUP INT TERM

command -v cargo >/dev/null 2>&1 || {
    echo "cargo is required" >&2
    exit 1
}
command -v ffmpeg >/dev/null 2>&1 || {
    echo "ffmpeg is required" >&2
    exit 1
}
command -v ffprobe >/dev/null 2>&1 || {
    echo "ffprobe is required" >&2
    exit 1
}
command -v rustc >/dev/null 2>&1 || {
    echo "rustc is required" >&2
    exit 1
}

mkdir -p "$fixture_dir"

# Generate and decode-validate the two RFB rectangle payload streams.
cd "$repo_dir"
cargo run --quiet --release -p ard-core --example generate_oracle_mvs_zlib_fixtures -- \
    "$fixture_dir" "$duration_seconds"

# Generate the four-band raw source once and feed it to both VideoToolbox
# encoders. Duration is the source of truth; the native display cadence and
# four-band layout determine the number of emitted access units.
rustc --edition=2024 -C opt-level=3 \
    "$repo_dir/crates/ard-core/examples/generate_oracle_avc_hevc_frames.rs" \
    -o "$frame_generator"

"$frame_generator" "$duration_seconds" | ffmpeg -hide_banner -loglevel warning -y \
    -f rawvideo -pixel_format rgb24 -video_size 1920x272 -framerate "$coded_fps" \
    -t "$duration_seconds" -i - \
    -filter_complex \
    "[0:v]format=yuv420p,setparams=range=limited:color_primaries=bt709:color_trc=bt709:colorspace=bt709,split=2[avc][hevc]" \
    -map "[avc]" -an -c:v h264_videotoolbox -realtime 1 -prio_speed 1 \
    -profile:v high -level:v 4.2 -g "$coded_frames" -bf 0 -max_ref_frames 1 -b:v 32M \
    -bsf:v "h264_metadata=aud=insert:tick_rate=$h264_tick_rate:fixed_frame_rate_flag=0" \
    -color_range tv -colorspace bt709 -color_primaries bt709 -color_trc bt709 \
    -f h264 "$fixture_dir/oracle-diagonal-frames-1920x1080-4x272.h264" \
    -map "[hevc]" -an -c:v hevc_videotoolbox -realtime 1 -prio_speed 1 \
    -profile:v main -g "$coded_frames" -bf 0 -max_ref_frames 1 -b:v 32M \
    -bsf:v "hevc_metadata=aud=insert:tick_rate=$coded_fps:num_ticks_poc_diff_one=1" \
    -color_range tv -colorspace bt709 -color_primaries bt709 -color_trc bt709 \
    -f hevc "$fixture_dir/oracle-diagonal-frames-1920x1080-4x272.h265"

verify_video_fixture() {
    local file=$1
    local codec=$2
    local profile=$3
    local probe_rate=$4
    local packet_duration=$5
    local stream_info frame_info first_packet

    stream_info=$(ffprobe -v error -count_frames -select_streams v:0 \
        -show_entries stream=codec_name,profile,width,height,pix_fmt,color_range,color_space,r_frame_rate,nb_read_frames \
        -of default=noprint_wrappers=1 "$file")
    for expected in \
        "codec_name=$codec" "profile=$profile" width=1920 height=272 \
        pix_fmt=yuv420p color_range=tv color_space=bt709 \
        "r_frame_rate=$probe_rate/1" "nb_read_frames=$coded_frames"
    do
        grep -qx "$expected" <<<"$stream_info" || {
            echo "$file: missing expected probe value: $expected" >&2
            exit 1
        }
    done

    frame_info=$(ffprobe -v error -select_streams v:0 \
        -show_entries frame=key_frame,pict_type -of csv=p=0 "$file")
    [[ $(grep -c '^1,I' <<<"$frame_info") -eq 1 ]] || {
        echo "$file: expected exactly one initial random-access frame" >&2
        exit 1
    }
    ! grep -q ',B' <<<"$frame_info" || {
        echo "$file: B-frames are not valid for the oracle fixture" >&2
        exit 1
    }
    first_packet=$(ffprobe -v error -select_streams v:0 -read_intervals '%+#1' \
        -show_entries packet=duration_time -of default=noprint_wrappers=1:nokey=1 "$file")
    [[ "$first_packet" == "$packet_duration" ]] || {
        echo "$file: packet duration is $first_packet, expected $packet_duration" >&2
        exit 1
    }
    printf '%s\n' "$stream_info"
}

verify_video_fixture "$fixture_dir/oracle-diagonal-frames-1920x1080-4x272.h264" \
    h264 High "$h264_tick_rate" 0.004167
verify_video_fixture "$fixture_dir/oracle-diagonal-frames-1920x1080-4x272.h265" \
    hevc Main "$coded_fps" 0.004167

printf 'generated and validated AVC, HEVC, MVS, and zlib oracle fixtures in %s\n' \
    "$fixture_dir"
