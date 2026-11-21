#!/usr/bin/env bash
# End-to-end scenarios on one machine, against a throwaway profile: nothing in
# %ProgramData%\Imperio or %APPDATA%\Imperio is read or changed.
#
# Run from the imperio/ directory after `cargo build --release` (and, for the
# signal-server scenarios, `cargo build --release` in ../imperio-cloud/signal).
# Windows opens briefly for each viewer; the mouse pointer moves once.
set -u
R=./target/release
S=../imperio-cloud/signal/target/release
OUT=bench-results
PROFILE="$(pwd)/$OUT/e2e-profile"
PORT=7791
SIGPORT=7891
mkdir -p "$OUT"
rm -rf "$PROFILE"
mkdir -p "$PROFILE/data" "$PROFILE/appdata"
export IMPERIO_DATA_DIR="$(cygpath -w "$PROFILE/data")"
export APPDATA="$(cygpath -w "$PROFILE/appdata")"
CFG="$PROFILE/data/config.toml"
PASS=0
FAIL=0

strip() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }
kill_all() { for p in imperio-viewer imperio-host imperio-signal imperio-bench; do taskkill //IM $p.exe //F >/dev/null 2>&1; done; sleep 1; }
check() { # check <description> <file> <regex> [absent]
  if strip "$2" | tr -d '\r' | grep -qE "$3"; then found=1; else found=0; fi
  if { [ "${4:-}" = "absent" ] && [ $found = 0 ]; } || { [ "${4:-}" != "absent" ] && [ $found = 1 ]; }; then
    echo "  PASS  $1"; PASS=$((PASS+1))
  else
    echo "  FAIL  $1   (see $2)"; FAIL=$((FAIL+1))
  fi
}
write_cfg() { # write_cfg <extra toml lines...>; keeps device id + identity stable
  { echo 'device_id = 123456789'; echo "port = $PORT"; echo 'send_audio = false'; echo 'clipboard = false';
    [ -n "${IDENTITY:-}" ] && echo "identity_key = \"$IDENTITY\"";
    for l in "$@"; do echo "$l"; done; } > "$CFG"
}
host() { ($R/imperio-host.exe --no-ui "$@" > "$OUT/e2e-$NAME-host.log" 2>&1 &); sleep 3; }
viewer() { timeout "${T:-8}" $R/imperio-viewer.exe "$@" --no-input > "$OUT/e2e-$NAME-viewer.log" 2>&1; }

kill_all
write_cfg

echo "##### 1. a normal session streams, and the identity is pinned #####"
NAME=1a; host --code 424242; viewer 127.0.0.1:$PORT --code 424242; kill_all
check "frames arrive" $OUT/e2e-1a-viewer.log "viewer.*fps"
check "identity pinned on first use" $OUT/e2e-1a-viewer.log "identity pinned"
IDENTITY=$(grep -m1 '^identity_key' "$CFG" | sed 's/.*"\(.*\)".*/\1/')
[ -n "$IDENTITY" ] && { echo "  PASS  identity key written to the config on first run"; PASS=$((PASS+1)); } || { echo "  FAIL  no identity key in config"; FAIL=$((FAIL+1)); }
NAME=1b; host --code 424242; viewer 127.0.0.1:$PORT --code 424242; kill_all
check "same identity verified on reconnect (host restarted)" $OUT/e2e-1b-viewer.log "identity verified"

echo "##### 2. wrong code: refused quickly, with a reason, and no stream #####"
NAME=2; host --code 424242
# (The command-line viewer then re-opens its connect dialog; `timeout` ends it.)
T=9 viewer 127.0.0.1:$PORT --code 000000; kill_all
check "viewer explains the code was not accepted" $OUT/e2e-2-viewer.log "code was not accepted"
check "no stream reached the viewer" $OUT/e2e-2-viewer.log "stream info|viewer.*fps" absent

echo "##### 3. a different computer answering under a pinned ID is refused #####"
SAVED_IDENTITY=$IDENTITY; IDENTITY=""; write_cfg
NAME=3a; host --code 424242; viewer 127.0.0.1:$PORT --code 424242; kill_all
check "changed identity refused with an explanation" $OUT/e2e-3a-viewer.log "not the same computer"
check "no stream from the impostor" $OUT/e2e-3a-viewer.log "viewer.*fps" absent
IDENTITY=$(grep -m1 '^identity_key' "$CFG" | sed 's/.*"\(.*\)".*/\1/')
NAME=3b; host --code 424242; viewer 127.0.0.1:$PORT --code 424242 --trust-new; kill_all
check "--trust-new accepts the new identity" $OUT/e2e-3b-viewer.log "identity pinned"

echo "##### 4. second factor (TOTP) #####"
# base32("12345678901234567890")
SECRET="GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
write_cfg 'password = "secretpw"' "totp_secret = \"$SECRET\""
CODE=$($R/imperio-bench.exe totp "$SECRET" 2>/dev/null)
NAME=4a; host; viewer 127.0.0.1:$PORT --code secretpw --totp "$CODE"; kill_all
check "correct TOTP streams" $OUT/e2e-4a-viewer.log "viewer.*fps"
NAME=4b; host; viewer 127.0.0.1:$PORT --code secretpw --totp 000000; kill_all
check "wrong TOTP: viewer told why" $OUT/e2e-4b-viewer.log "two-step code was wrong"
check "wrong TOTP: nothing streamed" $OUT/e2e-4b-host.log "streaming" absent
NAME=4c; host; T=14 viewer 127.0.0.1:$PORT --code secretpw; kill_all
check "missing TOTP: viewer told one is needed" $OUT/e2e-4c-viewer.log "two-step"
check "missing TOTP: nothing streamed" $OUT/e2e-4c-host.log "streaming" absent

echo "##### 5. no remote input before every check has passed #####"
# The probe knows the password but sends no TOTP, then tries to move the mouse.
NAME=5a; host; $R/imperio-bench.exe probe-input 127.0.0.1:$PORT secretpw > $OUT/e2e-5a-probe.log 2>&1; kill_all
check "mouse did NOT move for a half-authenticated peer" $OUT/e2e-5a-probe.log "FAIL"
write_cfg
NAME=5b; host --code 424242; $R/imperio-bench.exe probe-input 127.0.0.1:$PORT 424242 > $OUT/e2e-5b-probe.log 2>&1; kill_all
check "mouse moves once fully authenticated" $OUT/e2e-5b-probe.log "PASS"

echo "##### 9. file transfer over the encrypted link #####"
FILES="$PROFILE/files"
mkdir -p "$FILES/src/tree/sub/deeper" "$FILES/there" "$FILES/back"
head -c 40000000 /dev/urandom > "$FILES/src/tree/big.bin"
head -c 700001 /dev/urandom > "$FILES/src/tree/sub/mid.bin"
echo "hello" > "$FILES/src/tree/sub/deeper/note.txt"
: > "$FILES/src/tree/empty.dat"
head -c 3000000 /dev/urandom > "$FILES/there/fetch-me.bin"
W() { cygpath -w "$1"; }
write_cfg
NAME=9a; host --code 424242
T=14 viewer 127.0.0.1:$PORT --code 424242 --push "$(W "$FILES/src/tree")" "$(W "$FILES/there")" --pull "$(W "$FILES/there/fetch-me.bin")" "$(W "$FILES/back")"; kill_all
check "the other computer says files are allowed" $OUT/e2e-9a-viewer.log "file access: the other computer answered.*allowed=true"
check "a folder was sent (4 files)" $OUT/e2e-9a-viewer.log "file transfer finished.*files=4 .*error=\"?none"
check "a file was fetched" $OUT/e2e-9a-viewer.log "file transfer finished.*files=1 .*error=\"?none"
check "the picture arrives in the same session" $OUT/e2e-9a-viewer.log "viewer.*fps"
if diff -r "$FILES/src/tree" "$FILES/there/tree" >/dev/null 2>&1 && cmp -s "$FILES/there/fetch-me.bin" "$FILES/back/fetch-me.bin"; then
  echo "  PASS  what arrived is identical to what was sent"; PASS=$((PASS+1))
else
  echo "  FAIL  what arrived differs from what was sent"; FAIL=$((FAIL+1))
fi
if find "$FILES" -name '*.imperio-part' | grep -q .; then
  echo "  FAIL  a half-written file was left behind"; FAIL=$((FAIL+1))
else
  echo "  PASS  no half-written file is left"; PASS=$((PASS+1))
fi
NAME=9b; host --code 424242 --no-files
T=9 viewer 127.0.0.1:$PORT --code 424242 --push "$(W "$FILES/src/tree/sub")" "$(W "$FILES/back")" --pull "$(W "$FILES/there/fetch-me.bin")" "$(W "$FILES/src")"; kill_all
check "file access off: the viewer is told" $OUT/e2e-9b-viewer.log "file access: the other computer answered.*allowed=false"
check "file access off: sending is refused" $OUT/e2e-9b-viewer.log "file transfer finished.*turned off"
if [ ! -e "$FILES/back/sub" ] && [ ! -e "$FILES/src/fetch-me.bin" ]; then
  echo "  PASS  file access off: nothing was written or read"; PASS=$((PASS+1))
else
  echo "  FAIL  file access off: files moved anyway"; FAIL=$((FAIL+1))
fi

echo "##### 10. copied files cross between the two clipboards #####"
# Uses the real Windows clipboard; text that was on it is put back afterwards.
PSH() { powershell -NoProfile -Command "$1" 2>/dev/null | tr -d '\r'; }
CLIPSAVE="$(PSH 'Get-Clipboard -Raw')"
NAME=10a; host --code 424242
T=9 viewer 127.0.0.1:$PORT --code 424242 --push "$(W "$FILES/src/tree/sub")" "::clipboard"
PSH '(Get-Clipboard -Format FileDropList).FullName' > $OUT/e2e-10a-clip.txt; kill_all
check "copy here, paste there: the files reach the other clipboard" $OUT/e2e-10a-host.log "copied files are on the clipboard"
check "that clipboard points at a staging folder" $OUT/e2e-10a-clip.txt 'Imperio-clipboard.*sub$'
STAGED="$(cygpath -u "$(head -1 $OUT/e2e-10a-clip.txt)" 2>/dev/null)"
if [ -n "$STAGED" ] && diff -r "$FILES/src/tree/sub" "$STAGED" >/dev/null 2>&1; then
  echo "  PASS  the staged files are identical"; PASS=$((PASS+1))
else
  echo "  FAIL  the staged files differ or are missing"; FAIL=$((FAIL+1))
fi
NAME=10b; host --code 424242
(T=13 viewer 127.0.0.1:$PORT --code 424242 &)
sleep 6; PSH "Set-Clipboard -Path '$(W "$FILES/there/fetch-me.bin")'"; sleep 4
PSH '(Get-Clipboard -Format FileDropList).FullName' > $OUT/e2e-10b-clip.txt; sleep 4; kill_all
check "copy there: the viewer fetches the files" $OUT/e2e-10b-viewer.log "fetching the files copied on the other computer"
check "this clipboard points at the fetched copy" $OUT/e2e-10b-clip.txt 'Imperio-clipboard.*fetch-me.bin$'
FETCHED="$(cygpath -u "$(head -1 $OUT/e2e-10b-clip.txt)" 2>/dev/null)"
if [ -n "$FETCHED" ] && cmp -s "$FILES/there/fetch-me.bin" "$FETCHED"; then
  echo "  PASS  the fetched file is identical"; PASS=$((PASS+1))
else
  echo "  FAIL  the fetched file differs or is missing"; FAIL=$((FAIL+1))
fi
N=$(strip $OUT/e2e-10b-viewer.log | grep -c "fetching the files copied")
[ "$N" = 1 ] && { echo "  PASS  no echo between the clipboards (fetched once)"; PASS=$((PASS+1)); } || { echo "  FAIL  fetched $N times"; FAIL=$((FAIL+1)); }
if [ -n "$CLIPSAVE" ]; then printf '%s' "$CLIPSAVE" | clip.exe; else PSH 'Set-Clipboard -Value $null'; fi

if [ -x "$S/imperio-signal.exe" ]; then
  BIND="$PROFILE/bindings.txt"
  echo "##### 6. signal server: connect by Device ID (direct path) #####"
  ($S/imperio-signal.exe --port $SIGPORT --data "$BIND" > $OUT/e2e-6-signal.log 2>&1 &); sleep 1
  NAME=6; host --code 424242 --server 127.0.0.1:$SIGPORT; sleep 2
  T=10 viewer --id 123456789 --server 127.0.0.1:$SIGPORT --code 424242; kill_all
  check "viewer streams via the server" $OUT/e2e-6-viewer.log "viewer.*fps"
  check "ID bound to the key on the server" "$BIND" "^123456789 [0-9a-f]{64}$"

  echo "##### 7. signal server: forced relay #####"
  ($S/imperio-signal.exe --port $SIGPORT --data "$BIND" > $OUT/e2e-7-signal.log 2>&1 &); sleep 1
  NAME=7; host --code 424242 --server 127.0.0.1:$SIGPORT --force-relay; sleep 2
  T=12 viewer --id 123456789 --server 127.0.0.1:$SIGPORT --code 424242 --force-relay; kill_all
  check "viewer streams through the relay" $OUT/e2e-7-viewer.log "viewer.*fps"
  check "server relayed traffic" $OUT/e2e-7-signal.log "relay bound"

  echo "##### 8. signal server: another key cannot take a bound ID #####"
  ($S/imperio-signal.exe --port $SIGPORT --data "$BIND" > $OUT/e2e-8-signal.log 2>&1 &); sleep 1
  IDENTITY=""; write_cfg
  NAME=8; host --code 424242 --server 127.0.0.1:$SIGPORT; sleep 3; kill_all
  check "server refuses the impostor (ID offline, server restarted)" $OUT/e2e-8-signal.log "bound to a different key"
else
  echo "(signal server not built; skipping scenarios 6-8)"
fi

echo
echo "passed $PASS, failed $FAIL"
[ $FAIL = 0 ]
