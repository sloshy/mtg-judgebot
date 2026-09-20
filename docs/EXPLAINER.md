# MTG Judge Bot explainer

An explainer for a programmer who is comfortable with web services and SQL but new to
language models in production, embeddings, and vector search. It describes what the
application does and walks one question through it. It then explains each technique, lists
what goes wrong in a system like this and how this one handles it, and closes with the
technology choices and the reasons behind them.

`docs/ARCHITECTURE.md` is the terse, kept-current reference. This document is the tour.
Where the two disagree, the code wins, then ARCHITECTURE.md.

---

## 1. What it does

A user in a Discord server (or on a small web page, or an AI agent over MCP) asks a
Magic: The Gathering rules question:

> Does bob's trigger still happen if he dies in response to it?

The bot answers like a judge would: ruling first, then the reasoning, with rule numbers.
Every answer carries **citations**. Each citation is a short verbatim quote from one of
four kinds of source:

- a numbered rule in the **Comprehensive Rules** (the CR, a large text file Wizards publishes),
- a **Scryfall ruling** (official card-specific clarifications),
- a card's current **Oracle text** (its official wording, which changes through errata),
- a **prior call** the bot itself made earlier that users rated well.

Discord users rate each answer 1 to 3. Ratings never make the bot "learn" in the model
sense. They decide which old answers the model sees later as examples, and nothing else.

The system is one Postgres database, a handful of Rust binaries, two paid APIs
(a chat model for reasoning, an embedding model for search), and a nightly refresh job.

---

## 2. Limits of asking the model directly

A large language model already "knows" a lot about Magic. Asking it directly fails in
three ways that matter for a judge bot:

1. **It is confidently wrong.** Rule numbers get invented, old Oracle text gets quoted as
   current, and a plausible paragraph looks the same as a correct one.
2. **Its knowledge is frozen.** New sets, keywords and errata arrive monthly. The CR is
   renumbered a few times a year.
3. **Nobody can check it.** An answer without a pointer into the rules text cannot be
   verified by the asker or by a human judge.

The standard remedy is **retrieval-augmented generation** (RAG). Before asking the model,
look up the relevant source material yourself and hand it to the model. Tell the model to
answer *only* from that material. This bot is a RAG system with two additions that most
RAG systems skip:

- **Citation validation.** The model must quote its sources, and the program checks that
  every quote is a substring of the source it names. A failed check rejects the answer.
  This turns "the model was told to cite" into "the answer is grounded".
- **Entity resolution before search.** Card names are looked up in a table, not searched for
  semantically. "bob" must become one card, *Dark Confidant*, or the user must be asked.

Most of this document is about how those two ideas play out.

---

## 3. One question, end to end

Take the question above. The steps below happen in order. The pipeline lives in
`crates/core/src/judge.rs`. Each step is a "port" (a trait) that core defines and an adapter
in `crates/bot` implements.

### Step 1: Extraction and classification

This step is one cheap model call. The bot sends the question, plus the last five Q&A pairs
in the same thread, to the chat model with a small system prompt and a **JSON schema** for
the reply. The model returns:

```json
{
  "card_spans": ["bob"],
  "concepts": ["dies in response to trigger", "leaves-the-battlefield trigger"],
  "primary": {"category": "triggered_abilities", "confidence": "high"},
  "secondary": [{"category": "zones", "confidence": "low"}],
  "source": "cr"
}
```

Three things happen in this call:

- **Card spans** are cut out of the sentence. Fuzzy name matching later sees `bob` alone. It
  never sees "trigger" or "response", which would otherwise fuzzy-match real cards.
- The question is **classified** into a fixed taxonomy of 25 categories that mirror the
  CR's structure (`data/categories.yaml`). `primary` is required. Up to two `secondary`
  guesses are kept. The categories drive the first retrieval leg.
- The **source** says whether this is a rules question (`cr`), a Commander-format question,
  tournament policy, or off-topic. The last two stop here with a polite decline.

"Structured output" means the API constrains the model to emit JSON matching a schema.
The schema is generated from the Rust struct the reply is decoded into, so the two cannot
drift apart.

