# Model providers

The reference for how the judge reaches its models: the provider-neutral seam
(`crates/llm`), the two chat backends (`crates/anthropic`, `crates/openai`), the two
embedding backends (`crates/embed`), and the `judge.toml` file that chooses among them.
Implemented 2026-09-02 from a proposal; the reasoning behind the shape is recorded in
`docs/DECISIONS.md` (D6–D9). This file describes what exists. The pipeline, the prompts,
the validation and the typestates do not know which provider is on the other end of the
HTTP connection; that is the whole point of the seam.

## 1. Goal and non-goals

**Goal.** An operator who is not the author can run the judge with:

- Anthropic models through the first-party API, through Claude Platform on AWS, Amazon
  Bedrock or Google Vertex AI (Anthropic's Messages API shape, different auth and
  hostnames), or through a proxy that speaks the Messages API (LiteLLM's `/v1/messages`);
- any model behind an **OpenAI-compatible chat completions** endpoint: LiteLLM, OpenRouter,
  Ollama, vLLM, llama.cpp, Azure OpenAI, Bedrock's and Vertex's OpenAI-compatible
  endpoints, OpenAI itself;
- embeddings from Voyage or any OpenAI-compatible `/v1/embeddings` endpoint;
- a **different model per stage**: a cheap model for extraction, a strong one for synthesis,
  possibly on different providers;
- a spend cap that still means something when the price table does not know the model.

**Non-goals.** Streaming (every request keeps `max_tokens` ≤ 16k, inside the
non-streaming guidance). A plugin system: providers are Rust crates in this workspace,
chosen by configuration. Supporting every quirk of every OpenAI-compatible server: the
OpenAI backend has a small, explicit set of dialect knobs and otherwise follows OpenAI's
documented API. The Responses API (`/v1/responses`) is not modelled; a Responses-only model
would be a third backend, not a knob.

## 2. What the pipeline asks of a model

Four request shapes, all non-streaming, all with the answer decoded by serde into a type
that *is* the schema (invariant I5 in `docs/DECISIONS.md`):

| # | Call | System | Tools | Tool choice | Structured output | Effort |
|---|---|---|---|---|---|---|
| 1 | Extraction | cached, stable | none | — | `Extraction` schema | low |
| 2 | Synthesis, first turn | cached, stable | `lookup_rules`, strict | auto, ≤1 call | `Verdict` schema | high (medium on truncation retry) |
| 3 | Synthesis, tool continuation | same | same | same (cache-preserving) | same | same |
| 4 | Synthesis, citation retry | same | `lookup_rules` still listed | **none** | same | same |

Plus: refusal detection (`stop_reason: refusal` → `JudgeError::LlmRefused`), truncation
detection (`max_tokens` → `Truncated`, retried once at lower effort), usage for the spend
cap, and the model's *own* assistant turn echoed back verbatim in call 3 (thinking blocks
with signatures on Anthropic; `reasoning_content` on DeepSeek-style servers; `tool_calls`
on OpenAI). Embeddings need `embed(texts, kind) -> Vec<Vec<f32>>` at a fixed width.

Everything that makes the judge trustworthy is **client-side**: citations are validated
against `Context` (I3), the tool round is bounded by the typestate (I4), decoding enforces
the schema (I5). Structured-output enforcement on the server is an *optimisation* that
lowers the retry rate; a backend without it degrades to more retries, not to weaker
guarantees. That is what makes providers configurable without touching `core`.

`crates/bot/tests/anthropic_golden.rs` pins the four Anthropic request shapes
byte-for-byte against captured fixtures (`UPDATE_GOLDEN=1` re-captures them after an
intended prompt or schema change; review the diff).

## 3. Where the seam is

```
core ← llm ← { anthropic, openai }        (chat backends)
core ← embed { voyage, openai-embeddings } (embedding backends)
bot  ← llm, embed                          (adapters + build_deps; picks backends by config)
```

