---
title: The HTTP API
description: POST /api/judge with a curl example, every reply shape and status code, the public feed and dashboard reads, and the admin API.
sidebar:
  order: 3
---

The HTTP interfaces serve three routes on `API_ADDR`:

- `POST /api/judge`, on with the `--api` role (the compose service runs it unless
  `JUDGE_ROLES` leaves it out).
- `GET /api/health` and `GET /api/about`, served whenever any of `--api`, `--web` and
  `--mcp` is on.
- `GET /api/feed` and `GET /api/calls/{id}`, the public feed, and `GET /api/stats`, the
  public dashboard, on with `--web` unless `JUDGE_FEED=false`.

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
      "kind": "rule",
      "label": "702.19b",
      "url": "https://yawgatog.com/resources/magic-rules/#R70219b",
      "quote": "…"
    }
  ],
  "cards": [{ "name": "Dark Confidant", "url": "https://scryfall.com/search?q=oracleid%3A…" }],
  "call": "0d3f…"
}
```

| `kind` | Status | Fields | Meaning |
| --- | --- | --- | --- |
| `answer` | 200 | `answer`, `confidence` (`low`/`medium`/`high`), `source` (`cr`/`commander`), `cr_version` (`YYYYMMDD`), `citations`, `cards`, `call` | A validated verdict. `call` is the stored call's id, to rate it or open `/calls/<id>`; `null` when the store failed to keep it. The other kinds store no call. Every citation's `quote` was checked verbatim against its source. A citation's `kind` is `rule`, `ruling`, `oracle_text` or `prior_call`; its `label` is for display and may be translated. `url` is `null` for a prior call, which has no public page. |
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
listener serves the feed. `stats` is `null` where the dashboard is not served, and
otherwise says what it shows beyond the counts: `{"cost": false, "models": false}`
(`JUDGE_PUBLIC_COST`, `JUDGE_PUBLIC_MODELS`).

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
  "cr_version": "20260925", "citations": [{"kind": "rule", "label": "702.19b", "url": "…", "quote": "…"}],
  "cards": [{"name": "…", "url": "…"}],
  "ratings": {"discord": {"correct": 2, "partial": 0, "incorrect": 0},
              "web": {"correct": 1, "partial": 0, "incorrect": 3}, "judge": null},
  "retired": null}],
 "next": "1760788800000000.0d3f…"}
```

The query parameters are all optional:

- `cursor`: the previous page's `next`. `next` is `null` on the last page.
- `interface`: `discord`, `web`, `mcp`, `cli`, `agent_session`, or `agent` (MCP and
  `judge-cli` before 2.0.0, which were not told apart).
- `user`: a Discord username, with or without `@`.
- `q`: text to search for, at most 100 characters: a call matches when its question,
  its answer or its asker's username contains it, ignoring case. A mention in the text
  is searched as the feed shows it (`@…`), so it never matches an id.
- `locale`: the language of the citation labels and messages, ahead of `Accept-Language`.

`asker` is `null` for an anonymous call. Asker ids are never sent. `ratings` counts
Discord's and the web's ratings apart: only Discord's shape later answers, and the web
app shows the web's only when its reader asks. `ratings.judge` is the latest rating by a
member with the judge role (`correct`, `partial`, `incorrect`), or `null`. With
`JUDGE_PUBLIC_FEEDBACK=true` an entry also has `reasons`, oldest first:
`[{"rater_kind": "discord" | "web", "score": "partial" | "incorrect", "category", "wrong",
"right"}]`, never who gave them. The rater's words are as written except that user and
role mentions become `@…`, channel mentions `#…`, and `@everyone` / `@here` carry a
zero-width space after the `@`. A reason whose queue item the operator dismissed is left
out. Without the setting there is no `reasons` field. Web counts are anonymous and
trivially inflated (any new session is a new rater), which is why the web app hides them
by default. `retired` is why the call no longer serves as an example, or `null`. A query that
does not parse is a `400` `error`.

## `GET /api/calls/{id}`

One entry, in the same shape. A private call, a call older than `JUDGE_FEED_DAYS` and an id
that names nothing are the same `404` `error`.

