#!/usr/bin/env bash
# Builds every on-chain program in `programs/`, one at a time, into programs/target/deploy.
#
# Each program is built on its own because building the whole workspace in one cargo run
# unifies dependency features across programs, which changes the router's bytes. Built
# this way, the router matches what `solana-verify build` produces for it.
#
# Usage: scripts/build-programs.sh [program …]   (default: every workspace member)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/programs"

if [ "$#" -gt 0 ]; then
  programs=("$@")
else
  programs=()
  while IFS= read -r name; do
    programs+=("$name")
  done < <(python3 -c 'import tomllib; print("\n".join(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["members"]))')
fi

for program in "${programs[@]}"; do
  echo "== $program"
  cargo build-sbf --manifest-path "$program/Cargo.toml"
done
