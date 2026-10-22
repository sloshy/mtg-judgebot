---
title: "Model choice (judge.toml)"
description: "Providers, models per stage, pricing for the spend cap, and embeddings."
sidebar:
  order: 4
---

With nothing but `.env`, the judge runs on Anthropic's first-party API: `claude-opus-5-5`
at medium effort for both LLM stages, and Voyage `voyage-3.5` for embeddings if
`VOYAGE_API_KEY` is set. The eval numbers were measured on that setup.

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

Embeddings count toward the same cap. Each request reserves its texts' bytes plus a small
allowance as tokens, and settles to the usage the provider reports. Their price:

- Voyage models are priced from Voyage's list (`voyage-3.5` $0.06, `voyage-3.5-lite`
  $0.02, `voyage-3-large` $0.18 per million tokens) from the first token. The free
  allowance is not subtracted. An unknown Voyage model is priced as the dearest listed.
- A model on an `openai` provider needs `[models.embed.pricing]`, or the provider must say
  `pricing = "free"`. Otherwise startup fails naming `models.embed`:

  ```toml
  [models.embed]
  provider = "openai"
  model = "text-embedding-3-small"
  dimensions = 1536

  [models.embed.pricing]
  input = 0.02                         # USD per million tokens
  ```

At the cap a question is still answered, without vector search, and its call is stored
without a vector for the next `ingest embed` to fill.

