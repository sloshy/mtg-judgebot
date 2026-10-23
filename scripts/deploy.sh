#!/usr/bin/env bash
# A deploy without downtime (blue-green, docs/DECISIONS.md D29): start the new
# judgebot image beside the running container, wait until Docker reports it
# healthy, then stop the old one. A major upgrade (D32) is the same command
# with a short downtime: see below.
#
#   scripts/deploy.sh            pull the image JUDGE_IMAGE_TAG names, then switch
#   scripts/deploy.sh --build    build it from this checkout instead (a dev host)
#   scripts/deploy.sh --db       replace the db container with the one
#                                docker-compose.yml describes (see the end),
#                                or start it when there is none
#   scripts/deploy.sh --proxy    replace the proxy and edge (Caddy) containers
#                                with the ones docker-compose.yml describes
#
# It is the one way to apply a change: a new release, an edited .env,
# judge.toml or docker-compose.yml, a profile turned on in COMPOSE_PROFILES.
# The new judgebot container reads them all, and every other service but db is
# brought up to date with them at the end. A first start is `--db` (which
# starts the database alone when there is none), `docker compose run --rm
# refresh init` (the first data load), then this; it waits for judgebot to be
# healthy as any deploy does.
#
# The new container migrates the schema as it starts. A release's migrations
# keep the previous release working, so the old container goes on answering
# against the new schema while the new one becomes healthy. Then `docker stop`
# sends the old one SIGTERM and it drains: it takes no new work, releases the
# Discord gateway (the new container connects at once, or 15 s later when the
# old one is from a release that does not record a clean release), and finishes
# the answers in flight within two minutes before it exits. The `proxy`
# service on 127.0.0.1:8787, `edge` and the tunnel reach whichever listens.
#
# A new container that is not healthy within DEPLOY_WAIT_SECS (default 300) is
# removed, its last log lines are printed, and the old one keeps serving; the
# exit status is non-zero, and with JUDGE_ALERT_WEBHOOK set (.env) the failure
# is posted there. With no judgebot running it is `up -d --remove-orphans`.
#
# The new image says first what its migrations are (`judgebot ingest
# upgrade`). A major upgrade's cannot run beside the old release, so: a backup
# is taken when one is configured (.env.deploy's BACKUP_S3_BUCKET;
# DEPLOY_SKIP_BACKUP=1 skips it), the new container starts beside the old one and answers every
# question with a maintenance reply, the old one is stopped as soon as the new
# one listens, and the new one migrates once it has gone and becomes healthy.
# From a 1.x release, whose processes hold no schema lock, the old container
# is stopped before the new one starts. Once the old one is stopped there is
# nothing to fall back to: a failure then says so, and the way back is the
# backup. With JUDGE_AUTO_MIGRATE=false the new container would never migrate
# a major, so it is refused before anything changes, with the steps to
# migrate by hand first.
#
# The backup's settings are checked before anything changes too: the
# pre-2.0.0 R2_* names, a BACKUP_S3_* variable set twice in .env.deploy, and,
# when the `backup` profile is on, the new image's `judgebot backup check`.
# Once the deploy has succeeded, the `backup` service is brought up to date
# (`up -d --no-deps backup`: the new image and the current .env.deploy). It
# holds no state but the bucket, and a backup cut short uploads nothing. The
# tunnel's connector (cloudflared, under the `tunnel` profile) is swapped the
# way judgebot is: a new one beside the old, which stops once the new one has
# registered with Cloudflare, so no request is dropped.
#
# --db is the database's update (docs/DEPLOYMENT.md §8, Database image): a new
# Postgres minor or pgvector release, pinned in docker-compose.yml. It pulls
# that image, refuses one of another Postgres major than the data directory
# (that is a dump and restore), takes a backup when one is configured
# (DEPLOY_SKIP_BACKUP=1 skips it), recreates the db container and waits for it
# to be healthy. There is one database, so this is a short outage: answers in
# flight fail, and judgebot reconnects on its own. Then the running judgebot's
# `judgebot ingest migrate` brings the extensions up to the image's version
# (with JUDGE_AUTO_MIGRATE=false it prints that command instead). Nothing else
# is touched; a db already as described only has its extensions checked.
# Every other deploy refuses while the db container differs from the db
# service docker-compose.yml and .env describe (its image, its port, any
# setting), since an `up` would recreate the database without these checks.
# Only --db ever recreates it.
#
# --proxy is the same for proxy and edge, whose host ports only one container
# can hold: replacing one (a new Caddy release, an edited service) refuses new
# connections until the old one has finished its requests (up to 70 s) and the
# new one listens. Every other deploy refuses while either differs from what
# docker-compose.yml and .env describe. A Caddyfile edit needs neither: Caddy
# loads it in place.
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
  # `old` is set once the running containers have been counted.
  if [ "${old+set}" = set ] && [ -z "$old" ]; then kept="no $service container was running before it"; fi
  if [ "$stage" = refused ]; then
    kept="it refused before changing anything: no container was started or stopped"
  elif [ "$stage" = before ] && [ -n "$new" ]; then
    docker rm -f "$new" >/dev/null 2>&1 || true
  elif [ "$stage" = stopping ]; then
    kept="the old container was being stopped and the new one is serving; check docker compose ps"
  elif [ "$stage" = major ]; then
    kept="this was a major upgrade and the old container has stopped, so nothing is serving questions; the database may be partly migrated. Check docker compose logs $service; to go back, restore the backup taken before it and deploy the previous release (docs/DEPLOYMENT.md, Rolling back)"
  elif [ "$stage" = db ]; then
    kept="the db container was being replaced, so nothing can answer until it is healthy; check docker compose ps db and docker compose logs db (the backup taken first, if one is configured, is the way back)"
  elif [ "$stage" = extensions ]; then
    kept="the new db container is healthy and serving, but updating its extensions failed; run docker compose exec judgebot judgebot ingest migrate"
  elif [ "$stage" = first ]; then
    kept="the services started, but $service did not become healthy (its log is above); fix what it names, then run this again"
  elif [ "$stage" = proxy-swap ]; then
    kept="proxy or edge was being replaced, so new connections may be refused until it listens; check docker compose ps proxy edge and docker compose logs proxy edge"
  elif [ "$stage" = tunnel ]; then
    kept="the new container is serving, but the tunnel's connector could not be swapped; the old connector keeps serving unless docker compose ps cloudflared says otherwise"
  elif [ "$stage" = services ]; then
    kept="the new container is serving, but another service could not be brought up to date; check docker compose ps"
  elif [ "$stage" = backup ]; then
    kept="the new container is serving, but the backup service could not be brought up to date; check docker compose ps backup and docker compose logs backup"
  elif [ "$stage" = proxy ]; then
    kept="the new container is serving, but the proxy in front of it did not load proxy/Caddyfile; check docker compose logs proxy"
    [ -z "${edge:-}" ] || kept="the new container is serving, but the proxy or the public site (edge) in front of it did not start or load its Caddyfile; check docker compose logs proxy edge"
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
log "checking that proxy/Caddyfile loads"
docker compose run --rm --no-deps -T proxy caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null 2>&1 ||
  die "proxy/Caddyfile does not load: docker compose run --rm --no-deps proxy caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile"
