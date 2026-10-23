---
name: judge
description: "Answer a Magic: The Gathering (MTG) rules question with the judgebot pipeline. Either call its built-in model (costs API budget) or do the model's work yourself through a session whose citations are validated against the Comprehensive Rules (CR), Oracle text and Scryfall rulings. Also for looking up cards, CR rules text, rulings and glossary terms, and for the operator's reads of the instance (stored calls, private ones included, failed calls, usage and spend) and working its feedback queue (ratings given a reason). Use when asked a rules question, to check a ruling or an interaction, to consult the judgebot database, to troubleshoot what the instance answered, or to work through the feedback queue."
---

# Judge

The judgebot answers Magic rules questions from the Comprehensive Rules (CR), current
Oracle text, Scryfall rulings and rated prior calls. It **validates every citation**:
a rule id must appear in the material and the quote must be a verbatim substring of it.
This skill drives that pipeline from outside.

There are two transports with the same operations. Prefer MCP when the `judge` MCP server
is connected (tool names below). Otherwise use the CLI, `target/release/judge-cli`
(subcommands below, written as `judge-cli` here). Replies are JSON. Where the outcome
varies they are tagged by `kind`, except `session_prompt`, which is tagged by `step`. A
failure you cannot act on is `{"kind":"error","message":...}` (CLI exit 1). The message
says what was wrong: unknown or expired session, wrong step, over a limit, concurrent
step.

## Two ways to answer

**1. Built-in pipeline.** `judge {question, thread?, pins?}` / `judge-cli judge
"<question>" [--thread T] [--pin "span=Full Name"]...`. The server runs its own model calls
and returns `answer` (validated and cited, plus `thread` and `call`). It spends the
operator's model budget, about $0.10 a question on the default Anthropic setup. Other
replies:

- `unavailable`: the server has no API key.
- `rate_limited`: over HTTP, the client's hourly quota is used.
- `busy`: every slot is taken.
- `ambiguous` (`spans: [{query, choices, truncated}]`): ask again with
  `pins: [{span, name}]` / `--pin "span=Full Name"`.
- `not_found` (`names`), `out_of_scope` or `error`.

Use it when the user explicitly wants the bot's own answer, or to compare against yours. Never loop
on it.

**2. Session.** You are the model. No API call is made. Steps, in order:

1. `begin_session {question, thread?}` / `judge-cli begin "<question>" [--thread T]`
   → `{session, thread, extraction: {system, user, schema}}`. That is the **extraction
   prompt**.
2. Produce the extraction JSON yourself, following `extraction.system` exactly:
   - `card_spans` copied character for character from the question (keep `[[brackets]]`)
   - `concepts` in rules vocabulary
   - `primary` + up to two `secondary` categories from the taxonomy listed in the prompt
   - `source` (`cr` | `commander` | `tournament` | `out_of_scope`)

   At most 20 spans and 20 concepts, each at most 200 characters.
3. `submit_extraction {session, extraction}` / `judge-cli extract <session> <file|->`
   → `ready` with the **synthesis prompt** at top level (`system`, `material`, `question`,
   `schema`, `lookup_available`), or:
   - `ambiguous` (`spans`): a span matched several cards. The session is unchanged. Pick
     the card the user meant (ask them if it is unclear), replace that span in
     `card_spans` with `[[Full Card Name]]`, submit again.
   - `not_found` (`names`): fix the spelling or drop the span, submit again.
   - `out_of_scope`: tournament policy or not a rules question. The session is closed.
     Tell the user the bot does not cover it.
4. Read the material. If the rule you need is not in it, call `lookup_rules {session, ids}`
   / `judge-cli rules <session> <id>...` **once**, with every id you need. Ids are rule
   ids like `702.19` or `613.7b`, or a subsection like `613`. At most 10 ids. Then re-read the
   prompt with `session_prompt {session}` / `judge-cli prompt <session>` (reply
   `{step: "synthesis", system, material, question, schema, lookup_available}`). The
   fetched rules are now in the material and citable. A second lookup is refused, and a
   rejected verdict forfeits the round, so look up before you answer.
5. Write the verdict JSON following the synthesis `system` prompt: `answer`, `confidence`,
   `citations`, `category`. The `answer` gives the ruling first, then reasoning with rule
   numbers. Aim for under 1500 characters. The hard limit is 4000, and an oversized answer
   is a rejection that spends your one retry.
6. `submit_verdict {session, verdict, persist?}` / `judge-cli verdict <session> <file|->
   [--persist]` →
   - `accepted`: the answer with citation labels and links, and `call` if persisted. If
     persisting failed there is a `persist_error`, the verdict still stands and
     `persist_session` can be retried.
   - `rejected`: `reason`, the typed `rejection`, and `retry: {system, material, question,
     schema, lookup_available}`. That is the prompt again, with a "Previous attempt
     rejected" notice in `question` saying which citation failed and why. Fix it and
     submit once more.
   - `exhausted`: the retry failed too and the session is closed. Start a new session if
     you want another go.
