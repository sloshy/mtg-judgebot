---
title: Database schema
description: The tables, what fills them, and the invariants the loaders and readers agree on.
sidebar:
  order: 2
---

The schema is one Postgres database with the `vector` (pgvector) and `pg_trgm` extensions.
Migrations live in `crates/bot/migrations/` and are embedded in the binaries. `judgebot`
applies pending ones at startup. `judgebot ingest migrate` applies them explicitly. Every
SQL query is checked against this schema at compile time (`sqlx`, with the committed
offline data in `.sqlx/`).

## Cards (from Scryfall)

| Table | Contents |
| --- | --- |
| `cards` | One row per card (Scryfall `oracle_id`), with its name and layout. |
| `card_faces` | One row per face (every card has at least one): name, Oracle text, mana cost, type line. |
| `printed_names` | Names a card was printed under before errata or renaming, so historical names still resolve. (Short names like "Ragavan" resolve by the name before the comma, not through this table.) |
| `rulings` | Scryfall rulings, keyed by content (`ruling_key`, sixteen hex characters) so a reindexed ruling is the same ruling. |
| `card_aliases` | Nicknames from `data/aliases.yaml`, lowercased. |
| `card_notes` | Hand-written notes from `data/notes.yaml`. |
| `curated_lists` | One row per curated list (`aliases`, `notes`): where the loaded copy came from (`builtin` or `file`), the SHA-256 of its YAML and when it was loaded. Written in the transaction that replaces the list. The refresh reloads a `builtin` list whose digest is not the binary's copy's, and never a `file` one. |

## Rules

| Table | Contents |
| --- | --- |
| `rules` | Two levels of row. Rule-level rows (`702.19`) have a body that includes the lettered sub-rules and examples. They carry embeddings and feed retrieval. Leaf rows (`702.19b`, `parent_id` set) are what answers cite. Each load records a `cr_version`. |
| `glossary` | Glossary entries, embedded. |
| `categories` | The taxonomy from `data/categories.yaml` with each category's CR sections. |

A new CR release re-embeds only rules whose text changed. When rules are renumbered,
stored calls keep their citations. Inside the load transaction, old and new rules are
matched by body text with every rule id masked. Citations to a rule that moved are
rewritten, but only where the match is unambiguous.

## Calls and ratings

| Table | Contents |
| --- | --- |
| `calls` | Every answered question, private ones included. Holds the thread id, `interface` (`discord`, `web`, `mcp`, `cli`, `agent_session`, or `agent` for an MCP or `judge-cli` call from before 2.0.0), `asker_id` and `asker_name` (the Discord user id and username when asked; null for the web app and agents), `private`, `locale`, the question, answer, category, source, confidence, citations, the ids of the context it was answered from, `cr_version` and an embedding (none for a private call). `retired_at`/`retired_reason` are set when its citations stop holding. `/forget` sets `asker_id` and `asker_name` to null, and on a private call replaces the question and answer. The context ids include a fingerprint of each context card's Oracle text, taken when the call is saved. An erratum therefore retires calls *about* a card even when they cited only the CR. `session_id` is set for calls saved through an agent session. With `JUDGE_CALL_RETENTION_DAYS` set, the refresh deletes calls older than that, with their ratings; their `judge_runs` rows stay. The web app's public feed reads every call not marked `private` (the username, never the id), within `JUDGE_FEED_DAYS`. |
| `ratings` | One row per (call, user id): score 1 to 3, whether the rater held the judge role, timestamp. `/forget` deletes a user's rows. |
| `calls_rated` (view) | Per call: the Bayesian-smoothed mean (prior 2.0, weight 3), the vote count, the latest judge rating, and the effective score the retriever uses. |

Some calls never appear as examples to the model: private calls, retired calls, calls
saved through an agent session, and calls scoring below 1.5 with at least five votes. A
private call is never read as a public question's thread history either.

## Operations

