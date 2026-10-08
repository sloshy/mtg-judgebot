#!/usr/bin/env bash
# judge-config, the config editor, from the image: no Rust toolchain needed.
# Run it from the checkout. It prints a http://127.0.0.1:8790/#token=... URL to
# open; Ctrl-C stops it. Arguments go to judge-config (--config FILE,
# --allow-host NAME). From another machine: ssh -L 8790:127.0.0.1:8790 <host>.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
status=0
docker compose run --rm --service-ports --user "$(id -u):$(id -g)" config "$@" || status=$?
# On a failure other than Ctrl-C (130), say so if the image has no judge-config
# at all: an image from before it existed, such as JUDGE_IMAGE_TAG=1.1.
if [ "$status" -ne 0 ] && [ "$status" -ne 130 ] &&
  ! docker compose run --rm --no-deps --entrypoint sh config -c 'command -v judge-config' >/dev/null 2>&1; then
  echo "scripts/config.sh: this image has no judge-config (1.1.x and older). Use a newer one" \
    "(JUDGE_IMAGE_TAG blank or latest, then docker compose pull), or run it from source:" \
    "cargo run --release -p judge-configure" >&2
fi
exit "$status"
