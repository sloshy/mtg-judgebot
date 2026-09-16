---
name: judge
description: "Answer a Magic: The Gathering (MTG) rules question with the judgebot pipeline: either call its built-in model (costs API budget) or do the model's work yourself through a session whose citations are validated against the Comprehensive Rules (CR), Oracle text and Scryfall rulings. Also for looking up cards, CR rules text, rulings and glossary terms. Use when asked a rules question, to check a ruling or an interaction, or to consult the judgebot database."
---

# Judge

The judgebot answers Magic rules questions from the Comprehensive Rules (CR), current
Oracle text, Scryfall rulings and rated prior calls, and **validates every citation**:
a rule id must appear in the material and the quote must be a verbatim substring of it.
This skill drives that pipeline from outside.

Two transports, same operations. Prefer MCP when the `judge` MCP server is connected
(tool names below); otherwise use the CLI, `target/release/judge-cli` (subcommands
below; written as `judge-cli` here). Replies are JSON; where the outcome varies they are
tagged by `kind` (the one exception: `session_prompt` is tagged by `step`). A failure you
cannot act on is `{"kind":"error","message":...}` (CLI exit 1): read the message, it says
what was wrong (unknown or expired session, wrong step, over a limit, concurrent step).

## Two ways to answer

**1. Built-in pipeline** — `judge {question, thread?, pins?}` / `judge-cli judge
"<question>" [--thread T] [--pin "span=Full Name"]...`. The server runs its own model calls
and returns `answer` (validated and cited, plus `thread` and `call`). It spends the
operator's model budget (about $0.12 a question on the default Anthropic setup), is `unavailable` when the server has
no API key, `rate_limited` over HTTP once the client's hourly quota is used, `busy` when
every slot is taken. It can also reply `ambiguous` (`spans: [{query, choices, truncated}]`;
ask again with `pins: [{span, name}]` / `--pin "span=Full Name"`), `not_found` (`names`),
`out_of_scope` or `error`. Use it when the user explicitly wants the bot's own answer, or
to compare against yours. Never loop on it.

**2. Session** — you are the model. No API call is made. Steps, in order:

1. `begin_session {question, thread?}` / `judge-cli begin "<question>" [--thread T]`
   → `{session, thread, extraction: {system, user, schema}}`. That is the **extraction
   prompt**.
2. Produce the extraction JSON yourself, following `extraction.system` exactly:
   `card_spans` copied character for character from the question (keep `[[brackets]]`),
   `concepts` in rules vocabulary, `primary` + up to two `secondary` categories from the
   taxonomy listed in the prompt, `source` (`cr` | `commander` | `tournament` |
   `out_of_scope`). At most 20 spans and 20 concepts, each at most 200 characters.
3. `submit_extraction {session, extraction}` / `judge-cli extract <session> <file|->`
   → `ready` with the **synthesis prompt** at top level (`system`, `material`, `question`,
   `schema`, `lookup_available`), or:
   - `ambiguous` (`spans`): a span matched several cards. The session is unchanged. Pick
     the card the user meant (ask them if it is genuinely unclear), replace that span in
     `card_spans` with `[[Full Card Name]]`, submit again.
   - `not_found` (`names`): fix the spelling or drop the span, submit again.
   - `out_of_scope`: tournament policy or not a rules question; the session is closed.
     Tell the user the bot does not cover it.
4. Read the material. If the rule you need is not in it, call `lookup_rules {session, ids}`
   / `judge-cli rules <session> <id>...` **once**, with every id you need (rule ids like
   `702.19` or `613.7b`, or a whole subsection like `613`; at most 10). Then re-read the
   prompt with `session_prompt {session}` / `judge-cli prompt <session>` (reply
   `{step: "synthesis", system, material, question, schema, lookup_available}`): the fetched
   rules are now in the material and citable. A second lookup is refused, and a rejected
   verdict forfeits the round, so look up before you answer, not after.
5. Write the verdict JSON following the synthesis `system` prompt: `answer` (ruling first,
   then reasoning with rule numbers; aim for under 1500 characters, hard limit 4000, and an
   oversized answer is a rejection that spends your one retry), `confidence`, `citations`,
   `category`.
6. `submit_verdict {session, verdict, persist?}` / `judge-cli verdict <session> <file|->
   [--persist]` →
   - `accepted`: the answer with citation labels and links, `call` if persisted
     (`persist_error` if persisting failed; the verdict still stands and
     `persist_session` can be retried).
   - `rejected`: `reason`, the typed `rejection`, and `retry: {system, material, question,
     schema, lookup_available}` — the prompt again with a "Previous attempt rejected"
     notice in `question` saying exactly which citation failed and why. Fix it and submit
     once more.
   - `exhausted`: the retry failed too; the session is closed. Start a new session if you
     want another go.
