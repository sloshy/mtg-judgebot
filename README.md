# mtg-judgebot

[![Build](https://github.com/sloshy/mtg-judgebot/actions/workflows/publish-image.yml/badge.svg?branch=main)](https://github.com/sloshy/mtg-judgebot/actions/workflows/publish-image.yml)
[![Release](https://img.shields.io/github/v/release/sloshy/mtg-judgebot)](https://github.com/sloshy/mtg-judgebot/releases/latest)
[![License: AGPL-3.0-or-later](https://img.shields.io/github/license/sloshy/mtg-judgebot)](LICENSE)

<img src="assets/icon.png" alt="" width="96" align="right">

A Discord bot and web app that answers Magic: The Gathering rules questions like a judge.
Ask in plain language. Nicknames such as "bob", "goyf" and "snappy" work. Every claim in
the answer is **backed by a checked, clickable citation**: a Comprehensive Rules section,
an official Scryfall ruling, the card's current Oracle text, or a past answer. On Discord,
members rate answers as correct, partially correct or incorrect, and ratings decide which
past answers new ones draw on as examples. A rating that finds fault says why, and the
reason goes to the operator's feedback queue to be fixed.

<img src="site/src/assets/screenshots/discord-answer-citations.png" width="720" alt="A Discord reply to /judge about revealing an X-cost spell to Dark Confidant: a two-paragraph ruling with a rule number inline and mana symbols drawn as pictures, an embed quoting a dated ruling, the card's Oracle text and rule 107.3g, the card the question resolved to, the confidence and CR version, and three rating buttons.">

The same answer in the web app:

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="site/src/assets/screenshots/web-answer-dark.png">
  <img src="site/src/assets/screenshots/web-answer-light.png" width="720" alt="The web app answering a question about Dark Confidant and Tarmogoyf: the ruling, six linked citations, the cards the question was resolved to, and the confidence.">
</picture>

More answers, copied verbatim from a published evaluation run, are on the
[Sample answers](https://mtg-judgebot.rpeters.dev/start-here/sample-answers/) page.

## How it answers

1. **Read the question.** One model call picks out card names and rules concepts and
   sorts the question into up to three categories. Tournament-policy and price questions
   are declined here, before the expensive step.
2. **Find the cards.** Nicknames and possessives, exact names, old printed names, short
   names ("Ragavan"), then fuzzy matching. `[[Full Card Name]]` in brackets matches that
   exact name (or a listed nickname) only. Answers show which cards they used. The bot
   **never guesses**: an ambiguous name ("Tibalt") gets a "Did you mean…?" prompt.
3. **Gather the material.** The rules sections for the question's categories, plus
   full-text and semantic search over the Comprehensive Rules. Then the cards' rulings,
   glossary entries, hand-written notes for notoriously tricky cards (Humility, Blood
   Moon…), and similar past answers with their ratings.
4. **Write the ruling.** The model answers from that material and nothing else, and may
   ask for more rules once. Every citation must quote its source word for word, and every
   rule number in the text must be cited. An answer that fails the check is sent back once
   with a note on what failed. One that fails again is never shown.
5. **Learn from ratings.** Ratings decide which past answers the model sees as examples.
   A member with the Judge role outranks the crowd, and the rules always outrank past
   answers. *Partially correct* and *Incorrect* come with a reason, which the operator
   works through in a feedback queue.

## What it takes

Each community runs its own judgebot: one compose file, a model API key, and a Discord
application you create in a few minutes.

| | |
| --- | --- |
| **Cost per answer** | $0.07–0.15 on the default model (Claude Opus 5.5). A `judge.toml` can switch to a cheaper or a local model. |
| **Cost per month** | About $20–45 for ten questions a day. Nothing else costs money. `JUDGE_MAX_USD` is a hard cap, and `JUDGE_BUDGET_PERIOD=month` makes it a monthly budget. |
| **Accounts** | A model provider (Anthropic by default). Optional: Voyage AI for semantic search (a few cents, once), and Discord for the bot. |
| **Host** | Any machine that runs Docker and stays on, with about 200 MB of RAM to spare. Images are published for amd64 and arm64, so nothing is compiled. |
| **Setup** | Six commands. The first data load downloads about 110 MB from Scryfall and takes about a minute on a fast connection. |

You don't need Discord to start: the web app and the command line work without a bot
token. The documentation site, <https://mtg-judgebot.rpeters.dev/>, has the long form of
everything below and explains how the judge works.

## Running it

You need Docker with the compose plugin, and a model API key.

```sh
git clone https://github.com/sloshy/mtg-judgebot && cd mtg-judgebot
docker compose pull                # the published image, amd64 and arm64
scripts/config.sh                  # the config editor: open the URL it prints, set ANTHROPIC_API_KEY,
                                   # JUDGE_OPERATOR_EMAIL (a support address the app shows) and
                                   # JUDGE_ROLES to '--api --web --jobs' (no Discord yet), save, Ctrl-C.
                                   # VOYAGE_API_KEY turns on semantic search
docker compose up -d db            # Postgres with pgvector, on localhost:5432
docker compose run --rm refresh init   # the first data load: schema, cards, rules, nicknames, notes and
                                       # embeddings if keyed. Safe to rerun. judgebot refreshes the data
                                       # daily after that (JUDGE_REFRESH_HOURS)
docker compose up -d               # judgebot: the web app and the daily refresh
```

Open <http://localhost:8787> and ask a question. Each address can ask 4 questions per 5
minutes by default (`API_RATE_LIMIT`). If port 5432 is taken, set `DB_PORT` and change
`DATABASE_URL` to match.

The config editor checks every change the same way the bot does at startup, and tells you
what would fail and why. To edit by hand instead, `cp .env.example .env` and fill it in.
Every variable is documented in the file.

Every model call, embeddings included, counts against a hard cap, `JUDGE_MAX_USD`
(default $5):

- By default the cap covers the process's lifetime and resets on restart.
- `JUDGE_BUDGET_PERIOD=day` or `month` makes it a daily or monthly budget that survives
  restarts.
- When what's left can't cover a call's worst case (about $0.36 for the answering step), questions
  are refused and `JUDGE_ALERT_WEBHOOK` is notified. `JUDGE_SPEND_WARN_AT=50,80` warns
  there at those percentages of the cap first.

`docker compose pull` fetches the image CI builds from upstream `main`. To run your own
checkout instead, skip it and run `docker compose up -d --build` (takes minutes and about
4 GB of RAM). [CONTRIBUTING.md](CONTRIBUTING.md) covers working from source with Rust,
where `docker compose run --rm refresh <command>` becomes
`cargo run --release -p judgebot -- ingest <command>`.

### Your own Discord bot

Every judgebot is its own Discord application, owned by whoever runs it. Create one in
the [Discord Developer Portal](https://discord.com/developers/applications) (Discord's
[Building your first Discord Bot](https://docs.discord.com/developers/quick-start/getting-started)
walks through the portal). Then:

1. Under **Bot**, reset the token and put it in `.env` as `DISCORD_TOKEN`. Leave every
   *Privileged Gateway Intent* off: the bot only receives its own slash commands and
   button clicks, never messages. Put your own Discord username in `.env` as
   `JUDGE_OPERATOR_DISCORD`. The bot won't start without it, and `/help` and `/license`
   show it so people know who runs the bot.
2. Under **OAuth2 → URL Generator**, tick the scopes `bot` and `applications.commands`
   and leave the permissions empty. Open the generated URL to add the bot to your server
   (you need *Manage Server* there).
3. For commands that appear immediately, put your server's id in `.env` as `GUILD_ID`
   (*User Settings → Advanced → Developer Mode*, then right-click the server → *Copy
   Server ID*). The commands then work only in that server. Without it they register
   everywhere, which can take up to an hour, and work in every server the bot joins and
   in DMs (all but `/judge`).
4. Clear `JUDGE_ROLES` (unset, it runs the bot, the web app and the refresh) and run
   `docker compose up -d`. The log line `registered /judge, /card, /rule, /help,
   /license and /forget` means it worked. Try `/help` in your server.

Members with the role named by `JUDGE_ROLE` (default `Judge`) rate as judges, and their
rating overrides everyone else's. Run `docker compose run --rm refresh emoji` once to
upload the mana symbols, so answers show pictures instead of `{W}`. After code changes,
`docker compose up -d --build judgebot` redeploys.
[Run your own judgebot](https://mtg-judgebot.rpeters.dev/self-hosting/first-run/) has the
long form.

### Choosing a model

With only `.env`, the judge uses Anthropic's API with `claude-opus-5-5` for both
steps, and Voyage `voyage-3.5` for semantic search if `VOYAGE_API_KEY` is set. The
[results below](#results) were measured on that setup.

A `judge.toml` can pick a different model for each step: Anthropic directly, through a
proxy, on Claude Platform on AWS, Bedrock or Vertex, or any OpenAI-compatible server
(OpenAI, LiteLLM, OpenRouter, vLLM, a local Ollama priced `free`). Embeddings can come from Voyage or an
OpenAI-compatible server. `judge.example.toml` documents every option, and
[Model choice](https://mtg-judgebot.rpeters.dev/self-hosting/models/) is the guide. Under
Docker, also set `JUDGE_CONFIG=./judge.toml` in `.env`.

The config editor (`scripts/config.sh`) edits both files from a local page. It never
shows a saved secret, only lets you replace it.
[The config editor](https://mtg-judgebot.rpeters.dev/self-hosting/config-editor/) has the
details.

### Web app and HTTP API

The web app at <http://localhost:8787> uses the same pipeline, citations and "did you
mean…?" prompts as the bot. Its ratings are anonymous and never shape later answers. Its `/feed`
lists answered questions from every interface, except those asked private, and its
`/dashboard` counts them by day, interface, outcome, rating, topic and card. What a
judgebot runs is set by roles: `--discord`, `--api` (the JSON API), `--web` (the web
app), `--mcp` (for AI agents), `--admin` (the operator's pages, on a port of their own)
and `--jobs` (the data refresh). `JUDGE_ROLES` in `.env` lists them, and unset it is
`--discord --api --web --jobs`.
[The web app](https://mtg-judgebot.rpeters.dev/using/web/) and
[The HTTP API](https://mtg-judgebot.rpeters.dev/using/api/) have the details.

### Hosting

The intended setup is a machine you own running the compose stack, with no cloud bill.
Out of the box it answers on `127.0.0.1:8787` only, which is enough for a Discord bot
over its gateway connection. To publish the web app, set `COMPOSE_PROFILES=public` and
`JUDGE_DOMAIN` in `.env`: the bundled Caddy then serves `https://<JUDGE_DOMAIN>` on ports
80 and 443 with a Let's Encrypt certificate. A reverse proxy of your own, or a [Cloudflare
Tunnel](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/)
with no open ports, works instead. `docs/DEPLOYMENT.md` is the runbook. It covers the
three routes, protecting the model bill, and weekly backups to S3-compatible storage
(Cloudflare R2, AWS S3, Backblaze B2, MinIO).

## Evaluation

`eval/gold.yaml` holds 22 questions chosen to be hard (layers, multi-faced cards, errata traps,
Commander, out-of-scope questions, and one ruling corrected after a wrong answer was
reported) with the rules a correct answer cites and a reference answer. All but the
corrected one were checked adversarially.

```sh
cargo run -p judge-eval -- recall            # are the expected rules in the material? (free)
cargo run -p judge-eval -- answer --label x --limit 22 --max-usd 6   # full live run (~$1.70)
cargo run -p judge-eval -- grade eval/runs/x.json                    # a model grades each answer (~$0.40)
cargo run -p judge-eval -- rescore eval/runs/x.json                  # re-score a stored run (free)
cargo run -p judge-eval -- show eval/runs/x.json                     # bot vs. reference, side by side
cargo run -p judge-eval -- sweep eval/sweeps/efforts.toml            # models × efforts per stage, averaged
```

Each question can list alternate rule ids that state the same fact, so the score tracks
whether the answer is right, not whether it picked one author's preferred citation.
`grade` asks a model whether each ruling follows from its quotes and agrees with the
reference, which is how the Results table below was graded (`docs/DECISIONS.md` D28). It
is a score, not a pass/fail gate. `sweep` runs a plan of models and efforts, each several
times, graded by one model, and averages them; `eval/sweeps/` holds the plans behind the
table.

### Results

The default and four changes from it, each run twice over the 22 questions (19 in scope,
3 out of scope) on 2026-10-22, on CR 2026-09-25 with Voyage `voyage-3.5` embeddings. Every
figure is pooled over both runs. Each configuration's `judge.toml` and both run files are in
`eval/published/` (`v2-*`), with every question, answer, citation, time and cost.
`judge-eval show eval/published/v2-default-r1.json` prints each answer beside its
reference. The two cheaper models keep the default's extraction and only answer.

| | Default: `claude-opus-5-5` at medium effort, both stages | Extraction at low | Synthesis at xhigh | Synthesis on `claude-sonnet-5-5` at low | Synthesis on `claude-haiku-5-5` at high |
| --- | --- | --- | --- | --- | --- |
| Out-of-scope questions declined (of 6) | 6 | 6 | 6 | 6 | 6 |
| In-scope questions answered (of 38) | 38 | 37 | 38 | 38 | 38 |
| …agreeing with the reference ruling | 38 | 37 | 38 | 36 | 35 |
| …partly (right on the main point, a sub-question missed) | 0 | 0 | 0 | 2 | 3 |
| …contradicting the reference | 0 | 0 | 0 | 0 | 0 |
| Asked "did you mean?" instead | 0 | 1 | 0 | 0 | 0 |
| Not answered | 0 | 0 | 0 | 0 | 0 |
| …following from what they cite alone | 21 | 21 | 24 | 12 | 20 |
| Wrong side remarks | 1 | 2 | 0 | 5 | 3 |
| Decisive rule ids cited | 64 of 74 (86%) | 61 of 74 (82%) | 67 of 74 (91%) | 57 of 74 (77%) | 59 of 74 (80%) |
| Supporting rule ids also cited | 34 of 66 | 26 of 66 | 34 of 66 | 32 of 66 | 24 of 66 |
| Cost per in-scope question (median) | $0.087 | $0.086 | $0.099 | $0.046 | $0.009 |
| Time per in-scope question (median / longest) | 13 s / 24 s | 12 s / 29 s | 18 s / 43 s | 9 s / 19 s | 17 s / 39 s |
| Whole run (of 22 questions) | $1.67 | $1.63 | $1.93 | $0.93 | $0.21 |

How to read the table:

- **Answered** means the answer passed validation: every citation points to material the
  model was shown and quotes it word for word, and every rule number in the text is
  cited. "Not answered" means the bot refused to show an answer, not that it showed a
  wrong one. A rejected answer gets one retry with a note on what failed. Of the 44
  questions each configuration was asked, Sonnet needed 8 retries, the default 2, low
  extraction 4, xhigh and Haiku none, and every retry succeeded.
- **"Did you mean?"** is the bot working as designed when a name could mean several
  cards, but it leaves the question unanswered. With extraction at low, Opus once wrote
  "Bruna" and "Gisela" without the full names; at medium it named the melded pair every
  time.
- **Agreement with the reference** was judged by Claude Opus 5.5 (`judge-eval grade`),
  reading each answer against the reference under a strict rubric. The references were
  written and checked by models, then audited against Oracle text, rulings and the rules,
  which found three to correct. No human judge has reviewed either side, so read this as
  "no contradiction found", not as measured accuracy. It grades the ruling, not every side
  remark. The default made one wrong remark (that you choose the order of Bruna and Gisela
  in exile). Sonnet's partial answers misdescribed Opalescence under Humility and missed
  the Lion's Eye Diamond line of holding priority. Haiku's never worked through the
  trample-and-deathtouch case the Questing Beast question describes, and once wouldn't
  name "Tim" (Prodigal Sorcerer). Both said once that Blood Moon leaves Urborg with no
  abilities, when it gives it "{T}: Add {R}".
- **Following from what they cite** asks whether the ruling follows from the quoted text
  alone. The rest rely on a step they don't cite: Doubling Season's own text for the
  counter math, the definition of "dies" (700.4), or the rule that Oracle text overrides
  an old printing (108.1). Graders apply this one less evenly than agreement.
- **Decisive rule ids** are the rules a reference answer rests on, which a correct answer
  should cite. **Supporting** ids are background a good answer may leave out, so missing
  one doesn't count against it. The split was drafted by a model and accepted by the
  maintainer, not a judge. It signals regressions between runs rather than measuring
  accuracy.
- **Small differences are noise.** On 19 graded questions one answer is about five points
  of a run, and repeating a run moves agreement by one or two answers. Read a difference
  of one or two as a tie; cost and decisive-rule recall move far less between runs.
- Twenty-two hard questions is a small sample. It shows the bot holds up on layers,
  multi-faced cards, old wordings and Commander. It doesn't tell you how often an answer
  in your server will be right.
- **The dollar figures are list prices** as the spend cap counts them, from the built-in
  price table, with every question's material written to the prompt cache as a new
  question's is. Your provider's console has the real bill. The questions ran four at a
  time, so the times include some waiting.

Opus 5.5 at medium effort on both stages is the default: every in-scope answer agreed
with the reference, for $0.087 a question. The effort sweep behind it ran each stage alone
at every effort from low to xhigh on all three models, two runs each
(`eval/published/v2-efforts-summary.md`, `eval/sweeps/efforts.toml`). Opus agreed with the
reference at every synthesis effort; medium is where its decisive-rule recall levels off,
for 7% over low. At extraction, low and medium found the same cards except where low wrote
part of a name alone, so low is a few percent cheaper with an occasional "did you mean?".

Moving one stage from the default, as the table shows:

- **Extraction at low** saves about 3% and is otherwise as good, with an occasional "did
  you mean?" when a name is written in part (once in 38 here).
- **Synthesis at xhigh** gives the same rulings with a few more of the decisive rules
  cited, for 16% more and about 40% longer per answer.
- **Synthesis on Sonnet 5.5 at low** costs about half the default. It ran at low because
  that matched its high-effort runs for a quarter less. About one answer in twenty is
  right only in part, and it makes more wrong side remarks.
- **Synthesis on Haiku 5.5 at high** costs a tenth of the default. About one answer in
  thirteen is right only in part. It suits a community where the bill matters more than an
  occasional partial answer.

The documentation site's Model choice page has each of these as a `judge.toml` and covers
other ways to save money. The 2026-09-29 and 2026-10-07 runs of the earlier configurations
stay in `eval/published/` as `v1-*`.

## Design

Rust was chosen so the compiler enforces correctness. `docs/DECISIONS.md` records that
choice and the other main design decisions, with the alternatives rejected. In practice:

- Closed enums wherever a value has a fixed set of cases, and validated newtypes for ids.
- `Verdict<Unvalidated> → validate() → Verdict<Validated>`, so an unchecked answer
  *cannot* be stored or shown.
- A typestate on the answer loop, so the extra-rules lookup cannot repeat.
- SQL checked at compile time (sqlx + committed offline data).
- The model's output schema is generated from the same structs its responses parse into.

`docs/ARCHITECTURE.md` is the full design reference.

```
crates/
  core       domain types, ports, judge() pipeline, citation validation — no I/O
  llm        provider-neutral chat types, Backend + sealed ChatModel port, spend cap, retry loop, Synth typestate
  anthropic  the Messages API as a judge-llm backend: wire types, schema transform, endpoints
  openai     OpenAI-compatible chat completions as a judge-llm backend: strict-schema transform, dialect knobs
  embed      Voyage and OpenAI-compatible embeddings, each tagged with its vector Space
  bot        Postgres adapters (resolver / retriever / call store), the Scryfall + Comprehensive Rules loaders
             and embedder, the refresh schedule, judge.toml loader, prompts, Discord (serenity)
  api        HTTP adapter (axum): the JSON API, the web app, the /mcp transport and the admin listener
  judgebot   the one long-running binary: --discord --api --web --mcp --admin --jobs as launch-time roles, and
             `judgebot ingest` (init, refresh, cards, rules, embed, …) over the loaders (bin)
  eval       gold-set harness: recall / answer / grade / rescore / show (bin)
  agent      the judge for other agents: sessions, lookups and the pipeline as judge-cli and judge-mcp
  configure  judge-config: a localhost page editing judge.toml and .env, checked by the loaders (bin)
web/         SolidJS + TypeScript web app (Vite)
site/        the documentation site (Astro + Starlight); docs/ is its source
data/        categories.yaml (generates the Category enum), aliases.yaml, notes.yaml
eval/        gold.yaml, published/ (graded runs), runs/ (yours, gitignored)
docs/        EXPLAINER.md (the tour), ARCHITECTURE.md (the reference), DECISIONS.md (why),
             PROVIDERS.md (the model-provider reference), DEPLOYMENT.md (the runbook)
```

One process is one spend cap, one judge role and one Discord application, by design
(`docs/DECISIONS.md` D16). Tournament policy (MTR/IPG) is not covered: the bot declines
those questions rather than guessing.

## License and attribution

AGPL-3.0-or-later. See [LICENSE](LICENSE). If you run a modified version as a network
service (Discord, the HTTP API or MCP), you must make your modified source available to
its users. The bot does this for you: every remote interface shows the licence, the
copyright, the source repository and the commit it was built from:

- the web app's footer and `GET /api/about`
- Discord's `/help` and `/license`
- the MCP server's instructions and its `about` tool
- `judge-cli about`

CI stamps the commit into the published image, and `git rev-parse HEAD` stamps it into a
local build. If you change anything, set `JUDGE_SOURCE_URL` in `.env` to the repository
with your changes, and every interface points there. That covers your obligation under
section 13.

The same interfaces show who runs the instance. The bot requires
`JUDGE_OPERATOR_DISCORD` (a Discord username) and the web app, API and MCP require
`JUDGE_OPERATOR_EMAIL` (a support address). Each shows the other contact too when it is
set. `judge-cli` and `judge-mcp` on stdio need neither.

This is unofficial Fan Content permitted under Wizards of the Coast's [Fan Content
Policy](https://company.wizards.com/en/legal/fancontentpolicy), not approved or
endorsed by Wizards. Magic: The Gathering, the Comprehensive Rules, card text and
rulings are © Wizards of the Coast. Card data and rulings come from
[Scryfall](https://scryfall.com) under its [data guidelines](https://scryfall.com/docs/api).
An instance downloads both when it loads its data (`judgebot ingest`). The repository
carries only a short excerpt of the rules as a test fixture. Rule links go to the
independent [Yawgatog](https://yawgatog.com/resources/magic-rules/) rules mirror.
`NOTICE` has the full statement. The web app and every page of the documentation site
repeat this notice in their footers, and the bot's `/help` gives a short form of it.
