#!/usr/bin/env bash
# Builds the Node-API addon (crates/zdlc-napi) into sdk/sdk/native/zdlc.<platform>-<arch>.node.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
(cd "$ROOT" && cargo build --release -p zdlc-napi)
PLAT=$(node -p 'process.platform + "-" + process.arch')
mkdir -p "$ROOT/sdk/sdk/native"
for lib in libzdlc_napi.dylib libzdlc_napi.so zdlc_napi.dll; do
  [ -f "$TARGET/release/$lib" ] && cp "$TARGET/release/$lib" "$ROOT/sdk/sdk/native/zdlc.$PLAT.node" && break
done
ls -la "$ROOT/sdk/sdk/native/"
