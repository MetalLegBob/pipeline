#!/usr/bin/env bash
# Tests scripts/new-strategy.sh in a temporary folder, with a stand-in `solana-keygen`
# so it runs without the Solana CLI. Checks the renaming, where the keypair goes, which
# folder permissions change, and that bad input is refused.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ADDRESS=7Q2nN6z9qY4R8kQm5wJ3xV1pT6sL2dH9fB4cE8aG3uZk

# The stand-in writes a placeholder file for `new` and prints a fixed address for `pubkey`.
mkdir "$WORK/bin"
cat > "$WORK/bin/solana-keygen" <<EOF
#!/usr/bin/env bash
case "\$1" in
  new) while [[ \$# -gt 0 ]]; do [[ "\$1" == --outfile ]] && echo '[]' > "\$2"; shift; done ;;
  pubkey) echo $ADDRESS ;;
esac
EOF
chmod +x "$WORK/bin/solana-keygen"
export PATH="$WORK/bin:$PATH"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

permissions() {
  if stat -f %Lp "$1" >/dev/null 2>&1; then
    stat -f %Lp "$1"
  else
    stat -c %a "$1"
  fi
}

# A new project with its keypair in a folder that already exists.
mkdir "$WORK/shared"
chmod 755 "$WORK/shared"
"$ROOT/scripts/new-strategy.sh" my-amm "$WORK/project" "$WORK/shared/my-amm.json" >/dev/null
[[ -f "$WORK/shared/my-amm.json" ]] || fail "keypair not created"
[[ "$(permissions "$WORK/shared")" == 755 ]] || fail "an existing keypair folder had its permissions changed"
[[ -d "$WORK/project/programs/my-amm" ]] || fail "program folder not renamed"
grep -q "declare_id!(\"$ADDRESS\")" "$WORK/project/programs/my-amm/src/lib.rs" || fail "declare_id! not set"
grep -q 'name = "my-amm"' "$WORK/project/programs/my-amm/Cargo.toml" || fail "crate not renamed"
if grep -rq "my-strategy\|my_strategy\|MyStrategy1111" "$WORK/project"; then
  fail "template names left in the project"
fi
if find "$WORK/project" -name '*.json' -exec grep -l '^\[\]$' {} + | grep -q .; then
  fail "a keypair ended up inside the project"
fi

# A keypair folder the script creates is made private.
"$ROOT/scripts/new-strategy.sh" other-amm "$WORK/project2" "$WORK/new-keys/other-amm.json" >/dev/null
[[ "$(permissions "$WORK/new-keys")" == 700 ]] || fail "a new keypair folder is not private"

# Refusals: a bad name, a non-empty destination, a keypair inside the project.
if "$ROOT/scripts/new-strategy.sh" My_Amm "$WORK/p3" >/dev/null 2>&1; then
  fail "accepted a bad name"
fi
if "$ROOT/scripts/new-strategy.sh" my-amm "$WORK/project" "$WORK/shared/x.json" >/dev/null 2>&1; then
  fail "accepted a non-empty destination"
fi
if "$ROOT/scripts/new-strategy.sh" my-amm "$WORK/p4" "$WORK/p4/keys/k.json" >/dev/null 2>&1; then
  fail "accepted a keypair inside the project"
fi

echo "new-strategy.sh: all checks passed"