# The public profile's site (`edge`, COMPOSE_PROFILES=public) mounts the same
# directory and is checked the same way, with the JUDGE_DOMAIN it will serve.
# Without the profile it is none of this deploy's business.
edge=""
if docker compose config --services 2>/dev/null | grep -qx edge; then
  edge=edge
  [ -f proxy/edge.Caddyfile ] || die "proxy/edge.Caddyfile is missing: run this from a full checkout (git pull)"
  log "checking that proxy/edge.Caddyfile loads"
  docker compose run --rm --no-deps -T edge caddy validate --config /etc/caddy/edge.Caddyfile --adapter caddyfile >/dev/null 2>&1 ||
    die "proxy/edge.Caddyfile does not load, or JUDGE_DOMAIN in .env is not a host name: docker compose run --rm --no-deps edge caddy validate --config /etc/caddy/edge.Caddyfile --adapter caddyfile"
fi

# The backup's settings in .env.deploy, for every kind of deploy: a refused
# backup service stops taking backups, which is noticed only when one is
# needed. R2_* are the names before 2.0.0, which the backup now refuses.
if [ -f .env.deploy ]; then
  retired="$(grep -Eo '^[[:space:]]*(export[[:space:]]+)?R2_(ENDPOINT|BUCKET|ACCESS_KEY_ID|SECRET_ACCESS_KEY)=[[:space:]]*([^[:space:]"#'"'"']|"[^"]|'"'"'[^'"'"'])' .env.deploy |
    sed -E 's/^[[:space:]]*(export[[:space:]]+)?(R2_[A-Z_]+)=.*/\2/' | sort -u | tr '\n' ' ' || true)"
  if [ -n "$retired" ]; then
    stage=refused
    die ".env.deploy names the backup's store with ${retired% }, which 2.0.0 no longer reads. Rename each line, keeping its value (sed -i 's/^R2_/BACKUP_S3_/' .env.deploy does all four): R2_ENDPOINT= to BACKUP_S3_ENDPOINT=, R2_BUCKET= to BACKUP_S3_BUCKET=, R2_ACCESS_KEY_ID= to BACKUP_S3_ACCESS_KEY_ID=, R2_SECRET_ACCESS_KEY= to BACKUP_S3_SECRET_ACCESS_KEY= (docs/DEPLOYMENT.md §6)"
  fi
  # A later line wins, a blank one too: a pasted example line would unset
  # what the line above it set.
  twice="$(grep -Eo '^[[:space:]]*(export[[:space:]]+)?BACKUP_S3_[A-Z_]+=' .env.deploy |
    sed -E 's/^[[:space:]]*(export[[:space:]]+)?//; s/=$//' | sort | uniq -d | tr '\n' ' ' || true)"
  if [ -n "$twice" ]; then
    stage=refused
    die ".env.deploy sets ${twice% } more than once, and the last line wins (a blank one unsets it): keep one line each"
  fi
