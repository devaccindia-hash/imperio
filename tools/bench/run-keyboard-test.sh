#!/usr/bin/env bash
# Types into a host on this computer through a weak connection and checks, in
# the host's log, that every key press and release arrived once and in order
# and that no key was left held down. No key is really pressed: the host runs
# with IMPERIO_INPUT_LOG_ONLY=1. Throwaway profile.
#
#   bash tools/bench/run-keyboard-test.sh <name> <presses> <old|new> [bad-link options]
#   bash tools/bench/run-keyboard-test.sh loss10 300 new --loss 10 --delay-ms 20
#
# "old" sends the way versions up to 0.3.17 did (one datagram per event).
# Run from the imperio/ directory after `cargo build --release`.
set -u
NAME="$1"; COUNT="$2"; MODE="$3"; shift 3
R=./target/release
OUT=bench-results
PROFILE="$(pwd)/$OUT/keys-profile"
HOSTPORT=7797
PROXYPORT=7798
mkdir -p "$PROFILE/data" "$PROFILE/appdata"
export IMPERIO_DATA_DIR="$(cygpath -w "$PROFILE/data")"
export APPDATA="$(cygpath -w "$PROFILE/appdata")"
{ echo 'device_id = 123456789'; echo "port = $HOSTPORT"; echo 'send_audio = false'; echo 'clipboard = false'; echo 'file_access = false';
  grep -m1 '^identity_key' "$PROFILE/data/config.toml" 2>/dev/null; } > "$PROFILE/data/config.new"
mv "$PROFILE/data/config.new" "$PROFILE/data/config.toml"
strip() { sed 's/\x1b\[[0-9;]*m//g' "$1" | tr -d '\r'; }

IMPERIO_INPUT_LOG_ONLY=1 $R/imperio-host.exe --no-ui --code 424242 > "$OUT/keys-$NAME-host.log" 2>&1 &
HOST=$!
$R/imperio-bench.exe bad-link $PROXYPORT 127.0.0.1:$HOSTPORT "$@" > "$OUT/keys-$NAME-proxy.log" 2>&1 &
PROXY=$!
sleep 3
LEGACY=""; [ "$MODE" = "old" ] && LEGACY="--legacy"
$R/imperio-bench.exe probe-keys 127.0.0.1:$PROXYPORT 424242 "$COUNT" $LEGACY > "$OUT/keys-$NAME-probe.log" 2>&1
sleep 2
# By process, never by name: the owner's own Imperio may be running.
kill $PROXY $HOST 2>/dev/null
sleep 1

echo "=== $NAME ($MODE): $COUNT presses, $* ==="
grep -E "typing|confirmed|did not" "$OUT/keys-$NAME-probe.log"
strip "$OUT/keys-$NAME-host.log" | perl -e '
  my $count = shift;
  my @want; for my $i (0..$count-1) { push @want, (2 + $i % 10)."d", (2 + $i % 10)."u"; }
  my (@got, $stuck);
  while (<STDIN>) {
    push @got, $1.($2 eq "true" ? "d" : "u") if /input \(log only\).*scancode: (\d+),.*down: (true|false)/;
    $stuck = $1 if /released what the viewer left held down keys=(\d+)/;
  }
  my $same = "@got" eq "@want";
  my %seen; my $pos = 0; my $missing = 0;
  # Events of the expected sequence that never arrived (in order).
  for my $w (@want) { if ($pos < @got && $got[$pos] eq $w) { $pos++ } else { $missing++ } }
  printf "host received %d of %d key events; %s; keys left held down when the session ended: %s\n",
    scalar(@got), scalar(@want),
    $same ? "all there, once each, in order" : "NOT complete (about $missing missing or out of order)",
    $stuck // 0;
  exit($same && !$stuck ? 0 : 1);
' "$COUNT"
