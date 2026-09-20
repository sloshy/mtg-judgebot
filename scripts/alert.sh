#!/usr/bin/env bash
# Tell the operator a scheduled job failed. Sourced by refresh-data.sh and
# backup-db.sh, which run from cron where nobody is watching the exit code.
#
#   alert "text"     post to JUDGE_ALERT_WEBHOOK, or do nothing if it is unset
#
# The webhook is the one the bot and api report a tripped spend cap to: a
# Discord or Slack-style incoming webhook. It is read from the environment,
# else .env.deploy, else .env, by name only: neither file is sourced here, since
# .env holds values (API_INTERFACES) that are not shell syntax.
#
# The URL is a credential (whoever holds it can post to the channel), so it goes
# to curl or (GNU) wget on stdin and never into argv, where `ps` would show it.
# Nothing here can fail the caller: an alert that cannot be sent is logged.

alert_webhook() {
  if [ -n "${JUDGE_ALERT_WEBHOOK:-}" ]; then
    printf '%s' "$JUDGE_ALERT_WEBHOOK"
    return
  fi
  local f line
  for f in .env.deploy .env; do
    [ -f "$f" ] || continue
    line="$(grep -E '^JUDGE_ALERT_WEBHOOK=' "$f" | tail -n 1)" || continue
    line="${line#JUDGE_ALERT_WEBHOOK=}"
    line="${line%\"}"; line="${line#\"}"
    line="${line%\'}"; line="${line#\'}"
    if [ -n "$line" ]; then
      printf '%s' "$line"
      return
    fi
  done
}

alert() {
  local url text body
  url="$(alert_webhook)" || true
  [ -n "$url" ] || return 0
  case "$url" in
    https://*) ;;
    *) echo "alert: JUDGE_ALERT_WEBHOOK is not an https:// URL; not sent" >&2; return 0 ;;
  esac
  # JSON string: escape backslash and double quote, flatten newlines.
  text="$(printf '%s' "$1" | tr '\n\r\t' '   ' | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
  body="{\"content\":\"$text\",\"text\":\"$text\"}"
  if command -v curl >/dev/null 2>&1; then
    printf 'url = "%s"\n' "$url" \
      | curl -fsS -m 15 -K - -H 'Content-Type: application/json' -d "$body" -o /dev/null \
      || echo "alert: webhook post failed" >&2
  elif command -v wget >/dev/null 2>&1; then
    printf '%s\n' "$url" \
      | wget -q -T 15 -O /dev/null --header='Content-Type: application/json' --post-data="$body" -i - \
      || echo "alert: webhook post failed" >&2
  else
    echo "alert: neither curl nor wget is installed; not sent" >&2
  fi
  return 0
}
