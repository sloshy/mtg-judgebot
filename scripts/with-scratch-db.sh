#!/usr/bin/env bash
# Run a command with DATABASE_URL pointing at a throwaway Postgres container:
#
#   scripts/with-scratch-db.sh scripts/check.sh sqlx test
#
# For a host whose .env names a database that must not carry the test suite's
# load, such as the one a running instance uses. The container is the pgvector
# image docker-compose.yml pins, published on a free loopback port, and is
# removed when the command ends. It replaces any DATABASE_URL already set.
# pre-push runs itself through this script when
# `git config judgebot.scratchdb true` is set in the clone.
set -uo pipefail

if [ $# -eq 0 ]; then
  echo "usage: scripts/with-scratch-db.sh <command> [args...]" >&2
  exit 2
fi
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

image="$(sed -nE 's#^ *image: (pgvector/pgvector:[^ ]+)$#\1#p' "$root/docker-compose.yml")"
if [ -z "$image" ]; then
  echo "no pgvector image in docker-compose.yml" >&2
  exit 1
fi

name="judgebot-scratch-db-$$"
trap 'docker rm -f "$name" > /dev/null 2>&1' EXIT
trap 'exit 130' HUP INT TERM
# max_connections: #[sqlx::test] opens a pool per test, many at once.
if ! docker run -d --rm --name "$name" -p 127.0.0.1::5432 \
  -e POSTGRES_USER=judgebot -e POSTGRES_PASSWORD=judgebot -e POSTGRES_DB=judgebot \
  "$image" -c max_connections=300 > /dev/null; then
  echo "could not start the scratch database ($image)" >&2
  exit 1
fi
# Over TCP: the image's first-start server listens on its socket alone, so a
# socket probe passes before the real server is up.
waited=0
until docker exec "$name" pg_isready -q -h 127.0.0.1 -U judgebot 2> /dev/null; do
  if [ "$waited" -ge 60 ]; then
    echo "the scratch database did not start within 60 s" >&2
    docker logs "$name" 2>&1 | tail -n 20 >&2
    exit 1
  fi
  sleep 1
  waited=$((waited + 1))
done

port="$(docker port "$name" 5432/tcp | sed -nE 's/^127\.0\.0\.1:([0-9]+)$/\1/p' | head -n 1)"
if [ -z "$port" ]; then
  echo "the scratch database has no published port" >&2
  exit 1
fi

echo "scratch database on 127.0.0.1:$port ($name)" >&2
# JUDGEBOT_SCRATCH_DB tells pre-push it is already running under this script.
JUDGEBOT_SCRATCH_DB=1 DATABASE_URL="postgres://judgebot:judgebot@127.0.0.1:$port/judgebot" "$@"
