# mtg-judgebot

A Discord bot and anonymous web page that answers Magic: The Gathering rules questions
like a judge. Ask in plain language. Nicknames such as "bob", "goyf" and "snappy" work.
The reply is a concise ruling in which **every claim is backed by a validated, clickable
citation**: a Comprehensive Rules section, an official Scryfall ruling, the card's
current Oracle text, or a prior rated call. On Discord, buttons rate an answer 1–3
(incorrect / partially correct / correct), and ratings feed back into how future answers
are grounded.

```
/judge question: does bob's trigger count goyf's mana value as 0?

@you asked: does bob's trigger count goyf's mana value as 0?
You'll lose 2 life, not 0. Tarmogoyf's mana value is 0 anywhere its {X}… [702.19b] …
  Citations: [202.3](…CR link…) · [Ruling (2021-02-05) — Tarmogoyf](…scryfall…) · [Oracle text — Dark Confidant](…)
  Confidence: High · CR 2026-08-19        [Incorrect] [Partially correct] [Correct]
```

## Answer pipeline

1. **Extract & classify** (LLM, structured output). The model separates card-name spans
   from rules concepts, picks up to three categories from a fixed taxonomy, and checks
   scope. Tournament-policy and price questions are politely refused without spending on
   synthesis.
2. **Resolve cards** through a typed ladder: alias table (nicknames, possessives) →
   exact name → old printed names → short names ("Ragavan") → trigram fuzzy. A
   `[[Full Card Name]]` in brackets matches that exact name only. Answers list the cards
   they resolved to. The bot **never guesses**: ambiguity ("Tibalt") becomes a
   "Did you mean…?" button row.
3. **Retrieve** from Postgres through three legs: a curated category→CR-section map,
   full-text search, and pgvector semantic search over rule embeddings. Retrieval adds
   the cards' rulings, glossary entries, hand-written notes for "nightmare" cards
   (Humility, Blood Moon…), and similar prior calls labeled with their community rating.
4. **Synthesize** (LLM): a judge-persona prompt over that material and nothing else, with one optional
   `lookup_rules` tool round (type-enforced to at most one). Every citation must quote
   its source verbatim. Hallucinated citations are rejected and retried, and an answer
   with no citations never ships.
5. **Learn from ratings**: a Bayesian-smoothed score per call, with an override for
   users holding a Judge role, decides which prior calls are shown, warned about, or
   excluded. The CR always outranks precedent.

## Self-hosting

A judgebot belongs to the community that
runs it, so you run your own. It takes one compose file, a Discord application you create
in a few minutes, and a model API key. Nothing in the pipeline needs Discord, so the web
page and the command line work before you have a bot token. "Running it" below is the
short version. The documentation site, <https://sloshy.github.io/mtg-judgebot/>, walks
through it and explains how the judge works inside.

## Running it

Requirements:

- Docker with the compose plugin.
- Rust 1.98 (`rust-toolchain.toml` installs it through rustup).
- A model provider: an Anthropic API key out of the box, or a `judge.toml` naming
  another provider (below).

A Voyage AI key turns on the semantic-search leg. Without it the bot still works on the
other two. Nothing here needs Discord until you want the bot in a server.

```sh
git clone https://github.com/sloshy/mtg-judgebot && cd mtg-judgebot
cp .env.example .env               # add ANTHROPIC_API_KEY (and VOYAGE_API_KEY if you have one),
                                   # JUDGE_OPERATOR_EMAIL for judge-api, JUDGE_OPERATOR_DISCORD for the bot
docker compose up -d db            # pgvector Postgres on localhost:5432
cargo run --release -p judge-ingest -- migrate              # create the schema (bot and api also do this at startup)

# Load the data (once; the nightly refresh keeps it current afterwards)
cargo run --release -p judge-ingest -- cards                # Scryfall bulk data, ~110 MB, cached in .cache/
cargo run --release -p judge-ingest -- rules latest         # the current Comprehensive Rules from Wizards' site
cargo run --release -p judge-ingest -- aliases data/aliases.yaml
cargo run --release -p judge-ingest -- notes data/notes.yaml
cargo run --release -p judge-ingest -- embed                # optional: every rule and glossary entry through the
                                                            # embedder once; a few cents on Voyage

docker compose up -d api           # first run builds the image (minutes, ~4 GB RAM), then serves the web page
```

Open <http://localhost:8787> and ask a question. If port 5432 is taken on your machine,
set `DB_PORT` in `.env` and change `DATABASE_URL` to match. A typical answer costs
$0.08–0.25 in model calls. Every call is metered and hard-capped per process
(`JUDGE_MAX_USD`, default $5).