`crates/llm` (`judge-llm`) holds the neutral types, the port, the one-tool-round typestate
(`Synth<Fresh | ToolRequested | Final>`), `classify` (refusal / truncation / tool /
verdict over a neutral response), the spend cap and the shared HTTP retry loop
(408/409/429/5xx honouring `retry-after`, identical for every backend). `crates/anthropic`
and `crates/openai` are *backends* of it, each owning its wire types and its schema-subset
transform. `crates/bot`'s `extract.rs` and `synth.rs` (prompts, rendering, budget,
hydration, truncation retry) depend on `judge-llm` only. `crates/core` knows nothing about
any of this.

### 3.1 The neutral chat types

```rust
// crates/llm: what providers implement …
#[async_trait]
pub trait Backend: Send + Sync {
    async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, LlmError>;
    fn capabilities(&self) -> Capabilities;
    fn model(&self) -> &str;                  // as the backend will bill it
    fn provider(&self) -> &'static str;       // the price table and the log line key on it
}
// … and what the pipeline calls. Sealed: `Metered<B>` is its only implementation,
// so every send is behind the spend cap by type, not by composition discipline.
pub trait ChatModel: sealed::Sealed + Send + Sync { /* complete, capabilities, model */ }

pub struct ChatRequest {
    pub max_tokens: u32,
    pub system: Vec<TextBlock>,           // text + CacheHint
    pub turns: Vec<Turn>,
    pub tools: Vec<ToolSpec>,             // name, description, JSON Schema, strict
    pub tool_choice: ToolChoice,          // Auto { parallel: bool } | None
    pub output: Option<OutputSchema>,     // the JSON Schema of the type we decode into
    pub effort: Option<Effort>,           // Low | Medium | High | XHigh | Max
    pub thinking: bool,                   // extended reasoning where a backend offers it as an opt-in
    pub fallbacks: Option<RefusalFallback>, // Default | Models(Vec<String>): server-side refusal fallback
}                                         // (both carried so the Anthropic bodies stay byte-identical)

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
    pub usage: Usage,                    // input, output, cache_read, cache_write (0 when unknown)
    pub model: String,                   // as billed (may differ under fallbacks)
    pub assistant: AssistantTurn,        // for replay
}
```

- **`AssistantTurn` is opaque.** The neutral layer never inspects it; it exists so the
  continuation request can replay it. A backend receiving an `AssistantTurn` tagged with
  another backend's name returns `LlmError::ForeignTurn` rather than guessing.
- **`CacheHint` is a hint.** Anthropic emits `cache_control`; OpenAI ignores it (it caches
  by prefix automatically); LiteLLM forwards `cache_control` when the upstream is
  Anthropic. Placement rules (stable content first) are the adapters' business.
- **`OutputSchema` carries the schemars schema untransformed.** Each backend applies *its*
  subset transform: Anthropic's (`additionalProperties:false`, `oneOf→anyOf`, strip
  constraints outside the subset) or OpenAI strict mode's (additionally: every property
  listed in `required`, optional fields become `anyOf [T, null]`, no `format`, no
  `default`). Both are pure functions with walk-and-assert tests. Decoding is unchanged,
  so a stripped constraint is still enforced client-side.
- **`Effort` maps per backend**: Anthropic `output_config.effort`; OpenAI
  `reasoning_effort` (`low|medium|high`, `xhigh|max → high`) only when the provider's
  `reasoning_effort = true`; asking for effort on a provider that cannot send it is a
  *load error*, not a silent drop.
- **`Capabilities`** = `{ structured_output: Enforced | JsonMode | PromptOnly, strict_tools,
  effort, cache_hints, refusal_fallbacks }`. The adapters use it for two things: to append
  the schema to the **user turn** (`judge_llm::schema_block`) when enforcement is
  `PromptOnly`/`JsonMode`, so the pinned system-prompt digest holds on every backend; and
  to log what they are relying on.

### 3.2 The spend cap

`Metered<B>` wraps a `Backend`: reserve the worst case → send → settle on the usage the
response reports. The reservation is sized from the serialized *neutral* request (a few
percent larger than the wire body, pessimistic either way). `Price` is a closed sum:

- `Table`: the built-in table (`judge_llm::PRICES`, Anthropic first-party models), re-read
  for the model the *response* names, because an Anthropic fallback may route elsewhere;
