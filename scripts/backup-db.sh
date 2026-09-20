#!/usr/bin/env bash
# Weekly Postgres backup to Cloudflare R2.
#
# The judgebot database is small (~200 MB, tens of MB compressed) but expensive
# to rebuild: a full re-ingest re-parses the Comprehensive Rules and the
# Scryfall bulk file and re-embeds every rule through Voyage, which costs real
# money. So the dump is cheap insurance — and this script refuses to upload one
# that looks truncated, rather than letting a stub become the newest restore
# point and quietly age out the good ones.
#
# Config comes from .env.deploy (see .env.deploy.example), kept separate from
# .env so these credentials never reach the internet-facing containers.
#
#   scripts/backup-db.sh              dump, upload, prune
#   scripts/backup-db.sh list         list stored backups, newest last
#   scripts/backup-db.sh fetch NAME   download one into the repo root
#
# Cron:  15 4 * * 0  /path/to/repo/scripts/backup-db.sh >> ~/judgebot-backup.log 2>&1
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }
die() { log "ERROR: $*" >&2; exit 1; }

mode="${1:-backup}"
tmp=""
# shellcheck source=scripts/alert.sh
source "$root/scripts/alert.sh"
# Installed before the first check below, so a cron run that dies on a missing
# .env.deploy is reported too.
finish() {
  local status=$?
  [ -z "$tmp" ] || rm -rf "$tmp" || true
  # Only the scheduled form: a failed `list` or `fetch` has someone watching.
  if [ "$status" -ne 0 ] && [ "$mode" = backup ]; then
    alert "judgebot database backup failed on $(hostname 2>/dev/null || echo this host) (exit $status). The last good dump is still in R2."
  fi
}
trap finish EXIT

[ -f .env.deploy ] || die "no .env.deploy at $root (copy .env.deploy.example)"
set -a
# shellcheck disable=SC1091
source .env.deploy
set +a

for v in R2_BUCKET R2_ENDPOINT R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY; do
  [ -n "${!v:-}" ] || die "$v is not set in .env.deploy"
done

min_bytes="${BACKUP_MIN_BYTES:-20000000}"
keep_days="${BACKUP_KEEP_DAYS:-60}"
prefix="${BACKUP_PREFIX:-db}"
keep_local="${BACKUP_KEEP_LOCAL:-}"

tmp="$(mktemp -d)"

# rclone runs in a container so the host needs nothing but Docker. Credentials
# are passed by NAME only: `-e VAR` inherits the already-exported value instead
# of writing it into the docker CLI's argv, where any local user could read it
# out of `ps` for the duration of the transfer.
export RCLONE_CONFIG_R2_ENDPOINT="$R2_ENDPOINT"
export RCLONE_CONFIG_R2_ACCESS_KEY_ID="$R2_ACCESS_KEY_ID"
export RCLONE_CONFIG_R2_SECRET_ACCESS_KEY="$R2_SECRET_ACCESS_KEY"
rclone() {
  docker run --rm \
    -e RCLONE_CONFIG_R2_TYPE=s3 \
    -e RCLONE_CONFIG_R2_PROVIDER=Cloudflare \
    -e RCLONE_CONFIG_R2_REGION=auto \
    -e RCLONE_CONFIG_R2_NO_CHECK_BUCKET=true \
    -e RCLONE_CONFIG_R2_ENDPOINT \
    -e RCLONE_CONFIG_R2_ACCESS_KEY_ID \
    -e RCLONE_CONFIG_R2_SECRET_ACCESS_KEY \
    -v "$tmp:/staging" \
    -v "$root:/repo" \
    rclone/rclone:1.71 "$@"
}

case "$mode" in
  list)
    rclone lsf "r2:$R2_BUCKET/$prefix" | sort
    ;;

  fetch)
    name="${2:?usage: backup-db.sh fetch <object-name>   (see: backup-db.sh list)}"
    rclone copyto "r2:$R2_BUCKET/$prefix/$name" "/repo/$name"
    log "wrote $root/$name"
    ;;

  backup)
    stamp="$(date -u +%Y%m%dT%H%M%SZ)"
    name="judgebot-${stamp}.dump.gz"

    log "dumping database"
    # -Fc is the custom format pg_restore wants. pipefail makes a pg_dump
    # failure abort here rather than producing a short file that only the size
    # guard below would catch.
    docker compose exec -T db \
      pg_dump -U judgebot -Fc judgebot | gzip -9 > "$tmp/$name"

    size="$(stat -c %s "$tmp/$name")"
    [ "$size" -ge "$min_bytes" ] \
      || die "dump is ${size} bytes, below the ${min_bytes} floor — refusing to upload"
    log "dump ok: $name (${size} bytes)"

    log "uploading to r2:$R2_BUCKET/$prefix/$name"
    rclone copyto "/staging/$name" "r2:$R2_BUCKET/$prefix/$name"

    # Prune only after a successful upload, so a failed run never costs
    # history. --min-age reads the object's stored mtime, so the object just
    # written is zero days old and cannot be caught here.
    log "pruning backups older than ${keep_days}d"
    rclone delete --min-age "${keep_days}d" "r2:$R2_BUCKET/$prefix"

    if [ -n "$keep_local" ]; then
      mkdir -p "$keep_local"
      cp "$tmp/$name" "$keep_local/$name"
      log "local copy kept at $keep_local/$name"
    fi

    log "done"
    ;;

  *)
    die "unknown command ${1:?}; expected backup, list or fetch"
    ;;
esac
