# Design decisions

The decisions that shaped this codebase, and why each was made. A new reader or a future
maintainer can use it to tell a deliberate choice from an accident.
`docs/ARCHITECTURE.md` describes *what* exists and is kept current with the code.
`docs/EXPLAINER.md` is the narrative tour. This file records *why*. Each entry names the
alternative that was rejected. Dates are when the decision was made.

## D1. Compiler-enforced correctness

*Decided 2026-08-29.*

Correctness comes from the compiler, not from test discipline. The project started with
a priority order:

1. what the compiler enforces,
2. how naturally the domain can be expressed,
3. whether the ecosystem makes it buildable without inventing infrastructure.

Code volume and build time were declared non-criteria. Nine invariants were written down
first. The language, the libraries and most of the type design follow from them:

| # | Invariant | Where it bites |
|---|---|---|
| I1 | `Resolution`, `Citation`, `JudgeError`, `Source` are closed sums; every consumer handles every case | "Ambiguous" can never be silently treated as "resolved" |
| I2 | `Card.faces` is non-empty; `RuleId` matches `^[0-9]{3}(\.[0-9]+[a-z]{0,2})?$`; a rating is 1, 2 or 3 | Invalid data is unconstructible |
| I3 | A `Verdict` cannot be persisted or sent to Discord until citation validation has run | Hallucinated citations never reach users |
| I4 | The synthesis tool loop runs at most one `lookup_rules` round | Bounded cost, no runaway loops |
| I5 | The model's output schema and the type we decode into are one definition | Schema drift is a compile error, not a 400 |
| I6 | SQL parameter and column types match the schema | Retrieval cannot silently return the wrong shape |
| I7 | `core` (resolution, context assembly, validation) performs no I/O | Pure logic is testable and cannot sneak in a model call |
| I8 | Errors on the judge path are values of `JudgeError`, not exceptions | The Discord layer must render every failure |
| I9 | No `null`/`undefined` reaches domain code | No boundary leaks |

**Rule that follows.** When adding an invariant, make the bad state unrepresentable
(exhaustive enums, `nutype` newtypes with validators, `NonEmpty`, typestates). A runtime
check is the fallback. A test is the fallback to that. `CONTRIBUTING.md` restates the
ones a change is most likely to bump into.

## D2. Rust

*Decided 2026-08-29.*

Eight languages were scored against I1–I9. Three cleared the bar, and the gap between
them was small. The gap from third to fourth was not.

- **Haskell** scored highest on paper. It is the only language enforcing I5, I7, I8 and
  I9 at once: autodocodec derives schema and parser from one codec, and effect systems
  give per-port effect sets. Rejected for ecosystem concentration (single-team Discord
  and model-API libraries) and toolchain friction.
- **Scala 3 on the JVM** is the author's best-known stack and enforces the core
  invariants. Every gap is a Java-interop leak (I5, I8, I9 at the SDK and Discord-library
  boundary) that must be fenced by convention and tests rather than by the compiler.
  Scala.js was a net correctness loss: the two things you would import arrive as
  unchecked facades.
- **Rust** enforces I1–I6, I8 and I9 at compile time and is the *most* idiomatic home for
  I3 and I4 (typestate). Its one structural gap is I7: an `async fn` can do anything and
  the compiler will not object. That gap is accepted and fenced by the dependency graph.
  `crates/core` has no reqwest, sqlx or tokio-net, so I/O there is a build failure by
  *dependency* rather than by *type*. The other accepted cost is owning the model-API
  clients, because there is no official Rust SDK (see D3).

The rest fell short:

- F# was ecosystem-safe but bought little beyond Kotlin with discriminated unions.
- TypeScript, Kotlin and Go were below the bar for "compiler over test suite".
- OCaml, Gleam, Elixir, Swift and the research languages lacked a viable Discord library
  or exhaustiveness.

## D3. Owned wire types and derived schemas

*Decided 2026-08-29.*

The project owns its wire types and derives each model schema from the type it decodes
into.

- **Hand-written HTTP clients** (reqwest + serde) for Anthropic, OpenAI-compatible
  servers, Voyage and Scryfall. They are treated as project code and pinned by golden
  fixtures. The community Anthropic crates were either framework-sized or unmaintained,
  and the wire surface is one endpoint per provider. Every call is non-streaming. Every
  request keeps `max_tokens` ≤ 16k, inside the non-streaming guidance. Streaming would
  buy nothing the judge needs.
- **schemars from the serde struct** (I5). One transform per backend reduces the schema
  to what that backend accepts: `additionalProperties:false`, `oneOf→anyOf` and stripped
  constraints, plus strict-mode `required`/`anyOf [T, null]` for OpenAI. serde and
  `nutype` still enforce the stripped constraints on decode. The server's enforcement is
  an optimisation, not a guarantee.
- **sqlx with compile-time checked queries** and a committed `.sqlx` offline cache (I6),
  over Diesel or a query builder. `cargo sqlx prepare --check` in CI keeps the cache
  current.
- **serenity + poise** for Discord (slash commands, buttons and threads were all verified
  before the choice), **axum** for HTTP, **rmcp** for MCP, **SolidJS + Vite** for the
  page. These are conventional, maintained choices where nothing in the invariants pushed
  one way.
- **Workspace lints** deny `unwrap`, `expect`, indexing and `panic!` in every crate, and
  warn on the pedantic group and missing docs. I8 is a lint failure, not a review
  comment.

## D4. Entity-first hybrid retrieval

*Decided 2026-08-29.*

Retrieval is entity-first and hybrid, and the CR is chunked at two granularities. Rules
questions have the shape "card A + card B + rule concept C", so:

