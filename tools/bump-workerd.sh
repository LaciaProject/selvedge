#!/usr/bin/env bash
# Moves Selvedge to another official workerd release: the image (version and
# SHA-512 checksums, which CI also installs from), the e2e test dependencies,
# the docs, and the list of experimental compatibility flags tenants may not
# use. It must not edit .github/workflows/: the workerd update workflow pushes
# its result with GITHUB_TOKEN, which may not change workflow files.
#
# Usage: tools/bump-workerd.sh [<version>]   (default: the latest on npm)
# Needs npm, python3 and network access. Run from anywhere in the repository.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
NEW=${1:-$(npm view workerd version)}
OLD=$(sed -n 's/^ARG WORKERD_VERSION=//p' Dockerfile)
[[ "$NEW" =~ ^1\.[0-9]{8}\.[0-9]+$ ]] || { echo "not a workerd version: $NEW" >&2; exit 1; }
[ -n "$OLD" ] || { echo "no WORKERD_VERSION in Dockerfile" >&2; exit 1; }
if [ "$NEW" = "$OLD" ]; then
  echo "already on workerd $OLD"
  exit 0
fi

# The image verifies the npm tarballs against these.
integrity() { npm view "@cloudflare/workerd-$1@$NEW" dist.integrity | sed 's/^sha512-//'; }
AMD64=$(integrity linux-64)
ARM64=$(integrity linux-arm64)
[ -n "$AMD64" ] && [ -n "$ARM64" ] || { echo "no linux packages for workerd $NEW" >&2; exit 1; }

sed -i "s/^ARG WORKERD_VERSION=.*/ARG WORKERD_VERSION=$NEW/; \
  s|^ARG WORKERD_SHA512_AMD64=.*|ARG WORKERD_SHA512_AMD64=$AMD64|; \
  s|^ARG WORKERD_SHA512_ARM64=.*|ARG WORKERD_SHA512_ARM64=$ARM64|" Dockerfile
sed -i "s/${OLD//./\\.}/$NEW/g" README.md docs/compatibility.md docs/status.md
(cd test/e2e && npm install --save-exact --no-audit --no-fund "workerd@$NEW" > /dev/null)
python3 tools/experimental-flags.py "$NEW" > crates/selvedge-workerd/src/experimental-flags.txt

# Every pin must have moved. RFCs keep the version they were written against.
left=$(git grep -n "${OLD//./\\.}" -- . ':!test/e2e/package-lock.json' ':!docs/rfcs/' || true)
[ -z "$left" ] || { echo "still pinned to $OLD:" >&2; echo "$left" >&2; exit 1; }
git diff --quiet -- .github/ || { echo "changed .github/, which the workflow cannot push" >&2; exit 1; }
echo "workerd $OLD -> $NEW"
git --no-pager diff --stat