fi
backup=""
docker compose config --services 2>/dev/null | grep -qx backup && backup=backup

# Whether a service's container differs from the service as docker-compose.yml
# and .env describe it now (compose's own config hash, which `up` compares),
# or runs another image than the one the service names: what an `up` of it
# would recreate. False with no container of it at all.
service_differs() {
  local c want have image
  # A running container if there is one: a stopped leftover is not what `up`
  # would compare against.
  c="$(docker compose ps -q "$1" | head -n 1)"
  [ -n "$c" ] || c="$(docker compose ps -a -q "$1" | head -n 1)"
  [ -n "$c" ] || return 1
  want="$(docker compose config --hash "$1" | awk '{print $2}')"
  have="$(docker inspect -f '{{index .Config.Labels "com.docker.compose.config-hash"}}' "$c")"
  [ "$want" = "$have" ] || return 0
  image="$(docker image inspect -f '{{.Id}}' "$(docker compose config --images "$1")" 2>/dev/null || true)"
  [ -n "$image" ] && [ "$image" != "$(docker inspect -f '{{.Image}}' "$c")" ]
}
db_differs() { service_differs db; }

# Ask a Caddy service to load its file as it is now. One started a moment ago
# may not have opened its admin API yet, so a few tries; Caddy's own answer is
# shown when none succeeds.
reload() {
  local reload_log try
  reload_log="$(mktemp)"
  for try in 1 2 3 4 5; do
    if docker compose exec -T "$1" caddy reload --adapter caddyfile --config "/etc/caddy/$2" >"$reload_log" 2>&1; then
      break
    fi
    [ "$try" -lt 5 ] || {
      tail -n 20 "$reload_log" >&2
      rm -f "$reload_log"
      die "the $1 service did not load proxy/$2 (Caddy's answer is above): docker compose exec $1 caddy reload --adapter caddyfile --config /etc/caddy/$2"
    }
    sleep 2
  done
  rm -f "$reload_log"
}

# --proxy: replace proxy (and edge, under the public profile) when they
# differ from the compose file, described at the top. Exits when done.
deploy_proxy() {
  local svc changed=()
  local targets=(proxy)
  [ -z "$edge" ] || targets+=(edge)
  log "pulling the ${targets[*]} image"
  docker compose pull "${targets[@]}"
  for svc in "${targets[@]}"; do
    if [ -z "$(docker compose ps -q "$svc")" ] || service_differs "$svc"; then changed+=("$svc"); fi
  done
  if [ "${#changed[@]}" -eq 0 ]; then
    log "${targets[*]} already as docker-compose.yml describes"
    exit 0
  fi
  stage=proxy-swap
  log "replacing ${changed[*]}: new connections are refused until the old container has finished its requests (up to 70 s) and the new one listens"
  docker compose up -d --no-deps "${changed[@]}"
  stage=proxy
  for svc in "${changed[@]}"; do
    ! service_differs "$svc" ||
      die "the new $svc container still differs from docker-compose.yml by this Docker Compose's own reckoning (its config hash does not match the label its \`up\` wrote), so every deploy would refuse; report the Compose version (docker compose version)"
  done
  reload proxy Caddyfile
  [ -z "$edge" ] || reload edge edge.Caddyfile
  log "replaced: $(docker compose ps --format '{{.Name}} {{.Image}}' "${changed[@]}" | tr '\n' ';' | sed 's/;$//; s/;/; /g')"
  exit 0
}

