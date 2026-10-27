#!/usr/bin/env bash
# The escrow room on the dev stack (scripts/dev.sh up first): starts zdlc-room on
# http://127.0.0.1:$ROOM_PORT/ (default 8080), funds its demo wallet from the regtest miner, and stays in
# the foreground. Regtest blocks come every 2 s, so the heights are 37.5 times testnet's
# to keep testnet's timing: rulings count after ~2.5 min, the refund unlocks at ~5 min.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck disable=SC1091
source "$ROOT/.dev/env"
mkdir -p "$ROOT/.dev/room"
LOG="$ROOT/.dev/room.log"
PORT="${ROOM_PORT:-8080}"
"$ZDLC_BIN/zdlc-room" --node "$ZDLC_NODE_URL" --desk http://127.0.0.1:7900 \
  --oracles http://127.0.0.1:7801,http://127.0.0.1:7802 --oracle-token-file "$ROOT/.dev/admin" \
  --dir "$ROOT/.dev/room" --listen "127.0.0.1:$PORT" --label regtest \
  --block-secs 2 --maturity 75 --refund 150 --close-wait 37 --close-wait-secs 30 >"$LOG" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT INT TERM
for _ in $(seq 1 30); do grep -q "http://" "$LOG" && break; sleep 1; done
ADDR=$(sed -n 's/.*demo wallet (the client.s treasury) at \(t[a-zA-Z0-9]*\).*/\1/p' "$LOG")
[ -n "$ADDR" ] || { cat "$LOG"; exit 1; }
if [ "$(curl -s "http://127.0.0.1:$PORT/api/info" | sed -n 's/.*"demoWalletZat":\([0-9]*\).*/\1/p')" = 0 ]; then
  # Several small coins, so several rooms can fund at once (regtest coinbases shrink
  # every 150 blocks, so a long-running chain cannot pay much more).
  "$ZDLC_BIN/zdlc-dev" fund --node "$ZDLC_NODE_URL" "$ADDR" 0.03 "$ADDR" 0.03 "$ADDR" 0.03 "$ADDR" 0.03 "$ADDR" 0.03 "$ADDR" 0.03
fi
echo "escrow room: http://127.0.0.1:$PORT/ (log: $LOG)"
wait $PID
