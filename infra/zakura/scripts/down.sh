#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
"${COMPOSE[@]}" down
