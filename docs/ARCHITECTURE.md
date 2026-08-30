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
      rating and CR version; stale (pre-current-CR) calls excluded unless
      re-verified; shown as examples AFTER the CR material
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

## 4. Data

| Source | Refresh | Storage |
|---|---|---|
| Scryfall bulk `oracle-cards.json` | weekly | `cards` (oracle_id, name, layout, type_line, …) + `card_faces` (oracle_id, face_idx, name, oracle_text, mana_cost, …) |
| Scryfall bulk `default-cards.json` (names only) | weekly | `printed_names` (printed_name, oracle_id) — old names, errata'd names |
| Scryfall bulk `rulings.json` | weekly (bulk-loaded, keyed by oracle_id) | `rulings` (oracle_id, idx, published_at, text) |
| Comprehensive Rules txt | on CR release | `rules` (id, parent_id, subsection, heading, body, examples, embedding, cr_version) |
| CR Glossary | same | `glossary` (term, text, embedding) |
| Nicknames | hand-curated YAML | `card_aliases` (alias, oracle_id) |
| Nightmare notes | hand-written markdown | `card_notes` (oracle_id, note) |
| Categories → subsections | YAML (single source of truth; enum generated or validated from it) | `categories` |
| Calls | continuous | `calls` (id, thread_id, question, answer, category, citations jsonb, source, cr_version, embedding) |
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
Citation      Rule(id, quote) | ScryfallRuling(card, idx, quote) | PriorCall(id, quote)
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

Tournament policy (MTR/IPG), multi-server tenancy, web UI, retraining of any
kind, automatic nightmare-card detection.