# The tunnel's connector onto the compose file and .env.deploy as they are,
# without a gap: Cloudflare spreads a tunnel's requests over every connector
# that has registered, so the new one starts beside the old, and the old one
# stops (finishing its requests within its 70 s --grace-period) once the new
# one has registered. A new one that does not register is removed and the old
# one keeps serving. "Registered tunnel connection" is the line cloudflared
# logs per connection (connection/observer.go); a release that rewords it fails
# the swap that way, never the tunnel.
swap_tunnel() {
  local olds old tunnel_new started deadline
  mapfile -t olds < <(docker compose ps -q cloudflared)
  if [ "${#olds[@]}" -eq 0 ]; then
    log "starting the tunnel's connector"
    docker compose up -d --no-deps cloudflared
    return
  fi
  [ "${#olds[@]}" -eq 1 ] || die "${#olds[@]} cloudflared containers are running, so an earlier deploy did not finish; stop all but one (docker ps), then deploy again"
  service_differs cloudflared || return 0
  old="${olds[0]}"
  # A stopped connector left by an interrupted deploy would be started as the
  # "new" one by --no-recreate.
  docker compose rm -f cloudflared >/dev/null
  log "starting a new tunnel connector beside the running one"
  docker compose up -d --no-deps --no-recreate --scale cloudflared=2 cloudflared
  tunnel_new="$(docker compose ps -q cloudflared | grep -vx "$old" || true)"
  [ -n "$tunnel_new" ] || die "the new tunnel connector did not start; the old one keeps serving"
  started=$SECONDS
  deadline=$((SECONDS + wait_secs))
  last_progress=0
  until docker logs "$tunnel_new" 2>&1 | grep -q 'Registered tunnel connection'; do
    progress "$started" "the new tunnel connector has not registered with Cloudflare"
    if [ "$(docker inspect -f '{{.State.Running}} {{.RestartCount}}' "$tunnel_new" 2>/dev/null)" != "true 0" ] || [ "$SECONDS" -ge "$deadline" ]; then
      docker logs --tail 20 "$tunnel_new" >&2 2>&1 || true
      docker rm -f "$tunnel_new" >/dev/null 2>&1 || true
      die "the new tunnel connector did not register with Cloudflare within ${wait_secs}s (its log is above); it was removed and the old one keeps serving"
    fi
    sleep 2
  done
  log "the new tunnel connector has registered; stopping the old one, which finishes its requests first"
  docker stop --time 80 "$old" >/dev/null
  docker rm "$old" >/dev/null
}

# Start the db service as the compose file describes it and wait until it is
# healthy. Only for a db that is not running and that the compose file does
# not describe differently (callers check), so it replaces nothing.
start_db() {
  local c started deadline
  docker compose up -d --no-deps db
  c="$(docker compose ps -q db)"
  [ -n "$c" ] || die "the db container did not start"
  started=$SECONDS
  deadline=$((SECONDS + wait_secs))
  last_progress=0
  until [ "$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "$c" 2>/dev/null)" = healthy ]; do
    progress "$started" "the db container is not healthy yet"
    [ "$SECONDS" -lt "$deadline" ] || die "the db container was not healthy within ${wait_secs}s: docker compose logs db"
    sleep 2
  done
  log "the db container is healthy"
}