7. Give the user the accepted answer, and its `note` first when there is one: a card in
   the question is not out yet and the loaded CR may not cover it (D42). The material
   says so under that card's heading; answer from its Oracle and reminder text and say
   what the rules shown leave unsettled. Pass `persist: true` (or `persist_session
   {session}` / `judge-cli persist <session>`) when the user may ask a follow-up. A
   persisted call is the thread's history for the next `begin_session` with the same
   `thread`. It is never shown to other askers as an example.

`session_status {session}` / `judge-cli status <session>` says where a session is
(`stage`, `lookup_available`, `attempts`, `outcome`). Sessions expire after **one hour
idle**. An expired or unknown id is an `error`, so do not park a session while doing
something long.

For a **follow-up question** in the same conversation, pass the `thread` from the earlier
reply to `begin_session` / `--thread`. The earlier Q&A is rendered as history. Only
`agent:<uuid>` ids are accepted, so an agent can never read or write a Discord or web
thread.

If the material is long, hand the synthesis prompt to a subagent with a fresh context.
Give it `system` as its instructions and `material` + `question` as the task, and ask for
the JSON only. Do it promptly, because the session clock is running.

## Citation rules

- Copy quotes **verbatim from the material as shown**. Punctuation is forgiven: ASCII
  `'`, `"` and `-` match the CR's `’`, `“”` and `—`, and the accepted citation comes back
  carrying the source's own typography. Nothing else is forgiven. A changed word, a
  dropped word or a different capitalisation is still a rejection.
- Keep a quote inside one line of the source and short (the prompt asks for at most 200
  characters). Use two citations rather than one spanning lines.
- Cite the finest rule containing the quote: `702.19b` for a line that starts with
  `702.19b`, `702.19` only for its own first line. Subsections (`613`) are never citable,
  even after a lookup.
