#!/bin/sh
# deploy/entrypoint.sh — the image's entrypoint (SUPERCONDUCTOR-20).
# plexus-agents reads flags only; this turns the container's AGENTS_*
# environment (the image, deploy/env/<env>.env, the box) into them, then
# execs it. Arguments given to the container are appended.
#
#   AGENTS_DB, AGENTS_BIND, AGENTS_PORT     --db, --bind, --port (the image sets them)
#   AGENTS_PUBLIC_URL                       --public-url (the URL hosts dial)
#   AGENTS_PLACEMENTS   ;-separated         --placement each (never `local` here)
#   AGENTS_GRANTS       ;-separated         --grant each (PRINCIPAL=ROLE@SCOPE)
#   AGENTS_KEY_AUDIENCES ;-separated        --key-audience each
#   AGENTS_ISSUER + AGENTS_AUDIENCE         --issuer, --audience
#   AGENTS_SERVICE_KEY_FILE                 --service-key (the box's secret `server-key`)
#
# Not yet flags of plexus-agents, so not passed (said once at start):
# AGENTS_TRUSTED_PROXIES (the box sets it) and AGENTS_WORKOS_API_KEY_FILE.
set -eu

first=$#
set -- "$@" --db "${AGENTS_DB:-/data/agents.db}" --bind "${AGENTS_BIND:-0.0.0.0}" --port "${AGENTS_PORT:-4470}"
if [ -n "${AGENTS_PUBLIC_URL:-}" ]; then set -- "$@" --public-url "$AGENTS_PUBLIC_URL"; fi
if [ -n "${AGENTS_ISSUER:-}" ]; then set -- "$@" --issuer "$AGENTS_ISSUER" --audience "${AGENTS_AUDIENCE:?AGENTS_ISSUER needs AGENTS_AUDIENCE}"; fi
if [ -n "${AGENTS_SERVICE_KEY_FILE:-}" ] && [ -r "$AGENTS_SERVICE_KEY_FILE" ]; then
  set -- "$@" --service-key "$AGENTS_SERVICE_KEY_FILE"
fi

# ;-separated lists: split on `;` alone, no globbing (a grant's scope may be `*`).
set -f
for pair in "--placement:${AGENTS_PLACEMENTS:-}" "--grant:${AGENTS_GRANTS:-}" "--key-audience:${AGENTS_KEY_AUDIENCES:-}"; do
  flag=${pair%%:*}; list=${pair#*:}
  old_ifs=$IFS; IFS=';'
  for item in $list; do
    if [ -n "$item" ]; then set -- "$@" "$flag" "$item"; fi
  done
  IFS=$old_ifs
done
set +f

# The container's own arguments go last: move the first $first to the end.
i=0
while [ "$i" -lt "$first" ]; do a=$1; shift; set -- "$@" "$a"; i=$((i + 1)); done

for v in AGENTS_TRUSTED_PROXIES AGENTS_WORKOS_API_KEY_FILE; do
  eval "val=\${$v:-}"
  if [ -n "$val" ]; then echo "agents-entrypoint: $v is set but plexus-agents has no flag for it yet; not passed" >&2; fi
done

exec /usr/local/bin/plexus-agents "$@"
