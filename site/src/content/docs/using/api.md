---
title: The HTTP API
description: POST /api/judge with a curl example, every reply shape, the status codes, and the two GET routes.
sidebar:
  order: 3
---

The HTTP interfaces serve three routes on `API_ADDR`:

- `POST /api/judge`, on with the `--api` role (the compose service runs it unless
  `JUDGE_ROLES` leaves it out).
- `GET /api/health` and `GET /api/about`, served whenever any of `--api`, `--web` and
  `--mcp` is on.
- `GET /api/feed` and `GET /api/calls/{id}`, the public feed, on with `--web` unless
  `JUDGE_FEED=false`.

With `--web`, any other path under `/api/` is a `404` with a JSON `error` body rather than
the web app's page.

There is no authentication. The API is the anonymous interface, bounded by a per-address
rate limit, the concurrency slots and the spend cap. It sends no CORS headers, so call it
from a server or from the bundled web app, not from a browser page on another origin.

## `POST /api/judge`

```sh
curl -s http://localhost:8787/api/judge \
  -H 'Content-Type: application/json' \
  -d '{"question": "Does deathtouch work with trample?"}'
```

| Field | Type | |
| --- | --- | --- |
| `question` | string, required | Not blank, at most 1000 characters. `[[Full Card Name]]` matches that exact card. |
| `session_id` | UUID, optional | Questions sharing one share history, so a follow-up works. Generate it on the client. Without one the question stands alone. |
| `pins` | array, optional | Resolved ambiguities, at most 8: `{"span": "<as the ambiguous reply gave it>", "name": "<the full name chosen>"}`, each string at most 200 characters. |
| `locale` | string, optional | The language for the reply's `message`s and citation `label`s, as a language tag such as `en-GB`. When it names no language the instance speaks, the `Accept-Language` header decides, then `JUDGE_LOCALE`. `kind` codes are never translated. The answer itself is English. |

An answer takes twenty to forty-five seconds. Every reply to a `POST` is JSON tagged by
`kind`, including a request that cannot be read, which gets an `error` with a 4xx status
(table below).

```json
{
  "kind": "answer",
  "answer": "Yes. With deathtouch, one damage is lethal damage, so …",
  "confidence": "high",
  "source": "cr",
  "cr_version": "20260819",
  "citations": [
    {
      "label": "702.19b",
      "url": "https://yawgatog.com/resources/magic-rules/#R70219b",
      "quote": "…"
    }
  ],
  "cards": [{ "name": "Dark Confidant", "url": "https://scryfall.com/search?q=oracleid%3A…" }]
}
```

| `kind` | Status | Fields | Meaning |
| --- | --- | --- | --- |
| `answer` | 200 | `answer`, `confidence` (`low`/`medium`/`high`), `source` (`cr`/`commander`), `cr_version` (`YYYYMMDD`), `citations`, `cards` | A validated verdict. Every citation's `quote` was checked verbatim against its source. `url` is `null` for a prior call, which has no public page. |
| `ambiguous` | 200 | `spans`: `[{"query", "choices", "truncated"}]` | A name matched several cards. Ask again with the same `question` and a `pins` entry per span. At most five `choices` each; `truncated` says there were more. |
| `not_found` | 200 | `names` | Card names that matched nothing. |
| `error` | 200 or 4xx | `message` | 200 for a question the judge declined or could not answer: out of scope, no verified answer, the spend cap. 400 for a body that is not JSON or a request over the limits above, 413 for a body over 2 MB, 415 for a missing `Content-Type` or one that is not JSON (`application/json` or `application/*+json`), 422 for JSON of the wrong shape (a missing `question`, a `session_id` that is not a UUID). |
| `busy` | 429 | `message` | Every judge slot is taken (`JUDGE_CONCURRENCY`). Retry shortly. |
| `rate_limited` | 429 | `message` | This address used up its window (`API_RATE_LIMIT` per `API_RATE_WINDOW_SECS`). |
| `maintenance` | 503 | `message`, `retry_after_secs` | The instance is migrating its database for a major upgrade, usually for a minute or two. Nothing was asked and nothing was counted against the window: send the same request again after `retry_after_secs` (also in the `Retry-After` header). |

