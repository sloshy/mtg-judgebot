#!/usr/bin/env bash
# Scheduled data refresh: Scryfall cards and rulings, the current Comprehensive
# Rules release, embeddings for whatever changed, and any new card-symbol emoji.
#
# Everything happens inside the published image (`judge-ingest refresh`, the
# compose service `refresh`); the host needs only Docker. Each step is
# idempotent, a new CR is detected from Wizards' rules page and skipped when the
# database already has it, and only rules whose text changed are re-embedded,
# so a run that finds nothing new costs a Scryfall download and no API spend.
#
#   scripts/refresh-data.sh                 run every step
#   scripts/refresh-data.sh cards           run one ingest subcommand instead
#   scripts/refresh-data.sh rules latest    (anything judge-ingest accepts)
#
# Cron:  30 5 * * *  /path/to/repo/scripts/refresh-data.sh >> ~/judgebot-refresh.log 2>&1
#
# Exits non-zero if any step failed, so a scheduler's on-error hook fires.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }
die() { log "ERROR: $*" >&2; exit 1; }

[ -f .env ] || die "no .env at $root"

# One run at a time. Two concurrent CR loads would race on the stale-row delete,
# and Scryfall asks clients not to download bulk files in parallel. mkdir is
# atomic on every filesystem and needs no flock binary (Synology lacks one).
lock="$root/.refresh.lock"
if ! mkdir "$lock" 2>/dev/null; then
  die "another refresh is running (or crashed without removing $lock)"
fi
trap 'rmdir "$lock"' EXIT

log "refresh start: ${*:-refresh}"
# --pull missing: use the image `docker compose pull` already fetched for
# bot/api, and never fall back to building on the host (docs/DEPLOYMENT.md §1).
docker compose run --rm --pull missing refresh "$@"
log "refresh done"
