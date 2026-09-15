---
title: Create the Discord app
description: Your judgebot is your own Discord application. The developer-portal steps, the token, the install link and GUILD_ID, each linked to Discord's documentation.
sidebar:
  order: 2
---

Every judgebot is its own Discord application, owned by whoever runs it. This page takes
you from nothing to `/judge` answering in your server. Discord's own walkthrough of the
portal, [Building your first Discord Bot](https://docs.discord.com/developers/quick-start/getting-started),
covers the same screens in general terms; the steps below say what to choose for this bot.

The bot is slash-command only. It needs **no privileged gateway intents** and **no
channel permissions**, because it only ever receives its own commands and button presses
and replies through the interaction. You never grant it the ability to read messages.

## Before you start

- The [first run](../first-run/) done up to the point where the web page answers: the
  database is up and the data is loaded. Discord is the last thing to add, and nothing here
  spends money.
- *Manage Server* in the Discord server you are adding the bot to. Adding an app to a
  server requires that permission
  ([Installation Context](https://docs.discord.com/developers/resources/application#installation-context)).
- A Discord account with email verified, which the developer portal requires.

## 1. Create the application

Open the [Developer Portal](https://discord.com/developers/applications) and choose **New
Application**. The name is what members see beside `/judge` in the command picker and on
the bot's profile, so name it for your server ("Rules Judge", "Club Judge").
Nothing else on the *General Information* page matters to the bot; the *Application ID*
shown there is what the install link in step 3 carries.

## 2. The bot user and its token

Under **Bot**:

1. **Reset Token** and copy the result into `.env` as `DISCORD_TOKEN`. Discord shows a
   token once; if you lose it, reset again. The token is a credential for the whole
   application and is held as a redacted secret by every binary that reads it.
2. Leave all three **Privileged Gateway Intents** (*Presence*, *Server Members*, *Message
   Content*) **off**. The bot connects with no intents at all
   ([Gateway Intents](https://docs.discord.com/developers/events/gateway#privileged-intents)
   explains what they are). Turning one on changes nothing in the bot and, past 100
   servers, would trigger Discord's verification process for nothing.
3. **Public Bot** can be off. It controls whether *other* people can use your install link;
   for a bot you run for your own servers there is no reason to allow that.

## 3. The install link

Under **OAuth2 → URL Generator**:

1. Tick the scopes **`bot`** and **`applications.commands`**. `bot` puts a bot user in the
   server; `applications.commands` lets it register slash commands there
   ([OAuth2 scopes](https://docs.discord.com/developers/topics/oauth2#shared-resources-oauth2-scopes)).
2. Under *Bot Permissions*, tick **nothing**. Replies, buttons and the "did you mean?" row
   all travel through the interaction, which needs no permission in the channel. The URL
   ends in `permissions=0`.
3. Open the generated URL in your browser, pick the server, and **Authorize**. This is
   Discord's [bot authorization flow](https://docs.discord.com/developers/topics/oauth2#bot-authorization-flow);
   the URL has the shape
   `https://discord.com/oauth2/authorize?client_id=<application id>&scope=bot%20applications.commands&permissions=0`,
   and you can reuse it for every server you administer.

The newer **Installation** page in the portal (installation contexts and a
Discord-provided install link) does the same job; if you use it instead, keep *Guild
Install* as the only context, the same two scopes, and no permissions. The bot has no
user-install behaviour.

## 4. `GUILD_ID`: where the commands register

Slash commands are registered by the running bot, per application, either in one server
(instant) or globally (every server the bot is in, up to an hour to appear)
([Registering a command](https://docs.discord.com/developers/interactions/application-commands#registering-a-command)).
For a bot in one or two servers, register in the server:

1. In Discord, *User Settings → Advanced → Developer Mode* on.
2. Right-click the server icon → **Copy Server ID** (Discord's help article
   [Where can I find my User/Server/Message ID?](https://support.discord.com/hc/en-us/articles/206346498)).
3. Put it in `.env` as `GUILD_ID`.

Leave `GUILD_ID` unset to register globally. Do not switch back and forth casually: the
bot registers the set it is configured for and does not remove the other, so members
would see two identical `/judge` entries from the same bot until the stale set is cleared
(the portal does not do this; delete the guild set with any tool that speaks the API, or
wait for the global set and remove the guild one the same way).

## 5. Start it

```sh
docker compose up -d bot           # or: cargo run --release -p judge-bot
```

The log line `registered /judge, /help and /forget in one guild` (or `… globally`)
confirms registration; `/help` in the server confirms it end to end. Discord's command
picker shows your bot's icon beside its commands, so another bot's `/judge` in the same
server does not conflict with yours.

## 6. Card symbols as pictures (once)

```sh
cargo run --release -p judge-ingest -- emoji
```

uploads Scryfall's mana and card symbols as **application emoji**, which belong to the
application rather than to any server and need no emoji permission to use
([Application-owned emoji](https://docs.discord.com/developers/resources/emoji#emoji-object-applicationowned-emoji)).
Without them answers render the literal `{W}`. Idempotent; reads `DISCORD_TOKEN`; needs no
database.

## The judge role

Members holding a role whose name matches `JUDGE_ROLE` (default `Judge`) rate as judges:
their rating overrides the crowd's smoothed average for that answer. Create the role in
your server (*Server Settings → Roles*; Discord's
[Role Management 101](https://support.discord.com/hc/en-us/articles/214836687) covers it)
and give it to the people whose rulings you trust. The name comparison is exact, and the
role needs no permissions of its own.

## Several servers

One application can be in every server you administer: reuse the install link from step 3
and register commands globally. What the servers then share is the process: one spend cap
(`JUDGE_MAX_USD`), one concurrency limit and one judge role name. That is by design
([D16](../../how-it-works/decisions/#d16-one-judgebot-per-community-no-multi-tenancy)):
a community that wants its own budget runs its own instance, which is a second copy of the
same compose file with a second application's token.

## Discord's rules for the bot

Your application is bound by the [Discord Developer Policy](https://support-dev.discord.com/hc/articles/8563934450327-Discord-Developer-Policy)
and Terms of Service like any other. The relevant points for a judgebot: it must not
collect more data than it needs (this one stores questions per channel and ratings per
user, and `/forget` deletes the latter), and Magic content is used under Wizards of the
Coast's [Fan Content Policy](../../reference/attribution/), which your instance's `/help`
already states.
