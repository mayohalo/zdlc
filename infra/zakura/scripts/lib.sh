# Shared helpers. Source me.
ZDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RPC_URL="http://127.0.0.1:${ZDLC_NODE_RPC_PORT:-18252}/"
COMPOSE=(docker compose -f "$ZDIR/docker-compose.yml")

# rpc METHOD [JSON_PARAMS] -> prints .result, or exits with .error
rpc() {
  curl -s --max-time 900 -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":${2:-[]}}" "$RPC_URL" |
    /usr/bin/python3 -c 'import json,sys; r=json.load(sys.stdin)
e=r.get("error")
if e: sys.exit("rpc error: %s" % e)
v=r["result"]; print(v if isinstance(v,(str,int)) else json.dumps(v))'
}

wait_rpc() {
  for _ in $(seq 1 120); do
    rpc getblockcount >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "zdlc-zakura RPC not up at $RPC_URL" >&2; return 1
}
