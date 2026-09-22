---
title: The HTTP API
description: POST /api/judge with a curl example, every reply shape, the status codes, and the two GET routes.
sidebar:
  order: 3
---

`judge-api` serves three routes:

- `POST /api/judge`, on with `--api` (or with no flags at all).
- `GET /api/health` and `GET /api/about`, served whatever is switched off.

There is no authentication. The API is the anonymous front door, bounded by a per-address
rate limit, the concurrency slots and the spend cap. It sends no CORS headers, so call it
from a server or from the bundled page, not from a browser page on another origin.

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

An answer takes twenty to forty-five seconds. A request that parses gets JSON tagged by
`kind`. A request that does not parse gets a 4xx with a plain-text body, so check the
status before parsing. That covers malformed JSON, a missing `question`, a `session_id`
that is not a UUID, and a `Content-Type` other than `application/json`.

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
| `error` | 200 or 400 | `message` | 400 for a request over the limits above. 200 for a question the judge declined or could not answer: out of scope, no verified answer, the spend cap. |
| `busy` | 429 | `message` | Every judge slot is taken (`JUDGE_CONCURRENCY`). Retry shortly. |
| `rate_limited` | 429 | `message` | This address used up its window (`API_RATE_LIMIT` per `API_RATE_WINDOW_SECS`). |

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
does not. The compose healthcheck and the tunnel's start condition use it.

## `GET /api/about`

The source offer and the operator contact as JSON: `program`, `repository`, `commit`,
`commit_url`, `dirty`, `license`, `license_name`, `license_url`, `copyright`,
`operator_email`, `operator_discord` and a ready-to-show `notice`. The page footer is
built from it.

## MCP

`/mcp` is a separate door with its own credential. See [Agents](../../using/agents/).
