# zDLC development. `brew install just` (or use scripts/ directly).

# Build and start the local stack: regtest, desk, two oracles, two nodes with 5 ZEC each.
dev:
    scripts/dev.sh up

# Stop it (`just down --chain` also removes the regtest chain and .dev).
down *ARGS:
    scripts/dev.sh down {{ARGS}}

status:
    scripts/dev.sh status

# An end-to-end demo on the stack: escrow (default), stable, bet.
demo KIND="escrow":
    scripts/demo.sh {{KIND}}

# zdlc-cli against alice's or bob's node, e.g. `just a contracts`.
a *ARGS:
    bash -c 'source .dev/env && za "$@"' -- {{ARGS}}
b *ARGS:
    bash -c 'source .dev/env && zb "$@"' -- {{ARGS}}

# The dev wallet pays a ZIP 321 link (a deposit or a leg).
pay URI:
    bash -c 'source .dev/env && zpay "$1"' -- '{{URI}}'

# Both arbiter oracles resolve contract ID's event.
resolve ID OUTCOME:
    bash -c 'source .dev/env && zresolve "$1" "$2"' -- {{ID}} '{{OUTCOME}}'

# Every test that needs no chain.
test:
    cargo test --release -p zdlc -p zdlc-kit
    cd sdk/ts && npm run build && npm test

# Build @zdlc/sdk: the Node addon and the WebAssembly core, then its offline tests.
sdk:
    scripts/build-node.sh
    scripts/build-wasm.sh
    cd sdk/sdk && npm install && npm test
