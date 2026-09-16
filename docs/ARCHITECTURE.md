# MTG Judge Bot — Architecture

The design reference, kept current with the code. Written stack-independent before the
language was chosen (Rust, 2026-08-29) and maintained since. `docs/DECISIONS.md` records
why each load-bearing choice was made; `docs/EXPLAINER.md` is the narrative version for
someone new to the ideas; this file is the terse one.

## 1. Goal

A Discord bot that answers Magic: The Gathering rules questions. Users reference
cards by full name, partial name, or nickname ("bob" → Dark Confidant). Every
answer cites its sources: Comprehensive Rules (CR) sections, Scryfall rulings,
and/or prior rated calls. Answers and user-submitted calls are rated 1–3
(incorrect / partially correct / correct); ratings feed back into retrieval as
*examples*, never as authorities over the CR.

## 2. Design principle: entity-first hybrid retrieval

Rules questions have the shape "card A + card B + rule concept C":

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
    Splits the message into card-name spans vs. rules concepts. Runs FIRST so
    fuzzy matching only sees candidate spans, not rules vocabulary.
    Which model, and where: `judge_bot::config` (a `judge.toml`, else
    Anthropic direct from the environment) picks a provider per stage —
    Anthropic's Messages API (direct, through a proxy, or on a cloud
    account: Claude Platform on AWS and Bedrock with SigV4, Vertex AI with
    ADC — credentials from the platform chain, probed at startup) or any
    OpenAI-compatible chat completions server — and every model sits behind
    the one spend-capped `Metered` per process. The backend reports its
    `Capabilities`; when it cannot enforce the output schema server-side the
    adapter appends the schema to the *user turn* (the system prompt is
    pinned by digest and stays byte-identical on every backend).
  │
  ▼
[2] Card resolution (per span)
    A typed ladder, each rung tried only when the one above found nothing:
    alias → possessive-stripped alias ("bob's" → "bob") → [[bracket]] syntax →
    exact name → printed-name table → short name before the comma ("Ragavan")
    → alias as a suffix → trigram fuzzy
    Output: Resolution = Resolved(card, matchedVia) | Ambiguous(candidates) | NotFound
    A span that is Ambiguous from a non-fuzzy rung and shares a candidate with
    a card Resolved from another span, or NotFound but whose words appear as
    whole words in a resolved card's (face) name, is a duplicate reference
    (nickname + full name) and is dropped. Fuzzy-ambiguous spans are never
    dropped: trigram neighbours are not names the user could have meant.
    Remaining Ambiguous ⇒ "did you mean…?" buttons and stop. Never guess.
  │
  ▼
[3] Classification (same LLM call as [1])
    One required primary category plus up to 2 secondary, each with
    confidence, from a fixed taxonomy (~25 entries mirroring CR structure);
    the schema requires the primary, so an unclassified answer is an API error. Also assigns Source: CR | Commander | Tournament(MTR/IPG) | OutOfScope.
    Tournament/OutOfScope ⇒ reply "I don't cover that" with a pointer.
    Flags "nightmare" cards (Humility, Opalescence, Blood Moon, Mycosynth
    Lattice, Panglacial Wurm, …) → inject hand-written notes.
  │
  ▼
[4] Retrieval → Context
    - CR: category → curated subsection IDs (always) + tsvector BM25 + pgvector
          cosine; union, dedupe, expand to full rule chunk. Ordered by
          priority, because the synthesis budget (25 chunks / 30 kB) renders a
          prefix and the legs return several times that: the primary
          category's rules sharing a word with the question (ranked by text
          relevance, not by id), BM25, vector, the primary's remaining rules,
          secondary categories (ranked). `eval recall` gates on what that
          prefix shows (≥ 75%) as well as on what was retrieved (≥ 90%).
    - Scryfall rulings for each resolved card (all faces)
    - Glossary entries for terms in the oracle text
    - Prior calls: vector search filtered by (cards ∩ category), labeled with
      rating and CR version; retired calls excluded (a call is retired by the
      nightly pass when any citation's source no longer contains its quote or a
      context card's Oracle text changed, and restored when they hold again;
      renumbered rules carry their calls with them); shown as examples AFTER
      the CR material
    - Nightmare notes
    - Thread history (last N Q&A)
  │
  ▼
