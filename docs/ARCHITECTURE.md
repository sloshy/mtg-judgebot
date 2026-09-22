# MTG Judge Bot architecture

The design reference, kept current with the code. It was written stack-independent, before
the language was chosen (Rust, 2026-08-29).
`docs/DECISIONS.md` records why each choice was made. `docs/EXPLAINER.md` is the narrative
version for someone new to the ideas. This file is the terse one.

## 1. Goal

A Discord bot that answers Magic: The Gathering rules questions. Users reference
cards by full name, partial name, or nickname ("bob" → Dark Confidant). Every
answer cites its sources: Comprehensive Rules (CR) sections, Scryfall rulings,
and/or prior rated calls. Answers and user-submitted calls are rated 1–3
(incorrect / partially correct / correct). Ratings feed back into retrieval as
*examples*, never as authorities over the CR.

## 2. Entity-first hybrid retrieval

The design principle. Rules questions have the shape "card A + card B + rule concept C":

- **Card names are entities** → extraction + lookup, not embeddings.
- **CR is a numbered hierarchy** → chunk at the *rule* level (e.g. `702.19` with
  all lettered sub-rules and `Example:` lines), keep parent links, and map
  question categories to whole subsections (e.g. `613`, `614`).
- **Scryfall rulings are keyed by card** → fetch directly once cards resolve.
- **Three retrieval legs** for the fuzzy part: category map (structured),
  BM25/full-text (keywords like "leaves the battlefield"), vector (semantic).
- **The CR always outranks prior calls.**

## 3. Pipeline

