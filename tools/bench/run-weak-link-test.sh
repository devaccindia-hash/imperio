#!/usr/bin/env bash
# One session of this computer to itself through a weak connection (see
# `imperio-bench bad-link`), with a summary of how the picture and the delay
# held up. Throwaway profile; view only (no input reaches this computer).
#
#   bash tools/bench/run-weak-link-test.sh <name> <seconds> [bad-link options]
#   bash tools/bench/run-weak-link-test.sh 3mbit 30 --rate-kbps 3000 --queue-ms 300 --delay-ms 20
#
# Run from the imperio/ directory after `cargo build --release`. A viewer
# window is open for the length of the test.
set -u
NAME="$1"; SECS="$2"; shift 2
R=./target/release
OUT=bench-results
PROFILE="$(pwd)/$OUT/weak-profile"
HOSTPORT=7795
PROXYPORT=7796
mkdir -p "$PROFILE/data" "$PROFILE/appdata"
export IMPERIO_DATA_DIR="$(cygpath -w "$PROFILE/data")"
export APPDATA="$(cygpath -w "$PROFILE/appdata")"
{ echo 'device_id = 123456789'; echo "port = $HOSTPORT"; echo 'send_audio = false'; echo 'clipboard = false'; echo "file_access = ${FILE_ACCESS:-false}";
  grep -m1 '^identity_key' "$PROFILE/data/config.toml" 2>/dev/null; } > "$PROFILE/data/config.new"
mv "$PROFILE/data/config.new" "$PROFILE/data/config.toml"
strip() { sed 's/\x1b\[[0-9;]*m//g' "$1" | tr -d '\r'; }

${HOST_CMD:-$R/imperio-host.exe} --no-ui --code 424242 > "$OUT/weak-$NAME-host.log" 2>&1 &
HOST=$!
$R/imperio-bench.exe bad-link $PROXYPORT 127.0.0.1:$HOSTPORT "$@" > "$OUT/weak-$NAME-proxy.log" 2>&1 &
PROXY=$!
sleep 3
RUST_LOG=${RUST_LOG:-info} ${VIEWER_CMD:-$R/imperio-viewer.exe} 127.0.0.1:$PROXYPORT --code 424242 --no-input --trust-new ${VIEWER_EXTRA:-} > "$OUT/weak-$NAME-viewer.log" 2>&1 &
VIEWER=$!
sleep "$SECS"
# By process, never by name: the owner's own Imperio may be running.
kill $VIEWER $PROXY $HOST 2>/dev/null
sleep 1

echo "=== $NAME: $* ==="
head -1 "$OUT/weak-$NAME-proxy.log"
tail -n 2 "$OUT/weak-$NAME-proxy.log"
strip "$OUT/weak-$NAME-viewer.log" | perl -ne '
  if (/imperio_viewer: viewer fps=([\d.]+) rtt_ms=([\d.]+).*capture_to_present_ms=([\d.]+)/) { push @fps, $1; push @lat, $3; }
  if (/imperio_viewer: link .*lost=(\d+) recovered=(\d+).*queue_ms=([\d.]+) rtt_ms=([\d.]+) frames_dropped=(\d+) frames_discarded=(\d+)/) {
    push @rtt, $4; $lost=$1; $rec=$2; $dropped=$5; $disc=$6; }
  if (/timing .*longest_gap_ms=([\d.]+)/) { push @gap, $1; }
  END {
    @s = sort { $a <=> $b } @rtt; @g = sort { $a <=> $b } @gap; @f = sort { $a <=> $b } @fps;
    printf "viewer: seconds_with_picture %d  fps median %s  min %s\n", scalar(@fps), $f[$#f/2] // "-", $f[0] // "-";
    shift @lat; @l = sort { $a <=> $b } @lat;
    printf "screen change to shown ms (this includes the link delay): median %s  worst second %s\n", $l[$#l/2] // "-", $l[-1] // "-";
    printf "ping (rtt) ms: first %s  median %s  max %s  last %s\n", $rtt[0] // "-", $s[$#s/2] // "-", $s[-1] // "-", $rtt[-1] // "-";
    printf "longest freeze ms: median %s  max %s\n", $g[$#g/2] // "-", $g[-1] // "-";
    printf "datagrams lost %s  recovered %s  frames dropped %s  discarded waiting for a full picture %s\n", $lost // "-", $rec // "-", $dropped // "-", $disc // "-";
  }'
strip "$OUT/weak-$NAME-host.log" | perl -ne '
  if (/imperio_host: stream .*video_mbps=([\d.]+) target_kbps=(\d+) keyframes=(\d+)/) { $n++; $mb += $1; $keys += $3; $t = $2; $tmin = $2 if !defined $tmin || $2 < $tmin; }
  $adapt++ if /bitrate adapted/;
  END { printf "host: average %.2f Mbit/s  keyframes %d  bitrate changes %d  lowest target %s kbit/s  last target %s kbit/s\n", $mb/($n||1), $keys, $adapt, $tmin // "-", $t // "-"; }'
