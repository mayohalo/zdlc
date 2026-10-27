#!/usr/bin/env bash
# Builds a release's assets into dist/v<VERSION>/, for upload to a GitHub release:
#   - zdlc-client-<V>.tgz   @zdlc/client (TypeScript, no dependencies)
#   - zdlc-sdk-<V>.tgz      @zdlc/sdk: the native addon for this machine and for Linux
#                           x86_64, and the WebAssembly core
#   - zdlc-<V>-x86_64-linux.tar.gz, zdlc-<V>-<arch>-macos.tar.gz: the binaries
#   - SHA256SUMS
#   scripts/release.sh 0.1.0
# Needs cargo-zigbuild (the Linux builds), wasm-pack and a clang with a wasm32 target.
set -euo pipefail
V="${1:?usage: release.sh VERSION}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/dist/v$V"
LINUX=x86_64-unknown-linux-gnu
BINS="zdlc-node zdlc-cli zdlc-agent zdlc-room zdlc-oracled zdlc-desk"
BUILD=(-p zdlc --bin zdlc-oracled --bin zdlc-desk -p zdlc-kit --bin zdlc-node --bin zdlc-cli --bin zdlc-agent --bin zdlc-room)
rm -rf "$OUT" && mkdir -p "$OUT"
cd "$ROOT"

# For packing, the packages carry the release's version, and @zdlc/sdk depends on the
# client's release tarball instead of the repo path. The repo's files come back after.
URL="https://github.com/mayohalo/zdlc/releases/download/v$V"
KEEP="$(mktemp -d)"
cp sdk/ts/package.json "$KEEP/client.json" && cp sdk/sdk/package.json "$KEEP/sdk.json"
trap 'cp "$KEEP/client.json" sdk/ts/package.json; cp "$KEEP/sdk.json" sdk/sdk/package.json; rm -rf "$KEEP"' EXIT
node -e '
  const fs = require("fs"), [v, url] = process.argv.slice(1);
  for (const p of ["sdk/ts/package.json", "sdk/sdk/package.json"]) {
    const j = JSON.parse(fs.readFileSync(p, "utf8"));
    j.version = v;
    if (j.dependencies && j.dependencies["@zdlc/client"]) j.dependencies["@zdlc/client"] = `${url}/zdlc-client-${v}.tgz`;
    fs.writeFileSync(p, JSON.stringify(j, null, 2) + "\n");
  }' "$V" "$URL"

# @zdlc/client
(cd sdk/ts && npm install --silent && npm run build --silent && npm pack --silent --pack-destination "$OUT")

# @zdlc/sdk: the addon for this machine, then for Linux x86_64, then the WebAssembly core.
scripts/build-node.sh >/dev/null
CARGO_TARGET_DIR=target/linux cargo zigbuild --release --target "$LINUX.2.35" -p zdlc-napi
cp "target/linux/$LINUX/release/libzdlc_napi.so" sdk/sdk/native/zdlc.linux-x64.node
scripts/build-wasm.sh >/dev/null
(cd sdk/sdk && npm pack --silent --pack-destination "$OUT")

# The binaries: this machine, and Linux x86_64.
cargo build --release "${BUILD[@]}"
tar -czf "$OUT/zdlc-$V-$(uname -m)-macos.tar.gz" -C target/release $BINS
CARGO_TARGET_DIR=target/linux cargo zigbuild --release --target "$LINUX.2.35" "${BUILD[@]}"
tar -czf "$OUT/zdlc-$V-x86_64-linux.tar.gz" -C "target/linux/$LINUX/release" $BINS

(cd "$OUT" && shasum -a 256 -- * >SHA256SUMS)
ls -la "$OUT"
