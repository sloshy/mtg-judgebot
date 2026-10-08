---
title: "Model choice (judge.toml)"
description: "Providers, models per stage, pricing for the spend cap, and embeddings."
sidebar:
  order: 4
---

With nothing but `.env`, the judge runs on Anthropic's first-party API: `claude-opus-5-5`
for both LLM stages, and Voyage `voyage-3.5` for embeddings if `VOYAGE_API_KEY` is set.
The eval numbers were measured on that setup.

A `judge.toml` picks something else, such as a different model per stage on different
providers. The file is the one `JUDGE_CONFIG` names, else `./judge.toml` if present.
`judge.example.toml` shows every knob with its default. The file names secrets by
environment variable and never holds one.

The [config editor](../config-editor/) (`scripts/config.sh`, Models tab) is the way to
write it: it builds the file from forms, shows each endpoint only the keys it takes, checks
every change with the loader, and sets the `JUDGE_CONFIG` the containers need. The rest of
this page describes the file itself, for reading or editing it by hand.

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
effort = "medium"                    # the default for this model; see below
```

Two kinds of chat backend exist. `kind = "anthropic"` is the Messages API, reached through
one of five endpoints:

- `direct`: the first-party API.
- `proxy`: a gateway speaking `/v1/messages`, such as LiteLLM, with the key in
  `x-api-key` or `Authorization: Bearer`.
- `claude-platform-on-aws`: SigV4, a `region` and a `workspace_id`. Takes no key.
- `bedrock`: SigV4, a `region`, `anthropic.`-prefixed model ids. Takes no key.
- `vertex`: Google ADC, a `project` and a `region`. Takes no key.

Credentials for the three cloud endpoints come from the platform's own credential chain:
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

- The built-in table knows Anthropic's current first-party models. It prices any other
  Anthropic model as the default, `claude-opus-5-5`, so a model that costs more needs a
  price of its own (below).
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

## Cheaper models

The default is the expensive model. It and two cheaper ones were measured on the
21-question gold set, Opus and Sonnet on 2026-09-29 and Haiku on 2026-10-07
([Evaluation](../../how-it-works/evaluation/#results) has the table, and the run files are
in `eval/published/`):

| | Answered (of 18 in scope) | Agree with the reference | Per question answered |
| --- | --- | --- | --- |
| `claude-opus-5-5` on both stages, synthesis at medium effort | 18 | 18 | $0.09 |
| `claude-sonnet-5-5` on both stages, synthesis at high effort | 17 | 17 | $0.06 |
| `claude-haiku-5-5` on both stages, synthesis at medium effort | 16 | 14, and 2 in part | $0.003 |

Sonnet 5.5 is the budget option. Its input and output tokens cost half as much, and an
answer about 60% as much, slightly faster. Every answer it gave agreed with the
reference. It cited about as many of the rules its answers rest on but fewer of the
background ones, needed a retry more often, and made one small mistake in an aside the ruling did not turn on. It also asks
"did you mean?" more often for a shortened name: it asked about "Bruna" and "Gisela" in
this run, where the melded pair makes the card clear, which is the one question it did
not answer. To run it, name it for both stages:

```toml
[providers.anthropic]
kind = "anthropic"
endpoint = "direct"
api_key_env = "ANTHROPIC_API_KEY"

[providers.voyage]
kind = "voyage"

[models.extract]
provider = "anthropic"
model = "claude-sonnet-5-5"

[models.synth]
provider = "anthropic"
model = "claude-sonnet-5-5"
effort = "high"

[models.embed]                       # a judge.toml without it runs with no vector search
provider = "voyage"
model = "voyage-3.5"
```

This is the file the run used (`eval/published/v1-sonnet-5-5.judge.toml`). The built-in
price table knows the model, so it needs no `pricing` table.

Haiku 5.5 is the cheapest: a twentieth of Sonnet's price per token, and five cents for the
whole set. None of its answers contradicted the reference, but two answered only part of
the question, and two more questions got "did you mean?", once over a card name its
extraction made up. Fewer of its rulings follow from what they cite, and it made two
wrong asides. Sonnet stays the budget option to recommend. Haiku suits a server where the
bill matters more than an occasional partial answer. Its file is
`eval/published/v1-haiku-5-5.judge.toml`: the one above with `claude-haiku-5-5` on both
stages and `effort = "medium"`. The price table knows both of its rate cards, including
the dearer one for a prompt over 100K tokens, which the judge's prompts stay well under.

Twenty-one questions is a small sample. Any other model means running
`judge-eval answer --config <your file>` before you trust it. A model that fails often
there may need the prompts in `crates/bot/src/prompts/` re-tuned for it against the gold
set.

Other ways to lower the bill, on any model:

- **The extraction stage** is a small share of an answer's cost (about a third of a cent
  of ten on the default). A local model there (`pricing = "free"`) removes it. It is the
  safer stage to move: all three models above separated the 3 out-of-scope questions from the
  18 in scope, which is the part of extraction the gold set measures.
- **`JUDGE_USER_LIMIT`, `API_RATE_LIMIT` and `JUDGE_BUDGET_PERIOD`** limit how many
  questions are asked, and the bill is proportional to that.
- **`/card` and `/rule`** answer the look-something-up questions without a model.
- **An embedder** costs a few cents once and improves what the model is shown.

## Effort

`effort` on a stage is how hard the model thinks, and thinking tokens are billed as
output. Extraction runs at `low`. Synthesis, when the file names no effort, runs at the
level measured for its model on the gold set:

| Model | Synthesis effort | Why |
| --- | --- | --- |
| `claude-opus-5-5` | `medium` | As good as `high` on the gold set (all 18 agreeing, no wrong asides) and a little cheaper. It is also Anthropic's default for this model. |
| `claude-sonnet-5-5` | `high` | At `medium` it misdescribed a card on one question and made three wrong asides, for 15% less. |
| `claude-haiku-5-5` | `medium` | On the questions every run answered it agreed with the reference as often as at `high`, with fewer wrong asides; `low` did worse. It is also Anthropic's default for this model. |
| any other model | `high` | Unmeasured, so the setting that errs toward correctness. |

Each setting was run once, on 2026-09-29 (Haiku on 2026-10-07), and the Opus `high` run predates a change to how
the material prints CR examples, so read "as good" as "no worse". Model ids match exactly: on
Bedrock, `anthropic.claude-opus-5-5` is not in the table and runs at `high` unless you set
`effort`.

Setting `effort` on `[models.synth]` overrides this. An answer cut off at `max_tokens` is
retried once at `medium`, or at `low` when it was already `medium`, and one at `low` is
not retried. Raising effort
above these costs more per answer. Measure a change with `judge-eval answer --config
<your file>` before relying on it.
