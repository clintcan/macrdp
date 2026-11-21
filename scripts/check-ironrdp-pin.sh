#!/usr/bin/env bash
# Check that macrdp takes IronRDP only from the protected `macrdp*` branches of
# clintcan/IronRDP, at one pinned commit.
#
# Why: every ironrdp-* crate comes from clintcan/IronRDP by `rev` (#225). GitHub
# serves any commit of the IronRDP fork network through that repo's URL, so a rev
# that names clintcan/IronRDP can still point at code that is on no branch of it,
# and cargo-deny checks only the URL. The `macrdp*` branches there can't be
# rewritten, deleted, or updated by anyone but the owner (repository rulesets),
# so a rev on one of them is reviewed code.
#
# Checks, in order:
#   1. Every IronRDP git dependency in a tracked Cargo.toml uses
#      https://github.com/clintcan/IronRDP.git with a full 40-hex `rev`
#      (no branch, no tag, no short rev, no other fork).
#   2. All of those revs are the same commit.
#   3. Every IronRDP source in Cargo.lock is that commit, from that URL.
#   4. The commit is on a `macrdp*` branch of clintcan/IronRDP.
#
# Usage: scripts/check-ironrdp-pin.sh [--checkout DIR]
#   --checkout DIR  leave the pinned commit checked out in DIR (an empty or
#                   missing directory), for a step that builds or tests it.
#
# Runs in ci.yml (pull requests and main), security.yml (main and the daily
# schedule) and release.yml (before any release build). Needs git and network
# access to github.com. Portable to the bash 3.2 that macOS ships.

set -euo pipefail

FORK="https://github.com/clintcan/IronRDP.git"
checkout=""
if [ "${1:-}" = "--checkout" ]; then
    checkout="${2:?--checkout needs a directory}"
elif [ $# -gt 0 ]; then
    echo "usage: $0 [--checkout DIR]" >&2
    exit 2
fi

fail() {
    if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error::$*"; fi
    echo "check-ironrdp-pin: $*" >&2
    exit 1
}

cd "$(git rev-parse --show-toplevel)"

# 1. Every IronRDP git dependency in a tracked manifest (comments skipped).
deps=$(git ls-files -z -- 'Cargo.toml' '*/Cargo.toml' \
    | xargs -0 grep -nHiE '^[^#]*git[[:space:]]*=[[:space:]]*"[^"]*ironrdp[^"]*"' || true)
[ -n "$deps" ] || fail "no IronRDP git dependency found in any Cargo.toml"

revs=""
while IFS= read -r line; do
    url=$(printf '%s\n' "$line" | sed -nE 's/.*git[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p')
    rev=$(printf '%s\n' "$line" | sed -nE 's/.*rev[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p')
    [ "$url" = "$FORK" ] || fail "IronRDP dependency not from $FORK: $line"
    if printf '%s\n' "$line" | grep -qE '(branch|tag)[[:space:]]*='; then
        fail "IronRDP dependency pinned by branch or tag, not rev: $line"
    fi
    printf '%s\n' "$rev" | grep -qE '^[0-9a-f]{40}$' || fail "IronRDP rev is not a full 40-hex commit: $line"
    revs="$revs$rev"$'\n'
done <<EOF
$deps
EOF

# 2. One commit for all of them.
distinct=$(printf '%s' "$revs" | sort -u)
[ "$(printf '%s\n' "$distinct" | wc -l | tr -d ' ')" = 1 ] \
    || fail "IronRDP dependencies pin more than one rev: $(printf '%s' "$distinct" | tr '\n' ' ')"
rev="$distinct"
count=$(printf '%s' "$revs" | grep -c .)

# 3. Cargo.lock agrees: every IronRDP git source is that commit from the fork.
lock=$(grep -iE '^source = "git\+[^"]*ironrdp' Cargo.lock || true)
[ -n "$lock" ] || fail "Cargo.lock has no IronRDP git source"
bad=$(printf '%s\n' "$lock" | grep -vxF "source = \"git+$FORK?rev=$rev#$rev\"" || true)
[ -z "$bad" ] || fail "Cargo.lock has an IronRDP source other than $FORK at $rev: $(printf '%s' "$bad" | head -1)"

# 4. The commit is on a protected macrdp* branch of the fork.
dir="$checkout"
if [ -z "$dir" ]; then
    dir=$(mktemp -d)
    # Best effort: a leftover temporary directory must never fail a check that
    # passed. (On GitHub's macOS runners git's background maintenance was still
    # writing commit-graph files when the cleanup ran, "Directory not empty",
    # and the failed cleanup became this script's exit status.)
    trap 'rm -rf "$dir" 2>/dev/null || true' EXIT
fi
# No background maintenance, so nothing keeps writing into $dir after a command
# returns (see the trap above).
g() {
    git -c gc.auto=0 -c maintenance.auto=false -c fetch.writeCommitGraph=false -C "$dir" "$@"
}
git init -q "$dir"
g fetch -q --filter=blob:none "$FORK" '+refs/heads/macrdp*:refs/remotes/fork/macrdp*'
g for-each-ref --format='%(refname)' refs/remotes/fork | grep -q . \
    || fail "no macrdp* branch found on $FORK"
g branch -r --contains "$rev" 2>/dev/null | grep -q . \
    || fail "pinned rev $rev is on no macrdp* branch of $FORK"

# Not the tip of any branch: allowed (old tags must stay buildable), but worth a
# look in review, since an older commit may lack a later fix.
if ! g for-each-ref --format='%(objectname)' refs/remotes/fork | grep -qx "$rev"; then
    msg="pinned rev $rev is not the tip of any macrdp* branch; check that an older commit is intended"
    if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::warning::$msg"; else echo "note: $msg" >&2; fi
fi

if [ -n "$checkout" ]; then
    g checkout -q "$rev"
fi

echo "ok: $count IronRDP pins, all $rev, on a macrdp* branch of $FORK"