A fresh database is created 1024 wide, Voyage's width. For an embedder of another width
(OpenAI's `text-embedding-3-small` is 1536), run `ingest reembed --yes` *instead of*
`ingest embed` the first time. It retypes the columns before it fills them. `ingest init`
does this by itself on a database that holds no vectors.

The database records which model's vectors it holds (`embedding_space`), and nothing
mixes two. A bot configured for another model logs an error and runs without vector
search. To switch models:

1. Run `cargo run --release -p judgebot -- ingest reembed`. It prints the row counts, the
   cost at the configured price and what is left under the spend cap, probes the new
   model once, and changes nothing.
2. Run it again with `--yes`. It retypes the columns, clears every vector and re-embeds
   them. That is paid per row, which is why it asks first. The run is billed to
   `JUDGE_MAX_USD`, and `--yes` clears nothing unless its worst case fits what is left.
   When the dry run says it may not fit, set a higher cap for the run alone
   (`JUDGE_MAX_USD=20 … reembed --yes`).

Run with the switch already made, `reembed` only fills rows that are still empty.
`--clear` clears and re-pays every row on purpose.

`judge-cli config` prints the resolved setup with secrets redacted. Every binary logs the
same summary line at startup.

## Cheaper models

The default, Opus 5.5 at medium on both stages, answered every in-scope question of the
gold set in agreement with the reference, at about $7.60 per 100 questions. Each change
below moves one stage from it, measured end to end on the same set (two runs each,
2026-10-22; [Evaluation](../../how-it-works/evaluation/#results) has the full table, and
`eval/published/v2-*` the files and runs):

| Change from the default | Agree (of 38) | Partly right | Decisive rules cited | Per 100 questions | vs. default |
| --- | --- | --- | --- | --- | --- |
| None: Opus 5.5 at medium, both stages | 38 | 0 | 86% | $7.60 | |
| Extraction at `low` | 37, and 1 "did you mean?" | 0 | 82% | $7.40 | −3% |
| Synthesis at `xhigh` | 38 | 0 | 91% | $8.80 | +16% |
| Synthesis on `claude-sonnet-5-5` at `low` | 36 | 2 | 77% | $4.20 | −44% |
| Synthesis on `claude-haiku-5-5` at `high` | 35 | 3 | 80% | $0.96 | −87% |

The gold set is deliberately hard, so expect an everyday question to cost less. A
difference of one or two answers is within what repeating a run moves.

**Extraction at low** is about as good and slightly cheaper. Now and then it writes part
of a card's name alone ("Bruna" for Bruna, the Fading Light), and the bot asks "did you
mean?" instead of answering. Set it on the extraction stage:

```toml
[models.extract]
provider = "anthropic"
model = "claude-opus-5-5"
effort = "low"
```

**Synthesis at xhigh** gives the same rulings with a few more of the rules they rest on
cited, for about 16% more and 40% longer per answer:

```toml
[models.synth]
provider = "anthropic"
model = "claude-opus-5-5"
effort = "xhigh"
```

**Synthesis on Sonnet 5.5** costs about half the default per answer, and is faster. It
answered as well at `low` as at `high` for a quarter less, so `low` is its default effort.
About one answer in twenty was right only in part, and it made more wrong side remarks
than Opus. Keep extraction on Opus: it is a small share of the bill, and Sonnet more often
writes a shortened name alone. The whole file:

```toml
[providers.anthropic]
kind = "anthropic"
endpoint = "direct"
api_key_env = "ANTHROPIC_API_KEY"

[providers.voyage]
kind = "voyage"

[models.extract]
provider = "anthropic"
model = "claude-opus-5-5"

[models.synth]
provider = "anthropic"
model = "claude-sonnet-5-5"

[models.embed]                       # a judge.toml without it runs with no vector search
provider = "voyage"
model = "voyage-3.5"
```

This is `eval/published/v2-sonnet-low.judge.toml` without the efforts it spells out, which
are each model's defaults. The built-in price table knows the model, so it needs no
`pricing` table.

**Synthesis on Haiku 5.5** costs about a tenth of the default. None of its answers
contradicted the reference, but about one in thirteen answered only part of the question,
and fewer of its rulings follow from what they cite than Opus's. It suits a server where the
bill matters more than an occasional partial answer. The file is the one above with
`model = "claude-haiku-5-5"` on `[models.synth]` (`eval/published/v2-haiku-high.judge.toml`).
The price table knows both of its rate cards, including the dearer one for a prompt over
100K tokens, which the judge's prompts stay well under.

Twenty-two questions is a small sample. Any other model means running
`judge-eval answer --config <your file>` (or a `judge-eval sweep` plan) before you trust
it. A model that fails often there may need the prompts in `crates/bot/src/prompts/`
re-tuned for it against the gold set.

Other ways to lower the bill, on any model:

- **The extraction stage** is a small share of an answer's cost (about half a cent of
  nine on the default). A local model there (`pricing = "free"`) removes it. It is the
  safer stage to move, but measure it: a model that writes card names in part makes the
  bot ask "did you mean?" more often.
- **`JUDGE_USER_LIMIT`, `API_RATE_LIMIT` and `JUDGE_BUDGET_PERIOD`** limit how many
  questions are asked, and the bill is proportional to that.
- **`/card` and `/rule`** answer the look-something-up questions without a model.
- **An embedder** costs a few cents once and improves what the model is shown.

## Effort

`effort` on a stage is how hard the model thinks, and thinking tokens are billed as
output. When the file names no effort, each stage runs at the level measured for its model
on the gold set (2026-10-21, each stage alone at every effort from `low` to `xhigh`, two
runs each, `eval/published/v2-efforts-summary.md`):

| Model | Extraction | Synthesis | Why |
| --- | --- | --- | --- |
| `claude-opus-5-5` | `medium` | `medium` | Extraction at `low` occasionally wrote a card's name in part; `medium` never did, for 3% more. Synthesis agreed with the reference at every effort, and `medium` is where the rules it cites level off. |
| `claude-sonnet-5-5` | `low` | `low` | Extraction found the cards as often at `low` as at `high`. Synthesis at `low` agreed as often as at `high`, with fewer failed answers, for a quarter less. |
| `claude-haiku-5-5` | `high` | `medium` | Extraction found every card on 95% of questions at `high`, against 86% at `low`, for a fraction of a cent; at `xhigh` it outthinks the stage's output limit. Synthesis tied between `medium` and `high`; `medium` is Anthropic's default for it. |
| any other model | `low` | `high` | Unmeasured: enough for extraction, and the setting that errs toward correctness for synthesis. |

Model ids match exactly: on Bedrock, `anthropic.claude-opus-5-5` is not in the table and
runs at the unmeasured levels unless you set `effort`.

Setting `effort` on a stage overrides this. An answer cut off at `max_tokens` is retried
once at `medium`, or at `low` when it was already `medium`, and one at `low` is not
retried. Raising effort costs more per answer. Measure a change with `judge-eval answer
--config <your file>` before relying on it.
