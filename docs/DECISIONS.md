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
- **serenity** for Discord (slash commands, buttons and threads were all verified
  before the choice), **axum** for HTTP, **rmcp** for MCP, **SolidJS + Vite** for the
  page. These are conventional, maintained choices where nothing in the invariants pushed
  one way.
  The slash commands are the adapter's own (`discord::command`) rather than poise's: poise
  dispatches only from a live gateway connection, which an interaction received over HTTP
  (#29) does not have.
- **Workspace lints** deny `unwrap`, `expect`, indexing and `panic!` in every crate, and
  warn on the pedantic group and missing docs. I8 is a lint failure, not a review
  comment.

## D4. Entity-first hybrid retrieval

*Decided 2026-08-29.*

Retrieval is entity-first and hybrid, and the CR is chunked at two granularities. Rules
questions have the shape "card A + card B + rule concept C", so:

- **Card names are entities.** A cheap structured-output call extracts them and SQL
  resolves them through a typed resolution order: alias → possessive-stripped alias → exact →
  printed name → short name before the comma → the name after the comma (two words or
  more, and offered, not taken, when another card's name holds the same words: "Nimble
  Pilferer" is Ragavan, "Grand Arbiter" asks between Dovin and Augustin) → alias suffix →
  trigram fuzzy. A
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

Every model call is behind the spend cap by type, embeddings included. The operator is
cost-sensitive, and the model calls are the only thing that costs money per question. The
pipeline's `ChatModel` port is *sealed*. `Metered<B>` is its only implementation and
`Models`' fields are private, so there is no way to construct an uncapped model or to
meter one against a foreign budget.

Embeddings follow the same shape. The provider adapters implement the open
`judge_embed::EmbedBackend`. What `Vectors`, the retriever, the call store and the ingest
embed step take is the sealed `WithSpace`, and `MeteredEmbedder<B>` is its only
implementation. `Config` hands out only metered embedders, on the meter its models bill
to, so a process has one total and one cap whatever it calls.

The cap **reserves the worst case before sending** and settles on reported usage. That is
why caps under about $0.36 refuse synthesis outright rather than overshooting. An
embedding request reserves its texts' UTF-8 bytes plus 16 tokens each, since no tokenizer
the providers use makes a token of less than a byte, and settles on the provider's
`usage` (at the reservation when a server reports none). The bound can be beaten by
fractions of a cent, by a server-side instruction prompt on a very short text or a
tokenizer that normalises text into more pieces than bytes; settlement records the real
cost either way. Embedding requests and refusals are counted apart from chat calls, so
the "questions are being refused" alert and a run's call count mean what they did. A refusal leaves retrieval
without its vector source and a call stored without a vector for `ingest embed` to fill.
It never fails a question by itself.

Pricing is a closed sum:

- the built-in table for Anthropic models, re-read for the model the *response* names,
  since a fallback may route elsewhere, and Voyage's list price for embedding models,
- an operator's per-token rate,
- `free`.

The table lists only current models: an upgrade replaces its predecessor's row. An
unknown Anthropic model, a refusal fallback's included, prices as the default, the dearest
in the table. A dearer one (Fable, say) is therefore under-counted: a stage that names one
needs an operator's rate, and a fallback that lands on one settles low. That is accepted,
because a fallback happens only on a safety refusal.
An unknown Voyage model prices as the dearest listed. Voyage's free allowance is per
account and invisible here, so the cap prices from the first token. An unpriced model on
an OpenAI-compatible provider, chat or embedding, is a startup error, because it could be
anything.

Dollars stay per process rather than per user or per server. The meter settles after the
call, so a finer-grained dollar cap would either over-reserve or overshoot. Question-count
limits are the tool for finer grain, and the MCP transport, the web app and the Discord
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

*Revised 2026-10-20:* a rating that finds fault now carries a reason for the operator
(D38). The reason never reaches retrieval, and neither does a web rating.

*Revised 2026-10-21:* a judge's low rating excludes a call at once. The five-vote floor
had applied to it too, so a call only a judge had rated Incorrect stayed an example.

Ratings shape retrieval and nothing else. Answers can be rated 1–3 on Discord. A rating's
score changes one thing: which prior calls are shown to the model as *examples* for a
similar question.

- Scores are a Bayesian mean (prior 2.0, weight 3), so one early vote cannot swing a
  call's standing.
- A member holding the operator's judge role overrides the crowd (`effective_score`).
- Calls below 1.5 are excluded: on the crowd's score, once it has at least five votes; on
  a judge's, at once. The five-vote floor guards against early crowd noise, and a judge's
  rating is not noise.
- Only Discord ratings count. `calls_rated` reads `rater_kind = 'discord'`, so a rating
  with no identity behind it (a web browser session's, D38) never moves a call's
  standing: it is noise.
- A rating's reason (a category, what is wrong, the right answer) goes to the operator's
  feedback queue and nowhere else. The model never reads a rater's words.

Prior calls are always rendered *after* the CR material and labelled as precedent, never
authority. The CR outranks anything the community has said. A Discord call records who
asked (D31). `/forget` deletes a user's ratings with their reasons and feedback-queue
items, and removes them from their calls and failed calls.

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
   (`eval/gold.yaml`, 22 today, one of them a human correction). Per-question
   *equivalence lists* make the metric track correctness rather than one author's
   citation taste.
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

## D15. Single self-hosted machine

*Decided 2026-08-31.*

The reference deployment is one machine at home, running a CI-built image, with no
cloud compute bill. One machine keeps two properties a serverless split would lose. The
Discord gateway stays one long-lived connection, with no HTTP-interactions rewrite. The
spend cap, semaphore and rate limiter stay single-process values.

It was first published only through a Cloudflare Tunnel: no public IP, no forwarded
port. D41 made the bundled HTTPS site the default route and the tunnel the
alternative; what follows holds for both.

CI publishes the image and the host pulls it, because a release build wants about 4 GB of
RAM and real CPU, which a NAS does not have. The data refresh runs inside the `judgebot`
process (its `--jobs` role, D25) on a schedule kept in the database (D24). The one-shot
`refresh` container stays for the first load and for manual runs. The weekly backup is
an opt-in container of its own (D26).

**Rate limiting buckets on an address the caller cannot choose.** `API_CLIENT_IP` is
`peer` or `cloudflare` (`CF-Connecting-IP`), and since D41 `proxy`, never
`X-Forwarded-For`. Cloudflare *appends*
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
- An instance's anonymous web app is a second public interface to the instance its operator
  runs. It is public in the sense that it needs no login. It is not a shared service
  other communities are meant to depend on.
- One bot in several servers you administer works today. What is shared between them is
  the spend cap and the judge role name, by design.

## D17. Non-goals

*Revised 2026-10-20:* the web app rates answers now (D38), still without accounts; its
ratings never shape retrieval.

Tournament policy (MTR/IPG). Accounts on the web app, or web ratings that shape
retrieval. Streaming responses.
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
where an operator sees what a day cost. The ledger records embedding spend with the chat
spend, combined in one figure. `judgebot ingest` writes it too, so a `reembed` counts
toward the period and shows in the stats. `--jobs` alone runs it for its refresh.

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

It can also warn before then (`JUDGE_SPEND_WARN_AT`, percentages of the cap, unset by
default), once per threshold per period. In a shared period every process sees the same
total, so a process claims a warning by inserting a `spend_warnings` row keyed by the
period, the cap and the percentage, and only the one whose insert lands sends it. With
the cap in the key, a cap raised mid-period, or an ingest run under a raised one, warns
at its own percentages. A claim lands before
the post, so a webhook that fails loses that warning (it is still logged) rather than
every process repeating it.

**Rejected:** *warning from the in-memory meter alone*, which sends one warning per
process (`judgebot ingest` beside the bot is two) per threshold.

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
  does not belong among gates that are otherwise exact. It belongs in `judge-eval` as a
  score, not in the path of every answer, and D28 put it there.
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
checked for its length and its rule numbers. Discord and the web app showed the
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

The data refresh (Scryfall, a new CR release, the built-in curated lists, retirement,
embeddings, emoji) needs to run
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
lease indefinitely. A run stopped by a schema change, or by its process shutting down
(D40), records neither success nor failure. A run whose process died is counted as failed once it is older than any live
run can be, so a crash loop backs off and alerts once.

**A scheduled run never pays for a mass re-embed.** Embeddings are behind the spend cap
(D7), billed to the process's meter, but a cap sized for questions would still let a
timer spend most of it on vectors. A scheduled run counts the rows waiting for
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
`--discord`, `--api`, `--web`, `--mcp` and `--jobs` (D37 adds `--admin`), from the command
line or, with none there, `JUDGE_ROLES`. `judgebot ingest <command>` is the data command line.

- **The set cannot be empty.** `Roles` holds a `NonEmpty<Role>`, as `Interfaces` does
  for the API's interfaces, so a process that serves nothing is not a value the program
  can hold.
- **Requirements are checked before anything connects.** `roles::plan` matches every
  role exhaustively and returns the types the roles run on, which only their checks can
  make: `--discord` needs `DISCORD_TOKEN` and `JUDGE_OPERATOR_DISCORD`, the network roles
  `JUDGE_OPERATOR_EMAIL` (and `--web` a built web app, `--mcp` an `MCP_TOKEN`), and any
  serving role a model that builds. Every unmet requirement is reported at once. A role
  added later cannot start without saying what it needs.
- **One process, one composition.** One pool, one configuration, one set of models
  behind one spend meter, one `Vectors`, the schema migrated once. The first serving
  role to stop ends the process, non-zero, so the restart policy brings all of it back.
- **The old names are gone.** The image carries `judgebot` alone, as its entrypoint.
  The upgrade to it is a major release, so a compose file written for two services is
  replaced with the image rather than kept running. `API_INTERFACES`, which chose the old
  `api` service's interfaces, is refused at startup with the `JUDGE_ROLES` line that
  replaces it, rather than ignored: ignoring it would drop the interfaces it named.

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

- **One failure domain.** A crash or a deploy takes the bot and the web app down together.
  The restart policy brings both back, and the HTTP listener is bound before the gateway
  is contacted, so a taken port fails before the bot logs in.
- **One spend meter.** With `JUDGE_BUDGET_PERIOD=process` the bot and the web app now share
  one `JUDGE_MAX_USD`, where the two processes had one each. `day` and `month` were
  already shared (D19).
- **The backup and `judge-config` stay separate containers.** The backup reads the object
  store's credentials from `.env.deploy`, which must stay out of the internet-facing process
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
- **Several replicas are not a goal** (D29). Safe is not supported: the per-user and
  per-IP limits are per process. An instance runs one `judgebot`, and a second is a
  deploy's overlap or a standby. A "did you mean?" pick
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
  or Discord keys, and the internet-facing `judgebot` still holds no bucket keys.
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
- *A role of `judgebot`.* The bucket's keys can delete every backup; they would sit in the
  internet-facing process.

**The S3 client is four signed requests.** List (`ListObjectsV2`), put, get and delete,
signed with `aws-sigv4` and sent with reqwest, the response XML read with `roxmltree`.
All three were already in the dependency graph (the Anthropic AWS endpoints, resvg), so
the change adds no crate that is built (the lockfile gains a wasm-only `wasm-streams`
entry, from reqwest's `stream` feature). An upload is one `PUT` streamed from the file, with its SHA-256
signed in `x-amz-content-sha256`, so the store refuses bytes that differ from the dump.

*Rejected:*

- *`aws-sdk-s3`.* The standard client, but a very large generated crate for four calls,
  and its default request checksums have needed configuration to suit R2 and other
  S3-compatible stores.
- *`object_store`.* A good abstraction, but it brings its own reqwest and TLS stack
  beside ours.
- *Keeping `rclone` in a container.* That needs the Docker socket, which is root on the
  host, handed to a container.

**No healthcheck.** What matters is the age of the newest backup, which is in the
bucket, and a failure goes to the webhook rather than into a status nobody reads on an
unattended host.

**Any S3-compatible store** (amended 2026-10-20). The settings were Cloudflare R2's
(`R2_ENDPOINT`, `R2_BUCKET`, `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`), with the
signing region fixed at R2's `auto` and the bucket always in the path. Nothing else in
the client was R2's: the four requests send no checksum header but `SigV4`'s own and
list with `ListObjectsV2`, which AWS S3, Backblaze B2 and MinIO take as R2 does. So the
store became configuration, with R2 one documented provider:

- **`BACKUP_S3_*`.** `BACKUP_S3_ENDPOINT`, `BACKUP_S3_BUCKET`,
  `BACKUP_S3_ACCESS_KEY_ID` and `BACKUP_S3_SECRET_ACCESS_KEY` replace the four `R2_*`
  one for one. `BACKUP_S3_REGION` (a `Region`, default `auto`) is what requests are
  signed for, and `BACKUP_S3_ADDRESSING` (`Addressing::{Path, VirtualHosted}`, default
  `path`) where the bucket goes. Path-style is the default because R2, B2, MinIO and AWS
  all take it today. AWS recommends virtual-hosted, so the runbook gives AWS
  `virtual-hosted`. `Addressing::check` refuses at load the two
  settings virtual-hosted cannot reach: an IP-address endpoint, and over `https` a
  bucket with dots, which the store's wildcard certificate does not cover.
- **The old names are refused, not aliased.** A set `R2_*` is a `Problem` naming the
  line that replaces it (or, when that is set too, saying to remove it), never its
  value, and its successor is not reported missing on top. `scripts/backup-db.sh`
  refuses them the same way, and `scripts/deploy.sh` before anything changes, on every
  deploy (`export` lines included). The scripts also refuse a `BACKUP_S3_*` set twice
  in `.env.deploy`, since a pasted blank example line after a renamed one would unset
  it; `judgebot` sees only the environment, so it cannot tell.
- **A refusal is heard, once.** Settings that are not usable stop `run` and `serve`
  before anything else, and with them the alert webhook they hold. So
  `settings::alert_from_env` reads `JUDGE_ALERT_WEBHOOK` (and `JUDGE_LOCALE`) on its
  own, and the variables' names, never their values, are posted
  (`Messages::backup_unusable`). `serve` then stays up, refusing and logging hourly,
  until stopped: exiting would have `restart: unless-stopped` restart it with the
  environment it was created with, alerting again on every restart (Docker's backoff
  stops at a minute) and fixing nothing. The fix is an edited `.env.deploy` and
  `up -d`, which recreates it.
- **The deploy checks the backup too.** `scripts/deploy.sh` recreates only `judgebot`,
  so a `backup` service would keep its old image and environment until some later
  `up -d`, where a refusal goes unwatched. With the `backup` profile on, the deploy runs
  the new image's `judgebot backup check` (settings and `pg_dump`, nothing sent) before
  it changes anything, and ends with `up -d --no-deps backup`. The service holds no
  state but the bucket, and an upload cut short leaves no object. The object names and prefix are unchanged, so a bucket's backups
  keep listing, fetching and setting the schedule.
- **The script follows.** It passes the region and addressing to rclone
  (`force_path_style`), and picks rclone's provider from the endpoint (`Cloudflare`,
  `AWS`, else `Other`), which tunes only rclone's own quirks.

*Rejected:*

- *Reading `R2_*` as aliases.* Two names for one setting, with a precedence to learn,
  kept for good; the instances that use them are few and the refusal names the rename.
- *A required region.* An R2 or MinIO setup would need a line that says nothing. A
  store that checks the region refuses a wrong one on the first request, and AWS's
  refusal names the region it expects.
- *Deriving the region or addressing from the endpoint's host.* A guess that holds for
  the providers named today and fails silently for the next.
- *Virtual-hosted by default.* MinIO on the compose network (`http://minio:9000`) and
  any endpoint given as an IP address cannot take it.
- *Exiting on a refusal and alerting from a marker.* No state survives a restart but
  the bucket, which the settings at fault may be what reaches; a marker file needs a
  volume of its own for one alert.

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
- **Private questions are stored**, flagged `private`: a failure cannot be read without
  its question, and the operator decided that a private question is private from the
  channel, not from them. An answered private question is stored too (D31).
- **`/forget` anonymizes.** A Discord failure stores the asker's user id and username
  (`asker_id`, `asker_name`), so `/forget` can find the rows. It sets both to NULL and
  replaces the question, error, first rejection and attempts, keeping the time, thread,
  interface and `private` flag (`Forgotten` counts what it deleted and anonymized). The
  HTTP API and agents have no user and store none. A call still running when its asker runs `/forget` can record its failure afterwards,
  with the id; `/forget` again clears it (a late rating behaves the same). Rejected:
  deleting the rows, which would also drop the failure counts; and
  storing no id, which would leave a question the asker cannot take back.
- **Read by the operator only**, like `stats`: `judge-cli failures`, stdio `judge-mcp`'s
  `admin_failures`, the admin pages, and `/mcp` with `MCP_ADMIN_TOKEN` (D37). Never with
  `MCP_TOKEN`, which is handed out for judging.

## D28. Model-graded eval score

*Decided 2026-10-10.*

Citation recall says whether an answer cites the rules its reference rests on, not whether
the answer is right. An answer can cite the right rules and still reach the wrong ruling:
a reported Academy Manufactor answer cited 614.1a correctly and kept a token the
replacement removes. The README's agreement and grounding columns were graded by hand,
with subagents.

`judge-eval grade <run.json>` now asks a model for that reading, and `answer --grade`
runs it after a run. For each verdict on an answerable question it sees the question, the
gold reference, the answer and each citation with its quote, and writes:

- each claim of the answer, marked supported by the quoted words alone, unsupported, or
  contradicted;
- whether the ruling follows from the quotes (`follows`), rests on a step no quote states
  (`gap`), or is not supported by them (`unfounded`);
- whether the conclusion agrees with the reference (`agrees`, `partial`, `disagrees`);
- the wrong remarks, side remarks included.

The grade is stored on the row with the grader's `provider/model`, its cost and the
reference it was made against. It is written back after each answer, so a pass the spend
cap stops resumes. A grade whose reference the gold file has since changed is stale:
`grade` makes it again and `show`/`rescore` leave it out. A failed grade or a stopped pass
exits non-zero. Grading the 18 answers of the published Opus run costs about $0.40.

- **A score, never a gate.** It is a model's opinion, so it stays out of the answer path
  (D20) and out of the exit code's meaning for answers: `grade` fails when grading fails,
  not when an answer is graded down.
- **The grader is the configured synthesis model** (`--config` names another). Grading
  Opus's answers with Opus is grading with the same blind spots, so a different grader
  is worth a run before a result is published.
- **Against the hand grading.** The published Opus run, graded by hand at 18 agreeing and
  14 following from their quotes, grades at 18 and 11. The gaps it names are the ones the
  hand grading listed (Doubling Season's own text, the definition of "dies") and a few it
  passed over (Humility's own text, priority while casting). A card's name, mana cost,
  colors and type line are taken as given, because the material prints them on a label
  line that cannot be quoted. Read the column between runs with one grader, not as a
  measure on its own.
- Rejected: *scoring answer text against required phrases* in `gold.yaml`. It is exact,
  but a correct answer worded differently fails, and every question would need phrases
  written for it.

## D29. Blue-green deploys

*Decided 2026-10-13.*

A deploy starts the new `judgebot` container beside the running one, waits until Docker
reports it healthy, then stops the old one (`scripts/deploy.sh`). The two overlap for a
minute or two. Running several instances all the time is not a goal: one community's
load does not need it (D16), and the per-user and per-address limits stay per process.

- **SIGTERM drains** (`judge_bot::shutdown`). `docker stop` asks; it no longer kills.
  The process takes no new work: the HTTP listener closes and the Discord role closes
  its gateway connection. It then finishes what is in flight within two minutes
  (`DRAIN_LIMIT`), writes the spend ledger once more and exits 0. Each Discord event
  holds a `Work` guard for as long as serenity's task runs its handler, and an answer
  already deferred is sent over the REST API, which needs no gateway. The compose
  `stop_grace_period` (130 s) is the limit plus ten seconds, a relation a test holds,
  and the last ledger write is bounded by five. A signal during startup, before the
  roles run, or a second signal exits at once.
- **The gateway is handed over at once.** A holder that is shutting down releases the
  lease as soon as its connection has closed, not when the process exits. A connection
  closed within a second is recorded as a clean release (`gateway_handover`), and a
  standby that takes the lease within 5 s of it connects without the grace (15 s), which
  covers only a holder that may still be connected. Every holder clears the record as it
  takes the lease, before it connects, so the record speaks of the last holder only. A
  holder that never wrote it (an older release) connects no sooner than the grace after
  taking the lease, longer than the 5 s window (a `const` relation), so a stale release
  never skips the grace. A gateway slow to close keeps
  the lease until it has closed, so nothing else connects while it may be open. One
  still open at the drain's limit lets it go then, and the process is gone within the
  five-second ledger bound, inside the 15 s a standby waits (a `const` relation). A standby that is shutting down stops waiting.
- **A proxy holds the host port.** Two containers of one service cannot share a name or
  a published port, so `judgebot` has neither. The `proxy` service (Caddy) publishes
  `127.0.0.1:8787` and forwards to `judgebot:8787`, and the tunnel goes through it
  (`proxy:8787`, or the `api` alias). During the overlap the name resolves to both
  containers. One that has stopped listening refuses the connection, and the dial goes
  on to the other address. The proxy dials afresh for every request: a kept-alive
  connection to the draining container can be closed under a request already written,
  which a POST cannot repeat (a drill with two backends swapped under load lost 4 of
  12 000 requests with keep-alive). What remains: closing the listener resets a
  connection the kernel had queued for the old container but the process had not
  accepted. A drill of real deploys under about 2 000 requests a second hit it twice in
  17 000; at a community's traffic it is unlikely. Caddy cannot save such a request,
  because it resends no POST body, and pointing the proxy at the new container alone
  first (a configuration reload) reset more connections than it saved. Four deploys
  under steady load through the compose network, the tunnel's path, lost none of 5 000.
- **Migrations keep the previous release working.** The new container migrates as it
  starts, while the old one still answers, so a release's migrations must leave the
  previous release's queries valid: expand now, contract a release later. It applies
  from the next migration on. Those already on `main` since 1.2.0 add tables only, so
  they pass it. `CONTRIBUTING.md` lists what passes it.

The reasons:

- **A deploy was an outage.** Compose stops the old container before starting the new
  one, so every deploy cost the startup and the migration, and an answer in progress
  died with the process. PID 1 ignores a SIGTERM it has no handler for, so `docker stop`
  waited its ten seconds and then sent SIGKILL.
- **Most of it existed.** The refresh lease, the gateway lease, picks answered from their
  message and the schema-skew checks (#16) already made a second process safe. What was
  missing was letting go cleanly and room in the compose file for two containers.

**Rejected:**

- *Several replicas behind a load balancer.* Rate limits and the spend overshoot would
  then need to live in the database, for throughput nobody needs.
- *A CI job that runs the previous release against the new schema.* It would catch a
  broken expand step, but needs the previous image and a populated database in CI. A
  written rule and a review are proportionate to a migration every few weeks.
- *A port range* (`127.0.0.1:8787-8788`) instead of a proxy. Each copy takes the first
  free port, so the address moves with every deploy and a host's reverse proxy must
  list both.
- *Discord's HTTP interactions endpoint* in place of the gateway. It would remove the
  handover, but needs a public URL for every Discord deployment. A gap of 15 to 25
  seconds per deploy is accepted. D30 later added it as an option beside the gateway.

What follows:

- **Discord is quiet for a login** (a few seconds), or for the grace plus a login
  (about 15 to 25 s) when the old container did not record a clean release. An
  interaction in that gap gets Discord's "did not respond". Receiving interactions over
  HTTP instead of the gateway closes it (D30). The web app and the API see no gap.
- **Limits reset once per deploy.** The per-user `/judge` window and the per-address
  limit are in memory, so the new container starts them empty, as a restart does.
- **Spend overlaps by one sync.** Both containers bill one period through the ledger,
  which syncs every 10 s (D19).
- **A refresh in progress is stopped with the old container** (D40). The step in
  progress is cut off, the run is recorded as stopped (no alert, no backoff), and the
  new container's first check runs the refresh again.
- **A migration that cannot keep the previous release working** (a rewrite of rows the
  old binary writes) is deployed with `docker compose up -d`, stop then start, and the
  CHANGELOG says so.

## D30. Discord interactions over HTTP

*Decided 2026-10-17.*

`DISCORD_INTERACTIONS=http` has Discord POST each slash command and button click to the
application's Interactions Endpoint URL, which reaches `/discord/interactions` on the
process's HTTP listener (`judge_bot::discord::endpoint`), instead of sending it over a
gateway connection. `gateway` stays the default. Discord delivers an application's
interactions one way or the other, never both, so the setting has to agree with the
Developer Portal.

- **No gateway, no lease.** In `http` mode the Discord role opens no gateway
  connection and takes no gateway lease. It reads the application's id and public key
  over the REST API (`GET /applications/@me`), registers the commands, starts the
  symbol watcher and opens the endpoint. Every response already went over REST; only
  the delivery of the interaction changes. An HTTP-only bot is listed online in the
  member list with no status dot.
- **One dispatch for both routes.** The commands are the adapter's own
  (`discord::command`, `Data::dispatch`) rather than poise's, whose dispatch needs a
  live gateway connection. The gateway feeds the same dispatch from serenity's event
  handler.
- **Verify, then answer.** A request without a valid Ed25519 signature over the
  timestamp and the body, or signed more than five minutes from the host's clock,
  gets `401` before anything else is read. Discord sends invalid
  ones on purpose and removes the URL from an application that accepts one. A PING is
  answered in the body. Anything else runs on a task of its own, counted as in-flight
  work for the drain, and the request is answered `202` with no body once the handler
  has sent its initial response through the REST callback, as Discord documents for an
  interaction answered that way. That is bounded by 2.8 s from the request's arrival,
  under Discord's 3 s.
- **Through the proxy.** Discord's requests reach the endpoint through the tunnel and
  the `proxy` service, which dials a container afresh for every request (D29), so a
  request sent while a deploy swaps containers lands on whichever is listening. The
  old container's endpoint keeps answering what reaches it while it drains, and the
  Discord role drains its handlers only after the listener has answered every
  request it accepted.
- **A closed endpoint takes no requests.** Until the role has the public key, the
  listener has its port but does not listen, so the proxy's dial is refused and goes
  on to the old container, which can verify them. It listens once the endpoint opens,
  or after 30 s so a Discord API that does not answer cannot keep the web app down; a
  request that still finds it closed waits up to half a second, then gets `503`.
  `/api/health` fails while it is closed, so a deploy does not switch to it.
- **A listener either way.** With a network role the endpoint is mounted on its
  listener; `--discord` alone binds `API_ADDR` for the endpoint and `/api/health`.
- **Outside the per-address limit.** Every request it acts on is signed by Discord, so
  the anonymous API's per-IP limit does not apply. The per-user `/judge` window does,
  as over the gateway.
- **A mismatch is reported, not refused.** At startup the role compares the setting
  with the application's Interactions Endpoint URL and logs an ERROR (and posts to the
  alert webhook) when they disagree. Refusing would make the switch impossible:
  Discord saves a URL only after its endpoint has answered, so an `http` instance must
  be running before the URL can be set. It would also take down the web app in the
  same process.

The reasons:

- **A deploy lost Discord for a login.** Over the gateway, interactions sent while the
  connection changes hands get "did not respond". Over HTTP they arrive through the
  tunnel and the proxy like any other request, and the listener's drain already
  finishes what is in flight.
- **Discord recommends it.** Its documentation describes the callback endpoint as the
  way to respond, and an HTTP endpoint needs no long-lived connection or IDENTIFY
  budget.

**Rejected:**

- *Choosing the mode from the application's URL alone.* It needs no setting, but makes
  the process's behaviour a consequence of state outside it, and a URL saved by
  mistake would silently close the gateway path.
- *Answering inline* (the initial response as the HTTP response body). It saves one
  REST call, but a handler's error on that response would be invisible, and every
  handler would need a second code path. The callback keeps the code identical for
  both routes.
- *Keeping the gateway as a fallback in `http` mode* until the URL is saved. It would
  make the switch seamless, but needs a second way to start and stop the gateway for a
  one-time step.

What follows:

- **Switching modes costs a short gap.** Deploy with `http`, then save the URL in the
  Developer Portal; interactions in between still go to the gateway, which nobody
  holds. Switching back is the reverse.
- **It needs a public HTTPS address.** A Discord-only deployment with no tunnel stays
  on the gateway.
- **A replay is bounded.** Discord gives no timestamp freshness window; the endpoint
  refuses a signed timestamp more than five minutes from the host's clock, which bounds
  a replay and tolerates a drifted clock. Within the window a replayed request's token
  has been used, so its handler's first call to Discord fails and no model is called,
  but what comes before that call happens again (a `/judge` charges the asker's
  window). A host clock five minutes wrong refuses every interaction; the first
  refusal of a streak is an ERROR and an alert.

## D31. Call origin and private calls

*Decided 2026-10-18.*

Every stored call and failed call records where it came from (`judge_core::origin`): the
interface it was asked through, the Discord user who asked, and whether it is private.
The public feed, the feedback queue and the dashboard read these.

- **The interface is stated, not guessed.** `calls.interface` is `discord`, `web`,
  `mcp`, `cli` or `agent_session`. Each interface passes its own `Origin` to
  `CallStore::persist` and `record_failure`, so an interface cannot store a call without
  naming itself. Rows from before 2.0.0 were labelled from the thread id. An `agent:`
  thread with no session keeps the value `agent`, because the MCP `judge` tool and
  `judge-cli judge` could not be told apart. The column is `interface` because
  `calls.source` already means `cr` or `commander`. Rejected: reading the thread id's
  prefix, which cannot separate MCP from the CLI.
- **The asker's username.** A Discord call stores the user id and the unique username,
  snapshotted when asked, so a later rename does not change who asked. The display name
  and the server nickname are not stored. A value that is not a username by Discord's
  rule (a legacy `name#1234` tag) is stored as NULL and shown as anonymous. Web and agent
  calls have no asker.
- **Private means not public-facing.** A `/judge private:True` call, and a `judge` or
  `begin_session` call with `private`, is stored with its text and asker. On Discord it
  gets rating buttons. Only the operator's surfaces show it. It is never a prior-call
  example and is never embedded. Rejected: leaving answered private calls unstored, which
  leaves the operator unable to read an answer someone rated wrong.
- **History by scope** (`Origin::history_scope`). A private Discord question reads no
  history, and a public question in any thread reads only public calls. A Discord
  channel is shared, so one user's private calls must never reach another's answer. An
  agent thread (`agent:<uuid>`) is known only to the caller it was returned to, so a
  private agent question reads its own thread whole.
- **`/forget`.** It deletes the user's ratings and removes their id and username from
  their calls. On their private calls it also replaces the question and answer with
  `[removed]` and clears the citations and context ids, which would say what was asked
  about. A public call keeps its text, which the channel saw. It holds the shared side of
  `CALLS_REWRITE_LOCK`, so a CR renumbering cannot write a removed answer back from an
  earlier read. A question still running when its asker runs `/forget` is stored
  afterwards with the asker, as a late failure or rating is (D27); `/forget` again
  clears it. Rejected: deleting the calls, which would also remove answers other people
  rated and the counts the dashboard shows.
- **The web has no private option.** It has no account to show a private answer to. A
  database constraint pins that, and another keeps an asker on Discord calls only.

## D32. Major releases and maintenance

*Decided 2026-10-18.*

A major release may change the database in ways the previous release cannot run
against. Deploying one is still one ordinary deploy, with a short downtime while it
migrates, during which questions get a maintenance reply rather than an error.

- **Within a major, migrations only add** (CONTRIBUTING). A new table, a nullable
  column, an index: the previous release keeps working beside the new one (D29), an
  older `judge-cli` or `judge-mcp` keeps working against the new schema, and rolling back
  to an older release of the major works. Dropping, renaming and rewriting rows wait for
  the next major, made during its `X.0.0-dev` cycle.
- **Versions say which is which.** `main` carries `X.Y.Z-dev` between releases, and each
  migration run records the release and commit that ran it (`instance_release`). A new
  major's pre-release (`X.0.0-*`) works only with the very build that migrated the
  database; any other pre-release orders among its major's releases
  (`judge_core::release`). The version comes from the tree, not from the tag, because a
  release retags the image CI already built and tested; the release check holds the tag
  to the tree's version.
- **The tools check the instance's release** (`judge_agent::release`). Same major,
  newer instance: a note, and they work. Anything else: every operation fails, naming
  what to update and the instance's commit. The check is a type: the ports are reached
  only through it.
- **`judgebot` refuses a rollback across a major.** Unknown migrations within a major
  only added, so an older release starts; across a major, or between builds of a new
  major's pre-release, it exits naming both builds. The decision is made under the
  migration lock, so it cannot race another process's migration.
- **Maintenance while a major migrates.** Every `judgebot` process with a role (not a
  one-shot `judgebot ingest` or `judgebot backup`) holds the shared side of an advisory
  lock (`SCHEMA_LOCK`) for its life, taken before it looks at the schema, and checks
  every 10 s that it still does: one lost (a Postgres restart) is taken again only
  while the schema is still its own, or the process exits. A major upgrade starts its
  roles closed,
  takes the exclusive side once every older process has exited, migrates, and opens.
  `judgebot ingest migrate` waits the same way. A one-shot `judgebot ingest` step of
  the previous release that is running stops at its next step, which checks the
  ledger.
  Closed, every question is answered with the maintenance reply and runs nothing: a
  Retry button on Discord, read back from the message as a "did you mean?" pick is, so
  nothing is held in memory; a `maintenance` reply with `Retry-After` on the web and the
  API; the message as the error over `/mcp`. `/api/health` fails, so nothing routes to
  the process as if it were up. The process's own background work does read the
  previous schema meanwhile (the spend ledger, the data's freshness, the gateway lease
  and its handover record) and logs rather than fails when it cannot, so a major that
  changes those tables keeps them readable until it has migrated.
- **`scripts/deploy.sh` decides the path.** It asks the new image what its migrations
  are (`judgebot ingest upgrade`). Within a major: blue-green, unchanged. A major: a
  backup when one is configured, the new container beside the old one, the old one
  stopped as soon as the new one listens, then the wait for the migration. From 1.x,
  whose processes hold no schema lock, the old container is stopped first.

**Rejected:**

- *Expand and contract across every release, majors included.* It keeps every deploy
  free of downtime, but a breaking change takes two releases and leaves a compatibility
  shape behind in between. A short, announced downtime once per major costs less.
- *A special command for a major's deploy.* Operators would have to know which kind of
  release they are deploying. The new process decides from the database instead.
- *Rebuilding the image on a tag, so the binary can read its tag.* The release would no
  longer be the image that was tested.
- *Refusing every question with an error while migrating.* An error reads as a fault;
  the maintenance reply says what is happening and how to ask again.


## D33. Per-run usage records

*Decided 2026-10-18.*

Every run of the built-in pipeline leaves one `judge_runs` row, whatever its outcome or
audience, with one `judge_run_charges` row per billed model call or embedding. The
dashboard and the admin surfaces read cost, models and outcomes from them.

- **An explicit tally per run.** `judge_core::Tally` is made by `judge_bot::run::run`
  and passed to every port that can spend: `Extractor`, `Retriever`, `Synthesizer`,
  `Embedder` and `CallStore::persist`. `ChatModel::complete` takes a `Bill` (a tally
  and a `Stage`), so a model call cannot be made without saying whose it is. `Metered`
  and `MeteredEmbedder` add the charge when they settle with the shared `SpendMeter`,
  so the cap is unchanged. Rejected: the shared meter's total before and after a run,
  which concurrent runs bleed into, and task-local state, which a spawned task or a
  forgotten scope loses without a compile error.
- **A missing usage is a flagged worst case.** A response that reports no usage was
  still billed. The cap keeps the reservation it took, and the charge records that
  amount as `Cost::WorstCase` (`cost_estimated`), with no tokens. A sum with an
  estimate in it is itself an estimate. The run's figure then matches what the cap
  counted and still adds up. Rejected: NULL, which turns every total it touches
  unknown, and 0, which under-counts. A chat response with no usage, all-zero usage
  from a paid model, or a 2xx body nothing could be read from used to settle at 0
  against the cap. Each now keeps the reservation, as an embedding already did.
- **The stage is named by the caller.** Extraction bills `extraction`. The synthesis
  typestate bills `tool_round` for the request carrying the `lookup_rules` result and
  `synthesis` for every other request, so "the tool round ran" is a fact about billed
  requests. The model is the one the response named, which differs from the configured
  one under refusal fallbacks.
- **The run outlives its call.** A run links its call or failed call with `ON DELETE
  SET NULL`, so retention and the failed-call bound leave spend and outcome figures
  intact. A spend-cap refusal marks the tally, so the outcome is `at_cap` rather than
  `upstream`.
- **One flow for every interface.** Discord, the HTTP API and the agent toolbox's
  `judge` call `run::run`, which judges, stores the call or the failure, and records
  the run. The run is a task of its own holding the caller's judge slot, so a request
  that goes away (a closed tab, a cancelled MCP call) does not drop it half-way. The
  record is written after the result is handed back, so it never delays a reply.
  `judge_traced` returns the context the answer was judged from, lookup round
  included, so persisting no longer needs a retriever decorator keyed by question
  text.
- **A caller that leaves stops the run at the next model call.** The run's task marks
  the tally abandoned when the caller's end of the result channel closes: the HTTP
  handler's future is dropped with its connection, and the MCP `judge` tool drops its
  future when rmcp cancels the request's token (`notifications/cancelled`, or the
  session ending), since rmcp itself keeps a cancelled handler running. The request
  in flight finishes, since a non-streamed request is likely billed whether or not
  anyone reads it, and is charged at its real cost. `Metered` sends nothing more
  (`LlmError::Abandoned`), the run ends `abandoned`, and it is not a failed call. An
  answer already written is still stored. The slot is held until the run's result is
  ready (its persist included). `retried` stays true for a run abandoned at its retry,
  which then has no second synthesis charge. Rejected: dropping the run with the request, which spent the same on the
  request in flight, recorded none of it and threw away an answer already paid for; and
  finishing the run regardless, which pays for synthesis or a retry nobody reads.
- **Not recorded.** An agent-driven session has no run row: its model is the agent, and
  its steps span processes. Ingest, search and eval runs pass their own tallies and
  read or drop them.

## D34. Call retention

*Decided 2026-10-18.*

An operator can bound how long answered calls are kept: `JUDGE_CALL_RETENTION_DAYS`,
unset or `0` to keep every call (the default), 1 to 3650 otherwise.

- **Opt-in.** Kept calls are the prior-call examples and the thread history, so deleting
  them makes answers worse. That is the operator's trade to make, not a default. The
  docs suggest 90 days. Rejected: a default retention, which would quietly delete an
  instance's best examples after an upgrade.
- **A refresh step.** `prune` runs in every refresh, after `lists` and before `retire`,
  so the pass does not check calls about to go. It is also `judgebot ingest prune`.
  Under the refresh lease like every data-writing step, so a scheduled run, a cron run
  and a manual one take turns, and the schema check before each step applies. With the
  schedule off, the operator crons either. Rejected: a separate scheduler task, which
  would need its own lease, skew check and record for a daily `DELETE`.
- **What goes and what stays.** A call goes with its ratings (`ON DELETE CASCADE`). Its
  `judge_runs` row stays, with `call_id` set NULL (D33), so cost and outcome records
  cover the whole history; counts read from `calls` (`judge-cli stats` today) cover
  only what is kept. `failed_calls` keeps its own bound (D27). Deletion runs in batches
  of 1000, oldest first, on an index of `created_at`, until a batch finds nothing. A
  call locked by a writer in progress is skipped (`SKIP LOCKED`) and goes next time. A
  batch that deadlocks with `/forget` (ratings then calls, the cascade's opposite
  order) is tried again. A rating of a deleted call is `Rated::CallGone`, answered as
  such rather than as a failure to retry.
- **Citations of a deleted call hold.** A `prior_call` citation quotes what an answer
  said, checked when the citing call was stored. The retirement pass already declines
  to cascade a retirement for that reason, so it treats a cited call that no longer
  exists as supported. A cited call that exists but cannot be read still retires its
  citers. The pass cannot tell a deleted call from one that was never stored, but no
  path stores a citation of one: persist and session validation both check it against
  stored rows. Rejected: retiring every call that cited a pruned one, which would let the
  retention retire newer, still-correct answers.

## D35. Typed message catalogs

*Decided 2026-10-18.*

The app should be usable by people who do not speak English natively. Every surface's
text therefore lives in a catalog, one per language, and each request picks its
language. Only English ships. The rules material, the model prompts, the answers and the
logs stay English: logs so they can be grepped and pasted into bug reports, the rest
until answering in other languages is taken on (#25's future work). Two pieces of text
are data rather than messages and stay as they come: a card symbol's `alt` text
(Scryfall's description of it) and the web page's static `<title>` and meta tags.

- **A catalog is a struct with a field per message**, one value per `judge_core::Locale`
  (core's own text in `judge_core::locale`, Discord's in `judge_bot::discord::catalog`,
  the HTTP API's in `judge_api::catalog`, the web app's in `web/src/messages.ts`,
  checked with `satisfies Messages`). A message with values is a function, and so are
  plurals and durations, so each language writes its own rules. A new language is a
  `Locale` variant and one value per catalog, and the compiler lists every message it
  misses. The API's catalog names the Discord catalog's pipeline replies rather than
  copying them. Rejected: string keys looked up at run time (Fluent, gettext), where a
  missing message is a fallback nobody notices.
- **Choosing the language.** The asker's first, then `JUDGE_LOCALE` (validated at
  `Config` load, default `en`): on Discord the user's locale; on the
  web the browser's languages, sent as `POST /api/judge`'s `locale`, then
  `Accept-Language`. A language with no catalog falls through. The guild's preferred
  locale is not consulted: Discord defaults it to `en-US`, so it would almost always
  hide `JUDGE_LOCALE`, the community's own choice (D16). The agent tools speak
  English whatever `JUDGE_LOCALE` says, for good: models read them
  (`judge_agent::ops::AGENT_LOCALE`).
- **Parsed-back messages carry their language.** A "did you mean?" or Retry message is
  read back by re-rendering its template, so the button's id carries the locale it was
  written in (`:<locale>`; English writes none, the shape every earlier build reads, so a
  rollback within the major or the other container mid-deploy still answers it), and the
  answer is in the asker's language even when someone else clicks. The question limits
  (`PICK_QUESTION_LIMIT`, `RETRY_QUESTION_LIMIT`) are the least over every locale's
  templates, computed at compile time.
- **Command descriptions** register the default locale's text as the base and every
  other locale's as `description_localizations`, under that locale's Discord tags.
  Command and option *names* stay English (no `name_localizations`): `/judge` is what
  the docs, `/help` and a community's conversation call it, in every language.

*Amended 2026-10-20* (#25's second half): every remaining surface, and a stub locale
that proves the structure.

- **A stub second locale, in tests only.** `judge-core`'s `stub-locale` feature adds
  `Locale::Stub` (`qps`, ISO 639's private-use range, as pseudo-locales use), and only
  `[dev-dependencies]` turn it on (`scripts/check.sh lint` refuses it anywhere else),
  so the tests negotiate it from a Discord user, a body's `locale`, `Accept-Language`,
  a terminal and `JUDGE_LOCALE`, round-trip "did you mean?" and Retry prompts in it, and
  render every message of every catalog in it, while no build that ships can choose it.
  Every catalog has a `STUB` value (each message its own name in `⟦…⟧`, with its
  values) whatever the feature, and each surface picks its catalog through
  `locale::Catalogs::new`, which takes every locale's: a new `Locale` variant makes
  every surface's call stop compiling until it names its catalog, and that catalog's
  struct literal lists the messages it misses. Rejected: a `#[cfg(test)]` variant,
  which exists only inside `judge-core`'s own tests; a variant that always exists but
  is never negotiated, which ships; and a test-only catalog type, which proves
  nothing about negotiation.
- **The web app, the admin pages and the docs site** are TypeScript: one object per
  locale in a `Record<Locale, …>` (`web/src/messages.ts`, `web/src/admin/messages.ts`,
  `site/src/i18n.ts`), so `tsc` and `astro check` list a missing catalog or message.
  They carry no stub: it would ship. The admin pages fall back to `JUDGE_LOCALE` as the
  public page does, read from `GET /admin/api/about`. The site's locales are
  Starlight's, English at the root, one list in `site/src/locales.mjs`; a
  `sync-docs.mjs` manifest entry may name a locale.
- **Command lines** speak the terminal's language (`LC_ALL`, `LC_MESSAGES`, `LANG`, the
  first one set deciding, as POSIX has it, except that `C` and `POSIX` name no language
  and pass to the next: a container's `LC_ALL=C.UTF-8` says nothing about the person),
  else `JUDGE_LOCALE`
  (`Locale::for_command_line`): their usage is catalog text (`judge_agent::catalog`,
  `judgebot`'s `catalog`). What else they print is JSON for agents and scripts, a line
  a script parses (`ingest upgrade`'s `upgrade: <kind>`), or a diagnostic (errors,
  `ingest migrate`'s progress lines), and stays as it is. `judge-eval` is a developer
  tool and stays English. The agent tools stay English, as above.
- **Alerts** are written in `JUDGE_LOCALE`: the webhook carries it
  (`AlertWebhook::locale`) and `judge_bot::alert::catalog` writes them. A check that
  logs what it alerts logs the English text. The operator's shell scripts
  (`deploy.sh`, `backup-db.sh`, `refresh-data.sh`) stay English, alerts included: their
  console output is a diagnostic, and a shell catalog would have no compiler to check
  it.
- **judge-config** has its own catalog (`judge_configure::catalog`: the checks'
  findings, `BadValue`, its replies and terminal output) and its page one JS object
  per locale (`crates/configure/ui/messages.js`). Both follow the browser's languages
  (sent in a header), then the edited `.env`'s `JUDGE_LOCALE`, the file the instance's
  language lives in (`scripts/config.sh` runs it without loading `.env`); its terminal
  output, the terminal's language first. The page is plain JS with no compiler (D23),
  so a test reads its catalog's source and holds every locale to English's keys. The schema's and `.env.example`'s help are English sources;
  a locale supplies its own per field path or variable through an override map, falling
  back to English per entry, and a test holds every key to a real field or variable.
  The loaders' refusals it shows (`ConfigError`, a role's `Problem`) are the binaries'
  startup diagnostics and stay English.
- **Diagnostics stay English** beside logs: errors, `/api/health`'s failure text,
  `/mcp`'s 401 body, and a parser's account of an unreadable request, which the API's
  `unreadable` message carries as it comes.
  `calls.locale` stays `en`: it records the answer's language, and answers are English.
  So do judge-config's parser and I/O errors and the schema's enum values.

## D36. Public feed of answered calls

*Decided 2026-10-18.*

The web app lists every answered call not asked private, from every interface, newest
first (#26). A judgebot's answers are already public where they were asked; listing them
lets a community read what the bot has said and spot wrong answers without being in the
channel. `JUDGE_FEED=false` turns it off.

- **Private is the only exclusion.** A Discord channel's visibility is not checked: the
  bot has no view of who can see a channel, and guessing would make "public" mean
  something different per server. `private: True` (or an agent call's `private`) is the
  one way to keep a call off it, and `/help` says so.
- **What is public is decided in SQL.** `judge_bot::feed` holds every public read, each
  with `publicly_visible` (not private, within the `JUDGE_FEED_DAYS` window; a SQL
  function the dashboard's counts use too, D39) in its `WHERE`, and selects the
  username only, never the asker id. A missing, private or too-old call is the same
  `None`, so a 404 says nothing about which. Failed calls live in another table.
- **Names are the username snapshotted at asking time** (D31), anonymous otherwise, and
  `/forget` clears them. Servers and channels are never shown, and a Discord mention in
  the text (`<@id>`, `<#id>`) is shown as `@…` / `#…`. A username Discord later hands to
  someone else still lists the earlier holder's calls under it: the name is what was
  shown when they were asked.
- **The window hides, retention deletes.** `JUDGE_FEED_DAYS` limits the public reads only;
  prior-call examples and the operator's tools still see older calls.
  `JUDGE_CALL_RETENTION_DAYS` (D34) is the deletion.
- **Keyset pages** on `(created_at, id)`, with partial indexes that leave private calls
  out. The cursor is opaque to clients. Rejected: offset pages, which shift as calls
  arrive.
- **Reads have their own rate limit** (120 a minute per address), apart from the
  questions' 4 per 5 minutes, so browsing never costs a question.
- **No back-dating cut-off.** Earlier public calls are listed too; the upgrade notes say
  how to keep them unlisted.

## D37. Admin role behind its own tokens

*Decided 2026-10-20.*

The operator can read everything the instance stores (private calls with their askers,
failed calls, every rating with its rater, each run's models and cost) without any of it
being reachable from the public interfaces (#24). A person uses pages on a listener of
their own; an agent uses the same operations through MCP or the CLI.

- **A role with its own listener.** `--admin` (`Role::Admin`) serves `/admin` and
  `/admin/api/*` on `ADMIN_ADDR`, default `127.0.0.1:8789`, behind `ADMIN_TOKEN`
  (bearer, compared in constant time; `Cache-Control: no-store` on every reply). The
  network roles' listener answers those paths 404. In compose the container binds
  `0.0.0.0:8789` (its loopback is unreachable from the proxy), the proxy's second site
  forwards to `judgebot:8789`, and compose publishes it on the host's `127.0.0.1:8789`
  only (`ADMIN_PORT` moves the host port). From another machine: an SSH port forward, or
  on a trusted network a compose override that publishes it on the LAN address as well
  (plain HTTP, so the token crosses that network unencrypted).
  `ADMIN_ADDR` may not share `API_ADDR`'s port: `roles::plan` refuses it, since both
  listeners would reserve the port and the second fail once the first serves. The listener is reserved at startup
  and listens once the instance is open, and it drains on shutdown like the main one,
  so a blue-green deploy hands it over the same way (D29). Rejected: admin routes on the
  public listener behind a token, which would put the private material one leaked
  token away from the internet.
- **The tunnel is not kept off the network; the token is the guard there.** cloudflared
  shares the compose network and could dial `judgebot:8789`. A second network does not
  separate them: judgebot binds every address of its container, so a container on the
  default network reaches the port whatever other network it joins. The tunnel's public
  hostname points at `proxy:8787` only, and a request reaching 8789 any other way still
  needs `ADMIN_TOKEN`. Rejected: binding the admin listener to one network's address,
  which compose does not give a container before it starts.
- **Loopback and a token, not accounts.** One judgebot is one community's (D16) and has
  one operator; D17 keeps logins out of the web app. The page is opened with a
  `#token=` address (`judge-cli admin-url`), as judge-config's is (D23): the fragment
  never reaches a server log, the page moves it to `sessionStorage` and out of the
  address bar, and sends it only as a header.
- **`MCP_TOKEN` is not reused.** It is handed to MCP clients for judging, and its worst
  case is spend, bounded by `MCP_JUDGE_LIMIT`. The admin reads expose other people's
  questions and Discord ids, a different kind of loss, so they take their own
  credentials: `ADMIN_TOKEN` for the pages and `MCP_ADMIN_TOKEN` (off unless set) for
  `/mcp`. Each is at least 24 characters, and no two of the three may be equal: a reader
  of any one refuses the launch.
- **One list of operations.** `admin_calls`, `admin_call`, `admin_failures` and
  `admin_usage` are in `judge_agent::ops` beside the rest, so `judge-cli` (`calls`,
  `call`, `failures`, `stats`), stdio `judge-mcp` and `/mcp` share them. Their queries
  (`judge_bot::admin`, `db::stats`) are also what the admin pages read.
- **The capability is a type.** Every admin operation takes a `judge_agent::admin::Admin`,
  which only that crate constructs, in two places:
  - `Toolbox::from_env`, the only way another crate gets a standalone toolbox, returns
    one beside it (`Local`). Whoever calls it has connected with `DATABASE_URL`, which
    reads everything the admin operations do. `judge-cli` and stdio `judge-mcp` are
    built on it. A toolbox inside the instance is made with `Client::instance` through
    `Toolbox::new`, which hands out no capability, and `Client`'s standalone kind is
    private to the crate.
  - `mcp::admin_http_service` takes a `McpAdminToken`, which only `from_vars` makes,
    after checking it against `MCP_TOKEN` and `ADMIN_TOKEN`. It returns the handler
    paired with that token.

  The handler `MCP_TOKEN` reaches is `JudgeMcp<Public>`, a type with no admin tools; the
  admin tools exist only on `JudgeMcp<Privileged>`. `judge_api::mcp::router` takes the
  public service as `JudgeMcp<Public>`'s and the admin one as an `AdminService`, and
  sends a request to the admin service only when it bears that service's own token. What
  the compiler does not check is that comparison itself, a line in `mcp::Gate::grant`
  covered by a test over the real services. Every admin call over MCP is logged at INFO
  with its tool's name. Rejected: one handler that checks a flag per call, where a
  missed check is a leak.
- **Remote agents go through SSH first.** Running stdio `judge-mcp` over SSH (`docker
  compose exec -T judgebot judge-mcp`) reaches the admin tools with no token at all.
  `MCP_ADMIN_TOKEN` is for agents that cannot run a command (a cloud agent, a hosted
  connector).
- **Opt-in.** The compose service's default roles stay `--discord --api --web --jobs`:
  `--admin` needs a token nobody has set yet.

## D38. Ratings with reasons

*Decided 2026-10-20.*

A rating and a piece of feedback are one thing (#27). *Correct* is recorded at once.
*Partially correct* and *Incorrect* are recorded with a reason: a category, and optionally
what is wrong and the right answer. The reason goes to a work queue the operator clears
from the admin surfaces (D37), beside the call's question, answer, citations and cards.
It never reaches the model (D10).

- **A rating is a sum, not a score with optional fields.** `judge_core::Rating` is
  `Correct | Partial(Reason) | Incorrect(Reason)`, so a Correct with a reason cannot be
  built. `Reason` holds a `FeedbackCategory` (an exhaustive enum of eight) and two
  optional `FeedbackText`s (trimmed, 1 to 1000 characters; blank is none). The table's
  checks say the same: a reason only on a score below 3, texts only with a category.
  A rating from before reasons has a fault and no category, and opens no item.
- **The queue is its own table, keyed on the rating.** `feedback_queue` holds the
  operator's state (status `open | resolved | dismissed`, note, "fixed by", when it
  closed), with a foreign key to the rating that cascades. A replaced reason reopens the
  item, keeping the operator's note. Re-rating as Correct drops an item nobody wrote on
  and keeps one with a note or a "fixed by". Rejected: the state as columns on
  `ratings`, where the rater's next upsert would overwrite the operator's note.
- **A rating is replaced only after asking.** A click reads the rater's earlier rating
  first (`CallStore::earlier_rating`, one indexed read, bounded at 1.5 s from the interaction's arrival) and
  `judge_core::feedback::next_step` decides: the same Correct again is "already
  recorded"; any other earlier rating gets a private Replace / Keep prompt; otherwise
  Correct is recorded and a fault asks for its reason. **Keep** reads the rating back
  and says what is on record, since a prompt stays clickable after its Replace went on
  to a form. The write is still an upsert, so two racing clicks end with the last.
- **The form is the click's initial response.** Discord opens a modal only as the first
  answer to an interaction, so a fault's click is not deferred: the earlier-rating read
  happens inside Discord's 3 s. Submitting the form defers, looks the judge role up and
  stores the rating.
- **One form over HTTP, a category menu first over the gateway.** Discord takes a menu
  in a modal only inside a Label component (type 18). serenity 0.12.5 can build neither
  and reads a submitted Label as an empty action row, dropping the values inside; over
  the gateway serenity parses the event before the bot sees it, and offers no raw
  payload. So with `DISCORD_INTERACTIONS=http` the modal (written as JSON,
  `discord::feedback::modal`) holds the category menu and both texts, and the endpoint
  reads the values from the request body. Over the gateway the click shows a private
  category menu, whose choice opens a modal holding the two texts in action rows, the
  category carried in its id. Rejected: vendoring a patched serenity, a fork to carry
  for one struct; the menu-first flow everywhere, which costs the HTTP deployment a
  step.
- **Stateless steps.** Every id carries the call and the score (`rate-replace:`,
  `rate-keep:`, `rate-cat:`, and the form's `rate-form:<call>:<1|2>[:<category>]`), so
  any process answers any step after any restart. An old `rate:<call>:<n>` button gets
  the new flow. Maintenance answers every step with the maintenance reply (D32).
- **Two kinds of rater.** `ratings.rater_kind` is `discord` (the user id) or `web` (the
  browser session: `user_id` is `web:<session uuid>`, so the primary key is unchanged).
  `judge_core::Rater` keeps a web rater's name, address and user agent off a Discord
  rating by type, and the table by check.
- **The web rates without accounts.** `POST /api/calls/{id}/feedback`, served beside
  `/api/judge` (`--api`), takes the score, the reason and an optional name, as the
  browser tab's session id: the one its questions already share history under, so a
  tab is one rater and a new tab another. The server is authoritative on replacing:
  without `replace: true` an earlier rating is a 409 `confirm` and nothing is written,
  so a client that skips the question cannot replace silently. Its own per-address
  limit (30 per ten minutes), a 16 KiB body, unknown fields refused. Only a public call
  within the feed window may be rated (one 404 otherwise). `JUDGE_FEED=false` leaves
  rating on: an answer's id reaches only its asker. Rejected: a rater id of its own in
  `localStorage`, which would follow a browser across visits for no gain in counts
  that never shape retrieval.
- **Counts apart, reasons on request.** The feed and call pages count Discord's and the
  web's ratings apart; the page shows the web's only when its reader asks (a choice the
  browser remembers), because they are trivially inflated: a new session is a new
  rater. `judge-cli stats` keeps them apart too. Reasons stay the operator's unless
  `JUDGE_PUBLIC_FEEDBACK=true`, and even then are sent without the rater: no name, user
  id, address or user agent. Raters are told when their reasons are public (the form
  and the confirmation, Discord's and the web's; `/api/about` carries the setting).
  Turning it on publishes free text from anonymous raters, so a **dismissed** queue
  item's reason is never read publicly: dismissing is how the operator takes one down.
  User, role and channel mentions are reduced and `@everyone` / `@here` broken; nothing
  else is filtered.
- **Unconfirmed writes only add.** Without `replace: true` the web route writes with
  `CallStore::rate_if_new` (`ON CONFLICT DO NOTHING`, then a re-read), so two requests
  racing from one session cannot replace each other unasked; the loser gets the 409.
  Discord keeps its upsert: its Replace / Keep prompt is the confirmation.
- **Addresses expire.** A web rating keeps its address and user agent for
  `FEEDBACK_IP_DAYS` (default 90, 1 to 3650; there is no "keep forever"), for dealing
  with abuse. Every rating written clears older ones, and so does the refresh's `sweep`
  step, so an instance nobody rates on still clears them. The rating and its reason
  stay.
- **Worked from the admin surfaces.** `queue_list`, `queue_show`, `queue_resolve` and
  `queue_dismiss` are operations in `judge_agent::ops` behind the `Admin` capability
  (`judge-cli queue …`, stdio `judge-mcp`, `/mcp` with `MCP_ADMIN_TOKEN`), and the admin
  listener serves them as `/admin/api/queue*` and the Feedback page. Closing is the one
  write there: a POST behind the same bearer, which a browser never sends on its own.

## D39. Public dashboard

*Decided 2026-10-20.*

The web app's `/dashboard` shows what the instance has been asked and how it went: questions
per day by interface, the outcomes, the rating mix, the most-asked categories, cards and
askers, by range, interface, user and model (#28). The admin page shows the same report over
every call, with the spend and the private detail. Both, `judge-cli stats` and the MCP tool
`admin_usage` read one query module, `judge_bot::db::stats`, so no two of them can disagree.

- **Public by default, with the feed.** The dashboard counts what the feed already shows
  (D36), so it is served where the feed is: under `--web`, unless `JUDGE_FEED=false`, when
  `GET /api/stats` is not routed (the JSON 404 of any unserved `/api/` path) and the page
  has no tab. `/api/about` carries `stats: {cost, models}`, or null. Rejected: a setting of
  its own, a second switch for the same exposure.
- **Private is never counted.** Not in a total, an outcome, a rating, a category, a card, a
  user's figures, a cost or a model. Every public count reads calls and runs through
  `publicly_visible`, the predicate the feed reads through, defined once as a SQL function
  the planner inlines. The query keeps the public and operator scopes as two branches
  of a `UNION ALL`, each switched on or off as a whole, so the public branch states the
  predicate unconditionally and even a generic plan proves `NOT private` and reads the
  partial indexes (the feed's, and `judge_runs_public_finished_at`); a test holds a
  generic plan of the real queries to that. Rejected: `NOT $public OR …`, which no
  generic plan can match to a partial index. A run carries its
  call's `private`, so the outcome counts take only public runs, and runs older than
  `JUDGE_FEED_DAYS` are left out like calls. Rejected: counting private runs in the
  outcomes as anonymous totals, which would let a narrow filter (one interface, one day)
  reveal that someone asked privately and how it went.
- **Runs record no asker.** Under a user filter the run figures (outcomes, and on the
  admin side run cost and latency) are `null` rather than estimated from the runs that left
  a call. A user filter takes a Discord username on the public side, and a username or id
  on the admin side.
- **Cost and models are opt-in, by type.** `JUDGE_PUBLIC_COST=true` adds what the public
  calls cost (their runs' charges, and per question); `JUDGE_PUBLIC_MODELS=true` adds which
  models read and answered them, and the `model` filter. Both are validated at `Config`
  load. The figures exist only through a `CostAccess` / `ModelAccess`, and the public
  scope gets one only from `stats::Disclosure`, so a forgotten check leaves the field
  empty. A `model` filter while models are hidden is a 400, not ignored: an ignored
  filter would show every model's figures under the name of one.
- **Web ratings are sent, and hidden by the page.** The rating mix counts Discord's and
  the web's apart (D38); the page shows the web's only when its reader asks.
- **Bounded reads.** A report covers 1 to 366 UTC days, by the database's clock. A
  public range that ends before the `JUDGE_FEED_DAYS` window is a `400` naming the
  window, not a report of some other days; one that starts before it starts at it. The
  public endpoint has its own per-address limit (30 a minute) and two slots per process
  whoever asks, so many addresses cannot pile aggregate queries on the database. Each
  query reads its range through the time indexes, and a run's charges by their key.
  A report runs in one read-only transaction with a statement timeout (5 s public, a
  minute for the operator); past it the public page answers `503` busy, saying to ask
  for less.
- **The budget is the instance's.** Every process that keeps the spend ledger records
  its `JUDGE_BUDGET_PERIOD` and `JUDGE_MAX_USD` beside it (`budget_setting`), and the
  operator's report reads that, so `judge-cli stats` on another machine or with another
  `.env` reports the server's cap, not its own. Nothing recorded is "unknown" (`null`).
- **The operator's report adds what the public never sees:** private calls (counted, and
  counted apart), failed calls, the `spend_days` ledger by day and month (chat and
  embeddings, ingest included), the budget (period, cap, what is left), the runs by model,
  interface and stage, cost per answer, latency, the dearest runs and refusals at the cap,
  the lowest-rated calls, the feedback queue's counts and the latest refreshes. The
  ledger is not narrowed by a filter: it records spend per day, not per call.

## D40. Refresh stopped at shutdown

*Decided 2026-10-20.*

A scheduled refresh running when `judgebot` is asked to stop (every `scripts/deploy.sh`,
D29) used to die with the process. Its `refresh_runs` row stayed open until the record
counted it as a failed run, 3 h 10 min later, which alerted and backed off the next run
for an ordinary deploy. While it was in the CR load or the retirement pass it held
`CALLS_REWRITE_LOCK`, and the answers still draining waited on a step that would never
finish (#33).

The shutdown is now a third reason for a run to stop, beside the lost lease and the
schema skew, and it reuses what the run timeout does:

- **The scheduler is told.** `jobs::start` takes the process's `Shutdown`. Once it is
  requested no check starts, a wait between checks ends, and a run in progress stops.
- **The step is cut off at once.** `ingest::refresh` takes a `Caller`:
  `Caller::Schedule(&Shutdown)` for the schedule, `Caller::Manual` for `judgebot ingest
  refresh`, so a scheduled run cannot be started without the shutdown it obeys. The step
  in progress is dropped at its next await point, as `RUN_TIMEOUT` drops one, and recorded
  as skipped with `interrupted`. The steps after it are skipped with `shutdown`.
- **The run is stopped, not failed.** It closes its row with `ok` null
  (`RunOutcome::Stopped`), even when a step before the cut failed: the next run does every
  step again, and a failure that is real fails that run. A stopped run is no attempt
  either (`runs::history` leaves it out of `last_attempt_age_secs`), so it charges no
  backoff and the new container's first check, 1.5 to 3.5 minutes after it starts, finds
  the refresh due and runs it. That applies to a run stopped by a schema change too.
- **The lock goes with the connection.** The thread releases the lease and ends, and
  shuts its runtime down, which closes every connection it opened. A statement the cut
  step had already sent keeps running on the server, since sqlx sends no cancel, and a
  server notices a closed client only when it next writes to it. The scheduler's
  connections therefore set `client_connection_check_interval` to 2 s (Postgres 14 on,
  set after connecting so an older server still connects): the server checks while a
  statement runs, ends it and its transaction, and frees `CALLS_REWRITE_LOCK`. A test
  holds a lock under `pg_sleep` in a step, stops the thread, and sees the lock free
  within seconds. Without the setting it stays held until `statement_timeout` (30 min).
- **`main` waits for the record.** After the roles have drained, `Scheduler::stopped`
  waits for the thread at most until the drain's limit, then the ledger is written, so
  the refresh's spend is in it. The thread has been stopping since the request, beside
  the drain. Every wait outside a step is raced against the request too: the reads and
  checks before a run or a step (the migration ledger, the record, the lease, the stored
  CR version) are dropped at once, and the writes that record the run (its row begun,
  read back and finished) and the lease's release share one deadline,
  `ingest::STOP_WRITES` (15 s) after the request. A row the start wrote is always
  closed; an insert still waiting at the deadline goes with its connection. A run that
  had finished, failed, before the request still posts its alerts (each bounded by the
  webhook's 10 s), since the next run would not repeat a streak's first failure; a
  stopped run posts none. All of it, with the runtime's shutdown, is
  `jobs::STOP_LIMIT` (46 s), which a `const` assertion keeps under `DRAIN_LIMIT`. It
  runs on the scheduler's own runtime and pool, so the wait cannot deadlock with the
  serving roles. The exit stays within `DRAIN_LIMIT` plus the ledger's `FLUSH_LIMIT`, as
  before. A second signal still exits at once.

Every step is safe to cut off at any await point:

| Step | What a cut leaves | Why it is safe |
|------|-------------------|----------------|
| `cards` | Bulk files are downloaded to `.part` and renamed whole; the upserts commit per batch, the batch in progress rolls back; the deletion of retired cards and withdrawn rulings runs only after every batch | The upserts are idempotent, and nothing is deleted on a partial pass. Its parse loops, which read the bulk files for tens of seconds on a slow machine, yield every 2,000 lines, so a cut does not wait for them |
| `rules` | One transaction holding `CALLS_REWRITE_LOCK`: rolled back, or committed whole if the cut came during its `COMMIT` | `rules latest` compares the stored version, so the next run loads it again or finds it loaded; the retirement pass runs then. The CR file is written to `.part` and renamed |
| `lists` | Each list's replacement and its record are one transaction | The next run's plan reads the same record and reloads |
| `prune` | Each batch is one statement: deleted or not | Batches until none is left, so the next run finishes it |
| `sweep` | One statement | Idempotent |
| `retire` | One transaction holding `CALLS_REWRITE_LOCK`, rolled back | It recomputes every call from scratch |
| `embed` | Each batch embeds, then writes its vectors in one transaction; the space row is written in the first batch's | Rows left empty are embedded by the next run. A request already sent is billed (its reservation kept at the worst case, D7), so at most one batch is paid twice |
| `emoji` | An upload already sent may have been created; the SVG cache is written then renamed | The next run lists the application's emoji and skips those that exist. `runs::emoji_since` counts an `interrupted` emoji step as one that may have uploaded, so the bot reloads its table |

**Not covered:** a crash or SIGKILL still leaves an open row, which the record counts as
failed after 3 h 10 min (D24). A process that exits because a role failed, not on a
signal, does not stop its scheduler first. `scripts/refresh-data.sh` runs in its own
container and is stopped by whoever stops it.

**Rejected:**

- *Letting the step finish.* A Scryfall sync on a NAS, or an embed of a new CR's few
  hundred rules, can outlast the two minutes of the drain, and the CR load holds the
  lock the draining answers wait on.
- *Recording the cut run as failed.* Every deploy that met a refresh would alert and
  back the next run off by an hour.
- *`pg_cancel_backend` or `pg_terminate_backend` from a side connection.* It needs the
  scheduler's backend ids: its `application_name` is the same in the old and the new
  container during a deploy, so a name would reach the new one's connections. A
  cancelled statement also leaves its transaction open, with its locks, until the
  client rolls it back or goes away. Closing the connection, with the server checking
  for it, does both.
- *Waiting a separate, shorter bound after the drain.* The thread stops beside the
  drain, and a bound counted from the request keeps the exit inside the compose
  `stop_grace_period` whatever the drain took.

## D41. Public HTTPS by default

*Decided 2026-10-20.*

Cloudflare is an alternative, not the default. A first setup answers on the host's
loopback only, and an operator who wants the web app, the API or Discord's interactions
endpoint on the internet turns on the bundled HTTPS site. The owner's own instance keeps
its Cloudflare Tunnel unchanged.

- **The bundled site is a profile service.** `edge` runs the same Caddy image as
  `proxy`, under the compose profile `public` (`COMPOSE_PROFILES=public`, as
  `cloudflared` is under `tunnel`). It publishes ports 80 and 443 (and 443/udp for
  HTTP/3) on every address of the host and serves `https://<JUDGE_DOMAIN>` with a Let's
  Encrypt certificate Caddy gets and renews itself, kept in the `judgebot-edge-data`
  volume. Without the profile nothing claims 80 or 443, and the stack is what it was:
  `127.0.0.1:8787` through `proxy`.
- **It dials `judgebot:8787` itself**, with `proxy`'s handover settings (a new
  connection per request, dial retries for 5 s), so a blue-green deploy (D29) hands
  over the same way, and a deploy never restarts it. Going through `proxy:8787` would
  have added a hop and made `proxy` tell the bundled site's header apart from a
  caller's on the same site.
- **A neutral client-address header.** `API_CLIENT_IP=proxy`
  (`ClientIpSource::ProxyHeader`) buckets on `X-Judgebot-Client-IP`. `edge` sets it to
  the address it was dialled from, replacing any copy the caller sent, and removes a
  caller's `CF-Connecting-IP`. The other public routes remove it: `proxy`'s `:8787`
  site, where a tunnel's hostname points, and its admin site. The
  host's `127.0.0.1:8787` now maps to a third site, `:8788`, which passes it on, so a
  reverse proxy on the host can set it as well. That site is reached by the host's
  processes and by any container on the compose network, which can dial `proxy:8788`;
  Caddy cannot tell the two apart (both arrive from the network), so nothing on the
  network may be pointed there and no other host address may be mapped to it. judgebot
  reads the header only in that mode, only as a single copy (a proxy that appended to
  the caller's leaves two) and only from a peer on a loopback, private, link-local,
  shared (100.64.0.0/10) or unique local address, which keeps out a caller connecting
  from a public address of its own. Where Docker rewrites the source (its userland
  proxy for IPv6, rootless Docker, Docker Desktop) every caller is a private address,
  so the check passes them all and the routes are the guard. Anything else counts the
  peer, which over-counts rather than under-counts.
- **An IPv6 /64 is one bucket.** Every limiter keys on `limit::Bucket`: an IPv4
  address, or an IPv6 address cut to its /64, the block one subscriber is given. Keyed
  on the /128, one visitor could pick a new address per request.
- **Caddy drains for 70 s.** Both Caddyfiles set `grace_period 70s` (an answer takes up
  to a minute) and compose's `stop_grace_period` is 80 s, so stopping or recreating
  `proxy` or `edge` finishes the requests in flight, Discord's HTTP interactions among
  them, instead of Docker killing Caddy's unbounded wait at 10 s.
- **A missing domain stops `edge`, not compose.** Compose interpolates every service
  whatever its profile, so `${JUDGE_DOMAIN:?}` would fail every command on a host
  without the profile. The container's entrypoint (`proxy/edge-entrypoint.sh`) refuses
  an empty or malformed `JUDGE_DOMAIN` with a line naming it, and `judge-config` checks
  the same rule (`check::is_host_name`, which a test holds the script to) in its
  `exposure` check, which also warns when `public` runs with another `API_CLIENT_IP`
  and `tunnel` with `proxy`.

The reasons:

- **Cloudflare was a requirement in all but name.** A first setup needed a Cloudflare
  account and the zone's DNS moved there before anyone could open the web app from
  outside, for a stack that already runs Caddy, which gets certificates by itself.
- **A profile, not always-published ports.** Compose cannot publish a port on a
  condition, and a stack that always took 80 and 443 would fail to start beside a NAS's
  own web page or an existing reverse proxy, and would put a public listener on every
  trial install. A profile keeps the default loopback-only and the public site one
  variable away.
- **A header of judgebot's own.** `CF-Connecting-IP` from a non-Cloudflare proxy, the
  earlier advice for a reverse proxy, claims an origin the request does not have, and
  under the tunnel a caller's copy arriving any other way would be read as Cloudflare's.
  A name no CDN or proxy sets by default cannot be mistaken for another's, and stripping
  it on every other route is a line per site.

**Rejected:**

- *`X-Forwarded-For` from the bundled proxy.* Caddy rewrites it, but proxies differ in
  whether they append or replace, and a mode that trusts it is one misconfigured hop
  from the forgery D15 refused.
- *The public site inside `proxy`.* One Caddy with ports 80 and 443 would need them
  published whether or not the site is wanted, and `auto_https off`, which `proxy` keeps
  so it never asks for a certificate, would have to go.
- *Restricting `:8788` to the host's connections.* They arrive from the compose
  network's gateway, whose address Docker picks when it creates the network; pinning a
  subnet to name it could collide with the host's own networks.
- *Rate limiting in Caddy.* The stock image has no rate-limit module, and the process's
  own limit already counts per address once the header is right.

What follows:

- **Existing tunnel setups need no change.** `proxy:8787` and the `api` alias behave as
  before, `CF-Connecting-IP` still passes through, and `127.0.0.1:8787` still answers.
- **The host needs ports 80 and 443** and a DNS record for the name; a dynamic home
  address needs a dynamic DNS client. Spend is protected by the process alone: the
  per-address limit, the concurrency slots and the cap, with no edge rules in front.
- **IPv6 visitors may share a bucket.** Docker's userland proxy hands `edge` the
  network's gateway as the address of every IPv6 visitor unless the compose network
  has IPv6, which over-counts; the runbook says to leave out the `AAAA` record or
  enable IPv6 there. Rootless Docker and Docker Desktop do the same for every visitor.
- **`proxy` mode trusts the compose network.** A container on it that dials
  `judgebot:8787` or `proxy:8788` (a tunnel whose service names either, which the
  runbook says never to do) passes a caller's header through.
- **A proxy on another machine cannot use `proxy` mode.** It would need 8787 published
  on the LAN, whose site removes the header; it keeps `peer` and rate limits itself.
- **The first deploy recreates `proxy` once,** for its new port mapping and grace
  period, and `edge` once where it already ran; each finishes its requests in flight
  first.

## D42. Unreleased cards

*Decided 2026-10-23.*

Scryfall lists a card as soon as it is previewed, weeks before its set releases, and the
daily refresh loads it. The Comprehensive Rules update for that set comes out about a week
before the release (prerelease): the Hobbit CR is dated 2026-08-07 for a 2026-08-14
release, Reality Fracture's 2026-09-25 for 2026-10-02. Until then the judge answers a
question about the card from rules that may not have its keywords, creature types or
mechanics. A question about such a card is still answered, with a note that the rules may
not cover it, and the answer never becomes an example for later questions.

- **A card's first printing is a date.** The cards step writes `cards.first_printed`, the
  earliest `released_at` over the card's printings in the default-cards bulk file
  (digital ones included: a card out on Arena is out). A reprint keeps its first set's
  date. A card first printed after the database's `current_date` is upcoming, so the
  note stops on release day by itself, with no refresh.
- **Coverage is judged from two signals** (`judge_core::preview`), because the CR does not
  name the sets it covers (it names no set at all, outside the trademark lines). An
  upcoming card is covered only when both hold:
  - The loaded CR is dated no more than `COVER_WINDOW_DAYS` (21) before the card's first
    printing, so it is that set's update and not the previous one's.
  - Every rules term on the card appears in the loaded CR's rules or glossary as a whole
    word. The terms (`rules_terms`) are the card types, supertypes and subtypes on its
    type lines and its Scryfall keywords, less ability words, flavor words and mode names
    (a keyword the Oracle text uses only as a line's `Word — ` prefix: CR 207.2c and
    207.2d give those no rules meaning, and a set's flavor words never reach the CR). A
    keyword whose dash is followed by its cost (`Exhaust — {6}:`, `Ward—Pay 3 life.`) is
    kept.
    `Halflingcycling` also counts through `typecycling`, and `Desertwalk` through
    `landwalk`.
  The term check can only miss in one direction: a new keyword that is an ordinary word
  (`Warp`, `Void`) is found in unrelated CR text, and a two-word subtype is looked up a
  word at a time. The date check is what catches a set's cards before its update.
  Against the CRs of August and September 2026, Reality Fracture's `Empower Jace` is in
  September's and not August's, and none of Star Trek's terms (`Face a dilemma`,
  `Vulcan`, `Klingon`, `Borg`, `Assimilate`) is in either.
- **The status travels with the card.** `Card::release` (`Release::Released`, or
  `Upcoming` boxed with its date and `Coverage`, `Uncovered` holding a `NonEmpty<Gap>`) is set by
  the one loader that builds cards for a question (`db::cards::load_cards`), and
  `CardRef::status` (`CardStatus::{Released, Previewed, Unreleased}`) follows it onto
  the verdict. Only an upcoming card costs the term lookup.
- **Every interface notes an `Unreleased` card** above the answer, naming it: Discord's
  reply (`render::unreleased_note`, the catalog's `unreleased_note`), the web app
  (`CardView::status`, `unreleasedNote`), and the agent toolbox's `note`. A `Previewed`
  card, prerelease week with its set's CR loaded, gets none.
- **The model is told.** The material puts a line under an upcoming card's heading: not
  released yet, first printed on what date, and for an uncovered card the reasons and an
  instruction to answer from its Oracle and reminder text and the rules shown, saying
  what they leave unsettled. A released card's material is unchanged, so the pinned
  prompts and golden fixtures are too.
- **A stored call keeps its note.** `calls.unreleased_cards` lists the cards the reply
  noted, and the feed and the admin view show the note worded for a past answer.
- **Never an example.** The prior-call query skips a call with any noted card, and any call
  about a card first printed after the call's day. That second test covers calls stored
  before this change and survives the release: a call answered under the old CR stays
  out once the card is out, since its answer may predate the rules it needed.
- **The gold set gains two questions** (`unreleased: [card]` in `eval/gold.yaml`): one
  needing an undefined keyword (the answer should hedge) and one using only existing
  rules (it should not). The run records each answer's note against the list
  (`UnreleasedCheck`), and reports a question whose card is out or covered as stale, to
  be re-authored rather than scored.

**Rejected:**

- *Comparing the card's date with the CR's date alone.* The CR is dated about a week
  before the set's release date, so every card of a new set would read as newer than the
  CR covering it until the next CR, months later.
- *Telling a set's CR by the cards it mentions.* The CR names no cards beyond its
  examples, and new sets' cards are not among them.
- *Noting released cards with unknown terms too.* A promo can come out before its set
  (Star Trek's Spock on 2026-09-04, Borg Queen on 2026-10-08), and then the date says
  released while the CR knows neither `Vulcan` nor `Borg`. But 164 released cards first
  printed before September 2026 have terms the CR never mentions: digital-only actions
  (`Conjure`, `Seek`), playtest cards' one-off creature types and a handful of
  idiosyncratic keywords. A note on all of them would be noise. Released stays released.
- *Storing the coverage on the card at ingest.* It depends on the CR loaded as well as
  the card, and the refresh loads them in separate steps. Read at question time it is
  always the pair that answers.