- **Card names are entities.** A cheap structured-output call extracts them and SQL
  resolves them through a typed resolution order: alias → possessive-stripped alias → exact →
  printed name → short name before the comma → alias suffix → trigram fuzzy. A
  `[[bracketed]]` span is exact name, printed name or exact alias only, with near misses
  offered as choices. An alias is not a near miss: the table maps that spelling to one
  card, so `[[bob]]` is Dark Confidant. Embeddings are never used for this: a nickname
  like "bob" has no semantic relation to *Dark Confidant*.
- **The resolver never guesses.** Ambiguity is `Resolution::Ambiguous` and becomes a "did
  you mean?" button row (I1). A wrong card silently resolved would produce a confidently
  wrong ruling with valid-looking citations, which is the worst failure the bot can have.
- **Three retrieval sources, unioned in priority order.** They are the curated category →
  CR-section map (structured, always on), full-text search (keywords like "leaves the
  battlefield") and pgvector similarity (meaning). Each fails differently. The synthesis
  budget renders a prefix of the union, so the order is what the model reads. Rulings for
  every face, glossary entries, hand-curated "nightmare card" notes and rated prior calls
  are added directly once cards resolve.
- **CR rows at two granularities.** The rule (`702.19`) has a body including every
  lettered sub-rule and example. These rows carry embeddings and feed retrieval. The leaf
  (`702.19b`, `parent_id` set) is the citation target. Scoring and `lookup_rules` treat
  leaf and parent as covering each other.
- **The category taxonomy is data.** `data/categories.yaml` generates the `Category` enum
  in `core/build.rs`, so a taxonomy edit is a recompile and every `match` stays exhaustive.
  The extractor's schema makes the primary category required.

## D5. Citation validation and the verdict lifecycle

*Decided 2026-08-29.*

Citations are validated client-side, and the answer's lifecycle is a type.

- Every `Citation` carries a verbatim `quote`, checked as a substring of its source in the
  retrieved `Context`. The stored quote is the *source's* span, not the model's string.
  A persisted quote is therefore byte-exact, and the retirement pass (D11) stays a strict
  check.
- The comparison folds typographic punctuation one character to one character: curly
  quotes, the dash block, non-breaking spaces. It never folds case or words. Models
  retype the CR's `’` as `'`, and that was the most common rejection.
- Only `Verdict<Validated>` can reach `CallStore::persist` or Discord rendering (I3). Only
  `Verdict<Unvalidated>` is `Deserialize`. `Synth<Fresh | ToolRequested | Final>` makes a
  second tool round a compile error (I4).
- **One retry**, with the rejection rendered into the prompt and the rejected answer quoted
  back as a blockquote, then an error the user sees. Retries live in `judge()`, not in the
  Discord layer, so every interface gets the same behaviour.
- The bot **declines tournament policy** (MTR/IPG) after the cheap classification call
  rather than answering it badly. That material is not ingested.

## D6. Providers as configuration

*Decided 2026-09-02.*

Providers are configuration: a TOML file, with env-only as the zero-config default. An
operator should be able to run the judge on any model they can reach. The alternative
was a dozen `JUDGE_*_MODEL` / `_PROVIDER` / `_BASE_URL` variables. That fits Docker's
`env_file` better, but it cannot express per-provider dialect knobs or pricing without
becoming a naming scheme of its own.

So there is `judge.toml`: typed structs, `deny_unknown_fields`, `nutype` validators, and
secrets named by environment variable and never written in the file. With no file, the
binaries build the default setup from `.env`: Anthropic direct, `claude-opus-5-5` for both
stages, Voyage if keyed. The eval numbers were measured on that setup. A knob that would
be silently ignored is a load error naming both keys.
`docs/PROVIDERS.md` is the reference.

## D7. Spend cap by type

*Decided 2026-09-02.*

Every model call is behind the spend cap by type. The operator is cost-sensitive, and the
model calls are the only thing that costs money per question. The pipeline's `ChatModel`
port is *sealed*. `Metered<B>` is its only implementation and `Models`' fields are
private, so there is no way to construct an uncapped model or to meter one against a
foreign budget.

The cap **reserves the worst case before sending** and settles on reported usage. That is
why caps under about $0.36 refuse synthesis outright rather than overshooting.

Pricing is a closed sum:

- the built-in table for Anthropic models, re-read for the model the *response* names,
  since a fallback may route elsewhere,
- an operator's per-token rate,
- `free`.

The table lists only current models: an upgrade replaces its predecessor's row. An
unknown Anthropic model, a refusal fallback's included, prices as the default, the dearest
in the table. A dearer one (Fable, say) is therefore under-counted: a stage that names one
needs an operator's rate, and a fallback that lands on one settles low. That is accepted,
because a fallback happens only on a safety refusal.
An unpriced model on an OpenAI-compatible provider is a startup error, because it could
be anything.

Dollars stay per process rather than per user or per server. The meter settles after the
call, so a finer-grained dollar cap would either over-reserve or overshoot. Question-count
limits are the tool for finer grain, and the MCP transport, the web page and the Discord
bot have them. D19 makes the cap a budget over time without changing any of this.

## D8. Native cloud auth for Anthropic

*Decided 2026-09-02.*

Bedrock and Vertex were already reachable through LiteLLM or their OpenAI-compatible
endpoints with no new dependencies. Native SigV4 and ADC support was added anyway, for the
operator who wants Claude on their own cloud account *without* running a proxy. The cost
is two auth crates behind the `aws`/`gcp` Cargo features. The features are on by default
and named in the Dockerfile. A lean build without them cannot name these endpoints, and the
loader says "not built".

Credentials come from the platforms' own chains, never from `judge.toml`. They are
resolved lazily and probed once at startup, so an empty chain fails there, not on the
first question. Each endpoint's feature mask (what it cannot accept: server-side fallbacks,
strict tools, betas) was verified against the live docs. The mask is an exhaustive
`match`, not a flag. The one flag over it is a provider's `refusal_fallbacks`, because
Anthropic documents that beta for its own API only and a proxy or Claude Platform on AWS
may take it anyway. It cannot turn fallbacks on for an endpoint that takes no beta header.

## D9. Embedding space tracking

*Decided 2026-09-02.*

The database records which vector space it holds, and switching is one explicit command.
Vectors from two embedding models cannot share a column, and pgvector's HNSW index has a
fixed width. The alternative to `ingest reembed --yes` was to refuse and tell the operator
to `ALTER` and re-run `embed` by hand. The command is safer.

- Every embedder carries its `Space` (kind, model, width).
- A one-row `embedding_space` table names the stored space.
- `ingest embed` writes that row with the first vector it writes and refuses to write
  into another space.
- The adapters re-read the row on every use and turn vector search **off, never mixed** on a mismatch
  (error log naming both spaces).

Writers hold the space under the shared side of an advisory lock. The switch takes the
exclusive side, so it waits for in-flight writes. `reembed` probes the new embedder
before clearing anything, because re-embedding pays the provider per row. That is also
why the weekly backup exists (D26).

## D10. Ratings and retrieval

*Decided 2026-08-29.*

Ratings shape retrieval and nothing else. Answers can be rated 1–3 on Discord. A rating
changes one thing: which prior calls are shown to the model as *examples* for a similar
question.

- Scores are a Bayesian mean (prior 2.0, weight 3), so one early vote cannot swing a
  call's standing.
- Calls below 1.5 with at least five votes are excluded.
- A member holding the operator's judge role overrides the crowd (`effective_score`).

Prior calls are always rendered *after* the CR material and labelled as precedent, never
authority. The CR outranks anything the community has said. The anonymous web page never
rates, because a rating with no identity behind it is noise. `/forget` deletes a user's
ratings and anonymizes their failed calls (D27), the only per-user data kept. Questions
are stored against the channel, not the asker.

## D11. Call retirement

*Decided 2026-09-01.*

A stored answer is retired when its citations stop holding, not when the CR changes. The
alternative, retiring every call on a new CR release, would discard almost everything for
nothing: most rules do not change.

The retirement pass re-runs `citation_supported` over every stored call against today's
rules, rulings and Oracle text. It sets `retired_at` both ways, so restored text brings a
call back. Each call also carries an Oracle-text fingerprint per context card, so an
erratum retires calls *about* the card even when they cited only the CR. Rulings are keyed
by content, so a re-indexed ruling is the same ruling.

A **renumbered rule keeps its calls**. Inside the CR load transaction, old and new rules
are matched by body with every rule id masked. A match counts only where the masked body
is unique on both sides, and only where rewriting the old rule with the whole map
reproduces the new one exactly. That fixpoint stops a redirected cross-reference being
mistaken for a renumbering. Anything ambiguous is left to the retirement pass. The loader
never guesses, for the same reason the resolver does not (D4).

## D12. One Postgres

*Decided 2026-08-29.*

One Postgres holds everything: relational storage, full-text search (`tsvector`), trigram
fuzzy matching (`pg_trgm`), vector search (pgvector, HNSW) and advisory locks. All of it
is joinable in one query and covered by one transaction and one backup. A separate vector
database at a few thousand rows would add an operational component and a consistency
problem for nothing. A separate rate-limit or session store would turn single-process
in-memory values into distributed state. The spend cap, the concurrency semaphore and the
rate limiter are per process by design (see D16).

## D13. Eval first

*Decided 2026-08-29.*

The build order was chosen so retrieval was measured before any synthesis existed:

1. A gold set of adversarially verified questions with expected rule ids
   (`eval/gold.yaml`, 21 today). Per-question *equivalence lists* make the metric track
   correctness rather than one author's citation taste.
2. Ingest.
3. Extraction and resolution on the gold set's card mentions.
4. Retrieval, behind a gate of at least 90 % of gold rule ids present in the context.
5. Synthesis, scored against the gold answers.
6. Discord.
7. The prior-call query, which needs rated data to exist.

`judge-eval recall` still runs the gate for free on every retrieval change. The paid full
run costs about $1.70 and is not part of CI. Nothing in the test suite calls a paid API:
HTTP backends are tested against wiremock.

## D14. Outside agents on the same pipeline

*Decided 2026-09-02.*

Outside agents drive the same pipeline, with the same validation. A Claude Code session,
or any MCP client, can be the model. `judge_bot::session` hands it the extraction prompt,
then the synthesis prompt rendered from the same `Context`. It admits the agent's verdict
only through `Verdict::validate`, with the same one tool round, one retry and rejection
notice. The alternative was to expose the lookups alone and let the agent answer freely.
That would have produced answers with no validated citations, which is what the project
exists to prevent.

- Agent thread ids carry their own prefix, so a session can never read or write a Discord
  thread's history.
- Inputs are bounded.
- Persisting is idempotent in the database.
- A session-persisted call is thread history only, because nothing can rate it.

The operations are one list (`crates/agent/src/ops.rs`). The CLI and the MCP server only
transport. This is also the cheapest way to reproduce a reported bad answer: no API spend,
same code paths.

## D15. Single host behind a Cloudflare Tunnel

*Decided 2026-08-31.*

The reference deployment is one machine at home behind a Cloudflare Tunnel, running a
CI-built image. It has no public IP, no forwarded port and no cloud compute bill. The
tunnel keeps two properties a serverless split would lose. The Discord gateway stays one
long-lived connection, with no HTTP-interactions rewrite. The spend cap, semaphore and
rate limiter stay single-process values.

CI publishes the image and the host pulls it, because a release build wants about 4 GB of
RAM and real CPU, which a NAS does not have. The data refresh runs inside the `judgebot`
process (its `--jobs` role, D25) on a schedule kept in the database (D24). The one-shot
`refresh` container stays for the first load and for manual runs. The weekly backup is
an opt-in container of its own (D26).

**Rate limiting buckets on an address the caller cannot choose.** `API_CLIENT_IP` is
`peer` or `cloudflare` (`CF-Connecting-IP`), never `X-Forwarded-For`. Cloudflare *appends*
to a caller-supplied header instead of replacing it, which would hand every request a
fresh allowance against a paid endpoint. Any other `API_CLIENT_IP` value fails at
startup rather than falling back. Deploy credentials live in their own env file that the
internet-facing processes never read.

## D16. One judgebot per community

*Decided 2026-09-15.*

There is one judgebot per community and no multi-tenancy. Each instance is private to the
servers of whoever runs it, and the project is offered as something to **run yourself**,
not as a bot to invite. A per-server tenancy layer (admission lists, per-guild quotas and
judge roles, an admin command) was sketched and rejected:

- Everything tenancy would have to partition is *already per process*: the spend cap, the
  concurrency limit, the judge role name and the Discord token. When the process is
  yours, that is the behaviour you want, and it needs no new table.
- The knowledge is global. The Comprehensive Rules, rulings, card text and the rated prior
  calls apply to every server alike, so nothing in retrieval or persistence needs a tenant
  boundary. Tenancy would have lived only in the Discord adapter and bought the maintainer an
  admission and billing problem.
- The model calls cost money per question. A shared instance means one operator paying
  for strangers' questions, or a billing system. Both are out of scope for a hobby
  project. Running your own puts the bill with the person who chose the model.
- The licence (AGPL-3.0-or-later) and the design already make self-hosting the first-class
  path: one compose file, a CI-built image, a zero-config model setup, data loads that
  cost cents.

What follows for the code and the docs:

- The setup experience is organised around creating your own Discord application and
  instance (the README's "Running it" and the site's "Run your own judgebot" section).
- `GUILD_ID` keeps its meaning as a registration shortcut rather than an allowlist.
- An instance's anonymous web page is a second public interface to the instance its operator
  runs. It is public in the sense that it needs no login. It is not a shared service
  other communities are meant to depend on.
- One bot in several servers you administer works today. What is shared between them is
  the spend cap and the judge role name, by design.

## D17. Non-goals

Tournament policy (MTR/IPG). Accounts or ratings on the web page. Streaming responses.
A plugin system for providers (they are workspace crates chosen by configuration).
Retraining or fine-tuning of any kind. Automatic detection of "nightmare" cards (the
notes are curated by hand). Multi-server tenancy (D16).

---

## D18. Releases and image architectures

*Decided 2026-09-15.*

A release is a tag on the image already running, and the image is built natively for two
architectures.

`publish-image.yml` builds on every push to `main` and publishes `latest` plus an
immutable `sha-<short>`. A GitHub release (`vX.Y.Z`) then points `X.Y.Z`, `X.Y` and `X`
at the image already built for that commit, through a manifest retag. A release is
therefore what has been running as `latest`, down to the platform digests, and costs no
build. Only a commit with no image of its own (docs-only, or a build cancelled by a later
push) is built at release time, and then through CI like any other. **Rejected:**
rebuilding on the tag. A second build of the same tree is not guaranteed identical to the
one that was deployed, and the release would be an untested artefact.

The image is a manifest list for `linux/amd64` and `linux/arm64`. Each is built on a
GitHub runner of that architecture and joined by digest, so a Raspberry Pi or ARM NAS
pulls the same tag. **Rejected:** emulating arm64 under QEMU (a Rust release build takes
hours there) and cross-compiling inside the Dockerfile (a second toolchain and linker to
keep working for `aws-lc-sys` and `ring`, on a build that would then run twice on one
runner).

## D19. Budget period beside the cap

*Decided 2026-09-19.*

`JUDGE_MAX_USD` capped one process for its lifetime. That is simple and safe, but it is
not what an operator means by a budget: `bot` and `api` each had the whole cap, and a
restart handed it out again. `JUDGE_BUDGET_PERIOD=day|month` makes the cap cover the
current UTC day or month, across the processes and across restarts.

The meter stays the enforcement point: in memory, atomic, reserving before it sends
(D7). The period enters as one number added to what the cap sees, the meter's
*adjustment*. It counts other processes' spend this period in and this process's
earlier periods out. `judge_bot::budget` computes it from a `spend_days` table, which
each process adds its own share to every ten seconds. The period boundary is Postgres's
clock, so two processes cannot disagree about which day it is. `judge-llm` still knows
nothing of storage or time.

The cost is a bounded overshoot: two processes can together pass the cap by what they
spend between two syncs. A crash loses at most that much of the record. With no period
set nothing is summed, but the ledger is still written, because `judge-cli stats` is
where an operator sees what a day cost.

**Rejected:**

- *Reserving in the database*, one row lock per model call. It closes the overshoot and
  puts a Postgres round trip and a contended row in front of every request, to protect
  against a few cents.
- *Persisting the meter on shutdown.* A crash loop, the case that spends the most, never
  shuts down cleanly.
- *A rolling window.* "The last 30 days" needs per-call rows and tells an operator
  nothing a calendar month does not.

The same task tells the operator when the cap trips (`JUDGE_ALERT_WEBHOOK`), once per
period. A capped judgebot is otherwise silent until someone reads the log, and the people
who notice first are the members being refused.

## D20. The prose answers to the citations

*Decided 2026-09-20.*

Validation checked every citation and nothing tied the prose to them. An answer could
write "per `605.3b`" with no citation of 605.3b and pass, as long as the citations it did
carry were good. A reader cannot tell a checked rule number from an unchecked one, so the
guarantee the project states ("every claim is backed by a validated citation") was
stronger than what the code enforced.

Now every rule number in the answer text must be covered by a rule citation: that id, its
rule, or one of its sub-rules, the same covering the retrieval scoring uses. The check is
a regex and a `RuleId` parse, run after the citations themselves are checked. A miss is
a typed rejection (`UncitedRules`) with one retry, like the others. On the 17 published answers
of the 2026-09-20 gold run the prose named 57 rule numbers and 55 were covered. The two
misses were in two different answers (`605.3b`, and `903.9a` beside a cited `903.9b`), so
about one answer in eight would have been retried, at about ten cents a retry.

The retry is given what it needs to comply. A number often comes from a pointer in the
material ("see rule 111.10" in a glossary entry) to a rule retrieval did not return, and
the retry cannot call `lookup_rules`. Before it, `LlmSynthesizer::fetch_uncited` looks up
each uncited rule the model was not shown, or pins one retrieval held but the budget cut,
so the notice's "add that citation" has something to quote. On 2026-10-09 a retry with no
way to cite 111.10 came back with an empty answer.

It covers rule numbers only, and only rule citations cover them. A ruling or a card's
Oracle text has no identifier in prose, so "a ruling says…" with no ruling cited still
passes, and a rule number is not covered by citing a prior call that mentions it.

**Rejected:**

- *A second model call that judges whether the prose follows from the citations.* It is
  the check one actually wants. It also costs a quarter to three-quarters of an answer
  again, adds seconds, and is itself a model that can be wrong. A probabilistic opinion
  does not belong among gates that are otherwise exact. If it is ever wanted, it belongs
  in `judge-eval` as a score, not in the path of every answer.
- *Accepting a number that is in the material but not cited.* Looser, and it would let the
  prose lean on text no quote was checked against.
- *Leaving the model to find out from the retry.* The prompt asks for rule numbers inline
  and for "usually one to four citations", which pulls against this check. It was
  measured first on the unchanged prompt: about one answer in eight retried. The answer
  style section now says it in one sentence: every rule number written must be one of the
  rule citations. The pinned digest and the golden fixtures were updated on purpose, and
  the published runs were redone on the new prompt.

## D21. Empty citations are dropped

*Decided 2026-09-20.*

The model sometimes pads its citations with a stub: `{"quote":"","ruling":""}`, a quote of
`"x"` or `"placeholder"`. It happens most on a card with no Scryfall rulings, where the
model opens a `scryfall_ruling` entry from memory and has no key to put in it. The output
is schema-constrained, so an entry once begun cannot be abandoned, only filled. Saying in
the material that the card has no rulings did not stop it, and neither did the prompt's
paragraph against stubs. Every stub rejected the whole answer and cost a retry, and the
retry sometimes stubbed again: in the 2026-09-20 gold runs, stubs were the only thing that
lost an in-scope answer on the default model.

A stub quotes nothing, so it supports nothing, and dropping it takes no checked support
away from the answer. So `validate` sets stubs aside and validates what is left exactly
as before. A stub is an entry whose `quote` is present and is blank, a stock word or
fewer than four characters, whether or not the rest of the entry parses.

What keeps this honest:

- Every citation that is shown was still checked verbatim against its source.
- At least one real citation is still required. An answer with nothing but stubs is
  rejected for them, with the notice that names the habit.
- D20 catches an answer whose prose leaned on a dropped rule by number.
- An unreadable entry that *does* quote something is a citation the model meant, and is
  still a rejection. So is one with no `quote` key or a `quote` that is not a string (a
  misspelt key, from a backend that does not constrain the output). So is a real quote
  under a wrong id.

What it does not catch: prose that still says "a ruling on this card says…" after the
ruling stub behind it was dropped. D20 ties rule numbers to citations, but rulings and
Oracle text have no identifier in prose to tie. The same sentence could be written today
with no stub at all. The prompt keeps its stern paragraph against stubs on purpose. It is
still true when nothing else is cited, and it costs nothing as a deterrent.

**Rejected:**

- *Repairing the entry*, such as relabelling Oracle text filed as a ruling. The quote may
  be genuine, but the verdict would then carry a citation the model did not write.
- *Dropping any citation that fails*, not only stubs. A failed citation with a real quote
  is a claim about a source, and the answer may rest on it.

## D22. Stray escapes are decoded

*Decided 2026-09-29.*

With structured output the answer is a JSON string, and Sonnet 5.5 sometimes escapes one
level too many: it writes `\\n` for a line break, which reads back as a backslash and an
`n`. Five answers across four Sonnet runs had it, and no Opus answer. Nothing caught it,
because a stray escape in a citation fails the verbatim check but the prose is only
checked for its length and its rule numbers. Discord and the web page showed the
backslashes, and a stored call carried them into later prompts as a prior example.

The answer is now an `Answer` newtype whose construction decodes a literal `\n`, `\t` or
`\"` into the character it stands for. A doubled backslash is read as one unit, as JSON
reads it, and kept as written, so decoding twice changes nothing. Every path that makes a
verdict goes through it, and every check on the answer runs on the decoded text. That can
add a rejection: a rule number the stray `\n` ran into (`see\n702.19b`) is now seen by
the prose check (D20). Its schema is `String`'s, so the request fixtures do not move.

This edits model output, which D21 declined to do for citations. The difference is what
the edit can change. A repaired citation makes a claim about a source that the model did
not make. A decoded `\n` changes layout and nothing a reader could take as a ruling, and
no rules answer means those two characters.

**Rejected:**

- *A rejection with its own retry notice.* It keeps "admitted as written" intact, but it
  costs a retry per occurrence for a habit the retry can repeat, losing an answer that was
  right.
- *Decoding at display time.* Each interface would need it, and the stored call would
  still carry the escapes into later prompts.

## D23. A generated config editor on localhost

*Decided 2026-10-07.*

`judge.toml` and `.env` are documented by their example files, but which keys apply where
(an endpoint's keys, a stage's provider kinds, which knob makes another an error) was learned
by loading the file and reading the error. `judge-config` is an optional page for editing
both. Three choices make it hold to the loader rather than drift from it.

- **The form is the loader's types.** `file_schema()` is schemars over the same serde
  structs `Config::from_toml` parses, so a new knob is a field with no editor change, and
  the structs' doc comments are written as the operator's help text. What types cannot
  say comes from the tables the loader checks against: which endpoint takes which key is one
  `EndpointKey::on` table, read by the misplaced-key check and emitted as `x-endpoints`.
  `endpoint_table_matches_the_loader` holds the endpoint constructors' required keys to it.
- **Validation is the binaries' own code.** Each draft runs through the loaders the
  binaries call at startup, one surface at a time, and `ConfigError::location()` (an
  exhaustive match) names the key or variable to fix. Nothing in the editor restates a
  rule. Two checks are left to startup because they depend on the serving machine: a
  cloud credential chain and `WEB_DIST`.
- **Secrets are write-only.** A value reaches the page only through a `Setting`, and
  only the registry makes one. A secret, an unknown variable, a value that expands `$`
  or a URL with credentials is reported as set or not. It can still be replaced: the
  page sends a new value (`DotEnv::replace`, typed into a masked input) and never
  receives the old one. Every reply is scrubbed of each secret in the file and each
  replacement, in the escaped spellings the loaders' messages use too, and the `.env`
  diff names a replaced variable without its value. A hidden setting is hidden for its
  value's shape, so one replaced with a plain value is shown again on the next load. A `.env`
  that dotenvy and Compose would read differently (a duplicate, an assignment the editor
  cannot place) is refused rather than edited.

It binds `127.0.0.1`, requires a per-run token in a header (which a cross-origin page
cannot send without a preflight the server never answers), and checks `Host` against
loopback names, which closes DNS rebinding. Writes are whole-file renames through
`toml_edit` and a line-preserving `.env` writer, so comments and untouched lines survive.

**Rejected:**

- *A static page on the documentation site.* It needs nothing installed, but it would
  either restate the cross-field rules in JavaScript or need the loader split into a pure
  crate compiled to WebAssembly. The loader reaches into the backends' endpoint types and
  the environment, so that split is the larger change.
- *Serving it from `judge-api`.* That process faces the internet. A route that writes files
  does not belong there, even behind a token.
- *A terminal UI.* It works over SSH without a port forward, but shows less of each key's
  help at once than a form does.
- *Showing secrets so they can be edited in place.* A page that never receives a secret
  cannot leak one, and replacing a key does not need the old one.
- *Leaving secrets to the file.* The first version did, but a new provider's
  `api_key_env` then meant leaving the page to finish the job. Write-only keeps what that
  protected: the page holds no secret it was not just given.

## D24. Refresh inside the running process

*Decided 2026-10-09.*

The data refresh (Scryfall, a new CR release, retirement, embeddings, emoji) needs to run
about daily on every instance, and it used to depend on the operator installing a cron
entry for `scripts/refresh-data.sh`. An instance whose operator skipped that step answered
from data that only got older, and nothing said so. The long-running process now runs it
itself (`judge_bot::jobs`, the `--jobs` role of D25), every `JUDGE_REFRESH_HOURS`
(default 24, `0` = off).

The schedule lives in Postgres, not in any one process:

- **A lease.** Every run, scheduled or not, holds `REFRESH_LOCK`, a session-level advisory
  lock on a connection of its own. Postgres drops it with the session, so a crash leaves
  nothing to clean up. A scheduled check tries it without waiting and does nothing when it
  is held.
- **A record.** `refresh_runs` holds each run's start, finish, trigger, process and step
  outcomes. A cron'd or manual `judgebot ingest refresh` writes the same row, so the schedule
  counts it.
- **Due on the database's clock.** `jobs::due` is pure: the last success older than the
  interval, and the last attempt older than a backoff (an hour, doubling with each
  failure in a row, capped at the interval). Ages come from `now()` in SQL, so every
  process and host agrees, a restart does not reset the schedule, and a failing refresh
  is retried soon, then less often, never on every check.

Any number of processes, plus cron, plus a person at a terminal, take turns through the
one lease. The run has its own OS thread, current-thread runtime and small pool, so its
blocking file I/O, a CR parse and its queries cannot take a worker or a pooled
connection from the request path. The database is not partitioned: while the CR load or
the retirement pass holds the calls lock, persisting an answer's vector waits, and the
reply with it, exactly as under a cron run. Every run, whatever started it, checks before
each step that the migration ledger matches its binary, ahead or behind, and stops after
three hours, so neither a stale container nor a hung download can write or hold the
lease indefinitely. A run stopped by a schema change records neither success nor
failure. A run whose process died is counted as failed once it is older than any live
run can be, so a crash loop backs off and alerts once.

**A scheduled run never pays for a mass re-embed.** Embeddings are outside the spend
cap (D7's `Metered` wraps chat models only). A scheduled run counts the rows waiting for
a vector first, and above `embed::UNATTENDED_CEILING` (800, against about 1,900 for a
full re-embed and a few hundred for a new CR release) it skips the step and alerts. A
manual run has no ceiling, because someone decided to pay. That includes a cron'd
`scripts/refresh-data.sh`, which is unattended but was installed by an operator who
chose cron over the schedule.

The webhook hears the first failure of a streak and the recovery, never each retry.

**Rejected:**

- *Host cron, or the NAS's task scheduler.* It is host-specific (DSM's Task Scheduler,
  systemd timers, crontab), needs root or the `docker` group to run `docker compose`, and
  fails silently when it was never installed. Nothing in the stack can tell that it is
  missing. It stays available: `JUDGE_REFRESH_HOURS=0` and the cron line of
  `docs/DEPLOYMENT.md` §7.
- *A scheduler container with the Docker socket*, starting the `refresh` service on a
  timer. The socket is root on the host, handed to a container.
- *`pg_cron`.* Postgres can schedule, but it cannot download a Scryfall bulk file, parse
  the CR or call an embedder.
- *A separate long-running jobs container.* It isolates the refresh from requests, but it
  is another service to configure and keep running, and the thread already gives the
  isolation. The single binary (D25) makes jobs one of its roles (`--jobs`), the same
  code in whichever process an operator chooses.

## D25. One binary with roles

*Decided 2026-10-09.*

The Discord bot, the HTTP interfaces and the data command line were three binaries
(`bot`, `api`, `ingest`) in two long-running compose services. They are now one binary,
`judgebot`, and one service. What a process does is a set of roles chosen at launch:
`--discord`, `--api`, `--web`, `--mcp` and `--jobs`, from the command line or, with none
there, `JUDGE_ROLES`. `judgebot ingest <command>` is the data command line.

- **The set cannot be empty.** `Roles` holds a `NonEmpty<Role>`, as `Interfaces` does
  for the API's interfaces, so a process that serves nothing is not a value the program
  can hold.
- **Requirements are checked before anything connects.** `roles::plan` matches every
  role exhaustively and returns the types the roles run on, which only their checks can
  make: `--discord` needs `DISCORD_TOKEN` and `JUDGE_OPERATOR_DISCORD`, the network roles
  `JUDGE_OPERATOR_EMAIL` (and `--web` a built page, `--mcp` an `MCP_TOKEN`), and any
  serving role a model that builds. Every unmet requirement is reported at once. A role
  added later cannot start without saying what it needs.
- **One process, one composition.** One pool, one configuration, one set of models
  behind one spend meter, one `Vectors`, the schema migrated once. The first serving
  role to stop ends the process, non-zero, so the restart policy brings all of it back.
- **The old names still run.** The image links `judge-bot`, `judge-api` and
  `judge-ingest` to `judgebot`, which reads the name it was invoked as and runs what that
  binary ran, plus `--jobs`, with a warning naming the replacement. A compose file
  written for two services keeps working on the new image. `API_INTERFACES`, which chose
  the old `api` service's interfaces, is still folded into the compose service's roles
  while `JUDGE_ROLES` is unset, and logs a warning. A later release removes both.

The reasons:

- **Setup is one service.** An operator configures, starts, reads the logs of and
  upgrades one container, not two that had to agree on a `.env`. The default
  (`--discord --api --web --jobs`) is what the two services ran together, and a
  deployment without Discord is one variable, not a service to leave stopped.
- **The refresh scheduler is a role.** D24 put the refresh inside the long-running
  process. As a role it runs wherever an operator says, with no host scheduler and no
  third container.

**Rejected:**

- *Separate `bot`, `api` and `jobs` containers.* Three processes, three pools and three
  spend meters that `JUDGE_BUDGET_PERIOD=process` capped separately. The isolation they
  bought was between components that share one database and one model budget anyway.
- *A jobs-only container* beside `bot` and `api`. D24 rejects it: the refresh thread
  already isolates the work, and a container is one more thing to keep running.

What follows:

- **One failure domain.** A crash or a deploy takes the bot and the page down together.
  The restart policy brings both back, and the HTTP listener is bound before the gateway
  is contacted, so a taken port fails before the bot logs in.
- **One spend meter.** With `JUDGE_BUDGET_PERIOD=process` the bot and the page now share
  one `JUDGE_MAX_USD`, where the two processes had one each. `day` and `month` were
  already shared (D19).
- **The backup and `judge-config` stay separate containers.** The backup reads R2
  credentials from `.env.deploy`, which must stay out of the internet-facing process
  (D15), so it is the `backup` service rather than a role (D26). `judge-config` edits `.env` and `judge.toml` on localhost and has no business in
  a process the public reaches.
- **Safe to run twice.** The refresh lease makes a second `--jobs` process safe, and the
  gateway lease (`GATEWAY_LOCK`, `judge_bot::discord::gateway`) a second `--discord`
  process: one holds the gateway and any other stands by, serving its HTTP roles. A
  holder that exits is replaced within the grace period (15 s) plus a gateway login,
  and one that vanished without closing its connection within about 25 s more, the
  TCP keepalives every lease session sets. The holder checks its lease every 5 s (4 s
  timeout) and on a loss closes the gateway (1 s limit) and stands by again, so a
  database restart costs the bot a reconnect and the HTTP roles nothing. Losses in
  quick succession pause before reconnecting (up to 10 min), because each connection
  spends one of Discord's 1000 daily logins per token. The grace
  exceeds the interval, the check timeout and the shutdown together (10 s), so the two
  never answer side by side. A holder in a paused VM that loses its session is the
  exception (`docs/DEPLOYMENT.md` §8). The lease is in the database and is
  session-level, so it covers processes sharing one database over direct connections,
  and the old `bot` image never takes it: the upgrade to `judgebot` still needs
  `--remove-orphans`.
- **Several replicas are still deferred** (issue 17). Safe is not supported: the
  per-user and per-IP limits are per process. Until those are solved, an instance runs
  one `judgebot`, and a second is a deploy overlap or a standby. A "did you mean?" pick
  is no longer one of the obstacles: it is answered from the message its buttons are
  on (the question) and the button's custom-id (asker, audience, span, a digest of
  the prompt, card), so a standby that takes the gateway answers the picks the old
  holder offered.
  `pick_claims`, a row per prompt with no text, keeps a double click from running the
  question twice.

