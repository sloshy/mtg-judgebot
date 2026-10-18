#!/usr/bin/env bash
# A deploy without downtime (blue-green, docs/DECISIONS.md D29): start the new
# judgebot image beside the running container, wait until Docker reports it
# healthy, then stop the old one. A major upgrade (D32) is the same command
# with a short downtime: see below.
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
#
# The new image says first what its migrations are (`judgebot ingest
# upgrade`). A major upgrade's cannot run beside the old release, so: a backup
# is taken when one is configured (.env.deploy's R2_BUCKET; DEPLOY_SKIP_BACKUP=1
# skips it), the new container starts beside the old one and answers every
# question with a maintenance reply, the old one is stopped as soon as the new
# one listens, and the new one migrates once it has gone and becomes healthy.
# From a 1.x release, whose processes hold no schema lock, the old container
# is stopped before the new one starts. Once the old one is stopped there is
# nothing to fall back to: a failure then says so, and the way back is the
# backup.
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
  elif [ "$stage" = major ]; then
    kept="this was a major upgrade and the old container has stopped, so nothing is serving questions; the database may be partly migrated. Check docker compose logs $service; to go back, restore the backup taken before it (docs/DEPLOYMENT.md §6) and deploy the previous release"
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
    sed -n '2,33p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *) die "unknown argument '$1' (scripts/deploy.sh [--build])" ;;
esac

mapfile -t running < <(docker compose ps -q "$service")
case "${#running[@]}" in
  0) old="" ;;
  1) old="${running[0]}" ;;
  *) die "${#running[@]} $service containers are running, so an earlier deploy did not finish; stop all but one (docker ps), then deploy again" ;;
esac

# What the new image's migrations are; it changes nothing.
probe_log="$(mktemp)"
upgrade="$(timeout 120 docker compose run --rm --no-deps -T "$service" ingest upgrade 2>"$probe_log" | sed -n 's/^upgrade: //p' | tail -n 1 || true)"
case "$upgrade" in
  in-place | major | major-from-1.x)
    rm -f "$probe_log"
    log "the new image's migrations: $upgrade"
    ;;
  *)
    tail -n 20 "$probe_log" >&2
    rm -f "$probe_log"
    die "could not read what the new image's migrations are: docker compose run --rm --no-deps $service ingest upgrade"
    ;;
esac

# A major upgrade cannot be rolled back without the database as it was.
if [ "$upgrade" != in-place ]; then
  if [ "${DEPLOY_SKIP_BACKUP:-}" = 1 ]; then
    log "a major upgrade, and DEPLOY_SKIP_BACKUP=1: no backup taken first"
  elif [ -f .env.deploy ] && grep -Eq '^R2_BUCKET=.+' .env.deploy; then
    log "a major upgrade: taking a backup first"
    docker compose run --rm backup run ||
      die "the backup before a major upgrade failed; fix it, or deploy with DEPLOY_SKIP_BACKUP=1 to go on without one"
  else
    log "a major upgrade and no backup configured (.env.deploy R2_BUCKET): it cannot be rolled back without one (docs/DEPLOYMENT.md §6)"
  fi
fi

# The old container may come from a compose file older than its
# stop_grace_period, so the current file's is passed (read from the new
# container once there is one; the service's own value until then).
grace_period="$(docker compose config --format json 2>/dev/null | sed -n 's/.*"stop_grace_period": *"\([^"]*\)".*/\1/p' | head -n 1)"
grace=""
if [[ "$grace_period" =~ ^(([0-9]+)m)?(([0-9]+)s)?$ ]] && [ -n "$grace_period" ]; then
  grace=$((${BASH_REMATCH[2]:-0} * 60 + ${BASH_REMATCH[4]:-0}))
fi

# Print the new container's log and fail; finish removes it, or says what is left.
abandon() {
  log "ERROR: $1" >&2
  [ -n "$new" ] && { docker logs --tail 40 "$new" >&2 2>&1 || true; }
  exit 1
}

# The new container's state, or abandon when it has exited or restarted.
inspect() {
  local state
  state="$(docker inspect -f '{{.State.Status}} {{.RestartCount}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$new" 2>/dev/null)" ||
    abandon "the new container is gone"
  read -r status restarts health <<<"$state"
  [ "$restarts" -eq 0 ] || abandon "the new container restarted (it exited: see its log)"
  [ "$status" = running ] || abandon "the new container is $status, health $health"
}

# Wait until the new container is healthy. A major upgrade's is reported
# unhealthy while it waits and migrates, so it may pass through that state.
wait_healthy() {
  local deadline=$((SECONDS + wait_secs))
  while :; do
    inspect
    case "$health" in
      healthy | none) return 0 ;;
      starting) ;;
      unhealthy) [ "$upgrade" != in-place ] || abandon "the new container is unhealthy" ;;
    esac
    [ "$SECONDS" -lt "$deadline" ] || abandon "the new container was not healthy within ${wait_secs}s"
    sleep 2
  done
}

