#!/usr/bin/env bash
# Builds crates/zdlc-wasm into sdk/sdk/core: one package for Node (wasm-node) and one for
# browsers and bundlers (wasm-web). Needs wasm-pack and a clang with a wasm32 target for
# the secp256k1 C libraries (Homebrew's llvm on macOS; set WASM_CC to another).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LLVM="${WASM_LLVM:-/opt/homebrew/opt/llvm/bin}"
export CC_wasm32_unknown_unknown="${WASM_CC:-$LLVM/clang}" AR_wasm32_unknown_unknown="${WASM_AR:-$LLVM/llvm-ar}"
cd "$ROOT/crates/zdlc-wasm"
wasm-pack build --release --target nodejs --out-dir "$ROOT/sdk/sdk/core/node" --out-name zdlc_core --no-pack
wasm-pack build --release --target web --out-dir "$ROOT/sdk/sdk/core/web" --out-name zdlc_core --no-pack
rm -f "$ROOT"/sdk/sdk/core/*/.gitignore
# The Node build is CommonJS; the package around it is ESM.
echo '{ "type": "commonjs" }' > "$ROOT/sdk/sdk/core/node/package.json"
echo '{ "type": "module" }' > "$ROOT/sdk/sdk/core/web/package.json"
ls -la "$ROOT"/sdk/sdk/core/node/*.wasm "$ROOT"/sdk/sdk/core/web/*.wasm