# --db: the database's update, described at the top. Exits when done.
deploy_db() {
  local before image pulled running_id data_major image_major judge after deadline started out
  mapfile -t before < <(docker compose ps -q db)
  if [ "${#before[@]}" -eq 0 ]; then
    # A first start (no container), or a stopped db: nothing is served without
    # it, so there is no outage to choose. A stopped one the compose file now
    # describes differently is left alone: its data directory is not checked
    # until it runs.
    if service_differs db; then
      stage=refused
      die "the db container is stopped, and docker-compose.yml now describes it differently: start it as it was (docker compose start db), then run this again"
    fi
    log "the db service is not running: starting it"
    docker compose pull db
    start_db
    exit 0
  fi
  if [ "${#before[@]}" -ne 1 ]; then
    stage=refused
    die "${#before[@]} db containers are running: stop all but one (docker ps), then run this again"
  fi
  image="$(docker compose config --images db)"
  [ -n "$image" ] || die "docker-compose.yml names no image for db"
  log "pulling the db image ($image)"
  docker compose pull db
  pulled="$(docker image inspect -f '{{.Id}}' "$image")"
  running_id="$(docker inspect -f '{{.Image}}' "${before[0]}")"
  # A new major cannot open the old one's data directory: it would refuse to
  # start, with the old container already gone.
  # shellcheck disable=SC2016 # $PGDATA is the container's
  data_major="$(docker exec "${before[0]}" sh -c 'cat "$PGDATA/PG_VERSION"' 2>/dev/null || true)"
  image_major="$(docker image inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$image" | sed -n 's/^PG_MAJOR=//p')"
  if [ -z "$data_major" ] || [ -z "$image_major" ]; then
    stage=refused
    die "could not read the data directory's Postgres major (${data_major:-unknown}) or the image's (${image_major:-unknown}); nothing was changed"
  fi
  if [ "$data_major" != "$image_major" ]; then
    stage=refused
    die "the pinned db image is Postgres $image_major and the data directory is Postgres $data_major: a major upgrade is a dump and restore (docs/DEPLOYMENT.md §8, Database image); nothing was changed"
  fi

  # pgvector has no update path down: an extension already updated past the
  # image's release would not load. Read from the tag; a tag without a
  # release (a floating pgNN) is not checked.
  local tag_vector installed_vector
  tag_vector="$(sed -nE 's#^pgvector/pgvector:([0-9]+(\.[0-9]+)*)-.*#\1#p' <<<"$image")"
  installed_vector="$(docker exec "${before[0]}" psql -U judgebot -d judgebot -tAc \
    "SELECT extversion FROM pg_extension WHERE extname = 'vector'" 2>/dev/null || true)"
  if [ -n "$tag_vector" ] && [ -n "$installed_vector" ] && [ "$tag_vector" != "$installed_vector" ] &&
    [ "$(printf '%s\n%s\n' "$tag_vector" "$installed_vector" | sort -V | head -n 1)" = "$tag_vector" ]; then
    stage=refused
    die "the pinned db image carries pgvector $tag_vector, older than the database's vector extension ($installed_vector), which has no way back down: pin $installed_vector or newer, or restore the backup taken before the update (docs/DEPLOYMENT.md §8, Database image); nothing was changed"
  fi
  if [ -z "$tag_vector" ] || [ -z "$installed_vector" ]; then
    log "not checking the image's pgvector against the database's (image release ${tag_vector:-not in the tag}, extension ${installed_vector:-not read})"
  fi

  mapfile -t judge < <(docker compose ps -q "$service")
  if [ "${#judge[@]}" -gt 1 ]; then
    stage=refused
    die "${#judge[@]} $service containers are running, so an earlier deploy did not finish; stop all but one (docker ps), then run this again"
  fi

  if [ "$pulled" = "$running_id" ] && ! db_differs; then
    log "the db container is already as docker-compose.yml describes it"
  else
    # A refresh cut off by the outage fails, alerts and backs off for an
    # hour; waiting for it costs nothing.
    local refreshing
    # The lease's lock (judge_bot::lease::REFRESH_LOCK, as pg_locks splits
    # it), named by its session's label when it has one.
    refreshing="$(docker exec "${before[0]}" psql -U judgebot -d judgebot -tAc \
      "SELECT string_agg(coalesce(nullif(a.application_name, ''), 'pid ' || l.pid), ', ') FROM pg_locks l LEFT JOIN pg_stat_activity a ON a.pid = l.pid WHERE l.locktype = 'advisory' AND l.granted AND l.objsubid = 1 AND l.classid = 1786078311 AND l.objid = 1701995878" 2>/dev/null || true)"
    if [ -n "$refreshing" ]; then
      stage=refused
      die "a data refresh is running ($refreshing); run this again once it has finished (docker compose logs $service); nothing was changed"
    fi
    if [ "${DEPLOY_SKIP_BACKUP:-}" = 1 ]; then
      log "DEPLOY_SKIP_BACKUP=1: no backup taken first"
    elif [ -f .env.deploy ] && grep -Eq '^[[:space:]]*(export[[:space:]]+)?BACKUP_S3_BUCKET=.+' .env.deploy; then
      # The image JUDGE_IMAGE_TAG names, which carries `judgebot backup`: the
      # local one may be an older release's (a 1.x image has no such command).
      log "taking a backup first"
      docker compose pull backup
      docker compose run --rm --no-deps backup run ||
        die "the backup before replacing the db container failed; fix it, or run this with DEPLOY_SKIP_BACKUP=1 to go on without one"
    else
      log "no backup configured (.env.deploy BACKUP_S3_BUCKET): going on without one (docs/DEPLOYMENT.md §6)"
    fi
    stage=db
    log "replacing the db container (answers in flight fail; judgebot reconnects)"
    docker compose up -d --no-deps db
    after="$(docker compose ps -q db)"
    [ -n "$after" ] || die "the new db container did not start"
    started=$SECONDS
    deadline=$((SECONDS + wait_secs))
    last_progress=0
    until [ "$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "$after" 2>/dev/null)" = healthy ]; do
      progress "$started" "the new db container is not healthy yet"
      if [ "$SECONDS" -ge "$deadline" ]; then
        docker logs --tail 40 "$after" >&2 2>&1 || true
        die "the new db container was not healthy within ${wait_secs}s (its log is above)"
      fi
      sleep 2
    done
    log "the new db container is healthy"
    if db_differs; then
      die "the new db container still differs from docker-compose.yml by this Docker Compose's own reckoning (its config hash does not match the label its \`up\` wrote), so every deploy would refuse; report the Compose version (docker compose version)"
    fi
  fi

  stage=extensions
  if [ "${#judge[@]}" -eq 0 ]; then
    # Its next start updates them (the startup migration does).
    log "no $service container is running: its next start updates the extensions"
  else
    # A process that lost the Discord gateway too slowly may have exited and
    # be restarting; its startup migration updates them too, but wait for it.
    started=$SECONDS
    deadline=$((SECONDS + wait_secs))
    last_progress=0
    until [ "$(docker inspect -f '{{.State.Running}} {{.State.Restarting}}' "${judge[0]}" 2>/dev/null)" = "true false" ]; do
      progress "$started" "the $service container is not running"
      [ "$SECONDS" -lt "$deadline" ] || die "the $service container was not running within ${wait_secs}s of the database's replacement: docker compose ps $service"
      sleep 2
    done
    out="$(docker exec "${judge[0]}" judgebot ingest upgrade </dev/null 2>/dev/null || true)"
    if ! grep -q '^upgrade: ' <<<"$out"; then
      die "the $service container did not say whether it migrates (judgebot ingest upgrade), so its extensions were not updated: docker compose exec $service judgebot ingest migrate, unless JUDGE_AUTO_MIGRATE=false"
    elif grep -qx 'auto-migrate: off' <<<"$out"; then
      log "JUDGE_AUTO_MIGRATE=false: update the extensions with your migrations, docker compose exec $service judgebot ingest migrate"
    else
      log "updating the extensions to the image's version"
      docker exec "${judge[0]}" judgebot ingest migrate </dev/null
    fi
  fi
  stage=after
  log "the database: $(docker compose exec -T db psql -U judgebot -d judgebot -tAc \
    "SELECT current_setting('server_version') || ', ' || string_agg(extname || ' ' || extversion, ', ' ORDER BY extname) FROM pg_extension WHERE extname IN ('vector', 'pg_trgm')" 2>/dev/null || echo unknown)"
  exit 0
}