```
Discord message (+ last N Q&A in the same thread)
  │
  ▼
[1] Entity extraction (LLM, low effort, structured output)
    Splits the message into card-name spans and rules concepts. Runs FIRST so
    fuzzy matching sees only candidate spans, not rules vocabulary.
    Model choice: `judge_bot::config` picks a provider per stage, from a
    `judge.toml`, else Anthropic direct from the environment. A provider is
    either Anthropic's Messages API or any OpenAI-compatible chat completions
    server. The Messages API is reached direct, through a proxy, or on a
    cloud account: Claude Platform on AWS and Bedrock (SigV4), Vertex AI
    (ADC). Cloud credentials come from the platform chain and are probed at
    startup.
    Every model sits behind the process's one spend-capped `Metered`.
    A backend reports its `Capabilities`. If it cannot enforce the output
    schema server-side, the adapter appends the schema to the *user turn*.
    The system prompt is pinned by digest and is byte-identical on every
    backend.
  │
  ▼
[2] Card resolution (per span)
    A typed ladder, each rung tried only when the one above found nothing:
    alias → possessive-stripped alias ("bob's" → "bob") →
    exact name → printed-name table → short name before the comma ("Ragavan")
    → alias as a suffix → trigram fuzzy
    A [[bracketed]] span has its own ladder: exact name → printed name.
    It is CardSpan::Exact, chosen by an exhaustive match, so no loose rung
    can resolve it. A miss is never resolved, only offered as Ambiguous:
      - the cards the naming rungs (alias, possessive, short name, alias
        suffix) point at, under that rung's matchedVia, so [[bolt]] offers
        Lightning Bolt. Dropped as a duplicate when the extractor also
        named the card.
      - with no naming-rung hit, the fuzzy neighbours.
    Output: Resolution = Resolved(card, matchedVia) | Ambiguous(candidates) | NotFound
    Duplicate references (nickname + full name) are dropped. A span is a
    duplicate when it is
      - Ambiguous from a non-fuzzy rung and shares a candidate with a card
        Resolved from another span, or
      - NotFound, but its words appear as whole words in a resolved card's
        (face) name.
    Fuzzy-ambiguous spans are never dropped: trigram neighbours are not
    names the user could have meant.
    Remaining Ambiguous ⇒ "did you mean…?" buttons and stop. Never guess.
  │
  ▼
[3] Classification (same LLM call as [1])
    One required primary category plus up to 2 secondary, each with
    confidence, from a fixed taxonomy (~25 entries mirroring CR structure).
    The schema requires the primary, so an unclassified answer is an API error.
    Also assigns Source: CR | Commander | Tournament(MTR/IPG) | OutOfScope.
    Tournament/OutOfScope ⇒ reply "I don't cover that" with a pointer.
    Flags "nightmare" cards (Humility, Opalescence, Blood Moon, Mycosynth
    Lattice, Panglacial Wurm, …) → inject hand-written notes.
  │
  ▼
[4] Retrieval → Context
    - CR: category → curated subsection IDs (always) + tsvector BM25 + pgvector
          cosine. Union, dedupe, expand to full rule chunk.
          Order matters: the synthesis budget (25 chunks / 30 kB) renders
          only a prefix, and the legs return several times that. Priority:
            1. the primary category's rules sharing a word with the
               question (ranked by text relevance, not by id)
            2. BM25
            3. vector
            4. the primary's remaining rules
            5. secondary categories (ranked)
          `eval recall` gates on what was retrieved (≥ 90%) and on what
          that prefix shows (≥ 75%).
    - Scryfall rulings for each resolved card (all faces)
    - Glossary entries for terms in the oracle text
    - Prior calls: vector search filtered by (cards ∩ category), labeled with
      rating and CR version, shown as examples AFTER the CR material.
      Retired calls are excluded. The nightly pass retires a call when a
      citation's source no longer contains its quote, or a context card's
      Oracle text changed. It restores the call when both hold again.
      Renumbered rules carry their calls with them.
    - Nightmare notes
    - Thread history (last N Q&A)
  │
  ▼
[5] Synthesis (LLM, high effort, structured output, one tool-use round)
    Same per-stage provider choice as [1]. The tool round and the schema
    travel in each backend's wire format (strict function calling on
    OpenAI-compatible servers when the provider says it has it).
    Tool: lookup_rules(ids). The model may request additional CR sections
    once before answering, closing the classifier-miss gap.
    Output: Verdict { answer, confidence: Low|Medium|High, citations[], category }
    The model does not report `source`, `crVersion` or `cards`. Validation
    stamps them:
      - source from the extraction, as AnswerableSource (by type, only
        Cr | Commander reach this step)
      - crVersion from the retrieved chunks
      - the resolved cards (CardRef: id + name) from Context
    Every front door then shows "Cards: …" from the verdict alone.
    Each citation = typed reference + quoted span. Validation:
      - first, citations that quote nothing (blank, a stock word, under 4
        chars) are dropped (D21). An answer with nothing else is a
        MalformedCitation.
      (a) the reference exists in Context
      (b) the span is a substring of that chunk. Curly and ASCII
          punctuation compare equal (judge_core::quote). The chunk's own
          text is stored for the span, so a stored quote is exact.
      (c) the verdict cites something and the answer is ≥ 40 chars,
          else JudgeError.EmptyVerdict
      (d) every rule number in the answer text is covered by a rule
          citation, else UncitedRules (D20)
    Failure ⇒ BadCitation / MalformedCitation / EmptyVerdict / UncitedRules →
    retry once (the notice says which), then reply with error.
    Always quotes CURRENT Oracle text (errata note if the printed text differs).
  │
  ▼
[6] Persist call (question, verdict, context ids, crVersion); rating buttons.
```

The two model calls ([1] and [5]) and the vector leg of [4] reach their providers through
one seam (`docs/PROVIDERS.md`).

- `crates/llm` holds the provider-neutral request and response, the spend cap and the
  one-tool-round typestate. `ChatRequest` has system blocks, turns, tools, an output
  schema and an effort. `ChatResponse` has text, tool calls, a stop reason and usage.
  - The cap is a process total plus one *adjustment*. `judge_bot::budget` sets it from
    the `spend_days` ledger when `JUDGE_BUDGET_PERIOD` is `day` or `month`, which makes
    the cap a shared budget that survives restarts (D19). It also tells
    `JUDGE_ALERT_WEBHOOK` when the cap trips. `judge-cli stats` reads the same ledger.
- `crates/anthropic` and `crates/openai` are backends of it. Each owns its wire types and
  its schema-subset transform.
- The model's own previous turn is replayed as an opaque blob that only the backend that
  produced it reads (a thinking signature, a `reasoning_content`, a `tool_calls` array).
  The neutral layer never inspects it.
- The pipeline's port, `ChatModel`, is sealed and implemented only by `Metered`, so a
  model that bypasses the cap is unrepresentable.
- Each backend declares `Capabilities`. What a server cannot enforce (an output schema,
  strict tools) the adapter moves into the prompt, since decoding and citation validation
  are client-side either way.

