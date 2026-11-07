#!/usr/bin/env bash
# deploy/push-ui.sh URL KEYFILE — build the superconductor's page (ui/, a
# plexus-ui bundle, SUPERCONDUCTOR-11), sign it and push it live to the service
# at URL (wss://agents-test.menger.sh/ws, or ws://127.0.0.1:4470 locally).
#
# KEYFILE is an Ed25519 seed (raw, hex or base64; 0600). The push signs in with
# it (as `deploy`) and signs the bundle with it, so its public key must be both
# a ui signer (AGENTS_UI_SIGNERS: `plexus-agents --print-key KEYFILE`, without
# `key:`) and an admin (AGENTS_ADMINS: that `key:…`), or sign in as an admin
# another way. Nothing is printed of the key.
#
# Needs: the plexus-ui CLI ($PLEXUS_UI, default `plexus-ui` on PATH), its engine
# files ($PLEXUS_UI_ENGINE: engine.js, engine.wasm, host.js) and node_modules
# with svelte and esbuild ($PLEXUS_UI_NODE_MODULES), as plexus-ui's README builds
# them. Prints the version pushed.
set -euo pipefail
[ $# -eq 2 ] || { echo "usage: $0 URL KEYFILE" >&2; exit 2; }
url=$1 key=$2
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
ui="${PLEXUS_UI:-plexus-ui}"
: "${PLEXUS_UI_ENGINE:?the plexus-ui engine directory (engine.js, engine.wasm, host.js)}"
: "${PLEXUS_UI_NODE_MODULES:?node_modules holding svelte and esbuild}"
out="$(mktemp -d "${TMPDIR:-/tmp}/sc-ui.XXXXXX")"
trap 'rm -rf "$out"' EXIT

"$ui" build "$repo/ui" --engine "$PLEXUS_UI_ENGINE" --node-modules "$PLEXUS_UI_NODE_MODULES" \
  --strict-colours -o "$out/ui.tar" >/dev/null
"$ui" push "$out/ui.tar" --url "$url" --service plexus-agents --key-file "$key" --as deploy --timeout 300
