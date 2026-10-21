#!/usr/bin/env bash
# End-to-end demos on the dev stack (scripts/dev.sh up first).
#   scripts/demo.sh escrow    # alice invoices bob 1.5 ZEC; bob pays a deposit link; "delivered"; close
#   scripts/demo.sh stable    # alice is owed $50 in ZEC; bob hedges; the live ZEC/USD price (internet)
#   scripts/demo.sh bet       # a bet from both treasuries; the arbiters decide
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck disable=SC1091
source "$ROOT/.dev/env"
BOB=$(zb info | sed -n 's/.*"identity": "\(.*\)".*/\1/p')

field() { sed -n "s/.*\"$1\": \"\{0,1\}\([^\",]*\)\"\{0,1\}.*/\1/p" | head -1; }
until_state() { # node fn, id, state
  for _ in $(seq 1 300); do
    s=$($1 show "$2" 2>/dev/null | field state || true)
    [ "$s" = "$3" ] && return 0
    [ "$s" = failed ] && { $1 show "$2"; exit 1; }
    sleep 1
  done
  echo "timed out waiting for $3" >&2; exit 1
}

case "${1:-escrow}" in
  escrow)
    ID=$(za offer escrow --to "$BOB" --amount 1.5 --question "Milestone 1 delivered?" --maturity 10 --refund 400 | field id)
    echo "offered escrow $ID"; until_state zb "$ID" incoming
    zb accept "$ID" --funding deposit >/dev/null; until_state zb "$ID" awaiting_deposit
    zb qr "$ID"
    URI=$(zb show "$ID" | field uri); zpay "$URI"
    until_state za "$ID" funded; echo "funded"; zresolve "$ID" delivered
    until_state za "$ID" settled; za show "$ID" | grep -A4 '"settlement"'
    ;;
  stable)
    MIN=$(( ($(date +%s) / 60 - 3) * 60 ))
    ID=$(za offer stable-payment --to "$BOB" --usd 50.00 --collateral 0.1 --premium 0.0005 --maturity 8 --refund 300 --minute "$MIN" | field id)
    echo "offered stable payment $ID"; until_state zb "$ID" incoming
    zb accept "$ID" --funding treasury >/dev/null
    until_state za "$ID" settled; za show "$ID" | grep -E '"attested"|"how"'
    ;;
  bet)
    ID=$(za offer bet --to "$BOB" --question "Rain on Friday?" --outcomes yes,no --offerer-wins yes --stake 0.2 --maturity 10 --refund 400 | field id)
    echo "offered bet $ID"; until_state zb "$ID" incoming
    zb accept "$ID" --funding treasury >/dev/null; until_state za "$ID" funded; zresolve "$ID" no
    until_state zb "$ID" settled; zb show "$ID" | grep -A4 '"settlement"'
    ;;
  *) echo "usage: $0 escrow|stable|bet" >&2; exit 1 ;;
esac
