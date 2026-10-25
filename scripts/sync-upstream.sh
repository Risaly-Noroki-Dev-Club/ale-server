#!/usr/bin/env bash
#
# sync-upstream.sh - vendor ale-core/ale-modeld from an upstream desktop checkout.
#
# Replaces the local `ale-core/` and `ale-modeld/` trees with the contents of
# the pinned upstream commit (`UPSTREAM_COMMIT` by default) using `git archive`,
# so the vendored copies stay byte-identical to the desktop repository. The
# upstream checkout must be clean: `git archive` only carries committed state,
# and a dirty working tree means the pinned sha no longer describes what you
# are looking at.
#
# Usage:
#   ./scripts/sync-upstream.sh <upstream-repo-path> [commit]
#
# Arguments:
#   <upstream-repo-path>  path to an `ale-my-eyes-desktop` git checkout
#   [commit]              upstream commit to vendor (default: UPSTREAM_COMMIT)
#
# After running this script:
#   1. update `UPSTREAM_COMMIT` to the new 40-character sha (the script prints it)
#   2. update `docs/UPSTREAM.md` (source table, vendored content, caveats and the
#      verification table you actually ran)
#   3. refresh `Cargo.lock`:
#        cargo check -p ale-core -p ale-modeld --no-default-features
#   4. run `./scripts/verify-upstream-sync.sh <upstream-repo-path>`
#   5. fix dependants in `ale-device/` and `ale-server/` (their owners), then run
#      their checks and the workspace verification from docs/UPSTREAM.md
#
# This script does not commit anything.

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

warn() {
    printf 'warning: %s\n' "$*" >&2
}

[ "$#" -ge 1 ] || die "usage: $0 <upstream-repo-path> [commit]"
UPSTREAM=$1
[ "$#" -le 2 ] || die "usage: $0 <upstream-repo-path> [commit]"
[ -d "$UPSTREAM/.git" ] || die "$UPSTREAM is not a git checkout (expected $UPSTREAM/.git)"

if [ "$#" -eq 2 ]; then
    COMMIT=$2
elif [ -f "$PIN_FILE" ]; then
    COMMIT=$(tr -d '[:space:]' < "$PIN_FILE")
else
    die "no commit argument and $PIN_FILE is missing"
fi
COMMIT=$(printf '%s' "$COMMIT" | tr -d '[:space:]')
[ -n "$COMMIT" ] || die "empty commit; pass one explicitly or fix UPSTREAM_COMMIT"

git -C "$UPSTREAM" rev-parse --verify --quiet "${COMMIT}^{commit}" >/dev/null \
    || die "unknown commit in $UPSTREAM: $COMMIT"
SHA=$(git -C "$UPSTREAM" rev-parse "${COMMIT}^{commit}")

if [ -n "$(git -C "$UPSTREAM" status --porcelain)" ]; then
    git -C "$UPSTREAM" status --short >&2
    die "upstream working tree is dirty; commit or discard the changes first (only committed state is vendored)"
fi

note "vendoring ale-core and ale-modeld from $UPSTREAM @ $SHA"
rm -rf "$REPO_ROOT/ale-core" "$REPO_ROOT/ale-modeld"
git -C "$UPSTREAM" archive "$SHA" ale-core ale-modeld | tar -x -C "$REPO_ROOT"
COUNT=$(find "$REPO_ROOT/ale-core" "$REPO_ROOT/ale-modeld" -type f | wc -l | tr -d '[:space:]')
note "extracted $COUNT files into $REPO_ROOT/ale-core and $REPO_ROOT/ale-modeld"

CURRENT=$(tr -d '[:space:]' < "$PIN_FILE" 2>/dev/null || true)
if [ "$CURRENT" != "$SHA" ]; then
    warn "UPSTREAM_COMMIT is still '$CURRENT'"
    printf '    update it with: printf %%s\\\\n %s > UPSTREAM_COMMIT\n' "$SHA"
fi

cat <<EOF
Next steps (nothing has been committed):
  1. update UPSTREAM_COMMIT to $SHA (if the warning above fired)
  2. update docs/UPSTREAM.md: source table, vendored content, caveats, verification table
  3. cargo check -p ale-core -p ale-modeld --no-default-features   # refresh Cargo.lock
  4. $SCRIPT_DIR/verify-upstream-sync.sh $UPSTREAM
  5. fix dependants (ale-device/ale-server owners) and run:
       cargo check -p ale-core
       cargo test -p ale-core
       cargo check -p ale-modeld --no-default-features
       cargo test -p ale-modeld --no-default-features
       cargo fmt -p ale-core -p ale-modeld -- --check
EOF