## D26. Backup as an opt-in service

*Decided 2026-10-09.*

The weekly backup was the last host cron job: `scripts/backup-db.sh` ran `pg_dump` in the
`db` container through the Docker socket and uploaded with an `rclone` container. It is
now also `judgebot backup serve`, the `backup` compose service, behind the `backup`
profile. The service reaches `db:5432` over the compose network like any client, so it
needs neither the socket nor a host scheduler.

- **The schedule is the bucket.** A backup is due when the newest object named like
  one (`judgebot-<UTC stamp>.dump.gz`) is `BACKUP_EVERY_DAYS` old. Nothing else is
  recorded, so a restart does not reset it and a backup the script took counts.
  Failures back off in memory (an hour, doubling, at most a day). The webhook hears the
  first failure of a streak, a failure at a different step than the last (so a
  pruning failure that recurs weekly cannot hide a dump that starts failing), and the
  recovery. A listing that works again ends a streak of listing failures.
- **Pruning keeps a floor.** Past `BACKUP_KEEP_DAYS` only, only backup-named objects,
  and never the `KEEP_NEWEST` (2) newest, so a lapse longer than the retention followed
  by one success, perhaps of a re-initialised database, cannot leave one restore point.
  The script's `rclone delete --min-age` has no such floor; the difference is
  documented rather than ported to the script.
