#!/usr/bin/env bash
# scripts/ui-gate.sh [OUT] — the bundle's gate (SUPERCONDUCTOR-11): `plexus-ui
# build ui/` must succeed (every component compiles, declares its `uses`, and
# paints with theme tokens alone: --strict-colours). With
# SC_UI_E2E=1 it then runs tests/ui_e2e.rs, the bundle in Chromium against the
# one binary: pair a host, start a thread, answer the approval, see it end.
#
# Env as deploy/push-ui.sh: $PLEXUS_UI, $PLEXUS_UI_ENGINE, $PLEXUS_UI_NODE_MODULES.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-${CARGO_TARGET_DIR:-$repo/target}/ui.tar}"
"${PLEXUS_UI:-plexus-ui}" build "$repo/ui" --engine "${PLEXUS_UI_ENGINE:?}" \
  --node-modules "${PLEXUS_UI_NODE_MODULES:?}" --strict-colours -o "$out"
if [ "${SC_UI_E2E:-0}" = 1 ]; then
  cargo build --manifest-path "$repo/Cargo.toml" --examples
  SC_UI_BUNDLE="$out" cargo test --manifest-path "$repo/Cargo.toml" --test ui_e2e -- --ignored
fi
