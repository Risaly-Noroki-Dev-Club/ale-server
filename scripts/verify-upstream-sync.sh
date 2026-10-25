#!/usr/bin/env bash
#
# verify-upstream-sync.sh - prove the vendored crates match `UPSTREAM_COMMIT`.
#
# Extracts `ale-core/` and `ale-modeld/` from the pinned commit of the upstream
# desktop checkout into a temporary directory and runs `diff -rq` against the
# vendored trees. Any drift is printed (files that differ and files only present
# on one side) and the script exits non-zero. `.DS_Store` is ignored because it
# is a macOS artifact that `git archive` never carries.
#
# CI runs this from `.github/workflows/build.yml` (job `upstream-sync`) with the
# upstream repository checked out at the pinned sha into `upstream/`.
#
# Usage:
#   ./scripts/verify-upstream-sync.sh <upstream-repo-path>

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
PIN_FILE="$REPO_ROOT/UPSTREAM_COMMIT"

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

note() {
    printf '==> %s\n' "$*"
}

[ "$#" -eq 1 ] || die "usage: $0 <upstream-repo-path>"
UPSTREAM=$1
[ -d "$UPSTREAM/.git" ] || die "$UPSTREAM is not a git checkout (expected $UPSTREAM/.git)"
[ -f "$PIN_FILE" ] || die "$PIN_FILE is missing"

COMMIT=$(tr -d '[:space:]' < "$PIN_FILE")
[[ "$COMMIT" =~ ^[0-9a-f]{40}$ ]] \
    || die "UPSTREAM_COMMIT must contain exactly one 40-character lowercase sha (got: '$COMMIT')"

git -C "$UPSTREAM" rev-parse --verify --quiet "${COMMIT}^{commit}" >/dev/null \
    || die "pinned commit $COMMIT does not exist in $UPSTREAM"

SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT

note "comparing vendored crates against $UPSTREAM @ $COMMIT"
git -C "$UPSTREAM" archive "$COMMIT" ale-core ale-modeld | tar -x -C "$SCRATCH"

STATUS=0
for crate in ale-core ale-modeld; do
    [ -d "$REPO_ROOT/$crate" ] || die "vendored $REPO_ROOT/$crate is missing"
    if diff -rq --exclude='.DS_Store' "$SCRATCH/$crate" "$REPO_ROOT/$crate"; then
        note "$crate matches $COMMIT"
    else
        printf 'FAIL: %s is out of sync with %s (see diff above); re-run scripts/sync-upstream.sh\n' \
            "$crate" "$COMMIT" >&2
        STATUS=1
    fi
done

if [ "$STATUS" -eq 0 ]; then
    note "vendored crates are byte-identical to upstream $COMMIT"
fi
exit "$STATUS"
