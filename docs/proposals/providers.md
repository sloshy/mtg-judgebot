# Proposal D — Model providers

Status: **implemented 2026-09-02** (phases 1–5, commits `8f0cbd3`, `5405941`, `f72abce`,
`0ec0605` and the docs phase; deviations in §10). Extends `docs/ARCHITECTURE.md`
§3 step 1/3/5 (the two LLM calls) and the vector leg of step 4 (embeddings) so that an
operator can run the judge on any model they can reach, not only Anthropic's first-party
API and Voyage. The pipeline, the prompts, the validation and the typestates do not change;
what changes is *who is on the other end of the HTTP connection* and how that is configured.

## 1. Goal and non-goals

**Goal.** An operator who is not the author can deploy the judge with:

- Anthropic models through the first-party API (today), through Claude Platform on AWS,
  Amazon Bedrock or Google Vertex AI (Anthropic's Messages API shape, different auth and
  hostnames), or through a proxy that speaks the Messages API (LiteLLM's `/v1/messages`);
- any model behind an **OpenAI-compatible chat completions** endpoint: LiteLLM, OpenRouter,
  Ollama, vLLM, llama.cpp, Azure OpenAI, Bedrock's and Vertex's OpenAI-compatible
  endpoints, OpenAI itself;
- embeddings from Voyage (today) or any OpenAI-compatible `/v1/embeddings` endpoint;
- a **different model per stage**: a cheap model for extraction, a strong one for synthesis,
  possibly on different providers;
- a spend cap that still means something when the price table does not know the model.

**Non-goals.** Streaming (still not needed: every request keeps `max_tokens` ≤ 16k). A
plugin system: providers are Rust crates in this workspace, chosen by configuration.
Supporting every quirk of every OpenAI-compatible server: the OpenAI backend has a small,
explicit set of dialect knobs and otherwise follows OpenAI's documented API.

## 2. What the pipeline actually asks of a model

Four request shapes, all non-streaming, all with the answer decoded by serde into a type that
*is* the schema (invariant I5):

| # | Call | System | Tools | Tool choice | Structured output | Effort |
|---|---|---|---|---|---|---|
| 1 | Extraction | cached, stable | none | — | `Extraction` schema | low |
| 2 | Synthesis, first turn | cached, stable | `lookup_rules`, strict | auto, ≤1 call | `Verdict` schema | high (medium on truncation retry) |
| 3 | Synthesis, tool continuation | same | same | same (cache-preserving) | same | same |
| 4 | Synthesis, citation retry | same | `lookup_rules` still listed | **none** | same | same |

Plus: refusal detection (`stop_reason: refusal` → `JudgeError::LlmRefused`), truncation
detection (`max_tokens` → `Truncated`, retried once at lower effort), usage for the spend
cap, and the model's *own* assistant turn echoed back verbatim in call 3 (thinking blocks with
signatures on Anthropic; `reasoning_content` on DeepSeek-style servers; `tool_calls` on
OpenAI). Embeddings need `embed(texts, kind) -> Vec<Vec<f32>>` at a fixed width.

Everything that makes the judge trustworthy is **already client-side**: citations are
validated against `Context` (I3), the tool round is bounded by the typestate (I4), decoding
enforces the schema (I5). Structured-output enforcement on the server is an *optimisation*
that lowers the retry rate; a backend without it degrades to more retries, not to weaker
guarantees. That is what makes this feasible without touching `core`.

## 3. Where the seam goes

Today `crates/anthropic` owns three things that are provider-neutral and one that is not:

| Concern | Lives in | Neutral? |
|---|---|---|
| `Synth<Fresh \| ToolRequested \| Final>` typestate (I4) | `anthropic/synth.rs` | yes — it is about *one tool round*, not about Anthropic |
| Spend cap: reserve → send → settle, pricing table | `anthropic/client.rs` | yes — it is about usage × price |
| `classify(response) -> Step` (refusal / truncation / tool / verdict) | `anthropic/synth.rs` | yes, once the response is neutral |
| Wire types, headers, retry-after, beta flags, schema subset transform | `anthropic/{wire,client,schema}.rs` | no |

