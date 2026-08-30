# Proposal C — Rust  ✅ SELECTED

Implements `docs/ARCHITECTURE.md`. Library status verified 2026-08-29.

## Stack

| Concern | Choice | Notes |
|---|---|---|
| Toolchain | stable Rust, edition 2024 | `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]`, `clippy::pedantic` warn, `unsafe_code = "forbid"` |
| Async | tokio | |
| Anthropic | **hand-written client** (`reqwest` + `serde`), ~300–500 LOC, treated as project code. Non-streaming for the prototype: every request keeps `max_tokens` ≤ 16k, inside the non-streaming timeout guidance; add `eventsource-stream` + a `messages_stream` method when outputs need to grow. Bounded retries (2) on 408/409/429/529/5xx honouring `retry-after`; `fallbacks: "default"` (beta `server-side-fallback-2026-07-01`) on by default | No official Rust SDK (verified: nothing under github.com/anthropics). Community crates: `adk-anthropic` 2.1 (part of a 42-crate framework), `anthropic-tools` 1.1 (84 downloads) — neither worth the dependency. Model `claude-opus-5`; `thinking: {type:"adaptive"}`; `output_config.effort`; `cache_control`; handle `stop_reason == "refusal"` |
| Structured output | **schemars 1.2** derive from the serde struct | One `RecursiveTransform` sets `additionalProperties:false`, rewrites `oneOf→anyOf`, and strips keys outside Anthropic's subset (`pattern`, `minimum`/`maximum`, `minLength`/`maxLength`, non-listed `format`s such as `uint32`); serde/nutype still enforce them on decode. Schema unit test asserts none survive |
| Discord | **serenity 0.12.5 + poise 0.6.2** | Slash commands, buttons, threads verified. twilight 0.17 is the lower-level alternative |
| DB | **sqlx 0.9** + `pgvector` 0.4.2 (sqlx 0.9 support added 2026-05) | `query!`/`query_as!` checked at compile time; `.sqlx` offline cache with `cargo sqlx prepare --check` in CI. Diesel 2.3 is the sync alternative |
| Refinement | `nutype` 0.7, `nonempty` 0.12 | |
| Embeddings | plain HTTP to Voyage (`voyage-3.5` / `voyage-4` family; the `voyageai` crate is dead) | |
| Config / logging | `figment` or env + `serde`, `tracing` | |
| Tests | `cargo test`, testcontainers-rs for Postgres | |

## Layout

```
crates/
  core/     domain types, ports (traits), judge(), validation — no tokio/reqwest deps
  anthropic/ wire types + client + typestate tool loop
  ingest/   scryfall sync, CR parser, embed (bin)
  bot/      sqlx repos, voyage, serenity/poise, main (bin)
build.rs in core generates `Category` from data/categories.yaml
```

## Domain model

```rust
#[nutype(validate(regex = r"^\d{3}(\.\d+[a-z]?)?$"), derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash))]
pub struct RuleId(String);
#[nutype(derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash))]
pub struct CardId(Uuid);

pub struct Card { pub id: CardId, pub name: String, pub layout: Layout, pub faces: NonEmpty<Face> }

#[derive(Serialize, Deserialize, JsonSchema)] #[serde(tag = "kind", deny_unknown_fields)]
pub enum Citation {
    Rule { id: RuleId, quote: String },
    ScryfallRuling { card: CardId, idx: u32, quote: String },
    PriorCall { id: CallId, quote: String },
}

pub enum Resolution { Resolved { card: Card, via: MatchedVia }, Ambiguous { query: String, candidates: NonEmpty<Card> }, NotFound { query: String } }

#[derive(Debug, thiserror::Error)]
pub enum JudgeError { AmbiguousCards(NonEmpty<Ambiguous>), OutOfScope(Source), BadCitation(Citation), LlmRefused, Upstream(#[from] anyhow::Error) }

// I3: typestate — only a validated verdict can be persisted or rendered.
// Only Verdict<Unvalidated> is Deserialize/JsonSchema. `Validated { cr_version }` is stamped from Context in validate(); the model never reports cr_version.
pub struct Verdict<S: State = Unvalidated> { data: VerdictData /* answer, confidence, citations, category, source */, state: S }
impl Verdict<Unvalidated> { pub fn validate(self, ctx: &Context) -> Result<Verdict<Validated>, JudgeError> }
impl CallStore { pub async fn persist(&self, v: &Verdict<Validated>, ...) }   // Verdict<Unvalidated> does not type-check here

// I4: typestate — the tool round cannot repeat.
pub struct Synth<S> { .. }
impl Synth<Fresh>         { pub async fn send(self) -> Result<Either<Synth<ToolRequested>, Verdict<Unvalidated>>, JudgeError> }
impl Synth<ToolRequested> { pub fn answer_tool(self, chunks: &[RuleChunk]) -> Synth<Final> }   // infallible; tool_use ids + requested ids live in the ToolRequested stage value
impl Synth<Final>         { pub async fn finish(self) -> Result<Verdict<Unvalidated>, JudgeError> }   // no method to request tools again

#[async_trait] pub trait Retriever: Send + Sync { async fn retrieve(&self, q: &Question, cards: &[Card], e: &Extraction) -> Result<Context, JudgeError>; }
// Extractor, Resolver, Synthesizer, Embedder, CallStore analogous

pub async fn judge(deps: &Deps, q: &Question, history: &[Qa]) -> Result<Verdict<Validated>, JudgeError> {
    let e = deps.extractor.extract(&q, history).await?;
    if matches!(e.source, Source::Tournament | Source::OutOfScope) { return Err(JudgeError::OutOfScope(e.source)); }
    let cards = collect_resolved(try_join_all(e.card_spans.iter().map(|s| deps.resolver.resolve(s))).await?)?;
    let mut ctx = deps.retriever.retrieve(q, &cards, &e).await?;
    ctx.history = history.to_vec();
    // BadCitation ⇒ one retry naming the rejected citation, then error (retries live here, not in Discord).
    let rejected = match deps.synthesizer.answer(q, &mut ctx, None).await?.validate(&ctx) { Err(JudgeError::BadCitation(c)) => c, done => return done };
    deps.synthesizer.answer(q, &mut ctx, Some(&rejected)).await?.validate(&ctx)
}
```

## What the compiler enforces here

I1 (exhaustive enums), I2 (nutype regex, `NonEmpty`), I3 and I4 (typestate —
idiomatic in Rust), I5 (schemars from the same struct serde decodes; modulo
the transform, which is unit-tested), I6 (sqlx compile-time checked queries;
`pgvector::Vector` typed column), I8 (`Result` + `#[must_use]` + `unwrap_used`
denied), I9 (no null). `Send + 'static` bounds on Discord handlers surface
shared-state races at compile time.

## What it does not

- **I7 effect tracking.** An `async fn` retriever can call the LLM and the
  compiler won't object. Mitigation: `core` crate has no `reqwest`/`sqlx`
  dependency, so any I/O there is a build failure by *dependency graph*
  rather than by type — a weaker but real fence.
- sqlx nullability inference on `LEFT JOIN`s needs manual `!`/`?` overrides;
  a wrong override is a runtime error.
- Anthropic schema-subset rules are checked at request time unless the CI
  schema test exists.

## Risks specific to this stack

- We own the Anthropic wire types (adaptive thinking, effort, refusal
  `stop_details`, cache_control). Drift is ours to track; newtype the request
  builder so invalid model/thinking combinations don't type-check.
- `async_trait` boxing on every port; `Send` bounds leak into signatures.
- Schema tweaks recompile; violations of Anthropic's subset surface at API
  time without the CI check.