# A line every 30 s while a wait goes on, so a long one is not mistaken for
# a hang: `progress <started> <what>`.
progress() {
  local waited=$((SECONDS - $1)) bucket
  bucket=$((waited / 30))
  if [ "$bucket" -gt 0 ] && [ "$bucket" != "${last_progress:-0}" ]; then
    last_progress=$bucket
    log "still waiting (${waited}s): $2"
  fi
}

case "${1:-}" in
  --db) deploy_db ;;
  --proxy) deploy_proxy ;;
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
    sed -n '2,/^set -euo pipefail$/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *) die "unknown argument '$1' (scripts/deploy.sh [--build | --db | --proxy])" ;;
esac

# A changed db service (a merged Dependabot pull request, DB_PORT in .env) is
# applied by --db alone, with its checks: any `up` here would recreate the db
# with none of them.
if db_differs; then
  stage=refused
  die "the db service in docker-compose.yml or .env (its image, port or settings) differs from the running db container (a merged update, an edited setting, or a Docker Compose upgrade that changed how it compares): run scripts/deploy.sh --db first (a few seconds' outage), then this again"
fi
# A first start (no db container yet) or a stopped db: started as the
# compose file describes it, which the check above has just compared, and
# waited for. Nothing is serving without it, so there is no outage to choose.
if [ -z "$(docker compose ps -q db)" ]; then
  log "the db service is not running: starting it"
  start_db