- `PerToken`: the operator's rate from `[models.<stage>.pricing]`, settled at exactly that;
- `Free`: never reserves, still counts calls (`pricing = "free"` on a provider).

One `SpendMeter` per process, shared by both stages and by every clone; `JUDGE_MAX_USD`
is the cap. A 2xx body that fails to decode is still billed (each backend exposes a
lenient `usage_of(body)`). The per-call log line is `llm call` with a `provider` field.
`Models::{single, pair, priced}` take the meter and bare backends and meter them
themselves; the fields are private, so there is no uncapped model and no foreign meter.

## 4. Backends

### 4.1 Anthropic Messages API — one wire format, several doors

The body is the Messages API everywhere; what differs is auth, URL and a feature mask.
`Endpoint` is an enum, so a new door is an exhaustive-match compile error, not a config
typo. Formats verified against the live docs 2026-09-02:

| `Endpoint` | Auth | URL | Model id | Mask |
|---|---|---|---|---|
| `Direct { base_url, api_key }` | `x-api-key` | `{base}/v1/messages` | `claude-opus-5` | none |
| `Proxy { base_url, api_key, header }` | `x-api-key` or `Authorization: Bearer` | `{base}/v1/messages` | whatever the proxy routes | `fallbacks` off |
| `ClaudePlatformOnAws { base_url, region, workspace_id, credentials }` | SigV4, service `aws-external-anthropic`, header `anthropic-workspace-id` | `https://aws-external-anthropic.{region}.api.aws/v1/messages` | bare | none |
| `Bedrock { base_url, region, credentials }` | SigV4, service `bedrock-mantle` | `https://bedrock-mantle.{region}.api.aws/anthropic/v1/messages` | `anthropic.`-prefixed (a whole `anthropic` segment is required) | `fallbacks`, `output_config.format`, tool `strict` and every `anthropic-beta` off; the schema goes in the prompt |
| `Vertex { base_url, project, region, token }` | GCP ADC bearer token | model in the URL, `anthropic_version` in the body (`wire::ModelField`); origin depends on `global`, a multi-region or a specific region | bare | `fallbacks` off |

Every door's `base_url` is derived from the region (or project) and can be overridden in
the provider table, so a changed platform hostname is a config edit, not a release.

Structured outputs, strict tools, adaptive thinking/effort and prompt caching are GA on
Bedrock's and Vertex's *native* Anthropic endpoints; server-side `fallbacks` is
first-party (and Claude Platform on AWS) only, so the masked doors turn
`SynthConfig::fallbacks` off with a warning rather than sending a beta the door rejects.

The cloud doors sit behind `judge-anthropic`'s `aws` (aws-config + aws-sigv4) and `gcp`
(gcp_auth) Cargo features, on by default, forwarded from `judge-bot`'s own features and
named in the Dockerfile; a lean build cannot even name them and the loader says "not
built". Credentials come from the platforms' standard chains (env, profile, instance
role / ADC), never from `judge.toml`; API-key auth for the cloud doors is not supported.
The chains are lazy (loading never touches the network) and `Config::probe_auth`
resolves each door once at startup, so an empty chain fails there, naming the provider and
the door, rather than per question. Each provider table resolves to one `Endpoint`,
shared by the stages that name it.

### 4.2 OpenAI-compatible chat completions

`crates/openai`: hand-written like the Anthropic client (we own the types; no SDK).

| Neutral | Chat completions |
|---|---|
| `system` blocks | one `system` message: the texts joined, or content parts each carrying its `cache_control` when `cache_hints = true` (which LiteLLM honours for Anthropic upstreams) |
| `Turn::User` | `user` message |
| `Turn::Assistant(raw)` | the `choices[0].message` object replayed verbatim (keeps `tool_calls`, `reasoning_content`, anything else) |
| `Turn::ToolResults` | one `tool` message per result with `tool_call_id` |
| `ToolSpec` | `tools: [{type: function, function: {name, description, parameters, strict}}]` |
| `ToolChoice::Auto { parallel: false }` | `tool_choice: "auto"`, `parallel_tool_calls: false` |
| `ToolChoice::None` | `tool_choice: "none"` |
| `OutputSchema` | `response_format: {type: json_schema, json_schema: {name, schema, strict: true}}`; or `{type: json_object}` + schema in the user turn; or prompt only — by the `structured_output` knob |
| `Effort` | `reasoning_effort` when `reasoning_effort = true` |
| `max_tokens` | `max_completion_tokens` (OpenAI) or `max_tokens` (most compatible servers) — `max_tokens_param` knob |
| `stop` | `finish_reason`: `stop`→EndTurn, `length`→MaxTokens, `tool_calls`→ToolUse, `content_filter`→Refusal; a non-null `message.refusal` → Refusal |
| `usage` | `prompt_tokens − prompt_tokens_details.cached_tokens` → input, `cached_tokens` → cache_read, `completion_tokens` → output |
| tool call arguments | `function.arguments` is a **string**; parsed with serde, then validated against `LookupRulesInput` exactly as on Anthropic |