[5] Synthesis (LLM, high effort, structured output, one tool-use round)
    Same per-stage provider choice as [1]; the tool round and the schema
    travel in each backend's wire format (strict function calling on
    OpenAI-compatible servers when the provider says it has it).
    Tool: lookup_rules(ids) — the model may request additional CR sections
    once before answering, closing the classifier-miss gap.
    Output: Verdict { answer, confidence: Low|Medium|High, citations[], category }
    `source` and `crVersion` are not model-reported: validation stamps the
    source from the extraction (as AnswerableSource — only Cr | Commander
    reach this step, by type) and crVersion from the retrieved chunks.
    Each citation = typed reference + quoted span. Validation:
      (a) reference exists in Context, (b) span is a substring of that chunk,
      comparing curly/ASCII punctuation as equal (judge_core::quote) and
      storing the chunk's own text for the span, so a stored quote is exact.
    Also (c) every verdict must cite something and the answer must be
    ≥ 40 chars, else JudgeError.EmptyVerdict.
    Failure ⇒ BadCitation / EmptyVerdict → retry once (the notice says which),
    then reply with error.
    Always quotes CURRENT Oracle text (errata note if the printed text differs).
  │
  ▼
[6] Persist call (question, verdict, context ids, crVersion); rating buttons.
```

The two model calls ([1] and [5]) and the vector leg of [4] reach their providers through
one seam (`docs/PROVIDERS.md`). `crates/llm` holds the provider-neutral
request and response (`ChatRequest` with system blocks, turns, tools, an output schema
and an effort; `ChatResponse` with text, tool calls, a stop reason and usage), the spend
cap and the one-tool-round typestate; `crates/anthropic` and `crates/openai` are backends
of it, each owning its wire types and its schema-subset transform, and the model's own
previous turn is replayed as an opaque blob only the backend that produced it reads (a
thinking signature, a `reasoning_content`, a `tool_calls` array — never inspected in the
neutral layer). The pipeline's port, `ChatModel`, is sealed and implemented only by
`Metered`, so a model that bypasses the cap is unrepresentable; each backend declares
`Capabilities`, and what a server cannot enforce (an output schema, strict tools) the
adapter moves into the prompt, since decoding and citation validation are client-side
either way. `judge_bot::config` reads a `judge.toml` into that: one provider per stage,
secrets by environment-variable name, a price for every model the cap must reserve for,
and — for embeddings — the vector `Space` (provider kind, model, width) the database
records in `embedding_space` and every vector reader and writer checks before touching a
column, so two models' vectors are never mixed and switching is one explicit,
transactional `ingest reembed`. Nothing in `crates/core` knows any of this exists.

Three front doors share this pipeline through the same composition root
(`judge_bot::build_deps`):

- **Discord adapter** (`crates/bot`): `/judge` slash command, rating buttons,
  stateful "did you mean…?" buttons (pending store), thread history.
- **HTTP adapter** (`crates/api` + `web/`): anonymous `POST /api/judge` behind
  a per-IP fixed-window rate limit, and a SolidJS single page. Which of its
  front doors a process opens is a launch option, not a property of being
  started: `judge-api` alone is the JSON route, `--web` adds the page, `--mcp`
  the MCP transport, and an interface nobody named is not mounted
  (`crates/api/src/interfaces.rs`; the set is a `NonEmpty`, so "serving
  nothing" is unrepresentable). `GET /api/health` is outside the set, because
  the container healthcheck has to reach it whatever is switched off. No rating
  endpoints (anonymous callers are not accountable identities). Ambiguity is
  returned as data and resolved statelessly: the client re-asks with
  `pins: [{span, name}]`, which the server rewrites to `[[Full Name]]` with the
  same `pin_card` used by the Discord buttons. Follow-up history comes from a
  client-generated session UUID, stored as thread id `web:<uuid>`.
- **Agent adapter** (`crates/agent`): the judge as a tool surface for *other*
  agents, over MCP (`judge-mcp` on stdio for a local client; `judge-api --mcp`
  mounts the same handler at `/mcp` behind `MCP_TOKEN` for a remote one — the
  flag without a token is refused at startup, the token without the flag serves
  nothing and warns) and as
  `judge-cli` (one subcommand per operation, JSON out, for a shell agent — the
  repo's `.claude/skills/judge` skill). It offers the pipeline two ways: the
  `judge` tool runs it as above with the built-in model calls (spend-capped,
  same concurrency semaphore as the web route, only when a model is
  configured — `ANTHROPIC_API_KEY` or a `judge.toml`); a **session** runs it in
  pull mode, where the calling agent *is* the model. `judge_bot::session` is that state machine: `begin` returns the
  extraction prompt (steps 1 + 3 as text plus the JSON Schema), the agent's
  `Extraction` JSON drives steps 2 + 4 and yields the synthesis prompt (the same
  system prompt, with `Harness`-specific wording for the one `lookup_rules`
  round and the output format, plus the rendered material), the agent's
  `Verdict` JSON goes through the same `Verdict::validate` against the session's
  own `Context`, with the same single retry and the same rejection notice. The
  invariants the typestates carry on the API path (one tool round, one retry,
  only a validated verdict is persisted) are a `Stage` enum here, because the
  state lives in Postgres (`agent_sessions`, one jsonb document, optimistic
  version) between calls — the 2026-07-28 MCP revision removed protocol
  sessions in favour of server-minted handles passed as tool arguments, which
  is what the session id is (the HTTP transport is served statelessly for
  every protocol version). Sessions are unauthenticated at the tool level, so
  their thread ids are a type (`AgentThread`, always `agent:<uuid>`) that
  cannot name a Discord or web thread, their inputs are bounded
  (question, spans, concepts, lookup ids, answer length), and a persisted call
  is keyed by session (`calls.session_id`, unique) so persisting twice cannot
  file two calls, and is excluded from the prior-call leg (nothing can rate it;
  it is history for its own thread only). Over HTTP, `judge` runs are also
  capped per window (`MCP_JUDGE_LIMIT`) so a leaked token cannot take the
  public page's slots and spend cap with it. Read-only lookups (resolve a card, rules by id or search,
  rulings, notes, glossary) round the surface out (`PgLibrary`).

The Discord and web front doors draw Magic's card symbols (`{W}`, `{2/U}`, `{T}`) as pictures,
from one set of names:

- **Discord** substitutes *application* emoji — `<:mana_w:…>`, owned by the bot
  rather than by a server, so they work in every guild and cost no emoji slots.
  `ingest emoji` uploads them (Scryfall's SVG → a 128 px PNG via resvg, scaled
  to fit and centred: nine of the 84 symbols are not square). The name is
  defined once, in `judge_core::symbol` — pure, no I/O, and in core rather than
  in either binary because the uploader and the renderer are separate programs
  that must agree on it exactly. A test there pins the mapping as total and
  injective over everything Scryfall publishes, so no symbol can silently
  overwrite another's emoji. A tag is ~28 characters
  where `{W}` is three and Discord counts the tag, so `mana::Rendered` keeps
  text as segments and only lets plain text be cut: a half-written tag is
  unrepresentable rather than merely tested against. An application with no
  emoji uploaded gets an empty table and the literal `{W}`, unchanged.
- **Web** renders Scryfall's SVGs inline from their CDN (`web/src/symbols.ts`
  is the generated table; `Symbols.tsx` the component). A symbol the table does
  not know, or an image that fails to load, falls back to the literal text.
  `split`/`lookup` mirror the Rust scanner, so both surfaces accept the same
  spellings (`{W/U}`, `{w/u}`, `{U/W}`, `{WU}`) and leave the same text alone.
  Seven symbols (`{E} {P} {PW} {CHAOS} {TK} {L} {D}`) are flat black with no
  disc and are invisible on the dark palette, so they carry a `flat` flag and
  are inverted in dark mode; the coloured ones must not be. If a
  Content-Security-Policy is ever added to `judge-api`, `img-src` must allow
  `https://svgs.scryfall.io`.

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
| Categories → subsections | YAML (single source of truth; enum generated or validated from it) | `categories` |
| Calls | continuous; `retired_at`/`retired_reason` recomputed nightly from citation validity | `calls` (id, thread_id, question, answer, category, citations jsonb, source, cr_version, retired_at, retired_reason, embedding) |
| Ratings | continuous | `ratings` (call_id, user_id, score, is_judge, ts) |