### Step 2: Card resolution

This step uses SQL and no model. Each span goes down a ladder of increasingly loose
lookups and stops at the first rung that answers:

1. hand-curated alias table (`bob` → Dark Confidant),
2. the same after stripping a possessive (`bob's`), also retrying the exact and short-name rungs,
3. exact current name (also matches a single face of a two-faced card),
4. every name the card has ever been printed under (old names, errata'd names),
5. the part of a name before the comma (`Jace` → several Jaces → ambiguous),
6. a nickname preceded only by printing words (`foil bob`),
7. trigram fuzzy match, for typos.

A span written in brackets, `[[Full Card Name]]`, skips that ladder. The brackets say "this
exact name", so it is tried only against current and printed names. Anything else is
offered, never resolved. `[[bolt]]` asks "did you mean Lightning Bolt?", and asks nothing
when the extractor already named Lightning Bolt from the same question. A near miss like
`[[Dark Confidnt]]` offers the closest spellings. Answers name the cards they resolved to
("Cards: …"), so the reader can see what a nickname was taken to mean.

The important property: **it never guesses.** If two or more cards remain, the result is
`Ambiguous` and Discord shows "Did you mean…?" buttons. If nothing matches, the result is
`NotFound`. The type system forces every consumer to handle all three outcomes.

Fuzzy matching uses Postgres's `pg_trgm` extension. A trigram is a three-letter window.
"bolt" is `{" b","bo","ol","lt","t "}`. Two strings are similar when they share many
trigrams, which tolerates typos without any model. The fuzzy rung accepts a candidate in
two cases: it is alone with a strong score (0.7 or more), or it leads the runner-up by a
clear margin (0.15).

### Step 3: Retrieval

Now the bot assembles the "material": everything the model will be allowed to read. It runs
seven queries concurrently and unions the results into a `Context`:

- **CR rules**, from three "legs" (explained in §5): the curated subsections for the
  categories, a full-text keyword search, and a vector similarity search.
- **Scryfall rulings** for every face of every resolved card.
- **Glossary** entries whose term appears in the cards' Oracle text.
- **Nightmare-card notes**: hand-written explanations for cards like Humility that break
  everyone's intuition.
- **Prior calls**: up to five earlier well-rated answers in the same categories about one of
  the same cards (any card, if none resolved).

The thread history is added to the context as well, so "what if it also had flying?" makes
sense.

### Step 4: Synthesis

This step is one expensive model call with one optional tool round. The context is rendered
into a long user turn under a character budget:

- 25 rule chunks,
- 30,000 characters of rules text,
- 20 rulings per card,
- 4 history pairs, with each earlier answer cut to 600 characters.

It is sent with the judge system prompt in `crates/bot/src/prompts/synth_system.md`. That
file is worth reading, because it is the contract between program and model.

The model may make **at most one** `lookup_rules` tool call to fetch rules the retrieval
missed ("I need 603.10"). Then it must answer with a JSON verdict:

```json
{
  "answer": "Yes. Dark Confidant's trigger ...",
  "confidence": "high",
  "category": "triggered_abilities",
  "citations": [
    {"kind": "rule", "id": "603.10", "quote": "Normally, objects that exist immediately after an event are checked..."},
    {"kind": "oracle_text", "card": "9f2c...", "face": 0, "quote": "At the beginning of your upkeep, reveal the top card"}
  ]
}
```

The other two citation kinds are `scryfall_ruling` (`card`, `ruling`, `quote`) and
`prior_call` (`id`, `quote`).

### Step 5: Validation

For every citation the program checks two things. The id must name something that was in
the context, and the quote must be a substring of that source's text. If anything fails, or
the verdict has no citations, the model gets **one retry** with the rejection rendered
into the prompt ("your quote `...` was not found in 603.10"). A second failure is an error
reply.

The validated verdict is a different Rust type from the raw one. Only the validated type
can be saved or sent to Discord (§6).

### Step 6: Persist and reply

The call is stored with its question, answer, citations, the ids of everything in its
context, and the CR version it was answered under. Discord gets the answer with rule numbers
linked to a CR mirror, card symbols drawn as emoji, and 1/2/3 rating buttons.

---

## 4. The data and where it comes from

Everything lives in one Postgres 16 database with two extensions: `pgvector` (vector
columns and indexes) and `pg_trgm` (trigram similarity). Migrations are in
`crates/bot/migrations/`.

| Table | Source | Refreshed | Notes |
|---|---|---|---|
| `cards`, `card_faces` | Scryfall bulk `oracle_cards` | nightly | one row per Oracle identity, and faces hold the Oracle text |
| `printed_names` | Scryfall bulk `default_cards` | nightly | every name ever printed, for old or errata'd names |
| `rulings` | Scryfall bulk `rulings` | nightly | keyed by a hash of the content, so a re-import is the same ruling |
| `rules`, `glossary` | the CR `.txt` from Wizards | on release, detected nightly | see chunking below |
| `card_aliases` | `data/aliases.yaml` | when edited | nicknames |
| `card_notes` | `data/notes.yaml` | when edited | nightmare cards |
| `categories` | `data/categories.yaml`, via the compiled enum | with each CR load | category → CR subsections |
| `calls`, `ratings` | the bot | continuous | answers and their votes |
| `embedding_space` | `ingest embed` | on switch | one row: which embedding model the vectors came from |
| `agent_sessions` | the agent surface | continuous | state for the step-by-step agent mode |

Scale is small: ~30k cards, ~2k rule chunks, well under 10k calls. This matters for
technology choices. Nothing here needs a dedicated vector database.

### How the CR is chunked

A text file is useless to search until it is cut into pieces. The cut is the most
consequential decision in any RAG system, because a piece is what gets found, what gets
shown, and what gets cited.

The CR is a numbered hierarchy: section `702` (Keyword Abilities) → rule `702.19`
(Trample) → sub-rules `702.19a`, `702.19b`. The parser emits rows at **two granularities**:

- **Rule-level rows** (`702.19`): the body is the rule's own sentence plus every lettered
  sub-rule and every `Example:` paragraph beneath it. These are the search unit. They get
  embeddings and appear in retrieval results. A rule plus its sub-rules is usually one
  coherent idea of a few hundred words, the right size for a model to read in one piece.
- **Leaf rows** (`702.19b`): one line each, with `parent_id = 702.19`. These are the
  citation unit. A model that quotes sub-rule b should cite `702.19b`, not the rule above it.

Scoring and lookups treat a leaf and its parent as covering each other. No rows exist for
three-digit sections. Asking for `702` expands to every rule in it.

---

## 5. Retrieval

This section holds most of the vector-database material. The bot combines three search
techniques because each fails differently.

### Leg A: category map

This leg is structured and always on. The classifier put the question in
`triggered_abilities`. The YAML says that category maps to CR sections 603 and 113.3. Every
rule in those sections goes into the context.

This is dumb and reliable. It costs nothing and never misses when the classifier is right.
It gives the model the surrounding rules it needs even when the "obvious" rule alone is not
enough. It fails when the classifier is wrong or when the answer lives in a section nobody
would file the question under.

### Leg B: full-text search

This leg matches keywords. Postgres has a built-in full-text engine. Each rule row has a
generated `tsvector` column: the text tokenised, lower-cased, stemmed ("triggers" →
"trigger") and stop-words removed. The query is turned into the same lexemes, OR-ed
together. Rows are ranked with `ts_rank_cd`, a relevance score in the same family as BM25
(frequency of matching terms, weighted by how rare they are, discounted by document
length). Concept phrases from the extraction count double against the raw question. The
top 12 rows are taken.

This finds rules that share **vocabulary** with the question: "leaves the battlefield",
"in response", "upkeep". It is exact, cheap, and needs no external service. It fails when
the user and the CR use different words for the same idea ("dies" versus "is put into a
graveyard from the battlefield"). Common words also distract it.

### Leg C: vector similarity

This leg matches meaning. It is the piece most people are new to, so it gets the most
detail.

**Embeddings.** An embedding model is a neural network that turns a piece of text into a
list of numbers, a vector, typically 512 to 3072 floats long. This bot uses Voyage AI's
`voyage-3.5` at 1024 dimensions by default. The model is trained so that texts with similar
*meaning* land near each other in that space, whatever words they use. "Dies in response
to its trigger" and "an ability that triggers on leaving the battlefield resolves even if
the source is gone" should be neighbours even though they share almost no words.

**Similarity.** Two vectors are compared by **cosine similarity**: the cosine of the angle
between them, 1.0 for identical direction, 0 for unrelated. pgvector exposes this as the
`<=>` operator (cosine *distance*, 1 minus similarity, so smaller is closer). The leg is
essentially:

```sql
SELECT ... FROM rules
WHERE parent_id IS NULL AND embedding IS NOT NULL
ORDER BY embedding <=> $question_vector
LIMIT 12
```

(The real query also restricts `id` to the `NNN.N` rule pattern.)

**Indexing.** Comparing the question against 2,000 rule vectors by brute force would be
fine at this scale. pgvector also provides an **HNSW** index (Hierarchical Navigable
Small World), a graph structure that finds approximate nearest neighbours quickly. It is
approximate, so it can occasionally miss the true nearest row. That is acceptable here
because the union with the other two legs covers for it. Because the index is *partial*,
over rule-level rows only (`WHERE parent_id IS NULL`), every candidate it yields is usable.
A post-filter cannot shrink the result below the limit.

**Query versus document.** Voyage's API takes an `input_type` of `document` or `query`.
The model embeds a short question differently from a long passage so that the two match
up better. The ingest job embeds rules as documents. The retriever embeds the user's
question as a query.

**Cost and storage.** Embedding is paid per token, once per rule, at ingest time. A new CR
release re-embeds only the rules whose text changed. The loader nulls those embeddings and
the nightly `ingest embed` fills them. Each question costs one small embedding call.

The vector leg's failures are instructive too. It is fuzzy by design, so it returns rules
that are *about* the same theme without being the one that decides the question. It cannot
tell `702.19` from `702.20` if their wording is similar. It also has a class of operational
problems that get their own section (§7).

### Leg order

The legs are unioned in priority order and deduplicated by rule id: category map first,
then full-text, then vector. When the budget cuts, chunks are kept in that order, so the
structured leg survives and the fuzziest leg is trimmed first. The retrieval gate in the
eval suite (`judge-eval recall`) requires that at least 90% of the gold set's expected rule
ids appear in the context.

The safety net for whatever all three miss is the one `lookup_rules` tool round in
synthesis. Having read the material, the model can ask for rules by number once.

### Prior calls

The prior-call query is the feedback loop. Earlier answers are stored with their own
embedding (of the question). The leg picks calls in the same categories, about at least one
of the same cards, not retired and not down-voted. It orders them by vector distance to the
new question. The rating is a **Bayesian-smoothed mean**: `(2.0 × 3 + Σ scores) / (3 + n)`.
That is "pretend there were three votes of 2.0 before anyone voted". One 3 does not make a
call look perfect, and one 1 does not bury it. The latest rating from someone with the
Judge role overrides the crowd. Calls scoring under 1.5 with five or more votes are
excluded.

Prior calls are rendered *after* all CR material, and the prompt says they never outrank
it. They are examples of how a question was answered, not authorities.

---

## 6. Synthesis guardrails

### Structured output and one tool round

The model does not write free text for the program to parse. It fills a JSON schema derived
from the `Verdict` struct. The `lookup_rules` tool round is bounded to one by a
**typestate**:

- `Synth<Fresh>` can send and become `Synth<ToolRequested>`.
- `Synth<ToolRequested>` can fulfil the request and become `Synth<Final>`.
- `Synth<Final>` has no method that requests tools.

A runaway loop is not a bug for a test to catch. It is code that does not compile.

### Citation validation

The validator (`crates/core/src/verdict.rs`) checks each citation against the context:

- The **reference must exist**: the rule id was shown or fetched, the ruling key was
  rendered under that card, the prior-call id was in the list, the Oracle face exists.
- The **quote must be a contiguous substring** of that source's text.

The most common rejection in practice was punctuation. The CR is typeset with curly
apostrophes (`doesn’t`) and em dashes. Models reliably retype them as ASCII (`doesn't`)
even when told not to. Rejecting a correct citation over one character wastes the only
retry. So the comparison (`crates/core/src/quote.rs`) canonicalises each character, one
char to one char: curly to straight, every dash to a hyphen, non-breaking space to space.
It then stores the **source's** span, not the model's. The leniency applies at match time
only. What lands in the database is byte-exact, so later strict checks stay strict. Case,
word order and line breaks must still match, so a paraphrase is still rejected.

A verdict with no citations, or an answer under 40 characters, is rejected as empty. Two
fields the model might be tempted to lie about, `source` and `cr_version`, are not in the
model's schema. The program stamps them from the extraction and the retrieved chunks.

### The validated type

`Verdict<Unvalidated>` is what JSON decodes into. `Verdict<Validated>` is the only type
`CallStore::persist` and the Discord renderer accept, and the only way to make one is
`validate()`. Serde's `Deserialize` is implemented for the unvalidated state only, so
decoding straight into the validated one does not compile. Deleting the check is a type
error, not a silent regression.

### The retry

There is one retry, with a "Previous attempt rejected" notice showing the failing citation
and why. The retry starts with the tool disabled, so the model cannot spend another round.
Any rules the first attempt fetched are rendered regardless of budget. The first rejection
is logged at INFO so that when the second attempt also fails, the operator can read both.

---

## 7. Failure catalogue

The tables below list the failure classes this kind of system has. Each row says where the
handling lives, so you can read further.

### Model behaviour

| Problem | Handling |
|---|---|
| Model invents rule numbers or misquotes | citation validation, and the source's own span is stored (`core/verdict.rs`, `core/quote.rs`) |
| Model answers from memory instead of the material | system prompt ground rule 1, required citations, retry notice |
| Model pads with placeholder citations | prompt forbids stubs, and a malformed citation is a typed rejection with the parse error shown back; a quote that parses but is a stock word (`"placeholder"`) or a character or two is rejected the same way |
| Model files a card's Oracle text as a ruling (the card has no rulings to cite) | still rejected, never relabelled; the retry notice names the kind it meant (`oracle_text`, with the card and face) instead of telling it to drop a good quote |
| Model calls the tool repeatedly | `Synth` typestate: one round, by type |
| Model's output is cut off at `max_tokens` | detected from the stop reason, retried once at medium effort |
| Model claims a question is out of scope to dodge citing | `source` is stamped from extraction, not model-reported |
| Model is asked about tournament policy | classifier routes `Tournament`/`OutOfScope` to a decline before any synthesis spend |

### Card names

| Problem | Handling |
|---|---|
| Nicknames ("bob", "goyf") | curated alias table, possessive stripping |
| Old or errata'd names | `printed_names` from every printing |
| Typos | trigram fuzzy with a margin rule |
| Two cards could be meant | `Resolution::Ambiguous` → "did you mean?" buttons, never guessed |
| User wrote both nickname and full name | duplicate-span detection in `core/judge.rs` |
| Rules vocabulary mistaken for a card ("trample") | extraction runs first, so fuzzy sees only card spans |
| Card text has changed since an answer was stored | Oracle fingerprints on each call, and the retirement pass (`db/retire.rs`) |

### Retrieval

| Problem | Handling |
|---|---|
| Right rule uses different words than the question | vector leg |
| Vector leg returns thematically near but wrong rules | union with the exact legs, and the model can `lookup_rules` |
| Classifier picks the wrong category | full-text and vector legs, `lookup_rules` |
| Too much material for the prompt | `Budget` in `bot/synth.rs`, where the structured leg survives cuts |
| Model cites a sub-rule shown only inside its parent | the synthesizer hydrates the leaf row so validation finds it |
| CR renumbered, so stored calls cite stale ids | `renumber_map` (ingest, §8) |
| A cited rule was reworded or deleted | retirement pass marks the call retired, and restores it if the text returns |

### Vectors

| Problem | Handling |
|---|---|
| Vectors from two embedding models in one column (silently wrong results) | `embedding_space` table names the model, and readers and writers check it on every use (`db/space.rs`) |
| Embedding model switched while the bot runs | vector legs go dark with an error log rather than mixing spaces |
| Switching models is expensive (re-pays every row) | `ingest reembed --yes` is explicit, probes the new embedder first, and prints a rough cost without `--yes` |
| A switch races an in-flight write | advisory lock: writers take the shared side, the switch takes the exclusive side |
| No embedding key configured | vector leg off, and the other legs still work |
| Voyage free-tier token limits | batch size knob `VOYAGE_MAX_BATCH` |
| HNSW post-filtering shrinking results | partial index over the rows that are searched and no others |

### Money and abuse

| Problem | Handling |
|---|---|
| Runaway API spend | `Metered` spend cap by reservation (§9), the only `ChatModel` there is |
| Many concurrent requests | a semaphore (`JUDGE_CONCURRENCY`) shared by web and MCP |
| Anonymous web abuse | per-IP fixed-window rate limit, bucketed on an address the caller cannot forge |
| Leaked MCP token | separate per-window limit on `judge` runs through `/mcp` |
| Agent sessions reading Discord history | `AgentThread` ids are a type that can only be `agent:<uuid>` |
| Oversized agent inputs | bounded question, span, id and answer lengths |

### Operations

| Problem | Handling |
|---|---|
| New CR release | nightly scrape of Wizards' page, version compared before download |
| Scryfall data drift | nightly bulk re-sync, with content-hashed ruling keys that keep identity |
| Prompt or schema drift breaking the wire format | golden request fixtures pinned byte-for-byte, and the system prompt SHA pinned |
| Losing the database (re-embedding costs money) | weekly `pg_dump` to R2 with a restore drill |
| Unknown config keys silently ignored | `deny_unknown_fields` and "this knob would be ignored" errors at load |

---

## 8. Feedback loop and stale answers

Stored answers are an asset (examples for future questions) and a liability (they go
stale). Two mechanisms keep them current without a human curator.

**Retirement.** Every call's citations are its declared dependencies on the world. The
nightly pass re-runs the same substring check that admitted each citation, against today's
rules, rulings and Oracle text. If any check fails, the call is retired and leaves the
prior-call leg. If the text comes back, the call is restored. Each call also stores a
fingerprint of the Oracle text of every card in its context, so an erratum retires calls
*about* a card even when they cited only the CR. This replaced an earlier rule that retired
every call on every CR release. That rule threw away many still-correct answers and kept
wrong ones after an erratum.

**Renumbering.** When Wizards inserts a keyword at `702.20`, every later rule shifts by one.
Their bodies change too, because cross-references shift with them. Comparing the raw text fails on
the very release it is meant to see through. The CR loader masks every rule id out of every
body and matches old and new rules on the masked text where it is unique on both sides. It
then rewrites each old rule with the full map and keeps only mappings that reproduce the
new rule exactly. This consistency check rejects a cross-reference that was redirected
rather than renumbered. Matched calls get their citation ids, quoted ids and answer text
rewritten in one pass. Anything ambiguous is left for the retirement pass to judge.
The principle is the same as card resolution: never guess.

---

## 9. Spend cap

Every model call in every binary goes through one `Metered` wrapper around the backend.
The wrappers in a process share one `SpendMeter` with a cap (`JUDGE_MAX_USD`, default $5).
Before a request is sent, its **worst-case** cost is reserved against the counter: the
request at the input price plus `max_tokens` at the output price. If that would breach the
cap, the request is refused. After the response, the reservation is replaced with the
actual usage. So concurrent callers cannot collectively overshoot, and a response body that
fails to decode is still billed when its usage could be read.

The trait the pipeline calls (`ChatModel`) is sealed and `Metered` is its only implementor,
so a backend that skips the cap cannot be handed to the pipeline. A local model priced
`Free` is counted but never refused.

Two prompt-caching details cut cost. The system prompts are stable and carry a cache
breakpoint, so repeated questions reuse the cached prefix. The rendered material in the
synthesis user turn has its own breakpoint, so the tool-round continuation rereads it at
the cache price. After a tool round the `tool_choice` stays `auto` rather than switching to
`none`, because changing it would invalidate that cache.

A full run of the 21-question gold evaluation set costs about $2.50. Development is done
against a mocked HTTP server (`wiremock`), not the live API.

---

## 10. The three front doors

All three share one composition root, `judge_bot::build_deps`, so they run the same
pipeline.

- **Discord** (`crates/bot`): a `/judge` slash command, thread history as context, "did you
  mean?" buttons backed by a pending store, rating buttons, card mana symbols drawn as
  application emoji. The rendering logic is pure and unit-tested. A mana emoji tag is about
  thirty characters and must never be cut in half by Discord's length limit, so rendered
  text is carried as segments where only plain text is cuttable.
- **Web** (`crates/api` + `web/`, a SolidJS page): anonymous, so no ratings. Ambiguity comes
  back as data. The client re-asks with pins that the server rewrites to
  `[[Full Card Name]]`. Session history keys on a client UUID. Rate-limited per IP.
- **Agent** (`crates/agent`, `judge-cli` and `judge-mcp`): the judge as a tool for other AI
  agents. It has two modes. The `judge` tool runs the pipeline as above with the built-in
  model. A **session** runs it in pull mode, where the outside agent *is* the model. It
  receives the extraction prompt, returns extraction JSON, receives the rendered synthesis
  prompt, and returns a verdict. That verdict goes through the same validation. State lives
  in Postgres between calls as a `Stage` enum, with the same one-tool-round, one-retry
  limits. This is also the cheapest way to reproduce a bad answer: Claude Code drives it
  directly (`.claude/skills/judge/SKILL.md`), spending nothing.

---

## 11. Providers

Which chat model and which embedder to use is a `judge.toml` file, not code. The pipeline
talks to a provider-neutral `ChatRequest`/`ChatResponse` in `crates/llm`. `crates/anthropic`
and `crates/openai` are backends that own their wire formats. Anthropic can be reached
directly, through a proxy, or on AWS and GCP with the platform's own credential chains. Any
OpenAI-compatible chat-completions server works, which covers local models. Embeddings come
from Voyage or any OpenAI-compatible `/embeddings` endpoint.

Where a backend cannot enforce the output schema server-side, the adapter appends the schema
to the user turn. The system prompt is therefore byte-identical on every backend, and a
pinned digest and golden fixtures guard that. Every model on a paid provider must have a
price so the cap can reserve for it. Unknown Anthropic models fall back to a table that errs
high.

---

## 12. Evaluation

`eval/gold.yaml` holds 21 adversarially verified questions with the rule ids an answer must
cite, plus per-question lists of equivalent ids that state the same fact. Two gates:

- `judge-eval recall` runs only extraction, resolution and retrieval and fails below 90% of
  expected rule ids in context. No model spend for synthesis.
- `judge-eval answer` runs the full pipeline and scores the answers. Runs are stored and can
  be re-scored for free after the gold set is edited.

The gold set is extended whenever capability is added. It is the closest thing the system
has to a regression suite for the probabilistic parts.

---

## 13. Technology choices

**Rust.** Chosen for what the compiler enforces (`docs/DECISIONS.md` D1 lists nine
invariants, D2 the languages it was weighed against). In this codebase that means:

- Exhaustive enums: every consumer of `Resolution`, `Citation` and `JudgeError` handles
  every case, so "ambiguous" cannot be silently treated as "resolved".
- Newtypes with validators (`nutype`): a `RuleId` matches the CR's id pattern, a rating is 1
  to 3, a `NonEmpty<Face>` list cannot be empty. Invalid data is unconstructible.
- Typestates: `Verdict<Validated>` and `Synth<Final>` make "unvalidated answer reaches
  Discord" and "second tool round" compile errors.
- `Result` everywhere and lints that deny `unwrap`, `expect`, `panic` and slice indexing, so
  every failure on the judge path is a value the Discord layer must render.
- Compile-time checked SQL (`sqlx`): every query is checked against the schema at build
  time, including pgvector columns, so a renamed column is a build failure.
- A crate graph as an effect fence: `crates/core` has no I/O dependencies, so pure logic
  (resolution rules, context assembly, validation) cannot sneak in a network call.

The cost accepted: the HTTP clients for the model APIs are hand-written and pinned against
golden request fixtures.

**Postgres 16 + pgvector + pg_trgm.** One database does relational storage, full-text
search, trigram fuzzy matching and vector search, all joinable in one query with
transactions and advisory locks across them. At this scale a separate vector database would
add an operational component and a consistency problem for nothing. HNSW indexes make
approximate nearest neighbour search fast on a few thousand rows.

**Voyage AI embeddings** (`voyage-3.5`, 1024 dimensions, by default). A query/document
distinction and a hosted API, so the zero-config setup does not need a local model (which
is not worth the trouble under WSL2). Swappable by config.

**Anthropic Claude for both model stages.** A low-effort call for extraction and a
high-effort call with tool use and structured outputs for synthesis. Prompt caching
matters for the large synthesis turn. The provider seam means this is a default, not a
lock-in.

**serenity + poise** for Discord, **axum** for HTTP, **rmcp** for MCP, **SolidJS + Vite**
for the page. All conventional, well-maintained choices for their niches.

**Docker Compose behind a Cloudflare Tunnel.** One host, no open inbound ports, a
CI-built image, nightly data refresh as a cron job rather than a service, weekly backups
to R2. `docs/DEPLOYMENT.md` is the runbook.

---

## 14. Glossary

- **RAG (retrieval-augmented generation)**: fetch relevant documents first, then have a
  model answer from them rather than from memory.
- **Embedding**: a fixed-length vector of floats that an embedding model produces from
  text, such that similar meanings give nearby vectors.
- **Vector space / embedding space**: the set of vectors one specific model produces.
  Vectors from different models, or the same model at a different width, are not
  comparable. This bot records which space the database holds.
- **Cosine similarity / distance**: how aligned two vectors are. pgvector's `<=>` is the
  distance (0 is identical).
- **HNSW**: a graph index for approximate nearest-neighbour search. It is fast and
  occasionally misses the true nearest.
- **Dimensions**: the length of the vector (1024 here). More is not automatically better,
  because it costs storage and index time.
- **Full-text search / tsvector / BM25**: keyword search with stemming and rarity
  weighting. Postgres's `ts_rank_cd` is its relevance scorer.
- **Trigram (pg_trgm)**: similarity based on shared three-character windows. Good for
  typos in names.
- **Hybrid retrieval**: combining keyword, semantic and structured search because each
  fails differently.
- **Structured output**: the model API enforces a JSON schema on the reply.
- **Tool use / function calling**: the model asks the program to run a named function
  (`lookup_rules`) and gets the result back before answering.
- **Prompt caching**: the provider caches a stable prefix of the prompt and charges much
  less to reread it.
- **Typestate**: encoding an object's lifecycle stage in its type so that the wrong
  operation at the wrong stage does not compile.
- **Oracle text**: a Magic card's current official wording, as opposed to what is printed.
- **CR**: the Comprehensive Rules. **MTR / IPG**: tournament policy documents, out of scope.

## 15. Where to go next

- The pipeline: `crates/core/src/judge.rs`, then `verdict.rs` and `quote.rs`.
- Retrieval SQL: `crates/bot/src/db/retrieve.rs` and `rules.rs`.
- The resolution ladder: `crates/bot/src/db/resolve.rs` (its module comment is thorough).
- Vector-space bookkeeping: `crates/bot/src/db/space.rs`, `crates/embed/src/space.rs`.
- The model contract: `crates/bot/src/prompts/synth_system.md`.
- The spend cap: `crates/llm/src/spend.rs`.
- The reference and the reasoning: `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`,
  `docs/PROVIDERS.md`.
- Outside reading: the pgvector README (HNSW, distance operators), the Postgres full-text
  search chapter, Voyage AI's docs on `input_type`, and Anthropic's docs on structured
  outputs, tool use and prompt caching.
