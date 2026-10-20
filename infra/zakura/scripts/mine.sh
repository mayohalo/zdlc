#!/usr/bin/env bash
# Usage: mine.sh N  -> mines N blocks, prints the new height.
set -euo pipefail
source "$(dirname "$0")/lib.sh"
rpc generate "[${1:?number of blocks}]" >/dev/null
echo "height=$(rpc getblockcount)"
