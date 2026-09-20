---
title: Database schema
description: The tables, what fills them, and the invariants the loaders and readers agree on.
sidebar:
  order: 2
---

The schema is one Postgres database with the `vector` (pgvector) and `pg_trgm` extensions.
Migrations live in `crates/bot/migrations/` and are embedded in the binaries. `bot` and
`api` apply pending ones at startup, and `judge-ingest migrate` is the explicit form. Every
SQL query is
checked at compile time against this schema (`sqlx`, with the committed offline data in
`.sqlx/`).

## Cards (from Scryfall)

| Table | Contents |
| --- | --- |
| `cards` | One row per card (Scryfall `oracle_id`), with its name and layout. |
| `card_faces` | One row per face (every card has at least one): name, Oracle text, mana cost, type line. |
| `printed_names` | Names a card was printed under before errata or renaming, so historical names still resolve. (Short names like "Ragavan" resolve by the name before the comma, not through this table.) |
| `rulings` | Scryfall rulings, keyed by content (`ruling_key`, sixteen hex characters) so a reindexed ruling is the same ruling. |
| `card_aliases` | Nicknames from `data/aliases.yaml`, lowercased. |
| `card_notes` | Hand-written notes from `data/notes.yaml`. |

## Rules (from the Comprehensive Rules)

| Table | Contents |
| --- | --- |
| `rules` | Two granularities. Rule-level rows (`702.19`) have a body that includes the lettered sub-rules and examples. They carry embeddings and feed retrieval. Leaf rows (`702.19b`, `parent_id` set) are citation targets. A `cr_version` per load. |
| `glossary` | Glossary entries, embedded. |
| `categories` | The taxonomy from `data/categories.yaml` with each category's CR sections. |

A new CR release re-embeds only rules whose text changed. Inside the load transaction, a
renumbering pass matches old and new rules by body with every rule id masked. It rewrites
the stored calls' citations for rules that moved, only where the match is unambiguous.

## Calls and ratings

| Table | Contents |
| --- | --- |
| `calls` | Every answered question (except a Discord question asked with `private: True`): thread id, question, answer, category, source, confidence, the citations, the ids of the context it was answered from, `cr_version`, an embedding, and `retired_at`/`retired_reason` when its citations stopped holding. The context ids include a fingerprint of each context card's Oracle text, computed at persist time, so an erratum retires calls *about* a card even when they cited only the CR. `session_id` is set for calls persisted through an agent session. |
| `ratings` | One row per (call, user id): score 1 to 3, whether the rater held the judge role, timestamp. The only per-user data. `/forget` deletes a user's rows. |
| `calls_rated` (view) | Per call: the Bayesian-smoothed mean (prior 2.0, weight 3), the vote count, the latest judge rating, and the effective score the retriever uses. |

Retired calls and session-persisted calls never appear as examples. A call below 1.5 with
at least five votes is excluded too.

## Operations

| Table | Contents |
| --- | --- |
| `agent_sessions` | In-flight agent sessions (stage, question, context, rejection). Expired rows are swept when the next session is created. |
| `spend_days` | Estimated model spend and model calls of `bot` and `api` per UTC day. Each process adds its own share every ten seconds. `JUDGE_BUDGET_PERIOD=day\|month` sums the current period from it, and `judge-cli stats` reads it. No per-user or per-question data. |
| `embedding_space` | One row naming the embedder whose vectors the database holds (provider kind, model, dimensions). `ingest embed` writes it on first use and refuses to mix. `ingest reembed --yes` is the only thing that changes it. |
| `_sqlx_migrations` | The migration ledger. |

Writers that depend on the schema of the rules or the embedding space (the CR loader, the
retirement pass, `reembed`, every `persist` and `embed` batch) coordinate through one
advisory lock. A switch waits for in-flight writes, and a write after it sees the new
state.
