# MTG Judge Bot architecture

The design reference, kept current with the code. It was written stack-independent, before
the language was chosen (Rust, 2026-08-29).
`docs/DECISIONS.md` records why each choice was made. `docs/EXPLAINER.md` is the narrative
version for someone new to the ideas. This file is the terse one.

## 1. Goal

A Discord bot that answers Magic: The Gathering rules questions. Users reference
cards by full name, partial name, or nickname ("bob" → Dark Confidant). Every
answer cites its sources: Comprehensive Rules (CR) sections, Scryfall rulings,
and/or prior rated calls. Answers and user-submitted calls are rated 1–3
(incorrect / partially correct / correct). Ratings feed back into retrieval as
*examples*, never as authorities over the CR.

## 2. Entity-first hybrid retrieval

The design principle. Rules questions have the shape "card A + card B + rule concept C":

- **Card names are entities** → extraction + lookup, not embeddings.
- **CR is a numbered hierarchy** → chunk at the *rule* level (e.g. `702.19` with
  all lettered sub-rules and `Example:` lines), keep parent links, and map
  question categories to whole subsections (e.g. `613`, `614`).
- **Scryfall rulings are keyed by card** → fetch directly once cards resolve.
- **Three retrieval sources** for the fuzzy part: category map (structured),
  BM25/full-text (keywords like "leaves the battlefield"), vector (semantic).
- **The CR always outranks prior calls.**

## 3. Pipeline