| Table | Contents |
| --- | --- |
| `agent_sessions` | Agent sessions in progress (stage, question, `private`, context, rejection). Expired rows are deleted when the next session is created. |
| `spend_days` | Estimated model spend and model calls of the serving processes per UTC day. Each process adds its own share every ten seconds. `JUDGE_BUDGET_PERIOD=day\|month` sums the current period from it, and `judge-cli stats` reads it. No per-user or per-question data. |
| `spend_warnings` | One row per spend warning sent in a `day` or `month` budget period (`JUDGE_SPEND_WARN_AT`): the period, its first day, the threshold's amount and percentage, when it was sent. A process sends a warning only when its insert lands, so one process sends each. Rows older than two months are deleted as new ones are claimed. |
| `pick_claims` | One row per "did you mean?" prompt a card was picked from: the Discord message id, when the prompt was shown and a digest of the message (8 bytes of SHA-256, not the text). It makes each prompt answer one click. No question, user or card is stored, so a `private: True` question leaves no text here. Rows older than a day are deleted as new picks are claimed. |
| `failed_calls` | One row per call that failed for a reason the operator has to diagnose: the thread id, `interface`, `private`, `asker_id` and `asker_name` (the Discord asker; null for the web app and agents), the question, the error, why the first synthesis attempt was rejected and the answer text of each attempt, each cut to a fixed length. `/forget` sets `asker_id` and `asker_name` to null and replaces the question, error and text. Inserting a row deletes rows older than 30 days or past the newest 500 (pruning happens only on insert). An attempt rejected for tool misuse or truncated has no text. `judge-cli failures` reads it. |
| `judge_runs` | One row per run of the built-in pipeline (Discord, the web app and API, the MCP `judge` tool, `judge-cli judge`), whatever its outcome: when it started and finished, `interface`, `private`, `outcome` (`answered`, `ambiguous`, `not_found`, `out_of_scope`, `refused`, `at_cap`, `abandoned` (the caller went away and the run sent no further model call), `upstream` or a citation failure), whether the synthesis retry was attempted (`retried`) and whether the `lookup_rules` round ran (`tool_round`). `call_id` or `failed_call_id` links what it left. Deleting that call or failed call keeps the run, unlinked. An agent-driven session has no row. No question text or user id. |
| `judge_run_charges` | One row per billed request of a run, in order: `stage` (`extraction`, `synthesis`, `tool_round`, `embedding`), the provider, the model the response named, the input, output and cache tokens, and `micro_usd`. When a response reported no usage, the tokens are null and `micro_usd` is the worst case the spend cap kept for it, flagged by `cost_estimated`. A free model costs 0. |
| `instance_release` | One row: the release and commit of the binary that last migrated the database, whether its tree had uncommitted changes, and when. `judgebot` refuses to start against a database another major release (or another `-dev` build) migrated. `judge-cli` and a stdio `judge-mcp` compare their own release with it. |
| `refresh_runs` | One row per `judgebot ingest refresh` or scheduled refresh: when it started and finished, `trigger` (`manual` or `schedule`) and the process that ran it, the stored CR version before and after, each step's outcome (`steps`, JSON) and `ok`. A row with no `finished_at` is a run in progress or one that died. |
| `embedding_space` | One row naming the embedder whose vectors the database holds (provider kind, model, dimensions). `ingest embed` writes it on first use and refuses to mix embedders. Only `ingest reembed --yes` changes it. |
| `gateway_handover` | One row: when the last Discord gateway holder released the lease after its gateway closed at shutdown, or null while one holds it. A process taking the lease within 5 seconds of a release connects without the 15-second grace. |
| `_sqlx_migrations` | The migration ledger. |

Writers that depend on the rules or the embedding space share one Postgres advisory lock:
the CR loader, the retirement pass, `reembed`, and every `persist` and `embed` batch. The
first three wait for writes in progress, and a write after them sees the new state.
A second lock, the refresh lease, is held for a whole data-writing `judgebot ingest` command,
so two never overlap. Postgres drops either with the session that held it.