The seam is a new crate **`crates/llm` (`judge-llm`)** holding the neutral types, the port
trait, the typestate and the spend cap. `crates/anthropic` and a new **`crates/openai`**
become *backends* of it. `crates/bot`'s `extract.rs` and `synth.rs` (prompts, rendering,
budget, hydration, truncation retry) keep one implementation each and depend on `judge-llm`
only. The composition root picks backends from configuration. The dependency fence holds:
`core` still knows nothing about any of this.

```
core ← llm ← { anthropic, openai }        (chat backends)
core ← embed { voyage, openai-embeddings } (embedding backends; already a crate)
bot  ← llm, embed                          (adapters + build_deps; picks backends by config)
```

### 3.1 The neutral chat types (sketch)

```rust
// crates/llm/src/lib.rs
#[async_trait]
pub trait ChatModel: Send + Sync {
    /// One non-streaming round trip. Retries, spend reservation and usage
    /// settlement are done by `Metered<ChatModel>` (below), not here.
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError>;
    /// What this backend can enforce server-side; the adapters log a warning
    /// when a request relies on something the backend only approximates.
    fn capabilities(&self) -> Capabilities;
    /// Model id as the backend will bill it (for the price table and logs).
    fn model(&self) -> &str;
}

pub struct ChatRequest {
    pub max_tokens: u32,
    pub system: Vec<TextBlock>,           // text + CacheHint
    pub turns: Vec<Turn>,
    pub tools: Vec<ToolSpec>,             // name, description, JSON Schema, strict
    pub tool_choice: ToolChoice,          // Auto { parallel: bool } | None
    pub output: Option<OutputSchema>,     // the JSON Schema of the type we decode into
    pub effort: Option<Effort>,           // Low | Medium | High | XHigh | Max
}

pub enum Turn {
    User(Vec<TextBlock>),
    /// The model's own previous turn, replayed byte-for-byte. Opaque on purpose:
    /// thinking signatures, reasoning_content, tool_calls — whatever the backend
    /// returned is what it gets back. Only the backend that produced it reads it.
    Assistant(AssistantTurn),            // { backend: &'static str, raw: serde_json::Value }
    ToolResults(Vec<ToolResult>),        // { call_id, content: String, is_error }
}

pub struct ChatResponse {
    pub text: Vec<String>,               // text blocks in order; the JSON is the last
    pub tool_calls: Vec<ToolCall>,       // { id, name, input: Value }
    pub stop: Stop,                      // EndTurn | MaxTokens | ToolUse | Refusal(details) | Other(String)
    pub usage: Usage,                    // input, output, cache_read, cache_write (all u64, 0 when unknown)
    pub model: String,                   // as billed (may differ under fallbacks)
    pub assistant: AssistantTurn,        // for replay
}
```

Design points:

- **`AssistantTurn` is opaque.** The neutral layer never inspects it; it exists so the
  continuation request can replay it. A backend receiving an `AssistantTurn` tagged with
  another backend's name returns `LlmError::ForeignTurn` rather than guessing — the typestate
  never mixes backends within one conversation anyway.
- **`CacheHint` is a hint.** Anthropic emits `cache_control`; OpenAI ignores it (OpenAI
  caches automatically by prefix); LiteLLM forwards `cache_control` when the upstream is
  Anthropic. Placement rules (stable content first) stay as they are.
- **`OutputSchema` carries the schemars schema untransformed.** Each backend applies *its*
  subset transform: Anthropic's (`additionalProperties:false`, `oneOf→anyOf`, strip
  constraints) or OpenAI strict mode's (additionally: every property listed in `required`,
  optional fields become `anyOf [T, null]`, no `format`, no `default`). Both transforms are
  pure functions with the same walk-and-assert tests `schema.rs` has today. Decoding is
  unchanged, so a stripped constraint is still enforced client-side.