```
Discord message (+ last N Q&A in the same thread)
  │
  ▼
[1] Entity extraction (LLM, low effort, structured output)
    Splits the message into card-name spans and rules concepts. Runs FIRST so
    fuzzy matching sees only candidate spans, not rules vocabulary.
    Model choice: `judge_bot::config` picks a provider per stage, from a
    `judge.toml`, else Anthropic direct from the environment. A provider is
    either Anthropic's Messages API or any OpenAI-compatible chat completions
    server. The Messages API is reached direct, through a proxy, or on a
    cloud account: Claude Platform on AWS and Bedrock (SigV4), Vertex AI
    (ADC). Cloud credentials come from the platform chain and are probed at
    startup.
    Every model sits behind the process's one spend-capped `Metered`, and
    the embedder behind a `MeteredEmbedder` on the same meter.
    A backend reports its `Capabilities`. If it cannot enforce the output
    schema server-side, the adapter appends the schema to the *user turn*.
    The system prompt is pinned by digest and is byte-identical on every
    backend.
  │
  ▼
[2] Card resolution (per span)
    A typed resolution order, each step tried only when the one above found nothing:
    alias → possessive-stripped alias ("bob's" → "bob") →
    exact name → printed-name table → short name before the comma ("Ragavan")
    → alias as a suffix → trigram fuzzy
    A [[bracketed]] span has its own resolution order: exact name → printed name →
    alias (whole span). It is CardSpan::Exact, chosen by an exhaustive
    match, so no looser step can resolve it. An alias maps one spelling to
    one card, so [[bob]] resolves. A miss is never resolved, only offered
    as Ambiguous:
      - the cards the other naming steps (possessive, short name, alias
        suffix) point at, under that step's matchedVia, so [[bob's]] offers
        Dark Confidant. Dropped as a duplicate when the extractor also
        named the card.
      - with no naming-step hit, the fuzzy neighbours.
    Output: Resolution = Resolved(card, matchedVia) | Ambiguous(candidates) | NotFound
    Duplicate references (nickname + full name) are dropped. A span is a
    duplicate when it is
      - Ambiguous from a non-fuzzy step and shares a candidate with a card
        Resolved from another span, or
      - NotFound, but its words appear as whole words in a resolved card's
        (face) name.
    Fuzzy-ambiguous spans are never dropped: trigram neighbours are not
    names the user could have meant.
    Remaining Ambiguous ⇒ "did you mean…?" buttons and stop. Never guess.
  │
  ▼
[3] Classification (same LLM call as [1])
    One required primary category plus up to 2 secondary, each with
    confidence, from a fixed taxonomy (~25 entries mirroring CR structure).
    The schema requires the primary, so an unclassified answer is an API error.
    Also assigns Source: CR | Commander | Tournament(MTR/IPG) | OutOfScope.
    Tournament/OutOfScope ⇒ reply "I don't cover that" with a pointer.
    Flags "nightmare" cards (Humility, Opalescence, Blood Moon, Mycosynth
    Lattice, Panglacial Wurm, …) → inject hand-written notes.
  │
  ▼
[4] Retrieval → Context
    - CR: category → curated subsection IDs (always) + tsvector BM25 + pgvector
          cosine. Union, dedupe, expand to full rule chunk.
          Order matters: the synthesis budget (25 chunks / 30 kB) renders
          only a prefix, and the sources return several times that. Priority:
            1. the primary category's rules sharing a word with the
               question (ranked by text relevance, not by id)
            2. BM25
            3. vector
            4. the primary's remaining rules
            5. secondary categories (ranked)
          `eval recall` gates on what was retrieved (≥ 90%) and on what
          that prefix shows (≥ 75%).
    - Scryfall rulings for each resolved card (all faces)
    - Glossary entries for terms in the oracle text
    - Prior calls: vector search filtered by (cards ∩ category), labeled with
      rating and CR version, shown as examples AFTER the CR material.
      Retired calls are excluded. The refresh's retirement pass retires a call when a
      citation's source no longer contains its quote, or a context card's
      Oracle text changed. It restores the call when both hold again.
      Renumbered rules carry their calls with them.
    - Nightmare notes
    - Thread history (last N Q&A)
  │
  ▼
[5] Synthesis (LLM, the model's measured effort: medium on Opus 5.5, high
    otherwise; structured output, one tool-use round)
    Same per-stage provider choice as [1]. The tool round and the schema
    travel in each backend's wire format (strict function calling on
    OpenAI-compatible servers when the provider says it has it).
    Tool: lookup_rules(ids). The model may request additional CR sections
    once before answering, closing the classifier-miss gap.
    Output: Verdict { answer, confidence: Low|Medium|High, citations[], category }
    The model does not report `source`, `crVersion` or `cards`. Validation
    stamps them:
      - source from the extraction, as AnswerableSource (by type, only
        Cr | Commander reach this step)
      - crVersion from the retrieved chunks
      - the resolved cards (CardRef: id + name) from Context
    Every interface then shows "Cards: …" from the verdict alone.
    Each citation = typed reference + quoted span. Validation:
      - first, citations that quote nothing (blank, a stock word, under 4
        chars) are dropped (D21). An answer with nothing else is a
        MalformedCitation.
      (a) the reference exists in Context
      (b) the span is a substring of that chunk. Curly and ASCII
          punctuation compare equal (judge_core::quote). The chunk's own
          text is stored for the span, so a stored quote is exact.
      (c) the verdict cites something and the answer is ≥ 40 chars,
          else JudgeError.EmptyVerdict
      (d) every rule number in the answer text is covered by a rule
          citation, else UncitedRules (D20)
    The answer text is an Answer newtype: a stray `\n`, `\t` or `\"` the
    model wrote as two characters is decoded when the verdict is made (D22).
    Failure ⇒ BadCitation / MalformedCitation / EmptyVerdict / UncitedRules →
    retry once (the notice says which), then reply with error.
    A misused tool round (ToolMisuse: a second lookup_rules call, or one
    whose ids cannot be read) produces no verdict and is retried the same
    way. The retry may make the one call only if the first could not be
    read, since nothing was fetched. A retry after UncitedRules is given the
    uncited rules it was not shown (`fetch_uncited`: looked up, or pinned if
    retrieval held them and the budget cut them), since a number reached through
    a glossary pointer is often not in the material.
    Always quotes CURRENT Oracle text (errata note if the printed text differs).
  │
  ▼
