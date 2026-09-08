#!/usr/bin/env bash
#
# Verifies that the toolchain versions pinned across the repo agree with each
# other. Runs in CI before any toolchain is installed, so it only reads files.
#
#   rust-toolchain.toml  channel      -- the exact Rust the repo builds with
#   Cargo.toml           rust-version -- the MSRV the crates claim
#   foundry.toml         solc         -- the exact Solidity compiler
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
status=0

fail() {
  echo "[toolchain] FAIL: $*" >&2
  status=1
}

# Extract the value of `key = "value"` from a TOML file, first match wins.
toml_value() {
  sed -n "s/^[[:space:]]*$2[[:space:]]*=[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$1" | head -n 1
}

channel="$(toml_value "$REPO_ROOT/rust-toolchain.toml" channel)"
msrv="$(toml_value "$REPO_ROOT/Cargo.toml" rust-version)"
solc="$(toml_value "$REPO_ROOT/contracts/foundry.toml" solc)"

if [[ -z "$channel" ]]; then
  fail "rust-toolchain.toml has no [toolchain] channel pin"
elif [[ ! "$channel" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  fail "rust-toolchain.toml channel '$channel' is not an exact x.y.z version"
fi

if [[ -z "$msrv" ]]; then
  fail "Cargo.toml [workspace.package] has no rust-version"
fi

# The pinned toolchain must be able to build crates that declare the MSRV,
# i.e. channel >= rust-version. Compare on major.minor only, since rust-version
# is conventionally written without a patch level.
if [[ -n "$channel" && -n "$msrv" ]]; then
  channel_mm="${channel%.*}"
  msrv_mm="$(printf '%s' "$msrv" | cut -d. -f1,2)"
  lowest="$(printf '%s\n%s\n' "$channel_mm" "$msrv_mm" | sort -V | head -n 1)"
  if [[ "$lowest" != "$msrv_mm" ]]; then
    fail "rust-toolchain.toml channel ($channel) is older than Cargo.toml rust-version ($msrv)"
  fi
fi

if [[ -z "$solc" ]]; then
  fail "contracts/foundry.toml has no solc pin"
elif [[ ! "$solc" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  fail "contracts/foundry.toml solc '$solc' is not an exact x.y.z version"
fi

if [[ $status -eq 0 ]]; then
  echo "[toolchain] rust channel $channel (MSRV $msrv), solc $solc"
  echo "[toolchain] OK"
fi

exit $status