Dialect knobs live on the provider table (§5) with defaults that are right for OpenAI and
LiteLLM. `auth = "bearer" | "api-key"` covers Azure's header; `base_url` can carry Azure's
`?api-version=` query.

### 4.3 Embeddings

`judge-embed` has `VoyageEmbedder` and `OpenAiEmbedder` (`POST /v1/embeddings` with
`input`, `model` and, unless `send_dimensions = false`, `dimensions`; `InputKind` is
ignored there because that API has no query/document distinction). Both are chosen by
`[models.embed]`; without one, `VOYAGE_API_KEY` selects Voyage and a blank key turns the
vector leg off.

**Vector space identity.** Vectors from two models cannot share a column, and pgvector's
HNSW index needs a fixed width, so every embedder implements `WithSpace`: a `Space` of
provider *kind* (`voyage | openai`, not the operator's table name), model and dimensions.
`Space::check` (pure, in `judge_embed::space`) is the only definition of "same space".

- The one-row table `embedding_space` (migration `20260904000001`, seeded
  `voyage/voyage-3.5/1024` for a database that already held vectors) records what the
  stored vectors are. `ingest embed` writes it with the *first vector it writes*, never
  before, refuses on a mismatch or when the columns' actual `vector(N)` typmod differs
  (`db/space.rs` `column_width`), and never relabels vectors it did not write.
- The adapters hold no bare `Embedder`: `PgRetriever`, `PgLibrary` and `PgCallStore` take
  `Arc<db::Vectors>` (`Config::vectors(pool)`, one per process), which embeds nothing until
  the stored space equals its own. The row is re-read on **every use** (and once at
  startup, so the verdict sits beside the config summary), so a running bot picks up the
  first `ingest embed`, and a `reembed` under it darkens the vector legs — an error-level
  log naming both spaces — instead of erroring or mixing.
- Writers hold the space: `PgCallStore::persist` and every `ingest embed` batch take the
  shared side of `CALLS_REWRITE_LOCK` in their transaction and read the row under it
  (`hold_space` / `Vectors::hold`); `switch_space` takes the exclusive side (as the CR
  loader and the retirement pass do), so a switch waits for in-flight writes and a write
  after it sees the new row.
- `ingest reembed --yes` (`switch_space`) is the only thing that changes the row and the
  column width, in one transaction: retype the vector columns, drop and recreate the HNSW
  indexes (definitions in `VECTOR_TABLES`, verbatim from the migrations), NULL every
  vector, rewrite `embedding_space`, then run the normal embed loop. It probes the
  embedder with one short text first, so a wrong key, URL, model or width fails with the
  old vectors intact. When the database already holds the configured space it only fills
  NULL rows (idempotent; the resume after an interrupted refill); `--clear` re-pays every
  row in the same space. Without `--yes` it prints the row counts and a rough cost, exits
  non-zero and changes nothing.
- `config::Dimensions` is `1..=2000` (HNSW's limit) at load; `VOYAGE_DIMENSIONS` has the
  same bound. The migrations create `vector(1024)`; `reembed` is how it changes.

## 5. Configuration

**Zero config keeps working.** With no `judge.toml`, the binaries build today's setup
from `.env`: Anthropic direct with `ANTHROPIC_API_KEY`, `claude-opus-5` for both stages,
Voyage if `VOYAGE_API_KEY` is set. The eval numbers and the pinned prompt digest were
produced on that setup, and upgrading never changes it.

A `judge.toml` (path from `JUDGE_CONFIG`, else `./judge.toml` if present) selects
providers and models. `judge.example.toml` documents every knob with its default and is
pinned by two tests in `config.rs` (as shipped, and with every commented table uncommented
and each door named by a stage), so a renamed knob fails the gate, not the operator.

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
cache_read = 0.5                   # defaults to `input`
cache_write = 6.25                 # defaults to 1.25 × `input`, Anthropic's write premium

[models.embed]
provider = "voyage"                # kind = "voyage" provider, implied when absent
model = "voyage-3.5"
dimensions = 1024
```

Rules the loader enforces, all at load time with a message naming the key:

- typed structs with `deny_unknown_fields`, so a typo is an error; `nutype` validators for
  `BaseUrl` (absolute http(s) with a host), `Region`, `Project`, `WorkspaceId` (the shapes
  the platforms document) and `Dimensions`;
- every `api_key_env` a stage names must be present and non-blank, read once into a
  redacted `ApiKey` (a table no stage names is parsed, its key never read);
- a knob that would be silently ignored is an error naming both keys: `auth` without
  `api_key_env`, `effort` on an `openai` provider with `reasoning_effort = false`, a stage
  price on a `pricing = "free"` provider, a cloud-door key on a keyed door;
- a model on an `openai` provider must be priced or its provider `pricing = "free"`; the
  built-in table errs high for unknown *Anthropic* models only (erring high is safe there;
  an unknown OpenAI-compatible model could be anything).

The loader is hermetic (`from_toml`/`from_vars` read `JUDGE_MAX_USD` and the keys through
an injected environment), so its tests never touch the process environment. Every binary
logs `Config::summary()` at startup (`config=<file or "env"> extract=ollama/qwen3:8b
synth=anthropic/claude-opus-5 embed=voyage/voyage-3.5 cap=$5.00`); `judge-cli config`
prints the same, secrets redacted.

**Under Docker.** `JUDGE_CONFIG` in `.env` is a *host* path (`./judge.toml`, as `cargo run`
reads it). `docker-compose.yml` bind-mounts that file into `bot`, `api` and `refresh` at
`/etc/judgebot/judge.toml` and sets the containers' `JUDGE_CONFIG` to that path
(`${JUDGE_CONFIG:+…}`), so one variable serves `cargo run` and compose alike. Two
consequences: a `./judge.toml` with `JUDGE_CONFIG` blank is read by `cargo run` and by
nothing under Docker (the containers see only the mounted file, and blank mounts the
tracked example, which nothing reads); and editing the mounted file's content is not a
change `up -d` recreates for, so `docker compose restart bot api`. `judge.toml` is
gitignored (per host, not secret); `docker-compose.override.yml`, the documented home for
cloud-credential mounts, is too. `refresh` resolves all three stages of the file as the
others do, so every named provider's key must be in `.env` for it as well.

## 6. Things that do not depend on the provider

- `crates/core`: `Deps`, `judge()`, `Verdict::validate`, `Citation`. No provider type
  reaches it.
- The synthesis and extraction prompts, the `Harness` enum and the pinned SHA-256 of the
  system prompt. An OpenAI backend gets the same `Harness::Tool` prompt; the tool is still
  named `lookup_rules`, and the wire format is the backend's business.
- The agent surface (`crates/agent`, sessions, MCP, CLI, the skill): the calling agent is
  the model there, so providers do not apply. The built-in `judge` tool reports which
  model it is running in its replies, nothing more.
- Retrieval, ratings, retirement, renumbering, the HTTP API, the web page, deployment.

## 7. Eval

`judge-eval answer --config <judge.toml>` runs the gold set on another provider and stores
`{provider, model}` per stage in the run file, so `show` and `rescore` can compare two
providers on the same questions. The recall gate is unaffected (no chat model; `--vectors`
uses the configured embedder). A cheap smoke target for the OpenAI backend is `answer
--limit 3` against a local Ollama model with `pricing = "free"`: not for quality, for "the
backend still round-trips a tool call and a verdict".