[6] Persist call (question, verdict, context ids, crVersion); rating buttons.
```

The two model calls ([1] and [5]) and the vector search of [4] reach their providers through
one seam (`docs/PROVIDERS.md`).

- `crates/llm` holds the provider-neutral request and response, the spend cap and the
  one-tool-round typestate. `ChatRequest` has system blocks, turns, tools, an output
  schema and an effort. `ChatResponse` has text, tool calls, a stop reason and usage.
  - The cap is a process total plus one *adjustment*. `judge_bot::budget` sets it from
    the `spend_days` ledger when `JUDGE_BUDGET_PERIOD` is `day` or `month`, which makes
    the cap a shared budget that survives restarts (D19). It also tells
    `JUDGE_ALERT_WEBHOOK` when the cap trips. `judge-cli stats` reads the same ledger.
- `crates/anthropic` and `crates/openai` are backends of it. Each owns its wire types and
  its schema-subset transform.
- The model's own previous turn is replayed as an opaque blob that only the backend that
  produced it reads (a thinking signature, a `reasoning_content`, a `tool_calls` array).
  The neutral layer never inspects it.
- The pipeline's port, `ChatModel`, is sealed and implemented only by `Metered`, so a
  model that bypasses the cap is unrepresentable. The embedders' port, `WithSpace`, is
  sealed the same way (`judge_embed::MeteredEmbedder`), on the same meter.
- Each backend declares `Capabilities`. What a server cannot enforce (an output schema,
  strict tools) the adapter moves into the prompt, since decoding and citation validation
  are client-side either way.

`judge_bot::config` reads a `judge.toml` into that: one provider per stage, secrets by
environment-variable name, and a price for every model the cap must reserve for. For
embeddings it also reads the vector `Space` (provider kind, model, width). The database
records that space in `embedding_space`, and every vector reader and writer checks it
before touching a column. Two models' vectors are therefore never mixed, and switching is
one explicit, transactional `ingest reembed`. Nothing in `crates/core` knows any of this exists.

One binary, `judgebot` (`crates/judgebot`), runs the long-lived interfaces. What a
process does is a set of roles chosen at launch: `--discord`, `--api`, `--web`, `--mcp`
and `--jobs`, on the command line or else in `JUDGE_ROLES`.

- `Role` is exhaustive and the set is a `NonEmpty`, so a process that does nothing is
  unrepresentable.
- `roles::plan` checks every role's requirements before the pool connects or anything
  binds, and reports every unmet one at once: `--discord` needs `DISCORD_TOKEN` and
  `JUDGE_OPERATOR_DISCORD`, the network roles `JUDGE_OPERATOR_EMAIL`, `--web` a built
  page and `--mcp` an `MCP_TOKEN`. The serving roles' models are built there too, so a
  missing or unpriced chat model is reported before the database is touched. What it
  returns is the types the roles run on, so a role cannot start unchecked. `--jobs` on
  its own with the schedule off is refused, since it would have nothing to do.
- The roles of one process share one composition (`judge_bot::serving::Serving`): one
  pool, one `Models` behind one `SpendMeter` and one budget ledger, one `Vectors`, one
  migration at startup. Each role keeps its own concurrency slots and limits. The HTTP
  listener is bound before the Discord gateway is contacted, then both run side by side,
  and the first to stop ends the process with a non-zero status.
- Of the processes running `--discord` on one database, one holds the gateway
  (`judge_bot::discord::gateway`). The Discord role takes a `GatewayLease` before it
  connects, or stands by until the holder lets go, with no bound on the wait and
  reconnecting if its own connection drops; its HTTP roles serve meanwhile. Having taken
  it, it waits `Timing::grace` (15 s), checks it still holds it, and connects. The holder
  checks every 5 s (4 s timeout). A failed or unanswered check closes the gateway
  (serenity's `ShardManager::shutdown_all`, asked again until a still-connecting shard
  registers) and, if it closed within 1 s, stands by again inside the role; otherwise the
  role ends with an error and the process exits. The grace exceeds the check interval,
  the check's timeout and the shutdown limit together (10 s), a relation checked at
  compile time, so an old and a new holder never answer side by side. A holder in a
  paused VM that loses its session is the case it cannot cover (`docs/DEPLOYMENT.md` §8).
  Every lease session sets server-side TCP keepalives (`lease::SESSION_SETTINGS`), so a
  vanished holder's lock is freed in about 25 s, and turns `statement_timeout`,
  `idle_session_timeout` and `transaction_timeout` off. A loss soon after the last one
  pauses before standing by again (30 s doubling to 10 min) to stay inside Discord's
  1000 logins a day per token, and a failed command registration is a warning, never a
  connection left serving nothing. The
  role's `Data` is one `Arc` across reconnects, and the symbol watcher a connection
  starts is aborted with it. `hold` takes the gateway as a closure handed a `Stop`, so
  the hand-off is tested without Discord. The scheduler's thread
  is different: beside serving roles its end is logged as an error and the process keeps
  answering, while a process whose only role is `--jobs` exits non-zero.
- `judgebot ingest <cmd>` is the data command line. `judge-bot`, `judge-api` and
  `judge-ingest`, the binaries it replaced, are links to it in the image: it reads the
  name it was invoked as, runs what that binary ran (`judge-bot` is `--discord --jobs`,
  `judge-api` its interfaces plus `--jobs`) and logs a warning naming the replacement.
- `judgebot backup <run|list|fetch|serve>` is the database backup (`judge_bot::backup`,
  D26): `pg_dump -Fc` over the network, gzipped, uploaded to R2 by a `SigV4` client of
  four requests, pruned past `BACKUP_KEEP_DAYS` after the upload. `serve` checks the
  bucket hourly and takes a backup when the newest `judgebot-<stamp>.dump.gz` is
  `BACKUP_EVERY_DAYS` old. It writes what `scripts/backup-db.sh` writes, so each reads
  the other's backups.
- `docker-compose.yml` runs the application as one long-running service, `judgebot`, whose command is
  `JUDGE_ROLES` or, unset, `--discord --jobs` plus the deprecated `API_INTERFACES`
  (default `--api --web`): `roles::COMPOSE_COMMAND`, which a test holds the file to.
  `roles::compose_roles` is the same rule in Rust, so `judge-config` checks only the
  roles the service would run. The service keeps the network alias `api` for tunnels
  configured with the old service name. `refresh` runs `judgebot ingest` on demand,
  and `backup` (profile `backup`, `.env.deploy` and no `.env`) runs `judgebot backup
  serve`.

Three interfaces share this pipeline through the same composition root
(`judge_bot::build_deps`):

- **Discord adapter** (`crates/bot`, the `--discord` role): `/judge` slash command, rating buttons,
  "did you mean…?" buttons, thread history.
  - `/judge private:True` is `Audience::Private`: acknowledged ephemerally, no history
    read, never persisted, so no rating buttons. A call that *fails* is recorded in
    `failed_calls` for any audience, flagged `private`, through `CallStore::record_failure`
    (D27); that is the one place a private question is stored. The audience rides in the pick button's
    custom-id through a card pick.
  - A card pick is answered from Discord alone, so any process holding the gateway
    answers it, after any restart. The prompt message restates the question verbatim
    above a body with no blank line in it: the lead line, the numbered choices (names cut
    to 60 characters) and up to two notes (`render::PickPrompt`, whose `parse` must
    re-render the content exactly). Each button's custom-id (`ids::Pick`) carries the
    asker, the audience, the ambiguous span's byte range in the question, a digest of
    the prompt's content (`pick::Digest`, 8 bytes of SHA-256) and the card's oracle id.
  - A click does not rely on Discord to check the custom-id against the message. It is
    refused (as expired) unless the message's content has the button's digest, so a
    button answers only the prompt it was made for, and unless the message's EPHEMERAL
    flag agrees with the button's audience. A message whose flags are missing is
    answered privately (`pick::audience`), never recorded. The card must be on the
    prompt's list.
  - The prompt's age is read from the message's last edit, else its snowflake id,
    against a ten-minute TTL. `pick_claims` makes each prompt single-use: one row per
    message, prompt time and digest, no text, taken after the judge slot so a "busy"
    leaves the buttons usable, and given back if the acknowledgement fails. Rows older
    than a day are deleted by the claims that follow. `/judge`'s question option
    has a `max_length` (1300) under which the whole question fits the prompt. Longer
    content (UTF-16-heavy) gets the choices listed without buttons.
  - The click needs the message's content. Discord's interaction object carries "the
    message they were attached to" for components, ephemeral ones included as far as
    the docs say, and the message-content restriction exempts messages the app sends.
    The docs make no explicit promise for ephemeral messages, so a private prompt whose
    content arrives empty or altered is answered "expired", never guessed at.
  - A per-user fixed window (`discord/cooldown.rs`, `JUDGE_USER_LIMIT`) is charged once a
    judge slot is held, so a "busy" is free. A card pick is not counted again.
  - `/card` and `/rule` are lookups over the resolver, the retriever's `lookup_rules` and
    `PgLibrary::rulings`. They call no model and touch no meter.
- **HTTP adapter** (`crates/api` + `web/`): anonymous `POST /api/judge` behind
  a per-IP fixed-window rate limit, and a SolidJS single page.
  - Each interface is a role of its own on one listener (`API_ADDR`): `--api`
    the JSON route, `--web` the page and `--mcp` the MCP transport. An interface
    nobody named is not mounted.
  - `GET /api/health` and `GET /api/about` are served whatever interfaces are off.
    The container healthcheck needs the first.
  - `/api/about` is the source offer (`judge_core::source`): repository, built
    commit, licence and copyright. The AGPL requires every remote interface to
    offer it. The page's footer reads it from there. Discord gives the same in
    `/help` and `/license`, and the MCP server in its initialization
    instructions and an `about` tool. `JUDGE_SOURCE_URL` points all of them at
    a fork.
  - `About` also carries the data's freshness (`judge_core::Freshness`): the
    CR release loaded and the age of the last successful refresh, and whether
    the latest one failed. It is read from `refresh_runs` and `rules`
    (`ingest::runs::freshness`, within 2 s, `null` and a WARN otherwise; a
    missing run table leaves the CR version alone). It is cached for a
    minute, single-flight, by a `FreshnessReader`: `/api/about` has one, and
    every `PgLibrary` another (`/help` in the bot, the MCP `about` tool), so
    a process with `--mcp` holds two. `/help` lists it, the page footer shows it as
    one line, and `/api/health` ignores it.
  - The same places name who runs the instance (`judge_core::operator`). The
    bot takes a `DiscordOperator` and the HTTP layer a `NetworkOperator`. The
    only way to make either is `Operator::for_discord` / `for_network`. So the
    bot cannot start without `JUDGE_OPERATOR_DISCORD`, and the network roles
    cannot start without `JUDGE_OPERATOR_EMAIL`, whichever of them it opens. A local
    `judge-cli` or stdio `judge-mcp` holds a plain `Operator` and needs neither.
  - There are no rating endpoints, because anonymous callers are not
    accountable identities.
  - Ambiguity is returned as data and resolved statelessly. The client re-asks
    with `pins: [{span, name}]`, which the server rewrites to `[[Full Name]]`
    with the same `pin_card` used by the Discord buttons.
  - Follow-up history comes from a client-generated session UUID, stored as
    thread id `web:<uuid>`.
- **Config editor** (`crates/configure`, binary `judge-config`): a page on
  `127.0.0.1` for `judge.toml` and the settings in `.env` (D23).
  - The `judge.toml` form is `judge_bot::config::file_schema()`: JSON Schema
    generated from the loader's serde types, with `x-endpoints` from
    `EndpointKey::on`, the table the loader's misplaced-key check reads.
  - `env::VARS` lists every variable `.env.example` carries as a secret or a
    setting. A value reaches the page only through a `Setting`. A secret, a
    variable the list does not know, or a hidden setting is reported as set or
    not, and is changed write-only (`DotEnv::replace`): the page sends a new value
    and every reply is scrubbed of it. Help text is `.env.example`'s
    comments.
  - Each draft goes through `check::run`: the `judge.toml` loader,
    `discord::Config::from_vars`, `ApiConfig::from_vars` and `check`, the
    migration flag. Errors carry `ConfigError::location()`, the key or variable
    to fix.
  - Writes go through `toml_edit` and a line-preserving `.env` writer, so only
    changed lines move. They are refused when a file changed since it was read.
- **Agent adapter** (`crates/agent`): the judge as a tool surface for *other*
  agents.
  - Over MCP, `judge-mcp` serves a local client on stdio. For a remote one,
    `judgebot --mcp` mounts the same handler at `/mcp` behind `MCP_TOKEN`. The
    flag without a token is refused at startup. The token without the flag
    serves nothing and warns.
  - `judge-cli` has one subcommand per operation with JSON out, for a shell
    agent (the repo's `.claude/skills/judge` skill).
  - It offers the pipeline two ways:
    - The `judge` tool runs it as above, with the built-in model calls. It is
      spend-capped, shares the web route's concurrency semaphore, and is
      offered only when a model is configured (`ANTHROPIC_API_KEY` or a
      `judge.toml`).
    - A **session** runs it in pull mode, where the calling agent *is* the
      model.
  - `judge_bot::session` is that state machine.
    1. `begin` returns the extraction prompt (steps 1 + 3 as text, plus the
       JSON Schema).
    2. The agent's `Extraction` JSON drives steps 2 + 4 and yields the
       synthesis prompt: the same system prompt, with `Harness`-specific
       wording for the one `lookup_rules` round and the output format, plus
       the rendered material.
    3. The agent's `Verdict` JSON goes through the same `Verdict::validate`
       against the session's own `Context`, with the same one retry and the
       same rejection notice.
  - On the API path, typestates carry the invariants (one tool round, one
    retry, only a validated verdict is persisted). Here they are a `Stage`
    enum, because the state lives in Postgres between calls (`agent_sessions`,
    one jsonb document, optimistic version). The session id is a
    server-minted handle passed as a tool argument, which is what the
    2026-07-28 MCP revision adopted in place of protocol sessions. The HTTP
    transport is served statelessly for every protocol version.
  - Sessions are unauthenticated at the tool level. Their thread ids are
    therefore a type (`AgentThread`, always `agent:<uuid>`) that cannot name a
    Discord or web thread, and their inputs are bounded (question, spans,
    concepts, lookup ids, answer length). A persisted call is keyed by session
    (`calls.session_id`, unique), so persisting twice cannot file two calls. It
    is excluded from the prior-call query: nothing can rate it, and it is history
    for its own thread only.
  - Over HTTP, `judge` runs are also capped per window (`MCP_JUDGE_LIMIT`) so a
    leaked token cannot take the public page's slots and spend cap with it.
  - Read-only lookups (resolve a card, rules by id or search, rulings, notes,
    glossary) round the surface out (`PgLibrary`).

The Discord and web interfaces draw Magic's card symbols (`{W}`, `{2/U}`, `{T}`) as
pictures, from one set of names:

- **Discord** substitutes *application* emoji (`<:mana_w:…>`). The bot owns them,
  not a server, so they work in every guild and cost no emoji slots.
  - `ingest emoji` uploads them: Scryfall's SVG → a 128 px PNG via resvg,
    scaled to fit and centred (nine of the 84 symbols are not square).
  - The emoji name is defined once, in `judge_core::symbol`, with no I/O. It is
    in core because the uploader and the renderer are separate programs that
    must agree on it exactly. A test pins the mapping as total and injective
    over everything Scryfall publishes, so no symbol can silently overwrite
    another's emoji.
  - A tag is ~28 characters where `{W}` is three, and Discord counts the tag.
    `mana::Rendered` keeps text as segments and lets only plain text be cut, so
    a half-written tag is unrepresentable rather than tested against.
  - An application with no emoji uploaded gets an empty table and the literal
    `{W}`, unchanged.
  - The table sits behind a swappable `mana::SharedSymbols`; a render takes
    a snapshot. `discord::symbols::watch`, a task spawned on `Ready` so
    connecting never waits on it, reads the run record's mark, lists the emoji,
    then every ten minutes reads `refresh_runs` for a run finished since the
    mark whose `emoji` step may have uploaded (`runs::emoji_since`: anything
    but skipped or `uploaded: 0`). It lists them again after such a run, after
    a failed listing, while the table is empty, and hourly regardless, which
    catches uploads and deletions the record does not show. So a refresh by
    any process (a `--jobs` role, a cron'd or manual `judgebot ingest`) or an
    upload by hand reaches a running bot without a restart.
- **Web** renders Scryfall's SVGs inline from their CDN (`web/src/symbols.ts`
  is the generated table, `Symbols.tsx` the component).
  - A symbol the table does not know, or an image that fails to load, falls
    back to the literal text.
  - `split`/`lookup` mirror the Rust scanner, so both surfaces accept the same
    spellings (`{W/U}`, `{w/u}`, `{U/W}`, `{WU}`) and leave the same text alone.
  - Seven symbols (`{E} {P} {PW} {CHAOS} {TK} {L} {D}`) are flat black with no
    disc, invisible on the dark palette. They carry a `flat` flag and are
    inverted in dark mode. The coloured ones must not be.
  - If a Content-Security-Policy is ever added to the HTTP adapter, `img-src` must
    allow `https://svgs.scryfall.io`.