- **`Effort` maps per backend**: Anthropic `output_config.effort`; OpenAI
  `reasoning_effort` (`low|medium|high`, `xhigh|max → high`) *only if the model config says the
  model accepts it*, otherwise omitted; a warning if the caller asked and the backend cannot.
- **`Capabilities`** = `{ structured_output: Enforced | JsonMode | PromptOnly, strict_tools:
  bool, effort: bool, cache_hints: bool, refusal_fallbacks: bool }`. The adapters use it for
  two things only: to add the schema to the system prompt when enforcement is
  `PromptOnly`/`JsonMode` (Anthropic-backed requests keep the prompt byte-identical, so the
  pinned SHA-256 in `harness_tests` stays valid), and to log what they are relying on.

### 3.2 What moves, what stays

- `Synth<S>` and `classify` move to `judge-llm` over `ChatRequest`/`ChatResponse`; the
  wiremock tests in `bot/synth.rs` keep passing with Anthropic as the backend, and gain a
  twin with a mocked OpenAI server.
- `Client`'s spend machinery becomes `Metered<M: ChatModel>`: same reservation scheme, same
  `SpendCapExceeded` error (now `LlmError`), same shared counters across clones, same
  "bill a 2xx body that failed to decode" rule (each backend exposes a lenient
  `usage_of(body)`). `render.rs` and `eval/answer.rs`, which downcast
  `ClientError::SpendCapExceeded`, downcast `LlmError` instead.
- Retries (408/409/429/5xx + `retry-after`) move into the backends' shared HTTP helper in
  `judge-llm` (`http.rs`): both backends have identical retry semantics, so one loop.
- `judge-anthropic` keeps: `wire.rs`, the Anthropic schema subset, request/response
  conversion, `Endpoint` (§4.1). It loses the typestate and the spend cap.
- `judge_bot::build_deps(pool, client, embedder)` becomes
  `build_deps(pool, models: Models, embedder)` where `Models { extract: Arc<dyn ChatModel>,
  synth: Arc<dyn ChatModel> }` and both share one `Spend` (one cap per process, as today).
  The HTTP API and Discord layer read `spent_usd()`/`calls()` from a `SpendMeter` handle
  instead of the client.

## 4. Backends

### 4.1 Anthropic Messages API — one wire format, several doors

The body is the Messages API everywhere; what differs is auth, URL and a feature mask.
`Endpoint` is an enum so a new door is an exhaustive-match compile error, not a config typo:

| `Endpoint` | Auth | URL | Model id | Mask |
|---|---|---|---|---|
| `Direct { base_url, api_key }` | `x-api-key` | `{base}/v1/messages` | `claude-opus-5` | none (today) |
| `Proxy { base_url, api_key, header }` | `x-api-key` or `Authorization: Bearer` | `{base}/v1/messages` | whatever the proxy routes | `fallbacks` off (a proxy will not know the beta) |
| `ClaudePlatformOnAws { region, workspace_id }` | SigV4, service `aws-external-anthropic` | `https://aws-external-anthropic.{region}.api.aws/v1/messages` | bare | none (same-day parity) |
| `Bedrock { region, url }` | SigV4, service `bedrock` | Bedrock's Messages-shaped ("Mantle") endpoint | `anthropic.`-prefixed | `fallbacks` off |
| `Vertex { project, region }` | GCP ADC bearer token | Vertex's Anthropic publisher endpoint, `anthropic_version` in body | bare | `fallbacks` off |

Verified against the platform table in the claude-api skill (2026-09-02): structured outputs,
strict tools, adaptive thinking/effort and prompt caching are GA on Bedrock and Vertex;
server-side `fallbacks` is first-party (and Claude Platform on AWS) only, so the mask turns
`SynthConfig::fallbacks` off with a warning rather than sending a beta the door rejects. The
exact Bedrock/Vertex path formats are **to be verified at implementation time** from the
live docs; the design makes the URL a config value precisely so an operator can correct it
without a release.

Auth dependencies are optional Cargo features of `judge-anthropic` (`aws` → `aws-config` +
`aws-sigv4`; `gcp` → `gcp_auth`), on by default in the published image, off for anyone who
wants a lean build. Credentials come from the platforms' standard chains (env, profile,
instance role / ADC), never from `judge.toml`.