`judge_bot::config` reads a `judge.toml` into that: one provider per stage, secrets by
environment-variable name, and a price for every model the cap must reserve for. For
embeddings it also reads the vector `Space` (provider kind, model, width). The database
records that space in `embedding_space`, and every vector reader and writer checks it
before touching a column. Two models' vectors are therefore never mixed, and switching is
one explicit, transactional `ingest reembed`. Nothing in `crates/core` knows any of this exists.

Three front doors share this pipeline through the same composition root
(`judge_bot::build_deps`):

- **Discord adapter** (`crates/bot`): `/judge` slash command, rating buttons,
  stateful "did you mean…?" buttons (pending store), thread history.
  - `/judge private:True` is `Audience::Private`: acknowledged ephemerally, no history
    read, never persisted, so no rating buttons. The audience rides in the pending entry
    through a card pick.
  - A per-user fixed window (`discord/cooldown.rs`, `JUDGE_USER_LIMIT`) is charged once a
    judge slot is held, so a "busy" is free. A card pick is not counted again.
  - `/card` and `/rule` are lookups over the resolver, the retriever's `lookup_rules` and
    `PgLibrary::rulings`. They call no model and touch no meter.
- **HTTP adapter** (`crates/api` + `web/`): anonymous `POST /api/judge` behind
  a per-IP fixed-window rate limit, and a SolidJS single page.
  - Each front door is opted into at launch (`crates/api/src/interfaces.rs`).
    `judge-api` alone serves the JSON route, `--web` adds the page and `--mcp`
    adds the MCP transport. A door nobody named is not mounted. The set is a
    `NonEmpty`, so "serving nothing" is unrepresentable.
  - `GET /api/health` and `GET /api/about` are served whatever doors are off.
    The container healthcheck needs the first.
  - `/api/about` is the source offer (`judge_core::source`): repository, built
    commit, licence and copyright. The AGPL requires every remote interface to
    offer it. The page's footer reads it from there. Discord gives the same in
    `/help` and `/license`, and the MCP server in its initialization
    instructions and an `about` tool. `JUDGE_SOURCE_URL` points all of them at
    a fork.
  - The same places name who runs the instance (`judge_core::operator`). The
    bot takes a `DiscordOperator` and the HTTP layer a `NetworkOperator`. The
    only way to make either is `Operator::for_discord` / `for_network`. So the
    bot cannot start without `JUDGE_OPERATOR_DISCORD`, and `judge-api` cannot
    start without `JUDGE_OPERATOR_EMAIL`, whichever doors it opens. A local
    `judge-cli` or stdio `judge-mcp` holds a plain `Operator` and needs neither.
  - There are no rating endpoints, because anonymous callers are not
    accountable identities.
  - Ambiguity is returned as data and resolved statelessly. The client re-asks
    with `pins: [{span, name}]`, which the server rewrites to `[[Full Name]]`
    with the same `pin_card` used by the Discord buttons.
  - Follow-up history comes from a client-generated session UUID, stored as
    thread id `web:<uuid>`.