## 4. Data

| Source | Refresh | Storage |
|---|---|---|
| Scryfall bulk `oracle-cards.json` | daily (the scheduled refresh `judgebot --jobs` runs, `judge_bot::jobs`, every `JUDGE_REFRESH_HOURS` — DEPLOYMENT.md §7) | `cards` (oracle_id, name, layout, type_line, …) + `card_faces` (oracle_id, face_idx, name, oracle_text, mana_cost, …) |
| Scryfall bulk `default-cards.json` (names only) | daily | `printed_names` (printed_name, oracle_id) — old names, errata'd names |
| Scryfall bulk `rulings.json` | daily (bulk-loaded, keyed by oracle_id) | `rulings` (oracle_id, key, published_at, text) — `key` = content hash (`judge_core::ruling_key`), so a reindexed ruling keeps its identity |
| Comprehensive Rules txt | on CR release — detected daily from the `.txt` link on Wizards' rules page vs `max(cr_version)` | `rules` (id, parent_id, subsection, heading, body, examples, embedding, cr_version) |
| CR Glossary | same | `glossary` (term, text, embedding) |
| Scryfall `/symbology` (84 card symbols) | daily (idempotent, uploads only missing symbols) | not stored: uploaded as Discord application emoji (`ingest emoji`) and hard-coded for the web page (`web/src/symbols.ts`) |
| Nicknames | hand-curated YAML, compiled in; the refresh reloads a built-in copy an upgrade changed | `card_aliases` (alias, oracle_id) |
| Nightmare notes | hand-written markdown, likewise | `card_notes` (oracle_id, note) |
| Curated list sources | written by each alias or note load, in its transaction | `curated_lists` (list, source `builtin`/`file`, digest, loaded_at): the refresh's `lists` step reloads only a `builtin` list whose digest is not the binary's (`judge_bot::ingest::lists`) |
| Categories → subsections | YAML (single source of truth; the enum is generated from it) | `categories` |
| Calls | continuous; `retired_at`/`retired_reason` recomputed on each refresh from citation validity | `calls` (id, thread_id, question, answer, category, citations jsonb, source, cr_version, retired_at, retired_reason, embedding) |
| Ratings | continuous | `ratings` (call_id, user_id, score, is_judge, ts) |
| Refresh runs | one row per refresh, scheduled or `judgebot ingest refresh` | `refresh_runs` (started_at, finished_at, trigger, process, cr_before, cr_after, steps jsonb, ok) |

