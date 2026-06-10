#!/usr/bin/env bash
# End-to-end check: two apps, routing, conflict, idle self-exit.
set -u

cd "$(dirname "$0")/.."
export PORTPROXY_STATE_DIR=$(mktemp -d)
LISTEN_PORT=21356
printf 'listen = "127.0.0.1:%s"\nbase_domain = "e2e.test"\n' "$LISTEN_PORT" \
    > "$PORTPROXY_STATE_DIR/config.toml"

cargo build --quiet || exit 1
BIN=./target/debug/portproxy
fail=0

note() { printf '\n== %s\n' "$*"; }
bad()  { echo "FAIL: $*"; fail=1; }

note "start two apps"
$BIN app1 sh -c 'exec python3 -m http.server "$PORT" --bind 127.0.0.1' >/dev/null 2>&1 &
W1=$!
$BIN app2 sh -c 'exec python3 -m http.server "$PORT" --bind 127.0.0.1' >/dev/null 2>&1 &
W2=$!
sleep 3

curl -fsS -m 3 -H "Host: app1.e2e.test" "127.0.0.1:$LISTEN_PORT/" >/dev/null || bad "app1 not routed"
curl -fsS -m 3 -H "Host: app2.e2e.test" "127.0.0.1:$LISTEN_PORT/" >/dev/null || bad "app2 not routed"
$BIN list

note "conflict without --force must fail"
$BIN app1 true 2>/dev/null && bad "conflict not detected"

note "unknown name -> 404"
code=$(curl -s -m 3 -o /dev/null -w '%{http_code}' -H "Host: ghost.e2e.test" "127.0.0.1:$LISTEN_PORT/")
[ "$code" = 404 ] || bad "expected 404, got $code"

note "kill apps -> proxy self-exits"
kill -TERM "$W1" "$W2" 2>/dev/null
wait "$W1" "$W2" 2>/dev/null
sleep 7
if curl -s -m 1 -o /dev/null "127.0.0.1:$LISTEN_PORT/" 2>/dev/null; then
    bad "proxy still listening after idle"
fi

rm -rf "$PORTPROXY_STATE_DIR"
if [ "$fail" = 0 ]; then echo; echo "E2E OK"; else echo; echo "E2E FAILED"; fi
exit "$fail"
