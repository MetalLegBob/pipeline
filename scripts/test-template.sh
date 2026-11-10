#!/usr/bin/env bash
# Builds and tests the strategy template (templates/strategy) against this repository's
# own crates and router, the way CI checks that the template still works.
#
# The template depends on the published Pipeline crates and SDK. Here Cargo's `[patch]`
# swaps the crates for the local sources, the scripts use the local SDK, and the tests load
# the locally built router instead of the devnet copy `scripts/fetch-router.sh` would
# download. The template is built from a temporary copy, so nothing is written inside it.
#
#   scripts/test-template.sh
#
# Needs cargo-build-sbf 4.4 or later. Build artifacts are cached in target/template.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Where this repository keeps each crate the template takes from crates.io.
# Change a path here when a crate moves.
LOCAL_CRATES=(
  "pipeline-strategy-interface=$ROOT/programs/pipeline-strategy-interface"
  "pipeline-client=$ROOT/crates/pipeline-client"
  "pipeline-check=$ROOT/crates/pipeline-check"
)
ROUTER_MANIFEST="$ROOT/programs/pipeline/Cargo.toml"
ROUTER_SO="$ROOT/programs/target/deploy/pipeline.so"
TEMPLATE="$ROOT/templates/strategy"
TEMPLATE_PROGRAM=programs/my-strategy

if [[ ! -f "$ROUTER_SO" ]]; then
  echo "Building the router"
  cargo build-sbf --manifest-path "$ROUTER_MANIFEST"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
tar -C "$TEMPLATE" --exclude=target --exclude=node_modules -cf - . | tar -C "$WORK" -xf -

# `[patch]` in the copy's Cargo config, so every cargo call (including the
# `cargo metadata` that cargo-build-sbf runs first) uses the local crates.
mkdir -p "$WORK/.cargo"
{
  echo "[patch.crates-io]"
  for entry in "${LOCAL_CRATES[@]}"; do
    echo "${entry%%=*} = { path = \"${entry#*=}\" }"
  done
} > "$WORK/.cargo/config.toml"
export CARGO_TARGET_DIR="$ROOT/target/template"

cd "$WORK"
echo "Checking formatting"
cargo fmt --all -- --check
echo "Building the template program"
cargo build-sbf --manifest-path "$TEMPLATE_PROGRAM/Cargo.toml"
echo "Testing the template"
SBF_OUT_DIR="$CARGO_TARGET_DIR/deploy" PIPELINE_ROUTER_SO="$ROUTER_SO" cargo test

# The scripts need the SDK's own dependencies installed (`npm ci` in sdk/ts).
if [[ ! -d "$ROOT/sdk/ts/node_modules" ]]; then
  echo "Skipping the scripts' type check: run npm ci in sdk/ts first"
  exit 0
fi
echo "Type-checking the scripts against the local SDK"
(cd "$ROOT/sdk/ts" && npm run build >/dev/null)
cd "$WORK/scripts"
npm pkg set "dependencies.@pipeline-sol/sdk=file:$ROOT/sdk/ts"
npm install --no-audit --no-fund --no-package-lock >/dev/null
npm run typecheck