The loaders in the table (cards, rulings, the CR, symbols, nicknames, notes) and the
embedding step are `judge_bot::ingest` (`crates/bot/src/ingest/`), beside the other
Postgres adapters. `judgebot ingest` is the command line over them.

The database has three advisory lock keys, listed together in `judge_bot::lease` with a
compile-time check that they differ:

| Key | Scope | Held by |
| --- | --- | --- |
| `REFRESH_LOCK` | session | a data-writing run (`RefreshLease`) |
| `GATEWAY_LOCK` | session | the process connected to the Discord gateway (`GatewayLease`) |
| `CALLS_REWRITE_LOCK` | transaction, shared or exclusive; session for a migration | a persist that writes a vector and each embed batch (shared); the CR load, the retirement pass, a space switch (exclusive); a migration, for its whole run |

The two session locks are a `Lease<K>`, typed by what they guard (`K` is `Refresh` or
`Gateway`, a sealed `LeaseKey` carrying the key, the `application_name` label and, in its
own `impl`, the wait), so one lease cannot stand in for the other.

Every loader that writes the database takes a `RefreshLease`, a session-level advisory
lock (`REFRESH_LOCK`) held on a connection of its own, so two runs never overlap, in one
process or several, and a step without it does not compile. Steps take it as
`&mut RefreshLease`, so two cannot run at once under one lease either. Waiting for it is
bounded (an hour), and a run checks before each step that its session still holds it. Postgres drops the lock with
the session, so a crashed run leaves nothing to clean up. It is separate from
`CALLS_REWRITE_LOCK`, the short transaction-scoped lock the CR load, the retirement pass
and vector writes take inside a run (and a migration for its whole run). A run takes the
lease first and the calls lock inside it, never the other way round.

