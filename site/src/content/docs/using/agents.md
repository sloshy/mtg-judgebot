---
title: MCP and judge-cli for agents
description: The judge as a tool surface for other agents, over MCP or a shell, including the session mode in which the agent is the model.
sidebar:
  order: 4
---

`crates/agent` exposes the pipeline to other programs. The same set of operations is
available three ways:

- **`judge-mcp`**: an MCP server on stdio.
- **`judge-cli`**: one subcommand per operation, printing JSON.
- **`judgebot --mcp`**: the same MCP tools over HTTP at `/mcp`, for agents that are not
  on the host. It needs an `MCP_TOKEN`, and the flag without a token is refused at
  startup. The [admin tools](#admin-tools) are there only for `MCP_ADMIN_TOKEN`.

## Pipeline and sessions

**`judge`** runs the built-in pipeline: extraction and synthesis on the configured models,
citation validation, one retry. It spends model budget under the process's `JUDGE_MAX_USD`
and, over `/mcp`, counts against `MCP_JUDGE_LIMIT`.

**Sessions** turn the pipeline inside out: the agent is the model, and the judge makes
no model call.

1. The agent asks for a session, receives the extraction prompt and answers it itself.
2. It receives the synthesis prompt, rendered from the same retrieved context the bot
   would use. It may ask for one round of extra rules.
3. It submits a verdict. The verdict is admitted only if it passes the same citation
   validation as the bot's, with the same rejection notice and one retry.

This is how Claude Code answers rules questions in this repository (the `judge` skill
under `.claude/skills/`), and how a reported bad answer is reproduced for free.

```sh
judge-cli begin "does bob's trigger count goyf's mana value as 0?"   # → session id + extraction prompt
judge-cli extract <session> extraction.json                          # the agent's extraction → synthesis prompt
judge-cli rules <session> 202.3 702.19                               # optional, once
judge-cli verdict <session> verdict.json --persist                   # validated, or a rejection to correct
```

Session-persisted calls are thread history only. Nothing can rate them, so the prior-call
source never shows them as examples. Session thread ids carry an `agent:` prefix that a
Discord thread id cannot have, so a session cannot read a channel's history.

## Release check

`judge-cli` and a stdio `judge-mcp` are built on their own and read the instance's
database directly, so each compares its release with the one that last migrated the
database:

- **A newer release of the same major:** a note (on stderr for `judge-cli`, at the top
  of the MCP instructions), and everything keeps working.
- **Another major release, or another build of a new major's pre-release**
  (`3.0.0-dev`): every operation fails
  with a message saying what to update, until the tool is rebuilt or the instance is
  upgraded. The message names the commit the instance was built from, so the tool can
  be built from the same one (`git checkout <commit>`, then
  `cargo build --release -p judge-agent`). `judge-mcp` keeps
  running and returns that message from each tool, and its instructions lead with it.
- **A database missing a migration the tool carries** (the instance is older than the
  tool): the same stop, naming the instance as the one to upgrade.

- **Migrations the tool does not know, which no newer release of its major explains**
  (another build of the same `-dev` version migrated the database, or it was migrated
  with `sqlx migrate run`, which records no release): the same stop.
- **A release that cannot be read:** the same stop, naming the error, until a later
  operation reads it.

`about` is never stopped, so it can still say where the instance's source is. Over
`/mcp`, while the instance migrates its database for a major release, every tool but
`about` returns the maintenance message as its error; try again in a minute or two.
`judge-mcp` checks again a minute after its last check, so an upgrade under a running
server is noticed within a minute. `judgebot --mcp` is part of the instance and does no
check. `judge-mcp` run inside the image over SSH does, and passes, being the instance's
own build.

## Private calls

`judge` and `begin_session` take `private: true` (`--private` on `judge-cli judge` and
`begin`). The call is stored marked private: only the operator sees it, it is never an
example for a later question, and a public question in the same thread does not read it.
A private question reads its own thread whole, private calls included, because an
`agent:` thread is known only to whoever it was returned to. Every call records whether
it came from MCP, `judge-cli` or a session.

## Lookups

`card <name>`, `card-info <uuid>`, `get-rules <id>...`, `search <query> [--limit N]` and
`glossary <term>` query the database directly and cost nothing. `config` prints the
resolved provider and model setup with secrets redacted. `about` prints the source offer:
the repository holding this instance's source, the commit it was built from, the licence
and copyright. Beside it, `freshness` gives the Comprehensive Rules release loaded and
when the data was last refreshed (`null` without a database). Over MCP the same offer is
the tail of the server's initialization instructions and the `about` tool, so a client
sees it before calling anything. The tool also returns `freshness`, which the
instructions, fixed at startup, do not carry.

## Admin tools

The operator's reads are operations too (D37), and so is the feedback queue (D38). They
return private material: other people's questions, Discord user ids and usernames,
raters, their reasons and failed questions.

| MCP tool | `judge-cli` | What it returns |
| --- | --- | --- |
| `admin_calls` | `calls` | Every stored call, private ones included, newest first, 50 a page. Filters: `interface`, `asker` (username or Discord id), `model`, `private` (true / false), `q` (text). |
| `admin_call` | `call <id>` | One call whole: origin, question, answer, citations, ratings with raters, the run with each billed request. |
| `admin_failures` | `failures` | The newest failed calls: question, error, first rejection, each attempt's text. |
| `admin_usage` | `stats` | Questions and spend per day, runs by model and outcome, ratings, the lowest-rated calls, the feedback queue's counts by category, the latest data refreshes. |
| `queue_list` | `queue` | The feedback queue: every *Partially correct* or *Incorrect* rating given a reason, newest first, 50 a page. Filters: `status` (open / resolved / dismissed), `category`, `interface`, `rater` (discord / web). |
| `queue_show` | `queue-show <item>` | One item with its call whole: question, answer, citations, cards, models, retired state, every rating with its reason. |
| `queue_resolve` | `queue-resolve <item> --note N [--fixed-by F]` | Close an item as fixed, saying what was done and what fixed it (a commit, a notes entry). The one write. |
| `queue_dismiss` | `queue-dismiss <item> --note N` | Close an item with nothing to fix, saying why. With `JUDGE_PUBLIC_FEEDBACK=true` this also takes the reason off the public pages. |

A rater who replaces their reason reopens the item, with the note kept. The `judge`
skill (`.claude/skills/judge/SKILL.md`) walks an agent through the queue: read an item,
reproduce it with a session, fix the cause, resolve it with a note.

Who gets them:

- **`judge-cli` and stdio `judge-mcp`**: always. Whoever runs them holds `DATABASE_URL`,
  which reads all of this anyway.
- **`/mcp` with `MCP_ADMIN_TOKEN`**: the admin tools beside the others. Off unless the
  variable is set; it must differ from `MCP_TOKEN` and `ADMIN_TOKEN`. Every admin call is
  logged at INFO with its tool's name.
- **`/mcp` with `MCP_TOKEN`**: never. That handler has no admin tools to list or call.

For an agent on another machine, run stdio `judge-mcp` over SSH, which needs no token
and no open port:

```json
{ "command": "ssh", "args": ["deploy-host", "docker", "compose", "--project-directory", "/path/to/mtg-judgebot", "exec", "-T", "judgebot", "judge-mcp"] }
```

`MCP_ADMIN_TOKEN` is for an agent that cannot run a command (a cloud agent, a hosted
connector).

## Connecting an MCP client

Locally, `.mcp.json` in the repository starts `target/release/judge-mcp` for Claude Code
(`cargo build --release -p judge-agent` first). Remotely:

```sh
claude mcp add --transport http judge https://judge.example.com/mcp \
  --header "Authorization: Bearer <MCP_TOKEN>"
```

`MCP_ALLOWED_HOSTS` must name the public hostname, and the token must be at least 24
printable ASCII bytes. The [deployment runbook](../../self-hosting/deployment/)
covers the rest.
