---
title: Create the Discord app
description: The developer-portal steps, the token, the invite URL, and where GUILD_ID comes from.
sidebar:
  order: 2
---

The bot is slash-command only. It needs no privileged gateway intents and no channel
permissions, because it only ever receives its own commands and button presses and replies
through the interaction.

## Steps

1. Open the [Discord Developer Portal](https://discord.com/developers/applications) and
   **New Application**. The name is what members see beside `/judge` in the command picker.
2. Under **Bot**, **Reset Token** and copy it into `.env` as `DISCORD_TOKEN`. It is shown
   once. Leave every *Privileged Gateway Intent* off; the bot requests none.
3. Under **OAuth2 → URL Generator**, tick the scopes `bot` and `applications.commands`,
   leave the bot permissions at none, and open the generated URL to invite the bot to your
   server. The URL has the shape
   `https://discord.com/oauth2/authorize?client_id=<application id>&scope=bot%20applications.commands&permissions=0`.
4. For instant command registration, put your server's id in `.env` as `GUILD_ID`: in
   Discord, *User Settings → Advanced → Developer Mode*, then right-click the server icon →
   **Copy Server ID**. With `GUILD_ID` unset the commands register globally, which Discord
   can take up to an hour to propagate, but works in every server the bot joins.
5. `docker compose up -d bot`. The log line `registered /judge, /help and /forget` confirms
   registration; `/help` in the server confirms it end to end.

## The judge role

Members holding a role whose name matches `JUDGE_ROLE` (default `Judge`) rate as judges:
their rating overrides the crowd's smoothed average for that answer. Create the role in
your server and give it to the people whose rulings you trust. The name comparison is
exact.

## Several servers

One bot can be in many servers; the commands are registered per application, so they never
collide with another bot's `/judge` (Discord's picker shows which bot owns which). What is
shared is the spend cap and the judge role name; per-server quotas and roles are the
subject of the [tenancy proposal](../../design-history/proposal-tenancy/).

If you switch between guild and global registration, clear the old set first (the poise
framework registers what the process is configured for and does not remove the other), or
members see two identical `/judge` entries from the same bot.