Database: **Postgres 16 + pgvector + pg_trgm**. Scale: ~30k cards, ~2k rule
chunks, <10k calls.

Embeddings: **Voyage AI** by default (`voyage-3.5` or `voyage-4` family — `voyage-3` is superseded; chosen over local models for the zero-config setup, which are not worth it on WSL2; a local or OpenAI-compatible embedder is a `judge.toml` choice).
`Embedder` is an interface so this can change: `judge_embed` also has an OpenAI-compatible
`/v1/embeddings` adapter, chosen by `[models.embed]` in `judge.toml`. Every embedder carries its
`Space` (provider kind, model, width); the one-row `embedding_space` table records the space the
stored vectors belong to, `ingest embed` refuses to write into another, the adapters' vector legs go
dark (error log, never mixed) on a mismatch — re-checked on every use, and held under a shared
advisory lock by anything that writes a vector — and `ingest reembed --yes` switches the database in
one transaction after probing the new embedder (`docs/PROVIDERS.md` §4.3).

Rating aggregation: Bayesian-smoothed mean (prior 2.0, weight 3). A rating with
`is_judge = true` (operator-assigned role) dominates crowd votes. Used for
labeling and ordering *among* prior calls, and for exclusion (< 1.5 with ≥ 5
votes) — never for ranking prior calls above CR chunks.

