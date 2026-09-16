---
title: MCP and judge-cli for agents
description: The judge as a tool surface for other agents, over MCP or a shell, including the session mode in which the agent is the model.
sidebar:
  order: 3
---

`crates/agent` exposes the pipeline to other programs in two transports over one set of
operations: **`judge-mcp`**, an MCP server on stdio, and **`judge-cli`**, one subcommand
per operation with JSON output. `judge-api --mcp` also serves the same MCP tools over HTTP at
`/mcp`, for agents that are not on the host; it needs an `MCP_TOKEN` too, and the flag
without a token is refused at startup.

## Two ways to get an answer

**`judge`** runs the built-in pipeline: extraction and synthesis on the configured models,
citation validation, one retry. It spends model budget under the process's `JUDGE_MAX_USD`
and, over `/mcp`, counts against `MCP_JUDGE_LIMIT`.

**Sessions** turn the pipeline inside out. The agent asks for a session, receives the
extraction prompt, answers it itself, receives the synthesis prompt rendered from the very
same retrieved context the bot would use, may ask for one round of extra rules, and
submits a verdict, which is admitted only through the same citation validation, with the
same rejection notice and one retry. No model call is made by the judge; the agent is the
model. This is how Claude Code answers rules questions in this repository (the `judge`
skill under `.claude/skills/`), and how a reported bad answer is reproduced for free.

```sh
judge-cli begin "does bob's trigger count goyf's mana value as 0?"   # → session id + extraction prompt
judge-cli extract <session> extraction.json                          # the agent's extraction → synthesis prompt
judge-cli rules <session> 202.3 702.19                               # optional, once
judge-cli verdict <session> verdict.json --persist                   # validated, or a rejection to correct
```

Session-persisted calls are thread history only: nothing can rate them, so the prior-call
leg never shows them as examples. Session thread ids carry an `agent:` prefix that a
Discord thread id can never have, so a session cannot read a channel's history.

## Lookups

`card <name>`, `card-info <uuid>`, `get-rules <id>...`, `search <query> [--limit N]` and
`glossary <term>` query the database directly and cost nothing. `config` prints the
resolved provider and model setup with secrets redacted, and `about` the source offer:
the repository holding this instance's source, the commit it was built from, the licence
and copyright. Over MCP the same offer is the tail of the server's initialization
instructions and the `about` tool, so a client sees it before calling anything.

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