Both routes answer `429` `rate_limited` past 120 reads a minute per address (a window of
their own, apart from the questions') and `503` `maintenance` while a major upgrade
migrates.

## `GET /api/stats`

The public dashboard: counts of the calls and runs not asked private, within
`JUDGE_FEED_DAYS`, over a range of UTC days.

```sh
curl 'http://localhost:8787/api/stats?days=90&interface=discord'
```

```json
{"from": "2026-07-23", "to": "2026-10-20", "questions": 41, "askers": 12,
 "interfaces": [{"interface": "discord", "questions": 41}],
 "by_day": [{"day": "2026-07-23", "discord": 0, "web": 0, "mcp": 0, "cli": 0,
             "agent_session": 0, "agent": 0}, …],
 "outcomes": {"answered": 41, "ambiguous": 6, "not_found": 2, "out_of_scope": 1,
              "refused": 0, "failed": 1},
 "ratings": {"discord": {"correct": 9, "partial": 2, "incorrect": 1},
             "web": {"correct": 3, "partial": 0, "incorrect": 4}},
 "categories": [{"category": "combat", "label": "Combat: attacking, blocking, damage assignment", "questions": 9}, …],
 "cards": [{"id": "…", "name": "Blood Moon", "questions": 4}, …],
 "top_askers": [{"name": "alice", "questions": 7}, …]}
```

The query parameters are all optional:

- `from`, `to`: the first and last day, `YYYY-MM-DD` (UTC). `to` defaults to today.
- `days`: without `from`, the number of days ending on `to`, 1 to 366 (default 30). A
  range covers at most 366 days.
- `interface`: as the feed's.
- `user`: a Discord username, with or without `@`.
- `model`: only calls whose run billed this model. Only where models are shown; anywhere
  else it is a `400`, never ignored.
- `locale`: the language of the messages, ahead of `Accept-Language`.

`by_day` lists every day of the range, oldest first. `from` starts no earlier than the
window when `JUDGE_FEED_DAYS` is set, and `window_days` then gives it. `outcomes` counts
the runs that finished in the range: `failed` is every failure of the pipeline (a citation
that never validated, the spend cap, an upstream error), and a run whose caller went away
is in none. Runs record no asker, so with `user` it is `null`. `ratings` are the ratings
of these calls, Discord's and the web's apart. `categories`, `cards` and `top_askers` list
ten each, commonest first.

With `JUDGE_PUBLIC_COST=true` the reply also has `cost`: `{"usd", "questions",
"per_question_usd", "estimated"}`, what these calls' runs cost (`questions` counts the
ones with a recorded run; `estimated` when any charge is a worst case). With
`JUDGE_PUBLIC_MODELS=true` it has `models`: `[{"model", "answered", "extracted"}]`, the
calls each model answered and read. Without the setting the field is absent.

The route answers `429` `rate_limited` past 30 reads a minute per address (its own
window), `503` `busy` when two reports are already being read and a third has waited five
seconds or when a report takes longer than five seconds to count (ask for a shorter range
or a filter), and `503` `maintenance` while a major upgrade migrates. A query that does
not parse, a reversed range, one longer than 366 days or one that ends before the
`JUDGE_FEED_DAYS` window is a `400` `error`.

## `POST /api/calls/{id}/feedback`

Rate a call, anonymously, as a browser session. Served wherever `POST /api/judge` is
(the `--api` role).

```sh
curl -s http://localhost:8787/api/calls/0d3f…/feedback -H 'Content-Type: application/json' -d '{
  "session_id": "6f1c…", "score": "incorrect", "category": "wrong_card",
  "wrong": "It read Bob as Bob Jr.", "right": "…"
}'
```

| Field | Type | |
| --- | --- | --- |
| `session_id` | UUID, required | The rater. One rating per call and session; the web app sends the tab's session id. |
| `score` | `correct`, `partial` or `incorrect`, required | |
| `category` | string | Required for `partial` and `incorrect`, refused for `correct`: `wrong_conclusion`, `missed_part`, `bad_citation`, `wrong_card`, `misread_question`, `wrong_refusal`, `outdated`, `other`. |
| `wrong`, `right` | string, optional | What's wrong and the right answer, at most 1000 characters each, no control characters but line breaks and tabs. Blank is none. Refused for `correct`. |
| `name` | string, optional | A name the operator sees, at most 100 characters. Refused for `correct`. |
| `replace` | boolean, optional | `true` confirms replacing the session's earlier rating. |
| `locale` | string, optional | As `/api/judge`'s. |

Unknown fields are refused, and the body may be at most 16 KiB.

