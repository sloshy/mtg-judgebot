---
title: The config editor
description: judge-config, a page on localhost for editing judge.toml and the settings in .env, with every draft checked by the binaries' own loaders.
sidebar:
  order: 5
---

`judge-config` serves a page on `127.0.0.1` for editing [`judge.toml`](../models/) and the
non-secret settings in `.env`. It is optional: both files stay plain text, and the
editor writes them the way you would.

- **Forms from the loader's types.** The `judge.toml` form is generated from the types the
  loader parses into, and the help text is their documentation. A setting's help is its
  comment in `.env.example`. A knob added in a release appears in the editor without
  any change to it.
- **Each draft is checked by the binaries' own code.** On every change, the draft runs
  through the same loaders the binaries use at startup. The side panel says whether the
  database settings, the model configuration, the Discord bot and the HTTP API would
  start, and which key or variable to fix if not.
- **Only the lines you change are written.** Comments, blank lines, key order and the
  spelling of every unchanged value survive. The Review tab shows the diff of both
  files before anything is written.
- **Secrets stay in the file.** API keys, `DISCORD_TOKEN`, `MCP_TOKEN`,
  `JUDGE_ALERT_WEBHOOK`, `DATABASE_URL` and any variable the editor does not know are
  never shown or written. The page says only whether each is set. A `judge.toml`
  provider's `api_key_env` gets a "set in .env" or "not set in .env" badge.

## Running it

From the directory holding `.env` (the repository checkout):

```bash
cargo run --release -p judge-configure
```

It prints a URL ending in `#token=…`. Open that URL; the page needs the token. Ctrl-C
stops the editor.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--env FILE` | `./.env` | The `.env` to edit. When it does not exist, the first save creates it from `.env.example`, readable by its owner only. |
| `--config FILE` | `JUDGE_CONFIG` from that `.env`, else `judge.toml` beside it | The `judge.toml` to edit. With no file, the page offers to create one or to stay on the zero-config setup. |
| `--listen ADDR` | `127.0.0.1:8790` | Where to listen. |
| `--allow-host NAME` | | A `Host` header to accept besides `localhost`, `127.0.0.1` and `[::1]`. |

### On a deployment host

The published image carries `judge-config`. Run it with the checkout mounted and the port
published on loopback only, as your own user so the files keep their owner:

```bash
docker compose run --rm --no-deps --user "$(id -u):$(id -g)" -v "$PWD:/work" -w /work -p 127.0.0.1:8790:8790 --entrypoint judge-config api --listen 0.0.0.0:8790
```

From another machine, forward the port over SSH (`ssh -L 8790:127.0.0.1:8790 <host>`) and
open the printed URL there. A relative `JUDGE_CONFIG` resolves inside the mount. An
absolute one names a path in the container, so pass `--config` instead.

## What it checks, and what it leaves to startup

| Panel entry | Runs | Fails on, for example |
| --- | --- | --- |
| Database | `DATABASE_URL` present, `DB_PORT`, `JUDGE_AUTO_MIGRATE` | an unset `DATABASE_URL`, `JUDGE_AUTO_MIGRATE=maybe` |
| Models | the `judge.toml` loader (or the zero-config setup), the spend settings, the source offer | a misplaced key, an unpriced model on an `openai` provider, an unset `api_key_env` variable |
| Discord bot | the bot's own settings, then its operator contact | an unset `DISCORD_TOKEN`, a bad `GUILD_ID`, no `JUDGE_OPERATOR_DISCORD` |
| HTTP API | the API's settings and the doors `API_INTERFACES` opens, then its operator contact | `--mcp` with no `MCP_TOKEN`, a bad `API_ADDR`, no `JUDGE_OPERATOR_EMAIL` |

Two checks depend on the machine that serves rather than on the files, so the editor
leaves them to startup:

- a cloud door's credential chain (`claude-platform-on-aws`, `bedrock`, `vertex`)
- `--web`'s `WEB_DIST` directory (the image sets its own)

A part that would refuse to start does not block saving. A deployment that never runs
the bot does not need `DISCORD_TOKEN`.

The panel also warns about the files together:

- a `judge.toml` the containers will not see, because `JUDGE_CONFIG` is blank or names
  another file
- a `DB_PORT` that differs from the port in `DATABASE_URL`

## After saving

The binaries read both files only when they start.

- Under Docker, after a `.env` change: `docker compose up -d`, which recreates the
  containers whose environment changed.
- After a `judge.toml` change: `docker compose restart bot api`. The mounted file's
  content is not a change `up -d` sees.
- A `cargo run` binary picks up both on its next start.

## What it refuses

- A `.env` that dotenvy cannot read, that assigns a variable twice, or whose assignment
  is spelled in a way the editor cannot place on one line. The binaries and Compose
  would disagree about such a file, so fix it by hand first.
- A setting whose value expands `$`, or is a URL carrying a user name or password. It is
  marked "set, not shown" and is edited in the file.
- A `judge.toml` that is not valid TOML opens in the Text view. Saving the text is
  allowed, and the loader reports what it makes of it.

The editor never deletes a file. To go back to the zero-config setup, remove
`judge.toml` yourself.

## Access

The page's calls need the token, sent in a header that a page from another origin cannot
add. The `Host` must be a loopback name or one passed with `--allow-host`, which closes
DNS rebinding. Anyone who can reach the port and holds the token can change your
settings, so keep the port on loopback.
