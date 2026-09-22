---
title: "Model choice (judge.toml)"
description: "Providers, models per stage, pricing for the spend cap, and embeddings."
sidebar:
  order: 4
---

With nothing but `.env`, the judge runs on Anthropic's first-party API: `claude-opus-5-5`
for both LLM stages, and Voyage `voyage-3.5` for embeddings if `VOYAGE_API_KEY` is set.
The eval numbers were measured on that setup. The prompts, and so the pinned prompt
digest, were tuned on its predecessor, Opus 5.

A `judge.toml` picks something else, such as a different model per stage on different
providers. The file is the one `JUDGE_CONFIG` names, else `./judge.toml` if present.
`judge.example.toml` shows every knob with its default. The file names secrets by
environment variable and never holds one.

Under Docker, a `./judge.toml` is read only when `.env` sets `JUDGE_CONFIG=./judge.toml`. The containers see only the file
compose mounts, never the repo root, so the `./judge.toml` default does not apply. Without
it they run the paid zero-config setup above, while `judge-cli config` on the host shows
your file.

```toml
[providers.ollama]
kind = "openai"                      # any OpenAI-compatible chat completions server
base_url = "http://ollama:11434/v1"
structured_output = "json_object"
pricing = "free"                     # local: the spend cap never reserves for it

[providers.anthropic]
kind = "anthropic"
endpoint = "direct"                  # direct | proxy | claude-platform-on-aws | bedrock | vertex
api_key_env = "ANTHROPIC_API_KEY"

[models.extract]                     # cheap stage: card-name spans + classification
provider = "ollama"
model = "qwen3:8b"

[models.synth]                       # the answer itself
provider = "anthropic"
model = "claude-opus-5-5"
effort = "high"
```

Two kinds of chat backend exist. `kind = "anthropic"` is the Messages API, reached through
one of five doors:

- `direct`: the first-party API.
- `proxy`: a gateway speaking `/v1/messages`, such as LiteLLM, with the key in
  `x-api-key` or `Authorization: Bearer`.
- `claude-platform-on-aws`: SigV4, a `region` and a `workspace_id`. Takes no key.
- `bedrock`: SigV4, a `region`, `anthropic.`-prefixed model ids. Takes no key.
- `vertex`: Google ADC, a `project` and a `region`. Takes no key.

Credentials for the three cloud doors come from the platform's own credential chain:
`AWS_*` variables, a profile, an instance role, `GOOGLE_APPLICATION_CREDENTIALS`. They are
checked once at startup, so a host with none fails there rather than on the first
question.

`kind = "openai"` is chat completions as OpenAI documents it. A few dialect knobs adjust
it: `structured_output`, `strict_tools`, `reasoning_effort`, `max_tokens_param`,
`cache_hints`. Their defaults suit OpenAI and LiteLLM. Ollama, vLLM, llama.cpp, OpenRouter
and Azure OpenAI work by setting knobs, with no code changes.

Some backends cannot enforce the output schema on the server (`json_object`, `prompt`,
Bedrock). They get the schema in the prompt instead. That can mean more citation retries,
but the guarantees are the same: the judge always decodes the answer and validates its
citations itself.

**The spend cap must be able to price every model.** `JUDGE_MAX_USD` reserves each call's
worst case before sending, so it needs a price per token.

- The built-in table knows Anthropic's first-party models. It prices an unknown Anthropic
  model as Opus 5, erring high.
- An `openai` provider has no safe guess. A model there needs a `[models.<stage>.pricing]`
  table (USD per million tokens), or the provider must say `pricing = "free"`. Otherwise
  startup fails with an error naming the stage.
- A price you write overrides the table, and the cap charges exactly that price.

Embeddings are chosen the same way: `[models.embed]` on a `voyage` provider or on any
`openai` one (`POST /v1/embeddings`). On an `openai` provider `dimensions` is required.
It is the width of the `vector(N)` columns.

A fresh database is created 1024 wide, Voyage's width. For an embedder of another width
(OpenAI's `text-embedding-3-small` is 1536), run `ingest reembed --yes` *instead of*
`ingest embed` the first time. It retypes the columns before it fills them. `ingest init`
does this by itself on a database that holds no vectors.

The database records which model's vectors it holds (`embedding_space`), and nothing
mixes two. A bot configured for another model logs an error and runs without vector
search. To switch models:

1. Run `cargo run --release -p judge-ingest -- reembed`. It prints the row counts and a
   rough cost, probes the new model once, and changes nothing.
2. Run it again with `--yes`. It retypes the columns, clears every vector and re-embeds
   them. That is paid per row, which is why it asks first.

Run with the switch already made, `reembed` only fills rows that are still empty.
`--clear` clears and re-pays every row on purpose.

`judge-cli config` prints the resolved setup with secrets redacted. Every binary logs the
same summary line at startup.

## What a cheaper model costs

The default is the expensive model. Both it and a cheaper one were measured on the
21-question gold set on 2026-09-22
([Evaluation](../../how-it-works/evaluation/#results) has the table, and the run files are
in `eval/published/`):

| | Answered (of 18 in scope) | Agree with the reference | Per question answered |
| --- | --- | --- | --- |
| `claude-opus-5-5` on both stages | 18 | 18 | $0.09 |
| `claude-sonnet-5` on both stages | 13 | 12 | $0.16, erring high |

Sonnet 5 costs half as much per token but came out no cheaper per answer. (Its dollar
figure errs high, because that run metered cache reads at the input price.) The prompts
are tuned on Opus. Sonnet misquoted three citations and twice broke the tool round by
asking for a second `lookup_rules` call, which the pipeline allows only once by design. A
rejected attempt is paid for twice: once for the attempt and once for the retry. It was
also several times slower. Nothing it did answer contradicted the reference.

So this is not a verdict on the model. Moving the synthesis stage to another model means
re-tuning `crates/bot/src/prompts/synth_system.md` for it against the gold set, and
running `judge-eval answer --config <your file>` before you trust it.

What does lower the bill without that work:

- **The extraction stage** is a small share of an answer's cost (about a third of a cent
  of ten on the default). A local model there (`pricing = "free"`) removes it. It is the
  safer stage to move: both models above separated the 3 out-of-scope questions from the
  18 in scope, which is the part of extraction the gold set measures.
- **`JUDGE_USER_LIMIT`, `API_RATE_LIMIT` and `JUDGE_BUDGET_PERIOD`** limit how many
  questions are asked, and the bill is proportional to that.
- **`/card` and `/rule`** answer the look-something-up questions without a model.
- **An embedder** costs a few cents once and improves what the model is shown.

`claude-haiku-4-5` cannot be used on either stage as the pipeline stands. Every request sets a reasoning
effort, and that model is documented to reject it.
