#!/usr/bin/env bash
# Shallow-clone the pinned jj release tag into jj-<commit-sha>/ for AI/dev reference.
# Not versioned in dojjo; jj ignores the nested .git repo.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JJ_VERSION="${1:?jj version required, e.g. 0.38.0}"
TAG="v${JJ_VERSION}"
REPO="https://github.com/jj-vcs/jj.git"

SHA="$(git ls-remote "$REPO" "refs/tags/${TAG}" | cut -f1)"
if [[ -z "$SHA" ]]; then
  echo "tag ${TAG} not found on ${REPO}" >&2
  exit 1
fi

TARGET="${ROOT}/jj-${SHA}"

if [[ -d "${TARGET}/.git" ]]; then
  echo "jj reference already present at jj-${SHA} (${TAG})"
  exit 0
fi

git clone --depth 1 --branch "$TAG" "$REPO" "$TARGET"
echo "cloned ${TAG} → jj-${SHA}"