- **Indistinguishable from the script.** The same `.env.deploy` settings and defaults,
  the same names under the same prefix, the same bytes (`pg_dump -Fc`, gzipped), the
  same size floor, pruning only after an upload. `list`, `fetch` and the restore drill
  work across both, and either can be dropped.
- **Settings are types.** `backup::settings` parses every variable once at start into
  validated values (an `https` endpoint with nothing after the host, or `http` with a
  warning for a local stand-in, an S3 bucket name,
  a prefix of plain segments, day counts in range, the `DATABASE_URL` as libpq
  variables) and reports every problem at once, naming the variable and never quoting
  the value. Keys and the database password are `ApiKey`s, redacted in `Debug`.
- **Credentials stay separated (D15).** The service reads `.env.deploy` and is given
  `DATABASE_URL` by the compose file. It has no `env_file: .env`, so it holds no model
  or Discord keys, and the internet-facing `judgebot` still holds no R2 keys.
  `pg_dump` gets the database password in its environment, never its arguments.

**`pg_dump` comes from PGDG, binary only.** Debian bookworm's client is 15, and `pg_dump`
refuses a server newer than itself. The runtime stage adds the PostgreSQL project's apt
repository and installs `libpq5`; a build stage installs `postgresql-client-16` there
and only its `pg_dump` is copied out, and `pg_dump --version` in the runtime stage fails
the build on either platform when a library is missing. That adds about 6 MB unpacked.
The repository's key is pinned by checksum (`ADD --checksum`), so a swapped key fails
the build. `pg_dump` runs with a cleared environment: the connection's `PG*` variables
and `PATH`, `HOME`, `TMPDIR`, nothing else of the process's.

