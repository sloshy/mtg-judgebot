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
  request keeps `max_tokens` ≤ 16k, inside the non-streaming guidance, and streaming
  would buy nothing the judge needs.
- **schemars from the serde struct** (I5), with one transform per backend that reduces the
  schema to what that backend accepts (`additionalProperties:false`, `oneOf→anyOf`,
  stripped constraints; strict-mode `required`/`anyOf [T, null]` for OpenAI). Stripped
  constraints are still enforced on decode by serde and `nutype`, so the server's
  enforcement is an optimisation, not a guarantee.
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
  resolves them through a typed ladder: alias → possessive-stripped alias → exact →
  printed name → short name before the comma → alias suffix → trigram fuzzy. A
  `[[bracketed]]` span is exact or printed name only, with near misses offered as
  choices. Embeddings are never used for this: a nickname like "bob" has no semantic
  relation to *Dark Confidant*.
- **The resolver never guesses.** Ambiguity is `Resolution::Ambiguous` and becomes a "did
  you mean?" button row (I1). A wrong card silently resolved would produce a confidently
  wrong ruling with valid-looking citations, which is the worst failure the bot can have.
- **Three retrieval legs, unioned in priority order.** They are the curated category →
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
  retrieved `Context`. What is stored is the *source's* span, not the model's string, so a
  persisted quote is byte-exact and the retirement pass (D11) stays a strict check. The
  comparison folds typographic punctuation one character to one character (curly quotes,
  the dash block, non-breaking spaces). Models retype the CR's `’` as `'`, and that was
  the most common rejection. The comparison never folds case or words.
- Only `Verdict<Validated>` can reach `CallStore::persist` or Discord rendering (I3). Only
  `Verdict<Unvalidated>` is `Deserialize`. `Synth<Fresh | ToolRequested | Final>` makes a
  second tool round a compile error (I4).
- **One retry**, with the rejection rendered into the prompt and the rejected answer quoted
  back as a blockquote, then an error the user sees. Retries live in `judge()`, not in the
  Discord layer, so every front door gets the same behaviour.
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
binaries build the default setup from `.env` (Anthropic direct, `claude-opus-5` for both
stages, Voyage if keyed), which is the one the eval numbers and the pinned prompt digest
were produced on. A knob that would be silently ignored is a load
error naming both keys. `docs/PROVIDERS.md` is the reference.

## D7. Spend cap by type

*Decided 2026-09-02.*

Every model call is behind the spend cap by type. The operator is cost-sensitive, and the
model calls are the only thing that costs money per question. The pipeline's `ChatModel`
port is *sealed*. `Metered<B>` is its only implementation and `Models`' fields are
private, so there is no way to construct an uncapped model or to meter one against a
foreign budget.

The cap **reserves the worst case before sending** and settles on reported usage. That is
why caps under about $0.45 refuse synthesis outright rather than overshooting.

Pricing is a closed sum:

- the built-in table for Anthropic models, re-read for the model the *response* names,
  since a fallback may route elsewhere,
- an operator's per-token rate,
- `free`.

An unknown Anthropic model prices as the most expensive one, because erring high is safe.
An unpriced model on an OpenAI-compatible provider is a startup error, because it could
be anything.

Dollars stay per process rather than per user or per server. The meter settles after the
call, so a finer-grained dollar cap would either over-reserve or overshoot. Question-count
limits are the tool for finer grain, and the MCP transport and the web page have them.

## D8. Native cloud auth for Anthropic

*Decided 2026-09-02.*

Bedrock and Vertex were already reachable through LiteLLM or their OpenAI-compatible
endpoints with no new dependencies. Native SigV4 and ADC support was added anyway, for the
operator who wants Claude on their own cloud account *without* running a proxy. The cost
is two auth crates behind the `aws`/`gcp` Cargo features. The features are default on
and named in the Dockerfile, so a lean build cannot name the doors and the loader says
"not built".

Credentials come from the platforms' own chains, never from `judge.toml`. They are
resolved lazily and probed once at startup, so an empty chain fails there, not on the
first question. Each door's feature mask (what it cannot accept: server-side fallbacks,
strict tools, betas) was verified against the live docs. It is an exhaustive `match`, not
a flag.

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
- The adapters re-read the row on every use and go **dark, never mixed** on a mismatch
  (error log naming both spaces).

Writers hold the space under the shared side of an advisory lock and the switch takes the
exclusive side, so a switch waits for in-flight writes. `reembed` probes the new embedder
before clearing anything, because re-embedding pays the provider per row. That is also
why `scripts/backup-db.sh` exists.

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
ratings because they are the only per-user data kept. Questions are stored against the
channel, not the asker.

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
7. The prior-call leg, which needs rated data to exist.

`judge-eval recall` still runs the gate for free on every retrieval change. The paid full
run costs about $2.50 and is not part of CI. Nothing in the test suite calls a paid API:
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
RAM and real CPU, which a NAS does not have. Data refresh is a nightly cron job running a
one-shot container, not a service.

**Rate limiting buckets on an address the caller cannot choose.** `API_CLIENT_IP` is
`peer` or `cloudflare` (`CF-Connecting-IP`), never `X-Forwarded-For`. Cloudflare *appends*
to a caller-supplied header instead of replacing it, which would hand every request a
fresh allowance against a paid endpoint. The earlier flag that did that is rejected at
startup rather than ignored. Deploy credentials live in their own env file that the
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
- An instance's anonymous web page is a second front door to the instance its operator
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