7. Give the user the accepted answer. Pass `persist: true` (or `persist_session
   {session}` / `judge-cli persist <session>`) when the user may ask a follow-up: a
   persisted call is the thread's history for the next `begin_session` with the same
   `thread`. It is never shown to other askers as an example.

`session_status {session}` / `judge-cli status <session>` says where a session is
(`stage`, `lookup_available`, `attempts`, `outcome`). Sessions expire after **one hour
idle**; an expired or unknown id is an `error`, so do not park a session while doing
something long.

For a **follow-up question** in the same conversation, pass the `thread` from the earlier
reply to `begin_session` / `--thread`; the earlier Q&A is rendered as history. Only
`agent:<uuid>` ids are accepted: an agent can never read or write a Discord or web thread.

If the material is long, hand the synthesis prompt to a subagent with a fresh context:
give it `system` as its instructions and `material` + `question` as the task, and ask for
the JSON only. Do it promptly: the session clock is running.

## Citations: how to not get rejected

- Copy quotes **verbatim from the material as shown**. Punctuation is forgiven —
  ASCII `'`, `"` and `-` match the CR's `’`, `“”` and `—`, and the accepted citation comes
  back carrying the source's own typography — but nothing else is: a changed word, a
  dropped word or a different capitalisation is still a rejection.
- Keep a quote inside one line of the source and short (the prompt asks for at most 200
  characters); use two citations rather than one spanning lines.
- Cite the finest rule containing the quote: `702.19b` for a line that starts with
  `702.19b`, `702.19` only for its own first line. Whole subsections (`613`) are never
  citable even after a lookup.
- Rulings: `{"kind":"scryfall_ruling","card":"<uuid>","ruling":"<16-char key>","quote":...}`,
  with the uuid copied from the card's heading in the rulings section (`### <Card Name> —
  card <uuid>`) and the key from the `[ruling <key>]` label under it. Oracle text:
  `{"kind":"oracle_text","card":"<uuid>","face":0,"quote":...}` from the `[oracle
  <uuid>#<face>]` label on the face in the Cards section. Prior calls:
  `{"kind":"prior_call","id":"<uuid>","quote":...}` from `[call <uuid>]`.
- Never emit a placeholder or empty citation. Every answer must cite at least one thing.

## Lookups without a session

- `resolve_card {name}` / `judge-cli card "<name>"`: the pipeline's own resolution
  (aliases, printed names, fuzzy; `[[Full Card Name]]` matches that exact name only, with
  near spellings offered as `ambiguous`); returns `resolved` with the card
  (faces, Oracle text), or `ambiguous` with `candidates`, or `not_found`. Never guesses.
- `card_info {card}` / `judge-cli card-info <oracle-uuid>`: faces, all Scryfall rulings,
  notes for tricky cards.
- `get_rules {ids}` / `judge-cli get-rules <id>...`: CR chunks by id or subsection, at
  most 10 ids.
- `search_rules {query, limit?}` / `judge-cli search "<query>" [--limit N]`: full text
  (plus vector similarity when configured), at most 25 chunks. Rules vocabulary works best.
- `glossary {term}` / `judge-cli glossary <term>`.

## Running the CLI

Build once: `cargo build --release -p judge-agent`; then run `target/release/judge-cli`
from the repo root (it reads `.env` there: `DATABASE_URL` is required; a model — `ANTHROPIC_API_KEY`,
or a `judge.toml` named by `JUDGE_CONFIG` — only for `judge`; `VOYAGE_API_KEY` optional;
`judge-cli config` prints what resolved, secrets redacted). Logs go to stderr, JSON to stdout. Flags can
go anywhere after the subcommand; `--` ends them if a question starts with `--`. Write
extraction and verdict JSON to a file and pass its path, or pipe it with `-`. Each
invocation is its own process, so `judge` there has its own `JUDGE_MAX_USD` counter.

The stdio MCP server for local use is `target/release/judge-mcp`; the repo's `.mcp.json`
starts it, so build first or the server shows as failed. Same environment; it too is its
own process with its own cap.

Remote database: connect the MCP server over HTTP (`claude mcp add --transport http judge
https://<host>/mcp --header "Authorization: Bearer <MCP_TOKEN>"`), or run the same
commands on the deploy host (`docker compose run --rm --entrypoint judge-cli api ...`).
