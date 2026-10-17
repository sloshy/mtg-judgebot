#!/usr/bin/env bash
# A deploy without downtime (blue-green, docs/DECISIONS.md D29): start the new
# judgebot image beside the running container, wait until Docker reports it
# healthy, then stop the old one.
#
#   scripts/deploy.sh            pull the image JUDGE_IMAGE_TAG names, then switch
#   scripts/deploy.sh --build    build it from this checkout instead (a dev host)
#
# The new container migrates the schema as it starts. A release's migrations
# keep the previous release working, so the old container goes on answering
# against the new schema while the new one becomes healthy. Then `docker stop`
# sends the old one SIGTERM and it drains: it takes no new work, releases the
# Discord gateway (the new container connects at once, or 15 s later when the
# old one is from a release that does not record a clean release), and finishes
# the answers in flight within two minutes before it exits. The `proxy`
# service on 127.0.0.1:8787 and the tunnel reach whichever is listening.
#
# A new container that is not healthy within DEPLOY_WAIT_SECS (default 300) is
# removed, its last log lines are printed, and the old one keeps serving; the
# exit status is non-zero, and with JUDGE_ALERT_WEBHOOK set (.env) the failure
# is posted there. With no judgebot running it is `up -d --remove-orphans`.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }
die() { log "ERROR: $*" >&2; exit 1; }

# shellcheck source=scripts/alert.sh
source "$root/scripts/alert.sh"
# What a failure leaves: until the old container is told to stop, a new
# container is removed and the old one keeps serving.
new=""
stage=before
finish() {
  local status=$?
  [ "$status" -eq 0 ] && return
  local kept="the running container was kept"
  if [ "$stage" = before ] && [ -n "$new" ]; then
    docker rm -f "$new" >/dev/null 2>&1 || true
  elif [ "$stage" = stopping ]; then
    kept="the old container was being stopped and the new one is serving; check docker compose ps"
  elif [ "$stage" = proxy ]; then
    kept="the new container is serving, but the proxy in front of it did not load proxy/Caddyfile; check docker compose logs proxy"
  fi
  alert "judgebot deploy failed on $(hostname 2>/dev/null || echo this host) (exit $status); $kept."
}
trap finish EXIT
# An interrupted deploy (Ctrl-C, a dropped SSH session) exits through finish.
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

service=judgebot
wait_secs="${DEPLOY_WAIT_SECS:-300}"
case "$wait_secs" in '' | *[!0-9]*) die "DEPLOY_WAIT_SECS must be a number of seconds, not '$wait_secs'" ;; esac
# The proxy mounts proxy/; without a file it can load it answers nothing
# once the old container is gone. Checked before anything changes.
[ -f proxy/Caddyfile ] || die "proxy/Caddyfile is missing: run this from a full checkout (git pull)"
docker compose run --rm --no-deps -T proxy caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null 2>&1 ||
  die "proxy/Caddyfile does not load: docker compose run --rm --no-deps proxy caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile"

case "${1:-}" in
  "")
    log "pulling the $service image"
    docker compose pull "$service"
    ;;
  --build)
    log "building the $service image from this checkout"
    # The commit stamp for the source offer, as docker-compose.yml describes.
    JUDGE_COMMIT="$(git rev-parse HEAD)" \
      JUDGE_DIRTY="$(git diff-index --quiet HEAD -- || echo 1)" \
      docker compose build "$service"
    ;;
  -h | --help)
    sed -n '2,21p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *) die "unknown argument '$1' (scripts/deploy.sh [--build])" ;;
esac

mapfile -t running < <(docker compose ps -q "$service")
case "${#running[@]}" in
  0)
    log "no $service container is running: starting the services"
    # --remove-orphans: the services before judgebot (`bot`, `api`, D25)
    # would otherwise run beside it, the old bot on the same Discord token.
    docker compose up -d --remove-orphans
    exit 0
    ;;
  1) old="${running[0]}" ;;
  *) die "${#running[@]} $service containers are running, so an earlier deploy did not finish; stop all but one (docker ps), then deploy again" ;;
esac

# The new container from the current compose file and image, beside the old
# one (--no-recreate leaves the old one as it is). A stopped container of the
# service, left by an interrupted deploy, is removed first: --no-recreate
# would start it, old image and all, as the "new" one.
docker compose rm -f "$service" >/dev/null
log "starting the new $service container beside $(docker inspect -f '{{.Name}}' "$old")"
docker compose up -d --no-deps --no-recreate --remove-orphans --scale "$service=2" "$service"
new="$(docker compose ps -q "$service" | grep -vx "$old" || true)"
[ -n "$new" ] || die "the new $service container did not start"

# Print the new container's log and fail; finish removes it.
abandon() {
  log "ERROR: $1; keeping the running container" >&2
  docker logs --tail 40 "$new" >&2 2>&1 || true
  exit 1
}

log "waiting up to ${wait_secs}s for $(docker inspect -f '{{.Name}}' "$new") to be healthy"
deadline=$((SECONDS + wait_secs))
while :; do
  state="$(docker inspect -f '{{.State.Status}} {{.RestartCount}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$new" 2>/dev/null)" ||
    abandon "the new container is gone"
  read -r status restarts health <<<"$state"
  [ "$restarts" -eq 0 ] || abandon "the new container restarted (it exited: see its log)"
  case "$status $health" in
    "running healthy" | "running none") break ;;
    "running starting") ;;
    *) abandon "the new container is $status, health $health" ;;
  esac
  [ "$SECONDS" -lt "$deadline" ] || abandon "the new container was not healthy within ${wait_secs}s"
  sleep 2
done

# The old container may come from a compose file older than its
# stop_grace_period, so the new one's (the current file's) is passed.
grace="$(docker inspect -f '{{if .Config.StopTimeout}}{{.Config.StopTimeout}}{{end}}' "$new")"
# The proxy is brought up to date while the old container still serves, so
# a proxy whose definition changed is recreated with the old container and
# the new one both behind it. It cannot start while an old judgebot that
# publishes 8787 itself holds the port; the call after the stop starts it.
docker compose up -d --no-deps proxy ||
  log "the proxy did not start yet; trying again once the old container has stopped"
log "the new container is healthy; stopping the old one, which finishes its answers first (up to ${grace:-10}s)"
stage=stopping
docker stop --time "${grace:-10}" "$old" >/dev/null
docker rm "$old" >/dev/null
# The proxy on 127.0.0.1:8787 is never restarted by a deploy; this starts it
# if it is not running, as on the first deploy from a judgebot container that
# published the port itself. Then it is asked to load proxy/Caddyfile as it
# is now: nothing happens when it already runs it (it watches the file), and
# a proxy that is not running, or kept an older configuration because the
# file stopped loading, fails the deploy, since nothing reaches judgebot
# without it.
stage=proxy
docker compose up -d --no-deps proxy
docker compose exec -T proxy caddy reload --adapter caddyfile --config /etc/caddy/Caddyfile >/dev/null 2>&1 ||
  die "the proxy did not load proxy/Caddyfile"
log "deployed: $(docker inspect -f '{{.Name}} {{.Config.Image}}' "$new")"
