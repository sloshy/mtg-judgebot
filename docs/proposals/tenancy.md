# Proposal E: Multi-server tenancy

Status: **proposed, not implemented** (2026-09-15). Written during release preparation
because it is the first thing a public bot gets asked for; the decisions in §4 are the
operator's to make before any of it is built.

## 1. What is wrong today

One bot process serves every guild it is invited to with one configuration:

- **One spend cap.** `JUDGE_MAX_USD` is per process. A busy server drains the budget
  for every other server, and nothing tells a guild it is the one doing it.
- **One judge role name.** `JUDGE_ROLE` (default `Judge`) is read from the environment,
  so every guild must name its judges the same way for the override to work.
- **No admission control.** Anyone with the invite link can add the bot; the operator
  learns of a new guild from the `connected to Discord` log line's guild count.
- **`GUILD_ID` conflates two things.** It says *where to register the command* (one
  guild instantly, or globally in up to an hour), not *which guilds to serve*.

What is *not* wrong: the knowledge is global. The Comprehensive Rules, rulings, card
text and the rated prior calls apply to every server alike, so nothing in retrieval or
persistence needs a tenant boundary. Tenancy here is about **admission, budget and
roles**, and it lives entirely in the Discord adapter.

## 2. Shape

A `guilds` table and a `GuildPolicy` value the adapter reads per interaction:

```sql
CREATE TABLE guilds (
    id            text PRIMARY KEY,          -- Discord snowflake
    name          text NOT NULL,
    joined_at     timestamptz NOT NULL DEFAULT now(),
    enabled       boolean NOT NULL DEFAULT true,
    judge_role    text,                      -- NULL → JUDGE_ROLE
    quota         integer,                   -- questions per window, NULL → JUDGE_GUILD_QUOTA
    quota_window  interval                   -- NULL → JUDGE_GUILD_WINDOW_SECS
);
```

- **Admission.** `JUDGE_ADMISSION` is `open` (today's behaviour, every guild is
  enabled on `GuildCreate`) or `allowlist` (a guild is inserted disabled and `/judge`
  answers with a short "this server has not been enabled" until the operator flips
  it). `GuildDelete` marks the row rather than deleting it, so a re-invite keeps its
  settings.
- **Budget.** A per-guild fixed-window quota in *questions*, not dollars, layered in
  front of the global cap exactly as `MCP_JUDGE_LIMIT` is for the MCP transport
  (`judge_api::RateLimiter` already implements the window; it moves to `judge-bot`
  or `judge-llm` so both adapters share it). Dollars stay global because the meter
  settles after the call and a per-guild dollar cap would either over-reserve or
  overshoot. The busy reply names the window ("this server has used its 20 questions
  for the hour").
- **Roles.** `judge_role` per guild, falling back to `JUDGE_ROLE`; `has_role` takes the
  policy instead of the adapter's one string.
- **Admin surface.** One guild-scoped, admin-only command, `/judge-settings`, with
  subcommands `show`, `judge-role <name>`, `quota <n> <minutes>`. Operator-only
  actions (enable, disable, list) are `judge-cli guilds …`, since the operator is not
  necessarily a member of every guild.
- **Registration.** `GUILD_ID` keeps its meaning as a development shortcut; a
  multi-guild deployment registers globally.

Types: `GuildId` already exists (serenity). `GuildPolicy { enabled, judge_role, quota }`
is a plain struct in `judge_bot::discord`; the port is a small `GuildStore` trait beside
`CallStore`, Postgres-backed, cached per process with a short TTL because every
interaction reads it. Nothing enters `crates/core`.

## 3. Cost and blast radius

One migration, one table, one new trait, one new command, and roughly a hundred lines
in the adapter. Persisted calls gain nothing (no tenant column), so no backfill. The
allowlist mode is the only behaviour change visible to users, and it is off by default.

## 4. Decisions needed first

1. Is the hosted instance meant to be open to any server, or invite-only? That picks
   the default for `JUDGE_ADMISSION` and whether the README publishes an invite link.
2. Per-guild quota in questions is proposed; is a per-guild dollar figure wanted badly
   enough to accept settle-after-the-fact accounting?
3. Should guild admins be able to change their own quota, or only the judge role name?
   (The proposal lets them change both within a global ceiling.)
4. Does the rating override need to be per guild at all, or is a global "Judge" role
   name an acceptable convention to document?