- **Agent adapter** (`crates/agent`): the judge as a tool surface for *other*
  agents.
  - Over MCP, `judge-mcp` serves a local client on stdio. For a remote one,
    `judge-api --mcp` mounts the same handler at `/mcp` behind `MCP_TOKEN`. The
    flag without a token is refused at startup. The token without the flag
    serves nothing and warns.
  - `judge-cli` has one subcommand per operation with JSON out, for a shell
    agent (the repo's `.claude/skills/judge` skill).
  - It offers the pipeline two ways:
    - The `judge` tool runs it as above, with the built-in model calls. It is
      spend-capped, shares the web route's concurrency semaphore, and is
      offered only when a model is configured (`ANTHROPIC_API_KEY` or a
      `judge.toml`).
    - A **session** runs it in pull mode, where the calling agent *is* the
      model.
  - `judge_bot::session` is that state machine.
    1. `begin` returns the extraction prompt (steps 1 + 3 as text, plus the
       JSON Schema).
    2. The agent's `Extraction` JSON drives steps 2 + 4 and yields the
       synthesis prompt: the same system prompt, with `Harness`-specific
       wording for the one `lookup_rules` round and the output format, plus
       the rendered material.
    3. The agent's `Verdict` JSON goes through the same `Verdict::validate`
       against the session's own `Context`, with the same one retry and the
       same rejection notice.
  - On the API path, typestates carry the invariants (one tool round, one
    retry, only a validated verdict is persisted). Here they are a `Stage`
    enum, because the state lives in Postgres between calls (`agent_sessions`,
    one jsonb document, optimistic version). The session id is a
    server-minted handle passed as a tool argument, which is what the
    2026-07-28 MCP revision adopted in place of protocol sessions. The HTTP
    transport is served statelessly for every protocol version.
  - Sessions are unauthenticated at the tool level. Their thread ids are
    therefore a type (`AgentThread`, always `agent:<uuid>`) that cannot name a
    Discord or web thread, and their inputs are bounded (question, spans,
    concepts, lookup ids, answer length). A persisted call is keyed by session
    (`calls.session_id`, unique), so persisting twice cannot file two calls. It
    is excluded from the prior-call leg: nothing can rate it, and it is history
    for its own thread only.
  - Over HTTP, `judge` runs are also capped per window (`MCP_JUDGE_LIMIT`) so a
    leaked token cannot take the public page's slots and spend cap with it.
  - Read-only lookups (resolve a card, rules by id or search, rulings, notes,
    glossary) round the surface out (`PgLibrary`).

The Discord and web front doors draw Magic's card symbols (`{W}`, `{2/U}`, `{T}`) as
pictures, from one set of names:

- **Discord** substitutes *application* emoji (`<:mana_w:…>`). The bot owns them,
  not a server, so they work in every guild and cost no emoji slots.
  - `ingest emoji` uploads them: Scryfall's SVG → a 128 px PNG via resvg,
    scaled to fit and centred (nine of the 84 symbols are not square).
  - The emoji name is defined once, in `judge_core::symbol`, with no I/O. It is
    in core because the uploader and the renderer are separate programs that
    must agree on it exactly. A test pins the mapping as total and injective
    over everything Scryfall publishes, so no symbol can silently overwrite
    another's emoji.
  - A tag is ~28 characters where `{W}` is three, and Discord counts the tag.
    `mana::Rendered` keeps text as segments and lets only plain text be cut, so
    a half-written tag is unrepresentable rather than tested against.
  - An application with no emoji uploaded gets an empty table and the literal
    `{W}`, unchanged.
- **Web** renders Scryfall's SVGs inline from their CDN (`web/src/symbols.ts`
  is the generated table, `Symbols.tsx` the component).
  - A symbol the table does not know, or an image that fails to load, falls
    back to the literal text.
  - `split`/`lookup` mirror the Rust scanner, so both surfaces accept the same
    spellings (`{W/U}`, `{w/u}`, `{U/W}`, `{WU}`) and leave the same text alone.
  - Seven symbols (`{E} {P} {PW} {CHAOS} {TK} {L} {D}`) are flat black with no
    disc, invisible on the dark palette. They carry a `flat` flag and are
    inverted in dark mode. The coloured ones must not be.
  - If a Content-Security-Policy is ever added to `judge-api`, `img-src` must
    allow `https://svgs.scryfall.io`.

## 4. Data

| Source | Refresh | Storage |
|---|---|---|
| Scryfall bulk `oracle-cards.json` | nightly (`ingest refresh`, cron on the deploy host — DEPLOYMENT.md §7) | `cards` (oracle_id, name, layout, type_line, …) + `card_faces` (oracle_id, face_idx, name, oracle_text, mana_cost, …) |
| Scryfall bulk `default-cards.json` (names only) | nightly | `printed_names` (printed_name, oracle_id) — old names, errata'd names |
| Scryfall bulk `rulings.json` | nightly (bulk-loaded, keyed by oracle_id) | `rulings` (oracle_id, key, published_at, text) — `key` = content hash (`judge_core::ruling_key`), so a reindexed ruling keeps its identity |
| Comprehensive Rules txt | on CR release — detected nightly from the `.txt` link on Wizards' rules page vs `max(cr_version)` | `rules` (id, parent_id, subsection, heading, body, examples, embedding, cr_version) |
| CR Glossary | same | `glossary` (term, text, embedding) |
| Scryfall `/symbology` (84 card symbols) | nightly (idempotent, uploads only missing symbols) | not stored: uploaded as Discord application emoji (`ingest emoji`) and hard-coded for the web page (`web/src/symbols.ts`) |
| Nicknames | hand-curated YAML | `card_aliases` (alias, oracle_id) |
| Nightmare notes | hand-written markdown | `card_notes` (oracle_id, note) |
| Categories → subsections | YAML (single source of truth; the enum is generated from it) | `categories` |
| Calls | continuous; `retired_at`/`retired_reason` recomputed nightly from citation validity | `calls` (id, thread_id, question, answer, category, citations jsonb, source, cr_version, retired_at, retired_reason, embedding) |
| Ratings | continuous | `ratings` (call_id, user_id, score, is_judge, ts) |

