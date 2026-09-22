---
title: Create the Discord app
description: The developer-portal steps, the token, the install link and GUILD_ID, each linked to Discord's documentation.
sidebar:
  order: 2
---

Every judgebot is its own Discord application, owned by whoever runs it. This page takes
you from nothing to `/judge` answering in your server. Discord's walkthrough,
[Building your first Discord Bot](https://docs.discord.com/developers/quick-start/getting-started),
covers the same portal screens in general terms. The steps below say what to choose for
this bot.

The bot uses slash commands only. It needs **no privileged gateway intents** and **no
channel permissions**. It receives only its own commands and button presses, and replies
through the interaction. You never grant it the ability to read messages.

## Before you start

- The [first run](../first-run/) done up to the point where the web page answers: the
  database is up and the data is loaded. Nothing on this page spends money.
- The *Manage Server* permission in the Discord server you are adding the bot to. Adding
  an app to a server requires it
  ([Installation Context](https://docs.discord.com/developers/resources/application#installation-context)).
- A Discord account with email verified, which the developer portal requires.

## 1. Create the application

Open the [Developer Portal](https://discord.com/developers/applications) and choose **New
Application**. Members see the name beside `/judge` in the command picker and on the
bot's profile, so name it for your server ("Rules Judge", "Club Judge").

The *App Icon* is the bot's avatar. Any image works. The project's icon is
[a 512×512 PNG](../../icon.png) (`assets/icon.png` in the repository) if you want it.
Nothing else on the *General Information* page matters to the bot. The *Application ID*
shown there ends up in the install link in step 3.

## 2. Bot user and token

Under **Bot**:

1. Choose **Reset Token** and copy the result into `.env` as `DISCORD_TOKEN`. Discord
   shows a token once. If you lose it, reset again. The token is a credential for the
   application, and every binary that reads it keeps it redacted.
   At the same time, set `JUDGE_OPERATOR_DISCORD` in `.env` to your own Discord username.
   The bot does not start without it, and `/help` and `/license` show it.
2. Leave all three **Privileged Gateway Intents** (*Presence*, *Server Members*, *Message
   Content*) **off**. The bot connects with no intents
   ([Gateway Intents](https://docs.discord.com/developers/events/gateway#privileged-intents)
   explains what they are). Turning one on changes nothing in the bot. Past 100 servers it
   would trigger Discord's verification process for nothing.
3. **Public Bot** can be off. It controls whether *other* people can use your install link,
   which a bot for your own servers does not need.

## 3. The install link

Under **OAuth2 → URL Generator**:

1. Tick the scopes **`bot`** and **`applications.commands`**. `bot` puts a bot user in the
   server. `applications.commands` lets it register slash commands there
   ([OAuth2 scopes](https://docs.discord.com/developers/topics/oauth2#shared-resources-oauth2-scopes)).
2. Under *Bot Permissions*, tick **nothing**. Replies, buttons and the "did you mean?" row
   all go through the interaction, which needs no channel permission. The URL ends in
   `permissions=0`.
3. Open the generated URL in your browser, pick the server, and **Authorize**. This is
   Discord's [bot authorization flow](https://docs.discord.com/developers/topics/oauth2#bot-authorization-flow).
   The URL has the shape
   `https://discord.com/oauth2/authorize?client_id=<application id>&scope=bot%20applications.commands&permissions=0`.
   You can reuse it for every server you administer.

The portal's newer **Installation** page (installation contexts and a Discord-provided
install link) does the same job. If you use it instead, choose:

- *Guild Install* as the only context. The bot has no user-install behaviour.
- The same two scopes.
- No permissions.

## 4. Command registration and `GUILD_ID`

The bot registers its own slash commands when it starts. It registers them either:

- in one server, where they appear instantly, or
- globally, in every server the bot is in, where they take up to an hour to appear.

See [Registering a command](https://docs.discord.com/developers/interactions/application-commands#registering-a-command).
For a bot in one or two servers, register in the server:

1. In Discord, *User Settings → Advanced → Developer Mode* on.
2. Right-click the server icon → **Copy Server ID** (Discord's help article
   [Where can I find my User/Server/Message ID?](https://support.discord.com/hc/en-us/articles/206346498)).
3. Put it in `.env` as `GUILD_ID`.

Leave `GUILD_ID` unset to register globally.

Avoid switching back and forth. The bot registers the set it is configured for and does
not remove the other. Members would see two identical `/judge` entries from the same bot
until the stale set is cleared. The portal does not clear it. Overwriting the set with an
empty list does
([Bulk Overwrite Guild Application Commands](https://docs.discord.com/developers/interactions/application-commands#bulk-overwrite-guild-application-commands)).
You need the bot token, the *Application ID* from step 1 and, to clear a server's set,
the server id `GUILD_ID` held before you blanked it:

```sh
DISCORD_TOKEN=...      # as in .env
APPLICATION_ID=...     # General Information → Application ID
GUILD_ID=...           # the server the stale set was registered in

# clear the server's set (after moving to global registration)
curl -X PUT -H "Authorization: Bot $DISCORD_TOKEN" -H "Content-Type: application/json" -d '[]' \
  "https://discord.com/api/v10/applications/$APPLICATION_ID/guilds/$GUILD_ID/commands"
# clear the global set (after moving to GUILD_ID)
curl -X PUT -H "Authorization: Bot $DISCORD_TOKEN" -H "Content-Type: application/json" -d '[]' \
  "https://discord.com/api/v10/applications/$APPLICATION_ID/commands"
```

Clear only the set the bot is no longer configured for. The bot registers its own set
again at every start.

## 5. Start it

```sh
docker compose up -d bot           # or: cargo run --release -p judge-bot
```

The log line `registered /judge, /card, /rule, /help, /license and /forget in one guild` (or `… globally`)
confirms registration. Running `/help` in the server confirms it end to end. Another
bot's `/judge` in the same server does not conflict with yours: Discord's command picker
shows each bot's icon beside its commands.

## 6. Card symbol emoji

`init` uploads the symbols if `DISCORD_TOKEN` was already set when it ran. Otherwise run this
once:

```sh
docker compose run --rm refresh emoji      # or: cargo run --release -p judge-ingest -- emoji
```

It uploads Scryfall's mana and card symbols as **application emoji**. These belong to the
application, not to any server, and need no emoji permission
([Application-owned emoji](https://docs.discord.com/developers/resources/emoji#emoji-object-applicationowned-emoji)).
Without them, answers show the literal `{W}`. The command is safe to rerun, reads
`DISCORD_TOKEN` and needs no database.

## Keeping it to one channel

The bot answers wherever its commands can be used. That is a Discord setting, not the
bot's. In *Server Settings → Integrations*, open your application and restrict `/judge`
(or every command) to the channels, roles or members you choose
([application command permissions](https://docs.discord.com/developers/interactions/application-commands#permissions)).
A rules-questions channel keeps follow-ups together, because history is per channel.

The bot has two limits of its own. Neither applies to `/card` and `/rule`.

- `JUDGE_USER_LIMIT` questions per member per `JUDGE_USER_WINDOW_SECS`. The default is six
  per ten minutes, and `0` turns it off.
- `JUDGE_CONCURRENCY` runs in flight at once.

An *Incorrect* rating tells the rater to contact you. When the repository
`JUDGE_SOURCE_URL` names is on GitHub, it also links that repository's *Wrong or unhelpful
ruling* issue form. A fork has Issues off until you enable them under the repository's
*Settings → General → Features*.

## The judge role

Members holding a role named `JUDGE_ROLE` (default `Judge`) rate as judges. Their rating
overrides the crowd's smoothed average for that answer. Create the role under *Server Settings →
Roles* (Discord's [Role Management 101](https://support.discord.com/hc/en-us/articles/214836687)
covers it) and give it to the people whose rulings you trust. The name must match exactly.
The role needs no permissions of its own.

## Several servers

One application can be in every server you administer. Reuse the install link from step 3
and register commands globally. The servers then share one process, with one spend cap
(`JUDGE_MAX_USD`), one concurrency limit and one judge role name. That is by design
([D16](../../how-it-works/decisions/#d16-one-judgebot-per-community)).
A community that wants its own budget runs its own instance: a second copy of the same
compose file, with a second application's token.

## Discord's rules for the bot

Your application is bound by the [Discord Developer Policy](https://support-dev.discord.com/hc/articles/8563934450327-Discord-Developer-Policy)
and Terms of Service like any other. Two points matter for a judgebot:

- It must not collect more data than it needs. This one stores questions per channel and
  ratings per user, and `/forget` deletes a user's ratings.
- Magic content is used under Wizards of the Coast's
  [Fan Content Policy](../../reference/attribution/), which your instance's `/help` already
  states.
