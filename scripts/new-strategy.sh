#!/usr/bin/env bash
# Starts a new Pipeline strategy project from templates/strategy.
#
#   scripts/new-strategy.sh <name> <directory> [program keypair]
#
# <name> is the program's crate name: lowercase letters, digits and dashes (my-amm).
# The project is copied to <directory>, which must not exist or be empty, with the crate,
# library and paths renamed.
#
# The program's keypair decides its address. It is created at
# ~/.config/pipeline/keys/<name>.json unless you pass a path, and an existing keypair at
# that path is reused. It is never put inside the project: anyone holding it can deploy
# to your program's address before you do.
set -euo pipefail

usage() {
  sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

[[ $# -ge 2 && $# -le 3 ]] || usage
NAME=$1
DEST=$2
KEYPAIR=${3:-$HOME/.config/pipeline/keys/$NAME.json}
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE="$ROOT/templates/strategy"
PLACEHOLDER_ID=MyStrategy111111111111111111111111111111111

if [[ ! "$NAME" =~ ^[a-z][a-z0-9]*(-[a-z0-9]+)*$ ]]; then
  echo "The name must be lowercase letters, digits and single dashes, starting with a letter (my-amm)." >&2
  exit 2
fi
if [[ -e "$DEST" && -n "$(ls -A "$DEST")" ]]; then
  echo "$DEST already exists and is not empty." >&2
  exit 1
fi
if ! command -v solana-keygen >/dev/null; then
  echo "solana-keygen not found: install the Solana CLI first." >&2
  exit 1
fi

mkdir -p "$DEST"
DEST="$(cd "$DEST" && pwd)"
# Only a keypair folder this script creates is made private; an existing folder (which
# could be the user's home directory) keeps its permissions.
if [[ -d "$(dirname "$KEYPAIR")" ]]; then
  NEW_KEYPAIR_DIR=false
else
  NEW_KEYPAIR_DIR=true
fi
KEYPAIR_DIR="$(mkdir -p "$(dirname "$KEYPAIR")" && cd "$(dirname "$KEYPAIR")" && pwd)"
KEYPAIR="$KEYPAIR_DIR/$(basename "$KEYPAIR")"
case "$KEYPAIR_DIR/" in
  "$DEST"/*)
    echo "Keep the program keypair outside the project (got $KEYPAIR)." >&2
    exit 1
    ;;
esac

# The keypair first: if that fails, nothing has been copied yet.
if [[ -f "$KEYPAIR" ]]; then
  echo "Using the existing program keypair $KEYPAIR"
else
  if [[ "$NEW_KEYPAIR_DIR" == true ]]; then
    chmod 700 "$KEYPAIR_DIR"
  fi
  solana-keygen new --no-bip39-passphrase --silent --outfile "$KEYPAIR" >/dev/null
  echo "Created the program keypair $KEYPAIR"
fi
ADDRESS="$(solana-keygen pubkey "$KEYPAIR")"

# The template, without build outputs or files tied to this repository's layout.
tar -C "$TEMPLATE" \
  --exclude=target --exclude=node_modules --exclude=Cargo.lock \
  --exclude=package-lock.json --exclude='*.so' \
  -cf - . | tar -C "$DEST" -xf -
mv "$DEST/programs/my-strategy" "$DEST/programs/$NAME"

# Rename everywhere: the crate (my-strategy), the library and module (my_strategy), and
# the program address.
SNAKE="${NAME//-/_}"
export NAME SNAKE ADDRESS PLACEHOLDER_ID
grep -rlE "my-strategy|my_strategy|$PLACEHOLDER_ID" "$DEST" |
  while IFS= read -r file; do
    perl -pi -e '
      s/\Q$ENV{PLACEHOLDER_ID}\E/$ENV{ADDRESS}/g;
      s/my-strategy/$ENV{NAME}/g;
      s/my_strategy/$ENV{SNAKE}/g;
    ' "$file"
  done

cat <<EOF

New strategy "$NAME" in $DEST
Program address $ADDRESS (keypair $KEYPAIR)

Next:
  cd $DEST
  git init                       # optional, but keep the keypair out of it
  scripts/fetch-router.sh        # the Pipeline router, for the tests
  cargo build-sbf --manifest-path programs/$NAME/Cargo.toml   # target/deploy/$SNAKE.so
  cargo test                     # LiteSVM tests, including pipeline-check

Then change the pricing in programs/$NAME/src/pricing.rs and follow README.md.
EOF