Database: **Postgres 16 + pgvector + pg_trgm**. Scale: ~30k cards, ~2k rule
chunks, <10k calls.

Embeddings: **Voyage AI** by default (`voyage-3.5` or `voyage-4` family; `voyage-3` is
superseded). It was chosen over local models for the zero-config setup, because local
models are not worth it on WSL2. `Embedder` is an interface, so this can change:
`judge_embed` also has an OpenAI-compatible `/v1/embeddings` adapter for local or hosted
models, chosen by `[models.embed]` in `judge.toml`.

Every embedder carries its `Space` (provider kind, model, width). The one-row
`embedding_space` table records the space the stored vectors belong to, and
`ingest embed` refuses to write into another. On a mismatch the adapters' vector legs go
dark (error log, never mixed). The space is re-checked on every use, and anything that
writes a vector holds it under a shared advisory lock. `ingest reembed --yes` switches the
database in one transaction after probing the new embedder (`docs/PROVIDERS.md` §4.3).

Rating aggregation: Bayesian-smoothed mean (prior 2.0, weight 3). A rating with
`is_judge = true` (operator-assigned role) dominates crowd votes. It is used for
labeling and ordering *among* prior calls, and for exclusion (< 1.5 with ≥ 5
votes). It never ranks prior calls above CR chunks.

## 5. Domain model

The model is language-neutral.

```
CardId        = Scryfall oracle_id
RuleId        = "702.19" | "702.19b"
Face          { name, oracleText, manaCost, typeLine }
Card          { id, name, layout, faces: NonEmpty[Face] }
Category      closed set, ~25 values, sourced from categories.yaml
Source        CR | Commander | Tournament | OutOfScope
Confidence    Low | Medium | High
Resolution    Resolved(card, matchedVia) | Ambiguous(query, candidates) | NotFound(query)
Quote         non-blank text; compared to its source with typographic punctuation folded
Citation      Rule(id, quote) | ScryfallRuling(card, rulingKey, quote)
              | OracleText(card, quote) | PriorCall(id, quote)
Context       { cards, rules, rulings, glossary, prior, notes, history }
Verdict<S>    { answer, confidence, citations, category, source, crVersion, cards }
              S = Unvalidated | Validated; only Verdict<Validated> can be stored or shown
Rejection     BadCitation(citation) | Malformed(what) | Empty(why) | Uncited(rule ids)
              | Oversized { chars }
RejectedAttempt  { answer, rejection }   — quoted back to the retry as a blockquote
JudgeError    AmbiguousCards(...) | CardsNotFound(...) | OutOfScope(source)
              | BadCitation | MalformedCitation | EmptyVerdict | UncitedRules
              | LlmRefused | Upstream(err)

Ports:  Extractor, Resolver, Retriever, Synthesizer, Embedder, CallStore
judge : Question -> IO[Either[JudgeError, Verdict]]
```

## 6. Build order

The build was eval-first: retrieval was measured before any synthesis existed.

1. A gold set of adversarially verified questions with expected rule ids
   (`eval/gold.yaml`, 21 questions today).
2. Ingest.
3. Extraction and resolution, tested on the gold set's card mentions.
4. Retrieval, behind a **gate of ≥ 90% of gold rule ids present in the Context**.
5. Synthesis with citation validation, scored against the gold answers.
6. The Discord adapter with rating buttons.
7. The prior-call leg, which needs rated data to exist.

`judge-eval recall` still runs that gate for free on every retrieval change.

## 7. Non-goals

- Tournament policy (MTR/IPG). The bot declines those questions rather than winging them.
- Accounts or ratings on the web page. The anonymous page never rates.
- Retraining of any kind.
- Automatic detection of "nightmare" cards (the notes are curated by hand).
- Multi-server tenancy. The bot is meant to be run by each community for itself
  (`docs/DECISIONS.md` D16), so one process has one spend cap, one judge role and one
  token by design.