# Whether the new container answers with its maintenance reply (or is already
# open): /api/health says `maintenance:` once the listener, and Discord's
# interactions endpoint when it is served, are up.
maintenance_up() {
  docker exec "$new" bash -c 'exec 3<>/dev/tcp/127.0.0.1/8787 &&
    printf "GET /api/health HTTP/1.0\r\nHost: localhost\r\n\r\n" >&3 && cat <&3' 2>/dev/null |
    grep -q -e '^HTTP/[0-9.]* 200 ' -e '^maintenance:'
}

# With no listener (the gateway with no HTTP role) the healthcheck passes
# whatever the process does, so a major upgrade's end is read from its log.
wait_migrated() {
  local deadline=$((SECONDS + wait_secs))
  until docker logs "$new" 2>&1 | grep -q -e 'schema migrated; answering questions' -e 'migrated the schema; joining it'; do
    inspect
    [ "$SECONDS" -lt "$deadline" ] || abandon "the new container did not migrate within ${wait_secs}s"
    sleep 2
  done
}

stop_old() {
  log "stopping the old container, which finishes its answers first (up to ${grace:-10}s)"
  docker stop --time "${grace:-10}" "$old" >/dev/null
  docker rm "$old" >/dev/null
}

new=""
if [ -z "$old" ]; then
  log "no $service container is running: starting the services"
  # --remove-orphans: the services before judgebot (`bot`, `api`, D25)
  # would otherwise run beside it, the old bot on the same Discord token.
  # They are removed before anything starts, since a 1.x process holds no
  # schema lock that would keep a major upgrade from migrating under it.
  docker compose up -d --no-start --remove-orphans
  if [ "$upgrade" != in-place ]; then
    # judgebot alone first, so a service that waits for it to be healthy
    # (cloudflared) is not given up on during the migration.
    stage=major
    docker compose up -d --no-deps "$service"
    new="$(docker compose ps -q "$service" | head -n 1)"
    [ -n "$new" ] || die "the new $service container did not start"
    log "waiting up to ${wait_secs}s for the new container to migrate and be healthy"
    wait_migrated
    wait_healthy
  fi
  stage=proxy
  docker compose up -d
  log "deployed: $(docker compose ps --format '{{.Name}} {{.Image}}' "$service")"
  exit 0
fi
if [ "$upgrade" = major-from-1.x ]; then
  # A 1.x process holds no schema lock, so the new one could not tell it is
  # still running: it goes first.
  log "a major upgrade from 1.x: stopping the old container before the new one starts"
  stage=major
  stop_old
  docker compose up -d --no-deps --remove-orphans "$service"
  new="$(docker compose ps -q "$service" | head -n 1)"
  [ -n "$new" ] || die "the new $service container did not start"
  log "waiting up to ${wait_secs}s for the new container to migrate and be healthy"
  wait_migrated
  wait_healthy
else
  # The new container from the current compose file and image, beside the old
  # one (--no-recreate leaves the old one as it is). A stopped container of the
  # service, left by an interrupted deploy, is removed first: --no-recreate
  # would start it, old image and all, as the "new" one.
  docker compose rm -f "$service" >/dev/null
  log "starting the new $service container beside $(docker inspect -f '{{.Name}}' "$old")"
  docker compose up -d --no-deps --no-recreate --remove-orphans --scale "$service=2" "$service"
  new="$(docker compose ps -q "$service" | grep -vx "$old" || true)"
  [ -n "$new" ] || die "the new $service container did not start"
  grace="$(docker inspect -f '{{if .Config.StopTimeout}}{{.Config.StopTimeout}}{{end}}' "$new")"
  if [ "$upgrade" = in-place ]; then
    log "waiting up to ${wait_secs}s for $(docker inspect -f '{{.Name}}' "$new") to be healthy"
    wait_healthy
  else
    # It answers with the maintenance reply and waits for the old one to go;
    # it is stopped once the new one does (or, with no listener, is healthy).
    log "a major upgrade: waiting for the new container to answer with its maintenance reply, then stopping the old one"
    deadline=$((SECONDS + wait_secs))
    until maintenance_up; do
      inspect
      [ "$health" != healthy ] || break
      [ "$SECONDS" -lt "$deadline" ] || abandon "the new container did not answer within ${wait_secs}s"
      sleep 1
    done
  fi
fi

# The proxy is brought up to date while the old container still serves, so
# a proxy whose definition changed is recreated with the old container and
# the new one both behind it. It cannot start while an old judgebot that
# publishes 8787 itself holds the port; the call after the stop starts it.
docker compose up -d --no-deps proxy ||
  log "the proxy did not start yet; trying again once the old container has stopped"
case "$upgrade" in
  in-place)
    log "the new container is healthy"
    stage=stopping
    stop_old
    ;;
  major)
    stage=major
    stop_old
    log "waiting up to ${wait_secs}s for the new container to migrate and be healthy"
    wait_migrated
    wait_healthy
    ;;
esac
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
