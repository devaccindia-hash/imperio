#!/usr/bin/env bash
# Phase 5 security scenarios on one machine. Run from imperio/ after build.
set -u
R=./target/release
OUT=bench-results
mkdir -p "$OUT"
CFG="/c/ProgramData/Imperio/config.toml"
KH="$APPDATA/Imperio/known_hosts"
strip() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }
kill_all() { for p in imperio-host imperio-viewer; do taskkill //IM $p.exe //F >/dev/null 2>&1; done; sleep 1; }

kill_all
echo "##### 1. identity: first use pins, second use verifies #####"
rm -f "$KH"
($R/imperio-host.exe --port 7700 --code 424242 > $OUT/p5-1a-host.log 2>&1 &)
sleep 3
timeout 6 $R/imperio-viewer.exe 127.0.0.1:7700 --code 424242 --no-input > $OUT/p5-1a-viewer.log 2>&1
kill_all
($R/imperio-host.exe --port 7700 --code 424242 > $OUT/p5-1b-host.log 2>&1 &)
sleep 3
timeout 6 $R/imperio-viewer.exe 127.0.0.1:7700 --code 424242 --no-input > $OUT/p5-1b-viewer.log 2>&1
kill_all
echo "first connect:"; strip $OUT/p5-1a-viewer.log | grep -E "identity (pinned|verified)" | head -1
echo "second connect:"; strip $OUT/p5-1b-viewer.log | grep -E "identity (pinned|verified)" | head -1

echo "##### 2. identity: changed host key is refused #####"
# Force the host to regenerate its identity key.
python - "$CFG" <<'PY' 2>/dev/null || sed -i '/^identity_key/d' "$CFG"
import re,sys
p=sys.argv[1]
t=open(p).read()
t=re.sub(r'(?m)^identity_key.*$\n?','',t)
open(p,'w').write(t)
PY
($R/imperio-host.exe --port 7700 --code 424242 > $OUT/p5-2-host.log 2>&1 &)
sleep 3
timeout 6 $R/imperio-viewer.exe 127.0.0.1:7700 --code 424242 --no-input > $OUT/p5-2-viewer.log 2>&1
kill_all
echo "viewer reaction:"; strip $OUT/p5-2-viewer.log | grep -E "IDENTITY CHANGED|verified|pinned" | head -1
echo "  (then accept the new key with --trust-new)"
timeout 6 $R/imperio-viewer.exe 127.0.0.1:7700 --code 424242 --no-input --trust-new > $OUT/p5-2b-viewer.log 2>&1 &
($R/imperio-host.exe --port 7700 --code 424242 > $OUT/p5-2b-host.log 2>&1 &)
sleep 7
kill_all
strip $OUT/p5-2b-viewer.log | grep -E "pinned|verified" | head -1

echo "##### 3. TOTP second factor #####"
# base32("12345678901234567890") = GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ
SECRET="GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
python - "$CFG" "$SECRET" <<'PY' 2>/dev/null
import sys
p,sec=sys.argv[1],sys.argv[2]
lines=[l for l in open(p).read().splitlines() if not l.startswith("totp_secret") and not l.startswith("password")]
lines.append(f'password = "secretpw"')
lines.append(f'totp_secret = "{sec}"')
open(p,'w').write("\n".join(lines)+"\n")
PY
if ! grep -q totp_secret "$CFG"; then echo "  (python missing; skipping TOTP live test)"; else
CODE=$($R/imperio-bench.exe totp "$SECRET" 2>/dev/null)
echo "current TOTP = $CODE"
rm -f "$KH"
($R/imperio-host.exe --port 7700 > $OUT/p5-3-host.log 2>&1 &)
sleep 3
timeout 6 $R/imperio-viewer.exe 127.0.0.1:7700 --code secretpw --totp "$CODE" --no-input > $OUT/p5-3-ok.log 2>&1
kill_all
echo "correct TOTP -> host:"; strip $OUT/p5-3-host.log | grep -E "streaming|rejected" | head -1
($R/imperio-host.exe --port 7700 > $OUT/p5-3b-host.log 2>&1 &)
sleep 3
timeout 6 $R/imperio-viewer.exe 127.0.0.1:7700 --code secretpw --totp 000000 --no-input > $OUT/p5-3-bad.log 2>&1
kill_all
echo "wrong TOTP -> host:"; strip $OUT/p5-3b-host.log | grep -E "streaming|rejected|TOTP" | head -1
# restore config (drop totp + password)
python - "$CFG" <<'PY' 2>/dev/null
import sys
p=sys.argv[1]
lines=[l for l in open(p).read().splitlines() if not l.startswith("totp_secret") and not l.startswith("password")]
open(p,'w').write("\n".join(lines)+"\n")
PY
fi
echo done