### 4.2 OpenAI-compatible chat completions

`crates/openai`: hand-written like the Anthropic client (we own the types; no SDK). Mapping:

| Neutral | Chat completions |
|---|---|
| `system` blocks | one `system` message (joined; `cache_control` forwarded per block when `cache_hints = true`, which LiteLLM honours for Anthropic upstreams) |
| `Turn::User` | `user` message |
| `Turn::Assistant(raw)` | the `choices[0].message` object replayed verbatim (keeps `tool_calls`, `reasoning_content`, anything else) |
| `Turn::ToolResults` | one `tool` message per result with `tool_call_id` |
| `ToolSpec` | `tools: [{type: function, function: {name, description, parameters, strict}}]` |
| `ToolChoice::Auto { parallel: false }` | `tool_choice: "auto"`, `parallel_tool_calls: false` |
| `ToolChoice::None` | `tool_choice: "none"` |
| `OutputSchema` | `response_format: {type: json_schema, json_schema: {name, schema, strict: true}}`; or `{type: json_object}` + schema in the prompt; or prompt only — by `structured_output` knob |
| `Effort` | `reasoning_effort` when `reasoning_effort = true` |
| `max_tokens` | `max_completion_tokens` (OpenAI) or `max_tokens` (most compatible servers) — `max_tokens_param` knob |
| `stop` | `finish_reason`: `stop`→EndTurn, `length`→MaxTokens, `tool_calls`→ToolUse, `content_filter`→Refusal; a non-null `message.refusal` → Refusal |
| `usage` | `prompt_tokens`, `completion_tokens`, `prompt_tokens_details.cached_tokens` → cache_read |
| tool call arguments | `function.arguments` is a **string**; parsed with serde, then validated against `LookupRulesInput` exactly as today |

Dialect knobs live on the provider entry in `judge.toml` (§5), with defaults that are right for
OpenAI and LiteLLM. Azure OpenAI needs `api-key` header + `?api-version=` query: the `auth`
knob covers the header, `base_url` can carry the query. Not modelled: Responses API
(`/v1/responses`); if a future model is Responses-only, that is a third backend, not a knob.

### 4.3 Embeddings

`judge-embed` gains `OpenAiEmbedder` over `POST /v1/embeddings` (`input`, `model`, optional
`dimensions`). `InputKind` is ignored (no query/document distinction in that API). The
Voyage adapter is unchanged. Both are chosen by `[models.embed]`.

**Vector space identity.** Vectors from two models cannot share a column, and pgvector's
HNSW index needs a fixed width, so:

- a one-row table `embedding_space (provider, model, dimensions, created_at)` records what the
  stored vectors are; `ingest embed` writes it on first use and refuses to run when the
  configured model differs;
- the retriever compares its configured embedder with `embedding_space` at startup; on a
  mismatch the vector leg is **disabled with an error-level log**, never silently mixed;
- a new `ingest reembed --yes` switches spaces inside one transaction: `ALTER COLUMN
  embedding TYPE vector(N)`, drop/recreate the partial HNSW indexes, `SET embedding = NULL`
  everywhere, rewrite `embedding_space`, then run the normal embed loop. It prints the row
  counts and the estimated cost before `--yes` is honoured, because re-embedding pays the
  provider per row (the reason `scripts/backup-db.sh` exists).

The migrations keep `vector(1024)` as the initial width; `reembed` is how it changes.

## 5. Configuration

**Zero config keeps working.** With no `judge.toml`, the binaries build exactly today's
setup from `.env`: Anthropic direct with `ANTHROPIC_API_KEY`, `claude-opus-5` for both
stages, Voyage if `VOYAGE_API_KEY` is set. So the existing deployment, the eval numbers and
the pinned prompt digest are untouched by upgrading.

A `judge.toml` (path from `JUDGE_CONFIG`, default `./judge.toml` if present; mounted into the
containers) selects providers and models:

