# mtg-judgebot

A Discord bot (and anonymous web page) that answers Magic: The Gathering rules
questions like a judge: ask in plain language (nicknames welcome — "bob", "goyf",
"snappy"), get a concise ruling in which **every claim is backed by a validated,
clickable citation** — a Comprehensive Rules section, an official Scryfall ruling, the
card's current Oracle text, or a prior rated call. On Discord, answers can be rated 1–3
(incorrect / partially correct / correct) with buttons; ratings feed back into how
future answers are grounded.

```
/judge question: does bob's trigger count goyf's mana value as 0?

@you asked: does bob's trigger count goyf's mana value as 0?
You'll lose 2 life, not 0. Tarmogoyf's mana value is 0 anywhere its {X}… [702.19b] …
  Citations: [202.3](…CR link…) · [Ruling (2021-02-05) — Tarmogoyf](…scryfall…) · [Oracle text — Dark Confidant](…)
  Confidence: High · CR 2026-08-19        [Incorrect] [Partially correct] [Correct]
```

## How it answers

1. **Extract & classify** (LLM, structured output): card-name spans vs. rules concepts,
   up to three categories from a fixed taxonomy, and a scope check — tournament-policy
   and price questions are politely refused without spending on synthesis.
2. **Resolve cards** through a typed ladder: alias table (nicknames, possessives) →
   `[[bracket]]` syntax → old printed names → short names ("Ragavan") → trigram fuzzy.
   The bot **never guesses**: genuine ambiguity ("Tibalt") becomes a "Did you mean…?"
   button row.
3. **Retrieve** from Postgres: a curated category→CR-section map, full-text search, and
   pgvector semantic search over rule embeddings — plus the cards' rulings, glossary
   entries, hand-written notes for "nightmare" cards (Humility, Blood Moon…), and
   similar prior calls labeled with their community rating.
4. **Synthesize** (LLM): a judge-persona prompt over exactly that material, with one
   optional `lookup_rules` tool round (type-enforced to at most one). Every citation
   must quote its source verbatim; hallucinated citations are rejected and retried, and
   an answer with no citations never ships.
5. **Learn from ratings**: a Bayesian-smoothed score per call (with an override for
   users holding a Judge role) decides which prior calls are shown, warned about, or
   excluded — the CR itself always outranks precedent.

## Running it

Requirements: Docker, Rust 1.97+, an Anthropic API key, a Discord bot token; optional
Voyage AI key for the semantic-search leg.

```sh
cp .env.example .env          # fill in keys, DISCORD_TOKEN, GUILD_ID
docker compose up -d          # pgvector Postgres (localhost:5433) + bot + web API
~/.cargo/bin/sqlx migrate run --source crates/bot/migrations

# one-time data load (~110 MB from Scryfall, cached in .cache/)
cargo run --release -p judge-ingest -- cards
cargo run --release -p judge-ingest -- rules latest  # the CR release Wizards' rules page links
cargo run --release -p judge-ingest -- aliases data/aliases.yaml
cargo run --release -p judge-ingest -- notes data/notes.yaml
cargo run --release -p judge-ingest -- embed        # needs VOYAGE_API_KEY

docker compose up -d --build bot api                # redeploy after code changes
scripts/refresh-data.sh                             # nightly: cards, new CR, embeddings, emoji
```

Invite the bot with the `bot` + `applications.commands` scopes; `/judge` registers
instantly in the guild named by `GUILD_ID`. Every LLM call is metered and hard-capped
(`JUDGE_MAX_USD`); a typical answer costs $0.08–0.25.

### The web page

`docker compose up -d` also serves an anonymous web front end on
<http://localhost:8787> (SolidJS, built into the image): the same pipeline, citations
and "did you mean…?" flow in the browser, no login. Because nobody is logged in there
are **no rating buttons** on the web, and anonymous traffic is rate limited per IP
(`API_RATE_LIMIT` questions per `API_RATE_WINDOW_SECS`, default 4 per 5 minutes) on
top of the global spend cap. For local development:

```sh
cargo run --release -p judge-api     # API + static page on localhost:8787
npm --prefix web install
npm --prefix web run dev             # Vite dev server with /api proxied to :8787
```

### Hosting

The public instance runs on a machine at home behind a [Cloudflare
Tunnel](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/):
no public IP, no forwarded port, no cloud compute bill. `docs/DEPLOYMENT.md` is the
runbook — tunnel setup, edge rate limiting in front of the anonymous API, and the
weekly R2 backup.

## Evaluation

`eval/gold.yaml` holds 21 adversarially verified questions (layers nightmares,
multi-face cards, errata traps, Commander, out-of-scope) with expected rule ids and
reference answers.

```sh
cargo run -p judge-eval -- recall            # retrieval gate: expected rules present in context? (free)
cargo run -p judge-eval -- answer --label x --limit 21 --max-usd 6   # full live run (~$2.50)
cargo run -p judge-eval -- rescore eval/runs/x.json                  # re-grade a stored run (free)
cargo run -p judge-eval -- show eval/runs/x.json                     # bot vs. gold, side by side
```

Scoring accepts per-question *equivalence lists* — alternate rule ids that state the
same fact — so the metric tracks correctness rather than one author's citation taste.

## Design

Rust was chosen for compiler-enforced correctness (the comparison lives in
`docs/LANGUAGE_EVALUATION.md`): closed enums for every domain sum, refined newtypes for
ids, `Verdict<Unvalidated> → validate() → Verdict<Validated>` so an unchecked answer
*cannot* be persisted or shown, a typestate on the synthesis loop so the tool round
cannot repeat, compile-time-checked SQL (sqlx + committed offline data), and an LLM
output schema derived from the same structs the responses parse into.
`docs/ARCHITECTURE.md` is the living design document.

```
crates/
  core       domain types, ports, judge() pipeline, citation validation — no I/O
  llm        provider-neutral chat types, Backend + sealed ChatModel port, spend cap, retry loop, Synth typestate
  anthropic  the Messages API as a judge-llm backend: wire types, schema transform, endpoints
  openai     OpenAI-compatible chat completions as a judge-llm backend: strict-schema transform, dialect knobs
  embed      Voyage embeddings
  bot        Postgres adapters (resolver / retriever / call store), prompts, Discord (serenity/poise)
  ingest     Scryfall + Comprehensive Rules loaders, embedder  (bin)
  eval       gold-set harness: recall / answer / rescore / show (bin)
  api        anonymous HTTP adapter (axum) serving the web page (bin)
web/         SolidJS + TypeScript single page (Vite)
data/        categories.yaml (generates the Category enum), aliases.yaml, notes.yaml
eval/        gold.yaml + stored runs
docs/        architecture, language evaluation, proposals
```

Not yet built: multi-server tenancy, tournament-policy (MTR/IPG) coverage — the bot declines those
questions rather than winging them.

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE). Running a modified version of this bot
(Discord or the HTTP API) as a network service requires making the modified source
available to its users.
