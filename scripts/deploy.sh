#!/usr/bin/env bash
# Builds the escrow room's server binaries for Linux x86_64 here (cargo-zigbuild; a
# 1 GB server cannot compile the proving stack) and installs them on a server:
#   scripts/deploy.sh root@HOST DOMAIN [SSH_KEY]
set -euo pipefail
HOST="${1:?usage: deploy.sh root@HOST DOMAIN [SSH_KEY]}"; DOMAIN="${2:?domain}"; KEY="${3:-}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SSH=(ssh); SCP=(scp -q); [ -n "$KEY" ] && SSH+=(-i "$KEY") && SCP+=(-i "$KEY")
T=x86_64-unknown-linux-gnu
(cd "$ROOT" && CARGO_TARGET_DIR=target/linux cargo zigbuild --release --target "$T.2.35" \
  -p zdlc --bin zdlc-desk --bin zdlc-oracled -p zdlc-kit --bin zdlc-room)
BIN="$ROOT/target/linux/$T/release"
"${SSH[@]}" "$HOST" 'install -d /opt/zdlc/bin /opt/zdlc/deploy'
"${SCP[@]}" "$BIN/zdlc-desk" "$BIN/zdlc-oracled" "$BIN/zdlc-room" "$HOST:/opt/zdlc/bin.new/" 2>/dev/null || {
  "${SSH[@]}" "$HOST" 'install -d /opt/zdlc/bin.new'
  "${SCP[@]}" "$BIN/zdlc-desk" "$BIN/zdlc-oracled" "$BIN/zdlc-room" "$HOST:/opt/zdlc/bin.new/"
}
"${SCP[@]}" -r "$ROOT/deploy/." "$HOST:/opt/zdlc/deploy/"
# The landing page, served at / beside the room.
"${SSH[@]}" "$HOST" 'install -d /opt/zdlc/site'
"${SCP[@]}" "$ROOT/site/index.html" "$HOST:/opt/zdlc/site/index.html"
# Swap the binaries in while the services restart.
"${SSH[@]}" "$HOST" 'chmod 755 /opt/zdlc/bin.new/* && mv /opt/zdlc/bin.new/* /opt/zdlc/bin/ && rmdir /opt/zdlc/bin.new && bash /opt/zdlc/deploy/install.sh '"$DOMAIN"