Every `ingest::refresh`, whatever started it, checks before each step that it still
holds the lease and that the migration ledger matches its binary (`migrate::skew`), and
stops at `ingest::RUN_TIMEOUT` (3 h), abandoning the step in progress. A schema change
skips the remaining steps and the run is `RunOutcome::Stopped`, stored with `ok` null:
neither a success nor a failure. A single-step `judgebot ingest` command makes the same
check once (`ingest::ensure_writable`). `runs::history` counts an unfinished row older
than `runs::ABANDONED_AFTER` as a failed run, so a process that dies mid-run still
lengthens the failure streak.

The schedule is `judge_bot::jobs`, which a process with `--jobs` starts after migrating. It runs
on an OS thread of its own, with a current-thread runtime and a three-connection pool, so
a run's file I/O, CR parse and queries never take a worker or a pooled connection from
the request path. The database is shared, though: while the CR load or the retirement
pass holds the exclusive side of `CALLS_REWRITE_LOCK`, a `PgCallStore::persist` that
writes a vector waits on the shared side, and both interfaces persist before they reply.
A cron run has the same effect.

Every ten minutes or so the scheduler reads the migration ledger (a schema ahead of or
behind the binary pauses it), then `refresh_runs`; a database with no rules waits for
`init`, which records itself as a manual run, so the first scheduled refresh comes one
interval after it. The pure `jobs::due` decides on the database's clock: due when the last success
is older than `JUDGE_REFRESH_HOURS` and the last attempt older than `jobs::backoff` (1 h,
doubling per failure in a row, capped at the interval). When due it tries the lease
without waiting, re-reads the record under it and runs `ingest::refresh` with
`Trigger::Schedule`. That trigger also caps `embed` at `embed::UNATTENDED_CEILING` rows
(`Skip::EmbedCeiling`), so no timer pays for a mass re-embed. The webhook
(`judge_bot::alert`, shared with the spend cap) hears the first failure of a streak, the
recovery, a new ceiling skip and a panicking check. D24 records why the schedule is in
the process and not in cron.