| `kind` | Status | Fields | Meaning |
| --- | --- | --- | --- |
| `rated` | 200 | `message`, `score`, `replaced` | Stored. `replaced` is the score it replaced, or `null`. |
| `already_rated` | 200 | `message` | `correct` over `correct`: nothing changed. |
| `confirm` | 409 | `message`, `earlier` (`{"score", "category"}`) | The session already rated the call otherwise. Nothing was written; send the same body with `"replace": true` to replace it. The server decides this, and without `replace` its write only ever adds a first rating, so even two requests racing from one session cannot replace each other unasked. |
| `error` | 400, 404, 413, 415 | `message` | A body that does not read or breaks a rule above (400), a call that is private, older than `JUDGE_FEED_DAYS` or missing (one 404), a body over the limit (413). |
| `rate_limited` | 429 | `message` | 30 ratings per address per ten minutes, a window of its own. |
| `error` | 500 | `message` | The rating could not be stored or checked just now; nothing changed. Try again. |
| `maintenance` | 503 | `message`, `retry_after_secs` | As `/api/judge`'s. |

The rating stores the address the limits bucket on (`API_CLIENT_IP`; never
`X-Forwarded-For`) and the `User-Agent`, both cleared after `FEEDBACK_IP_DAYS`. A
`partial` or `incorrect` rating opens an item in the operator's feedback queue. Web
ratings never count toward the examples later answers see. `JUDGE_FEED=false` does not
turn rating off: the id of an answer reaches only whoever asked it. `GET /api/about`
says `"ratings": true` where the route is served, and `"public_feedback": true` when
reasons are shown publicly, so a form can tell the rater. The `rated` message says so
too.

## Admin API

The operator's reads, and the feedback queue's one write, are served by the `--admin`
listener only (`ADMIN_ADDR`, loopback), never on this one, which answers `/admin` paths
`404`. Every route takes `Authorization: Bearer <ADMIN_TOKEN>` and answers `401` without
it; replies carry `Cache-Control: no-store`, and an error is the same
`{"kind": "error", "message"}`.

| Route | Returns |
| --- | --- |
| `GET /admin/api/calls?cursor&interface&asker&model&private&q` | `{calls, next, models}`: every stored call, private ones included, newest first, 50 a page. `asker` is a Discord username or user id, `private` is `true` or `false`, `models` lists every chat model a run has billed. |
| `GET /admin/api/calls/{id}` | One call whole, its citations labelled as an answer's are, with `ratings` (rater, kind, score, judge, and a fault's `category`, `wrong`, `right`) and `runs` (outcome, retry, tool round, `charges`). `404` for an id with no call. |
| `GET /admin/api/queue?cursor&status&category&interface&rater` | `{items, next}`: the feedback queue, every rating given a reason, newest first (by when each was opened or reopened), 50 a page. `status` is `open`, `resolved` or `dismissed`; `category` a feedback category id; `rater` `discord` or `web`. |
| `GET /admin/api/queue/{id}` | `{item, call}`: one item and its call whole, as above. `404` for an id with no item. |
| `POST /admin/api/queue/{id}/resolve` | Body `{"note": "…", "fixed_by": "…"}` (`note` 1 to 2000 characters, `fixed_by` optional, up to 200): the item closed as fixed, as it now stands. `400` for a bad body, `404` for no item. |
| `POST /admin/api/queue/{id}/dismiss` | The same body: the item closed with nothing to fix. |
| `GET /admin/api/failures?limit` | `{failures}`: the newest failed calls (default 50, at most 100). |
| `GET /admin/api/usage?days&from&to&interface&asker&model` | The report `judge-cli stats` prints: the dashboard's fields over every call, private ones included, with `by_day[].private`, `cost` and `models` always present; then `days`, `private`, `failed_calls`, `usd`, `spend_by_day`, `spend_by_month` (the ledger: every process, chat and embeddings, unfiltered), `budget` (`period`, `cap_usd`, `recorded_at`, `period_start`, `spent_usd`, `left_usd`: the period and cap the instance's ledger-keeping processes last recorded, all `null` when none has; the last three `null` for a `process` cap), `run_stats` (`runs`, `usd`, `outcomes` by id, `refused_at_cap`, `by_model`, `by_interface`, `by_stage`, `usd_per_answer`, `latency`, `most_expensive`; `null` with `asker`), `retired_calls`, `lowest_rated`, `feedback`, `refresh_runs`. The range and filters are the public route's, `asker` a Discord username or user id. A report that runs past a minute is a `503` `error`. |

They answer `503` `maintenance` while a major upgrade migrates. The pages that read them
are the [web app's admin pages](../../using/web/#admin-pages).

## MCP

`/mcp` is a separate interface with its own credential. See [Agents](../../using/agents/).
