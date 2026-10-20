#!/usr/bin/env bash
# Weekly Postgres backup to an S3-compatible store (AWS S3, Cloudflare R2,
# Backblaze B2, MinIO).
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
# The `backup` compose service (`judgebot backup serve`, COMPOSE_PROFILES=backup)
# takes the same backup on its own schedule with no cron, and writes the same
# objects: each lists, fetches and counts the other's backups
# (docs/DEPLOYMENT.md §6). This script is for a host that keeps cron instead.
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
    alert "judgebot database backup failed on $(hostname 2>/dev/null || echo this host) (exit $status). The last good dump is still in the bucket."
  fi
}
trap finish EXIT

[ -f .env.deploy ] || die "no .env.deploy at $root (copy .env.deploy.example)"
# A later line wins, a blank one too: a pasted example line would unset what
# the line above it set.
twice="$(grep -Eo '^[[:space:]]*(export[[:space:]]+)?BACKUP_S3_[A-Z_]+=' .env.deploy |
  sed -E 's/^[[:space:]]*(export[[:space:]]+)?//; s/=$//' | sort | uniq -d | tr '\n' ' ' || true)"
[ -z "$twice" ] || die ".env.deploy sets ${twice% } more than once, and the last line wins (a blank one unsets it): keep one line each"
set -a
# shellcheck disable=SC1091
source .env.deploy
set +a

# The names before 2.0.0, when the store was always Cloudflare R2: refused, as
# `judgebot backup` refuses them, naming the line that replaces each and never
# its value.
retired=""
for pair in R2_ENDPOINT:BACKUP_S3_ENDPOINT R2_BUCKET:BACKUP_S3_BUCKET \
  R2_ACCESS_KEY_ID:BACKUP_S3_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY:BACKUP_S3_SECRET_ACCESS_KEY; do
  old="${pair%%:*}" new="${pair#*:}"
  [ -n "${!old:-}" ] || continue
  if [ -n "${!new:-}" ]; then
    retired+=" $old: remove the line ($new is set);"
  else
    retired+=" $old: rename the line to $new=, keeping its value;"
  fi
done
[ -z "$retired" ] || die "the R2_* names are no longer read since 2.0.0. In .env.deploy:${retired%;}"

for v in BACKUP_S3_ENDPOINT BACKUP_S3_BUCKET BACKUP_S3_ACCESS_KEY_ID BACKUP_S3_SECRET_ACCESS_KEY; do
  [ -n "${!v:-}" ] || die "$v is not set in .env.deploy"
done

# What `judgebot backup` checks, in its words.
[[ "$BACKUP_S3_ENDPOINT" =~ ^https?://[^/?#@[:space:]]+/?$ ]] ||
  die "BACKUP_S3_ENDPOINT: an https:// URL with nothing after the host and port, such as https://s3.eu-west-1.amazonaws.com"
[[ "$BACKUP_S3_BUCKET" =~ ^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$ ]] ||
  die "BACKUP_S3_BUCKET: a bucket name: 3 to 63 lowercase letters, digits, hyphens and dots"
region="${BACKUP_S3_REGION:-auto}"
[[ "$region" =~ ^[a-z0-9_-]{1,63}$ ]] ||
  die "BACKUP_S3_REGION: the store's region, such as auto (Cloudflare R2), eu-west-1 (AWS) or us-west-004 (Backblaze B2): lowercase letters, digits, '-' and '_'"
case "${BACKUP_S3_ADDRESSING:-path}" in
  path) path_style=true ;;
  virtual-hosted)
    path_style=false
    if [[ "$BACKUP_S3_ENDPOINT" == https://* && "$BACKUP_S3_BUCKET" == *.* ]]; then
      die "BACKUP_S3_ADDRESSING: set it to path: the store's certificate does not cover a virtual-hosted name for a bucket with dots (BACKUP_S3_BUCKET)"
    fi
    ;;
  *) die "BACKUP_S3_ADDRESSING: path or virtual-hosted" ;;
esac
# rclone's provider only tunes its quirks; the requests are plain S3 either way.
case "$BACKUP_S3_ENDPOINT" in
  *.r2.cloudflarestorage.com | *.r2.cloudflarestorage.com/) provider=Cloudflare ;;
  *.amazonaws.com | *.amazonaws.com/) provider=AWS ;;
  *) provider=Other ;;
esac

min_bytes="${BACKUP_MIN_BYTES:-20000000}"
keep_days="${BACKUP_KEEP_DAYS:-60}"
prefix="${BACKUP_PREFIX:-db}"
keep_local="${BACKUP_KEEP_LOCAL:-}"

tmp="$(mktemp -d)"

# rclone runs in a container so the host needs nothing but Docker. Credentials
# are passed by NAME only: `-e VAR` inherits the already-exported value instead
# of writing it into the docker CLI's argv, where any local user could read it
# out of `ps` for the duration of the transfer.
export RCLONE_CONFIG_STORE_ENDPOINT="$BACKUP_S3_ENDPOINT"
export RCLONE_CONFIG_STORE_ACCESS_KEY_ID="$BACKUP_S3_ACCESS_KEY_ID"
export RCLONE_CONFIG_STORE_SECRET_ACCESS_KEY="$BACKUP_S3_SECRET_ACCESS_KEY"
rclone() {
  docker run --rm \
    -e RCLONE_CONFIG_STORE_TYPE=s3 \
    -e RCLONE_CONFIG_STORE_PROVIDER="$provider" \
    -e RCLONE_CONFIG_STORE_REGION="$region" \
    -e RCLONE_CONFIG_STORE_FORCE_PATH_STYLE="$path_style" \
    -e RCLONE_CONFIG_STORE_NO_CHECK_BUCKET=true \
    -e RCLONE_CONFIG_STORE_ENDPOINT \
    -e RCLONE_CONFIG_STORE_ACCESS_KEY_ID \
    -e RCLONE_CONFIG_STORE_SECRET_ACCESS_KEY \
    -v "$tmp:/staging" \
    -v "$root:/repo" \
    rclone/rclone:1.71 "$@"
}

case "$mode" in
  list)
    rclone lsf "store:$BACKUP_S3_BUCKET/$prefix" | sort
    ;;

  fetch)
    name="${2:?usage: backup-db.sh fetch <object-name>   (see: backup-db.sh list)}"
    rclone copyto "store:$BACKUP_S3_BUCKET/$prefix/$name" "/repo/$name"
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

    log "uploading to store:$BACKUP_S3_BUCKET/$prefix/$name"
    rclone copyto "/staging/$name" "store:$BACKUP_S3_BUCKET/$prefix/$name"

    # Prune only after a successful upload, so a failed run never costs
    # history. --min-age reads the object's stored mtime, so the object just
    # written is zero days old and cannot be caught here.
    log "pruning backups older than ${keep_days}d"
    rclone delete --min-age "${keep_days}d" "store:$BACKUP_S3_BUCKET/$prefix"

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