fi

mapfile -t running < <(docker compose ps -q "$service")
case "${#running[@]}" in
  0) old="" ;;
  1) old="${running[0]}" ;;
  *) die "${#running[@]} $service containers are running, so an earlier deploy did not finish; stop all but one (docker ps), then deploy again" ;;
esac

# A changed proxy or edge (a merged Caddy update) is applied by --proxy alone,
# at a moment of the operator's choosing: replacing one drops new connections
# for a while. With no judgebot running nothing is being served, so the
# services' start below may as well replace them.
if [ -n "$old" ]; then
  for svc in proxy $edge; do
    if service_differs "$svc"; then
      stage=refused
      die "the $svc service in docker-compose.yml or .env differs from its running container (a new Caddy release, an edited setting such as JUDGE_DOMAIN, or a Docker Compose upgrade): run scripts/deploy.sh --proxy first (new connections wait while it is replaced), then this again"
    fi
  done
fi

# What the new image's migrations are; it changes nothing. A one-off
# container that reads the migration ledger: seconds, unless the database
# does not answer. Its stdin is closed, and a container that outlives the
# timeout is removed by name.
probe_secs=120
probe_name="judgebot-deploy-probe-$$"
probe_log="$(mktemp)"
log "asking the new image what its migrations are (at most ${probe_secs}s)"
probe_out="$(timeout -k 10 "$probe_secs" docker compose run --rm --no-deps -T --name "$probe_name" "$service" ingest upgrade </dev/null 2>"$probe_log" || true)"
docker rm -f "$probe_name" >/dev/null 2>&1 || true
upgrade="$(printf '%s\n' "$probe_out" | sed -n 's/^upgrade: //p' | tail -n 1)"
# JUDGE_AUTO_MIGRATE=false, as the new container would see it (.env).
auto_migrate=on
if grep -qx 'auto-migrate: off' <<<"$probe_out"; then auto_migrate=off; fi
case "$upgrade" in
  in-place | major | major-from-1.x)
    rm -f "$probe_log"
    log "the new image's migrations: $upgrade"
    ;;
  *)
    tail -n 20 "$probe_log" >&2
    rm -f "$probe_log"
    die "could not read what the new image's migrations are within ${probe_secs}s (its log is above): docker compose run --rm --no-deps $service ingest upgrade"
    ;;
esac

# With JUDGE_AUTO_MIGRATE=false the new container never migrates, so a
# major upgrade would wait here for a migration that does not come, with the
# old release already stopped. Refused before anything changes.
if [ "$upgrade" != in-place ] && [ "$auto_migrate" = off ]; then
  if [ "$upgrade" = major-from-1.x ]; then stop_hint="docker stop judgebot-bot judgebot-api"; else stop_hint="docker compose stop $service"; fi
  stage=refused
  die "the new image's migrations are a major upgrade ($upgrade) and .env sets JUDGE_AUTO_MIGRATE=false, so the new container would never migrate. Migrate by hand, then run this again: $stop_hint; docker compose run --rm refresh migrate; scripts/deploy.sh${1:+ $1} (docs/DEPLOYMENT.md, Releases with a migration)"
fi

# The backup service runs the new image too: its settings are checked by it
# now, while the deploy is watched, rather than when it is next recreated.
if [ -n "$backup" ]; then
  log "checking the backup service's settings with the new image"
  backup_log="$(mktemp)"
  if ! docker compose run --rm --no-deps -T backup check </dev/null >"$backup_log" 2>&1; then
    tail -n 20 "$backup_log" >&2
    rm -f "$backup_log"
    stage=refused
    die "the backup service's settings are not usable with the new image (its log is above); fix .env.deploy, or take backup out of COMPOSE_PROFILES: docker compose run --rm --no-deps backup check"
  fi
  rm -f "$backup_log"
