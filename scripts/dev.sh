#!/usr/bin/env bash
# The local zDLC stack: Zakura regtest, a desk, two arbiter oracles, a block every two
# seconds, and two zdlc-nodes (alice on :7950, bob on :7951) with funded treasuries.
#
#   scripts/dev.sh up      # build, start everything, write .dev/env
#   scripts/dev.sh down    # stop the processes (and the regtest node with --chain)
#   scripts/dev.sh status
#
# Then: source .dev/env; za info; zb contracts; see scripts/demo.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEV="$ROOT/.dev"
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release"
RPC="${ZDLC_NODE_URL:-http://127.0.0.1:18252/}"

start() { # name, command...
  local name="$1"; shift
  "$@" >"$DEV/$name.log" 2>&1 &
  echo $! >"$DEV/$name.pid"
}

wait_http() {
  for _ in $(seq 1 100); do curl -sf -o /dev/null "$1" && return 0; sleep 0.2; done
  echo "dev: $1 did not start" >&2; return 1
}

up() {
  mkdir -p "$DEV"
  (cd "$ROOT" && cargo build --release --bins -p zdlc -p zdlc-kit -p zdlc-demos)
  "$ROOT/infra/zakura/scripts/up.sh" >/dev/null
  [ -f "$DEV/admin" ] || head -c 16 /dev/urandom | xxd -p >"$DEV/admin"
  start desk "$BIN/zdlc-desk" --listen 127.0.0.1:7900 --state "$DEV/desk.json" --node "$RPC"
  start oracle-a "$BIN/zdlc-oracled" --listen 127.0.0.1:7801 --state "$DEV/a.sealed" --sealing-key "$DEV/a.key" --admin-token "$DEV/admin" --node "$RPC"
  start oracle-b "$BIN/zdlc-oracled" --listen 127.0.0.1:7802 --state "$DEV/b.sealed" --sealing-key "$DEV/b.key" --admin-token "$DEV/admin" --node "$RPC"
  wait_http http://127.0.0.1:7900/health; wait_http http://127.0.0.1:7801/pubkey; wait_http http://127.0.0.1:7802/pubkey
  start miner "$BIN/zdlc-dev" mine --node "$RPC" --every 2
  for who in alice:7950 bob:7951; do
    name="${who%%:*}"; port="${who##*:}"
    start "$name" "$BIN/zdlc-node" --seed "$DEV/$name.seed" --db "$DEV/$name.db" --node "$RPC" --desk http://127.0.0.1:7900 \
      --oracles http://127.0.0.1:7801,http://127.0.0.1:7802 --listen "127.0.0.1:$port" --block-secs 2
    wait_http "http://127.0.0.1:$port/v1/openapi.json"
  done
  for name in alice bob; do
    port=$([ "$name" = alice ] && echo 7950 || echo 7951)
    t=$("$BIN/zdlc-cli" --url "http://127.0.0.1:$port" --token-file "$DEV/$name.seed.token" info | sed -n 's/.*"treasury": "\(.*\)".*/\1/p')
    "$BIN/zdlc-dev" fund --node "$RPC" "$t" 5 >/dev/null
  done
  cat >"$DEV/env" <<ENV
export ZDLC_BIN="$BIN" ZDLC_NODE_URL="$RPC" ZDLC_ADMIN="\$(cat $DEV/admin)"
za() { "$BIN/zdlc-cli" --url http://127.0.0.1:7950 --token-file "$DEV/alice.seed.token" "\$@"; }
zb() { "$BIN/zdlc-cli" --url http://127.0.0.1:7951 --token-file "$DEV/bob.seed.token" "\$@"; }
zpay() { "$BIN/zdlc-dev" pay --node "$RPC" "\$1"; }
zresolve() { for p in 7801 7802; do curl -sf -X POST -H 'content-type: application/json' -d "{\"outcome\":\"\$2\",\"token\":\"\$(cat $DEV/admin)\"}" "http://127.0.0.1:\$p/events/zdlc-\$1/resolve" >/dev/null; done; echo "resolved \$1: \$2"; }
ENV
  echo "dev stack up: alice http://127.0.0.1:7950, bob http://127.0.0.1:7951 (5 ZEC each). Now: source .dev/env; za info"
}

down() {
  for f in "$DEV"/*.pid; do [ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null || true; rm -f "$f"; done
  if [ "${1:-}" = "--chain" ]; then "$ROOT/infra/zakura/scripts/down.sh" >/dev/null; rm -rf "$DEV"; fi
  echo "dev stack down"
}

status() {
  for f in "$DEV"/*.pid; do n=$(basename "$f" .pid); kill -0 "$(cat "$f")" 2>/dev/null && echo "$n up" || echo "$n down"; done
}

case "${1:-}" in up) up ;; down) shift; down "$@" ;; status) status ;; *) echo "usage: $0 up|down [--chain]|status" >&2; exit 1 ;; esac
