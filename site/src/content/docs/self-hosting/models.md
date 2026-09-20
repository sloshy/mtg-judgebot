---
title: "Model choice (judge.toml)"
description: "Providers, models per stage, pricing for the spend cap, and embeddings."
sidebar:
  order: 4
---

With nothing but `.env`, the judge runs on Anthropic's first-party API: `claude-opus-5`
for both LLM stages, and Voyage `voyage-3.5` for embeddings if `VOYAGE_API_KEY` is set.
The eval numbers and the pinned prompt digest were produced on that setup.

A `judge.toml` (named by `JUDGE_CONFIG`, else `./judge.toml` if present) picks something
else, such as a different model per stage on different providers. Under Docker the
`./judge.toml` default does not apply. The containers see only the file compose mounts,
never the repo root, so set `JUDGE_CONFIG=./judge.toml` in `.env`. Otherwise they run the
zero-config setup above, paid, while `judge-cli config` on the host shows your file.
`judge.example.toml` shows every knob with its default. The file names secrets by
environment variable and never holds one.

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
model = "claude-opus-5"
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

Credentials for the three cloud doors come from the platform's own chain: `AWS_*`
variables, a profile, an instance role, `GOOGLE_APPLICATION_CREDENTIALS`. They are probed
once at startup, so a host with none fails there rather than on the first question.

`kind = "openai"` is chat completions as OpenAI documents it, with a few dialect knobs
(`structured_output`, `strict_tools`, `reasoning_effort`, `max_tokens_param`,
`cache_hints`) whose defaults suit OpenAI and LiteLLM. Ollama, vLLM, llama.cpp, OpenRouter
and Azure OpenAI fit by turning knobs, not by code. A backend that cannot enforce the
output schema server-side (`json_object`, `prompt`, Bedrock) gets the schema in the prompt
instead. That costs more citation retries, not weaker guarantees. Decoding and citation
validation always happen client-side.

**The spend cap must be able to price every model.** `JUDGE_MAX_USD` reserves each call's
worst case before sending, so it needs a price per token. The built-in table knows
Anthropic's first-party models and prices an unknown Anthropic model as Opus 5 (erring
high). An `openai` provider has no safe guess, so a model there needs a
`[models.<stage>.pricing]` table (USD per million tokens) or the provider must say
`pricing = "free"`. Anything else is a startup error naming the stage. A price you write
beats the table and is what the cap settles at.

Embeddings are chosen the same way: `[models.embed]` on a `voyage` provider or on any
`openai` one (`POST /v1/embeddings`). On an `openai` provider `dimensions` is required.
It is the width of the `vector(N)` columns. A fresh database is created 1024 wide,
Voyage's width. For an embedder of another width (OpenAI's `text-embedding-3-small` is
1536), run `ingest reembed --yes` *instead of* `ingest embed` the first time (`ingest
init` does that by itself on a database that holds no vectors). It retypes
the columns before it fills them.

The database records which model's vectors it holds (`embedding_space`), and nothing
will mix two. A bot configured for another model logs an error and runs with the vector
leg dark. To switch, run `cargo run --release -p judge-ingest -- reembed`. It prints the
row counts and a rough cost and probes the new model once. With `--yes` it retypes the
columns, clears every vector and re-embeds them. That is paid per row, which is why it
asks first. Run again with the switch already made, it only fills rows still empty
(`--clear` clears and re-pays on purpose). `judge-cli config` prints what resolved,
secrets redacted, and every binary logs the same summary line at startup.

## What a cheaper model costs

The default is the expensive one, so the obvious question is what happens on a cheaper
model. It was measured on the 21-question gold set on 2026-09-20
([Evaluation](../../how-it-works/evaluation/#results) has the table, and the run files are
in `eval/published/`):

| | Answered (of 18 in scope) | Agree with the reference | Per question answered |
| --- | --- | --- | --- |
| `claude-opus-5` on both stages | 17 | 17 | $0.14 |
| `claude-sonnet-5` on both stages | 6 | 5 | $0.30, erring high |

Sonnet 5 is 40% of the price per token and came out no cheaper per answer (its dollars
here err high, because that run metered cache reads at the input price). The prompts are
tuned on Opus. On the hard questions Sonnet mostly returned answers with no citations, or
broke the tool round (asking for a second `lookup_rules` call, which the pipeline allows
once by design), and a rejected attempt is paid for twice, once for the attempt and once
for the retry. Nothing it did answer contradicted the reference. So this is not a verdict on the model. It says that moving the synthesis stage means re-tuning
`crates/bot/src/prompts/synth_system.md` against the gold set for the model you move to,
and running `judge-eval answer --config <your file>` before you trust it.

What does lower the bill without that work:

- **The extraction stage** is a small share of an answer's cost (about half a cent of
  twelve on the default). A local model there (`pricing = "free"`) removes it. Both
  models above separated the 3 out-of-scope questions from the 18 in scope, which is the
  part of extraction the gold set measures, so it is the safer stage to move.
- **`JUDGE_USER_LIMIT`, `API_RATE_LIMIT` and `JUDGE_BUDGET_PERIOD`** bound how many
  questions are asked, which is what the bill is proportional to.
- **`/card` and `/rule`** answer the look-something-up questions without a model.
- **An embedder** costs a few cents once and improves what the model is shown.

`claude-haiku-4-5` cannot be used on either stage as the pipeline stands: every request
sets a reasoning effort, which that model is documented to reject.
