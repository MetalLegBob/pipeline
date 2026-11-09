#!/usr/bin/env bash
# Copies the Pipeline router deployed on devnet to where the tests load it. Run it once,
# and again whenever the router is upgraded.
#
#   scripts/fetch-router.sh [cluster]     # cluster defaults to devnet
set -euo pipefail
cd "$(dirname "$0")/.."

ROUTER=PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34
CLUSTER=${1:-devnet}
OUT=programs/my-strategy/tests/fixtures/pipeline.so
mkdir -p "$(dirname "$OUT")"
solana program dump --url "$CLUSTER" "$ROUTER" "$OUT"
echo "Router $ROUTER from $CLUSTER -> $OUT"