- Rulings: `{"kind":"scryfall_ruling","card":"<uuid>","ruling":"<16-char key>","quote":...}`.
  Copy the uuid from the card's heading in the rulings section (`### <Card Name> —
  card <uuid>`) and the key from the `[ruling <key>]` label under it.
- Oracle text: `{"kind":"oracle_text","card":"<uuid>","face":0,"quote":...}` from the
  `[oracle <uuid>#<face>]` label on the face in the Cards section.
- Prior calls: `{"kind":"prior_call","id":"<uuid>","quote":...}` from `[call <uuid>]`.
- Never emit a placeholder or empty citation. Every answer must cite at least one thing.
- Every rule number you write in the answer text must be one of your rule citations (the
  id, its rule, or one of its sub-rules). Cite it or do not name it.

## Lookups without a session

- `resolve_card {name}` / `judge-cli card "<name>"`: the pipeline's own resolution
  (aliases, printed names, fuzzy). `[[Full Card Name]]` matches that exact name or a listed
  nickname only, with near spellings offered as `ambiguous`. Returns `resolved` with the card (faces, Oracle
  text), or `ambiguous` with `candidates`, or `not_found`. Never guesses.
- `card_info {card}` / `judge-cli card-info <oracle-uuid>`: faces, all Scryfall rulings,
  notes for tricky cards.
- `get_rules {ids}` / `judge-cli get-rules <id>...`: CR chunks by id or subsection, at
  most 10 ids.
- `search_rules {query, limit?}` / `judge-cli search "<query>" [--limit N]`: full text
  (plus vector similarity when configured), at most 25 chunks. Rules vocabulary works best.
- `glossary {term}` / `judge-cli glossary <term>`.

## Operator reads

What the instance stored, private material included: other people's questions and
Discord ids. Keep it to the operator's own conversation. These are on `judge-cli` and
stdio `judge-mcp` (whoever runs them holds the database), and on `/mcp` only with
`MCP_ADMIN_TOKEN`, never `MCP_TOKEN`.

- `admin_calls {cursor?, interface?, asker?, model?, private?, q?}` / `judge-cli calls
  [--interface I] [--asker A] [--model M] [--private | --public] [--search Q] [--cursor
  C]`: every stored call, newest first, 50 a page (`next` is the next `cursor`). `asker`
  is a Discord username or user id, `models` lists every model a run billed. Each row
  has the asker, `private`, its run's `models` and `usd`.
- `admin_call {call}` / `judge-cli call <id>`: one call whole: origin, thread, question
  and answer verbatim, citations, every rating with its rater, and the run with each
  billed request (stage, model, tokens, cost). `call` is null for an unknown id.
- `admin_failures {limit?}` / `judge-cli failures [--limit N]`: failed calls with the
  question, the error, the first rejection and each attempt's text.
- `admin_usage {days?, from?, to?, interface?, asker?, model?}` / `judge-cli stats
  [--days N | --from YYYY-MM-DD] [--to YYYY-MM-DD] [--interface I] [--asker A] [--model M]`:
  the admin dashboard (the same report as the admin web page). Questions per day by
  interface (private counted apart), outcomes, ratings, top categories, cards and askers,
  cost and models; spend per day and month, the budget and what is left, runs by model,
  interface and stage, cost per answer, latency, the dearest runs (`run_stats`, null with
  `asker`), failed calls, the lowest-rated calls, the queue's counts, recent refreshes.

To look into a reported bad answer: find it with `admin_calls` (`q` with words from it,
or `asker`), read it with `admin_call`, then reproduce it with a session above.
`judge-cli admin-url` prints the admin web page's address for a person.

## The feedback queue

Every *Partially correct* or *Incorrect* rating given a reason is an item in the
feedback queue: the category (`wrong_conclusion`, `missed_part`, `bad_citation`,
`wrong_card`, `misread_question`, `wrong_refusal`, `outdated`, `other`), and optionally
what is wrong and the right answer, in the rater's words. The reasons are for the
operator, never the model: nothing in retrieval reads them. Same access as the reads
above.

- `queue_list {cursor?, status?, category?, interface?, rater?}` / `judge-cli queue
  [--status open|resolved|dismissed] [--category C] [--interface I] [--rater discord|web]
  [--cursor C]`: newest first (by when each was opened or reopened), 50 a page. Each item
  has its `id`, the reason, the rating as it stands now (`score`), the rater, the
  operator's `note` and `fixed_by`, and the call's question on one line.
- `queue_show {item}` / `judge-cli queue-show <item>`: the item with its call whole
  (question and answer verbatim, citations, cards, the run's models, retired state, every
  rating with its reason).
- `queue_resolve {item, note, fixed_by?}` / `judge-cli queue-resolve <item> --note N
  [--fixed-by F]`: fixed. `note` (required, at most 2000 characters) says what was wrong
  and what you did; `fixed_by` (at most 200) names the fix.
- `queue_dismiss {item, note}` / `judge-cli queue-dismiss <item> --note N`: nothing to
  fix (the answer was right, or the reason is not about the answer), and why. With
  `JUDGE_PUBLIC_FEEDBACK=true` it also takes the reason off the public pages: dismiss
  spam and abuse.

To work the queue:

1. `queue_list` with `status: "open"`. Take one item at a time.
2. `queue_show` it. Read the reason against the answer and its citations. A rater can
   be wrong too: check the claim against the CR (`get_rules`, `search_rules`) and the
   card (`card_info`) before you change anything.
3. Reproduce it with a session (`begin_session` with the call's question, no API spend),
   not `judge`. What the session shows decides the fix:
   - The wrong card was resolved, or a nickname missed: an alias in `data/aliases.yaml`.
   - A card whose interaction the material does not explain: a note in
     `data/notes.yaml`.
   - The right rule was not retrieved, or the prompt led the model astray: a retrieval
     or prompt change. Check retrieval with `cargo run -p judge-eval -- recall` (no API
     spend), and add the question to `eval/gold.yaml` when it is a new capability.
   - The answer was right: dismiss it, saying which rule settles it.
4. Make the fix, then `queue_resolve` with a note naming what you found and did and
   `fixed_by` naming the change (a commit, the `notes.yaml` entry). Resolving changes no
   answer by itself: a fix in the data reaches new questions after the next load.

A rater who replaces their reason reopens the item, with your note kept. An item whose
rater re-rated the answer Correct stays only if it has a note. The admin web page's
Feedback tab (`judge-cli admin-url`) is the same queue for a person.

## Running the CLI

Build once with `cargo build --release -p judge-agent`, then run
`target/release/judge-cli` from the repo root. It reads `.env` there:

- `DATABASE_URL` is required.
- A model (`ANTHROPIC_API_KEY`, or a `judge.toml` named by `JUDGE_CONFIG`) is needed only
  for `judge`.
- `VOYAGE_API_KEY` is optional.

`judge-cli config` prints what resolved, secrets redacted. `judge-cli about` prints the
source offer (repository, commit, licence) and the data's freshness (CR release, last
refresh). Logs go to stderr, JSON to stdout. Flags can go
anywhere after the subcommand, and `--` ends them if a question starts with `--`. Write
extraction and verdict JSON to a file and pass its path, or pipe it with `-`. Each
invocation is its own process, so `judge` there has its own `JUDGE_MAX_USD` counter.

The stdio MCP server for local use is `target/release/judge-mcp`. The repo's `.mcp.json`
starts it, so build first or the server shows as failed. It uses the same environment and
is also its own process with its own cap.

Remote database: connect the MCP server over HTTP (`claude mcp add --transport http judge
https://<host>/mcp --header "Authorization: Bearer <MCP_TOKEN>"`; `MCP_ADMIN_TOKEN` adds
the operator reads), run stdio `judge-mcp` over SSH (`ssh <host> docker compose exec -T
judgebot judge-mcp`, operator reads included), or run the same commands on the deploy
host (`docker compose run --rm --entrypoint judge-cli judgebot ...`).
