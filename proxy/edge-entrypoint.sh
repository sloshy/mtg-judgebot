#!/bin/sh
# The `edge` service's entrypoint (docker-compose.yml): refuse a JUDGE_DOMAIN
# Caddy could get no certificate for, with a line that names it, then run the
# command as given (`caddy run ...`, or `docker compose run edge caddy ...`).
# The rule is judge-config's (`check::is_host_name`, which a test holds this
# script to): dot-separated labels of letters, digits and inner hyphens, at
# least two of them, each 1 to 63 characters, 253 in all, the last not all
# digits, and no scheme, port, path or trailing dot.
d="${JUDGE_DOMAIN:-}"
ok=1
case "$d" in
  "" | *[!A-Za-z0-9.-]* | .* | *. | *..* | -* | *- | *.-* | *-.*) ok=0 ;;
  *.*) ;;
  *) ok=0 ;;
esac
case "${d##*.}" in
  *[!0-9]*) ;;
  *) ok=0 ;;
esac
[ "${#d}" -le 253 ] || ok=0
old_ifs="$IFS"
IFS=.
for label in $d; do
  [ "${#label}" -le 63 ] || ok=0
done
IFS="$old_ifs"
if [ "$ok" -ne 1 ]; then
  echo "edge: set JUDGE_DOMAIN in .env to the public host name, such as judge.example.com: two or more dot-separated labels of letters, digits and inner hyphens, not an IP address, with no scheme, port, path or trailing dot (now: '$d')" >&2
  exit 78
fi
exec "$@"