### Your own Discord bot

Every judgebot is its own Discord application, owned by whoever runs it. Create one in
the [Discord Developer Portal](https://discord.com/developers/applications). Discord's
[Building your first Discord Bot](https://docs.discord.com/developers/quick-start/getting-started)
covers the portal. Then:

1. Under **Bot**, reset the token and copy it into `.env` as `DISCORD_TOKEN`. Leave every
   *Privileged Gateway Intent* off. The bot receives only its own slash commands and button
   presses, never messages. Put your own Discord username in `.env` as
   `JUDGE_OPERATOR_DISCORD`. The bot refuses to start without it, and `/help` and
   `/license` show it so that people know who runs the instance.
2. Under **OAuth2 → URL Generator**, tick the scopes `bot` and `applications.commands`
   and leave the permissions at none (replies go through the interaction). Open the
   generated URL to add the bot to your server. You need *Manage Server* there.
3. For instant command registration, put your server's id in `.env` as `GUILD_ID`
   (*User Settings → Advanced → Developer Mode*, then right-click the server → *Copy
   Server ID*). Without it the commands register globally. That can take up to an hour
   to appear but works in every server the bot joins.
4. `docker compose up -d bot`. The log line `registered /judge, /help, /license and
   /forget` confirms it, and `/help` in the server confirms it end to end.

Members holding a role named `JUDGE_ROLE` (default `Judge`) rate as judges: their rating
overrides the crowd's. `cargo run --release -p judge-ingest -- emoji` uploads the mana
symbols as application emoji once, so answers show pictures instead of `{W}`. The
documentation site's [Run your own judgebot](https://sloshy.github.io/mtg-judgebot/self-hosting/first-run/)
section has the long form with links into Discord's documentation.
`docker compose up -d --build bot api` redeploys after code changes.

### Choosing a model

With nothing but `.env`, the judge runs on Anthropic's first-party API: `claude-opus-5`
for both LLM stages, and Voyage `voyage-3.5` for embeddings if `VOYAGE_API_KEY` is set.
The eval numbers and the pinned prompt digest were produced on that setup, and upgrading
never changes it.

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
1536), run `ingest reembed --yes` *instead of* `ingest embed` the first time. It retypes
the columns before it fills them.

The database records which model's vectors it holds (`embedding_space`), and nothing
will mix two. A bot configured for another model logs an error and runs with the vector
leg dark. To switch, run `cargo run --release -p judge-ingest -- reembed`. It prints the
row counts and a rough cost and probes the new model once. With `--yes` it retypes the
columns, clears every vector and re-embeds them. That is paid per row, which is why it
asks first. Run again with the switch already made, it only fills rows still empty
(`--clear` clears and re-pays on purpose). `judge-cli config` prints what resolved,
secrets redacted, and every binary logs the same summary line at startup.

### The web page

Each of `judge-api`'s front doors is a launch option. Run on its own it serves
`POST /api/judge` and nothing else. The page needs `--web`, and the MCP transport needs
`--mcp`. The compose file passes `--api --web`, so `docker compose up -d` serves an
anonymous web front end on <http://localhost:8787> (SolidJS, built into the image). It
has the same pipeline, citations and "did you mean…?" flow in the browser, with no login.
Set `API_INTERFACES` in `.env` to change that list, for example `--api` alone for a
deployment with no public page. `judge-api` refuses to start without
`JUDGE_OPERATOR_EMAIL`, a support address that `GET /api/about`, the page footer and the
MCP instructions show. Nobody is logged in there, so the web has **no rating
buttons**. Anonymous traffic is rate limited per IP (`API_RATE_LIMIT` questions per
`API_RATE_WINDOW_SECS`, default 4 per 5 minutes) on top of the global spend cap. For
local development:

```sh
cargo run --release -p judge-api -- --api --web   # API + static page on localhost:8787
npm --prefix web install
npm --prefix web run dev             # Vite dev server with /api proxied to :8787
```

### Hosting

The intended deployment is a machine at home behind a [Cloudflare
Tunnel](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/):
no public IP, no forwarded port, no cloud compute bill. `docs/DEPLOYMENT.md` is the
runbook. It covers tunnel setup, edge rate limiting in front of the anonymous API, and
the weekly R2 backup.

## Evaluation

`eval/gold.yaml` holds 21 adversarially verified questions (layers nightmares,
multi-face cards, errata traps, Commander, out-of-scope) with expected rule ids and
reference answers.

```sh
cargo run -p judge-eval -- recall            # retrieval gate: expected rules present in context? (free)
cargo run -p judge-eval -- answer --label x --limit 21 --max-usd 6   # full live run (~$2.50)
cargo run -p judge-eval -- rescore eval/runs/x.json                  # re-grade a stored run (free)
cargo run -p judge-eval -- show eval/runs/x.json                     # bot vs. gold, side by side
```

Scoring accepts per-question *equivalence lists* of alternate rule ids that state the
same fact, so the metric tracks correctness rather than one author's citation taste.

## Design

Rust was chosen for compiler-enforced correctness. `docs/DECISIONS.md` records that
decision and the other main design decisions, with the alternatives rejected. In practice:

- Closed enums for every domain sum, and refined newtypes for ids.
- `Verdict<Unvalidated> → validate() → Verdict<Validated>`, so an unchecked answer
  *cannot* be persisted or shown.
- A typestate on the synthesis loop, so the tool round cannot repeat.
- Compile-time-checked SQL (sqlx + committed offline data).
- An LLM output schema derived from the same structs the responses parse into.

`docs/ARCHITECTURE.md` is the living design document.

```
crates/
  core       domain types, ports, judge() pipeline, citation validation — no I/O
  llm        provider-neutral chat types, Backend + sealed ChatModel port, spend cap, retry loop, Synth typestate
  anthropic  the Messages API as a judge-llm backend: wire types, schema transform, endpoints
  openai     OpenAI-compatible chat completions as a judge-llm backend: strict-schema transform, dialect knobs
  embed      Voyage and OpenAI-compatible embeddings, each tagged with its vector Space
  bot        Postgres adapters (resolver / retriever / call store), judge.toml loader, prompts, Discord (serenity/poise)
  ingest     Scryfall + Comprehensive Rules loaders, embedder  (bin)
  eval       gold-set harness: recall / answer / rescore / show (bin)
  api        anonymous HTTP adapter (axum); the web page and the /mcp transport are opt-in flags (bin)
  agent      the judge for other agents: sessions, lookups and the pipeline as judge-cli and judge-mcp
web/         SolidJS + TypeScript single page (Vite)
site/        the documentation site (Astro + Starlight); docs/ is its source
data/        categories.yaml (generates the Category enum), aliases.yaml, notes.yaml
eval/        gold.yaml + stored runs
docs/        EXPLAINER.md (the tour), ARCHITECTURE.md (the reference), DECISIONS.md (why),
             PROVIDERS.md (the model-provider reference), DEPLOYMENT.md (the runbook)
```

Each community runs its own judgebot. One process is one spend cap, one judge role and one
Discord application, by design rather than omission (`docs/DECISIONS.md` D16). Not built:
tournament-policy (MTR/IPG) coverage. The bot declines those questions rather than
winging them.

## License and attribution

AGPL-3.0-or-later. See [LICENSE](LICENSE). Running a modified version of this bot
(Discord, the HTTP API or MCP) as a network service requires making the modified source
available to its users. The bot does that for you. Every remote interface states the
licence and copyright and names the repository its source is in, with the commit the
binary was built from:

- the web page's footer and `GET /api/about`
- Discord's `/help` and `/license`
- the MCP server's initialization instructions and its `about` tool
- `judge-cli about`

CI stamps the commit into the published image, and `git rev-parse HEAD` stamps it into a
local build. If you change anything, set `JUDGE_SOURCE_URL` in `.env` to the repository
holding your changes and every interface points there. That is the whole of your
obligation under section 13.

The same interfaces name whoever runs the instance. The bot requires
`JUDGE_OPERATOR_DISCORD` (a Discord username) and `judge-api` requires
`JUDGE_OPERATOR_EMAIL` (a support address). Each shows the other contact too when it is
set. `judge-cli` and `judge-mcp` on stdio need neither.

This is unofficial Fan Content permitted under Wizards of the Coast's [Fan Content
Policy](https://company.wizards.com/en/legal/fancontentpolicy), not approved or
endorsed by Wizards. Magic: The Gathering, the Comprehensive Rules, card text and
rulings are © Wizards of the Coast. Card data and rulings come from
[Scryfall](https://scryfall.com) under its [data guidelines](https://scryfall.com/docs/api).
The bot fetches both at run time, and the repository carries only a short CR excerpt as
a parser test fixture. Rule links go to the independent
[Yawgatog](https://yawgatog.com/resources/magic-rules/) CR mirror. `NOTICE` has the
full statement. The web page and every page of the documentation site repeat this
disclaimer and the data sources in their footers, and the bot's `/help` gives a short
form of it.
