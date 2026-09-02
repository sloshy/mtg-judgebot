# MTG Judge Bot — Architecture (stack-independent)

Status: draft v2, revised after review. **Implementation language: Rust** (decided 2026-08-29). Language comparison in
`docs/LANGUAGE_EVALUATION.md`; stack-specific proposals in `docs/proposals/`. Target: small Discord server, single operator, prototype.

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
  │
  ▼
[2] Card resolution (per span)
    alias table → [[bracket]] syntax → printed-name table → trigram fuzzy
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
          cosine; union, dedupe, expand to full rule chunk
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
    Tool: lookup_rules(ids) — the model may request additional CR sections
    once before answering, closing the classifier-miss gap.
    Output: Verdict { answer, confidence: Low|Medium|High, citations[], category }
    `source` and `crVersion` are not model-reported: validation stamps the
    source from the extraction (as AnswerableSource — only Cr | Commander
    reach this step, by type) and crVersion from the retrieved chunks.
    Each citation = typed reference + quoted span. Validation:
      (a) reference exists in Context, (b) span is a substring of that chunk.
    Also (c) every verdict must cite something and the answer must be
    ≥ 40 chars, else JudgeError.EmptyVerdict.
    Failure ⇒ BadCitation / EmptyVerdict → retry once (the notice says which),
    then reply with error.
    Always quotes CURRENT Oracle text (errata note if the printed text differs).
  │
  ▼
[6] Persist call (question, verdict, context ids, crVersion); rating buttons.
```

Two front doors share this pipeline through the same composition root
(`judge_bot::build_deps`):

- **Discord adapter** (`crates/bot`): `/judge` slash command, rating buttons,
  stateful "did you mean…?" buttons (pending store), thread history.
- **HTTP adapter** (`crates/api` + `web/`): anonymous `POST /api/judge` behind
  a per-IP fixed-window rate limit, serving a SolidJS single page. No rating
  endpoints (anonymous callers are not accountable identities). Ambiguity is
  returned as data and resolved statelessly: the client re-asks with
  `pins: [{span, name}]`, which the server rewrites to `[[Full Name]]` with the
  same `pin_card` used by the Discord buttons. Follow-up history comes from a
  client-generated session UUID, stored as thread id `web:<uuid>`.

Both front doors draw Magic's card symbols (`{W}`, `{2/U}`, `{T}`) as pictures,
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

Embeddings: **Voyage AI** (`voyage-3.5` or `voyage-4` family — `voyage-3` is superseded; decided over local models, which are not worth it on WSL2).
`Embedder` is an interface so this can change.

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
Citation      Rule(id, quote) | ScryfallRuling(card, rulingKey, quote) | PriorCall(id, quote)
Context       { cards, rules, rulings, glossary, prior, notes, history }
Verdict       { answer, confidence, citations, category, source, crVersion }
JudgeError    Ambiguous(...) | OutOfScope(source) | BadCitation(citation) | LlmRefused | Upstream(err)

Ports:  Extractor, Resolver, Retriever, Synthesizer, Embedder, CallStore
judge : Question -> IO[Either[JudgeError, Verdict]]
```

## 6. Build order (eval-first)

1. **Gold set v0**: 20 questions with expected rule IDs and answers, covering
   DFC/MDFC, adventure/split, Commander, layers, replacement effects, errata,
   out-of-scope (MTR), and nicknames.
2. Ingest: cards + faces + printed names + CR chunks + embeddings.
3. Extraction + resolution; unit tests on the gold set's card mentions.
4. Retrieval; **gate: ≥ 90% of gold rule IDs present in Context** before any
   synthesis work.
5. Synthesis + citation validation, CLI-only; score against gold.
6. Discord adapter, threads, rating buttons.
7. Prior-call retrieval (needs data from 6). Extend gold set to ≥ 50.

## 7. Non-goals for the prototype

Tournament policy (MTR/IPG), multi-server tenancy, accounts/ratings on the web
UI (the anonymous page never rates), retraining of any kind, automatic
nightmare-card detection.
