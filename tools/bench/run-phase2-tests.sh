#!/usr/bin/env bash
# Phase 2 scenario tests on one machine. Run from the imperio/ directory after
# `cargo build --release` (and the signal server in ../imperio-cloud/signal).
set -u
R=./target/release
S=../imperio-cloud/signal/target/release
OUT=bench-results
mkdir -p "$OUT"
# Throwaway profile: never touch the real %ProgramData%\Imperio config.
PROFILE="$(pwd)/$OUT/phase2-profile"
mkdir -p "$PROFILE/data" "$PROFILE/appdata"
export IMPERIO_DATA_DIR="$(cygpath -w "$PROFILE/data")"
export APPDATA="$(cygpath -w "$PROFILE/appdata")"
strip_ansi() { sed 's/\x1b\[[0-9;]*m//g' "$1" | grep -vE '^\s*$'; }
killall_imperio() { for p in imperio-viewer imperio-host imperio-signal imperio-bench; do taskkill //IM $p.exe //F >/dev/null 2>&1; done; sleep 1; }
CODE=123456


killall_imperio
echo "##### 1. input probe through the encrypted session (host idle) #####"
($R/imperio-host.exe --port 7700 --code $CODE > $OUT/p2-1-host.log 2>&1 &)
sleep 3
ID=$(grep -m1 '^device_id' "$PROFILE/data/config.toml" 2>/dev/null | grep -oE '[0-9]+')
$R/imperio-bench.exe probe-input 127.0.0.1:7700 $CODE
sleep 1
killall_imperio

echo "##### 2. 5% packet loss both ways via lossy proxy (viewer -> :7701 -> host :7700) #####"
($R/imperio-host.exe --port 7700 --code $CODE > $OUT/p2-2-host.log 2>&1 &)
($R/imperio-bench.exe lossy-proxy 7701 127.0.0.1:7700 5 > $OUT/p2-2-proxy.log 2>&1 &)
sleep 3
(RUST_LOG=info,imperio_transport::jitter=debug $R/imperio-viewer.exe 127.0.0.1:7701 --code $CODE --no-input > $OUT/p2-2-viewer.log 2>&1 &)
sleep 16
killall_imperio
echo "--- proxy ---"; tail -n 2 $OUT/p2-2-proxy.log
echo "--- host ---"; strip_ansi $OUT/p2-2-host.log | grep -E "stream|bitrate|authenticated" | tail -n 5
echo "--- viewer ---"; strip_ansi $OUT/p2-2-viewer.log | grep -E "link|viewer fps|handshake" | tail -n 6
echo "--- dropped frames (first 8) ---"; strip_ansi $OUT/p2-2-viewer.log | grep "missed deadline" | head -n 8

echo "##### 3. signal server: device ID $ID, hole punch (direct path expected) #####"
($S/imperio-signal.exe --port 7800 > $OUT/p2-3-signal.log 2>&1 &)
sleep 1
($R/imperio-host.exe --port 7700 --code $CODE --server 127.0.0.1:7800 > $OUT/p2-3-host.log 2>&1 &)
sleep 4
($R/imperio-viewer.exe --id $ID --server 127.0.0.1:7800 --code $CODE --no-input > $OUT/p2-3-viewer.log 2>&1 &)
sleep 10
killall_imperio
echo "--- signal ---"; strip_ansi $OUT/p2-3-signal.log | tail -n 4
echo "--- host ---"; strip_ansi $OUT/p2-3-host.log | grep -E "registered|incoming|direct|relay|link up|authenticated|stream" | tail -n 6
echo "--- viewer ---"; strip_ansi $OUT/p2-3-viewer.log | grep -E "peer info|direct|relay|handshake|viewer fps|link" | tail -n 6

echo "##### 4. signal server, forced relay (server forwards ciphertext) #####"
($S/imperio-signal.exe --port 7800 > $OUT/p2-4-signal.log 2>&1 &)
sleep 1
($R/imperio-host.exe --port 7700 --code $CODE --server 127.0.0.1:7800 --force-relay > $OUT/p2-4-host.log 2>&1 &)
sleep 4
($R/imperio-viewer.exe --id $ID --server 127.0.0.1:7800 --code $CODE --no-input --force-relay > $OUT/p2-4-viewer.log 2>&1 &)
sleep 12
killall_imperio
echo "--- signal ---"; strip_ansi $OUT/p2-4-signal.log | tail -n 4
echo "--- host ---"; strip_ansi $OUT/p2-4-host.log | grep -E "relay|link up|authenticated|stream" | tail -n 5
echo "--- viewer ---"; strip_ansi $OUT/p2-4-viewer.log | grep -E "relay|handshake|viewer fps|link" | tail -n 6

echo "##### 5. wrong code must be rejected #####"
($R/imperio-host.exe --port 7700 --code $CODE > $OUT/p2-5-host.log 2>&1 &)
sleep 3
timeout 12 $R/imperio-viewer.exe 127.0.0.1:7700 --code 000000 --no-input > $OUT/p2-5-viewer.log 2>&1
sleep 6
killall_imperio
echo "--- host ---"; strip_ansi $OUT/p2-5-host.log | grep -E "Hello|wrong|link up" | tail -n 3
echo "--- viewer ---"; strip_ansi $OUT/p2-5-viewer.log | grep -E "stream info|viewer fps" | wc -l | xargs -I{} echo "viewer received {} stream/frame log lines (expected 0)"
echo "done"
