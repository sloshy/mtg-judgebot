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
# Runs never overlap: every judge-ingest command that writes data takes the
# refresh lease, an advisory lock in the database, and waits for a run that
# holds it. Postgres drops the lock with the session, so a run that crashed
# leaves nothing to clean up.
#
# Exits non-zero if any step failed, so a scheduler's on-error hook fires. With
# JUDGE_ALERT_WEBHOOK set (.env), a failed run is also posted there.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }
die() { log "ERROR: $*" >&2; exit 1; }

# shellcheck source=scripts/alert.sh
source "$root/scripts/alert.sh"
finish() {
  local status=$?
  [ "$status" -eq 0 ] || alert "judgebot data refresh failed on $(hostname 2>/dev/null || echo this host) (exit $status). See the refresh log."
}
trap finish EXIT

[ -f .env ] || die "no .env at $root"

log "refresh start: ${*:-refresh}"
# Never build on the host (docs/DEPLOYMENT.md §1): `run` builds when an image is
# missing, so require every image `docker compose pull` fetches to be present.
# (`run --pull missing` would say the same, but Compose v2.20, which Synology
# ships, has no such flag.)
images="$(docker compose --profile refresh config --images)"
while read -r image; do
  docker image inspect "$image" >/dev/null 2>&1 || die "image $image is not present; run \`docker compose pull\` first"
done <<<"$images"
docker compose run --rm refresh "$@"
log "refresh done"