The record is also what users and the operator see of it. `About` carries the CR release
loaded and the age of the last successful run (see the HTTP adapter above), and
`judge-cli stats` lists the last five runs (`runs::recent`) with their outcome and failed
steps.

Database: **Postgres 16 + pgvector + pg_trgm**. Scale: ~30k cards, ~2k rule
chunks, <10k calls.

Embeddings: **Voyage AI** by default (`voyage-3.5` or `voyage-4` family; `voyage-3` is
superseded). It was chosen over local models for the zero-config setup, because local
models are not worth it on WSL2. `Embedder` is an interface, so this can change:
`judge_embed` also has an OpenAI-compatible `/v1/embeddings` adapter for local or hosted
models, chosen by `[models.embed]` in `judge.toml`.

Every embedder carries its `Space` (provider kind, model, width). The one-row
`embedding_space` table records the space the stored vectors belong to, and
`ingest embed` refuses to write into another. On a mismatch the adapters turn vector search
off (error log, never mixed). The space is re-checked on every use, and anything that
writes a vector holds it under a shared advisory lock. `ingest reembed --yes` switches the
database in one transaction after probing the new embedder (`docs/PROVIDERS.md` §4.3).

Rating aggregation: Bayesian-smoothed mean (prior 2.0, weight 3). A rating with
`is_judge = true` (operator-assigned role) dominates crowd votes. It is used for
labeling and ordering *among* prior calls, and for exclusion (< 1.5 with ≥ 5
votes). It never ranks prior calls above CR chunks.