The "did you mean?" round trip is stateless. The server keeps nothing between the
`ambiguous` reply and the question asked again with `pins`:

```sh
curl -s http://localhost:8787/api/judge -H 'Content-Type: application/json' -d '{
  "question": "Can Tibalt be my commander?",
  "pins": [{"span": "Tibalt", "name": "Tibalt, the Fiend-Blooded"}]
}'
```

## `GET /api/health`

`200 ok` when the database answers within three seconds, `503` with the reason when it
does not, or while a major upgrade migrates the database (`maintenance: …`). The compose
healthcheck and the tunnel's start condition use it.

## `GET /api/about`

The source offer and the operator contact as JSON: `program`, `repository`, `commit`,
`commit_url`, `dirty`, `license`, `license_name`, `license_url`, `copyright`,
`operator_email`, `operator_discord` and a ready-to-show `notice`. The web app's footer is
built from it. `locale` is the instance's language (`JUDGE_LOCALE`), which `notice` is
written in, and `locales` every language its replies can be in. `feed` says whether this
listener serves the feed.

`freshness` says what the instance's data is:

- `cr_version`: the Comprehensive Rules release loaded, as `YYYYMMDD`, or `null` with no
  rules loaded.
- `refreshed_secs_ago`: seconds since the latest successful data refresh finished, or
  `null` when none is recorded.
- `last_refresh_failed`: whether the latest refresh failed.

`freshness` itself is `null` when the database does not answer within two seconds. The
rest of the reply is served regardless, and the endpoint never fails over it. On a schema
without the run table (migrations pending) only `cr_version` is read: `refreshed_secs_ago`
is `null` and `last_refresh_failed` is `false`. The read is cached for a minute, so the
answer can trail a refresh, or an outage, by up to 60 seconds. The age is advanced to the
moment of the reply. `GET /api/health` does not look at freshness: stale data is not an
outage.

```json
"freshness": {"cr_version": "20260925", "refreshed_secs_ago": 10800, "last_refresh_failed": false}
```

## `GET /api/feed`

A page of answered calls not asked private, newest first, 20 at a time:

```sh
curl 'http://localhost:8787/api/feed?interface=discord'
```

```json
{"entries": [{"id": "…", "asked_at": "2026-10-18T12:00:00Z", "interface": "discord",
  "asker": "alice", "question": "…", "answer": "…", "confidence": "high", "source": "cr",
  "cr_version": "20260925", "citations": [{"label": "702.19b", "url": "…", "quote": "…"}],
  "cards": [{"name": "…", "url": "…"}],
  "ratings": {"correct": 2, "partial": 0, "incorrect": 0, "judge": null},
  "retired": null}],
 "next": "1760788800000000.0d3f…"}
```

The query parameters are all optional:

- `cursor`: the previous page's `next`. `next` is `null` on the last page.
- `interface`: `discord`, `web`, `mcp`, `cli`, `agent_session`, or `agent` (MCP and
  `judge-cli` before 2.0.0, which were not told apart).
- `user`: a Discord username, with or without `@`.
- `locale`: the language of the citation labels and messages, ahead of `Accept-Language`.

`asker` is `null` for an anonymous call. Asker ids are never sent. `ratings.judge` is the
latest rating by a member with the judge role (`correct`, `partial`, `incorrect`), or
`null`. `retired` is why the call no longer serves as an example, or `null`. A query that
does not parse is a `400` `error`.

## `GET /api/calls/{id}`

One entry, in the same shape. A private call, a call older than `JUDGE_FEED_DAYS` and an id
that names nothing are the same `404` `error`.

Both routes answer `429` `rate_limited` past 120 reads a minute per address (a window of
their own, apart from the questions') and `503` `maintenance` while a major upgrade
migrates.

## MCP

`/mcp` is a separate interface with its own credential. See [Agents](../../using/agents/).