fi

# A major upgrade cannot be rolled back without the database as it was.
if [ "$upgrade" != in-place ]; then
  if [ "${DEPLOY_SKIP_BACKUP:-}" = 1 ]; then
    log "a major upgrade, and DEPLOY_SKIP_BACKUP=1: no backup taken first"
  elif [ -f .env.deploy ] && grep -Eq '^[[:space:]]*(export[[:space:]]+)?BACKUP_S3_BUCKET=.+' .env.deploy; then
    log "a major upgrade: taking a backup first"
    docker compose run --rm --no-deps backup run ||
      die "the backup before a major upgrade failed; fix it, or deploy with DEPLOY_SKIP_BACKUP=1 to go on without one"
  else
    log "a major upgrade and no backup configured (.env.deploy BACKUP_S3_BUCKET): it cannot be rolled back without one (docs/DEPLOYMENT.md §6)"
  fi
fi

# The old container may come from a compose file older than its
# stop_grace_period, so the current file's is passed (read from the new
# container once there is one; the service's own value until then).
grace_period="$(docker compose config --format json "$service" 2>/dev/null | sed -n 's/.*"stop_grace_period": *"\([^"]*\)".*/\1/p' | head -n 1)"
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
  local started=$SECONDS deadline=$((SECONDS + wait_secs))
  last_progress=0
  while :; do
    inspect
    progress "$started" "the new container is $status, health $health"
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
  local started=$SECONDS deadline=$((SECONDS + wait_secs))
  last_progress=0
  until docker logs "$new" 2>&1 | grep -q -e 'schema migrated; answering questions' -e 'migrated the schema; joining it'; do
    inspect
    progress "$started" "the new container has not finished migrating (docker compose logs $service)"
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
  # Nothing was serving, so nothing is kept. A judgebot that cannot start (a
  # missing variable) is reported here rather than left crash-looping; under
  # the tunnel profile `up` itself fails then, since cloudflared waits for it.
  stage=first
  docker compose up -d
  if [ "$upgrade" = in-place ]; then
    new="$(docker compose ps -q "$service" | head -n 1)"
    [ -n "$new" ] || die "the $service container did not start"
    log "waiting up to ${wait_secs}s for the $service container to be healthy"
    wait_healthy
  fi
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
    started=$SECONDS
    last_progress=0
    deadline=$((SECONDS + wait_secs))
    until maintenance_up; do
      inspect
      progress "$started" "the new container does not answer yet"
      [ "$health" != healthy ] || break
      [ "$SECONDS" -lt "$deadline" ] || abandon "the new container did not answer within ${wait_secs}s"
      sleep 1
    done
  fi
fi

# The proxy is started if it is not running (a proxy that differs was refused
# above: --proxy replaces it). It cannot start while an old judgebot that
# publishes 8787 itself holds the port; the call after the stop starts it.
log "making sure the proxy runs"
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
# published the port itself, and `edge` likewise under the public profile (as
# on the first deploy after turning it on). Each is then asked to load its
# file as it is now: nothing happens when it already runs it (it watches the
# file), and one that is not running, or kept an older configuration because
# the file stopped loading, fails the deploy, since nothing reaches judgebot
# through it.
stage=proxy
docker compose up -d --no-deps proxy
[ -z "$edge" ] || docker compose up -d --no-deps edge
reload proxy Caddyfile
[ -z "$edge" ] || reload edge edge.Caddyfile
# The backup service onto the new image and the current .env.deploy, checked
# above; nothing happens when it already runs both.
if [ -n "$backup" ]; then
  stage=backup
  docker compose up -d --no-deps backup
fi
if docker compose config --services | grep -qx cloudflared; then
  stage=tunnel
  swap_tunnel
fi
# Any other service of the active profiles onto the current compose file and
# .env; nothing happens to one that is already current. db is --db's alone,
# proxy and edge --proxy's.
mapfile -t others < <(docker compose config --services | grep -vxE 'db|judgebot|proxy|edge|backup|cloudflared' || true)
if [ "${#others[@]}" -gt 0 ]; then
  stage=services
  docker compose up -d --no-deps "${others[@]}"
fi
log "deployed: $(docker inspect -f '{{.Name}} {{.Config.Image}}' "$new")"