## 5. Domain model (language-neutral)

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
Verdict<S>    { answer, confidence, citations, category, source, crVersion }
              S = Unvalidated | Validated; only Verdict<Validated> can be stored or shown
Rejection     BadCitation(citation) | Malformed(what) | Empty(why) | Oversized { chars }
RejectedAttempt  { answer, rejection }   — quoted back to the retry as a blockquote
JudgeError    AmbiguousCards(...) | CardsNotFound(...) | OutOfScope(source)
              | BadCitation | MalformedCitation | EmptyVerdict | LlmRefused | Upstream(err)

Ports:  Extractor, Resolver, Retriever, Synthesizer, Embedder, CallStore
judge : Question -> IO[Either[JudgeError, Verdict]]
```

## 6. How it was built (eval-first)

The order was chosen so that retrieval was measured before any synthesis existed:
a gold set of adversarially verified questions with expected rule ids (`eval/gold.yaml`,
21 questions today), then ingest, then extraction and resolution tested on the gold set's
card mentions, then retrieval behind a **gate of ≥ 90% of gold rule ids present in the
Context**, then synthesis with citation validation scored against the gold answers, then
the Discord adapter with rating buttons, and last the prior-call leg, which needs rated
data to exist. `judge-eval recall` still runs that gate for free on every retrieval change.

## 7. Non-goals

Tournament policy (MTR/IPG): the bot declines those questions rather than winging them.
Accounts or ratings on the web page: the anonymous page never rates. Retraining of any
kind. Automatic detection of "nightmare" cards (the notes are curated by hand).
Multi-server tenancy: the bot is meant to be run by each community for itself
(`docs/DECISIONS.md` D16), so one process has one spend cap, one judge role and one
token by design.
