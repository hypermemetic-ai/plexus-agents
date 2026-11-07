#!/usr/bin/env bash
# deploy/build-image.sh [COMMIT] — plexus-agents' image (SUPERCONDUCTOR-20),
# ghcr.io/hypermemetic-ai/agents:<COMMIT> (default: HEAD of this checkout, full
# sha — what hypermemetic-infra's `app.yml -e sha=` names).
#
# plexus-harness is unpublished, so the build context is assembled, not taken
# from a checkout: plexus-agents at COMMIT (`git archive`), and each sibling at
# the commit the Dockerfile's header pins (`# pin <repo> <sha>`), `git archive`d
# from the first checkout beside this one that holds that commit:
# $AGENTS_SIBLINGS/<repo>, then $AGENTS_SIBLINGS/<repo>-* ($AGENTS_SIBLINGS
# defaults to this checkout's parent directory). Nothing of a working tree goes in.
#
# Prints the image reference on stdout; each step's wall-clock seconds on stderr.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(git -C "$here" rev-parse --show-toplevel)"
die() { echo "build-image: $*" >&2; exit 2; }
t0=$(date +%s); lap() { local n; n=$(date +%s); echo "build-image: $1 — $((n - t0)) s" >&2; t0=$n; }

commit="$(git -C "$repo" rev-parse --verify "${1:-HEAD}^{commit}")" || die "no commit ${1:-HEAD}"
image="${AGENTS_IMAGE:-ghcr.io/hypermemetic-ai/agents}:$commit"
siblings="${AGENTS_SIBLINGS:-$(cd "$repo/.." && pwd)}"

ctx="$(mktemp -d "${TMPDIR:-/tmp}/agents-image.XXXXXX")"
trap 'rm -rf "$ctx"' EXIT

# plexus-agents at COMMIT; the Dockerfile and its pins are that commit's.
git -C "$repo" archive --prefix=plexus-agents/ "$commit" | tar -x -C "$ctx"
cp "$ctx/plexus-agents/deploy/Dockerfile" "$ctx/Dockerfile"
pins=$(sed -n 's/^# pin \([a-z0-9-]*\) *\([0-9a-f]\{40\}\)$/\1 \2/p' "$ctx/Dockerfile")
[ -n "$pins" ] || die "the Dockerfile at $commit pins no sibling"
while read -r name sha; do
  src=""
  for cand in "$siblings/$name" "$siblings/$name"-*; do
    if [ -d "$cand" ] && git -C "$cand" cat-file -e "$sha^{commit}" 2>/dev/null; then src=$cand; break; fi
  done
  [ -n "$src" ] || die "no checkout of $name under $siblings has commit $sha (fetch or bundle it over)"
  git -C "$src" archive --prefix="$name/" "$sha" | tar -x -C "$ctx"
  echo "build-image: $name @ ${sha:0:12} (from $src)" >&2
done <<<"$pins"
lap "context assembled ($(du -sh "$ctx" | cut -f1))"

DOCKER_BUILDKIT=1 docker build --quiet \
  --label "org.opencontainers.image.revision=$commit" \
  --label "org.opencontainers.image.description=plexus-agents; $(echo "$pins" | awk '{printf "%s@%s ", $1, substr($2,1,12)}')" \
  -t "$image" "$ctx" >/dev/null
docker image inspect -f 'build-image: {{.Id}} {{.Size}} bytes' "$image" >&2
lap "image built"

if [ "${PUSH:-0}" = 1 ]; then
  docker push --quiet "$image" >/dev/null
  lap "pushed"
fi
echo "$image"