```toml
# Secrets are named by environment variable, never written here.
[providers.anthropic]
kind = "anthropic"                 # anthropic | openai
endpoint = "direct"                # direct | proxy | claude-platform-on-aws | bedrock | vertex
api_key_env = "ANTHROPIC_API_KEY"

[providers.litellm]
kind = "openai"
base_url = "http://litellm:4000/v1"
api_key_env = "LITELLM_KEY"
# dialect knobs, all optional; shown with defaults
structured_output = "json_schema"  # json_schema | json_object | prompt
strict_tools = true
reasoning_effort = false
max_tokens_param = "max_tokens"    # max_tokens | max_completion_tokens
cache_hints = false

[providers.ollama]
kind = "openai"
base_url = "http://ollama:11434/v1"
structured_output = "json_object"
pricing = "free"                   # local: the spend cap never trips on this provider

[models.extract]
provider = "ollama"
model = "qwen3:8b"
max_tokens = 2000

[models.synth]
provider = "anthropic"
model = "claude-opus-5"
effort = "high"
max_tokens = 16000
# USD per million tokens. Required when the model is not in the built-in table,
# unless the provider is `pricing = "free"`; a model the cap cannot price is a
# startup error, not a silent under-estimate.
[models.synth.pricing]
input = 5.0
output = 25.0
cache_read = 0.5
cache_write = 6.25

[models.embed]
provider = "voyage"                # kind = "voyage" provider, implied when absent
model = "voyage-3.5"
dimensions = 1024
```

Loaded into typed structs with `serde` + `nutype` validators (non-empty model ids, finite
prices, `dimensions > 0`, env var *present and non-blank* at load time, unknown keys
rejected with `deny_unknown_fields` so a typo is an error). `judge-cli config` prints the
resolved configuration with secrets redacted, and every binary logs the same summary at
startup (`extract=ollama/qwen3:8b synth=anthropic/claude-opus-5 embed=voyage/voyage-3.5
cap=$5.00`).

**Spend cap.** Unchanged in mechanism; the price table becomes `(provider, model) →
Pricing` merged from the built-in table (Anthropic first-party models, kept current) and the
config. The current "unknown model prices as Opus 5" rule stays for the `anthropic` kind
(erring high is safe there) and becomes an error for the `openai` kind (an unknown model
there could be anything; the operator must say). `pricing = "free"` bypasses the reservation
for that provider and still counts calls.

## 6. Things that stay exactly the same