## 5. Domain model

The model is language-neutral.

```
CardId        = Scryfall oracle_id
RuleId        = "702.19" | "702.19b"
Face          { name, oracleText, manaCost, typeLine }
Card          { id, name, layout, faces: NonEmpty[Face] }
Category      closed set, ~25 values, sourced from categories.yaml
Source        CR | Commander | Tournament | OutOfScope
Confidence    Low | Medium | High
Resolution    Resolved(card, matchedVia) | Ambiguous(query, candidates) | NotFound(query)
Quote         non-blank text; compared to its source with typographic punctuation folded
Citation      Rule(id, quote) | ScryfallRuling(card, rulingKey, quote)
              | OracleText(card, quote) | PriorCall(id, quote)
Context       { cards, rules, rulings, glossary, prior, notes, history }
Verdict<S>    { answer, confidence, citations, category, source, crVersion, cards }
              S = Unvalidated | Validated; only Verdict<Validated> can be stored or shown
Rejection     BadCitation(citation) | Malformed(what) | Empty(why) | Uncited(rule ids)
              | Tool(SecondRound(ids) | Unreadable(what)) | Oversized { chars }
RejectedAttempt  { answer, rejection }   — quoted back to the retry as a blockquote
JudgeError    AmbiguousCards(...) | CardsNotFound(...) | OutOfScope(source)
              | BadCitation | MalformedCitation | EmptyVerdict | UncitedRules
              | ToolMisuse | LlmRefused | Upstream(err)

Ports:  Extractor, Resolver, Retriever, Synthesizer, Embedder, CallStore
judge : Question -> IO[Either[JudgeError, Verdict]]
```

## 6. Build order

The build was eval-first: retrieval was measured before any synthesis existed.

1. A gold set of adversarially verified questions with expected rule ids
   (`eval/gold.yaml`, 22 questions today, one of them a human correction).
2. Ingest.
3. Extraction and resolution, tested on the gold set's card mentions.
4. Retrieval, behind a **gate of ≥ 90% of gold rule ids present in the Context**.
5. Synthesis with citation validation, scored against the gold answers.
6. The Discord adapter with rating buttons.
7. The prior-call query, which needs rated data to exist.

`judge-eval recall` still runs that gate for free on every retrieval change.

## 7. Non-goals

- Tournament policy (MTR/IPG). The bot declines those questions rather than winging them.
- Accounts or ratings on the web page. The anonymous page never rates.
- Retraining of any kind.
- Automatic detection of "nightmare" cards (the notes are curated by hand).
- Multi-server tenancy. The bot is meant to be run by each community for itself
  (`docs/DECISIONS.md` D16), so one process has one spend cap, one judge role and one
  token by design.
