---
title: The config editor
description: judge-config, a page on localhost for editing judge.toml and .env (secrets write-only), with every draft checked by the binaries' own loaders.
sidebar:
  order: 5
---

`judge-config` serves a page on `127.0.0.1` for editing [`judge.toml`](../models/) and
`.env`, and is the way to set a judgebot's configuration.

- Both files stay plain text, so editing them by hand remains possible.
- The editor writes them the way you would: only the lines you change.

- **Forms from the loader's types.** The `judge.toml` form is generated from the types the
  loader parses into, and the help text is their documentation. A setting's help is its
  comment in `.env.example`. A knob added in a release appears in the editor without
  any change to it.
- **Each draft is checked by the binaries' own code.** On every change, the draft runs
  through the same loaders the binaries use at startup. The side panel says whether the
  database settings, the model configuration and the roles would start (the Discord bot
  and the HTTP API, each when `JUDGE_ROLES` runs it), and which key or variable to fix
  if not.
- **Only the lines you change are written.** Comments, blank lines, key order and the
  spelling of every unchanged value survive. The Review tab shows the diff of both
  files before anything is written.
- **Secrets are write-only.** API keys, `DISCORD_TOKEN`, `MCP_TOKEN`,
  `JUDGE_ALERT_WEBHOOK`, `DATABASE_URL` and any variable the editor does not know are
  never shown. The page says whether each is set.
  - **Replace…** (**Set…** when unset) opens a masked input for typing or pasting a new
    value. **Show** reveals what you typed, and **Cancel** drops it.
  - The value goes into the file and nowhere else. The Review tab lists the variable as
    `NAME: new value (not shown)`, and no reply from the editor carries it.
  - **Add variable** writes one the editor does not know, such as a new provider's key.
    An `api_key_env` that `.env` does not assign is offered there already.
  - A `judge.toml` provider's `api_key_env` gets a "set in .env" or "not set in .env"
    badge.

## Running it

From the repository checkout:

```bash
scripts/config.sh
```

- It runs the editor from the image (`docker compose run` on the `config` service), so it
  needs no Rust toolchain and works before `.env` exists. Run `docker compose pull`
  first, or `docker compose build config` for your own checkout.
- The editor is not in images before 1.2.0. With `JUDGE_IMAGE_TAG` pinned to one, the script
  says so: use `latest` or a later release, or run it from source (below).
- It prints a URL ending in `#token=…`. Open that URL; the page needs the token.
- Ctrl-C stops it.
- It runs as your user, so the files it saves keep their owner.
- From another machine, forward the port over SSH first
  (`ssh -L 8790:127.0.0.1:8790 <host>`) and open the printed URL there.
- Under rootless Docker or Podman, `--user` maps to another id, and under SELinux the
  bind mount needs relabelling, so saving fails. Run it from source there.

Arguments after `scripts/config.sh` go to the editor:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--config FILE` | `JUDGE_CONFIG` from `.env`, else `judge.toml` beside it | The `judge.toml` to edit. With no file, the page offers to create one or to stay on the zero-config setup. Under the script, an absolute `JUDGE_CONFIG` names a path outside the mounted checkout, so pass a path inside it. |
| `--allow-host NAME` | | A `Host` header to accept besides `localhost`, `127.0.0.1` and `[::1]`. |
| `--env FILE` | `./.env` | The `.env` to edit. When it does not exist, the first save creates it from `.env.example`, readable by its owner only. |
| `--listen ADDR` | `127.0.0.1:8790` | Where to listen. The script listens inside the container and publishes on `127.0.0.1:8790`. |

### From source

With Rust installed, the same editor runs without Docker:

```bash
cargo run --release -p judge-configure
```

## Validation

| Panel entry | Runs | Fails on, for example |
| --- | --- | --- |
| Database | `DATABASE_URL` present, `DB_PORT`, `JUDGE_AUTO_MIGRATE` | an unset `DATABASE_URL`, `JUDGE_AUTO_MIGRATE=maybe` |
| Models | the `judge.toml` loader (or the zero-config setup), the spend settings, the source offer | a misplaced key, an unpriced model on an `openai` provider, an unset `api_key_env` variable |
| Roles | the roles the compose service would run: `JUDGE_ROLES`, else every role but `--mcp` | an unknown flag, a role named twice, a set `API_INTERFACES` (retired) |
| Discord bot | under `--discord`: the bot's own settings, then its operator contact | an unset `DISCORD_TOKEN`, a bad `GUILD_ID`, no `JUDGE_OPERATOR_DISCORD` |
| HTTP API | under `--api`, `--web` or `--mcp`: the API's settings and those roles, then its operator contact | `--mcp` with no `MCP_TOKEN`, a bad `API_ADDR`, no `JUDGE_OPERATOR_EMAIL` |

Two checks depend on the machine that serves rather than on the files, so the editor
leaves them to startup:

- a cloud endpoint's credential chain (`claude-platform-on-aws`, `bedrock`, `vertex`)
- `--web`'s `WEB_DIST` directory (the image sets its own)

A part that would refuse to start does not block saving. A role the service would not
run is shown as not checked, so a deployment with `JUDGE_ROLES='--api --web --jobs'`
needs no `DISCORD_TOKEN`.

The panel also warns about the files together:

- a `judge.toml` the containers will not see, because `JUDGE_CONFIG` is blank or names
  another file
- a `DB_PORT` that differs from the port in `DATABASE_URL`

## After saving

The binaries read both files only when they start.

- Under Docker, after a `.env` change: `docker compose up -d`, which recreates the
  containers whose environment changed.
- After a `judge.toml` change: `docker compose restart judgebot`. The mounted file's
  content is not a change `up -d` sees.
- A `cargo run` binary picks up both on its next start.

## What it refuses

- A `.env` that dotenvy cannot read, that assigns a variable twice, or whose assignment
  is spelled in a way the editor cannot place on one line. The binaries and Compose
  would disagree about such a file, so fix it by hand first.
- Showing a setting whose value expands `$`, or is a URL carrying a user name or
  password. While its value has that shape it is treated as a secret: replaced, never
  shown. Replaced with a plain value, it is an ordinary setting again and is shown on
  the next load.
- A blank replacement. **Cancel** keeps the current value.
- A `judge.toml` that is not valid TOML opens in the Text view. Saving the text is
  allowed, and the loader reports what it makes of it.

The editor never deletes a file. To go back to the zero-config setup, remove
`judge.toml` yourself.

## Access

The page's calls need the token, sent in a header that a page from another origin cannot
add. The `Host` must be a loopback name or one passed with `--allow-host`, which closes
DNS rebinding. Anyone who can reach the port and holds the token can change your
settings, so keep the port on loopback.
