#!/usr/bin/env bash
# Everything that has to be true before a change is believed.
#
# The point of this file is that "the fix works" is a claim someone can check in
# one command, and that a claim never rests on a summary of the work. Twice in
# this project a chained edit script asserted, failed to write, and was read as
# success; once a `return` in the middle of an onMount shipped empty startup
# state past every check that existed. None of those were caught by reasoning.
#
#   ./scripts/verify.sh          fast: tests, checks, debug builds
#   ./scripts/verify.sh --full   also the release-only harness runs
set -uo pipefail
cd "$(dirname "$0")/.."
full=${1:-}
fail=0
step() { printf '\n=== %s\n' "$1"; }

step "core tests"
if cargo test -p hwa-core 2>&1 | tail -20; then :; else fail=1; fi

step "workspace check"
cargo check --workspace --all-targets 2>&1 | grep -E "^(error|warning: unused)" && fail=1 || echo "clean"

step "frontend types"
(cd editor && bun run check 2>&1 | tail -3) || fail=1

step "debug builds"
cargo build -p hwa-editor -p hwa-preview 2>&1 | tail -2 || fail=1

if [ "$full" = "--full" ]; then
    step "release harness"
    cargo build --release -p hwa-preview --bin editor_export 2>&1 | tail -1 || fail=1
fi

printf '\n=== %s\n' "$([ $fail -eq 0 ] && echo 'PASS' || echo 'FAIL')"
exit $fail
