#!/usr/bin/env bash
# The escrow room on the dev stack (scripts/dev.sh up first): starts zdlc-room on
# http://127.0.0.1:$ROOM_PORT/escrow (default port 8080), funds its demo wallet from the regtest miner, and stays in
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
for _ in $(seq 1 60); do grep -q "fund the demo wallet at" "$LOG" && break; sleep 1; done
ADDR=$(sed -n 's/.*fund the demo wallet at \([a-z0-9]*\).*/\1/p' "$LOG")
[ -n "$ADDR" ] || { cat "$LOG"; exit 1; }
if [ "$(curl -s "http://127.0.0.1:$PORT/escrow/api/info" | sed -n 's/.*"demoWalletZat":\([0-9]*\).*/\1/p')" = 0 ]; then
  # The demo wallet's inbox is shielded: pay it from the dev wallet; the room sweeps it
  # into treasury coins within a minute.
  "$ZDLC_BIN/zdlc-dev" pay --node "$ZDLC_NODE_URL" "zcash:$ADDR?amount=0.02" || echo "could not fund the demo wallet; pay $ADDR from any regtest wallet"
fi
echo "demos: http://127.0.0.1:$PORT/escrow, /hedge and /auction (log: $LOG)"
wait $PID