*Rejected:*

- *The whole `postgresql-client-16` package.* It depends on Perl through
  `postgresql-client-common`'s wrapper: about 100 MB on disk for a binary that never
  runs Perl.
- *Copying `pg_dump` and `libpq` out of the `pgvector/pgvector:pg16` image.* libpq's
  own libraries (Kerberos, LDAP) would have to be matched by hand, under a lib path that
  differs per architecture.
- *A second image for the backup* (built `FROM postgres:16`). Another tag to publish,
  pin and roll back in step with the first, for a few megabytes.
- *A role of `judgebot`.* The R2 keys can delete every backup; they would sit in the
  internet-facing process.

**The S3 client is four signed requests.** List (`ListObjectsV2`), put, get and delete,
signed with `aws-sigv4` and sent with reqwest, the response XML read with `roxmltree`.
All three were already in the dependency graph (the Anthropic AWS endpoints, resvg), so
the change adds no crate that is built (the lockfile gains a wasm-only `wasm-streams`
entry, from reqwest's `stream` feature). An upload is one `PUT` streamed from the file, with its SHA-256
signed in `x-amz-content-sha256`, so R2 refuses bytes that differ from the dump.

*Rejected:*

- *`aws-sdk-s3`.* The standard client, but a very large generated crate for four calls,
  and its default request checksums have needed configuration to suit R2 and
  S3-compatible stand-ins.
- *`object_store`.* A good abstraction, but it brings its own reqwest and TLS stack
  beside ours.
- *Keeping `rclone` in a container.* That needs the Docker socket, which is root on the
  host, handed to a container.

**No healthcheck.** What matters is the age of the newest backup, which is in the
bucket, and a failure goes to the webhook rather than into a status nobody reads on an
unattended host.

## D27. Failed calls are kept, private ones included

A log line says "empty verdict" and not what the model wrote or what was asked, so a
failure could not be diagnosed after the fact. A failed call is now a row in `failed_calls`
(`CallStore::record_failure`): the question, the error, why the first attempt was rejected
and the answer text of each attempt. It is written for the operator failures of
`JudgeError::is_operator_failure` only, from every interface (Discord, HTTP API, agent
`judge`).

- **Table, not logs.** A log carries the text unbounded, across lines, and is gone with
  the container. Rejected: logging the text at INFO.
- **Gaps.** Only the `answer` text is stored, not citations (the error and first
  rejection name the failing one). A truncated or tool-misuse attempt returns no answer, so
  it has no text.
- **Bounded on write.** The question (2000 characters), each attempt (8000) and the error
  (4000) are cut, and an insert deletes rows older than 30 days or past the newest 500.
- **Private questions are stored when the call fails**, flagged `private`: a failure cannot
  be read without its question, and the operator decided that a private question is private
  from the channel, not from them. An answered private question is still never stored, and
  `Audience::record` still hides the store from the answering path. The Discord adapter
  reaches `record_failure` directly, which is the one exception.
- **`/forget` anonymizes.** A Discord failure stores the asker's user id, so `/forget` can
  find the rows. It sets the id to NULL and replaces the question, error, first rejection
  and attempts, keeping the time, thread and `private` flag (`Forgotten` counts ratings
  deleted and failed calls anonymized). The HTTP API and agents have no user and store
  none. A call still running when its asker runs `/forget` can record its failure afterwards,
  with the id; `/forget` again clears it (a late rating behaves the same). Rejected:
  deleting the rows, which would also drop the failure counts; and
  storing no id, which would leave a question the asker cannot take back.
- **Read on the CLI only** (`judge-cli failures`), like `stats`, and not an MCP tool.
