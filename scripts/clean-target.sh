#!/usr/bin/env bash
# Reclaim disk from cargo's target/ without a cold rebuild. Run it by hand or
# weekly from cron, and after a toolchain bump, a profile change or a large
# dependency update (each orphans a build's worth of output). Never from a git
# hook: it would put the rebuild back into the push.
#
#   scripts/clean-target.sh [--days N] [--dry-run]
#
# - target/debug/incremental is deleted: it is the largest part, and rebuilds
#   on demand.
# - Artifacts older than N days (default 14) are dropped with cargo-sweep, when
#   installed (`cargo install cargo-sweep`). Without it, only the first step
#   runs and the script says so.
# - Git worktree entries whose directory is gone are pruned (the check worktree's,
#   among any others).
#
# Do not run it during a build: nothing takes cargo's lock.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

days=14
dry=0
while [ $# -gt 0 ]; do
  case "$1" in
    --days)
      days="${2:?--days needs a number}"
      shift 2
      ;;
    --dry-run)
      dry=1
      shift
      ;;
    *)
      echo "usage: scripts/clean-target.sh [--days N] [--dry-run]" >&2
      exit 2
      ;;
  esac
done
case "$days" in '' | *[!0-9]*)
  echo "--days must be a whole number" >&2
  exit 2
  ;;
esac
if [ $((10#$days)) -eq 0 ]; then
  echo "--days 0 would sweep every artifact: use rm -rf target for a cold rebuild" >&2
  exit 2
fi

target="${CARGO_TARGET_DIR:-$root/target}"
size() { du -sh "$target" 2> /dev/null | cut -f1; }

echo "target/ before: $(size)"
if [ $dry = 1 ]; then
  echo "would delete $target/debug/incremental ($(du -sh "$target/debug/incremental" 2> /dev/null | cut -f1))"
else
  rm -rf "$target/debug/incremental"
fi

if cargo sweep --version > /dev/null 2>&1; then
  # No path argument: this project alone, not every Cargo.toml beneath it. A
  # failure is reported but does not skip the prune and the size line.
  sweep=(cargo sweep --time "$days")
  if [ $dry = 1 ]; then sweep+=(--dry-run); fi
  "${sweep[@]}" || echo "cargo sweep failed" >&2
else
  echo "cargo-sweep is not installed: stale deps/ output stays (cargo install cargo-sweep)" >&2
fi

if [ $dry = 0 ]; then git worktree prune; fi
echo "target/ after: $(size)"