- `crates/core`: no change at all. `Deps`, `judge()`, `Verdict::validate`, `Citation`.
- The synthesis and extraction prompts, the `Harness` enum and the pinned SHA-256 of the
  Anthropic rendering. An OpenAI backend gets the same `Harness::Tool` prompt (the tool is
  still named `lookup_rules`; the wire format is the backend's business).
- The agent surface (`crates/agent`, sessions, MCP, CLI, the skill): the calling agent is
  the model there, so providers do not apply. `judge` (the built-in pipeline tool) reports
  which model it is running in its `unavailable`/`answer` replies, nothing more.
- Retrieval, ratings, retirement, renumbering, the HTTP API, the web page, deployment.

## 7. Eval

`judge-eval answer` gains `--config <judge.toml>` and stores `{provider, model}` per stage in
the run file, so `show`/`rescore` can compare two providers on the same gold set. The recall
gate is unaffected (no model). A cheap smoke target: run `answer --limit 3` against a local
Ollama model in CI-like conditions with `pricing = "free"` — not for quality, for "the
OpenAI backend still round-trips a tool call and a verdict".

## 8. Implementation phases (each: build → adversarial review → fix → commit)

1. **Seam, no behaviour change.** `crates/llm` with the neutral types, `Metered`, the moved
   typestate and `classify`; `crates/anthropic` becomes a backend; `bot/extract.rs` and
   `bot/synth.rs` re-targeted. Acceptance: every existing test passes; a new golden test
   proves the Anthropic request bodies and headers are **byte-identical** to before for all
   four call shapes; the prompt digest is unchanged; `build_deps` callers (bot, api, eval,
   agent) compile with the new signature and zero-config startup is unchanged.
2. **Configuration + OpenAI chat backend.** `judge.toml` loader, `Endpoint::Proxy`,
   `crates/openai`, the OpenAI strict-schema transform, price table from config, `judge-cli
   config`. Acceptance: wiremock twins of the synth tool-round test and the extractor tests
   against a mocked chat-completions server (including `arguments`-as-string, `length`,
   `content_filter`, `refusal`, and replay of `reasoning_content`); a config with a typo or
   an unpriced OpenAI model fails at startup with a message naming the key.
3. **Embeddings.** `OpenAiEmbedder`, `embedding_space` table, retriever startup check,
   `ingest reembed`. Acceptance: `#[sqlx::test]` for the mismatch refusal and for `reembed`
   changing the column width and clearing vectors atomically.
4. **Cloud doors for Anthropic.** `Endpoint::{ClaudePlatformOnAws, Bedrock, Vertex}` behind
   Cargo features, with the URL formats verified against the live docs and the feature mask
   tested (no `fallbacks` beta on masked doors). Acceptance: wiremock tests asserting the
   signed/bearer headers and paths; the image builds with the features on.
5. **Docs.** README "Choosing a model", `judge.example.toml`, `.env.example`, `docs/DEPLOYMENT.md`
   (mounting the config, cloud credentials), `CLAUDE.md` architecture notes.

## 9. Open decisions

1. **Config file vs. environment only.** This proposal chooses a TOML file with env-only as
   the zero-config default. The alternative (a dozen `JUDGE_*_MODEL`/`_PROVIDER`/`_BASE_URL`
   variables) fits Docker `env_file` better but cannot express per-provider dialect knobs or
   pricing without becoming a naming scheme of its own.
2. **Native cloud auth (phase 4) or gateway-only.** Bedrock and Vertex are reachable in
   phase 2 through LiteLLM or their OpenAI-compatible endpoints with no new dependencies.
   Native SigV4/ADC support costs ~two auth crates and a Cargo feature; it matters to an
   operator who wants Claude on their cloud account *without* running a proxy.
3. **Re-embedding UX.** `ingest reembed --yes` as above, or simply refuse and tell the
   operator to restore/re-run `ingest embed` after a manual `ALTER`. The command is safer;
   the refusal is less code.

All three were decided as recommended: the TOML file with the env-only zero-config default,
native SigV4/ADC behind Cargo features, and `ingest reembed --yes`.

## 10. As built — deviations from the sketch above

Recorded from the phase commits and the code comments; the sketch is left as written so
the reasoning stays readable.

**Seam (phase 1).** `ChatRequest` carries `thinking` and `fallbacks` in addition to the
fields in §3.1 — needed for the Anthropic bodies to stay byte-identical (the golden test in
`crates/bot/tests/anthropic_golden.rs` replays fixtures captured at `ebac125`). The port is
split: providers implement an open `Backend` trait, and the pipeline's `ChatModel` is
*sealed* with `Metered<B>` as its only implementation, so the cap is a fact about the
types rather than a composition discipline; `Models` has private fields for the same
reason. `Price` is a closed sum `{Free, Table, PerToken}` (not a table lookup at call
time): `Table` re-reads the built-in price for the model the response names (an Anthropic
fallback may route elsewhere), `PerToken` is the operator's rate and settles at exactly
that, `Free` never reserves. The worst-case reservation is sized from the serialized
*neutral* request rather than the wire body (a few percent larger, pessimistic either
way). The per-call log line is `llm call` with a `provider` field, not `anthropic call`.

**Configuration and the OpenAI backend (phase 2).** Where §3.1 says a backend without
server-side enforcement gets the schema "in the system prompt", the adapters append it to
the *user turn* (`judge_llm::schema_block`), so the pinned system-prompt digest holds on
every backend, not only Anthropic's. A knob that would be silently ignored is a load error
naming both keys (`auth` without `api_key_env`, `effort` on an `openai` provider with
`reasoning_effort = false`, a stage price on a `pricing = "free"` provider, a cloud-door
key on a keyed door). `base_url` is validated as an absolute http(s) URL with a host at
load. `cache_write` in `[models.<stage>.pricing]` defaults to 1.25× `input` (Anthropic's
write premium), `cache_read` to `input`. `openai` providers gained `auth = "bearer" |
"api-key"` (Azure's header) and, for embeddings, `send_dimensions`. `judge.toml` is
gitignored (per host, not secret); `judge.example.toml` is the tracked reference. The
loader is hermetic (`from_toml`/`from_vars` read `JUDGE_MAX_USD` through the injected
environment) so its tests never touch the process environment.

**Embeddings (phase 3).** The retriever's check is not only "at startup": `db::Vectors`
re-reads `embedding_space` on every use (and once at startup, for the log), so a running
bot picks up the first `ingest embed` and goes dark — never mixed — under a `reembed`
without a restart. The adapters hold no bare `Embedder`; `PgRetriever`, `PgLibrary` and
`PgCallStore` take `Vectors`, which embeds nothing until the stored space equals its own.
Writers hold the space under the shared side of `CALLS_REWRITE_LOCK` and `switch_space`
takes the exclusive side, so a switch waits for in-flight writes. `ingest embed` writes the
row with the *first vector it writes*, never before, and never relabels vectors it did not
write; the migration seeds `voyage/voyage-3.5/1024` for a database that already held
vectors. `reembed` probes the configured embedder with one short text before clearing
anything, so a wrong key, URL, model or width fails with the old vectors intact.
`dimensions` is bounded to `1..=2000` (pgvector's HNSW limit) at load, and
`VOYAGE_DIMENSIONS` gets the same bound. The space's provider is the *kind*
(`voyage | openai`), not the operator's table name.

**Cloud doors (phase 4).** The formats verified on 2026-09-02 differ from the table in
§4.1: Bedrock's Messages-shaped endpoint is
`https://bedrock-mantle.{region}.api.aws/anthropic/v1/messages`, signed as service
`bedrock-mantle` (not `bedrock`), and its documentation lists structured outputs, strict
tools, fallbacks and betas as unsupported — so Bedrock masks all of those, not only
`fallbacks` (the schema goes in the prompt, validation is client-side as always). Claude
Platform on AWS requires the `anthropic-workspace-id` header, hence the `workspace_id`
key. Vertex puts the model in the URL and `anthropic_version` in the body
(`wire::ModelField`, an exhaustive enum rather than two `Option`s), with the origin
depending on whether the region is `global`, a multi-region or a specific one. The
credential chains are lazy (loading never touches the network) and `Config::probe_auth`
resolves each door once at startup, so an empty chain fails there naming the provider and
the door. Each provider table resolves to one `Endpoint`, shared by the stages that name
it. `region`, `project` and `workspace_id` are validated as the shapes the platforms
document; a Bedrock model id must carry a whole `anthropic` segment. API-key auth for the
cloud doors is not supported. The features are forwarded through `judge-bot` (`aws`,
`gcp`, both default) and named in the Dockerfile.

**Docs (phase 5).** `JUDGE_CONFIG` is a *host* path in `.env`; `docker-compose.yml`
bind-mounts it into `bot`, `api` and `refresh` and sets the containers' `JUDGE_CONFIG` to
the mounted path (`${JUDGE_CONFIG:+…}`), so the one variable serves `cargo run` and
compose alike — §5's "mounted into the containers" without a second variable. Two
consequences the docs state: a `./judge.toml` with `JUDGE_CONFIG` blank is read by
`cargo run` and by nothing under Docker, and a changed file needs `docker compose
restart`, not `up -d` (a bind mount's content is not a configuration change). The
shipped `judge.example.toml` is pinned by two tests in `config.rs` — as shipped, and
with every commented table uncommented and each door named by a stage — so a renamed
knob fails the gate instead of the operator. `docker-compose.override.yml` (the
documented home for cloud-credential mounts) is gitignored.
